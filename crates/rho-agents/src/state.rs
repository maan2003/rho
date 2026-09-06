//! How a client draws an agent: the block list and status it folds from
//! the mirror and the live tail. Nothing here crosses the wire.

use rho_core::{MessagePhase, ToolOutputStatus, UnixMs};
use rho_ui_proto::MessageDelivery;
use rho_ui_proto::mirror::TextPhase;
use senax_encoder::{Decode, Encode, Pack, Unpack};

/// One agent's transcript as a client draws it: a flat block list plus a
/// coarse status. Folded on the client from the mirror, with the runtime's
/// live frame layered after the last durable block.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiAgentState {
    /// Shared with the fold that made them, so taking the state after a
    /// row copies pointers, never text.
    pub blocks: Vec<std::sync::Arc<UiBlock>>,
    pub status: UiAgentStatus,
    /// Tokens occupying the model's context window after the latest
    /// response; `None` until the agent's first response.
    pub context_used: Option<u64>,
    /// Cumulative billable usage for this agent across all of its turns.
    #[senax(default)]
    pub usage: UiAgentUsage,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiAgentUsage {
    pub provider: String,
    pub total: rho_ui_proto::AgentUsageBucket,
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
    Tool(UiTool),
    Notice {
        text: String,
    },
    /// A message waiting in the agent's queue; becomes a `UserMessage` (or
    /// `AgentMessage`) block at delivery. Always trails the transcript.
    QueuedMessage {
        text: String,
        delivery: MessageDelivery,
        /// The sending agent; `None` for the user.
        sender: Option<rho_ui_proto::AgentId>,
    },
    /// A delivered message from another agent.
    AgentMessage {
        /// The sending agent.
        sender: rho_ui_proto::AgentId,
        text: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiAgentStatus {
    Idle,
    Streaming,
    ToolCalling {
        /// Deadline of the batch's armed `wait` call, if one is parked
        /// until mail arrives or the wall clock passes it.
        waiting: Option<UnixMs>,
    },
    UnfinishedTurn {
        outstanding_calls: usize,
    },
    /// The turn failed permanently; the error text is the trailing unsealed
    /// [`UiBlock::Notice`].
    Error,
    /// The daemon stopped this client's live state stream. Retained transcript
    /// content may still be displayed, but it is no longer being updated.
    Unloaded,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiToolMetadata {
    ApplyPatch(UiApplyPatchMetadata),
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiApplyPatchMetadata {
    pub changes: Vec<UiToolFileChange>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiToolFileChange {
    pub path: String,
    pub status: UiToolFileStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiToolFileStatus {
    Added,
    Modified,
    Deleted,
    Moved,
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
    pub metadata: Option<UiToolMetadata>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiToolStatus {
    Running,
    Success,
    Error,
    Cancelled,
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
