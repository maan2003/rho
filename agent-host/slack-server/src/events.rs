//! Slack events, fast: the host holds the app's Socket Mode connection and
//! keeps its recent envelopes in memory, and agents long-poll them with
//! `rho.events`. The host acknowledges each envelope itself.
//!
//! The buffer only says what happened lately. It is not a mirror: when an
//! agent's cursor falls behind the buffer, or comes from an earlier host
//! process, the reply says `truncated` and the agent reads Slack itself.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context as _, Result};
use futures_util::{SinkExt as _, StreamExt as _};
use reqwest::{Client, Url};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message as Frame;

use crate::TokenProvider;

/// Envelopes kept for agents that poll late.
const CAPACITY: usize = 10_000;

pub struct Events {
    /// Tells cursors from an earlier host process apart from this one's.
    epoch: u64,
    buffer: Mutex<Buffer>,
    arrived: Notify,
}

#[derive(Default)]
struct Buffer {
    /// Sequence number of the next event; the first is 1.
    next: u64,
    events: VecDeque<(u64, Value)>,
}

impl Default for Events {
    fn default() -> Self {
        Self {
            epoch: rand::random::<u32>().into(),
            buffer: Mutex::new(Buffer {
                next: 1,
                events: VecDeque::new(),
            }),
            arrived: Notify::new(),
        }
    }
}

impl Events {
    pub fn push(&self, event: Value) {
        let mut buffer = self.buffer.lock().expect("events lock");
        let seq = buffer.next;
        buffer.next += 1;
        buffer.events.push_back((seq, event));
        if buffer.events.len() > CAPACITY {
            buffer.events.pop_front();
        }
        drop(buffer);
        self.arrived.notify_waiters();
    }

    /// The envelopes after `cursor`, waiting up to `timeout` for the first.
    /// No cursor means "from now".
    pub(crate) async fn wait(&self, cursor: Option<&str>, timeout: Duration) -> Value {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut after = None;
        let mut truncated = false;
        loop {
            let notified = self.arrived.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let buffer = self.buffer.lock().expect("events lock");
                let head = buffer.next - 1;
                let after = *after.get_or_insert_with(|| match cursor.map(|c| self.parse(c)) {
                    None => head,
                    Some(Some(seq))
                        if seq <= head
                            && seq + 1
                                >= buffer.events.front().map_or(buffer.next, |(seq, _)| *seq) =>
                    {
                        seq
                    }
                    Some(_) => {
                        truncated = true;
                        head
                    }
                });
                let events: Vec<&Value> = buffer
                    .events
                    .iter()
                    .filter(|(seq, _)| *seq > after)
                    .map(|(_, event)| event)
                    .collect();
                if !events.is_empty() || truncated || tokio::time::Instant::now() >= deadline {
                    return json!({
                        "ok": true,
                        "events": events,
                        "cursor": format!("{}:{head}", self.epoch),
                        "truncated": truncated,
                    });
                }
            }
            let _ = tokio::time::timeout_at(deadline, notified).await;
        }
    }

    fn parse(&self, cursor: &str) -> Option<u64> {
        let (epoch, seq) = cursor.split_once(':')?;
        (epoch.parse::<u64>().ok()? == self.epoch)
            .then(|| seq.parse().ok())
            .flatten()
    }
}

