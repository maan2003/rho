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
    /// Active model turn, as reported by the workset worker; not durable.
    pub running_since: Option<UnixMs>,
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
pub struct CompactAgent {
    pub agent_id: AgentId,
}
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ListAgents;

/// Every provider's quota as it stands.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct QuotaUsage;

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct QuotaHistory;

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct GlobalUsage {
    pub since_ms: u64,
}

/// Raw per-agent usage needed to form cost distributions beginning at
/// `since_ms`, with the fixed trailing-window lookback.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct AgentCostDistribution {
    pub since_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct QuotaSummary {
    pub model: String,
    /// Host-local ChatGPT OAuth namespace; absent for Claude.
    pub auth_namespace: Option<String>,
    pub remaining_percent: u8,
    pub burn_10m: u16,
    pub burn_2h: u16,
    pub burn_1d: u16,
    pub burn_3d: u16,
    pub reset_at_unix: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct QuotaSeries {
    pub model: String,
    /// Host-local ChatGPT OAuth namespace; absent for Claude.
    pub auth_namespace: Option<String>,
    pub points: Vec<QuotaPoint>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct QuotaPoint {
    pub observed_at_ms: u64,
    pub remaining_percent: u8,
    pub reset_at_unix: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct AgentUsageBucket {
    pub bucket_start_ms: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub output_tokens: u64,
    pub requests: u64,
    pub approximate: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct AgentUsageSeries {
    pub model: String,
    pub buckets: Vec<AgentUsageBucket>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct AgentCostSeries {
    /// Host-local identity. Clients combining hosts must keep the host in the
    /// distribution key rather than merging equal counters.
    pub agent_id: AgentId,
    pub model: String,
    pub buckets: Vec<AgentUsageBucket>,
}

/// Window represented by each point in the agent-cost distribution graph.
pub const AGENT_COST_WINDOW_DAYS: u64 = 7;

/// Host-wide authentication settings presented by a GUI host.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct AuthState {
    pub namespaces: Vec<String>,
    pub disabled_namespaces: Vec<String>,
    pub active_namespace: Option<String>,
}

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
        CompactAgent(CompactAgent) -> ();
        ListAgents(ListAgents) -> Vec<AgentInfo>;
        QuotaUsage(QuotaUsage) -> Vec<QuotaSummary>;
        QuotaHistory(QuotaHistory) -> Vec<QuotaSeries>;
        GlobalUsage(GlobalUsage) -> Vec<AgentUsageSeries>;
        AgentCostDistribution(AgentCostDistribution) -> Vec<AgentCostSeries>;
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
    Snapshot {
        agents: Vec<AgentInfo>,
    },
    Created {
        agent: AgentInfo,
    },
    Chat {
        agent_id: AgentId,
        event: ChatEvent,
    },
    Archived {
        agent_id: AgentId,
        archived: bool,
    },
    RunningSince {
        agent_id: AgentId,
        since: Option<UnixMs>,
    },
    Auth {
        auth: AuthState,
    },
    QuotaUsage {
        summaries: Vec<QuotaSummary>,
    },
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
    fn usage_requests_and_replies_round_trip() {
        fn round_trip<
            T: senax_encoder::Packer + senax_encoder::Unpacker + PartialEq + std::fmt::Debug,
        >(
            value: T,
        ) {
            let bytes = senax_encoder::pack(&value).unwrap();
            let decoded: T = senax_encoder::unpack(&mut bytes.as_ref()).unwrap();
            assert_eq!(decoded, value);
        }

        for request in [
            Request::QuotaUsage(QuotaUsage),
            Request::QuotaHistory(QuotaHistory),
            Request::GlobalUsage(GlobalUsage {
                since_ms: 86_400_123,
            }),
            Request::AgentCostDistribution(AgentCostDistribution { since_ms: 42 }),
        ] {
            round_trip(Open::Request(request));
        }
        round_trip(rho_rpc::protocol::Answer::Done(vec![QuotaSummary {
            model: "gpt".into(),
            auth_namespace: Some("work".into()),
            remaining_percent: 38,
            burn_10m: 4,
            burn_2h: 18,
            burn_1d: 63,
            burn_3d: 91,
            reset_at_unix: Some(1_800_000_000),
        }]));
        round_trip(rho_rpc::protocol::Answer::Done(vec![QuotaSeries {
            model: "opus".into(),
            auth_namespace: None,
            points: vec![QuotaPoint {
                observed_at_ms: 86_400_123,
                remaining_percent: 24,
                reset_at_unix: Some(1_800_086_400),
            }],
        }]));
        let bucket = AgentUsageBucket {
            bucket_start_ms: 3_600_000,
            input_tokens: 11,
            cache_read_tokens: 23,
            cache_write_tokens: 37,
            cache_write_1h_tokens: 41,
            output_tokens: 53,
            requests: 7,
            approximate: true,
        };
        round_trip(rho_rpc::protocol::Answer::Done(vec![AgentUsageSeries {
            model: "astra".into(),
            buckets: vec![bucket.clone()],
        }]));
        round_trip(rho_rpc::protocol::Answer::Done(vec![AgentCostSeries {
            agent_id: AgentId::from_counter(7, &rho_agent_types::AgentIdDomain(2)).unwrap(),
            model: "gpt".into(),
            buckets: vec![bucket],
        }]));
        round_trip(AuthState {
            namespaces: vec!["work".into(), "personal".into()],
            disabled_namespaces: vec!["personal".into()],
            active_namespace: Some("work".into()),
        });
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
                running_since: None,
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
        let frames = [
            ServerFrame::Auth {
                auth: AuthState {
                    namespaces: vec!["primary".into(), "secondary".into()],
                    disabled_namespaces: vec!["secondary".into()],
                    active_namespace: Some("primary".into()),
                },
            },
            ServerFrame::QuotaUsage {
                summaries: vec![QuotaSummary {
                    model: "gpt".into(),
                    auth_namespace: Some("primary".into()),
                    remaining_percent: 13,
                    burn_10m: 2,
                    burn_2h: 4,
                    burn_1d: 9,
                    burn_3d: 21,
                    reset_at_unix: Some(456),
                }],
            },
        ];
        for frame in frames {
            let bytes = senax_encoder::pack(&frame).unwrap();
            assert_eq!(
                senax_encoder::unpack::<ServerFrame>(&mut bytes.as_ref()).unwrap(),
                frame
            );
        }
    }
}
