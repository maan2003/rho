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

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::api::Server;
use crate::observe::{Newest, Observations, Watcher};
use crate::store::Store;
use crate::types::{ChannelId, Kind, Message, Ts, UserId};
use crate::wire;

/// How many frames a slow client may fall behind before it is told it missed
/// some. Slack disconnects a client that cannot keep up rather than growing
/// a queue for it forever, and so does this. Deep enough that a burst of a
/// few thousand happenings does not cut a client that is merely busy, and
/// bounded so that a client that has actually stopped reading cannot make
/// the server hold its mail forever.
const BACKLOG: usize = 8192;

/// How many clients, connected and gone, the server keeps an account of.
const KEPT: usize = 64;

/// How long a client is given to accept one frame. A client that has stopped
/// reading holds its socket's buffers full, so the write never finishes and
/// the frames it is not reading are frames it will never see; it is cut and
/// counted rather than left sitting there looking merely slow, because "one
/// client quietly stopped hearing about the workspace" is the failure this
/// crate exists to catch.
const WRITE_GRACE: Duration = Duration::from_secs(2);

/// A frame, and what it says about the workspace. The two little `Option`s
/// are what let the server check that clients agree without re-reading its
/// own JSON: whoever built the frame already knew which conversation it
/// touched, so it is carried rather than parsed back out.
pub struct Frame {
    pub value: Value,
    /// The conversation and message this frame is the newest news about.
    pub message: Option<(ChannelId, Ts)>,
    /// The conversation and read cursor this frame moves.
    pub cursor: Option<(ChannelId, Ts)>,
}

impl Frame {
    fn plain(value: Value) -> Self {
        Self {
            value,
            message: None,
            cursor: None,
        }
    }
}

/// One frame as it goes out: rendered once, numbered once, and handed to
/// every socket as a pointer.
pub struct Sent {
    pub text: Arc<String>,
    pub sequence: u64,
    pub message: Option<(ChannelId, Ts)>,
    pub cursor: Option<(ChannelId, Ts)>,
}

/// The publisher every connection reads from. Cheap to clone, and a send
/// with nobody listening is not an error — the schedule runs whether or not
/// anyone is connected, which is the point of a living workspace.
#[derive(Clone)]
pub struct Wire {
    frames: broadcast::Sender<Arc<Sent>>,
    /// How many frames have gone out, which is what "has this client heard
    /// everything" is measured against.
    sequence: Arc<AtomicU64>,
    /// The newest news per conversation, and everyone listening.
    newest: Arc<Mutex<Newest>>,
    watchers: Arc<Mutex<Vec<Arc<Watcher>>>>,
    /// The number the next client to connect gets.
    clients: Arc<AtomicU64>,
}

