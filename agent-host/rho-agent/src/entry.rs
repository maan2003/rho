//! The Rho runtime's own rows in an agent's log: what the model was shown,
//! what it answered, and the messages around it. The model's context and
//! the transcript are both projections of these, and they are only ever
//! appended.

use rho_agent_types::{AgentId, UnixMs};
use senax_encoder::{Decode, Encode};

use crate::inference::{Carry, Image, Usage};

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

/// New contributions collected for one inference attempt. Retries append
/// another report; request construction merges them until a Step closes the
/// input group.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct Report {
    pub notices: Vec<RequestNotice>,
    pub notebook: rho_notebook::Report,
    /// Queued Received records delivered by this send, in delivery order.
    pub messages: Vec<MessageId>,
    /// Queued inputs handled without being shown to the model.
    pub acknowledged: Vec<MessageId>,
}

impl Report {
    pub fn merge(&mut self, newer: Self) {
        for notice in newer.notices {
            if !self.notices.contains(&notice) {
                self.notices.push(notice);
            }
        }
        self.notebook.merge(newer.notebook);
        self.messages.extend(newer.messages);
        self.acknowledged.extend(newer.acknowledged);
    }

    pub fn is_empty(&self) -> bool {
        self.notices.is_empty() && self.notebook.is_empty() && self.messages.is_empty()
    }

    pub fn render(&self) -> rho_notebook::RenderedReport {
        let mut rendered = self.notebook.render();
        let substantive = !self.notebook.is_empty()
            || !self.messages.is_empty()
            || self
                .notices
                .iter()
                .any(|n| !matches!(n, RequestNotice::Checkin | RequestNotice::NothingNew));
        let mut parts: Vec<String> = self
            .notices
            .iter()
            .filter(|n| {
                !substantive || !matches!(n, RequestNotice::Checkin | RequestNotice::NothingNew)
            })
            .map(|notice| notice.text().to_owned())
            .collect();
        if !rendered.text.is_empty() {
            parts.push(rendered.text);
        }
        if parts.is_empty() && !self.messages.is_empty() {
            parts.push("New messages below.".into());
        }
        rendered.text = parts.join("\n\n");
        rendered
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum RequestNotice {
    Restarted,
    Rewound,
    FreshNotebook,
    InterruptedExecution,
    PreviousResponseHadNoExec,
    Checkin,
    NothingNew,
}

impl RequestNotice {
    fn text(&self) -> &'static str {
        match self {
            Self::Restarted => {
                "rho restarted. Your notebook and everything running in it are gone, and their side effects may remain. Check the current state before carrying on."
            }
            Self::Rewound => {
                "The human rewound your visible history. Your Python notebook, running work, and side effects were not rewound. Check the current state before continuing."
            }
            Self::FreshNotebook => {
                "This agent was archived. You have a fresh notebook; earlier Python state and running work are gone."
            }
            Self::InterruptedExecution => {
                "Your response was cut off while you were writing its cell; only the code shown ran. Carry on from the notebook's state without replaying it."
            }
            Self::PreviousResponseHadNoExec => {
                "Your last response had no exec call. Text outside a call reaches nobody: speak with human.send()."
            }
            Self::Checkin => "Check-in: nothing new.",
            Self::NothingNew => "Nothing new.",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Entry {
    Step {
        at: UnixMs,
        exec: Option<String>,
        prose: String,
        carry: Carry,
        /// Complete billable usage, absent for interrupted output.
        usage: Option<ResponseUsage>,
    },
    /// Logical send boundary, not provider acknowledgment.
    RequestSent {
        at: UnixMs,
        why: Wake,
        report: Report,
        compact: bool,
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
    Awaiting {
        at: UnixMs,
        since: Option<UnixMs>,
    },
    Notice {
        at: UnixMs,
        notice: Notice,
    },
    /// Durable queued intent; RequestSent records when an attempt picks it up.
    CompactionTrigger {
        at: UnixMs,
        manual: bool,
    },
}

/// Derived scheduling facts shared by the live loop and durable recovery.
/// The log, not this projection, owns compaction intent and completion.
#[derive(Clone, Debug, Default, Encode, Decode)]
pub struct CompactionState {
    pub context_used: Option<u64>,
    pub pending: bool,
    pub sent: bool,
    pub owes_reply: bool,
    pub reply: bool,
}

impl CompactionState {
    pub fn observe(&mut self, entry: &Entry) {
        match entry {
            Entry::CompactionTrigger { manual, .. } => {
                self.pending = true;
                self.owes_reply = !manual;
            }
            Entry::RequestSent {
                report, compact, ..
            } => {
                self.sent = *compact;
                if *compact {
                    self.owes_reply |= !report.is_empty();
                } else {
                    self.reply = false;
                }
            }
            Entry::Step { carry, usage, .. } => {
                let compacted = carry.has_compaction();
                if compacted {
                    self.context_used = None;
                } else if let Some(usage) = usage {
                    self.context_used = Some(
                        usage
                            .input_tokens
                            .saturating_add(usage.cache_read_tokens)
                            .saturating_add(usage.output_tokens),
                    );
                }
                if self.sent {
                    self.reply = compacted && self.owes_reply;
                    self.sent = false;
                    self.pending = false;
                    self.owes_reply = false;
                }
            }
            _ => {}
        }
    }
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
            | Entry::RequestSent { at, .. }
            | Entry::Received { at, .. }
            | Entry::Sent { at, .. }
            | Entry::Status { at, .. }
            | Entry::Awaiting { at, .. }
            | Entry::Notice { at, .. }
            | Entry::CompactionTrigger { at, .. } => *at,
        }
    }
}
