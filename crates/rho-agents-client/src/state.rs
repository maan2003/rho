//! How a client draws an agent: the block list and status it folds from
//! the workset rows, chat messages, and the live tail. Nothing here crosses the
//! wire.

use rho_agent_types::{MessagePhase, ToolOutputStatus, UnixMs};
use senax_encoder::{Decode, Encode, Pack, Unpack};

use crate::protocol::transcript::{ArgumentsFormat, TextPhase};

/// One agent's transcript as a client draws it: a flat block list plus a
/// coarse status. Folded on the client from the mirror, with the runtime's
/// live frame layered after the last durable block.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiAgentState {
    /// Shared with the fold that made them, so taking the state after a
    /// row copies pointers, never text.
    pub blocks: Vec<std::sync::Arc<UiBlock>>,
    pub status: UiAgentStatus,
    /// Latest ephemeral runtime snapshot; absent when disconnected.
    pub runtime: Option<crate::protocol::transcript::RuntimeState>,
    /// Tokens occupying the model's context window after the latest
    /// response; `None` until the agent's first response.
    pub context_used: Option<u64>,
    /// Cumulative billable usage for this agent across all of its turns.
    #[senax(default)]
    pub usage: UiAgentUsage,
    #[senax(default)]
    pub exec_timings:
        std::sync::Arc<std::collections::BTreeMap<String, rho_agent_types::ExecTiming>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiAgentUsage {
    pub provider: String,
    pub total: crate::protocol::AgentUsageBucket,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiBlock {
    UserMessage {
        text: String,
    },
    AssistantMessage {
        text: String,
        phase: Option<UiMessagePhase>,
    },
    Reasoning {
        text: String,
    },
    /// Host-owned tentative outgoing text while its notebook cell has not sent.
    MessageDraft {
        /// The agent writing it.
        from: rho_agent_types::AgentId,
        text: String,
    },
    Tool(UiTool),
    Notice {
        text: String,
    },
    /// A message waiting in the agent's queue; becomes a `UserMessage` (or
    /// `AgentMessage`) block at delivery. Always trails the transcript.
    QueuedMessage {
        text: String,
        /// The sending agent; `None` for the user.
        sender: Option<rho_agent_types::AgentId>,
    },
    /// A delivered message from another agent.
    MessageSent {
        to: Option<rho_agent_types::AgentId>,
        text: String,
    },
    AgentMessage {
        /// The sending agent.
        sender: rho_agent_types::AgentId,
        text: String,
    },
    /// A message the user wrote that no host has taken yet. Always last:
    /// after the agent's own queue.
    Unsent {
        text: String,
    },
    /// What the agent says it is doing, from its status line. Last of all,
    /// just above the draft: the status bar is too narrow to hold it.
    Status {
        text: String,
    },
    /// A message in a chat topic: who posted it and whom it mentions.
    ChatMessage {
        /// What starting an agent from it links to.
        id: rho_agent_types::MessageId,
        from: rho_agent_types::Address,
        mentions: Vec<rho_agent_types::Mention>,
        text: String,
        /// Its sender's name heads it: an agent's message after another
        /// agent's, the user's in between not counted, in a topic more
        /// than one agent wrote in.
        named: bool,
    },
    /// An agent joined or left a chat topic.
    Member {
        agent: rho_agent_types::AgentId,
        change: crate::chat::Membership,
    },
}

impl UiBlock {
    /// The agents it names: whose label drawing it needs.
    pub fn agents(&self) -> Vec<rho_agent_types::AgentId> {
        match self {
            UiBlock::AgentMessage { sender, .. }
            | UiBlock::MessageSent {
                to: Some(sender), ..
            }
            | UiBlock::QueuedMessage {
                sender: Some(sender),
                ..
            } => vec![*sender],
            UiBlock::Member { agent, .. } => vec![*agent],
            UiBlock::ChatMessage { from, mentions, .. } => std::iter::once(*from)
                .chain(mentions.iter().map(|mention| mention.to))
                .filter_map(|address| match address {
                    rho_agent_types::Address::Agent(agent) => Some(agent),
                    rho_agent_types::Address::User => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiAgentStatus {
    Idle,
    Streaming,
    Retrying {
        at: UnixMs,
    },
    ToolCalling {
        /// Deadline of the batch's armed `wait` call, if one is parked
        /// until a message arrives or the wall clock passes it.
        waiting: Option<UnixMs>,
    },
    UnfinishedTurn {
        outstanding_calls: usize,
    },
    /// The turn failed permanently; the error text is the trailing unsealed
    /// [`UiBlock::Notice`].
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiTool {
    pub id: String,
    pub name: String,
    pub arguments: String,
    pub preview: Option<String>,
    pub status: UiToolStatus,
    pub output: Option<String>,
    pub error: Option<String>,
    pub started_at: Option<UnixMs>,
    pub finished_at: Option<UnixMs>,
    #[senax(default)]
    pub timing: rho_agent_types::ExecTiming,
    /// Whether `arguments` is JSON or the raw text the model wrote. A text
    /// tool's arguments are shown as they are and never parsed.
    pub format: ArgumentsFormat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiToolStatus {
    Running,
    Success,
    Reported,
    Error,
    Cancelled,
    /// Its agent stopped working before any report closed it: not
    /// running. The report comes with the agent's next request.
    Ended,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiMessagePhase {
    Commentary,
    FinalAnswer,
}

impl From<TextPhase> for UiMessagePhase {
    fn from(phase: TextPhase) -> Self {
        match phase {
            TextPhase::Commentary => Self::Commentary,
            TextPhase::FinalAnswer => Self::FinalAnswer,
        }
    }
}

impl From<MessagePhase> for UiMessagePhase {
    fn from(phase: MessagePhase) -> Self {
        match phase {
            MessagePhase::Commentary => Self::Commentary,
            MessagePhase::FinalAnswer => Self::FinalAnswer,
        }
    }
}

impl From<ToolOutputStatus> for UiToolStatus {
    fn from(status: ToolOutputStatus) -> Self {
        match status {
            ToolOutputStatus::Success => Self::Success,
            ToolOutputStatus::Error => Self::Error,
            ToolOutputStatus::Cancelled => Self::Cancelled,
        }
    }
}