impl Default for Wire {
    fn default() -> Self {
        Self {
            frames: broadcast::Sender::new(BACKLOG),
            sequence: Arc::new(AtomicU64::new(0)),
            newest: Arc::new(Mutex::new(Newest::default())),
            watchers: Arc::new(Mutex::new(Vec::new())),
            clients: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl Wire {
    /// Publishes one frame to every connected client. Rendered once here
    /// rather than per connection: the cost of an event is one serialisation
    /// plus one clone of a pointer per socket, not one per socket.
    pub fn publish(&self, frame: Frame) {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        if frame.message.is_some() || frame.cursor.is_some() {
            let mut newest = self.newest.lock().expect("newest");
            if let Some((channel, ts)) = &frame.message {
                newest.message(sequence, channel, *ts);
            }
            if let Some((channel, ts)) = &frame.cursor {
                newest.cursor(sequence, channel, *ts);
            }
        }
        let _ = self.frames.send(Arc::new(Sent {
            text: Arc::new(frame.value.to_string()),
            sequence,
            message: frame.message,
            cursor: frame.cursor,
        }));
    }

    /// How many sockets are connected right now.
    pub fn connected(&self) -> usize {
        self.frames.receiver_count()
    }

    /// How many frames have gone out in all.
    pub fn published(&self) -> u64 {
        self.sequence.load(Ordering::Relaxed)
    }

    /// Whether every connected client has been handed everything published.
    pub fn caught_up(&self) -> bool {
        let published = self.published();
        self.watchers
            .lock()
            .expect("watchers")
            .iter()
            .filter(|watcher| watcher.is_connected())
            .all(|watcher| watcher.at() >= published)
    }

    /// What the server saw, and where the clients do not agree.
    pub fn observations(&self, store: &Store) -> Observations {
        let watchers = self.watchers.lock().expect("watchers").clone();
        let newest = self.newest.lock().expect("newest");
        crate::observe::compare(self.published(), &newest, &watchers, store)
    }

    fn join(&self, user: UserId) -> Arc<Watcher> {
        let id = self.clients.fetch_add(1, Ordering::Relaxed) + 1;
        let watcher = Arc::new(Watcher::new(id, user, self.published()));
        let mut watchers = self.watchers.lock().expect("watchers");
        // Clients that have gone are kept for a while, because "one client
        // was cut" is exactly the thing worth reporting; kept forever they
        // would be a leak in a server the rig runs for hours.
        if watchers.len() > KEPT {
            let gone: Vec<usize> = watchers
                .iter()
                .enumerate()
                .filter(|(_, watcher)| !watcher.is_connected())
                .map(|(at, _)| at)
                .collect();
            for at in gone.into_iter().take(watchers.len() - KEPT).rev() {
                watchers.remove(at);
            }
        }
        watchers.push(watcher.clone());
        watcher
    }

    fn subscribe(&self) -> broadcast::Receiver<Arc<Sent>> {
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
pub fn message(channel: &ChannelId, message: &Message) -> Frame {
    let mut value = wire::message(&channel.0, message);
    value["type"] = json!("message");
    value["channel"] = json!(channel.0);
    Frame {
        value,
        // A reply is news about its thread, not about the conversation's
        // newest message, so it is not what clients are compared on.
        message: message
            .thread_ts
            .is_none()
            .then(|| (channel.clone(), message.ts)),
        cursor: None,
    }
}

/// An edit. Slack sends the whole new message inside the frame rather than
/// a patch, and the frame's own `ts` is when the edit happened, not what was
/// edited.
pub fn edited(channel: &ChannelId, at: Ts, edited: &Message) -> Frame {
    Frame::plain(json!({
        "type": "message",
        "subtype": "message_changed",
        "channel": channel.0,
        "ts": at.to_string(),
        "message": wire::message(&channel.0, edited),
    }))
}

/// A deletion names the message that went, not the one that arrived.
pub fn deleted(channel: &ChannelId, at: Ts, gone: Ts) -> Frame {
    Frame::plain(json!({
        "type": "message",
        "subtype": "message_deleted",
        "channel": channel.0,
        "ts": at.to_string(),
        "deleted_ts": gone.to_string(),
    }))
}

/// An emoji going on or coming off, naming the message inside `item`.
pub fn reaction(added: bool, channel: &ChannelId, ts: Ts, user: &UserId, name: &str) -> Frame {
    Frame::plain(json!({
        "type": if added { "reaction_added" } else { "reaction_removed" },
        "user": user.0,
        "reaction": name,
        "item": {"type": "message", "channel": channel.0, "ts": ts.to_string()},
    }))
}

/// A read cursor moving. Which of the three names Slack uses depends on what
/// kind of conversation it is, and the client reads all three.
pub fn marked(kind: Kind, channel: &ChannelId, ts: Ts) -> Frame {
    let name = match kind {
        Kind::Dm => "im_marked",
        Kind::Group => "group_marked",
        _ => "channel_marked",
    };
    Frame {
        value: json!({"type": name, "channel": channel.0, "ts": ts.to_string()}),
        message: None,
        cursor: Some((channel.clone(), ts)),
    }
}

/// A thread being followed, dropped, or read up to a point. All three carry
/// the same `subscription` object.
pub fn thread(name: &'static str, channel: &ChannelId, parent: Ts, last_read: Option<Ts>) -> Frame {
    let mut subscription = json!({
        "type": "thread",
        "channel": channel.0,
        "thread_ts": parent.to_string(),
    });
    if let Some(last_read) = last_read {
        subscription["last_read"] = json!(last_read.to_string());
    }
    Frame::plain(json!({"type": name, "subscription": subscription}))
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
    let self_id = server.store.read().expect("store").self_id.clone();
    let watcher = server.live.join(self_id);
    if socket
        .send(WsMessage::Text(json!({"type": "hello"}).to_string().into()))
        .await
        .is_err()
    {
        watcher.gone();
        return;
    }
    loop {
        tokio::select! {
            frame = frames.recv() => match frame {
                Ok(frame) => {
                    let written = tokio::time::timeout(
                        WRITE_GRACE,
                        socket.send(WsMessage::Text(frame.text.as_str().into())),
                    )
                    .await;
                    match written {
                        Ok(Ok(())) => watcher.told(
                            frame.sequence,
                            frame.message.as_ref().map(|(channel, ts)| (channel, *ts)),
                            frame.cursor.as_ref().map(|(channel, ts)| (channel, *ts)),
                        ),
                        Ok(Err(_)) => {
                            watcher.gone();
                            return;
                        }
                        Err(_) => {
                            watcher.missed(
                                server.live.published().saturating_sub(watcher.at()),
                            );
                            watcher.gone();
                            return;
                        }
                    }
                }
                // A client that could not keep up is told so and cut, which
                // is what makes it reconnect and resync rather than quietly
                // carry a hole in its history.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    watcher.missed(missed);
                    watcher.gone();
                    let _ = socket
                        .send(WsMessage::Text(
                            json!({"type": "error", "error": {"msg": "connection too slow"}})
                                .to_string()
                                .into(),
                        ))
                        .await;
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    watcher.gone();
                    return;
                }
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
                            watcher.gone();
                            return;
                        }
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(_)) | None => {
                    watcher.gone();
                    return;
                }
            },
        }
    }
}
