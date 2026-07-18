//! End-to-end test against a fake Slack: a minimal HTTP server for the Web
//! API methods rho calls, plus a websocket server playing the Socket Mode
//! side (hello, event envelopes, ack checks, disconnect).

use anyhow::Context as _;
use futures_util::{SinkExt as _, StreamExt as _};
use rho_slack::{SlackApi, SlackConfig, run_connection};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

const BOT_TOKEN: &str = "xoxb-test-bot";
const APP_TOKEN: &str = "xapp-test-app";
const BOT_USER: &str = "UBOT";

/// One-request-per-connection HTTP/1.1 handling, just enough for reqwest.
async fn serve_api(listener: TcpListener, ws_url: String, posts: mpsc::Sender<serde_json::Value>) {
    // First chat.postMessage gets rate limited to prove the retry path.
    let mut rate_limited_once = false;
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let mut raw = Vec::new();
        let (path, auth, body) = loop {
            let mut chunk = [0u8; 4096];
            let n = stream.read(&mut chunk).await.expect("read request");
            assert_ne!(n, 0, "client hung up mid-request");
            raw.extend_from_slice(&chunk[..n]);
            let Some(header_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&raw[..header_end]).to_string();
            let content_length = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")?
                        .trim()
                        .parse::<usize>()
                        .ok()
                })
                .unwrap_or(0);
            if raw.len() < header_end + 4 + content_length {
                continue;
            }
            let path = head
                .lines()
                .next()
                .unwrap()
                .split(' ')
                .nth(1)
                .unwrap()
                .to_owned();
            let auth = head
                .lines()
                .find_map(|line| {
                    Some(
                        line.to_ascii_lowercase()
                            .strip_prefix("authorization:")?
                            .trim()
                            .to_owned(),
                    )
                })
                .unwrap_or_default();
            let body = raw[header_end + 4..header_end + 4 + content_length].to_vec();
            break (path, auth, body);
        };
        let (path, query) = path.split_once('?').unwrap_or((path.as_str(), ""));
        let response = match path {
            "/apps.connections.open" => {
                assert_eq!(auth, format!("bearer {APP_TOKEN}"));
                serde_json::json!({ "ok": true, "url": ws_url })
            }
            "/auth.test" if auth == format!("bearer {BOT_TOKEN}") => {
                serde_json::json!({ "ok": true, "user_id": BOT_USER })
            }
            "/auth.test" => serde_json::json!({ "ok": false, "error": "invalid_auth" }),
            "/chat.postMessage" if !rate_limited_once => {
                rate_limited_once = true;
                let reply = "HTTP/1.1 429 Too Many Requests\r\nretry-after: 0\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                stream.write_all(reply.as_bytes()).await.expect("write 429");
                stream.shutdown().await.ok();
                continue;
            }
            "/chat.postMessage" => {
                assert_eq!(auth, format!("bearer {BOT_TOKEN}"));
                let body: serde_json::Value = serde_json::from_slice(&body).expect("post body");
                posts.send(body).await.expect("record post");
                serde_json::json!({ "ok": true, "ts": "111.222" })
            }
            "/users.info" => {
                assert_eq!(query, "user=UALICE");
                serde_json::json!({
                    "ok": true,
                    "user": { "name": "alice", "profile": { "display_name": "Alice" } },
                })
            }
            other => panic!("unexpected api call: {other}"),
        };
        let payload = response.to_string();
        let reply = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
            payload.len()
        );
        stream
            .write_all(reply.as_bytes())
            .await
            .expect("write response");
        stream.shutdown().await.ok();
    }
}

fn envelope(id: &str, event: serde_json::Value) -> Message {
    Message::text(
        serde_json::json!({
            "type": "events_api",
            "envelope_id": id,
            "payload": { "event": event },
        })
        .to_string(),
    )
}

async fn expect_ack(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    envelope_id: &str,
) {
    let frame = socket.next().await.expect("ack frame").expect("ack read");
    let value: serde_json::Value =
        serde_json::from_str(frame.to_text().expect("text ack")).expect("json ack");
    assert_eq!(value["envelope_id"], envelope_id);
}

