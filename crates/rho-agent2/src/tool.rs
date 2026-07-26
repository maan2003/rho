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

/// What a tool has to say about itself, and since when.
///
/// One enum rather than one for working-or-not and another for what is unsent,
/// because no question the core asks is answered by half of it — and because
/// two would spell pairs that cannot happen: output is not still mid-thought
/// once whatever was writing it has stopped.
///
/// Facts only. Which of these is worth a request, and how long any of them
/// waits, is `boundary`'s to say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolReport {
    /// Working, and the model has seen everything said so far.
    Running,
    /// Working, and holding output mid-thought. Worth sending eventually — half
    /// a build log beats none — but worth less than the whole, so it is the
    /// most patient thing the core knows about.
    Waiting { since: UnixMs },
    /// Working, and holding output that stands on its own: this much matters
    /// now, whatever else the tool goes on to say. The one thing a running tool
    /// can say about its own urgency, and it buys exactly one thing: it stops
    /// waiting for the rest of the call. It still cannot interrupt a request in
    /// flight.
    ///
    /// `since` is when it became worth sending, not when it started arriving.
    Settled { since: UnixMs },
    /// Finished at `at`; nothing more will arrive. Ending is the one thing a
    /// tool cannot take back, which is why the core is willing to act on it:
    /// everything else here is a guess about what happens next.
    ///
    /// Whether it is holding anything is not said, because an ending makes
    /// whatever it has final and the core is going to ask for it either way.
    Exited { at: UnixMs },
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
    /// What it has to say about itself right now.
    ///
    /// The only thing that says whether there is anything to collect: a tool
    /// reporting [`ToolReport::Running`] is not asked for output, however much
    /// it is holding.
    fn status(&self) -> ToolReport;

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
    /// until it reports [`ToolReport::Exited`] and its output has been
    /// collected.
    fn cancel(&mut self);
}

pub trait Tool: Send + Sync + 'static {
    fn spec(&self) -> ToolSpec;

    /// Start the work. Call [`SourceWaker::wake`] whenever the status
    /// changes; the core will come and ask.
    fn run(&self, call: ToolCall, waker: SourceWaker) -> Box<dyn ToolSession>;
}
