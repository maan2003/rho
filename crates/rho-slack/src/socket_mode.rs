//! One Socket Mode connection: open the URL, ack envelopes within Slack's
//! 3-second window, and forward normalized message events.
//!
//! Server frames are remote, semi-trusted input: malformed JSON is logged
//! and skipped, unknown envelope and event types are ignored, and nothing
//! here panics on missing fields.

use std::collections::VecDeque;

use anyhow::Context as _;
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::{SlackApi, SlackConfig};

/// Remember this many recently seen `channel:ts` pairs to drop Slack's
/// at-least-once redeliveries and message/app_mention double events.
const DEDUP_CAPACITY: usize = 512;

/// Slack's server pings every ~10s, so a connection this quiet is dead
/// (half-open TCP after suspend/NAT timeout would otherwise hang the read
/// forever with no error).
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A user message rho may want to respond to, normalized across `message`
/// and `app_mention` events. Bot messages, edits, and other subtypes are
/// filtered out before this is built.
#[derive(Debug, Clone)]
pub struct MessageEvent {
    pub channel: String,
    /// `im` for DMs; empty when Slack omits it (app_mention events).
    pub channel_type: String,
    pub user: Option<String>,
    /// Message text with the bot's own `<@U…>` mention tag stripped.
    pub text: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub is_mention: bool,
}

impl MessageEvent {
    /// The ts identifying the thread this message belongs to (its own ts
    /// when it is not a threaded reply); replies post under this.
    pub fn thread_root(&self) -> &str {
        self.thread_ts.as_deref().unwrap_or(&self.ts)
    }

    /// Stable key for mapping a Slack thread to an agent session.
    pub fn session_key(&self) -> String {
        format!("slack:{}:{}", self.channel, self.thread_root())
    }
}

/// Drive one Socket Mode connection until Slack asks to reconnect.
///
/// Returns `Ok` on a routine `disconnect` frame or server close (the caller
/// should reconnect immediately) and `Err` on transport or protocol
/// failures (the caller should back off).
pub async fn run_connection(
    api: &SlackApi,
    config: &SlackConfig,
    bot_user_id: &str,
    tx: &mpsc::Sender<MessageEvent>,
) -> anyhow::Result<()> {
    let url = api.connections_open(&config.app_token).await?;
    let (mut socket, _response) = connect_async(&url)
        .await
        .context("connecting to slack socket mode")?;
    let mut seen: VecDeque<String> = VecDeque::with_capacity(DEDUP_CAPACITY);

    loop {
        let frame = match tokio::time::timeout(READ_TIMEOUT, socket.next()).await {
            Ok(Some(frame)) => frame.context("reading from slack socket mode")?,
            Ok(None) => return Ok(()),
            Err(_) => anyhow::bail!("no frames for {READ_TIMEOUT:?}: connection is stale"),
        };
        match frame {
            Message::Text(payload) => {
                let value: serde_json::Value = match serde_json::from_str(payload.as_str()) {
                    Ok(value) => value,
                    Err(error) => {
                        tracing::warn!(%error, "malformed socket mode frame");
                        continue;
                    }
                };
                match value["type"].as_str() {
                    Some("hello") => tracing::info!("slack socket mode connected"),
                    Some("disconnect") => {
                        tracing::info!(
                            reason = value["reason"].as_str().unwrap_or(""),
                            "slack requested reconnect"
                        );
                        return Ok(());
                    }
                    Some("events_api") => {
                        if let Some(envelope_id) = value["envelope_id"].as_str() {
                            let ack = serde_json::json!({ "envelope_id": envelope_id });
                            socket
                                .send(Message::text(ack.to_string()))
                                .await
                                .context("acking slack envelope")?;
                        }
                        let event = &value["payload"]["event"];
                        if let Some(event) = parse_event(event, bot_user_id, &mut seen)
                            && tx.send(event).await.is_err()
                        {
                            // Receiver gone: the daemon side is shutting down.
                            return Ok(());
                        }
                    }
                    _ => {}
                }
            }
            Message::Ping(payload) => {
                socket
                    .send(Message::Pong(payload))
                    .await
                    .context("answering slack ping")?;
            }
            Message::Close(_) => return Ok(()),
            _ => {}
        }
    }
}

fn parse_event(
    event: &serde_json::Value,
    bot_user_id: &str,
    seen: &mut VecDeque<String>,
) -> Option<MessageEvent> {
    let event_type = event["type"].as_str()?;
    if !matches!(event_type, "message" | "app_mention") {
        return None;
    }
    // Only plain user messages: bot posts (including our own replies) carry
    // bot_id, and edits/joins/etc. carry a subtype.
    if !event["bot_id"].is_null() || !event["subtype"].is_null() {
        return None;
    }
    let user = event["user"].as_str();
    if user == Some(bot_user_id) {
        return None;
    }
    let channel = event["channel"].as_str()?.to_owned();
    let ts = event["ts"].as_str()?.to_owned();

    // A mention in a channel arrives as both `app_mention` and
    // `message.channels`; redeliveries repeat both.
    let dedup_key = format!("{channel}:{ts}");
    if seen.contains(&dedup_key) {
        return None;
    }
    if seen.len() >= DEDUP_CAPACITY {
        seen.pop_front();
    }
    seen.push_back(dedup_key);

    let raw_text = event["text"].as_str().unwrap_or("");
    let mention_tag = format!("<@{bot_user_id}>");
    let is_mention = event_type == "app_mention" || raw_text.contains(&mention_tag);
    Some(MessageEvent {
        channel,
        channel_type: event["channel_type"].as_str().unwrap_or("").to_owned(),
        user: user.map(str::to_owned),
        text: raw_text.replace(&mention_tag, "").trim().to_owned(),
        ts,
        thread_ts: event["thread_ts"].as_str().map(str::to_owned),
        is_mention,
    })
}
