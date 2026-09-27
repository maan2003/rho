//! The Rho runtime's own rows in an agent's log: what the model was shown,
//! what it answered, and the messages around it. The model's context and
//! the transcript are both projections of these, and they are only ever
//! appended.

use rho_agent_types::{AgentId, UnixMs};
use rho_inference::step::{Call, CallId, Carry, Image, Usage};
use senax_encoder::{Decode, Encode};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
pub struct MessageId(pub u64);

impl MessageId {
    pub fn new() -> Self {
        Self(rand::random())
    }
}

impl Default for MessageId {
    fn default() -> Self {
        Self::new()
    }
}

/// Who a message is from or to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode)]
pub enum Party {
    Human,
    Agent(AgentId),
}

/// Part of a message body. Text is kept as written; structure lives around
/// it, never inside it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Block {
    Text(String),
    Image(Image),
}

/// Why the model was woken.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Wake {
    Message,
    AgentMessage,
    /// The latest exec's code returned.
    Returned,
    Notify,
    /// A task failed and nothing claimed the failure.
    Failure,
    Checkin,
    /// The last step wrote prose and made no call.
    Prose,
    /// The host restarted; everything running is gone.
    Restarted,
    /// The model's history branched; notebook state was not changed.
    Rewound,
    /// Ask the provider to compact the current context.
    Compaction,
    /// Carry on after a compaction that interrupted owed work.
    CompactionReply,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Notice {
    /// A model request failed.
    Error(String),
    Restarted,
    Archived,
    FreshNotebook,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Entry {
    /// One model response and the calls it started.
    Step {
        at: UnixMs,
        calls: Vec<Call>,
        prose: String,
        carry: Carry,
        usage: Usage,
    },
    /// What one response cost, billed once, right after its step.
    Usage {
        at: UnixMs,
        usage: ResponseUsage,
    },
    /// What the model was shown when woken: news from the notebook, and the
    /// messages delivered with it.
    Woken {
        at: UnixMs,
        why: Wake,
        report: String,
        images: Vec<Image>,
        /// Messages this wake delivered into model context.
        messages: Vec<MessageId>,
        /// Messages handled without entering model context.
        acknowledged: Vec<MessageId>,
        /// The report as the answer to each call of the step before.
        results: Vec<CallResult>,
    },
    Received {
        at: UnixMs,
        id: MessageId,
        from: Party,
        body: Vec<Block>,
    },
    Sent {
        at: UnixMs,
        id: MessageId,
        to: Party,
        text: String,
    },
    Status {
        at: UnixMs,
        text: String,
    },
    /// Since when some task awaits `human.reply()`; `None` once none does.
    Awaiting {
        at: UnixMs,
        since: Option<UnixMs>,
    },
    /// Snapshot of notebook activity, independent of messages and human waits.
    Activity {
        at: UnixMs,
        responding: bool,
        running_tasks: u32,
        checkin_at: Option<UnixMs>,
        archived: bool,
    },
    Notice {
        at: UnixMs,
        notice: Notice,
    },
    /// Provider compaction, asked for through the model's normal input.
    CompactionTrigger {
        at: UnixMs,
        /// A pure manual request needs no model answer after compaction.
        manual: bool,
    },
}

/// A result for one call of a model response.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct CallResult {
    pub id: CallId,
    pub text: String,
    pub images: Vec<Image>,
}

/// Exact billable fields for one response.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ResponseUsage {
    pub model: String,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub output_tokens: u64,
}

impl ResponseUsage {
    /// Responses API input includes cache reads; charts keep fresh input
    /// separate.
    pub fn rho(model: String, usage: Usage) -> Self {
        Self {
            model,
            input_tokens: usage.input_tokens.saturating_sub(usage.cached_tokens),
            cache_read_tokens: usage.cached_tokens,
            cache_write_tokens: 0,
            cache_write_1h_tokens: 0,
            output_tokens: usage.output_tokens,
        }
    }
}

impl Entry {
    pub fn at(&self) -> UnixMs {
        match self {
            Entry::Step { at, .. }
            | Entry::Usage { at, .. }
            | Entry::Woken { at, .. }
            | Entry::Received { at, .. }
            | Entry::Sent { at, .. }
            | Entry::Status { at, .. }
            | Entry::Awaiting { at, .. }
            | Entry::Activity { at, .. }
            | Entry::Notice { at, .. }
            | Entry::CompactionTrigger { at, .. } => *at,
        }
    }
}

/// Runtime activity is independent of the messages it sends and whether a task
/// awaits the person. Kept separately so snapshots compare without timestamps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Activity {
    pub responding: bool,
    pub running_tasks: u32,
    pub checkin_at: Option<UnixMs>,
    pub archived: bool,
}

impl Activity {
    pub fn entry(self, at: UnixMs) -> Entry {
        Entry::Activity {
            at,
            responding: self.responding,
            running_tasks: self.running_tasks,
            checkin_at: self.checkin_at,
            archived: self.archived,
        }
    }
}
