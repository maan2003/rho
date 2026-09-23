//! What the Python notebook reports about itself, and the shape of a tool.
//!
//! A cell is not a future that resolves once. It is a source that produces
//! output over its lifetime, possibly for hours, and the commands it starts
//! outlive it. Nothing here decides anything: a cell and a job report facts,
//! and what any fact is worth is `boundary`'s to say
//! (`DECISION-boundary-is-the-only-decision`).
//!
//! Because the core pulls, a cell holds its own output until asked, and
//! answers at that moment in whatever shape it judges best.
//! `DECISION-pull-based-sources`.

use std::sync::Arc;

use rho_core::UnixMs;
use tokio::sync::Notify;

/// What a cell's wait controls asked for: how long the model is left alone,
/// and whether the notebook may wake it sooner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PythonCheckin {
    pub after: std::time::Duration,
    pub wake_on_tools: bool,
}

impl PythonCheckin {
    pub const DEFAULT_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(120);
}

impl Default for PythonCheckin {
    fn default() -> Self {
        Self {
            after: Self::DEFAULT_MAX_WAIT,
            wake_on_tools: true,
        }
    }
}

/// One cell, as the scheduler reads it. Every field is an observation the
/// notebook made; none is a verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellFacts {
    /// The cell's number in the notebook: later cells have larger numbers.
    pub cell: u64,
    /// The interpreter has begun the cell.
    pub started: bool,
    /// The cell's top-level code returned (or raised) at this instant. Its
    /// jobs may still be running.
    pub returned: Option<UnixMs>,
    /// The cell raised, or the runtime stopped underneath it: what it holds
    /// ends in an error the model has not seen.
    pub failed: bool,
    /// The oldest unsent print or text output, if any.
    pub output_since: Option<UnixMs>,
    /// The oldest unsent explicit `notify()`, if any. Only the model's own
    /// call sets this; generated error text never does.
    pub notified_at: Option<UnixMs>,
    pub checkin: Option<PythonCheckin>,
    /// Notebook-wide: the newest cell that registered any job, or 0 before
    /// any did. Cells from it onward are the foreground; older cells are
    /// background work the model has moved on from. A cell that registers
    /// nothing (a bare check-in) does not advance it.
    pub foreground_cell: u64,
}

/// One job a cell registered: a managed command, or a host operation such
/// as a web search or an advisor consultation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobFacts {
    /// The cell that registered it.
    pub cell: u64,
    pub registered_at: UnixMs,
    /// The oldest unsent output, if any.
    pub output_since: Option<UnixMs>,
    pub finished: Option<JobEnd>,
}

/// How a job ended, as the notebook saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobEnd {
    pub at: UnixMs,
    /// A non-zero or missing exit code, a spawn failure, a cancellation, or
    /// a host operation that returned an error, whether or not the cell went
    /// on to catch it.
    pub failed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceFacts {
    Cell(CellFacts),
    Job(JobFacts),
}

/// Tell the core that something changed.
///
/// Deliberately carries no payload: what changed is discovered by asking, at a
/// moment the core picks. That is what lets several sources collapse into one
/// request instead of each waking one.
#[derive(Clone, Debug)]
pub struct SourceWaker(Arc<Notify>);

impl SourceWaker {
    /// The core makes these for its tools; a test or an adapter may make its
    /// own over any `Notify` it wants to watch.
    pub fn new(notify: Arc<Notify>) -> Self {
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
