//! Values the workset log's rows and the usage tables share between the
//! host and the workers.

use redb_derive::{Key, Value as RedbValue};
use rho_agent_types::transcript::MessageSender;
use rho_agent_types::{ContentPart, UnixMs};
use senax_encoder::{Decode, Encode};
use uuid::Uuid;

pub const AGENT_USAGE_BUCKET_MS: u64 = 5 * 60 * 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue, Encode, Decode)]
pub struct AgentUsageModel(u8);

impl AgentUsageModel {
    pub const UNKNOWN: Self = Self(0);
    pub const GPT: Self = Self(1);
    pub const FABLE: Self = Self(2);
    pub const OPUS: Self = Self(3);
    pub const TERRA: Self = Self(4);
    pub const LUNA: Self = Self(5);
    pub const ASTRA: Self = Self(7);

    pub fn name(self) -> &'static str {
        match self {
            Self::GPT => "gpt",
            Self::FABLE => "fable",
            Self::OPUS => "opus",
            Self::TERRA => "terra",
            Self::LUNA => "luna",
            Self::ASTRA => "astra",
            _ => "unknown",
        }
    }

    /// The model [`Self::name`] names.
    pub fn named(name: &str) -> Self {
        [
            Self::GPT,
            Self::FABLE,
            Self::OPUS,
            Self::TERRA,
            Self::LUNA,
            Self::ASTRA,
        ]
        .into_iter()
        .find(|model| model.name() == name)
        .unwrap_or(Self::UNKNOWN)
    }
}

impl Default for AgentUsageModel {
    fn default() -> Self {
        Self::UNKNOWN
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct AgentUsageBucket {
    pub bucket_start_ms: u64,
    pub model: AgentUsageModel,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub output_tokens: u64,
    pub requests: u64,
    pub approximate: bool,
}

impl AgentUsageBucket {
    pub fn add(&mut self, other: &Self) {
        if self.requests == 0 {
            self.model = other.model;
        } else if other.model != AgentUsageModel::UNKNOWN && self.model != other.model {
            self.model = AgentUsageModel::UNKNOWN;
        }
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
        self.cache_write_1h_tokens = self
            .cache_write_1h_tokens
            .saturating_add(other.cache_write_1h_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.requests = self.requests.saturating_add(other.requests);
        self.approximate |= other.approximate;
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ClaudeRewind {
    pub source_session_id: Uuid,
    pub session_id: Uuid,
    pub resume_at: Option<Uuid>,
}

/// Leased notebook contributions transferred to durable host ownership before
/// contacting Claude Code. A handoff does not prove remote consumption.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ClaudeOutputBatch {
    pub id: uuid::Uuid,
    pub outputs: Vec<(
        rho_agent_types::transcript::ExecId,
        rho_agent_types::transcript::ToolOutput,
    )>,
    pub wake: WakeFacts,
    pub at: UnixMs,
}

/// Why a request went out when it did: the scheduler's reading of its
/// sources at the boundary (`agent/boundary.rs`), recorded with the request
/// so the pace of a session can be judged from its log.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct WakeFacts {
    /// The candidate whose deadline came first.
    pub trigger: WakeTrigger,
    /// Every event pending at the boundary, all of which the request carries.
    pub events: Vec<WakeEvent>,
    /// Jobs and cells still running that the model was watching...
    pub foreground_running: u64,
    /// ...and ones it had moved on from.
    pub background_running: u64,
    /// The latest cell asked not to be woken by the notebook.
    pub tools_suppressed: bool,
    /// When the model's check-in was due, if its turn had one.
    pub checkin_at: Option<UnixMs>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum WakeTrigger {
    /// A previously selected durable output batch is handed off.
    Delivery,
    /// A request somebody asked for outright: a retry, a compaction.
    Asked,
    /// A failed request's own clock.
    Retry,
    User,
    Mail,
    /// A cell's `notify()`.
    Notify,
    /// A job ended, or a cell returned with output.
    Finished,
    /// The model's check-in came due.
    Checkin,
}

/// One pending event as the scheduler saw it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct WakeEvent {
    pub cell: u64,
    /// The job's registration time, or `u64::MAX` for the cell itself.
    pub source: u64,
    pub kind: WakeKind,
    pub foreground: bool,
    /// When the tool recorded it.
    pub occurred_at: UnixMs,
    /// When the scheduler first saw it while able to act: where its
    /// patience was measured from.
    pub seen_at: UnixMs,
    /// When it would have sent on its own; `None` for an event content to
    /// wait for the foreground or the check-in.
    pub deadline: Option<UnixMs>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
pub enum WakeKind {
    Notify,
    Succeeded,
    Failed,
}

/// One input in the live Claude queue waiting to reach the model.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct QueuedInput {
    pub source: MessageSender,
    pub kind: InputKind,
    pub at: UnixMs,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum InputKind {
    Message {
        content: Vec<ContentPart>,
    },
    /// The user explicitly asked to compact. Automatic compaction is not an
    /// input at all — it happens while building a request.
    Compaction,
}

/// What one transcript line says, as far as a reader needs. Bodies are
/// whole: the wire strips them.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum TranscriptLine {
    /// The person spoke (an agent's mail reaches Claude the same way).
    User { text: String },
    /// The model spoke or called. One line per content block, so a text
    /// and the call after it are two rows. Usage rides on the first row
    /// of a message only, so a reader counts each request once.
    Assistant {
        text: String,
        calls: Vec<TranscriptCall>,
        usage: Option<AgentUsageBucket>,
        /// Context-window occupancy after this message.
        context_used: Option<u64>,
    },
    /// What the calls came back with.
    ToolResults {
        results: Vec<rho_agent_types::transcript::ToolResult>,
    },
    /// Claude compacted the context here.
    Compacted { context_used: Option<u64> },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct TranscriptCall {
    pub id: String,
    pub name: String,
    /// The arguments as JSON, whole.
    pub arguments: String,
}
