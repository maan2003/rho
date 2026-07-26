//! Should the next request start now?
//!
//! The whole scheduler, kept apart from the agent it schedules because it is
//! the only part that decides anything:
//! `DECISION-boundary-is-the-only-decision`. What it decides about is here too
//! — the durations, what the model's last turn settled, and the facts each
//! source reports. Nothing here touches the store, the provider, or a task.
//!
//! Tools get no urgency of their own and no status enum to declare:
//! `DECISION-model-sets-the-pace`.

use std::time::Duration;

use rho_core::UnixMs;

use crate::tool::ToolReport;
use crate::{Phase, Standing, Told};

/// One source, whether or not it has anything to say.
///
/// Facts and nothing else — when something arrived, whether a call has been
/// answered. Even "is this worth sending" is left to `boundary`, so an empty
/// queue and a tool that has produced nothing are both reported: being empty is
/// a fact too, and for a tool it is one that changes the answer.
/// `DECISION-boundary-is-the-only-decision`.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// deliberately no tidier enum in between: any name that summarised these
    /// facts would be deciding what they mean, outside the one place decisions
    /// live.
    Tool { told: Told, report: ToolReport },
}

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

/// What the model's latest turn settled: when it spoke, and whether it wants to
/// be looked in on.
///
/// Which calls it is waiting on is not here, and is not tracked anywhere: a
/// call that has never spoken is one the model is still owed an answer from,
/// and that is as much as the decision needs. `ModelAsked` sets the pace once
/// the answering starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ModelTurn {
    /// The end of the model's latest turn, whatever it contained.
    pub spoke_at: UnixMs,
    pub asked: ModelAsked,
}

/// What the model's latest turn asked for, which is what says whether to look
/// again, and when. A look-in lasts exactly one turn:
/// `DECISION-model-sets-the-pace`.
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
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the tool that names an interval is not built yet")
    )]
    Wait(Duration),
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
/// against a fabricated clock and fabricated sources. Every source is here,
/// including the ones with nothing to say, because "this one is not worth
/// sending" is a decision too and this is where decisions live.
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
/// Note what tools do *not* get: no patience of their own. Rule 2 sets the pace
/// at which a tool's output reaches the model, and only the two things a tool
/// can say that are true regardless — it ended, or what it holds stands on its
/// own — buy it any urgency. `DECISION-model-sets-the-pace`.
///
/// Every wait is measured from a moment that has already happened, never from
/// the last thing a source did, so no source can extend a wait by continuing to
/// talk — which is what stops one chatty tool from pinning everybody else.
pub(crate) fn boundary(
    sources: &[SourceKind],
    turn: Option<&ModelTurn>,
    phase: &Phase,
    now: UnixMs,
) -> Boundary {
    // Cases where only a fresh event can change the answer, so there is nothing
    // to wake up for.
    const NEVER: Boundary = Boundary::No { recheck: None };

    // Only an idle agent hands the question to its sources; every other phase
    // answers on its own. Exhaustive rather than a run of early returns, because
    // with those the precedence lived in the order they were written.
    let user_oldest_at = sources.iter().find_map(|source| match source {
        SourceKind::User { oldest_at, .. } => *oldest_at,
        SourceKind::Mail { .. } | SourceKind::Tool { .. } => None,
    });
    match phase {
        Phase::Requesting(_) => {
            let interrupt = sources.iter().any(|source| match source {
                SourceKind::User { interrupt, .. } => *interrupt,
                // However loud a peer or a tool is, the model finishes what it
                // is saying.
                SourceKind::Mail { .. } | SourceKind::Tool { .. } => false,
            });
            return match interrupt {
                true => Boundary::AbortAndResend,
                false => NEVER,
            };
        }
        // `DECISION-stopped-agents-wait-for-a-person`.
        Phase::Idle { standing, .. } if standing.stopped(user_oldest_at) => return NEVER,
        // What the next request owes is not itself a reason to make one, so
        // `owed` is never read here: only `standing` is.
        Phase::Idle {
            standing: Standing::Asked,
            ..
        } => return Boundary::Now,
        Phase::Idle { .. } => {}
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
            SourceKind::Tool { told, report } => match (told, report) {
                // An ended call has nothing left to wait for, nor has one that
                // says what it holds stands on its own — which is it saying not
                // to wait for the rest of the call. Nor has one that has
                // already answered: a dev server that never finishes would
                // otherwise be a wait nobody could end.
                (_, ToolReport::Exited { .. } | ToolReport::Settled { .. }) | (Told::Result, _) => {
                    Due::Nothing
                }
                (Told::Nothing, ToolReport::Running | ToolReport::Waiting { .. }) => {
                    Due::UntilItEnds
                }
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
            // An ending and a flag are the same kind of news and wait the same.
            // Both date from the moment they happened, so a tool that ends
            // after an hour of output gets its siblings' full attention rather
            // than looking an hour overdue.
            SourceKind::Tool { report, .. } => match report {
                ToolReport::Running => None,
                // Mid-thought, so nobody asked for it and it is nobody's reason
                // to make a request — it is only not worth leaving unsent
                // forever, which is why an empty room does not hurry it along.
                // While the model is being looked in on it never gets this far,
                // because the check-in is sooner; this is what half a build log
                // is worth to an agent that has moved on, or asked for a long
                // quiet.
                ToolReport::Waiting { since } => Some(since + PROGRESS_PATIENCE),
                ToolReport::Settled { since } => Some(since + patience(TOOL_PATIENCE)),
                ToolReport::Exited { at } => Some(at + patience(TOOL_PATIENCE)),
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