#[tokio::test]
async fn socket_mode_round_trip() -> anyhow::Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let ws_listener = TcpListener::bind("127.0.0.1:0").await?;
    let ws_url = format!("ws://{}", ws_listener.local_addr()?);
    let api_listener = TcpListener::bind("127.0.0.1:0").await?;
    let api_base = format!("http://{}", api_listener.local_addr()?);
    let (posts_tx, mut posts_rx) = mpsc::channel(8);
    tokio::spawn(serve_api(api_listener, ws_url, posts_tx));

    let slack_side = tokio::spawn(async move {
        let (stream, _) = ws_listener.accept().await.expect("ws accept");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("ws handshake");
        socket
            .send(Message::text(
                serde_json::json!({ "type": "hello" }).to_string(),
            ))
            .await
            .expect("hello");
        // A channel mention delivered twice (app_mention + message), plus a
        // bot message and a subtyped edit that must both be dropped.
        let mention = serde_json::json!({
            "type": "app_mention",
            "user": "UALICE",
            "text": format!("<@{BOT_USER}> deploy it"),
            "channel": "C123",
            "ts": "1.000",
        });
        socket
            .send(envelope("env-1", mention.clone()))
            .await
            .expect("send mention");
        expect_ack(&mut socket, "env-1").await;
        let duplicate = serde_json::json!({
            "type": "message",
            "channel_type": "channel",
            "user": "UALICE",
            "text": format!("<@{BOT_USER}> deploy it"),
            "channel": "C123",
            "ts": "1.000",
        });
        socket
            .send(envelope("env-2", duplicate))
            .await
            .expect("send duplicate");
        expect_ack(&mut socket, "env-2").await;
        let from_bot = serde_json::json!({
            "type": "message",
            "channel_type": "im",
            "bot_id": "B1",
            "text": "own reply",
            "channel": "D123",
            "ts": "2.000",
        });
        socket
            .send(envelope("env-3", from_bot))
            .await
            .expect("send bot message");
        expect_ack(&mut socket, "env-3").await;
        let threaded_dm = serde_json::json!({
            "type": "message",
            "channel_type": "im",
            "user": "UALICE",
            "text": "and a follow-up",
            "channel": "D123",
            "ts": "3.000",
            "thread_ts": "2.500",
        });
        socket
            .send(envelope("env-4", threaded_dm))
            .await
            .expect("send dm");
        expect_ack(&mut socket, "env-4").await;
        socket
            .send(Message::text(
                serde_json::json!({ "type": "disconnect", "reason": "test over" }).to_string(),
            ))
            .await
            .expect("disconnect");
    });

    let api = SlackApi::new(&api_base);
    let mut config = SlackConfig::new(BOT_TOKEN.to_owned(), APP_TOKEN.to_owned());
    config.api_base = api_base.clone();
    let identity = api.auth_test(&config.bot_token).await?;
    assert_eq!(identity.user_id, BOT_USER);

    let (tx, mut rx) = mpsc::channel(8);
    run_connection(&api, &config, &identity.user_id, &tx).await?;
    slack_side.await?;
    drop(tx);

    let mention = rx.recv().await.context("mention event")?;
    assert_eq!(mention.channel, "C123");
    assert_eq!(mention.user.as_deref(), Some("UALICE"));
    assert_eq!(mention.text, "deploy it");
    assert!(mention.is_mention);
    assert_eq!(mention.thread_root(), "1.000");
    assert_eq!(mention.session_key(), "slack:C123:1.000");

    let dm = rx.recv().await.context("dm event")?;
    assert_eq!(dm.channel_type, "im");
    assert!(!dm.is_mention);
    assert_eq!(dm.thread_root(), "2.500");
    assert!(
        rx.recv().await.is_none(),
        "duplicate or bot events leaked through"
    );

    // First attempt is answered with 429 + retry-after; this succeeding at
    // all proves the rate-limit retry.
    let ts = api
        .post_message(&config.bot_token, "C123", Some("1.000"), "done ✅")
        .await?;
    assert_eq!(ts, "111.222");

    let name = api.users_info(&config.bot_token, "UALICE").await?;
    assert_eq!(name, "Alice");
    let recorded = posts_rx.recv().await.context("recorded post")?;
    assert_eq!(recorded["channel"], "C123");
    assert_eq!(recorded["thread_ts"], "1.000");
    assert_eq!(recorded["text"], "done ✅");

    // Slack-style in-band errors ({"ok":false,…} in an HTTP 200) surface
    // with the error name but never the token.
    let error = api
        .auth_test("xoxb-wrong-token")
        .await
        .expect_err("wrong-token auth.test must fail");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("invalid_auth"));
    assert!(!rendered.contains("wrong-token"));
    Ok(())
}
