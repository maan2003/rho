//! The mail protocol ([`rho_rpc::protocol::Protocol::Mail`]): a host's log
//! as it grows, and what the user says in it.
//!
//! The session ([`Open::Session`]) carries the log from where the client
//! holds it. Everything the user does is one call ([`Open::Request`]), on a
//! stream of its own. The events themselves are
//! [`rho_agent_types::ChatEvent`]: the vocabulary is the host's, this is
//! only how it crosses the wire.

use rho_agent_types::{
    AgentId, ChatRecord, ChatSeq, ContentPart, EndpointId, MessageId, ThreadStatus,
};
use senax_encoder::{Decode, Encode, Pack, Unpack};

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum Open {
    Session,
    Request(Request),
}

impl rho_rpc::protocol::ProtocolOpen for Open {
    const PROTOCOL: rho_rpc::protocol::Protocol = rho_rpc::protocol::Protocol::Mail;

    fn debug_reply(&self, frame: &[u8]) -> Option<String> {
        match self {
            Self::Request(request) => Some(request.debug_answer(frame)),
            Self::Session => None,
        }
    }
}

rho_rpc::calls! {
    pub enum Request {
        /// A message from the user. Logged once per `client_id`.
        Post(Post) -> MessageId;
        /// Puts a message and its thread under another message, or among
        /// the roots.
        Reparent(Reparent) -> ();
        /// The user takes an agent out of a thread.
        Leave(Leave) -> ();
        /// Names a thread; an empty name takes the name back.
        Name(Name) -> ();
        /// Looks up the project part of `project@host`. The GUI owns host aliases.
        ResolveProject(ResolveProject) -> Option<Project>;
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Post {
    /// The client's own random name for the message, so a resend after a
    /// lost answer is logged once.
    pub client_id: u64,
    /// The thread it goes in; none starts a root.
    pub parent: Option<MessageId>,
    /// The mail this answers, if any; it need not be the parent.
    pub reply_to: Option<MessageId>,
    /// Agents the message reaches whether or not they are members. It
    /// does not make them members.
    pub mentions: Vec<AgentId>,
    pub content: Vec<ContentPart>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Reparent {
    pub message: MessageId,
    pub parent: Option<MessageId>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Leave {
    pub thread: MessageId,
    pub agent: AgentId,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Name {
    pub thread: MessageId,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct ResolveProject {
    pub name: String,
}

/// A project's root anchors its threads. Its address combines its name with the
/// client's alias for the host, not the host's machine or endpoint name.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct Project {
    #[senax(rename = "channel")]
    pub root: MessageId,
    pub name: String,
}

impl Project {
    pub fn address(&self, host_alias: &str) -> String {
        format!("{}@{host_alias}", self.name)
    }
}

/// What a client says on its session.
#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum ClientFrame {
    /// After [`ServerFrame::Hello`]: the newest event this client holds
    /// (zero for none). The host sends every later one, then each new one.
    Follow { since: ChatSeq },
    /// The one thread currently open on this session; `None` on leaving it.
    /// Closing the session also removes its open thread.
    View { thread: Option<MessageId> },
}

/// What a host says on its session.
#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum ServerFrame {
    /// The first frame: the newest event on the host, and the host's iroh
    /// identity, which names its log and its threads wherever a copy is
    /// held.
    Hello { head: ChatSeq, host: EndpointId },
    /// Events in log order: the answer to `Follow`, paged, then each new
    /// one.
    Mail { records: Vec<ChatRecord> },
    /// Full replacement snapshot; an omitted thread has no status. This is
    /// not part of the mail log and carries no cursor.
    Statuses { statuses: Vec<ThreadStatus> },
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{Address, AgentIdDomain, ChatEvent, Mention, MentionKind, Stop, UnixMs};

    use super::*;

    fn round_trips<
        T: senax_encoder::Packer + senax_encoder::Unpacker + PartialEq + std::fmt::Debug,
    >(
        frame: T,
    ) {
        let bytes = senax_encoder::pack(&frame).unwrap();
        let back: T = senax_encoder::unpack(&mut &bytes[..]).unwrap();
        assert_eq!(back, frame);
    }

    #[test]
    fn frames_and_requests_round_trip() {
        let agent = AgentId::from_counter(3, &AgentIdDomain(7)).unwrap();
        round_trips(ClientFrame::Follow { since: ChatSeq(9) });
        round_trips(ClientFrame::View {
            thread: Some(MessageId(ChatSeq(4))),
        });
        round_trips(ClientFrame::View { thread: None });
        round_trips(ServerFrame::Statuses {
            statuses: vec![ThreadStatus {
                thread: MessageId(ChatSeq(14)),
                at: UnixMs(28),
                state: rho_agent_types::StatusText::Written("Waiting for the review".into()),
            }],
        });
        round_trips(ServerFrame::Statuses {
            statuses: Vec::new(),
        });
        round_trips(Open::Request(
            ResolveProject {
                name: "tide".into(),
            }
            .into(),
        ));
        round_trips(rho_rpc::protocol::Answer::Done(Some(Project {
            root: MessageId(ChatSeq(2)),
            name: "tide".into(),
        })));
        round_trips(ServerFrame::Hello {
            head: ChatSeq(2),
            host: EndpointId([7; 32]),
        });
        round_trips(ServerFrame::Mail {
            records: vec![
                ChatRecord {
                    seq: ChatSeq(4),
                    at: UnixMs(5),
                    event: ChatEvent::Posted {
                        parent: Some(MessageId(ChatSeq(2))),
                        reply_to: None,
                        from: Address::Agent(agent),
                        mentions: vec![Mention {
                            to: Address::User,
                            kind: MentionKind::Ask,
                        }],
                        content: vec![ContentPart::Text { text: "hi".into() }],
                    },
                },
                ChatRecord {
                    seq: ChatSeq(5),
                    at: UnixMs(6),
                    event: ChatEvent::Reparented {
                        message: MessageId(ChatSeq(4)),
                        parent: None,
                    },
                },
            ],
        });
        round_trips(ServerFrame::Mail {
            records: vec![
                ChatRecord {
                    seq: ChatSeq(8),
                    at: UnixMs(19),
                    event: ChatEvent::Edited {
                        message: MessageId(ChatSeq(4)),
                        content: vec![ContentPart::Text {
                            text: "fixed".into(),
                        }],
                    },
                },
                ChatRecord {
                    seq: ChatSeq(9),
                    at: UnixMs(22),
                    event: ChatEvent::Deleted {
                        message: MessageId(ChatSeq(5)),
                    },
                },
                ChatRecord {
                    seq: ChatSeq(10),
                    at: UnixMs(28),
                    event: ChatEvent::Revised {
                        message: MessageId(ChatSeq(4)),
                        author: agent,
                        content: vec![ContentPart::Text {
                            text: "Clock B?".into(),
                        }],
                    },
                },
                ChatRecord {
                    seq: ChatSeq(11),
                    at: UnixMs(29),
                    event: ChatEvent::Posted {
                        parent: Some(MessageId(ChatSeq(7))),
                        reply_to: None,
                        from: Address::System {
                            agent,
                            stop: Stop::Stuck,
                            since: UnixMs(23),
                        },
                        mentions: vec![Mention {
                            to: Address::User,
                            kind: MentionKind::Ask,
                        }],
                        content: vec![ContentPart::Text {
                            text: "stuck · retries".into(),
                        }],
                    },
                },
            ],
        });
        round_trips(Open::Request(
            Post {
                client_id: 7,
                parent: Some(MessageId(ChatSeq(2))),
                reply_to: Some(MessageId(ChatSeq(4))),
                mentions: vec![agent],
                content: vec![ContentPart::Text {
                    text: "root".into(),
                }],
            }
            .into(),
        ));
        round_trips(Open::Request(
            Name {
                thread: MessageId(ChatSeq(4)),
                name: "Fix it".into(),
            }
            .into(),
        ));
        round_trips(rho_rpc::protocol::Answer::Done(MessageId(ChatSeq(3))));
    }
}
