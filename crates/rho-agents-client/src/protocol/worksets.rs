//! The agents protocol
//! ([ARCH-agent-host](../../../../agent-host/specs/ARCH-agent-host.md)):
//! worksets and the agent directory. Mail is its own protocol (`rho-chat`).
//!
//! The session ([`Open::Session`]) carries the directory, the rows of the
//! worksets a client follows, and the live tails of the agents it looks at.
//! Everything else is one call ([`Open::Request`]).

use rho_agent_types::{
    AgentId, EndpointId, MessageId, Props, Role, UnixMs, WorksetId, WorksetKind, Wpos,
};
use senax_encoder::{Decode, Encode, Pack, Unpack};

use super::transcript::{Item, Live, RuntimeKind, RuntimeState, Speaker, Usage};

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
        /// Stops the agent's current request and cells. Typed, because it
        /// must work while the workset's Python is stuck.
        Cancel(Cancel) -> CancelResult;
        /// End everything in the agent's shared workset, replace its worker
        /// with the host's current build, and reload the agent.
        RestartWorkset(RestartWorkset) -> ();
        /// The user's words to one agent, under `parent` or as a new root;
        /// the agent joins the message's thread. Answered with the
        /// message's id. For tools such as `rho-deploy` and `rho agent`.
        TellAgent(TellAgent) -> MessageId;
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
        RewindAgent(RewindAgent) -> RewoundAgent;
        StopAgent(StopAgent) -> ();
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
pub struct RewindAgent {
    pub agent: AgentId,
    pub turns: u32,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct RewoundAgent {
    pub taken_back: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct StopAgent {
    pub agent: AgentId,
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

impl EvalResult {
    /// What the code raised, in the exception's own words: the last line of
    /// its traceback, without the exception's type, such as `an agent keeps
    /// its runtime kind` of `RuntimeError: an agent keeps its runtime kind`.
    pub fn raised(&self) -> Option<&str> {
        let traceback = self.error.as_deref()?;
        let last = traceback
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .unwrap_or(traceback);
        let words = last.split_once(": ").filter(|(kind, _)| {
            !kind.is_empty()
                && kind
                    .chars()
                    .all(|char| char.is_alphanumeric() || char == '_' || char == '.')
        });
        Some(words.map_or(last, |(_, words)| words.trim()))
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct CreatedWorkset {
    pub workset: WorksetId,
    /// How its creation code ran.
    pub result: EvalResult,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Cancel {
    pub agent: AgentId,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub enum CancelResult {
    WasAsking,
    NoRequest,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct RestartWorkset {
    pub agent: AgentId,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct TellAgent {
    pub agent: AgentId,
    pub parent: Option<MessageId>,
    pub text: String,
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
    /// The worksets whose rows this client wants pushed as they land, each
    /// from the position it holds. Replaces the set wholesale; a workset
    /// this stream already follows goes on from the rows it sent, so none
    /// is sent twice.
    FollowWorksets { from: Vec<(WorksetId, Wpos)> },
    /// The agents whose live frames this client wants. Replaces the set.
    Focus { agent_ids: Vec<AgentId> },
}

/// What a host says on its session.
#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum ServerFrame {
    /// The first frame.
    Hello {
        machine_seed: u64,
        /// The host's iroh identity, which names its threads.
        endpoint: EndpointId,
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
        /// When the agent started doing what `state` says it does, on the
        /// host's clock: what its age on every device counts from.
        since: UnixMs,
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

impl SpecView {
    pub fn runtime_label(&self) -> String {
        format!("{}-{}", self.model, self.effort)
    }
}

/// One thing a request carried, with its body left as text.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct InputView {
    pub message: Option<MessageId>,
    pub text: String,
    pub role: Role,
}

/// One workset row as it crosses the wire: `strip` of exactly one row.
/// Bodies a person does not read at a glance are left behind.
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
        /// How the call it answers went.
        timing: Option<rho_agent_types::CallTiming>,
        /// Text the notebook reported for the call it answers.
        #[senax(default)]
        output: String,
        at: UnixMs,
    },
    /// The cell of the agent's call ended, whether or not a request answers it.
    CellEnded {
        agent: AgentId,
        end: rho_agent_types::CellEnd,
        at: UnixMs,
    },
    /// Claude Code says these calls came back, with no request to answer them.
    CallsEnded {
        agent: AgentId,
        calls: Vec<CallView>,
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
    /// The user cancelled what the agent had in flight.
    Cancelled {
        agent: AgentId,
        at: UnixMs,
    },
    MailWritten {
        agent: AgentId,
        message: MessageId,
        /// An old host log row may not identify which revision it wrote.
        #[senax(default)]
        version: Option<rho_agent_types::ChatSeq>,
        at: UnixMs,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct CallView {
    pub id: String,
    pub output: String,
    pub failed: Option<bool>,
}

impl LogEvent {
    pub fn at(&self) -> UnixMs {
        match self {
            Self::WorksetCreated { at, .. }
            | Self::Evaluated { at, .. }
            | Self::AgentCreated { at, .. }
            | Self::AgentChanged { at, .. }
            | Self::Request { at, .. }
            | Self::CellEnded { at, .. }
            | Self::CompactionRequested { at, .. }
            | Self::CallsEnded { at, .. }
            | Self::Replied { at, .. }
            | Self::Rewound { at, .. }
            | Self::Failed { at, .. }
            | Self::Stopped { at, .. }
            | Self::Cancelled { at, .. }
            | Self::ClaudeMessage { at, .. }
            | Self::MailWritten { at, .. } => *at,
        }
    }

    /// The agent the row is about; `None` for a workset row.
    pub fn agent(&self) -> Option<AgentId> {
        match self {
            Self::WorksetCreated { .. } | Self::Evaluated { .. } => None,
            Self::AgentCreated { agent, .. }
            | Self::AgentChanged { agent, .. }
            | Self::Request { agent, .. }
            | Self::CellEnded { agent, .. }
            | Self::CompactionRequested { agent, .. }
            | Self::CallsEnded { agent, .. }
            | Self::Replied { agent, .. }
            | Self::Rewound { agent, .. }
            | Self::Failed { agent, .. }
            | Self::Stopped { agent, .. }
            | Self::Cancelled { agent, .. }
            | Self::ClaudeMessage { agent, .. }
            | Self::MailWritten { agent, .. } => Some(*agent),
        }
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::AgentIdDomain;

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
    fn frames_round_trip() {
        let agent = AgentId::from_counter(3, &AgentIdDomain(7)).unwrap();
        round_trips(ClientFrame::FollowWorksets {
            from: vec![(WorksetId([1; 6]), Wpos(4))],
        });
        round_trips(ClientFrame::Focus {
            agent_ids: vec![agent],
        });
        round_trips(ServerFrame::Hello {
            machine_seed: 9,
            endpoint: EndpointId([9; 32]),
            agent_counter: 3,
        });
        round_trips(ServerFrame::Rows {
            workset: WorksetId([1; 6]),
            rows: vec![(
                Wpos(2),
                LogEvent::Request {
                    agent,
                    inputs: vec![InputView {
                        message: Some(MessageId(rho_agent_types::ChatSeq(4))),
                        text: "hi".into(),
                        role: Role::User,
                    }],
                    answers: true,
                    compaction: false,
                    timing: None,
                    output: "printed line".into(),
                    at: UnixMs(7),
                },
            )],
        });
    }

    #[test]
    fn requests_round_trip() {
        let agent = AgentId::from_counter(3, &AgentIdDomain(7)).unwrap();
        round_trips(Open::Request(
            CreateWorkset {
                kind: WorksetKind::Isolated,
                code: "new_agent()".into(),
            }
            .into(),
        ));
        round_trips(Open::Request(
            Eval {
                workset: WorksetId([3; 6]),
                code: "new_agent()".into(),
            }
            .into(),
        ));
        round_trips(Open::Request(Cancel { agent }.into()));
        round_trips(Open::Request(
            TellAgent {
                agent,
                parent: Some(MessageId(rho_agent_types::ChatSeq(4))),
                text: "deploy".into(),
            }
            .into(),
        ));
        round_trips(Open::Request(
            AgentLog {
                agent,
                from: Wpos(0),
            }
            .into(),
        ));
        round_trips(rho_rpc::protocol::Answer::Done(EvalResult {
            output: "x".into(),
            error: None,
            agent: Some(agent),
        }));
    }

    fn raising(error: Option<&str>) -> EvalResult {
        EvalResult {
            output: String::new(),
            error: error.map(str::to_owned),
            agent: None,
        }
    }

    #[test]
    fn what_code_raised_is_the_exception_in_its_own_words() {
        let traceback = "Traceback (most recent call last):\n  File \"<eval>\", line 1, in <module>\n  File \"/policy/rho.py\", line 80, in set_runtime\n    raise RuntimeError(\"an agent keeps its runtime kind\")\nRuntimeError: an agent keeps its runtime kind\n";
        assert_eq!(
            raising(Some(traceback)).raised(),
            Some("an agent keeps its runtime kind")
        );
        // A message that itself holds `: ` keeps all of it.
        assert_eq!(
            raising(Some("rho.errors.Refused: model: unknown, try sol-high")).raised(),
            Some("model: unknown, try sol-high")
        );
        // A message with no exception type is said as it is.
        assert_eq!(
            raising(Some("the workset is closed: try later")).raised(),
            Some("the workset is closed: try later")
        );
        assert_eq!(raising(None).raised(), None);
    }
}
