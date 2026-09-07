//! The websocket side: the frames Slack pushes, and the connection that
//! carries them.
//!
//! The vocabulary here is not invented. It is exactly what `rho-slack`'s
//! `events::parse` reads — a message and its subtypes, reactions, the three
//! read-cursor frames, the three thread-subscription frames, `hello`, `pong`
//! and `reconnect_url` — because a fake that speaks a dialect only proves
//! the dialect.
//!
//! One broadcast channel carries rendered frames to every connected client,
//! which is what makes several clients see the same workspace: a write, from
//! whichever client made it or from the schedule, is published once and
//! every socket gets that same frame.

use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::api::Server;
use crate::types::{ChannelId, Kind, Message, Ts, UserId};
use crate::wire;

/// How many frames a slow client may fall behind before it is told it missed
/// some. Slack disconnects a client that cannot keep up rather than growing
/// a queue for it forever, and so does this. Deep enough that a burst of a
/// few thousand happenings does not cut a client that is merely busy, and
/// bounded so that a client that has actually stopped reading cannot make
/// the server hold its mail forever.
const BACKLOG: usize = 8192;

/// The publisher every connection reads from. Cheap to clone, and a send
/// with nobody listening is not an error — the schedule runs whether or not
/// anyone is connected, which is the point of a living workspace.
#[derive(Clone)]
pub struct Wire {
    frames: broadcast::Sender<Arc<String>>,
}

impl Default for Wire {
    fn default() -> Self {
        Self {
            frames: broadcast::Sender::new(BACKLOG),
        }
    }
}

impl Wire {
    /// Publishes one frame to every connected client. Rendered once here
    /// rather than per connection: the cost of an event is one serialisation
    /// plus one clone of a pointer per socket, not one per socket.
    pub fn publish(&self, frame: Value) {
        let _ = self.frames.send(Arc::new(frame.to_string()));
    }

    /// How many sockets are connected right now.
    pub fn connected(&self) -> usize {
        self.frames.receiver_count()
    }

    fn subscribe(&self) -> broadcast::Receiver<Arc<String>> {
        self.frames.subscribe()
    }
}

/// What `rtm.connect` hands back: where the socket is, who the caller is,
/// and what the workspace is called.
pub fn rtm(url: &str, self_id: &UserId, self_name: &str) -> Value {
    json!({
        "ok": true,
        "url": url,
        "self": {"id": self_id.0, "name": self_name},
        "team": {"id": "T0RHO", "name": "acme"},
    })
}

/// A new message, exactly as a message in a history page but on the wire.
pub fn message(channel: &ChannelId, message: &Message) -> Value {
    let mut frame = wire::message(&channel.0, message);
    frame["type"] = json!("message");
    frame["channel"] = json!(channel.0);
    frame
}

/// An edit. Slack sends the whole new message inside the frame rather than
/// a patch, and the frame's own `ts` is when the edit happened, not what was
/// edited.
pub fn edited(channel: &ChannelId, at: Ts, edited: &Message) -> Value {
    json!({
        "type": "message",
        "subtype": "message_changed",
        "channel": channel.0,
        "ts": at.to_string(),
        "message": wire::message(&channel.0, edited),
    })
}

/// A deletion names the message that went, not the one that arrived.
pub fn deleted(channel: &ChannelId, at: Ts, gone: Ts) -> Value {
    json!({
        "type": "message",
        "subtype": "message_deleted",
        "channel": channel.0,
        "ts": at.to_string(),
        "deleted_ts": gone.to_string(),
    })
}

/// An emoji going on or coming off, naming the message inside `item`.
pub fn reaction(added: bool, channel: &ChannelId, ts: Ts, user: &UserId, name: &str) -> Value {
    json!({
        "type": if added { "reaction_added" } else { "reaction_removed" },
        "user": user.0,
        "reaction": name,
        "item": {"type": "message", "channel": channel.0, "ts": ts.to_string()},
    })
}

/// A read cursor moving. Which of the three names Slack uses depends on what
/// kind of conversation it is, and the client reads all three.
pub fn marked(kind: Kind, channel: &ChannelId, ts: Ts) -> Value {
    let name = match kind {
        Kind::Dm => "im_marked",
        Kind::Group => "group_marked",
        _ => "channel_marked",
    };
    json!({"type": name, "channel": channel.0, "ts": ts.to_string()})
}

/// A thread being followed, dropped, or read up to a point. All three carry
/// the same `subscription` object.
pub fn thread(name: &'static str, channel: &ChannelId, parent: Ts, last_read: Option<Ts>) -> Value {
    let mut subscription = json!({
        "type": "thread",
        "channel": channel.0,
        "thread_ts": parent.to_string(),
    });
    if let Some(last_read) = last_read {
        subscription["last_read"] = json!(last_read.to_string());
    }
    json!({"type": name, "subscription": subscription})
}

/// The upgrade handler. The session is checked the way the web API checks
/// it, because Slack can accept the upgrade and then refuse the session, and
/// the client is written to handle exactly that.
pub async fn connect(upgrade: WebSocketUpgrade, State(server): State<Server>) -> Response {
    upgrade.on_upgrade(move |socket| hold(socket, server))
}

/// One connection's life: `hello`, then everything that happens, until the
/// client goes away or falls too far behind.
async fn hold(mut socket: WebSocket, server: Server) {
    let mut frames = server.live.subscribe();
    if socket
        .send(WsMessage::Text(json!({"type": "hello"}).to_string().into()))
        .await
        .is_err()
    {
        return;
    }
    loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Ok(frame) => {
                    if socket.send(WsMessage::Text(frame.as_str().into())).await.is_err() {
                        return;
                    }
                }
                // A client that could not keep up is told so and cut, which
                // is what makes it reconnect and resync rather than quietly
                // carry a hole in its history.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let _ = socket
                        .send(WsMessage::Text(
                            json!({"type": "error", "error": {"msg": "connection too slow"}})
                                .to_string()
                                .into(),
                        ))
                        .await;
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            incoming = socket.recv() => match incoming {
                Some(Ok(WsMessage::Text(text))) => {
                    let Ok(value) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };
                    // The client's liveness check. Answering it is the whole
                    // protocol obligation of this direction.
                    if value["type"] == json!("ping") {
                        let pong = json!({"type": "pong", "reply_to": value["id"]});
                        if socket
                            .send(WsMessage::Text(pong.to_string().into()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return,
            },
        }
    }
}
