//! The two live sources: the RTM websocket and the `activity.feed` poll.
//!
//! The feed is the truth. It is a stable, paged list of exactly the things
//! that oblige the user, so a frame lost to a dropped socket is never a lost
//! mention. The websocket exists so the lamp lights within a second rather
//! than within a poll interval.
//!
//! Both loops are plain futures over an [`Client`] and an unbounded sink, so
//! the transport tests drive them against a fake Slack with no GUI, and the
//! session decides which runtime carries them.

use std::sync::Arc;
use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;
use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_tungstenite::tungstenite::Message as Frame;

use crate::api::{ActivityItem, Client, RtmConnection};
use crate::events::{WsEvent, parse};
use crate::types::Ts;

/// How often the client pings. Slack's own client pings on this order, and
/// it is short enough that a silently dead socket is noticed in well under
/// the "few minutes" the design allows before the lamp lights.
pub const PING_INTERVAL: Duration = Duration::from_secs(30);
/// A socket that has not answered a ping within this long is dead, whatever
/// the TCP layer still believes.
pub const PONG_GRACE: Duration = Duration::from_secs(20);
/// The feed poll. Fast enough to be the safety net, slow enough not to be a
/// second event stream.
pub const FEED_INTERVAL: Duration = Duration::from_secs(60);
/// How far back a catch-up poll pages before giving up and accepting that
/// the newest page is enough. A gap longer than this is not a dropped frame,
/// it is a laptop that was shut.
const MAX_CATCH_UP_PAGES: usize = 5;

/// How long the loops wait. Production uses [`Timings::default`]; the
/// transport tests shorten them so a reconnect or a poll happens in
/// milliseconds instead of a minute.
#[derive(Clone, Copy, Debug)]
pub struct Timings {
    pub ping_interval: Duration,
    pub pong_grace: Duration,
    pub feed_interval: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            ping_interval: PING_INTERVAL,
            pong_grace: PONG_GRACE,
            feed_interval: FEED_INTERVAL,
        }
    }
}

/// Everything the transport tells the model.
#[derive(Clone, Debug)]
pub enum Wire {
    Connected(RtmConnection),
    Frame(WsEvent),
    Disconnected(String),
    /// One poll's worth of feed items, newest first.
    Feed(Vec<ActivityItem>),
    FeedFailed(String),
}

/// Exponential backoff, doubling from a second to a minute. Slack is
/// unofficial territory: reconnecting hard would be the one behaviour that
/// gets a session banned.
pub fn backoff(attempt: u32) -> Duration {
    let seconds = 1u64.checked_shl(attempt.min(6)).unwrap_or(64).min(60);
    Duration::from_secs(seconds)
}

