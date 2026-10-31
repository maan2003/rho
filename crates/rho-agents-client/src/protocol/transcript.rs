//! The vocabulary of a transcript: what a workset row
//! ([`super::rho2::LogEvent`]) carries of a response and its calls, and the
//! live tail a runtime has past the log.

use rho_agent_types::UnixMs;
use senax_encoder::{Decode, Encode, Pack, Unpack};

/// Whose transcript this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum RuntimeKind {
    Rho,
    Claude,
}

/// Who said a mirrored Claude message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Speaker {
    User,
    Agent,
    Assistant,
}

/// What one model response cost, as the provider reported it. The model
/// is named so a client can price it; the tables the agent host keeps hold
/// the same numbers keyed by time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct Usage {
    pub model: String,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub output_tokens: u64,
}

/// How one call ended. The output is a body; ask for it by position.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ToolOutcome {
    pub id: String,
    pub status: ToolStatus,
    pub started_at: UnixMs,
    pub finished_at: UnixMs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ToolStatus {
    Success,
    /// A notebook report was delivered; the underlying task may continue.
    Reported,
    Error,
    Cancelled,
}

/// What a runtime has that the log does not yet, told as it changes:
/// the response in flight and the phase. Everything else a reader wants
/// is a row: the queue is `Message` rows no `Sent` has carried, a call
/// runs until a `Sent` answers it, a turn ends with a `Turn` row.
///
/// Each snapshot replaces the runtime state, streaming response and tentative
/// draft. Durable transcript rows remain the source of committed history.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Live {
    Snapshot {
        state: RuntimeState,
        response: Option<StreamingResponse>,
        /// Host-computed draft retained while the originating cell runs.
        #[senax(default)]
        draft: Option<String>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct RuntimeState {
    pub inference: InferenceState,
    pub running_tasks: u32,
    pub awaiting_human: bool,
    pub checkin_at: Option<UnixMs>,
    pub archived: bool,
    /// The agent runs in a workset process older than its agent host's
    /// build, so fixes since reach it only when the workset restarts.
    #[senax(default)]
    pub stale: bool,
}

impl RuntimeState {
    pub fn is_working(&self) -> bool {
        matches!(
            self.inference,
            InferenceState::Responding | InferenceState::Retrying { .. }
        ) || (!self.archived && self.running_tasks > 0)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum InferenceState {
    #[default]
    Idle,
    Responding,
    Retrying {
        at: UnixMs,
        error: String,
    },
    Failed {
        error: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct StreamingResponse {
    pub id: String,
    pub items: Vec<Item>,
}

/// One response item, shared by the live tail, committed mirror, and `Detail`.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Item {
    Text {
        text: String,
        phase: Option<TextPhase>,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: String,
        /// How to read `arguments`.
        format: ArgumentsFormat,
    },
}

/// What a call's `arguments` string holds. A function tool is given a JSON
/// object, which is only whole once the call is; a custom tool is given the
/// text the model wrote, which is never JSON and must not be parsed as it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ArgumentsFormat {
    Json,
    Text,
}

/// Whether a text item is the model thinking aloud or its answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum TextPhase {
    Commentary,
    FinalAnswer,
}

/// The bodies one raw event carries, for a client that asked.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum DetailBody {
    /// The event is not there, or has no body worth asking for.
    Nothing,
    /// A `Sent`: the results, output and all.
    Results(Vec<DetailResult>),
    /// A `Replied`: the response whole, reasoning and arguments included.
    Response(Vec<Item>),
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct DetailResult {
    pub id: String,
    pub status: ToolStatus,
    pub output: String,
    pub error: Option<String>,
}
