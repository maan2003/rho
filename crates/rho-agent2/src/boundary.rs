//! Should the next request start now?
//!
//! The whole scheduler, kept apart from the agent it schedules because it is
//! the only part that decides anything. What it decides about is here too: the
//! durations, and the two things the model's last turn settled. Nothing in
//! here touches the store, the provider, or a task.
//!
//! There is deliberately no stored status enum, and no foreground/background
//! tool distinction to declare — at the moment of the call `npm test` and
//! `npm run dev` are the same call, and nothing could tell them apart. So the
//! core does not try. A running tool has no urgency of its own; the pace at
//! which its output reaches the model is the model's own, stated by what its
//! last turn did and revised every turn. What a tool *can* say is that it
//! ended, or that what it holds stands alone, and those are worth waking
//! somebody for whatever else is going on.

use std::collections::BTreeSet;
use std::time::Duration;

use rho_core::{ToolCallId, UnixMs};

use crate::source::SourceKind;
use crate::tool::{Told, ToolActivity, Unsent};

/// How long a person's message waits for the machines around it to settle. Long
/// enough that a tool about to finish rides along with it, short enough not to
/// be felt.
pub(crate) const USER_PATIENCE: Duration = Duration::from_millis(500);
/// A peer usually sends several lines in a row; expect a beat more so they
/// collapse into one request instead of waking one apiece.
pub(crate) const MAIL_BURST: Duration = Duration::from_secs(1);
/// ...and how long a peer's mail waits for anything else, which also flushes a
/// peer that never stops.
pub(crate) const MAIL_PATIENCE: Duration = Duration::from_secs(2);
/// How long something a tool has finished with waits for a call that is still
/// working, so that a round of parallel calls arrives as one request rather
/// than one apiece.
pub(crate) const TOOL_PATIENCE: Duration = Duration::from_secs(10);
/// How long output a tool is still in the middle of sits unsent, once nobody is
/// waiting for it. Not a patience: it never shortens anybody else's wait, and
/// nothing about it is urgent. A build log is worth more whole than in pieces,
/// and a log nobody asked for is worth very little — but neither is worth
/// leaving unsent forever.
pub(crate) const PROGRESS_PATIENCE: Duration = Duration::from_secs(60);
/// How long the model is left alone with its calls when it did not say.
///
/// The one number here the model can overrule: it is what
/// [`ModelAsked::Calls`] is worth, and `wait` exists so the model can name a
/// longer one instead.
///
/// Every other number is a patience — how long something already worth sending
/// waits for company. This is the opposite and the only one of its kind: it is
/// the model asking to be woken, and it is honoured whether or not anything
/// arrived, because an empty request is how the model finds out there is
/// nothing to see and asks for longer next time.
pub(crate) const DEFAULT_WAIT: Duration = Duration::from_secs(10);

/// What the model's latest turn settled: whether it wants to be looked in on,
/// and which calls it is waiting on.
///
/// Two things rather than one, because a turn can move one without the other.
/// Replying in prose while a build runs buys everyone another interval without
/// changing what the model is blocked on — and if one field did both, a person
/// typing during a five minute test run would end up demoting it, which is the
/// shape of bug this whole design keeps walking into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelTurn {
    /// The end of the model's latest turn, whatever it contained.
    pub spoke_at: UnixMs,
    pub asked: ModelAsked,
    /// The calls the latest turn that issued any asked for. Those are the ones
    /// the model is waiting on; anything it called before that it has moved
    /// past, having asked for something else since.
    ///
    /// Named calls rather than a cutoff instant, because a call made at the
    /// same millisecond as the turn that superseded it is a coincidence no
    /// clock can rule out, and the model has already said which ones it means.
    pub waiting_on: BTreeSet<ToolCallId>,
}

/// What the model's latest turn asked for, which is what says whether to look
/// again, and when.
///
/// A look-in lasts exactly one turn. The model is shown what its calls have
/// once, and to be shown again it has to ask, by calling something or by naming
/// an interval. Otherwise an agent that answered "the build is going" would be
/// asked for another opinion every ten seconds for the length of the build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModelAsked {
    /// Nothing at all: prose, and then quiet. Whatever is still running will
    /// speak for itself when it ends, and until then the agent has no reason to
    /// wake up.
    Nothing,
    /// Calls, with nothing said about when to look at them.
    Calls,
    /// An interval the model named for itself. Honoured with nothing running,
    /// because a model with nothing to do asking to be woken later is the whole
    /// point of it.
    #[allow(dead_code, reason = "the tool that names an interval is not built yet")]
    Wait(Duration),
}