/// Runs the websocket until `sink` is closed, reconnecting forever.
///
/// `catch_up` is notified after every successful connect, which is what makes
/// the feed fill the gap the outage left before the lamp clears.
pub async fn run_socket(
    client: Arc<Client>,
    mut sink: UnboundedSender<Wire>,
    catch_up: Arc<Notify>,
    timings: Timings,
) {
    let mut attempt = 0u32;
    // Slack rotates the socket URL while connected and expects the newest one
    // to be used first; falling back to `rtm.connect` costs an extra
    // round-trip and, when rate-limited, an extra failure.
    let mut reconnect_url: Option<String> = None;
    loop {
        let connection = match reconnect_url.take() {
            Some(url) => Ok(RtmConnection {
                url,
                ..RtmConnection::default()
            }),
            None => client.rtm_connect().await,
        };
        let connection = match connection {
            Ok(connection) => connection,
            Err(error) => {
                if sink
                    .send(Wire::Disconnected(format!("{error:#}")))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(backoff(attempt)).await;
                attempt = attempt.saturating_add(1);
                continue;
            }
        };
        let announce = !connection.self_id.0.is_empty();
        let url = connection.url.clone();
        if announce && sink.send(Wire::Connected(connection)).await.is_err() {
            return;
        }
        match pump(&url, &mut sink, &catch_up, timings).await {
            Ok(next_url) => {
                attempt = 0;
                reconnect_url = next_url;
            }
            Err(error) => {
                if sink.send(Wire::Disconnected(error)).await.is_err() {
                    return;
                }
                tokio::time::sleep(backoff(attempt)).await;
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

/// One connection's life. Returns the URL to reconnect with when Slack gave
/// one, or an error describing why the socket ended.
async fn pump(
    url: &str,
    sink: &mut UnboundedSender<Wire>,
    catch_up: &Notify,
    timings: Timings,
) -> Result<Option<String>, String> {
    let (mut socket, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|error| format!("connecting to Slack: {error}"))?;
    catch_up.notify_one();
    let mut reconnect_url = None;
    let mut ping_id = 0u64;
    let mut awaiting_pong = false;
    let mut ping = tokio::time::interval(timings.ping_interval);
    ping.tick().await;
    loop {
        let deadline = timings.pong_grace;
        tokio::select! {
            _ = tokio::time::sleep(deadline), if awaiting_pong => {
                return Err("Slack stopped answering pings".to_owned());
            }
            _ = ping.tick(), if !awaiting_pong => {
                ping_id += 1;
                let ping = json!({"type": "ping", "id": ping_id}).to_string();
                socket
                    .send(Frame::Text(ping.into()))
                    .await
                    .map_err(|error| format!("pinging Slack: {error}"))?;
                awaiting_pong = true;
            }
            frame = socket.next() => {
                let frame = match frame {
                    Some(Ok(frame)) => frame,
                    Some(Err(error)) => return Err(format!("Slack socket failed: {error}")),
                    None => return Ok(reconnect_url),
                };
                let text = match frame {
                    Frame::Text(text) => text.to_string(),
                    Frame::Close(_) => return Ok(reconnect_url),
                    // Protocol-level pings are answered by the library; only
                    // Slack's own JSON ping counts as liveness.
                    _ => continue,
                };
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                let event = parse(&value);
                match event {
                    WsEvent::Pong => awaiting_pong = false,
                    WsEvent::ReconnectUrl(url) => reconnect_url = Some(url),
                    WsEvent::Ignored => {}
                    event => {
                        if sink.send(Wire::Frame(event)).await.is_err() {
                            return Ok(None);
                        }
                    }
                }
            }
        }
    }
}

/// Polls `activity.feed` forever, and immediately whenever `catch_up` fires.
pub async fn run_feed(
    client: Arc<Client>,
    mut sink: UnboundedSender<Wire>,
    catch_up: Arc<Notify>,
    timings: Timings,
) {
    let mut newest: Option<Ts> = None;
    loop {
        match poll_feed(&client, newest.as_ref()).await {
            Ok(items) => {
                if let Some(latest) = items.iter().max_by(|left, right| {
                    left.ts.epoch_seconds().total_cmp(&right.ts.epoch_seconds())
                }) {
                    newest = Some(latest.ts.clone());
                }
                if sink.send(Wire::Feed(items)).await.is_err() {
                    return;
                }
            }
            Err(error) => {
                if sink
                    .send(Wire::FeedFailed(format!("{error:#}")))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(timings.feed_interval) => {}
            _ = catch_up.notified() => {}
        }
    }
}

/// One poll: the newest page, and older pages while they might still contain
/// items rho has not seen. `newest` is the newest item the caller has ever
/// been handed, so a first poll (or one after a long sleep) reads one page
/// and a poll after an outage reads back to the gap.
pub async fn poll_feed(client: &Client, newest: Option<&Ts>) -> anyhow::Result<Vec<ActivityItem>> {
    let mut collected = Vec::new();
    let mut cursor = None;
    for _ in 0..MAX_CATCH_UP_PAGES {
        let page = client.activity_feed(cursor.as_deref()).await?;
        let reached_known = page.items.iter().any(|item| match newest {
            Some(newest) => !item.ts.is_newer_than(newest),
            None => true,
        });
        let empty = page.items.is_empty();
        collected.extend(page.items);
        cursor = page.cursor;
        if empty || reached_known || cursor.is_none() {
            break;
        }
    }
    Ok(collected)
}

impl Default for RtmConnection {
    fn default() -> Self {
        Self {
            url: String::new(),
            self_id: crate::types::UserId(String::new()),
            self_name: String::new(),
            team_name: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_from_a_second_and_stops_at_a_minute() {
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(4), Duration::from_secs(16));
        assert_eq!(backoff(6), Duration::from_secs(60));
        assert_eq!(backoff(99), Duration::from_secs(60));
    }
}
