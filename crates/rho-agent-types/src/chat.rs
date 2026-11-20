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
    /// A start declined before there was an agent to own the stop.
    StartFailed,
}

/// Who a message reaches beyond the members of its thread. A mention of
/// the user is mail to the user, which never goes to the thread's agents;
/// whether it pings decides whether it waits for them as a card
/// (`rho-dealer`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Mention {
    /// Mail to the user. A mail that pings is dealt; one that does not is
    /// how the project says what it is doing, edited in place as it works.
    User { ping: bool },
    /// Reaches the agent whether or not it is a member, as well as the
    /// members.
    Agent(AgentId),
    /// Mail to this agent alone, without delivery to thread members.
    Direct(AgentId),
}

impl Mention {
    /// The agent the mention reaches, if it names one.
    pub fn agent(self) -> Option<AgentId> {
        match self {
            Self::Agent(agent) | Self::Direct(agent) => Some(agent),
            Self::User { .. } => None,
        }
    }
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
        /// The specific mail the user answered; independent of where the answer
        /// is posted.
        reply_to: Option<MessageId>,
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
    /// The project replaces the live mail without making a new mail.
    /// Replies to it go to the writer of these words. `ping` turns the
    /// mail's ping to the user on or off; `None` leaves it as it was.
    Revised {
        message: MessageId,
        author: AgentId,
        content: Vec<ContentPart>,
        ping: Option<bool>,
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
}

/// What a thread's agents are doing beside its mail, for as long as it is
/// true. It is never logged: the mail is the history.
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

pub const THREAD_COLD_AFTER: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// The `## recap` section of a mail's text, heading included: the words
/// that let the user answer a cold thread without its history. It runs to
/// the next heading of the same or a higher level, or to the end.
pub fn recap_section(text: &str) -> Option<std::ops::Range<usize>> {
    let mut start = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        let level = trimmed.chars().take_while(|c| *c == '#').count();
        let heading = (1..=2).contains(&level) && trimmed[level..].starts_with(' ');
        match start {
            None if heading && level == 2 && trimmed[2..].trim().eq_ignore_ascii_case("recap") => {
                start = Some(offset);
            }
            Some(start) if heading => return Some(start..offset),
            _ => {}
        }
        offset += line.len();
    }
    start.map(|start| start..text.len())
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
    pub reply_to: Option<MessageId>,
    pub from: Address,
    pub mentions: Vec<Mention>,
    pub content: Vec<ContentPart>,
    /// A revision that started the current ping, rather than the original mail.
    pub ping_started: Option<(ChatSeq, UnixMs)>,
    pub revision: Revision,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Revision {
    Original,
    Edited {
        seq: ChatSeq,
        at: UnixMs,
    },
    /// Retains the anchor for replies and old references, but no visible mail.
    Deleted,
    TakenBack {
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
            reply_to,
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
            reply_to: *reply_to,
            from: *from,
            mentions: mentions.clone(),
            content: content.clone(),
            ping_started: None,
            revision: Revision::Original,
        })
    }

    pub fn apply_revision(&mut self, record: &ChatRecord) {
        let (content, writer, seq, at) = match &record.event {
            // Old edits changed the words on screen but were never part of
            // the verdict cursor. Do not reopen historical mail on upgrade.
            ChatEvent::Edited { message, content } if *message == self.id => {
                (content, None, self.id.0, self.at)
            }
            ChatEvent::Revised {
                message,
                author,
                content,
                ping,
            } if *message == self.id => {
                if let Some(ping) = *ping {
                    if ping && !self.pings_user() {
                        self.ping_started = Some((record.seq, record.at));
                    }
                    if !ping {
                        self.ping_started = None;
                    }
                    self.set_ping(ping);
                }
                (content, Some(*author), record.seq, record.at)
            }
            ChatEvent::TakenBack { message, .. } if *message == self.id => {
                self.set_ping(false);
                let (seq, at) = self.version();
                self.revision = Revision::TakenBack { seq, at };
                return;
            }
            ChatEvent::Recovered { message, content } if *message == self.id => {
                self.set_ping(false);
                (content, None, record.seq, record.at)
            }
            ChatEvent::Deleted { message } if *message == self.id => {
                self.content.clear();
                self.mentions.clear();
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

    /// The position and time of the words now shown, not of the mail's anchor.
    pub fn version(&self) -> (ChatSeq, UnixMs) {
        match self.revision {
            Revision::Edited { seq, at } | Revision::TakenBack { seq, at } => (seq, at),
            _ => (self.id.0, self.at),
        }
    }

    /// When the current wait began. An edit before a verdict keeps that
    /// clock; a version after a verdict can begin another wait.
    pub fn ping_since(&self, seen: Option<ChatSeq>) -> (ChatSeq, UnixMs) {
        let began = self.ping_started.unwrap_or((self.id.0, self.at));
        if seen.is_some_and(|seen| began.0 <= seen) {
            self.version()
        } else {
            began
        }
    }

    /// Whether the mail is to the user, pinging or not.
    pub fn to_user(&self) -> bool {
        self.mentions
            .iter()
            .any(|mention| matches!(mention, Mention::User { .. }))
    }

    /// Whether the mail waits for the user as a card.
    pub fn pings_user(&self) -> bool {
        self.mentions.contains(&Mention::User { ping: true })
    }

    /// Mail to the user pings or stops pinging; other mail stays as it is.
    fn set_ping(&mut self, ping: bool) {
        for mention in &mut self.mentions {
            if let Mention::User { ping: was } = mention {
                *was = ping;
            }
        }
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
    fn a_post_written_before_reply_addresses_still_decodes() {
        #[derive(Encode)]
        enum OldChatEvent {
            Posted {
                parent: Option<MessageId>,
                from: Address,
                mentions: Vec<Mention>,
                content: Vec<ContentPart>,
            },
        }
        let parent = Some(MessageId(ChatSeq(3)));
        let content = vec![ContentPart::Text {
            text: "still here".into(),
        }];
        let bytes = senax_encoder::encode(&OldChatEvent::Posted {
            parent,
            from: Address::User,
            mentions: Vec::new(),
            content: content.clone(),
        })
        .unwrap();
        assert_eq!(
            senax_encoder::decode::<ChatEvent>(&mut bytes.as_ref()).unwrap(),
            ChatEvent::Posted {
                parent,
                reply_to: None,
                from: Address::User,
                mentions: Vec::new(),
                content
            }
        );
    }

    #[test]
    fn a_recap_runs_from_its_heading_to_the_next_heading_or_the_end() {
        let text =
            "Use clock B?\n\n## Recap\nThe clock is shared.\n### detail\nmore\n## next\nafter";
        let range = recap_section(text).unwrap();
        assert_eq!(
            &text[range],
            "## Recap\nThe clock is shared.\n### detail\nmore\n"
        );
        let last = "Ask\n## recap\nAll of it";
        assert_eq!(&last[recap_section(last).unwrap()], "## recap\nAll of it");
        assert_eq!(recap_section("## recaps\nno"), None);
        assert_eq!(recap_section("#recap\nno"), None);
        assert_eq!(recap_section("##recap\nno"), None);
        assert_eq!(recap_section("plain words about a recap"), None);
    }

    #[test]
    fn an_edit_turns_the_ping_on_or_leaves_it() {
        let agent = AgentId::from_counter(4, &crate::AgentIdDomain(7)).unwrap();
        let posted = ChatRecord {
            seq: ChatSeq(5),
            at: UnixMs(1),
            event: ChatEvent::Posted {
                parent: Some(MessageId(ChatSeq(2))),
                reply_to: None,
                from: Address::Agent(agent),
                mentions: vec![Mention::User { ping: false }],
                content: Vec::new(),
            },
        };
        let mut mail = Message::from_record(&posted).unwrap();
        assert!(mail.to_user() && !mail.pings_user());
        let revised = |seq, ping| ChatRecord {
            seq: ChatSeq(seq),
            at: UnixMs(seq),
            event: ChatEvent::Revised {
                message: MessageId(ChatSeq(5)),
                author: agent,
                content: Vec::new(),
                ping,
            },
        };
        mail.apply_revision(&revised(6, None));
        assert!(!mail.pings_user());
        mail.apply_revision(&revised(7, Some(true)));
        assert!(mail.pings_user());
        mail.apply_revision(&revised(8, None));
        assert!(mail.pings_user());
    }

    #[test]
    fn a_message_is_shown_by_its_first_line() {
        let record = ChatRecord {
            seq: ChatSeq(3),
            at: UnixMs(1),
            event: ChatEvent::Posted {
                parent: Some(MessageId(ChatSeq(1))),
                reply_to: None,
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
