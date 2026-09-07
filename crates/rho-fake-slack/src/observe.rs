//! What the server saw every client be told.
//!
//! With several clients on one workspace the interesting question is not
//! whether one of them is right, it is whether they agree — a message one
//! was told about is a message all of them were told about, and a read
//! cursor that moved moved for everyone. The server is the only place that
//! can answer that, because it is the only place that knows what it sent to
//! whom.
//!
//! The bookkeeping is deliberately not a transcript. Per frame per client it
//! is two counters and one entry in a map keyed by the conversation the
//! frame touched, so a client costs O(conversations it heard about) and a
//! frame costs O(clients), never a walk of the workspace. The comparison
//! itself happens when someone asks for it, against the newest thing
//! published per conversation rather than against a log of everything that
//! ever happened.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::store::Store;
use crate::types::{ChannelId, Ts, UserId};

/// One connected client, from the server's side.
pub struct Watcher {
    pub id: u64,
    pub user: UserId,
    /// The sequence the workspace was at when this client connected. What
    /// happened before that was never its to hear.
    pub from: u64,
    delivered: AtomicU64,
    missed: AtomicU64,
    at: AtomicU64,
    connected: AtomicBool,
    heard: Mutex<HashMap<ChannelId, Heard>>,
}

/// The last thing a client was told about one conversation.
#[derive(Clone, Copy, Debug, Default)]
struct Heard {
    message: Option<Ts>,
    cursor: Option<Ts>,
}

impl Watcher {
    pub fn new(id: u64, user: UserId, from: u64) -> Self {
        Self {
            id,
            user,
            from,
            delivered: AtomicU64::new(0),
            missed: AtomicU64::new(0),
            at: AtomicU64::new(from),
            connected: AtomicBool::new(true),
            heard: Mutex::new(HashMap::new()),
        }
    }

    /// Records one frame this client was handed. Called on the connection's
    /// own task, so the lock is uncontended except when someone asks for the
    /// observations.
    pub fn told(
        &self,
        sequence: u64,
        message: Option<(&ChannelId, Ts)>,
        cursor: Option<(&ChannelId, Ts)>,
    ) {
        self.delivered.fetch_add(1, Ordering::Relaxed);
        self.at.store(sequence, Ordering::Relaxed);
        if message.is_none() && cursor.is_none() {
            return;
        }
        let mut heard = self.heard.lock().expect("heard");
        if let Some((channel, ts)) = message {
            heard.entry(channel.clone()).or_default().message = Some(ts);
        }
        if let Some((channel, ts)) = cursor {
            heard.entry(channel.clone()).or_default().cursor = Some(ts);
        }
    }

    /// Frames this client was never handed because it could not keep up.
    pub fn missed(&self, frames: u64) {
        self.missed.fetch_add(frames, Ordering::Relaxed);
    }

    pub fn gone(&self) {
        self.connected.store(false, Ordering::Relaxed);
    }

    pub fn at(&self) -> u64 {
        self.at.load(Ordering::Relaxed)
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    fn view(&self) -> ClientView {
        ClientView {
            client: self.id,
            user: self.user.clone(),
            delivered: self.delivered.load(Ordering::Relaxed),
            missed: self.missed.load(Ordering::Relaxed),
            at: self.at(),
            connected: self.is_connected(),
        }
    }
}

/// One client, as a value a test can assert against.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ClientView {
    pub client: u64,
    pub user: UserId,
    pub delivered: u64,
    pub missed: u64,
    /// The sequence of the last frame handed to it.
    pub at: u64,
    pub connected: bool,
}

/// Where clients do not agree. An empty list is the claim the fake exists to
/// support: what one client was told, all of them were told, and it matches
/// what the server holds.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "disagreement", rename_all = "snake_case")]
pub enum Disagreement {
    /// The client has not been handed everything published since it
    /// connected. Ordinary for a moment after a write; a disagreement if it
    /// persists, which is why `settled` exists.
    Behind { client: u64, by: u64 },
    /// The client could not keep up and was cut. Named rather than dropped
    /// quietly, because a hole in one client's history is the bug this whole
    /// crate is for finding.
    Missed { client: u64, frames: u64 },
    /// The newest message in a conversation is not the newest message this
    /// client was told about.
    Message {
        client: u64,
        channel: ChannelId,
        told: Option<Ts>,
        published: Ts,
    },
    /// The reader's cursor moved and this client was told something else, or
    /// was not told at all.
    Cursor {
        client: u64,
        channel: ChannelId,
        told: Option<Ts>,
        held: Option<Ts>,
    },
}

/// Everything the server saw, and everywhere it does not add up.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Observations {
    /// How many frames have been published to the workspace in all.
    pub published: u64,
    pub clients: Vec<ClientView>,
    pub disagreements: Vec<Disagreement>,
}

impl Observations {
    /// Whether every connected client has been told the same things.
    pub fn agree(&self) -> bool {
        self.disagreements.is_empty()
    }
}

/// The newest thing published per conversation, which is what a client's
/// account of that conversation is compared against.
#[derive(Default)]
pub struct Newest {
    messages: HashMap<ChannelId, (u64, Ts)>,
    cursors: HashMap<ChannelId, (u64, Ts)>,
}

impl Newest {
    pub fn message(&mut self, sequence: u64, channel: &ChannelId, ts: Ts) {
        self.messages.insert(channel.clone(), (sequence, ts));
    }

    pub fn cursor(&mut self, sequence: u64, channel: &ChannelId, ts: Ts) {
        self.cursors.insert(channel.clone(), (sequence, ts));
    }
}

/// Compares what each client was told with what was published and with what
/// the server holds. Costs one pass over the conversations something
/// happened in, per client — not over the workspace, and not over history.
pub fn compare(
    published: u64,
    newest: &Newest,
    watchers: &[std::sync::Arc<Watcher>],
    store: &Store,
) -> Observations {
    let mut disagreements = Vec::new();
    let mut clients = Vec::new();
    for watcher in watchers {
        clients.push(watcher.view());
        let missed = watcher.view().missed;
        if missed > 0 {
            disagreements.push(Disagreement::Missed {
                client: watcher.id,
                frames: missed,
            });
        }
        if !watcher.is_connected() {
            continue;
        }
        if watcher.at() < published {
            disagreements.push(Disagreement::Behind {
                client: watcher.id,
                by: published - watcher.at(),
            });
            // Behind is the reason for everything else that would be found
            // about this client, so saying it twice is noise.
            continue;
        }
        let heard = watcher.heard.lock().expect("heard");
        for (channel, (sequence, ts)) in &newest.messages {
            if *sequence <= watcher.from {
                continue;
            }
            let told = heard.get(channel).and_then(|heard| heard.message);
            if told != Some(*ts) {
                disagreements.push(Disagreement::Message {
                    client: watcher.id,
                    channel: channel.clone(),
                    told,
                    published: *ts,
                });
            }
        }
        for (channel, (sequence, _)) in &newest.cursors {
            if *sequence <= watcher.from {
                continue;
            }
            let told = heard.get(channel).and_then(|heard| heard.cursor);
            // The store is the truth for the reader's cursor; a client that
            // was told something else has a badge that will not match.
            let held = store.read_cursor(channel, &store.self_id);
            if told != held {
                disagreements.push(Disagreement::Cursor {
                    client: watcher.id,
                    channel: channel.clone(),
                    told,
                    held,
                });
            }
        }
    }
    Observations {
        published,
        clients,
        disagreements,
    }
}