/// A standing instruction that overrides the ordinary rhythm rules. The two
/// overrides are opposites, so they cannot both be in force.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Standing {
    /// Follow the rhythms.
    #[default]
    Normal,
    /// Send at the next opportunity even with nothing pending: a retry, a
    /// restart resume, or carrying on after a compaction.
    MustSend,
    /// Cancelled. Tool output still reaches history at the next boundary, but
    /// nothing may *start* a request until fresh user input arrives —
    /// otherwise a cancelled tool's dying words would wake the agent straight
    /// back up.
    Halted,
}

/// Whether the next request starts now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Boundary {
    /// Not yet. `recheck` is the earliest instant at which the answer could
    /// change by itself; `None` means only a new event can change it. Carrying
    /// it here means the loop cannot arm a timer that disagrees with the
    /// decision, because the decision handed it the timer.
    No { recheck: Option<UnixMs> },
    /// Drain every source and send.
    Now,
    /// Throw away the in-flight request and send in its place. One action, not
    /// an abort followed by the ordinary question: somebody who interrupts has
    /// already said they are not waiting, so there is nothing left to weigh.
    AbortAndResend,
}

/// Should the next request start now?
///
/// A plain function of what every source reports, so it can be exercised
/// against a fabricated clock and fabricated sources. Every rule and every
/// duration lives here rather than on the sources, because a number chosen by
/// one source in isolation is a number chosen without seeing what else is
/// waiting.
///
/// Every source is here, including the ones with nothing to say, because
/// "this one is not worth sending" is a decision too and this is where
/// decisions live.
///
/// Everything holding something names the moment it stops waiting for company.
/// Three rules, and nothing else in this crate decides anything:
///
/// 1. An `Interrupt` message is worth throwing away an in-flight request.
/// 2. The model is looked in on when it asked to be, whether or not anything
///    arrived.
/// 3. Otherwise go at the earliest moment anybody named, and everyone yields
///    into that same request.
///
/// `Due`, defined inside, is what makes rule 3 more than a list of timeouts.
/// Most of those moments are a *patience* — a wait for company — and a patience
/// with no company coming is no wait at all, so it collapses to the moment it
/// was counting from. That is why a finished `rg` answers instantly in a quiet
/// agent and waits ten seconds beside a build. The exceptions are the two
/// moments that are not patience at all but a promise not to sit on something
/// forever: the model's own interval, and output a tool is still in the middle
/// of. An empty room does not make half a build log any more worth reading, so
/// those are never collapsed.
///
/// Note what tools do *not* get: a running tool has no patience of its own
/// worth speaking of, because there is no way to tell `npm test` from
/// `npm run dev` by looking at either one. The pace at which a tool's output
/// reaches the model is set by the model, in rule 2, and revised every turn.
/// Only the two things a tool can say that are true regardless — it ended, or
/// what it holds stands on its own — buy it any urgency.
///
/// Every wait is measured from a moment that has already happened, never from
/// the last thing a source did, so no source can extend a wait by continuing to
/// talk — which is what stops one chatty tool from pinning everybody else.
pub(crate) fn boundary(
    sources: &[SourceKind],
    turn: Option<&ModelTurn>,
    inference_active: bool,
    standing: Standing,
    now: UnixMs,
) -> Boundary {
    // Cases where only a fresh event can change the answer, so there is nothing
    // to wake up for.
    const NEVER: Boundary = Boundary::No { recheck: None };

    if standing == Standing::Halted {
        return NEVER;
    }
    if inference_active {
        let interrupt = sources.iter().any(|source| match source {
            SourceKind::User { interrupt, .. } => *interrupt,
            // However loud a peer or a tool is, the model finishes what it is
            // saying.
            SourceKind::Mail { .. } | SourceKind::Tool { .. } => false,
        });
        if interrupt {
            return Boundary::AbortAndResend;
        } else {
            return NEVER;
        }
    }
    if standing == Standing::MustSend {
        return Boundary::Now;
    }
    /// Whether anything more is coming.
    ///
    /// Lives in here because nothing outside this function has any business
    /// knowing it: it is not a fact about any one source, it is a reading of
    /// the whole list, and it exists only to say whether a patience is worth
    /// having. Without it every finished tool call would cost `TOOL_PATIENCE`
    /// and every typed message `USER_PATIENCE`, however quiet the rest of the
    /// agent.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Due {
        Nothing,
        /// A peer may be mid-burst until this instant. The only guess in here,
        /// and the only expectation that has to be waited out on a clock,
        /// because a quiet peer and a finished peer look exactly alike.
        Until(UnixMs),
        /// A call the model is waiting on. It wakes the core when it ends, so
        /// there is nothing to guess and no timer to arm.
        UntilItEnds,
    }

    // It has to be settled before any moment below can be chosen, because half
    // of them depend on the answer.
    let due = sources.iter().fold(Due::Nothing, |due, source| {
        let source_due = match source {
            // Typed input is whole on arrival, so nothing more is ever due
            // from it.
            SourceKind::User { .. } => Due::Nothing,
            SourceKind::Mail { newest_at, .. } => match newest_at.map(|at| at + MAIL_BURST) {
                Some(until) if until > now => Due::Until(until),
                _ => Due::Nothing,
            },
            SourceKind::Tool {
                id,
                told,
                activity,
                unsent,
            } => match (told, activity, unsent) {
                // Everything there was to say has been said.
                (Told::Exit, ..) => Due::Nothing,
                // It says what it holds stands on its own, which is it saying
                // not to wait for the rest of the call.
                (_, ToolActivity::Running, Unsent::Settled { .. }) => Due::Nothing,
                // A call the model went on to ask for something else after is
                // one it has stopped waiting on. If it still counted, a dev
                // server that never finishes would be a wait nobody could end.
                (_, ToolActivity::Running, _)
                    if turn.is_some_and(|turn| turn.waiting_on.contains(id)) =>
                {
                    Due::UntilItEnds
                }
                _ => Due::Nothing,
            },
        };
        match (due, source_due) {
            (Due::UntilItEnds, _) | (_, Due::UntilItEnds) => Due::UntilItEnds,
            (Due::Until(one), Due::Until(other)) => Due::Until(one.max(other)),
            (Due::Nothing, other) | (other, Due::Nothing) => other,
        }
    });

    // How long something already worth sending waits for the rest of the round
    // to arrive — nothing at all, when the rest of the round is not coming.
    let patience = |duration| match due {
        Due::Nothing => Duration::ZERO,
        _ => duration,
    };
    // Rule 2. Nothing here asks what is running: the model said whether to look
    // again, and a model that asked for nothing gets a quiet agent rather than
    // one that keeps offering it the same silence.
    let checkin = turn.and_then(|turn| match turn.asked {
        ModelAsked::Nothing => None,
        ModelAsked::Calls => Some(turn.spoke_at + DEFAULT_WAIT),
        ModelAsked::Wait(asked) => Some(turn.spoke_at + asked),
    });

    let deadline = sources
        .iter()
        .filter_map(|source| match *source {
            // An empty queue names no moment, which is what makes it nothing to
            // send rather than a special case.
            SourceKind::User { oldest_at, .. } => oldest_at.map(|at| at + patience(USER_PATIENCE)),
            SourceKind::Mail { oldest_at, .. } => oldest_at.map(|at| at + patience(MAIL_PATIENCE)),
            SourceKind::Tool {
                told: Told::Exit, ..
            } => None,
            // An ending and a flag are the same kind of news and wait the same.
            // Both date from the moment they happened, so a tool that ends
            // after an hour of output gets its siblings' full attention rather
            // than looking an hour overdue.
            SourceKind::Tool {
                activity, unsent, ..
            } => match (activity, unsent) {
                (ToolActivity::Exited { at }, Unsent::Settled { since }) => {
                    Some(at.min(since) + patience(TOOL_PATIENCE))
                }
                (ToolActivity::Exited { at }, _) => Some(at + patience(TOOL_PATIENCE)),
                (ToolActivity::Running, Unsent::Settled { since }) => {
                    Some(since + patience(TOOL_PATIENCE))
                }
                // Mid-thought, so nobody asked for it and it is nobody's reason
                // to make a request — it is only not worth leaving unsent
                // forever, which is why an empty room does not hurry it along.
                // While the model is being looked in on it never gets this far,
                // because the check-in is sooner; this is what half a build log
                // is worth to an agent that has moved on, or asked for a long
                // quiet.
                (ToolActivity::Running, Unsent::Waiting { since }) => {
                    Some(since + PROGRESS_PATIENCE)
                }
                (ToolActivity::Running, Unsent::Nothing) => None,
            },
        })
        .chain(checkin)
        .min();

    // Nothing worth a request, whoever is still busy.
    let Some(deadline) = deadline else {
        return NEVER;
    };
    if now >= deadline {
        return Boundary::Now;
    }
    Boundary::No {
        recheck: Some(match due {
            // A peer going quiet is not an event, so the one expectation that
            // lapses on a clock has to be waited out: at `until` the patiences
            // above collapse, and the answer can change with nothing happening.
            Due::Until(until) => deadline.min(until),
            // Everything else announces itself.
            Due::Nothing | Due::UntilItEnds => deadline,
        }),
    }
}
