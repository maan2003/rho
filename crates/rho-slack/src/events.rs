//! Websocket frames, reduced to the handful rho acts on.
//!
//! Slack sends around fifty event types down the RTM socket. Rho handles the
//! ones that change what the user sees — a message, a thread reply, a read
//! marker moving because they read elsewhere — plus the three that keep the
//! socket alive. Everything else is ignored by name, not by accident: an
//! unrecognised frame is [`WsEvent::Ignored`], which the tests assert on.

use serde_json::Value;

use crate::api::parse_message;
use crate::types::{ChannelId, Message, Ts};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WsEvent {
    /// The socket is live. Slack sends this before anything else.
    Hello,
    /// A reply to our keepalive. Its absence is what detects a dead socket.
    Pong,
    /// A fresher URL to reconnect with; Slack rotates it while connected.
    ReconnectUrl(String),
    /// A new message, in a channel, a DM, or a thread.
    Message(Box<Message>),
    /// The conversation was read somewhere else (phone, web), up to `ts`.
    /// The card it raised should go quiet without the user acting in rho.
    Marked {
        channel: ChannelId,
        ts: Ts,
    },
    Ignored,
}

pub fn parse(frame: &Value) -> WsEvent {
    match frame["type"].as_str().unwrap_or_default() {
        "hello" => WsEvent::Hello,
        "pong" => WsEvent::Pong,
        "reconnect_url" => match frame["url"].as_str() {
            Some(url) if !url.is_empty() => WsEvent::ReconnectUrl(url.to_owned()),
            _ => WsEvent::Ignored,
        },
        "message" => parse_message_frame(frame),
        // `thread` frames announce activity in a thread; the reply itself
        // arrives as an ordinary `message` with a `thread_ts`, so the model
        // learns nothing new here and the frame is deliberately dropped.
        "thread" => WsEvent::Ignored,
        "channel_marked" | "im_marked" | "group_marked" | "thread_marked" => {
            match (frame["channel"].as_str(), frame["ts"].as_str()) {
                (Some(channel), Some(ts)) => WsEvent::Marked {
                    channel: ChannelId(channel.to_owned()),
                    ts: Ts(ts.to_owned()),
                },
                _ => WsEvent::Ignored,
            }
        }
        _ => WsEvent::Ignored,
    }
}

fn parse_message_frame(frame: &Value) -> WsEvent {
    // Edits, deletions, and joins all arrive as `message` with a subtype.
    // Only `bot_message` and a plain message are content the reader wants;
    // the rest would rewrite history under a reader mid-thread.
    match frame["subtype"].as_str() {
        None | Some("bot_message") | Some("thread_broadcast") | Some("file_share") => {}
        Some(_) => return WsEvent::Ignored,
    }
    let channel = ChannelId(frame["channel"].as_str().unwrap_or_default().to_owned());
    match parse_message(frame, &channel) {
        Some(message) if !message.channel.0.is_empty() => WsEvent::Message(Box::new(message)),
        _ => WsEvent::Ignored,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn keepalive_and_reconnect_frames_are_recognised() {
        assert_eq!(parse(&json!({"type": "hello"})), WsEvent::Hello);
        assert_eq!(
            parse(&json!({"type": "pong", "reply_to": 3})),
            WsEvent::Pong
        );
        assert_eq!(
            parse(&json!({"type": "reconnect_url", "url": "wss://new"})),
            WsEvent::ReconnectUrl("wss://new".into())
        );
        assert_eq!(parse(&json!({"type": "reconnect_url"})), WsEvent::Ignored);
    }

    #[test]
    fn messages_carry_their_channel_and_thread() {
        let WsEvent::Message(message) = parse(&json!({
            "type": "message",
            "channel": "C1",
            "ts": "20.0",
            "thread_ts": "10.0",
            "user": "U2",
            "text": "any update?",
        })) else {
            panic!("a plain message is a message");
        };
        assert_eq!(message.channel, ChannelId("C1".into()));
        assert_eq!(message.thread_root(), Ts("10.0".into()));
    }

    #[test]
    fn edits_deletions_and_unknown_types_are_ignored_by_name() {
        assert_eq!(
            parse(
                &json!({"type": "message", "subtype": "message_changed", "channel": "C1", "ts": "1.0"})
            ),
            WsEvent::Ignored
        );
        assert_eq!(
            parse(
                &json!({"type": "message", "subtype": "message_deleted", "channel": "C1", "ts": "1.0"})
            ),
            WsEvent::Ignored
        );
        assert_eq!(parse(&json!({"type": "user_typing"})), WsEvent::Ignored);
        // A message with no channel cannot be placed, so it is not content.
        assert_eq!(
            parse(&json!({"type": "message", "ts": "1.0"})),
            WsEvent::Ignored
        );
    }

    #[test]
    fn read_markers_from_other_clients_are_carried_through() {
        assert_eq!(
            parse(&json!({"type": "im_marked", "channel": "D1", "ts": "30.0"})),
            WsEvent::Marked {
                channel: ChannelId("D1".into()),
                ts: Ts("30.0".into()),
            }
        );
        assert_eq!(
            parse(&json!({"type": "channel_marked", "channel": "C1", "ts": "31.0"})),
            WsEvent::Marked {
                channel: ChannelId("C1".into()),
                ts: Ts("31.0".into()),
            }
        );
    }
}
