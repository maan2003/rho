//! The agent2 chat protocol. The host log is primary; every session begins
//! with a snapshot, then sends append-only chat changes.
use camino::Utf8PathBuf;
pub use rho_agent_types::AgentId;
use rho_agent_types::{AgentRole, Place, UnixMs, WorksetMode, WorkspaceInfo};
use senax_encoder::{Decode, Encode, Pack, Unpack};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode, Pack, Unpack)]
pub struct MessageId(pub u64);
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Party {
    Human,
    Agent(AgentId),
}
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ChatKind {
    Message {
        id: MessageId,
        from: Party,
        to: Party,
        text: String,
    },
    Status(String),
    /// Branch before a physical log position. All events remain in the stream.
    Rewound {
        to: u64,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ChatEvent {
    pub seq: u64,
    pub at: UnixMs,
    pub kind: ChatKind,
}

/// The current chat branch. Physical log sequence numbers remain stable so
/// nested rewinds can name a position on an older branch without deleting
/// abandoned events from the stream or reconnect snapshot.
pub fn visible_chat(events: &[ChatEvent]) -> Vec<&ChatEvent> {
    let mut visible = Vec::new();
    for event in events {
        if let ChatKind::Rewound { to } = event.kind {
            visible.retain(|kept: &&ChatEvent| kept.seq < to);
        }
        visible.push(event);
    }
    visible
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Effort {
    Low,
    #[default]
    Medium,
    High,
    XHigh,
}
impl std::str::FromStr for Effort {
    type Err = String;
    fn from_str(text: &str) -> Result<Self, String> {
        match text {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::XHigh),
            _ => Err("use low, medium, high, or xhigh".into()),
        }
    }
}
impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct AgentInfo {
    pub id: AgentId,
    pub place: Place,
    pub role: AgentRole,
    /// Spawning agent, if this agent was delegated.
    pub parent: Option<AgentId>,
    /// A delegated agent explicitly handed to the user is user-managed.
    pub user_owned: bool,
    pub model: String,
    pub effort: Effort,
    pub archived: bool,
    pub status: Option<String>,
    pub chat: Vec<ChatEvent>,
}
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct CreateAgent {
    pub start: StartMode,
    pub mode: WorksetMode,
    pub role: AgentRole,
    pub model: String,
    pub effort: Effort,
    pub initial_message: Option<String>,
}
/// Where the workset starts: a fresh clone on a revision or an existing place.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum StartMode {
    NewOn { repo: Utf8PathBuf, revset: String },
    Join(JoinTarget),
}

/// The existing worktree or user's checkout to join.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum JoinTarget {
    Workspace(WorkspaceInfo),
    User { repo: Utf8PathBuf },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct SendMessage {
    pub agent_id: AgentId,
    pub text: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ArchiveAgent {
    pub agent_id: AgentId,
}
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct RewindAgent {
    pub agent_id: AgentId,
    pub turns: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ListAgents;

#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum Open {
    Session,
    Request(Request),
}
rho_rpc::calls! {
    pub enum Request {
        CreateAgent(CreateAgent) -> AgentId;
        SendMessage(SendMessage) -> ();
        ArchiveAgent(ArchiveAgent) -> ();
        RewindAgent(RewindAgent) -> ();
        ListAgents(ListAgents) -> Vec<AgentInfo>;
    }
}
impl rho_rpc::protocol::ProtocolOpen for Open {
    const PROTOCOL: rho_rpc::protocol::Protocol = rho_rpc::protocol::Protocol::Agents2;
    fn debug_reply(&self, frame: &[u8]) -> Option<String> {
        match self {
            Self::Request(request) => Some(request.debug_answer(frame)),
            Self::Session => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum ServerFrame {
    Snapshot { agents: Vec<AgentInfo> },
    Created { agent: AgentInfo },
    Chat { agent_id: AgentId, event: ChatEvent },
    Archived { agent_id: AgentId, archived: bool },
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nested_rewinds_project_current_chat_without_erasing_stream() {
        let event = |seq, kind| ChatEvent {
            seq,
            at: UnixMs(seq),
            kind,
        };
        let events = vec![
            event(1, ChatKind::Status("kept".into())),
            event(3, ChatKind::Status("abandoned".into())),
            event(4, ChatKind::Rewound { to: 3 }),
            event(6, ChatKind::Status("temporary".into())),
            event(8, ChatKind::Rewound { to: 3 }),
            event(10, ChatKind::Status("now".into())),
        ];
        assert_eq!(events.len(), 6);
        assert_eq!(
            visible_chat(&events)
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            vec![1, 8, 10]
        );
        let bytes = senax_encoder::pack(&events[4]).unwrap();
        let decoded: ChatEvent = senax_encoder::unpack(&mut bytes.as_ref()).unwrap();
        assert_eq!(decoded, events[4]);
    }

    #[test]
    fn request_and_chat_round_trip_without_notebook_fields() {
        let id = AgentId::from_counter(17, &rho_agent_types::AgentIdDomain(42)).unwrap();
        let request = Open::Request(Request::CreateAgent(CreateAgent {
            start: StartMode::NewOn {
                repo: "/src/workset".into(),
                revset: "origin/main".into(),
            },
            mode: WorksetMode::Exposed,
            role: AgentRole::default(),
            model: "gpt-6-sol".into(),
            effort: Effort::High,
            initial_message: Some("hello".into()),
        }));
        let bytes = senax_encoder::pack(&request).unwrap();
        let decoded: Open = senax_encoder::unpack(&mut bytes.as_ref()).unwrap();
        assert_eq!(decoded, request);
        let place = Place {
            workset: "workset-6".into(),
            cwd: "/src/child".into(),
            mode: WorksetMode::View,
            origin: Some("/src/base".into()),
        };
        let joined = Open::Request(Request::CreateAgent(CreateAgent {
            start: StartMode::Join(JoinTarget::Workspace(place.clone().into())),
            mode: WorksetMode::Exposed,
            role: AgentRole::default(),
            model: "gpt-6-sol".into(),
            effort: Effort::Low,
            initial_message: None,
        }));
        let bytes = senax_encoder::pack(&joined).unwrap();
        assert_eq!(
            senax_encoder::unpack::<Open>(&mut bytes.as_ref()).unwrap(),
            joined
        );
        let snapshot = ServerFrame::Snapshot {
            agents: vec![AgentInfo {
                id,
                place,
                role: AgentRole::default(),
                parent: None,
                user_owned: false,
                model: "gpt-6-sol".into(),
                effort: Effort::Low,
                archived: false,
                status: Some("working".into()),
                chat: Vec::new(),
            }],
        };
        let bytes = senax_encoder::pack(&snapshot).unwrap();
        assert_eq!(
            senax_encoder::unpack::<ServerFrame>(&mut bytes.as_ref()).unwrap(),
            snapshot
        );
        let frame = ServerFrame::Chat {
            agent_id: id.clone(),
            event: ChatEvent {
                seq: 17,
                at: UnixMs(123),
                kind: ChatKind::Message {
                    id: MessageId(91),
                    from: Party::Human,
                    to: Party::Agent(id),
                    text: "hi".into(),
                },
            },
        };
        let bytes = senax_encoder::pack(&frame).unwrap();
        assert_eq!(
            senax_encoder::unpack::<ServerFrame>(&mut bytes.as_ref()).unwrap(),
            frame
        );
    }
}
