//! What the server knows, in its own words.
//!
//! These are the server's types, not the client's: a real Slack does not
//! share a struct with rho, and neither does this. The wire shapes are made
//! from them at the edge (`wire`), so a field that Slack spells oddly is
//! spelled oddly in one place instead of being carried through the store as
//! a string nobody dares parse.

use std::fmt;

/// A user id, as Slack mints them.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UserId(pub String);

/// A conversation id: a channel, a group or a DM, told apart by `Kind` and
/// not by the letter the id starts with.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChannelId(pub String);

/// A Slack timestamp: whole seconds and the microseconds that order two
/// messages inside one of them. Kept as numbers because the server sorts and
/// compares them constantly; it becomes `1700000000.000100` only on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ts {
    pub seconds: i64,
    pub micros: u32,
}

impl Ts {
    pub const fn new(seconds: i64, micros: u32) -> Self {
        Self { seconds, micros }
    }

    /// Parses the wire form. Returns `None` for anything that is not
    /// `<seconds>.<micros>`, which is what a bad `oldest` or `latest` from a
    /// client looks like.
    pub fn parse(text: &str) -> Option<Self> {
        let (seconds, micros) = text.split_once('.')?;
        Some(Self {
            seconds: seconds.parse().ok()?,
            micros: micros.parse().ok()?,
        })
    }
}

impl fmt::Display for Ts {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "{}.{:06}", self.seconds, self.micros)
    }
}

/// What kind of conversation it is. Slack says this with three booleans on
/// the wire (`is_channel`, `is_group`, `is_im`); it is one thing here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Channel,
    Private,
    Group,
    Dm,
}

#[derive(Clone, Debug)]
pub struct User {
    pub id: UserId,
    /// The handle, which is what `name` is on the wire.
    pub handle: String,
    /// What the person calls themselves, which is what rho shows.
    pub display: String,
    pub avatar: String,
    pub bot: bool,
}

#[derive(Clone, Debug)]
pub struct Conversation {
    pub id: ChannelId,
    pub kind: Kind,
    /// Empty for a DM, where the name is the other person.
    pub name: String,
    /// The other person, for a DM.
    pub user: Option<UserId>,
    pub members: Vec<UserId>,
    pub muted: bool,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub ts: Ts,
    /// Set on a reply; the thread it belongs to.
    pub thread_ts: Option<Ts>,
    pub user: UserId,
    pub text: String,
    pub edited: bool,
    pub reply_count: u32,
    pub latest_reply: Option<Ts>,
    pub reactions: Vec<Reaction>,
    /// Deleted messages stay in place as tombstones. Removing one would move
    /// every row after it, and the positions of those rows are what make the
    /// counts cheap.
    pub deleted: bool,
    /// Whether the text names the signed-in user. Kept beside the message
    /// because the unread counts are derived from it on every `client.counts`
    /// and re-scanning the text there would be a pass over the history.
    pub mentions_self: bool,
}

#[derive(Clone, Debug)]
pub struct Reaction {
    pub name: String,
    pub users: Vec<UserId>,
}
