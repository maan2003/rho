//! The chat protocol ([`rho_rpc::protocol::Protocol::Chat`]): a host's log
//! as it grows, and what the user says in it.
//!
//! The session ([`Open::Session`]) carries the log from where the client
//! holds it. Everything the user does is one call ([`Open::Request`]), on a
//! stream of its own. The events themselves are
//! [`rho_agent_types::ChatEvent`]: the vocabulary is the host's, this is
//! only how it crosses the wire.

use rho_agent_types::{
    AgentId, ChatRecord, ChatSeq, ContentPart, EndpointId, MessageId, ProjectChange, ProjectConfig,
    ProjectId, ProjectName, RuntimeLabel, ThreadWork,
};
use senax_encoder::{Decode, Encode, Pack, Unpack};

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum Open {
    Session,
    Request(Request),
}

impl rho_rpc::protocol::ProtocolOpen for Open {
    const PROTOCOL: rho_rpc::protocol::Protocol = rho_rpc::protocol::Protocol::Chat;

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
        /// Post under a project and start the idea using the host's settings.
        PostIdea(PostIdea) -> PostIdeaReply;
        RetryIdea(RetryIdea) -> MessageId;
        CreateProject(CreateProject) -> ProjectId;
        ChangeProject(ChangeProject) -> ();
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
    /// The card seen when the user opened the thread, not a recipient.
    pub settles: Option<rho_agent_types::MessageVersion>,
    /// Agents the message reaches whether or not they are members. It
    /// does not make them members.
    pub mentions: Vec<AgentId>,
    pub content: Vec<ContentPart>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct PostIdea {
    pub client_id: u64,
    pub project: ProjectId,
    /// An explicit answer; without one the host keeps a standing agent's
    /// runtime or uses the project's default for a new agent.
    pub runtime: Option<RuntimeLabel>,
    pub content: Vec<ContentPart>,
    /// The name this device uses for the host, for a failed start's mail.
    pub host_label: String,
}

/// A runtime refusal happens before posting. The client can drop that
/// attempt without discarding the draft; lost transport still means retry.
#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum PostIdeaReply {
    Posted(MessageId),
    RuntimeRefused(String),
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct RetryIdea {
    pub client_id: u64,
    pub failed_start: MessageId,
    pub content: Vec<ContentPart>,
    pub host_label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct CreateProject {
    pub name: ProjectName,
    pub config: ProjectConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ChangeProject {
    pub project: ProjectId,
    pub change: ProjectChange,
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
    Messages { records: Vec<ChatRecord> },
    /// What the agents of each thread do now, as a full replacement; an
    /// omitted thread has no agent at work. This is not part of the mail
    /// log and carries no cursor.
    Work {
        threads: Vec<(MessageId, ThreadWork)>,
    },
    /// The host's project settings, replacing the previous snapshot. A
    /// legacy project without a known place has no entry yet.
    Projects {
        configured: Vec<(ProjectId, ProjectConfig)>,
    },
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{Address, AgentIdDomain, ChatEvent, Mention, Stop, UnixMs};

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
        let project = ProjectId(MessageId(ChatSeq(2)));
        let config = ProjectConfig {
            place: rho_agent_types::ProjectPlace::parse("git@github.com:owner/reef").unwrap(),
            mode: rho_agent_types::ProjectMode::PerIdea,
            runtime: RuntimeLabel::parse("haiku-xhigh").unwrap(),
        };
        round_trips(ServerFrame::Projects {
            configured: vec![(project, config.clone())],
        });
        round_trips(Open::Request(
            CreateProject {
                name: ProjectName::parse("reef").unwrap(),
                config,
            }
            .into(),
        ));
        round_trips(Open::Request(
            ChangeProject {
                project,
                change: ProjectChange::Runtime(RuntimeLabel::parse("sol-medium").unwrap()),
            }
            .into(),
        ));
        round_trips(Open::Request(
            PostIdea {
                client_id: 17,
                project,
                runtime: Some(RuntimeLabel::parse("sol-high").unwrap()),
                content: vec![ContentPart::Text {
                    text: "New idea".into(),
                }],
                host_label: "lab".into(),
            }
            .into(),
        ));
        round_trips(ServerFrame::Work {
            threads: vec![
                (MessageId(ChatSeq(14)), ThreadWork::Working),
                (MessageId(ChatSeq(15)), ThreadWork::StoppedByYou),
            ],
        });
        round_trips(ServerFrame::Work {
            threads: Vec::new(),
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
        round_trips(ServerFrame::Messages {
            records: vec![
                ChatRecord {
                    seq: ChatSeq(4),
                    at: UnixMs(5),
                    event: ChatEvent::Posted {
                        kind: rho_agent_types::MessageKind::Kept,
                        parent: Some(MessageId(ChatSeq(2))),
                        reply_to: None,
                        thread_reply: None,
                        from: Address::Agent(agent),
                        mentions: vec![Mention::User { ping: true }],
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
        round_trips(ServerFrame::Messages {
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
                        ping: Some(true),
                    },
                },
                ChatRecord {
                    seq: ChatSeq(11),
                    at: UnixMs(29),
                    event: ChatEvent::Posted {
                        kind: rho_agent_types::MessageKind::Kept,
                        parent: Some(MessageId(ChatSeq(7))),
                        reply_to: None,
                        thread_reply: None,
                        from: Address::System {
                            agent,
                            stop: Stop::Stuck,
                            since: UnixMs(23),
                        },
                        mentions: vec![Mention::User { ping: true }],
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
                settles: Some(rho_agent_types::MessageVersion {
                    message: MessageId(ChatSeq(4)),
                    version: ChatSeq(8),
                }),
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
