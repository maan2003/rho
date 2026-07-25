//! Inputs waiting to reach the model.
//!
//! Every source works the same way: it accumulates on its own, reports how it
//! is doing, and is *pulled* by the core at a moment the core chooses. Nothing
//! outside the core decides when a request happens; the core never decides what
//! a source has to say.

mod mail;
mod user;

use rho_core::{ContentPart, UnixMs};
use senax_encoder::{Decode, Encode};

pub use crate::source::mail::MailSource;
pub use crate::source::user::UserSource;
use crate::tool::{Told, ToolActivity, Unsent};

/// The only scheduling lever a sender has: whether this input is worth
/// throwing away an in-flight request for.
///
/// There is deliberately no "deliver after the current task" mode. Prose says
/// that better than an enum can — "once you've finished the edits, run the
/// tests" is a boundary no variant could express.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub enum Delivery {
    /// Abort the in-flight request so this lands now.
    Interrupt,
    /// Ride along with the next request, whenever the core makes one.
    #[default]
    NextRequest,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum InputSource {
    User,
    Mail { sender: rho_core::AgentId },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct QueuedInput {
    pub source: InputSource,
    pub kind: InputKind,
    pub delivery: Delivery,
    pub at: UnixMs,
}

/// One source, whether or not it has anything to say.
///
/// Facts and nothing else — when something arrived, whether a call has been
/// answered — because a duration a source picked for itself would be a duration
/// picked without seeing what else is waiting. Even "is this worth sending" is
/// left to `boundary`: an empty queue and a tool that has produced nothing are
/// both reported, because being empty is a fact too, and for a tool it is one
/// that changes the answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceKind {
    /// Typed input is whole on arrival, so it never has more to say.
    /// `interrupt` is the one rule no other source has: a message worth
    /// throwing away an in-flight request for.
    User {
        interrupt: bool,
        /// The longest-waiting message, if anything is queued at all.
        oldest_at: Option<UnixMs>,
    },
    Mail {
        oldest_at: Option<UnixMs>,
        newest_at: Option<UnixMs>,
    },
    /// A called tool, reported exactly as it reports itself. There is
    /// deliberately no tidier enum in between: naming the states here would be
    /// deciding what they mean, and any name that collapsed two of these facts
    /// would put a decision outside the one place decisions live — which is how
    /// a request caused by a person came to demote somebody else's tool.
    Tool {
        called_at: UnixMs,
        told: Told,
        activity: ToolActivity,
        unsent: Unsent,
    },
}
