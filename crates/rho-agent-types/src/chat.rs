//! Chat: a forest of messages, kept by each host in one log
//! ([ARCH-agent-host](../../../agent-host/specs/ARCH-agent-host.md)).
//!
//! A host's chat is one log of [`ChatEvent`]s, dense from one. A message is
//! named by the position of the `Posted` event that made it, and the thread
//! under a message is named by the message: conversation `41` is every
//! message whose parent is `#41`. A message with no parent is a root, which
//! a project's anchor when a [`ChatEvent::Project`] names it. Children are
//! derived by the fold, never stored, so a move is a parent change and the
//! subtree follows.
//!
//! Chat and the agents are two hierarchies that meet only here: an agent
//! receives the user's words because it is a member of the thread they
//! were posted in, or of a thread above it, and another agent's words
//! because they were written to it ([`Voice::ToAgent`]). A client holds the
//! whole log and folds it; the host folds the same events into its indexes
//! in the commit that appends them.

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

/// What the user saw on one dealt card, independent of the route of a reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct MessageVersion {
    pub message: MessageId,
    pub version: ChatSeq,
}

/// A thread-wide reply may settle a dealt card; it never names a recipient.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ThreadReply {
    pub settles: Option<MessageVersion>,
}

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
    /// A start declined before there was an agent to own the stop.
    StartFailed,
}

/// How a post stands with the user, fixed when it is posted
/// (`product/chat.md`: a message is the record, a flash is the now).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Voice {
    /// Kept in the record. A message in a thread from the project (an
    /// agent, a stop or a failed start) pings the user and is dealt; the
    /// user's own words are messages that every member of the thread reads.
    Message,
    /// Shown to the user for twenty minutes from its post, then gone. It
    /// never pings and no agent receives it.
    Flash,
    /// One agent's words to another, kept under the project. Only that
    /// agent receives it, the user can read it, and it never pings.
    ToAgent(AgentId),
}

pub const FLASH_LIFETIME: std::time::Duration = std::time::Duration::from_secs(20 * 60);

/// One change to a host's chat.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ChatEvent {
    /// A message under `parent`, or a root. The user's words reach every
    /// member of the thread it is posted in, or of a thread above; the
    /// project's words reach the user; [`Voice::ToAgent`] reaches its agent.
    /// Posting never makes anyone a member.
    Posted {
        parent: Option<MessageId>,
        /// The failed start this user post retries.
        retry_of: Option<MessageId>,
        /// Absent on older posts. A present reply never implicitly settles
        /// earlier cards in the thread.
        thread_reply: Option<ThreadReply>,
        from: Address,
        voice: Voice,
        content: Vec<ContentPart>,
    },
    /// A root becomes a project, or its settings change: where, how and on
    /// what model its next idea starts. The latest event for a root wins.
    Project {
        project: crate::ProjectId,
        config: crate::ProjectConfig,
    },
    /// A message now under `parent`, or a root, wherever it was. Its
    /// thread goes with it. A move delivers nothing again: who received a
    /// message was fixed when it was posted.
    Reparented {
        message: MessageId,
        parent: Option<MessageId>,
    },
    /// An agent becomes a member of a thread: it receives every later
    /// message the user writes in it and in the threads below it, until it
    /// leaves one.
    Joined {
        thread: MessageId,
        agent: AgentId,
    },
    /// An agent stops being a member of a thread and of the threads below
    /// it.
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
    /// An agent replaces the words of a message without making a new one;
    /// its voice stays as it was posted.
    Revised {
        message: MessageId,
        author: AgentId,
        content: Vec<ContentPart>,
    },
    /// The turn that wrote this version was rewound. Keep its words readable.
    TakenBack {
        message: MessageId,
        version: ChatSeq,
        author: AgentId,
    },
    /// A system stop no longer needs an answer because work resumed.
    Recovered {
        message: MessageId,
        content: Vec<ContentPart>,
    },
    /// The agent that stands for a project (`one` mode) changes: it reads
    /// every idea sent `in: one`, and its workset is called by the project
    /// alone. `None`: no agent stands for it until the next such idea.
    Standing {
        project: crate::ProjectId,
        agent: Option<AgentId>,
    },
    /// The project is done with: no idea starts in it and no device offers
    /// it on a `to:` line. Its threads stay readable and searchable.
    Retired {
        project: crate::ProjectId,
    },
}

/// What a thread's agents are doing beside its messages, for as long as it
/// is true. It is never logged: the messages are the history.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ThreadWork {
    /// An agent works in the thread now.
    Working,
    /// The user stopped the thread's agent, and none works in it now.
    StoppedByYou,
}

