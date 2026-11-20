//! Chat: a forest of messages, kept by each host in one log
//! ([ARCH-agent-host](../../../agent-host/specs/ARCH-agent-host.md)).
//!
//! A host's chat is one log of [`ChatEvent`]s, dense from one. A message is
//! named by the position of the `Posted` event that made it, and the thread
//! under a message is named by the message: conversation `41` is every
//! message whose parent is `#41`. A message with no parent is a root, which
//! the GUI shows as a stream. Children are derived by the fold, never
//! stored, so a move is a parent change and the subtree follows.
//!
//! Chat and the agents are two hierarchies that meet only here: an agent
//! receives a message because it is a member of the thread it was posted
//! in, or of a thread above it, or because the message mentions it. A
//! client holds the whole log and folds it; the host folds the same events
//! into its indexes in the commit that appends them.

use senax_encoder::{Decode, Encode, Pack, Unpack};

use crate::{AgentId, ContentPart, Stop, UnixMs};

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

/// A message, named by the event that posted it. The same id names the
/// thread under it.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct MessageId(pub ChatSeq);

/// A chat member: the user or one of the host's agents.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub enum Address {
    User,
    Agent(AgentId),
    /// The host's bot. The stop identity survives even if its agent is deleted.
    System {
        agent: AgentId,
        stop: Stop,
        since: UnixMs,
    },
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
    /// A message under `parent`, or a root.
    /// Every member of the thread it is posted in, or of a thread above,
    /// receives it, and so does each agent it mentions, never its sender.
    /// Neither posting nor a mention makes anyone a member.
    Posted {
        parent: Option<MessageId>,
        from: Address,
        mentions: Vec<Mention>,
        content: Vec<ContentPart>,
    },
    /// A message now under `parent`, or a root, wherever it was. Its
    /// thread goes with it. A move delivers nothing again: who received a
    /// message was fixed when it was posted.
    Reparented {
        message: MessageId,
        parent: Option<MessageId>,
    },
    /// An agent becomes a member of a thread: it receives every later
    /// message in it and in the threads below it, until it leaves one.
    Joined {
        thread: MessageId,
        agent: AgentId,
    },
    /// An agent stops being a member of a thread and of the threads below
    /// it: it receives only the messages that mention it from here on.
    Left {
        thread: MessageId,
        agent: AgentId,
    },
    /// A thread's name. Empty takes a name back: the thread is then shown by
    /// the first line of its message.
    Named {
        thread: MessageId,
        name: String,
    },
    Edited {
        message: MessageId,
        content: Vec<ContentPart>,
    },
    Deleted {
        message: MessageId,
    },
}

/// The current status of a thread, separate from its durable mail.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ThreadStatus {
    pub thread: MessageId,
    pub at: UnixMs,
    pub text: String,
}

/// One entry of a host's chat log.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ChatRecord {
    pub seq: ChatSeq,
    pub at: UnixMs,
    pub event: ChatEvent,
}

/// A posted message, with where it is now.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Message {
    pub id: MessageId,
    pub at: UnixMs,
    /// Its parent now: a later `Reparented` moved it from where it was
    /// posted.
    pub parent: Option<MessageId>,
    pub from: Address,
    pub mentions: Vec<Mention>,
    pub content: Vec<ContentPart>,
    pub revision: Revision,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Revision {
    Original,
    Edited,
    /// Retains the anchor for replies and old references, but no visible mail.
    Deleted,
}

impl Message {
    /// The message a record posts, if it posts one, under the parent it was
    /// posted under.
    pub fn from_record(record: &ChatRecord) -> Option<Self> {
        let ChatEvent::Posted {
            parent,
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
            parent: *parent,
            from: *from,
            mentions: mentions.clone(),
            content: content.clone(),
            revision: Revision::Original,
        })
    }

    pub fn apply_revision(&mut self, event: &ChatEvent) {
        match event {
            ChatEvent::Edited { message, content } if *message == self.id => {
                self.content.clone_from(content);
                self.revision = Revision::Edited;
            }
            ChatEvent::Deleted { message } if *message == self.id => {
                self.content.clear();
                self.mentions.clear();
                self.revision = Revision::Deleted;
            }
            _ => {}
        }
    }

    pub fn mentions(&self, address: Address) -> Option<MentionKind> {
        self.mentions
            .iter()
            .find(|mention| mention.to == address)
            .map(|mention| mention.kind)
    }

    /// The message's text, images left out.
    pub fn text(&self) -> String {
        content_text(&self.content)
    }

    /// What the thread under this message is called when nothing named it:
    /// its first line.
    pub fn headline(&self) -> String {
        one_line(&self.text())
    }
}

/// The text of a message's parts, joined.
pub fn content_text(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            ContentPart::Image { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The first line, whitespace folded: the words a person remembers.
pub fn one_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_is_shown_by_its_first_line() {
        let record = ChatRecord {
            seq: ChatSeq(3),
            at: UnixMs(1),
            event: ChatEvent::Posted {
                parent: Some(MessageId(ChatSeq(1))),
                from: Address::User,
                mentions: Vec::new(),
                content: vec![
                    ContentPart::Image {
                        media_type: "image/png".into(),
                        data: vec![1],
                    },
                    ContentPart::Text {
                        text: "\n\n  Fix   the test \nmore".into(),
                    },
                ],
            },
        };
        let message = Message::from_record(&record).unwrap();
        assert_eq!(message.id, MessageId(ChatSeq(3)));
        assert_eq!(message.parent, Some(MessageId(ChatSeq(1))));
        assert_eq!(message.headline(), "Fix the test");
        assert_eq!(one_line("   "), "");
        let joined = ChatRecord {
            seq: ChatSeq(4),
            at: UnixMs(1),
            event: ChatEvent::Joined {
                thread: MessageId(ChatSeq(3)),
                agent: AgentId::from_counter(1, &crate::AgentIdDomain(7)).unwrap(),
            },
        };
        assert!(Message::from_record(&joined).is_none());
    }
}
