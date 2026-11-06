//! The agents protocol
//! ([ARCH-agent-host](../../../../agent-host/specs/ARCH-agent-host.md)):
//! worksets, the agent directory and chat.
//!
//! The session ([`Open::Session`]) carries the chat log, the directory, the
//! rows of the worksets a client follows, and the live tails of the agents
//! it looks at. Everything else is one call ([`Open::Request`]).

use rho_agent_types::{
    AgentId, ChatRecord, ChatSeq, ContentPart, ExecMilestone, MessageId, Props, Role, StreamId,
    TopicId, UnixMs, WorksetId, WorksetKind, Wpos,
};
use senax_encoder::{Decode, Encode, Pack, Unpack};

use super::transcript::{DetailBody, Item, Live, RuntimeKind, RuntimeState, Speaker, Usage};

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum Open {
    Session,
    Request(Request),
}

impl rho_rpc::protocol::ProtocolOpen for Open {
    const PROTOCOL: rho_rpc::protocol::Protocol = rho_rpc::protocol::Protocol::Worksets;

    fn debug_reply(&self, frame: &[u8]) -> Option<String> {
        match self {
            Self::Request(request) => Some(request.debug_answer(frame)),
            Self::Session => None,
        }
    }
}

rho_rpc::calls! {
    pub enum Request {
        /// A new workset, whose namespace runs `code` once after `init.py`.
        /// Answered once the creation code ends.
        CreateWorkset(CreateWorkset) -> CreatedWorkset;
        /// Code run in a workset's namespace; answered when it ends.
        Eval(Eval) -> EvalResult;
        /// A message from the user. Logged once per `client_id`.
        Post(Post) -> MessageId;
        /// A topic the user starts with its first message. Logged once per
        /// `client_id`.
        StartTopic(StartTopic) -> TopicId;
        CreateStream(CreateStream) -> StreamId;
        /// The user takes an agent out of a topic.
        Leave(Leave) -> ();
        /// Renames a topic, moves it to another stream, or resolves it.
        ChangeTopic(ChangeTopic) -> ();
        /// Moves messages to a topic, which may be new; answered with the
        /// topic they are in. Logged once per `client_id`.
        MoveMessages(MoveMessages) -> TopicId;
        /// Stops the agent's current request and cells. Typed, because it
        /// must work while the workset's Python is stuck.
        Cancel(Cancel) -> ();
        /// End everything in the agent's shared workset, replace its worker
        /// with the host's current build, and reload the agent.
        RestartWorkset(RestartWorkset) -> ();
        DeleteWorksets(DeleteWorksets) -> ();
        /// Bulk: one agent's rows from `from`, a page at a time.
        AgentLog(AgentLog) -> AgentLogPage, priority None;
        // Bulk: an answer that waits behind interactive traffic.
        Visualization(super::Visualization) -> super::VisualizationContent, priority None;
        /// Answered with the recorded visualization's id.
        RecordVisualization(super::RecordVisualization) -> String;
        QuotaUsage(super::QuotaUsage) -> Vec<super::QuotaSummary>;
        QuotaHistory(super::QuotaHistory) -> Vec<super::QuotaSeries>;
        GlobalUsage(super::GlobalUsage) -> Vec<super::AgentUsageSeries>;
        AgentCostDistribution(super::AgentCostDistribution) -> Vec<super::AgentCostSeries>;
        ClaudeAccounts(super::ClaudeAccounts) -> super::ClaudeAccountList;
        /// Answered with the accounts as they stand after the switch.
        SetClaudeAccount(super::SetClaudeAccount) -> super::ClaudeAccountList;
        SetAuthAccountEnabled(super::SetAuthAccountEnabled) -> ();
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct CreateWorkset {
    pub kind: WorksetKind,
    pub code: String,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Eval {
    pub workset: WorksetId,
    pub code: String,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct EvalResult {
    /// What the code printed.
    pub output: String,
    /// The traceback, when it raised.
    pub error: Option<String>,
    /// The first agent the code created, if it created one.
    pub agent: Option<AgentId>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct CreatedWorkset {
    pub workset: WorksetId,
    /// How its creation code ran.
    pub result: EvalResult,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Post {
    /// The client's own random name for the message, so a resend after a
    /// lost answer is logged once.
    pub client_id: u64,
    pub topic: TopicId,
    /// Agents the message reaches whether or not they are members. It
    /// does not make them members.
    pub mentions: Vec<AgentId>,
    pub content: Vec<ContentPart>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct StartTopic {
    /// As in [`Post`].
    pub client_id: u64,
    pub stream: StreamId,
    pub name: String,
    /// As in [`Post`].
    pub mentions: Vec<AgentId>,
    pub content: Vec<ContentPart>,
}

/// The stream of that name, created if the host has none. A stream's name
/// is unique on its host, so a resend finds the one it made.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct CreateStream {
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Leave {
    pub topic: TopicId,
    pub agent: AgentId,
}

/// A topic as it is to be, whole. Not for a direct conversation.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct ChangeTopic {
    pub topic: TopicId,
    pub stream: StreamId,
    pub name: String,
    pub resolved: bool,
}

/// Messages of topics, in any order, to one topic. A direct conversation's
/// messages stay where they are.
#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct MoveMessages {
    /// As in [`Post`]: a resend after a lost answer starts no second topic.
    pub client_id: u64,
    pub messages: Vec<MessageId>,
    pub to: MoveTo,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum MoveTo {
    Topic(TopicId),
    /// A topic started for them, with no members: its first message is the
    /// oldest one moved.
    NewTopic {
        stream: StreamId,
        name: String,
    },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Cancel {
    pub agent: AgentId,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct RestartWorkset {
    pub agent: AgentId,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct DeleteWorksets {
    pub worksets: Vec<WorksetId>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct AgentLog {
    pub agent: AgentId,
    pub from: Wpos,
}

#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub struct AgentLogPage {
    /// The agent's rows, in order. Its workset's other rows are not here.
    pub rows: Vec<(Wpos, LogEvent)>,
    pub next: LogNext,
}

/// Where an [`AgentLog`] read goes on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Pack, Unpack)]
pub enum LogNext {
    /// The next page starts here.
    Page(Wpos),
    /// The agent's log is read to its end, and its workset's next row goes
    /// here: following the workset from here misses none of its rows.
    Follow(Wpos),
}

/// What a client says on its session.
#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum ClientFrame {
    /// After [`ServerFrame::Hello`]: the newest chat event this client
    /// holds (zero for none). The host sends every later one, then each new
    /// one.
    FollowChat { since: ChatSeq },
    /// The worksets whose rows this client wants pushed as they land, each
    /// from the position it holds. Replaces the set wholesale; a workset
    /// this stream already follows goes on from the rows it sent, so none
    /// is sent twice.
    FollowWorksets { from: Vec<(WorksetId, Wpos)> },
    /// The agents whose live frames this client wants. Replaces the set.
    Focus { agent_ids: Vec<AgentId> },
    /// The bodies of rows: tool output, a response whole. One
    /// [`ServerFrame::Detail`] answers each position.
    Detail {
        workset: WorksetId,
        pos: Wpos,
        more: Vec<Wpos>,
    },
}

/// What a host says on its session.
#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum ServerFrame {
    /// The first frame.
    Hello {
        machine_seed: u64,
        /// The newest chat event on the host.
        chat_head: ChatSeq,
        /// The last agent-id counter handed out, for short-prefix rendering.
        agent_counter: u64,
    },
    /// Which provider accounts agents may run on: after `Hello`, and on change.
    Auth {
        auth: super::AuthState,
    },
    /// Every workset and agent, after `Hello`. Later changes are
    /// [`ServerFrame::Workset`] and [`ServerFrame::Agent`].
    Directory {
        worksets: Vec<WorksetEntry>,
        agents: Vec<AgentEntry>,
    },
    /// A workset was created or changed.
    Workset(WorksetEntry),
    /// A workset and its agents are gone.
    WorksetDeleted {
        workset: WorksetId,
    },
    /// An agent was created or its spec changed: the whole entry.
    Agent(AgentEntry),
    /// Chat events in log order: the answer to `FollowChat`, paged, then
    /// each new one.
    Chat {
        records: Vec<ChatRecord>,
    },
    /// Rows of a followed workset, in order, never repeated or skipped.
    Rows {
        workset: WorksetId,
        rows: Vec<(Wpos, LogEvent)>,
    },
    Live {
        agent_id: AgentId,
        live: Live,
    },
    /// What a loaded agent's runtime is doing, for every agent and not only
    /// the focused ones: after `Hello`, and on change. The list and the
    /// dealer read it (a stopped agent is a card).
    Status {
        agent_id: AgentId,
        state: RuntimeState,
    },
    Detail {
        workset: WorksetId,
        pos: Wpos,
        body: DetailBody,
    },
    QuotaUsage {
        summaries: Vec<super::QuotaSummary>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct WorksetEntry {
    pub workset: WorksetId,
    pub kind: WorksetKind,
    pub created_at: UnixMs,
}

/// An agent as the directory shows it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct AgentEntry {
    pub agent: AgentId,
    pub workset: WorksetId,
    pub spec: SpecView,
    pub created_at: UnixMs,
}

/// An agent's spec, as a reader needs it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct SpecView {
    pub runtime: RuntimeKind,
    /// The model by the name policy Python gives it: `sol`, `opus`.
    /// The same name selects it in `set_runtime`.
    pub model: String,
    pub effort: String,
    pub props: Props,
}

/// One thing a request carried, with its body left as text.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct InputView {
    pub message: Option<MessageId>,
    pub text: String,
    pub role: Role,
}

/// One workset row as it crosses the wire: `strip` of exactly one row.
/// Bodies a person does not read at a glance are left behind; ask for them
/// with [`ClientFrame::Detail`].
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum LogEvent {
    WorksetCreated {
        kind: WorksetKind,
        code: String,
        at: UnixMs,
    },
    Evaluated {
        code: String,
        at: UnixMs,
    },
    AgentCreated {
        agent: AgentId,
        spec: SpecView,
        at: UnixMs,
    },
    AgentChanged {
        agent: AgentId,
        spec: SpecView,
        at: UnixMs,
    },
    CompactionRequested {
        agent: AgentId,
        at: UnixMs,
    },
    /// A request went out with these inputs. `answers`: it answers the open
    /// call, if one is. Only a compaction with nothing to say leaves it open.
    Request {
        agent: AgentId,
        inputs: Vec<InputView>,
        answers: bool,
        compaction: bool,
        at: UnixMs,
    },
    /// Claude Code says these calls came back, with no request to answer them.
    CallsEnded {
        agent: AgentId,
        calls: Vec<String>,
        at: UnixMs,
    },
    Replied {
        agent: AgentId,
        items: Vec<Item>,
        compacted: bool,
        usage: Option<Usage>,
        context_used: Option<u64>,
        at: UnixMs,
    },
    Rewound {
        agent: AgentId,
        to: Wpos,
        at: UnixMs,
    },
    Failed {
        agent: AgentId,
        text: String,
        error: String,
        retrying: bool,
        at: UnixMs,
    },
    Stopped {
        agent: AgentId,
        error: String,
        at: UnixMs,
    },
    /// One message of a Claude transcript, mirrored as Claude confirmed it.
    ClaudeMessage {
        agent: AgentId,
        speaker: Speaker,
        text: String,
        at: UnixMs,
    },
    ExecObserved {
        agent: AgentId,
        id: String,
        milestone: ExecMilestone,
        at: UnixMs,
    },
}

impl LogEvent {
    pub fn at(&self) -> UnixMs {
        match self {
            Self::WorksetCreated { at, .. }
            | Self::Evaluated { at, .. }
            | Self::AgentCreated { at, .. }
            | Self::AgentChanged { at, .. }
            | Self::Request { at, .. }
            | Self::CompactionRequested { at, .. }
            | Self::CallsEnded { at, .. }
            | Self::Replied { at, .. }
            | Self::Rewound { at, .. }
            | Self::Failed { at, .. }
            | Self::Stopped { at, .. }
            | Self::ClaudeMessage { at, .. }
            | Self::ExecObserved { at, .. } => *at,
        }
    }

    /// The agent the row is about; `None` for a workset row.
    pub fn agent(&self) -> Option<AgentId> {
        match self {
            Self::WorksetCreated { .. } | Self::Evaluated { .. } => None,
            Self::AgentCreated { agent, .. }
            | Self::AgentChanged { agent, .. }
            | Self::Request { agent, .. }
            | Self::CompactionRequested { agent, .. }
            | Self::CallsEnded { agent, .. }
            | Self::Replied { agent, .. }
            | Self::Rewound { agent, .. }
            | Self::Failed { agent, .. }
            | Self::Stopped { agent, .. }
            | Self::ClaudeMessage { agent, .. }
            | Self::ExecObserved { agent, .. } => Some(*agent),
        }
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{Address, AgentIdDomain, ChatEvent, Mention, MentionKind};

    use super::*;

    fn round_trips<
        T: senax_encoder::Packer + senax_encoder::Unpacker + PartialEq + std::fmt::Debug,
    >(
        frame: T,
    ) {
        let bytes = senax_encoder::pack(&frame).unwrap();
        let mut slice: &[u8] = &bytes;
        let decoded: T = senax_encoder::unpack(&mut slice).unwrap();
        assert_eq!(frame, decoded);
    }

    #[test]
    fn frames_round_trip() {
        let agent_id = AgentId::from_counter(1, &AgentIdDomain(7)).unwrap();
        let workset: WorksetId = "0123456789ab".parse().unwrap();
        round_trips(ClientFrame::FollowChat { since: ChatSeq(9) });
        round_trips(ClientFrame::FollowWorksets {
            from: vec![(workset, Wpos(3))],
        });
        round_trips(ServerFrame::Chat {
            records: vec![ChatRecord {
                seq: ChatSeq(4),
                at: UnixMs(5),
                event: ChatEvent::Posted {
                    topic: TopicId(ChatSeq(2)),
                    from: Address::Agent(agent_id),
                    mentions: vec![Mention {
                        to: Address::User,
                        kind: MentionKind::Ask,
                    }],
                    content: vec![ContentPart::Text {
                        text: "which branch?".into(),
                    }],
                },
            }],
        });
        round_trips(ServerFrame::Status {
            agent_id,
            state: RuntimeState {
                inference: super::super::transcript::InferenceState::Failed {
                    error: "quota".into(),
                },
                ..Default::default()
            },
        });
        round_trips(ServerFrame::Rows {
            workset,
            rows: vec![(
                Wpos(7),
                LogEvent::Request {
                    agent: agent_id,
                    inputs: vec![InputView {
                        message: Some(MessageId(ChatSeq(4))),
                        text: "hi".into(),
                        role: Role::User,
                    }],
                    answers: true,
                    compaction: false,
                    at: UnixMs(8),
                },
            )],
        });
    }

    #[test]
    fn requests_round_trip() {
        let agent = AgentId::from_counter(7, &AgentIdDomain(1)).unwrap();
        for request in [
            Request::from(Post {
                client_id: 7,
                topic: TopicId(ChatSeq(2)),
                mentions: vec![agent],
                content: vec![ContentPart::Image {
                    media_type: "image/gif".to_owned(),
                    data: vec![1, 2, 3],
                }],
            }),
            CreateWorkset {
                kind: WorksetKind::Host {
                    cwd: "/src/notes".into(),
                },
                code: "new_agent()".into(),
            }
            .into(),
            StartTopic {
                client_id: 8,
                stream: StreamId(ChatSeq(1)),
                name: "Fix the flaky test".into(),
                mentions: Vec::new(),
                content: vec![ContentPart::Text { text: "go".into() }],
            }
            .into(),
            CreateStream { name: "rho".into() }.into(),
            Leave {
                topic: TopicId(ChatSeq(2)),
                agent,
            }
            .into(),
            ChangeTopic {
                topic: TopicId(ChatSeq(2)),
                stream: StreamId(ChatSeq(1)),
                name: "Fixed the flaky test".into(),
                resolved: true,
            }
            .into(),
            MoveMessages {
                client_id: 9,
                messages: vec![MessageId(ChatSeq(5)), MessageId(ChatSeq(3))],
                to: MoveTo::NewTopic {
                    stream: StreamId(ChatSeq(1)),
                    name: "aside".into(),
                },
            }
            .into(),
            Cancel { agent }.into(),
            super::super::QuotaUsage.into(),
            super::super::SetAuthAccountEnabled {
                name: "work".to_owned(),
                enabled: false,
            }
            .into(),
        ] {
            round_trips(Open::Request(request));
        }
        round_trips(rho_rpc::protocol::Answer::Done(MessageId(ChatSeq(3))));
    }
}