impl ThreadWork {
    pub fn words(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::StoppedByYou => "stopped by you",
        }
    }
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
    pub retry_of: Option<MessageId>,
    pub thread_reply: Option<ThreadReply>,
    pub from: Address,
    pub voice: Voice,
    pub content: Vec<ContentPart>,
    pub revision: Revision,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Revision {
    Original,
    Edited {
        seq: ChatSeq,
        at: UnixMs,
    },
    /// Retains the anchor for replies and old references, but no visible
    /// words.
    Deleted,
    TakenBack {
        seq: ChatSeq,
        at: UnixMs,
    },
    /// A stop whose agent worked again: its words say so, and it no
    /// longer waits for the user.
    Recovered {
        seq: ChatSeq,
        at: UnixMs,
    },
}

impl Message {
    /// The message a record posts, if it posts one, under the parent it was
    /// posted under.
    pub fn from_record(record: &ChatRecord) -> Option<Self> {
        let ChatEvent::Posted {
            parent,
            retry_of,
            thread_reply,
            from,
            voice,
            content,
        } = &record.event
        else {
            return None;
        };
        Some(Self {
            id: MessageId(record.seq),
            at: record.at,
            parent: *parent,
            retry_of: *retry_of,
            thread_reply: *thread_reply,
            from: *from,
            voice: *voice,
            content: content.clone(),
            revision: Revision::Original,
        })
    }

    pub fn apply_revision(&mut self, record: &ChatRecord) {
        let (content, writer, seq, at) = match &record.event {
            // Old edits changed the words on screen but were never part of
            // the verdict cursor. Do not reopen historical messages.
            ChatEvent::Edited { message, content } if *message == self.id => {
                (content, None, self.id.0, self.at)
            }
            ChatEvent::Revised {
                message,
                author,
                content,
            } if *message == self.id => (content, Some(*author), record.seq, record.at),
            ChatEvent::TakenBack { message, .. } if *message == self.id => {
                let (seq, at) = self.version();
                self.revision = Revision::TakenBack { seq, at };
                return;
            }
            ChatEvent::Recovered { message, content } if *message == self.id => {
                self.content.clone_from(content);
                self.revision = Revision::Recovered {
                    seq: record.seq,
                    at: record.at,
                };
                return;
            }
            ChatEvent::Deleted { message } if *message == self.id => {
                self.content.clear();
                self.revision = Revision::Deleted;
                return;
            }
            _ => return,
        };
        self.content.clone_from(content);
        if let Some(writer) = writer {
            self.from = Address::Agent(writer);
        }
        self.revision = Revision::Edited { seq, at };
    }

    /// The position and time of the words now shown, not of the message's
    /// anchor.
    pub fn version(&self) -> (ChatSeq, UnixMs) {
        match self.revision {
            Revision::Edited { seq, at }
            | Revision::TakenBack { seq, at }
            | Revision::Recovered { seq, at } => (seq, at),
            _ => (self.id.0, self.at),
        }
    }

    /// When the current wait began. An edit before a verdict keeps that
    /// clock; a version after a verdict can begin another wait.
    pub fn ping_since(&self, seen: Option<ChatSeq>) -> (ChatSeq, UnixMs) {
        if seen.is_some_and(|seen| self.id.0 <= seen) {
            self.version()
        } else {
            (self.id.0, self.at)
        }
    }

    /// Whether the project wrote it: an agent or the host, not the user.
    pub fn from_project(&self) -> bool {
        self.from != Address::User
    }

    /// Whether the user reads it as the project's words or their own: a
    /// message or a flash, never one agent's words to another.
    pub fn to_user(&self) -> bool {
        !matches!(self.voice, Voice::ToAgent(_))
    }

    /// Whether it waits for the user as a card: the project's message in a
    /// thread, until it is taken back, recovered or deleted. A root names a
    /// project and is never dealt.
    pub fn pings_user(&self) -> bool {
        self.voice == Voice::Message
            && self.from_project()
            && self.parent.is_some()
            && matches!(self.revision, Revision::Original | Revision::Edited { .. })
    }

    /// The first post starts the clock; `version()` may change independently.
    pub fn flash_deadline(&self) -> Option<UnixMs> {
        (self.voice == Voice::Flash && self.revision != Revision::Deleted)
            .then(|| self.at + FLASH_LIFETIME)
    }

    /// The message's text, images left out.
    pub fn text(&self) -> String {
        content_text(&self.content)
    }

    /// What the thread under this message is called when nothing named it:
    /// its first line.
    pub fn headline(&self) -> String {
        // A thread's name only needs its first nonempty line. Do not
        // concatenate (or even walk) the rest of a long message to name it.
        first_line(text_parts(&self.content).flat_map(str::lines))
    }
}

fn text_parts(content: &[ContentPart]) -> impl Iterator<Item = &str> {
    content.iter().filter_map(|part| match part {
        ContentPart::Text { text } => Some(text.as_str()),
        ContentPart::Image { .. } => None,
    })
}

