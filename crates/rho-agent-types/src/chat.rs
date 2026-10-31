//! Chat: streams of topics of messages, kept by each host in one log
//! ([ARCH-agent-host](../../../agent-host/specs/ARCH-agent-host.md)).
//!
//! A host's chat is one log of [`ChatEvent`]s, dense from one. Everything
//! chat names is named by the position of the event that made it: a
//! [`StreamId`] by its `StreamCreated`, a [`TopicId`] by its `TopicStarted`,
//! a [`MessageId`] by its `Posted`. A client holds the whole log and folds
//! it; the host folds the same events into its indexes in the commit that
//! appends them.

use senax_encoder::{Decode, Encode, Pack, Unpack};

use crate::{AgentId, ContentPart, UnixMs};

/// A position in a host's chat log: dense from one, never reused. Zero is
/// "before any event".
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct ChatSeq(pub u64);

impl ChatSeq {
    pub const ZERO: Self = Self(0);

    pub fn next(self) -> Self {
        Self(self.0.checked_add(1).expect("chat log overflow"))
    }
}

macro_rules! chat_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
        )]
        pub struct $name(pub ChatSeq);
    };
}

chat_id!(
    /// A stream, named by the event that created it.
    StreamId
);
chat_id!(
    /// A topic, named by the event that started it.
    TopicId
);
chat_id!(
    /// A message, named by the event that posted it.
    MessageId
);

/// A chat member: the user or one of the host's agents.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub enum Address {
    User,
    Agent(AgentId),
}

/// What a mention asks of the one it names. A mention of the user is how
/// an agent asks for attention: the dealer ranks on it (`rho-dealer`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum MentionKind {
    /// Asks for something the work needs.
    Ask,
    /// Delivers what was asked for.
    Result,
    /// Worth reading, not now.
    Fyi,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct Mention {
    pub to: Address,
    pub kind: MentionKind,
}

/// One change to a host's chat.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ChatEvent {
    StreamCreated {
        name: String,
    },
    /// A topic in `stream`; its agents receive every message in it.
    TopicStarted {
        stream: StreamId,
        name: String,
        by: Address,
        agents: Vec<AgentId>,
    },
    /// An agent joins a topic and receives its messages from here on.
    Joined {
        topic: TopicId,
        agent: AgentId,
    },
    /// A message in `topic`. Its content never changes. Every agent of the
    /// topic but its sender receives it, and so does each agent it
    /// mentions; posting makes an agent sender one of the topic's agents.
    Posted {
        topic: TopicId,
        from: Address,
        mentions: Vec<Mention>,
        content: Vec<ContentPart>,
    },
}

/// One entry of a host's chat log.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ChatRecord {
    pub seq: ChatSeq,
    pub at: UnixMs,
    pub event: ChatEvent,
}

/// A posted message, with where it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub id: MessageId,
    pub at: UnixMs,
    pub topic: TopicId,
    pub from: Address,
    pub mentions: Vec<Mention>,
    pub content: Vec<ContentPart>,
}

impl Message {
    /// The message a record posts, if it posts one.
    pub fn from_record(record: &ChatRecord) -> Option<Self> {
        let ChatEvent::Posted {
            topic,
            from,
            mentions,
            content,
        } = &record.event
        else {
            return None;
        };
        Some(Self {
            id: MessageId(record.seq),
            at: record.at,
            topic: *topic,
            from: *from,
            mentions: mentions.clone(),
            content: content.clone(),
        })
    }

    pub fn mentions(&self, address: Address) -> Option<MentionKind> {
        self.mentions
            .iter()
            .find(|mention| mention.to == address)
            .map(|mention| mention.kind)
    }
}
