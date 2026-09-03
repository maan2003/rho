//! Wire identities and the shapes rho keeps from Slack's payloads.
//!
//! Slack's ids and message timestamps are opaque strings that identify but
//! never read: they are newtypes here so nothing formats one into a label by
//! accident. The only human-facing names are channel and user names, which
//! the model resolves.

use serde::{Deserialize, Serialize};

use crate::config::WorkspaceName;

macro_rules! opaque_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}

opaque_id!(ChannelId, "A channel, group, or DM conversation id.");
opaque_id!(UserId, "A member of the workspace.");

/// A Slack message timestamp: both the message's id within its channel and
/// its send time. Never rendered — the surface shows a clock time derived
/// from it, and the dealer compares them.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Ts(pub String);

impl Ts {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Seconds since the epoch, for ordering and for the clock time shown
    /// next to a message. A malformed timestamp sorts oldest rather than
    /// panicking: a thread that renders slightly out of order beats a client
    /// that dies on one odd frame.
    pub fn epoch_seconds(&self) -> f64 {
        self.0.parse().unwrap_or(0.0)
    }

    pub fn millis(&self) -> i64 {
        (self.epoch_seconds() * 1000.0) as i64
    }

    /// Slack's own ordering: numeric, not lexicographic, because the integer
    /// part grows a digit every few years.
    pub fn is_newer_than(&self, other: &Self) -> bool {
        self.epoch_seconds() > other.epoch_seconds()
    }
}

impl From<&str> for Ts {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// The identity of one thread, everywhere in rho: the dealer card, the
/// surface, the desk node, and the journal all key on this.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ThreadKey {
    pub workspace: WorkspaceName,
    pub channel: ChannelId,
    /// The parent message of the thread. A mention that is not in a thread
    /// is its own thread root, which is exactly how a reply to it behaves.
    pub thread_ts: Ts,
}

/// Why a thread is rho's business at all. Channel traffic the user was not
/// addressed in never becomes an item, so this is a closed set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reason {
    /// The user was named, by handle, group, or a channel-wide broadcast.
    Mention,
    /// A direct message.
    DirectMessage,
    /// A reply in a thread the user has posted in.
    Thread,
}

/// Where a conversation lives, for the one line of chrome above a thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConversationKind {
    Channel,
    Group,
    DirectMessage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversation {
    pub id: ChannelId,
    pub kind: ConversationKind,
    pub name: String,
    /// The other person, for a one-to-one DM. Slack names a DM only by that
    /// id, so the label waits on the roster.
    pub user: Option<UserId>,
}

impl Conversation {
    /// `#design` or `@ada`: the only address the user ever sees.
    pub fn label(&self) -> String {
        match self.kind {
            ConversationKind::Channel | ConversationKind::Group => format!("#{}", self.name),
            ConversationKind::DirectMessage => format!("@{}", self.name),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub id: UserId,
    /// The display name, falling back to the handle Slack always provides.
    pub name: String,
}

/// One message as rho keeps it: who, when, the rendered text, and the two
/// timestamps that place it in its thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub ts: Ts,
    pub thread_ts: Option<Ts>,
    pub channel: ChannelId,
    pub user: Option<UserId>,
    /// A bot or app post has no user id; its name arrives inline.
    pub bot_name: Option<String>,
    /// Block Kit as received, kept unrendered so names can be resolved later
    /// when a user or channel first becomes known.
    pub blocks: Vec<serde_json::Value>,
    /// The plain `text` field, used when a message carries no blocks.
    pub text: String,
    pub attachments: Vec<Attachment>,
    pub files: Vec<FileSummary>,
}

impl Message {
    /// The thread this message belongs to: its parent, or itself when it is
    /// the parent.
    pub fn thread_root(&self) -> Ts {
        self.thread_ts.clone().unwrap_or_else(|| self.ts.clone())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    pub title: Option<String>,
    pub text: Option<String>,
    pub fallback: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSummary {
    pub title: String,
}
