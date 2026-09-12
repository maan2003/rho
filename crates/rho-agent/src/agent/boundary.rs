//! Should the next request start now?
//!
//! The whole scheduler, kept apart from the agent it schedules because it is
//! the only part that decides anything:
//! `DECISION-boundary-is-the-only-decision`. What it decides about is here too
//! — the durations, what the model's last turn settled, the facts each source
//! reports, and when the scheduler first saw each of them. Nothing here
//! touches the store, the provider, or a task.
//!
//! The trade the numbers make is wall time against requests. Every wake is a
//! whole request against a mostly-cached context, so the loop should run as
//! slowly as it can while still doing something useful with each request:
//! wait for the work the model is watching, deliver what finished in one go,
//! and never wake for something the model cannot act on.

use std::collections::BTreeMap;
use std::time::Duration;

use rho_agent_tools::{CellFacts, JobFacts};
use rho_core::UnixMs;

use super::{Phase, Standing};
use crate::{WakeEvent, WakeFacts, WakeKind, WakeTrigger};

/// One source, whether or not it has anything to say.
///
/// Facts and nothing else: when something arrived, whether a job has ended
/// and how. Even "is this worth sending" is left to [`boundary`], so an empty
/// queue and a job that has produced nothing are both reported: being empty
/// is a fact too. `DECISION-boundary-is-the-only-decision`.
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
    /// One exec call's cell. `latest` marks the model's most recent exec,
    /// whose check-in sets the pace; older cells keep their own facts but
    /// nothing they set about pacing counts.
    Cell { facts: CellFacts, latest: bool },
    /// A command or host call a cell registered. It outlives its cell and is
    /// reported until its end has been delivered.
    Job { facts: JobFacts },
}

/// How long a person's message waits for the machines around it to settle.
/// Long enough that a job about to finish rides along with it, short enough
/// not to be felt.
pub(crate) const USER_PATIENCE: Duration = Duration::from_millis(500);
/// A peer usually sends several lines in a row; expect a beat more so they
/// collapse into one request instead of waking one apiece.
pub(crate) const MAIL_BURST: Duration = Duration::from_secs(1);
/// ...and how long a peer's mail waits for anything else, which also flushes a
/// peer that never stops.
pub(crate) const MAIL_PATIENCE: Duration = Duration::from_secs(2);
/// How long a `notify()` waits for the ones right behind it. The model asked
/// to be told, so this is only coalescing, not patience for other work.
pub(crate) const NOTIFY_PATIENCE: Duration = Duration::from_secs(1);
/// How long a foreground job that succeeded waits for its siblings, so a
/// round of parallel commands arrives as one request. Long, because the
/// model spawned them together and usually wants them back together.
pub(crate) const FOREGROUND_PATIENCE: Duration = Duration::from_secs(60);
/// How long a job that failed waits, foreground or background, while other
/// work runs. Shorter than a success: a failed test suite is something the
/// model can act on now, and the work still running is likelier to be moot.
pub(crate) const FAILURE_PATIENCE: Duration = Duration::from_secs(20);
/// How long the model is left alone with its cell when it did not say.
///
/// The one number the model can overrule, through `set_checkin`. Every other
/// number is a patience — how long something worth sending waits for
/// company. This is the opposite: it is the model asking to be woken, and it
/// is honoured whether or not anything arrived, because an empty request is
/// how the model finds out there is nothing to see and asks for longer next
/// time. It is the most any event waits: a background job with nothing to
/// hurry it is still delivered here.
pub(crate) const DEFAULT_WAIT: Duration = Duration::from_secs(120);

/// What the model's latest turn settled: when it spoke, and whether it made a
/// call it is waiting on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ModelTurn {
    /// The end of the model's latest turn, whatever it contained.
    pub spoke_at: UnixMs,
    pub asked: ModelAsked,
}

/// Whether the model's latest turn made a call. A turn with one is looked in
/// on at its cell's check-in; a turn of prose is not, and only what its
/// cells go on to do wakes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ModelAsked {
    Nothing,
    Calls,
}