/// The text of a message's parts, joined.
pub fn content_text(content: &[ContentPart]) -> String {
    text_parts(content).collect::<Vec<_>>().join("\n")
}

/// The first line, whitespace folded: the words a person remembers.
pub fn one_line(text: &str) -> String {
    first_line(text.lines())
}

fn first_line<'a>(lines: impl Iterator<Item = &'a str>) -> String {
    let mut words = lines
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .split_whitespace();
    let mut first = words.next().unwrap_or_default().to_owned();
    for word in words {
        first.push(' ');
        first.push_str(word);
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(n: u64) -> AgentId {
        AgentId::from_counter(n, &crate::AgentIdDomain(7)).unwrap()
    }

    fn posted(seq: u64, parent: Option<u64>, from: Address, voice: Voice) -> ChatRecord {
        ChatRecord {
            seq: ChatSeq(seq),
            at: UnixMs(seq),
            event: ChatEvent::Posted {
                parent: parent.map(|parent| MessageId(ChatSeq(parent))),
                retry_of: None,
                thread_reply: None,
                from,
                voice,
                content: vec![ContentPart::Text {
                    text: "words".into(),
                }],
            },
        }
    }

    #[test]
    fn headline_takes_the_first_nonempty_line_across_parts() {
        let mut message =
            Message::from_record(&posted(1, None, Address::User, Voice::Message)).unwrap();
        message.content = vec![
            ContentPart::Text {
                text: "\n \t\r".into(),
            },
            ContentPart::Text {
                text: "\n  Réparer\t l’horloge\r\nfollow-ups".into(),
            },
            ContentPart::Text {
                text: "unrelated later part".into(),
            },
        ];
        assert_eq!(message.headline(), "Réparer l’horloge");
        assert_eq!(message.headline(), one_line(&message.text()));
    }

    /// Only the project's message in a thread pings: never a flash, a word
    /// to another agent, the user's own words, or a root.
    #[test]
    fn only_the_projects_message_in_a_thread_pings() {
        let a = Address::Agent(agent(1));
        let stop = Address::System {
            agent: agent(1),
            stop: crate::Stop::Failed,
            since: UnixMs(1),
        };
        for (record, pings) in [
            (posted(3, Some(2), a, Voice::Message), true),
            (posted(3, Some(2), stop, Voice::Message), true),
            (
                posted(3, Some(2), Address::StartFailed, Voice::Message),
                true,
            ),
            (posted(3, Some(2), a, Voice::Flash), false),
            (posted(3, Some(2), a, Voice::ToAgent(agent(2))), false),
            (posted(3, Some(2), Address::User, Voice::Message), false),
            (posted(3, None, a, Voice::Message), false),
        ] {
            let message = Message::from_record(&record).unwrap();
            assert_eq!(message.pings_user(), pings, "{record:?}");
        }
    }

    #[test]
    fn a_take_back_or_recovery_settles_the_ping_and_an_edit_keeps_it() {
        let a = agent(4);
        let mut message =
            Message::from_record(&posted(5, Some(2), Address::Agent(a), Voice::Message)).unwrap();
        message.apply_revision(&ChatRecord {
            seq: ChatSeq(6),
            at: UnixMs(6),
            event: ChatEvent::Revised {
                message: message.id,
                author: a,
                content: Vec::new(),
            },
        });
        assert!(message.pings_user());
        assert_eq!(message.version(), (ChatSeq(6), UnixMs(6)));
        let mut taken = message.clone();
        taken.apply_revision(&ChatRecord {
            seq: ChatSeq(7),
            at: UnixMs(7),
            event: ChatEvent::TakenBack {
                message: message.id,
                version: ChatSeq(6),
                author: a,
            },
        });
        assert!(!taken.pings_user());
        message.apply_revision(&ChatRecord {
            seq: ChatSeq(8),
            at: UnixMs(8),
            event: ChatEvent::Recovered {
                message: message.id,
                content: vec![ContentPart::Text {
                    text: "carried on".into(),
                }],
            },
        });
        assert!(!message.pings_user());
        assert_eq!(message.text(), "carried on");
    }

    #[test]
    fn a_message_is_shown_by_its_first_line() {
        let mut record = posted(3, Some(1), Address::User, Voice::Message);
        let ChatEvent::Posted { content, .. } = &mut record.event else {
            unreachable!()
        };
        *content = vec![
            ContentPart::Image {
                media_type: "image/png".into(),
                data: vec![1],
            },
            ContentPart::Text {
                text: "\n\n  Fix   the test \nmore".into(),
            },
        ];
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
                agent: agent(1),
            },
        };
        assert!(Message::from_record(&joined).is_none());
    }
}
