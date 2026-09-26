//! The chat: what the GUI syncs. A filter over the log, append-only, with no
//! cells, output or reasoning in it.

use rho_agent_types::UnixMs;

use crate::log::{Block, Entry, MessageId, Notice, Party};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatEvent {
    /// The entry's position in the log: a sync cursor.
    pub seq: u64,
    pub at: UnixMs,
    pub kind: ChatKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChatKind {
    Message {
        id: MessageId,
        from: Party,
        to: Party,
        body: Vec<Block>,
    },
    /// Replaces the last status.
    Status(String),
    /// Since when the agent waits on the human; `None` once it does not.
    Awaiting(Option<UnixMs>),
    Notice(Notice),
}

/// The chat event an entry makes, if any. `me` is the agent's own id, the
/// other end of a received message.
pub fn project(me: &str, seq: u64, entry: &Entry) -> Option<ChatEvent> {
    let kind = match entry {
        Entry::Received { id, from, body, .. } => ChatKind::Message {
            id: *id,
            from: from.clone(),
            to: Party::Agent(me.to_owned()),
            body: body.clone(),
        },
        Entry::Sent { id, to, text, .. } => ChatKind::Message {
            id: *id,
            from: Party::Agent(me.to_owned()),
            to: to.clone(),
            body: vec![Block::Text(text.clone())],
        },
        Entry::Status { text, .. } => ChatKind::Status(text.clone()),
        Entry::Awaiting { since, .. } => ChatKind::Awaiting(*since),
        Entry::Notice { notice, .. } => ChatKind::Notice(notice.clone()),
        Entry::Created { .. } | Entry::Step { .. } | Entry::Woken { .. } => return None,
    };
    Some(ChatEvent {
        seq,
        at: entry.at(),
        kind,
    })
}

/// The whole chat, from the start of a log.
pub fn chat(me: &str, entries: &[Entry]) -> Vec<ChatEvent> {
    entries
        .iter()
        .enumerate()
        .filter_map(|(seq, entry)| project(me, seq as u64, entry))
        .collect()
}