/// Holds the Socket Mode connection for as long as the host runs, and
/// reconnects whenever Slack or the network drops it.
pub async fn run_socket_mode(
    events: std::sync::Arc<Events>,
    bot_token: TokenProvider,
    app_token: TokenProvider,
    slack_api_url: Url,
) {
    let client = Client::new();
    let mut reported = false;
    loop {
        let Some(app) = present(&app_token) else {
            // Not configured yet: `rho slack init` may install it later.
            tokio::time::sleep(Duration::from_secs(10)).await;
            continue;
        };
        match connect(&events, &client, &bot_token, &app, &slack_api_url).await {
            Ok(()) => reported = false,
            Err(error) => {
                if !reported {
                    tracing::warn!(error = format!("{error:#}"), "slack socket mode");
                    reported = true;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

pub(crate) fn present(token: &TokenProvider) -> Option<String> {
    token()
        .ok()
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty())
}

/// One connection, until Slack asks the client to reconnect.
async fn connect(
    events: &Events,
    client: &Client,
    bot_token: &TokenProvider,
    app_token: &str,
    slack_api_url: &Url,
) -> Result<()> {
    // The bot's own messages would wake every agent that waits after it posts.
    let me = match present(bot_token) {
        Some(bot) => call(client, slack_api_url, "auth.test", &bot).await?,
        None => Value::Null,
    };
    let own = |event: &Value| {
        (me.get("user_id").is_some() && event.get("user") == me.get("user_id"))
            || (me.get("bot_id").is_some() && event.get("bot_id") == me.get("bot_id"))
    };
    let open = call(client, slack_api_url, "apps.connections.open", app_token).await?;
    let url = open["url"]
        .as_str()
        .context("apps.connections.open gave no url")?;
    let (mut socket, _) = tokio_tungstenite::connect_async(url)
        .await
        .context("connecting to the Socket Mode URL")?;
    while let Some(frame) = socket.next().await {
        let Frame::Text(text) = frame.context("reading Socket Mode")? else {
            continue;
        };
        let envelope: Value = serde_json::from_str(&text).context("Socket Mode frame")?;
        if envelope["type"] == "disconnect" {
            return Ok(());
        }
        if let Some(id) = envelope.get("envelope_id") {
            // Slack redelivers what is not acknowledged within three seconds.
            let ack = json!({ "envelope_id": id }).to_string();
            socket
                .send(Frame::text(ack))
                .await
                .context("acknowledging")?;
        }
        if envelope.get("envelope_id").is_some()
            && !envelope.pointer("/payload/event").is_some_and(own)
        {
            events.push(envelope);
        }
    }
    Ok(())
}

async fn call(client: &Client, slack_api_url: &Url, method: &str, token: &str) -> Result<Value> {
    let url = slack_api_url.join(method)?;
    let reply: Value = client
        .post(url)
        .bearer_auth(token)
        .send()
        .await
        .with_context(|| format!("calling {method}"))?
        .json()
        .await
        .with_context(|| format!("reading {method}"))?;
    anyhow::ensure!(reply["ok"] == true, "{method}: {}", reply["error"]);
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::routing::post;

    use super::*;

    fn envelope(event: Value) -> Value {
        json!({"envelope_id": "e", "type": "events_api", "payload": {"event": event}})
    }

    fn message(channel: &str, ts: &str, thread_ts: Option<&str>) -> Value {
        envelope(
            json!({"type": "message", "channel": channel, "ts": ts, "thread_ts": thread_ts, "user": "U2"}),
        )
    }

    #[tokio::test]
    async fn a_poll_from_now_waits_for_the_next_event() {
        let events = Arc::new(Events::default());
        events.push(message("D1", "1.0", None));
        let pusher = events.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            pusher.push(message("D1", "2.0", Some("1.0")));
        });

        let reply = events.wait(None, Duration::from_secs(5)).await;

        // The event from before the poll is not news.
        assert_eq!(reply["events"], json!([message("D1", "2.0", Some("1.0"))]));
        assert_eq!(reply["truncated"], false);
        let cursor = reply["cursor"].as_str().unwrap();
        assert!(cursor.ends_with(":2"), "{cursor}");

        // From that cursor, nothing new: the poll times out empty.
        let reply = events.wait(Some(cursor), Duration::from_millis(20)).await;
        assert_eq!(reply["events"], json!([]));
        assert_eq!(reply["cursor"], cursor);
    }

    #[tokio::test]
    async fn a_cursor_returns_what_arrived_since_without_waiting() {
        let events = Events::default();
        let start = events.wait(None, Duration::ZERO).await;
        events.push(message("D1", "5.0", None));
        events.push(message("C2", "6.0", Some("4.0")));

        let reply = events
            .wait(start["cursor"].as_str(), Duration::from_secs(60))
            .await;

        assert_eq!(
            reply["events"],
            json!([
                message("D1", "5.0", None),
                message("C2", "6.0", Some("4.0"))
            ])
        );
    }

    #[tokio::test]
    async fn cursors_the_buffer_lost_or_another_process_made_are_truncated() {
        let events = Events::default();
        let start = events.wait(None, Duration::ZERO).await;
        let start = start["cursor"].as_str().unwrap().to_owned();
        let epoch = start.split_once(':').unwrap().0.to_owned();

        // Just inside the buffer is not truncated.
        for i in 0..CAPACITY {
            events.push(message("D1", &format!("{i}.0"), None));
        }
        let reply = events.wait(Some(&start), Duration::ZERO).await;
        assert_eq!(reply["truncated"], false);
        assert_eq!(reply["events"].as_array().unwrap().len(), CAPACITY);

        events.push(message("D1", "late.0", None));
        for cursor in [
            start,
            "9:0".to_owned(),
            format!("{epoch}:999999"),
            "junk".to_owned(),
        ] {
            let reply = events.wait(Some(&cursor), Duration::from_secs(60)).await;
            assert_eq!(reply["truncated"], true, "{cursor}");
            assert_eq!(reply["events"], json!([]), "{cursor}");
            assert_eq!(reply["cursor"], format!("{epoch}:{}", CAPACITY + 1));
        }
    }

    fn socket_envelope(id: &str, event: Value) -> Value {
        json!({"envelope_id": id, "type": "events_api",
               "payload": {"type": "event_callback", "event": event}})
    }
    fn own_message() -> Value {
        socket_envelope(
            "e1",
            json!({"type": "message", "channel": "D1", "user": "UBOT"}),
        )
    }
    fn own_bot() -> Value {
        socket_envelope(
            "e2",
            json!({"type": "message", "channel": "D1", "bot_id": "BBOT"}),
        )
    }
    fn others() -> Value {
        socket_envelope(
            "e3",
            json!({"type": "message", "channel": "D1", "user": "U2"}),
        )
    }
    fn interactive() -> Value {
        json!({"envelope_id": "e4", "type": "interactive", "payload": {"type": "block_actions"}})
    }

    #[tokio::test]
    async fn socket_mode_acknowledges_every_envelope_and_keeps_others() {
        let ws = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_url = format!("ws://{}/", ws.local_addr().unwrap());
        let http = Router::new()
            .route(
                "/api/auth.test",
                post(|| async {
                    axum::Json(json!({"ok": true, "user_id": "UBOT", "bot_id": "BBOT"}))
                }),
            )
            .route(
                "/api/apps.connections.open",
                post(move |headers: axum::http::HeaderMap| async move {
                    assert_eq!(headers["authorization"], "Bearer xapp-1");
                    axum::Json(json!({"ok": true, "url": ws_url}))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = Url::parse(&format!("http://{}/api/", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move { axum::serve(listener, http).await.unwrap() });

        let slack = tokio::spawn(async move {
            let (stream, _) = ws.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            for frame in [
                json!({"type": "hello"}),
                own_message(),
                own_bot(),
                others(),
                interactive(),
            ] {
                socket.send(Frame::text(frame.to_string())).await.unwrap();
            }
            let mut acks = Vec::new();
            while acks.len() < 4 {
                if let Frame::Text(text) = socket.next().await.unwrap().unwrap() {
                    acks.push(serde_json::from_str::<Value>(&text).unwrap()["envelope_id"].clone());
                }
            }
            socket
                .send(Frame::text(
                    r#"{"type":"disconnect","reason":"refresh_requested"}"#,
                ))
                .await
                .unwrap();
            acks
        });

        let events = Events::default();
        connect(
            &events,
            &Client::new(),
            &(Arc::new(|| Ok("xoxb-1".into())) as TokenProvider),
            "xapp-1",
            &api,
        )
        .await
        .unwrap();

        assert_eq!(
            slack.await.unwrap(),
            [json!("e1"), json!("e2"), json!("e3"), json!("e4")]
        );
        let buffered: Vec<Value> = events
            .buffer
            .lock()
            .unwrap()
            .events
            .iter()
            .map(|(_, e)| e.clone())
            .collect();
        assert_eq!(buffered, [others(), interactive()]);
    }
}