/// Whether the next request starts now.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Boundary {
    /// Not yet. `recheck` is the earliest instant at which the answer could
    /// change by itself; `None` means only a new event can change it. Carrying
    /// it here means the loop cannot arm a timer that disagrees with the
    /// decision, because the decision handed it the timer.
    No {
        recheck: Option<UnixMs>,
    },
    /// Drain every source and send. `wake` is why, for the log.
    Now {
        wake: WakeFacts,
    },
    /// Throw away the in-flight request and send in its place. One action, not
    /// an abort followed by the ordinary question: somebody who interrupts has
    /// already said they are not waiting, so there is nothing left to weigh.
    AbortAndResend,
    RetryExhausted,
}

impl Boundary {
    #[cfg(test)]
    pub(crate) fn is_now(&self) -> bool {
        matches!(self, Boundary::Now { .. })
    }
}

/// When the scheduler first saw each pending event while it was in a position
/// to act on it.
///
/// A patience is a wait for company, and it has to be measured from a moment
/// the scheduler could have sent: an event that happened mid-request, or
/// while the agent was stopped, has not been waiting *for* anything. So the
/// clock for each event starts here, the first time [`boundary`] reads it
/// from an idle agent, and not at the instant the tool recorded. Cleared by
/// the drain, because a drain delivers everything.
#[derive(Debug, Default)]
pub(crate) struct Observations {
    seen: BTreeMap<(u64, u64, WakeKind), UnixMs>,
}

impl Observations {
    /// Everything was delivered; nothing is pending any more.
    pub(crate) fn clear(&mut self) {
        self.seen.clear();
    }
}

