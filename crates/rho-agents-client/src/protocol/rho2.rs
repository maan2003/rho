//! The rho2 agents protocol
//! ([ARCH-rho2](../../../../agent-host/specs/ARCH-rho2.md)): worksets, the
//! agent directory and mail.
//!
//! The session ([`Open::Session`]) carries the mail log, the directory, the
//! rows of the worksets a client follows, and the live tails of the agents
//! it looks at. Everything else is one call ([`Open::Request`]).

use rho_agent_types::{
    AgentId, ContentPart, ExecMilestone, InputWake, Mail, MailId, Props, SendKind, UnixMs,
    WorksetId, WorksetKind, Wpos,
};
use senax_encoder::{Decode, Encode, Pack, Unpack};

use super::transcript::{
    DetailBody, Item, Live, RuntimeKind, RuntimeState, Speaker, ToolOutcome, Usage,
};

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
        /// Answered when the workset is created, before `code` ends.
        /// Answered once the creation code ends.
        CreateWorkset(CreateWorkset) -> CreatedWorkset;
        /// Code run in a workset's namespace; answered when it ends.
        Eval(Eval) -> EvalResult;
        /// Mail from the user. Logged once per `client_id`.
        PostMail(PostMail) -> MailId;
        /// Stops the agent's current request and cells. Typed, because it
        /// must work while the workset's Python is stuck.
        Cancel(Cancel) -> ();
        DeleteWorksets(DeleteWorksets) -> ();
        /// Bulk: one workset's rows from `from`, a page at a time.
        WorksetLog(WorksetLog) -> WorksetLogPage, priority None;
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
    /// The user's first mail, to the first agent `code` creates. User mail
    /// comes only from the GUI, so Python cannot write it.
    pub message: Option<Vec<ContentPart>>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Eval {
    pub workset: WorksetId,
    pub code: String,
    /// As in [`CreateWorkset`]: mailed from the user to the first agent
    /// `code` creates.
    pub message: Option<Vec<ContentPart>>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct EvalResult {
    /// What the code printed.
    pub output: String,
    /// The traceback, when it raised.
    pub error: Option<String>,
    /// The agent the `message` was mailed to: the first one the code
    /// created. `None` without a message, or when the code created none.
    pub agent: Option<AgentId>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct CreatedWorkset {
    pub workset: WorksetId,
    /// How its creation code ran.
    pub result: EvalResult,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct PostMail {
    /// The client's own random name for the mail, so a resend after a lost
    /// answer is logged once.
    pub client_id: u64,
    pub to: AgentId,
    pub re: Option<MailId>,
    pub content: Vec<ContentPart>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct Cancel {
    pub agent: AgentId,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct DeleteWorksets {
    pub worksets: Vec<WorksetId>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode, Pack, Unpack)]
pub struct WorksetLog {
    pub workset: WorksetId,
    pub from: Wpos,
}

#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub struct WorksetLogPage {
    pub rows: Vec<(Wpos, LogEvent)>,
    /// Where the next page starts; `None` once the log is read to its end.
    pub next: Option<Wpos>,
}

/// What a client says on its session.
#[derive(Clone, Debug, PartialEq, Pack, Unpack)]
pub enum ClientFrame {
    /// After [`ServerFrame::Hello`]: the newest mail this client holds
    /// (zero for none). The host sends every later mail, then each new one.
    FollowMail { since: MailId },
    /// The worksets whose rows this client wants pushed as they land, each
    /// from the position it holds. Replaces the set wholesale.
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
        /// The newest mail on the host.
        mail_head: MailId,
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
    /// Mail in log order: the answer to `FollowMail`, paged, then each new one.
    Mail {
        mails: Vec<Mail>,
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
    /// The model, by the name usage is priced under.
    pub model: String,
    pub effort: String,
    pub props: Props,
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
    /// Something entered the agent's queue; its next request carries it.
    Input {
        agent: AgentId,
        mail: Option<MailId>,
        text: String,
        wake: InputWake,
        at: UnixMs,
    },
    CompactionRequested {
        agent: AgentId,
        at: UnixMs,
    },
    /// A request went out, carrying every input before it, with these results.
    Sent {
        agent: AgentId,
        results: Vec<ToolOutcome>,
        compaction: bool,
        at: UnixMs,
    },
    /// A notebook report answered provider calls but completed no task.
    NotebookReport {
        agent: AgentId,
        calls: Vec<String>,
        compaction: bool,
        at: UnixMs,
    },
    /// A Claude agent's tool results.
    Results {
        agent: AgentId,
        results: Vec<ToolOutcome>,
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
            | Self::Input { at, .. }
            | Self::CompactionRequested { at, .. }
            | Self::Sent { at, .. }
            | Self::NotebookReport { at, .. }
            | Self::Results { at, .. }
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
            | Self::Input { agent, .. }
            | Self::CompactionRequested { agent, .. }
            | Self::Sent { agent, .. }
            | Self::NotebookReport { agent, .. }
            | Self::Results { agent, .. }
            | Self::Replied { agent, .. }
            | Self::Rewound { agent, .. }
            | Self::Failed { agent, .. }
            | Self::Stopped { agent, .. }
            | Self::ClaudeMessage { agent, .. }
            | Self::ExecObserved { agent, .. } => Some(*agent),
        }
    }
}

/// What a mail to the user is for, on the wire with the mail.
pub type MailKind = SendKind;

#[cfg(test)]
mod tests {
    use rho_agent_types::{Address, AgentIdDomain};

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
        round_trips(ClientFrame::FollowMail { since: MailId(9) });
        round_trips(ClientFrame::FollowWorksets {
            from: vec![(workset, Wpos(3))],
        });
        round_trips(ServerFrame::Mail {
            mails: vec![Mail {
                id: MailId(4),
                at: UnixMs(5),
                from: Address::Agent(agent_id),
                to: Address::User,
                re: Some(MailId(2)),
                kind: Some(SendKind::Ask),
                content: vec![ContentPart::Text {
                    text: "which branch?".into(),
                }],
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
                LogEvent::Input {
                    agent: agent_id,
                    mail: Some(MailId(4)),
                    text: "hi".into(),
                    wake: InputWake::Person,
                    at: UnixMs(8),
                },
            )],
        });
    }

    #[test]
    fn requests_round_trip() {
        let agent = AgentId::from_counter(7, &AgentIdDomain(1)).unwrap();
        for request in [
            Request::from(PostMail {
                client_id: 7,
                to: agent,
                re: None,
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
                message: None,
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
        round_trips(rho_rpc::protocol::Answer::Done(MailId(3)));
    }
}
