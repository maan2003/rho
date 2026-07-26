//! Tools, and what they report about themselves.
//!
//! A tool is not a future that resolves once. It is a source that produces
//! output over its lifetime, possibly for hours, and no tool declares itself a
//! background job: a background job is simply a call that outlived the window
//! the model gave it. `DECISION-model-sets-the-pace`.
//!
//! Because the core pulls, a tool holds its own output until asked, and answers
//! at that moment in whatever shape it judges best.
//! `DECISION-pull-based-sources`.
//!
//! The core says exactly one thing to a running tool — [`ToolSession::cancel`],
//! meaning *wind down*. Everything else flows the other way.

use std::sync::Arc;

use rho_core::{ToolCall, ToolOutput, ToolSpec, UnixMs};
use tokio::sync::Notify;

/// How much of a hurry a tool's unsent output is in, and since when.
///
/// A hint for `boundary` and nothing else — it is not the tool's state, and
/// nothing outside the decision may read it. Whether a call has been answered
/// is the core's own bookkeeping and whether a tool can be forgotten is
/// [`ToolSession::done`]; neither is here, so neither can be inferred from a
/// hint the tool is free to revise.
///
/// One enum rather than one for working-or-not and another for what is unsent,
/// because no question the decision asks is answered by half of it — and
/// because two would spell pairs that cannot happen: output is not still
/// mid-thought once whatever was writing it has stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolHaste {
    /// None: the model has seen everything said so far.
    None,
    /// Holding output mid-thought. Worth sending eventually — half a build log
    /// beats none — but worth less than the whole, so it is the most patient
    /// thing the core knows about.
    Eventually { since: UnixMs },
    /// Holding output that stands on its own: this much matters now, whatever
    /// else the tool goes on to say. The one thing a working tool can say about
    /// its own urgency, and it buys exactly one thing: it stops waiting for the
    /// rest of the call. It still cannot interrupt a request in flight.
    ///
    /// `since` is when it became worth sending, not when it started arriving.
    Soon { since: UnixMs },
    /// The work finished at `at`, so whatever is unsent is final and waiting
    /// buys nothing. Says nothing about whether anything is left to say — that
    /// is [`ToolSession::done`], and a tool may well report this while it still
    /// has a summary to give.
    Ended { at: UnixMs },
}

/// Tell the core that something changed.
///
/// Deliberately carries no payload: what changed is discovered by asking, at a
/// moment the core picks. That is what lets several sources collapse into one
/// request instead of each waking one.
#[derive(Clone, Debug)]
pub struct SourceWaker(Arc<Notify>);

impl SourceWaker {
    pub(crate) fn new(notify: Arc<Notify>) -> Self {
        Self(notify)
    }

    /// Signal new output, or an exit. Cheap, and safe to call as often as you
    /// like — the core coalesces.
    pub fn wake(&self) {
        // `notify_one` stores a permit, so a wake that lands while the core is
        // busy is not lost.
        self.0.notify_one();
    }
}

/// One running invocation of a tool.
///
/// Output comes out in two shapes because a provider takes exactly one result
/// per call and everything after it is an update:
/// `REQ-provider-transcript-protocol`. The split is here rather than left to
/// the core to sort out, so the required one cannot be missing.
pub trait ToolSession: Send {
    /// How much of a hurry whatever it is holding is in.
    ///
    /// Read only by the decision that picks the next request boundary. It never
    /// governs what is collected — every call is asked for its one result, and
    /// every answered call is asked for updates, whatever this says.
    fn haste(&self) -> ToolHaste;

    /// Whether the core can forget this call: nothing left to say, ever.
    ///
    /// Asked after output has been collected, so the last thing a tool says is
    /// always taken. A tool that has stopped working but still owes a summary
    /// answers `false` here while reporting [`ToolHaste::Ended`].
    fn done(&self) -> bool;

    /// The call's one answer, taken the first time the core has anything to say
    /// about the call at all — because it is holding output, or because it
    /// ended.
    ///
    /// Required, and asked for exactly once. A tool that ends without producing
    /// anything says so in its own words here; nobody else can, and an empty
    /// success invented by the core reads as a command that ran quietly.
    fn first_output(&mut self) -> ToolOutput;

    /// Everything since, in whatever shape the tool judges best — a tail, a
    /// summary, a diff, an exit status. Called only at a request boundary, so
    /// a long-lived tool decides how to represent minutes of activity in one
    /// block.
    ///
    /// Returning `None` means "nothing new".
    fn more_output(&mut self) -> Option<ToolOutput>;

    /// Wind down: stop the work and finish soon.
    ///
    /// The tool still gets to produce its last words; the core keeps it around
    /// until [`ToolSession::done`].
    fn cancel(&mut self);
}

pub trait Tool: Send + Sync + 'static {
    fn spec(&self) -> ToolSpec;

    /// Start the work. Call [`SourceWaker::wake`] whenever the status
    /// changes; the core will come and ask.
    fn run(&self, call: ToolCall, waker: SourceWaker) -> Box<dyn ToolSession>;
}