/// Should the next request start now?
///
/// A plain function of what every source reports, so it can be exercised
/// against a fabricated clock and fabricated sources. Every source is here,
/// including the ones with nothing to say, because "this one is not worth
/// sending" is a decision too and this is where decisions live.
///
/// The rules, in the order they are applied:
///
/// 1. A request in flight is only disturbed by an interrupt.
/// 2. A stopped agent waits for fresh input; a retry keeps its own clock.
/// 3. Three kinds of thing are *events* — reasons to send: a person or a peer
///    speaking, a job ending, and a cell's `notify()`. A cell that returns with
///    output on it ended like a job; one that returns silently has said
///    nothing. Plain output from a running job is never an event: it rides
///    along with whatever sends next, and arms no timer.
/// 4. Every event names how long it waits for company, and waits only while
///    there is company: with nothing running in the foreground, everything goes
///    at once. The foreground is the newest cell to have registered a job, and
///    everything that cell registered; older cells' jobs are background, and
///    are never a reason to slow the foreground down.
/// 5. The model's check-in is the most anything waits: an event that has no
///    deadline of its own is delivered there.
///
/// Every patience is measured from when this function first saw the event
/// while able to act ([`Observations`]), never from the last thing a source
/// did, so no source can extend a wait by continuing to talk.
pub(crate) fn boundary(
    sources: &[SourceKind],
    turn: Option<&ModelTurn>,
    phase: &Phase,
    observations: &mut Observations,
    now: UnixMs,
) -> Boundary {
    // Cases where only a fresh event can change the answer, so there is nothing
    // to wake up for.
    const NEVER: Boundary = Boundary::No { recheck: None };

    let fresh_input_at = sources
        .iter()
        .filter_map(|source| match source {
            SourceKind::User { oldest_at, .. } => *oldest_at,
            SourceKind::Mail { newest_at, .. } => *newest_at,
            _ => None,
        })
        .max();
    // Rules 1 and 2. Exhaustive rather than a run of early returns, because
    // with those the precedence lived in the order they were written.
    let standing = match phase {
        Phase::Requesting(_) => {
            let interrupt = sources.iter().any(|source| match source {
                SourceKind::User { interrupt, .. } => *interrupt,
                // However loud a peer or a job is, the model finishes what it
                // is saying.
                _ => false,
            });
            return match interrupt {
                true => Boundary::AbortAndResend,
                false => NEVER,
            };
        }
        // `DECISION-stopped-agents-wait-for-fresh-input`. Nothing is observed
        // here either: a cancelled cell's last words start no clock.
        Phase::Idle { standing, .. } if standing.stopped(fresh_input_at) => return NEVER,
        // What the next request owes is not itself a reason to make one, so
        // `owed` is never read here: only `standing` is.
        Phase::Idle { standing, .. } => standing,
    };

    // -- the room: what is running, and where the foreground is ---------------

    // Every cell reports the notebook's foreground; they agree, and an agent
    // with no cells has no foreground.
    let foreground_cell = sources
        .iter()
        .filter_map(|source| match source {
            SourceKind::Cell { facts, .. } => Some(facts.foreground_cell),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let foreground = |cell: u64| cell >= foreground_cell;
    // What is still running, split by whether the model is watching it. A
    // cell that has not returned is running as much as a job is: it may be
    // awaiting one, or sleeping in a monitor loop.
    let (mut foreground_running, mut background_running) = (0u64, 0u64);
    for source in sources {
        let running_in = match source {
            SourceKind::Cell { facts, .. } if facts.returned.is_none() => Some(facts.cell),
            SourceKind::Job { facts } if facts.finished.is_none() => Some(facts.cell),
            _ => None,
        };
        match running_in {
            Some(cell) if foreground(cell) => foreground_running += 1,
            Some(_) => background_running += 1,
            None => {}
        }
    }
    // Company is what a patience waits for: foreground work that will end and
    // wake the loop by itself. Background work is never company — waiting on
    // it is exactly what the model chose not to do when it moved on.
    let company = foreground_running > 0;
    // A peer may be mid-burst until this instant: the only guess in here, and
    // the only expectation that has to be waited out on a clock, because a
    // quiet peer and a finished peer look exactly alike.
    let mail_burst_until = sources
        .iter()
        .find_map(|source| match source {
            SourceKind::Mail { newest_at, .. } => newest_at.map(|at| at + MAIL_BURST),
            _ => None,
        })
        .filter(|until| *until > now);
    // The latest cell's check-in: the one thing the model says about pacing,
    // and it says it for one turn. Older cells' settings are theirs alone.
    let checkin = sources.iter().find_map(|source| match source {
        SourceKind::Cell {
            latest: true,
            facts,
        } => facts.checkin,
        _ => None,
    });
    let tools_suppressed = checkin.is_some_and(|checkin| !checkin.wake_on_tools);
    // Rule 5. A turn of prose asked for nothing, and gets a quiet agent
    // rather than one that keeps offering it the same silence.
    let checkin_at = turn.and_then(|turn| match turn.asked {
        ModelAsked::Nothing => None,
        ModelAsked::Calls => {
            Some(turn.spoke_at + checkin.map_or(DEFAULT_WAIT, |checkin| checkin.after))
        }
    });

    // -- the events, and what each is willing to wait ------------------------

    // Rule 3: what has happened that the model could act on. A pending event
    // is pending until a drain takes it, so there is no bookkeeping about
    // what was delivered: a delivered job is gone from its session, and a
    // cell's output is taken with it.
    let mut pending = Vec::new();
    for source in sources {
        match source {
            SourceKind::Cell { facts, .. } => {
                if let Some(at) = facts.notified_at {
                    pending.push((facts.cell, u64::MAX, WakeKind::Notify, at));
                }
                // Returning is not itself news; returning with something on
                // the cell is, and a raise is output like any other.
                if let (Some(returned), Some(since)) = (facts.returned, facts.output_since) {
                    let kind = if facts.failed {
                        WakeKind::Failed
                    } else {
                        WakeKind::Succeeded
                    };
                    pending.push((facts.cell, u64::MAX, kind, returned.max(since)));
                }
            }
            SourceKind::Job { facts } => {
                if let Some(end) = facts.finished {
                    let kind = if end.failed {
                        WakeKind::Failed
                    } else {
                        WakeKind::Succeeded
                    };
                    pending.push((facts.cell, facts.registered_at.0, kind, end.at));
                }
            }
            SourceKind::User { .. } | SourceKind::Mail { .. } => {}
        }
    }
    // Start each event's clock the first time it is seen from here, and stop
    // keeping clocks for events that are gone.
    observations
        .seen
        .retain(|key, _| pending.iter().any(|(c, s, k, _)| (*c, *s, *k) == *key));
    let events = pending
        .into_iter()
        .map(|(cell, source, kind, occurred_at)| {
            let seen_at = *observations.seen.entry((cell, source, kind)).or_insert(now);
            let foreground = foreground(cell);
            // Rule 4: how long this waits for company, if there is any.
            // `None` is a wait with no clock of its own: it ends when the
            // foreground does, or at the check-in.
            let patience = match (kind, company) {
                (_, false) => Some(Duration::ZERO),
                // The model asked to hear this; it only waits for the ones
                // right behind it.
                (WakeKind::Notify, true) => Some(NOTIFY_PATIENCE),
                (WakeKind::Failed, true) => Some(FAILURE_PATIENCE),
                (WakeKind::Succeeded, true) if foreground => Some(FOREGROUND_PATIENCE),
                // Background success: nobody is waiting for it, so it rides
                // along with the next wake, whatever causes that.
                (WakeKind::Succeeded, true) => None,
            };
            WakeEvent {
                cell,
                source,
                kind,
                foreground,
                occurred_at,
                seen_at,
                deadline: patience.map(|patience| seen_at + patience),
            }
        })
        .collect::<Vec<_>>();

    let wake = |trigger| WakeFacts {
        trigger,
        events: events.clone(),
        foreground_running,
        background_running,
        tools_suppressed,
        checkin_at,
    };
    // Rule 2's other half: a request somebody asked for outright, and a
    // retry on its own clock. Read after the room so the record is complete.
    match standing {
        Standing::Asked => {
            return Boundary::Now {
                wake: wake(WakeTrigger::Asked),
            };
        }
        Standing::Retry {
            since,
            failed_at,
            attempts,
            ..
        } => {
            if fresh_input_at.is_some_and(|at| at >= *failed_at) {
                return Boundary::Now {
                    wake: wake(WakeTrigger::Retry),
                };
            }
            let expires = *since + Duration::from_secs(8 * 60 * 60);
            if now >= expires {
                return Boundary::RetryExhausted;
            }
            // Keep the provider's Fibonacci progression and 30-minute cap,
            // but own the clock here so every retry uses a fresh source drain.
            let (mut previous, mut current) = (1_u64, 1_u64);
            for _ in 2..*attempts {
                (previous, current) = (current, (previous + current).min(30 * 60));
            }
            let delay = Duration::from_secs(current);
            let deadline = (*failed_at + delay).min(expires);
            return if now >= deadline {
                Boundary::Now {
                    wake: wake(WakeTrigger::Retry),
                }
            } else {
                Boundary::No {
                    recheck: Some(deadline),
                }
            };
        }
        Standing::Nothing | Standing::Cancelled { .. } | Standing::Failed { .. } => {}
    }

    // -- the earliest moment anybody named --------------------------------------

    // People and peers are measured from arrival rather than from being seen:
    // their patience is short, and it is company they wait for, not the
    // scheduler's attention.
    let mut candidates: Vec<(UnixMs, WakeTrigger)> = Vec::new();
    for source in sources {
        match source {
            SourceKind::User {
                oldest_at: Some(at),
                ..
            } => candidates.push((
                *at + if company {
                    USER_PATIENCE
                } else {
                    Duration::ZERO
                },
                WakeTrigger::User,
            )),
            SourceKind::Mail {
                oldest_at: Some(at),
                ..
            } => candidates.push((
                *at + if company || mail_burst_until.is_some() {
                    MAIL_PATIENCE
                } else {
                    Duration::ZERO
                },
                WakeTrigger::Mail,
            )),
            _ => {}
        }
    }
    // `wake_on_tools=False`: the model said nothing from the notebook should
    // wake it. The events are still recorded, and still delivered with
    // whatever does.
    if !tools_suppressed {
        candidates.extend(events.iter().filter_map(|event| {
            let trigger = match event.kind {
                WakeKind::Notify => WakeTrigger::Notify,
                WakeKind::Succeeded | WakeKind::Failed => WakeTrigger::Finished,
            };
            event.deadline.map(|deadline| (deadline, trigger))
        }));
    }
    candidates.extend(checkin_at.map(|at| (at, WakeTrigger::Checkin)));

    // Nothing worth a request, whoever is still busy.
    let Some((deadline, trigger)) = candidates.into_iter().min_by_key(|(at, _)| *at) else {
        return NEVER;
    };
    if now >= deadline {
        return Boundary::Now {
            wake: wake(trigger),
        };
    }
    Boundary::No {
        // A peer going quiet is not an event, so the one expectation that
        // lapses on a clock has to be waited out: at `until` mail's patience
        // collapses, and the answer can change with nothing happening.
        // Everything else announces itself.
        recheck: Some(match mail_burst_until {
            Some(until) if !company => deadline.min(until),
            _ => deadline,
        }),
    }
}
