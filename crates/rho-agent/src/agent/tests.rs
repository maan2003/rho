use std::time::Duration;

use rho_agent_tools::{CellFacts, JobEnd, JobFacts, PythonCheckin};
use rho_core::ToolType;

use super::boundary::{
    DEFAULT_WAIT, FAILURE_PATIENCE, FOREGROUND_PATIENCE, MAIL_BURST, MAIL_PATIENCE,
    NOTIFY_PATIENCE, Observations, USER_PATIENCE,
};
use super::*;
use crate::{WakeKind, WakeTrigger};

fn call(id: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from(id).unwrap(),
        name: ToolName::try_from("exec").unwrap(),
        tool_type: ToolType::Function,
        arguments: "pass".to_owned(),
    }
}

/// The decision as an idle, uninterrupted agent asks it: the model made the
/// call it is waiting on at 0, and said nothing about when to look at it.
/// Each test varies only what it is about. The observations are the agent's:
/// they persist across the questions one test asks, as they do in the loop.
struct Ask {
    sources: Vec<SourceKind>,
    turn: Option<ModelTurn>,
    phase: Phase,
    observations: Observations,
}

fn ask(sources: Vec<SourceKind>) -> Ask {
    Ask {
        sources,
        turn: Some(ModelTurn {
            spoke_at: UnixMs(0),
            asked: ModelAsked::Calls,
        }),
        phase: Phase::Idle {
            owed: Vec::new(),
            standing: Standing::Nothing,
        },
        observations: Observations::default(),
    }
}

impl Ask {
    fn boundary(&mut self, now: UnixMs) -> Boundary {
        boundary(
            &self.sources,
            self.turn.as_ref(),
            &self.phase,
            &mut self.observations,
            now,
        )
    }

    /// When the decision says to come back, if it can change by itself.
    fn recheck(&mut self, now: UnixMs) -> Option<UnixMs> {
        match self.boundary(now) {
            Boundary::No { recheck } => recheck,
            other => panic!("expected to hold at {now:?}, got {other:?}"),
        }
    }

    /// The wake a `Now` carries.
    fn wake(&mut self, now: UnixMs) -> crate::WakeFacts {
        match self.boundary(now) {
            Boundary::Now { wake } => wake,
            other => panic!("expected to send at {now:?}, got {other:?}"),
        }
    }

    fn holds(&mut self, now: UnixMs) -> bool {
        matches!(self.boundary(now), Boundary::No { .. })
    }

    /// The model replied in prose and asked for nothing.
    fn replied(mut self, at: u64) -> Self {
        self.turn = Some(ModelTurn {
            spoke_at: UnixMs(at),
            asked: ModelAsked::Nothing,
        });
        self
    }

    /// Everything went out: what the loop does to its clocks at a drain.
    fn drained(&mut self) {
        self.observations.clear();
    }
}

// -- sources -----------------------------------------------------------------
//
// The notebook's foreground is the newest cell to have registered a job. The
// tables below use cell 1 for an older cell and cell 2 for the newest, and
// `FG` is which of them the foreground is.

fn cell_facts(cell: u64, foreground_cell: u64) -> CellFacts {
    CellFacts {
        cell,
        started: true,
        returned: None,
        failed: false,
        output_since: None,
        notified_at: None,
        checkin: None,
        foreground_cell,
    }
}

/// The model's current cell, still executing Python.
fn running_cell(cell: u64, fg: u64) -> SourceKind {
    SourceKind::Cell {
        facts: cell_facts(cell, fg),
        latest: true,
    }
}

/// A cell that returned silently at `at`.
fn returned_cell(cell: u64, fg: u64, at: u64) -> SourceKind {
    SourceKind::Cell {
        facts: CellFacts {
            returned: Some(UnixMs(at)),
            ..cell_facts(cell, fg)
        },
        latest: true,
    }
}

fn with_output(source: SourceKind, since: u64) -> SourceKind {
    match source {
        SourceKind::Cell { facts, latest } => SourceKind::Cell {
            facts: CellFacts {
                output_since: Some(UnixMs(since)),
                ..facts
            },
            latest,
        },
        SourceKind::Job { facts } => SourceKind::Job {
            facts: JobFacts {
                output_since: Some(UnixMs(since)),
                ..facts
            },
        },
        other => other,
    }
}

fn notified(source: SourceKind, at: u64) -> SourceKind {
    match source {
        SourceKind::Cell { facts, latest } => SourceKind::Cell {
            facts: CellFacts {
                notified_at: Some(UnixMs(at)),
                ..facts
            },
            latest,
        },
        _ => panic!("only a cell notifies"),
    }
}

fn failed(source: SourceKind) -> SourceKind {
    match source {
        SourceKind::Cell { facts, latest } => SourceKind::Cell {
            facts: CellFacts {
                failed: true,
                ..facts
            },
            latest,
        },
        SourceKind::Job { facts } => SourceKind::Job {
            facts: JobFacts {
                finished: facts.finished.map(|end| JobEnd {
                    failed: true,
                    ..end
                }),
                ..facts
            },
        },
        other => other,
    }
}

fn older(source: SourceKind) -> SourceKind {
    match source {
        SourceKind::Cell { facts, .. } => SourceKind::Cell {
            facts,
            latest: false,
        },
        other => other,
    }
}

fn with_checkin(source: SourceKind, seconds: u64, wake_on_tools: bool) -> SourceKind {
    match source {
        SourceKind::Cell { facts, latest } => SourceKind::Cell {
            facts: CellFacts {
                checkin: Some(PythonCheckin {
                    after: Duration::from_secs(seconds),
                    wake_on_tools,
                }),
                ..facts
            },
            latest,
        },
        _ => panic!("only a cell has a check-in"),
    }
}

/// A job `cell` registered at `registered_at`, still running.
fn running_job(cell: u64, registered_at: u64) -> SourceKind {
    SourceKind::Job {
        facts: JobFacts {
            cell,
            registered_at: UnixMs(registered_at),
            output_since: None,
            finished: None,
        },
    }
}

/// ...and one that exited 0 at `at`.
fn finished_job(cell: u64, registered_at: u64, at: u64) -> SourceKind {
    SourceKind::Job {
        facts: JobFacts {
            cell,
            registered_at: UnixMs(registered_at),
            output_since: None,
            finished: Some(JobEnd {
                at: UnixMs(at),
                failed: false,
            }),
        },
    }
}

fn pending_mail(oldest_at: u64, newest_at: u64) -> SourceKind {
    SourceKind::Mail {
        oldest_at: Some(UnixMs(oldest_at)),
        newest_at: Some(UnixMs(newest_at)),
    }
}

fn pending_user(at: u64) -> SourceKind {
    SourceKind::User {
        interrupt: false,
        oldest_at: Some(UnixMs(at)),
    }
}

fn interrupting_user() -> SourceKind {
    SourceKind::User {
        interrupt: true,
        oldest_at: Some(UnixMs(0)),
    }
}

/// The queues an idle agent always has: present, and holding nothing.
fn idle_queues() -> Vec<SourceKind> {
    vec![
        SourceKind::User {
            interrupt: false,
            oldest_at: None,
        },
        SourceKind::Mail {
            oldest_at: None,
            newest_at: None,
        },
    ]
}

fn with(
    mut sources: Vec<SourceKind>,
    more: impl IntoIterator<Item = SourceKind>,
) -> Vec<SourceKind> {
    sources.extend(more);
    sources
}

fn millis(duration: Duration) -> u64 {
    duration.as_millis() as u64
}

// -- the one decision -------------------------------------------------------
//
// Each test reads as a timeline: time down the page, one column per source,
// and what the decision says at each moment on the right. The `model` column
// is what the model's own turn said.

//  ms      model      cell 2       boundary
//  0       calls      runs a job   hold, until the check-in
//  120000                          SEND — the check-in, with nothing to show
#[test]
fn the_check_in_happens_even_when_nothing_arrived() {
    let mut idle = ask(with(
        idle_queues(),
        [returned_cell(2, 2, 10), running_job(2, 10)],
    ));
    assert_eq!(idle.recheck(UnixMs(0)), Some(UnixMs(millis(DEFAULT_WAIT))));
    let wake = idle.wake(UnixMs(millis(DEFAULT_WAIT)));
    assert_eq!(wake.trigger, WakeTrigger::Checkin);
    assert!(wake.events.is_empty());
    assert_eq!(wake.foreground_running, 1);
}

#[test]
fn nothing_pending_and_nobody_asking_means_no_request() {
    let mut nobody = ask(idle_queues());
    nobody.turn = None;
    assert_eq!(
        nobody.boundary(UnixMs(5_000)),
        Boundary::No { recheck: None }
    );
    // A turn of prose asked for nothing either: what its cells go on to do
    // is the only thing that wakes it, however long it runs.
    let mut prose = ask(with(idle_queues(), [running_job(2, 0)])).replied(0);
    assert_eq!(
        prose.boundary(UnixMs(500_000)),
        Boundary::No { recheck: None }
    );
}

#[test]
fn the_latest_cell_sets_the_check_in_and_an_older_one_does_not() {
    let mut named = ask(with(
        idle_queues(),
        [
            with_checkin(returned_cell(2, 2, 0), 600, true),
            running_job(2, 0),
        ],
    ));
    assert_eq!(named.recheck(UnixMs(1)), Some(UnixMs(600_000)));
    assert_eq!(named.wake(UnixMs(600_000)).trigger, WakeTrigger::Checkin);
    // An old monitor that asked for an hour of quiet does not slow the turn
    // after it, whose cell said nothing and gets the default.
    let mut inherited = ask(with(
        idle_queues(),
        [
            older(with_checkin(running_cell(1, 2), 3_600, false)),
            returned_cell(2, 2, 0),
            running_job(2, 0),
        ],
    ));
    assert_eq!(
        inherited.recheck(UnixMs(1)),
        Some(UnixMs(millis(DEFAULT_WAIT)))
    );
}

//  ms      model      user     cell 2       boundary
//  1000               types    (quiet)      SEND — nothing to wait for
#[test]
fn a_typed_message_goes_at_once_when_nothing_is_running() {
    let mut typed = ask(with(idle_queues(), [pending_user(1_000)]));
    assert_eq!(typed.wake(UnixMs(1_000)).trigger, WakeTrigger::User);
}

//  ms      model      user     cell 2       boundary
//  1000               types    job runs     hold — half a second for the job
//  1500                                     SEND
#[test]
fn a_typed_message_waits_half_a_second_for_the_foreground() {
    let mut typed = ask(with(
        idle_queues(),
        [running_job(2, 0), pending_user(1_000)],
    ));
    assert_eq!(
        typed.recheck(UnixMs(1_000)),
        Some(UnixMs(1_000 + millis(USER_PATIENCE)))
    );
    assert_eq!(
        typed.wake(UnixMs(1_000 + millis(USER_PATIENCE))).trigger,
        WakeTrigger::User
    );
}

#[test]
fn interrupt_discards_the_in_flight_request_and_nothing_else_disturbs_it() {
    let mut interrupted = ask(vec![interrupting_user()]);
    interrupted.phase = Phase::Requesting(InFlight::default());
    assert_eq!(interrupted.boundary(UnixMs(0)), Boundary::AbortAndResend);
    for loud in [
        pending_user(0),
        pending_mail(0, 0),
        failed(finished_job(2, 0, 0)),
        notified(running_cell(2, 2), 0),
    ] {
        let mut busy = ask(vec![loud]);
        busy.phase = Phase::Requesting(InFlight::default());
        assert_eq!(
            busy.boundary(UnixMs(60_000)),
            Boundary::No { recheck: None },
            "no timer is armed while a request is in flight"
        );
    }
}

//  ms      model      job a     job b     boundary
//  0       calls      starts    starts    hold
//  1000               exit 0              hold — 60s for b
//  5000                         exit 0    SEND — the round is whole
#[test]
fn one_round_of_parallel_jobs_arrives_in_one_request() {
    let mut round = ask(with(
        idle_queues(),
        [
            returned_cell(2, 2, 0),
            finished_job(2, 0, 1_000),
            running_job(2, 1),
        ],
    ));
    assert_eq!(
        round.recheck(UnixMs(1_000)),
        Some(UnixMs(1_000 + millis(FOREGROUND_PATIENCE)))
    );
    round.sources[4] = finished_job(2, 1, 5_000);
    let wake = round.wake(UnixMs(5_000));
    assert_eq!(wake.trigger, WakeTrigger::Finished);
    assert_eq!(wake.events.len(), 2);
    assert_eq!(wake.foreground_running, 0);
}

//  ms      model      job a     job b       boundary
//  1000               exit 0    (running)   hold
//  61000                        (running)   SEND — a never ends; a is worth
// having
#[test]
fn a_job_that_never_ends_does_not_hold_a_finished_sibling_forever() {
    let mut held = ask(with(
        idle_queues(),
        [
            returned_cell(2, 2, 0),
            finished_job(2, 0, 1_000),
            running_job(2, 1),
        ],
    ));
    let due = UnixMs(1_000 + millis(FOREGROUND_PATIENCE));
    assert_eq!(held.recheck(UnixMs(1_000)), Some(due));
    assert!(held.holds(UnixMs(due.0 - 1)));
    assert_eq!(held.wake(due).trigger, WakeTrigger::Finished);
}

//  ms      model      job a     job b       boundary
//  1000               exit 1    (running)   hold — but only 20s: a failure is
// news  21000                                    SEND
#[test]
fn a_failed_job_waits_less_for_its_siblings_whichever_cell_it_belongs_to() {
    for cell in [1, 2] {
        let mut failing = ask(with(
            idle_queues(),
            [
                returned_cell(2, 2, 0),
                failed(finished_job(cell, 0, 1_000)),
                running_job(2, 1),
            ],
        ));
        let due = UnixMs(1_000 + millis(FAILURE_PATIENCE));
        assert_eq!(failing.recheck(UnixMs(1_000)), Some(due), "cell {cell}");
        let wake = failing.wake(due);
        assert_eq!(wake.trigger, WakeTrigger::Finished);
        assert_eq!(wake.events[0].kind, WakeKind::Failed);
        assert_eq!(wake.events[0].foreground, cell == 2);
    }
}

//  ms      model      cell 1's job   cell 2's job   boundary
//  0       calls                     starts (FG=2)  hold
//  1000               exit 0                        hold — nobody is waiting
// for it  119999                                           still holding
//  120000                                           SEND — the check-in
// delivers it
#[test]
fn a_background_success_rides_along_with_the_next_wake() {
    let mut background = ask(with(
        idle_queues(),
        [
            returned_cell(2, 2, 0),
            finished_job(1, 0, 1_000),
            running_job(2, 5),
        ],
    ));
    assert_eq!(
        background.recheck(UnixMs(1_000)),
        Some(UnixMs(millis(DEFAULT_WAIT)))
    );
    assert!(background.holds(UnixMs(millis(DEFAULT_WAIT) - 1)));
    let wake = background.wake(UnixMs(millis(DEFAULT_WAIT)));
    assert_eq!(wake.trigger, WakeTrigger::Checkin);
    assert_eq!(wake.events.len(), 1);
    assert!(!wake.events[0].foreground);
    assert_eq!(wake.events[0].deadline, None, "no clock of its own");
    assert_eq!(wake.background_running, 0);
    assert_eq!(wake.foreground_running, 1);

    // ...or with the foreground ending, whichever comes first.
    let mut released = ask(with(
        idle_queues(),
        [
            returned_cell(2, 2, 0),
            finished_job(1, 0, 1_000),
            running_job(2, 5),
        ],
    ));
    released.recheck(UnixMs(1_000));
    released.sources[4] = finished_job(2, 5, 40_000);
    let wake = released.wake(UnixMs(40_000));
    assert_eq!(wake.trigger, WakeTrigger::Finished);
    assert_eq!(wake.events.len(), 2);
}

//  ms      model      cell 1's job   boundary
//  0       replies                   hold
//  1000               exit 0         SEND — it is the only thing there is
#[test]
fn a_background_finish_wakes_an_agent_with_nothing_else_running() {
    let mut alone = ask(with(idle_queues(), [finished_job(1, 0, 1_000)])).replied(0);
    let wake = alone.wake(UnixMs(1_000));
    assert_eq!(wake.trigger, WakeTrigger::Finished);
    // Other background work is not company either: the model moved on from
    // all of it when cell 2 registered its own job, so once that job is
    // done nothing is waited for.
    let mut crowd = ask(with(
        idle_queues(),
        [
            older(returned_cell(1, 2, 0)),
            finished_job(1, 0, 1_000),
            running_job(1, 1),
            returned_cell(2, 2, 900),
        ],
    ));
    let wake = crowd.wake(UnixMs(1_000));
    assert_eq!(wake.trigger, WakeTrigger::Finished);
    assert_eq!(wake.background_running, 1);
}

//  ms      model      cell 1's job   cell 2's job   boundary
//  1000               exit 1         (running)      hold — 20s, as a failure
// anywhere  21000                                            SEND
#[test]
fn a_background_failure_has_a_patience_of_its_own() {
    let mut failing = ask(with(
        idle_queues(),
        [
            returned_cell(2, 2, 0),
            failed(finished_job(1, 0, 1_000)),
            running_job(2, 5),
        ],
    ));
    assert_eq!(
        failing.recheck(UnixMs(1_000)),
        Some(UnixMs(1_000 + millis(FAILURE_PATIENCE)))
    );
}

//  ms      model      cell 2          boundary
//  1000               notify()        hold — a second for the ones behind it
//  2000                               SEND
#[test]
fn a_notify_coalesces_for_a_second_and_goes_at_once_with_nothing_running() {
    let mut loud = ask(with(idle_queues(), [notified(running_cell(2, 2), 1_000)]));
    assert_eq!(
        loud.recheck(UnixMs(1_000)),
        Some(UnixMs(1_000 + millis(NOTIFY_PATIENCE)))
    );
    let wake = loud.wake(UnixMs(1_000 + millis(NOTIFY_PATIENCE)));
    assert_eq!(wake.trigger, WakeTrigger::Notify);
    assert_eq!(wake.events[0].kind, WakeKind::Notify);
    // A returned cell whose detached task notifies is not company for itself.
    let mut quiet = ask(with(
        idle_queues(),
        [notified(returned_cell(2, 2, 0), 1_000)],
    ));
    assert_eq!(quiet.wake(UnixMs(1_000)).trigger, WakeTrigger::Notify);
}

#[test]
fn a_repeated_notify_after_a_drain_is_a_fresh_event() {
    let mut loud = ask(with(idle_queues(), [notified(running_cell(2, 2), 1_000)]));
    assert_eq!(loud.recheck(UnixMs(1_000)), Some(UnixMs(2_000)));
    loud.wake(UnixMs(2_000));
    loud.drained();
    loud.sources[2] = notified(running_cell(2, 2), 3_000);
    assert_eq!(loud.recheck(UnixMs(3_000)), Some(UnixMs(4_000)));
    assert_eq!(loud.wake(UnixMs(4_000)).trigger, WakeTrigger::Notify);
}

//  ms      model      job          boundary
//  0       calls      starts       hold
//  4000               prints       hold — nobody asked for it
//  120000                          SEND — and it rides along
#[test]
fn plain_output_is_never_an_event_and_arms_no_timer() {
    let mut chatty = ask(with(
        idle_queues(),
        [
            with_output(running_cell(2, 2), 4_000),
            with_output(running_job(2, 0), 4_000),
        ],
    ));
    assert_eq!(
        chatty.recheck(UnixMs(4_000)),
        Some(UnixMs(millis(DEFAULT_WAIT)))
    );
    let wake = chatty.wake(UnixMs(millis(DEFAULT_WAIT)));
    assert!(wake.events.is_empty());
    let mut prose = ask(with(idle_queues(), [with_output(running_job(2, 0), 4_000)])).replied(0);
    assert_eq!(
        prose.boundary(UnixMs(500_000)),
        Boundary::No { recheck: None }
    );
}

//  ms      model      cell 2                 boundary
//  0       calls      starts a job, returns  hold — returning silently is not
// news  1000               job exits              SEND
#[test]
fn a_cell_returning_silently_is_not_an_event_but_returning_with_output_is() {
    let mut silent = ask(with(
        idle_queues(),
        [returned_cell(2, 2, 0), running_job(2, 0)],
    ));
    assert_eq!(
        silent.recheck(UnixMs(1)),
        Some(UnixMs(millis(DEFAULT_WAIT)))
    );
    let mut spoken = ask(with(
        idle_queues(),
        [with_output(returned_cell(2, 2, 0), 0), running_job(2, 0)],
    ));
    assert_eq!(
        spoken.recheck(UnixMs(0)),
        Some(UnixMs(millis(FOREGROUND_PATIENCE)))
    );
    let wake = spoken.wake(UnixMs(millis(FOREGROUND_PATIENCE)));
    assert_eq!(wake.events[0].kind, WakeKind::Succeeded);
    assert_eq!(wake.events[0].source, u64::MAX);
    // Setting a check-in and returning is the quiet case: the interval it
    // named is honoured, not defeated by its own return.
    let mut paced = ask(with(
        idle_queues(),
        [
            with_checkin(returned_cell(2, 2, 0), 600, true),
            running_job(2, 0),
        ],
    ));
    assert_eq!(paced.recheck(UnixMs(1)), Some(UnixMs(600_000)));
}

#[test]
fn a_cell_that_raised_is_a_failure() {
    let mut raised = ask(with(
        idle_queues(),
        [
            failed(with_output(returned_cell(2, 2, 0), 0)),
            running_job(2, 0),
        ],
    ));
    assert_eq!(
        raised.recheck(UnixMs(0)),
        Some(UnixMs(millis(FAILURE_PATIENCE)))
    );
    assert_eq!(
        raised.wake(UnixMs(millis(FAILURE_PATIENCE))).events[0].kind,
        WakeKind::Failed
    );
}

//  ms      phase        job         boundary
//  1000    requesting   exit 0      hold — nothing is measured
//  5000    idle                     hold — the clock starts here
//  65000                            SEND
#[test]
fn patience_is_measured_from_when_the_scheduler_first_saw_the_event() {
    let mut late = ask(with(
        idle_queues(),
        [
            returned_cell(2, 2, 0),
            finished_job(2, 0, 1_000),
            running_job(2, 1),
        ],
    ));
    late.phase = Phase::Requesting(InFlight::default());
    assert_eq!(late.boundary(UnixMs(1_000)), Boundary::No { recheck: None });
    late.phase = Phase::Idle {
        owed: Vec::new(),
        standing: Standing::Nothing,
    };
    let due = UnixMs(5_000 + millis(FOREGROUND_PATIENCE));
    assert_eq!(late.recheck(UnixMs(5_000)), Some(due));
    let wake = late.wake(due);
    assert_eq!(wake.events[0].occurred_at, UnixMs(1_000));
    assert_eq!(wake.events[0].seen_at, UnixMs(5_000));
    assert_eq!(wake.events[0].deadline, Some(due));
    // A stopped agent sees nothing either; the clock starts when it is
    // lifted.
    let mut stopped = ask(with(
        idle_queues(),
        [
            returned_cell(2, 2, 0),
            finished_job(2, 0, 1_000),
            running_job(2, 1),
        ],
    ));
    stopped.phase = Phase::Idle {
        owed: Vec::new(),
        standing: Standing::Cancelled { at: UnixMs(500) },
    };
    assert_eq!(
        stopped.boundary(UnixMs(1_000)),
        Boundary::No { recheck: None }
    );
    stopped.sources[0] = pending_user(2_000);
    assert_eq!(
        stopped.recheck(UnixMs(2_000)),
        Some(UnixMs(2_000 + millis(USER_PATIENCE)))
    );
    assert_eq!(stopped.wake(UnixMs(2_500)).events[0].seen_at, UnixMs(2_000));
}

#[test]
fn checkin_can_suppress_all_tool_wakes_without_suppressing_timer_user_or_mail() {
    for wake_on_tools in [false, true] {
        let sources = with(
            idle_queues(),
            [
                with_checkin(notified(running_cell(2, 2), 1), 600, wake_on_tools),
                failed(finished_job(2, 0, 1)),
                finished_job(1, 0, 1),
            ],
        );
        let mut scenario = ask(sources.clone());
        if wake_on_tools {
            assert_eq!(scenario.recheck(UnixMs(1)), Some(UnixMs(1_001)));
            assert!(scenario.wake(UnixMs(1_001)).trigger == WakeTrigger::Notify);
            continue;
        }
        assert_eq!(scenario.recheck(UnixMs(1)), Some(UnixMs(600_000)));
        assert_eq!(scenario.recheck(UnixMs(599_999)), Some(UnixMs(600_000)));
        let wake = scenario.wake(UnixMs(600_000));
        assert_eq!(wake.trigger, WakeTrigger::Checkin);
        assert!(wake.tools_suppressed);
        assert_eq!(wake.events.len(), 3, "still delivered, and still recorded");
        for input in [pending_user(20_000), pending_mail(20_000, 20_000)] {
            let mut with_input = ask(with(sources.clone(), [input]));
            assert!(with_input.boundary(UnixMs(30_000)).is_now());
        }
        // The next turn's cell said nothing, so the notebook wakes it again.
        let mut next_turn = sources;
        next_turn[2] = older(next_turn[2].clone());
        assert_eq!(ask(next_turn).recheck(UnixMs(1)), Some(UnixMs(1_001)));
    }
}

//  ms      model      mail           job          boundary
//  1000               peer writes    (quiet)      hold — a beat, in case of
// more  2000                                           SEND — the burst is over
#[test]
fn mail_waits_a_beat_so_a_chatty_peer_costs_one_request() {
    let mut mailed = ask(with(idle_queues(), [pending_mail(1_000, 1_000)])).replied(0);
    assert_eq!(
        mailed.recheck(UnixMs(1_000)),
        Some(UnixMs(1_000 + millis(MAIL_BURST)))
    );
    assert_eq!(
        mailed.wake(UnixMs(1_000 + millis(MAIL_BURST))).trigger,
        WakeTrigger::Mail
    );
    // The last peer to speak sets how long the burst is expected to last.
    let mut again = ask(with(idle_queues(), [pending_mail(1_000, 1_500)])).replied(0);
    assert_eq!(
        again.recheck(UnixMs(1_500)),
        Some(UnixMs(1_500 + millis(MAIL_BURST)))
    );
    // ...and beside a running job, mail sits out its whole patience.
    let mut busy = ask(with(
        idle_queues(),
        [pending_mail(1_000, 1_000), running_job(2, 0)],
    ));
    assert_eq!(
        busy.recheck(UnixMs(1_000)),
        Some(UnixMs(1_000 + millis(MAIL_PATIENCE)))
    );
    assert_eq!(busy.wake(UnixMs(3_000)).trigger, WakeTrigger::Mail);
}

//  ms      model      user     job a    job b       boundary
//  1000               types    exit 0   (running)   hold — 500ms, the shortest
//  1500                                             SEND — and a rides along
#[test]
fn the_least_patient_holder_ends_the_wait_for_everyone() {
    let mut mixed = ask(with(
        idle_queues(),
        [
            returned_cell(2, 2, 0),
            finished_job(2, 0, 1_000),
            running_job(2, 1),
            pending_user(1_000),
        ],
    ));
    assert_eq!(mixed.recheck(UnixMs(1_000)), Some(UnixMs(1_500)));
    let wake = mixed.wake(UnixMs(1_500));
    assert_eq!(wake.trigger, WakeTrigger::User);
    assert_eq!(wake.events.len(), 1);
}

#[test]
fn the_loop_can_never_sleep_past_a_boundary() {
    let mut mixed = ask(with(
        idle_queues(),
        [
            returned_cell(2, 2, 0),
            failed(finished_job(2, 0, 1_000)),
            running_job(2, 1),
            pending_mail(1_000, 1_000),
        ],
    ));
    let recheck = mixed.recheck(UnixMs(1_000)).unwrap();
    assert!(mixed.holds(UnixMs(recheck.0 - 1)));
    assert!(mixed.boundary(recheck).is_now());
}

#[test]
fn the_waits_rank_people_above_peers_above_machines() {
    assert!(USER_PATIENCE < MAIL_PATIENCE);
    assert!(MAIL_BURST < MAIL_PATIENCE);
    assert!(NOTIFY_PATIENCE <= MAIL_PATIENCE);
    assert!(MAIL_PATIENCE < FAILURE_PATIENCE);
    assert!(FAILURE_PATIENCE < FOREGROUND_PATIENCE);
    assert!(FOREGROUND_PATIENCE < DEFAULT_WAIT);
}

//  ms      standing          job        user     boundary
//  500     cancelled
//  1000                      exit 1              hold — its dying words
//  2000                                 types    SEND
#[test]
fn a_cancelled_agent_is_not_woken_by_its_jobs_dying_words() {
    let mut cancelled = ask(with(idle_queues(), [failed(finished_job(2, 0, 1_000))]));
    cancelled.phase = Phase::Idle {
        owed: Vec::new(),
        standing: Standing::Cancelled { at: UnixMs(500) },
    };
    assert_eq!(
        cancelled.boundary(UnixMs(1_000)),
        Boundary::No { recheck: None }
    );
    assert_eq!(
        cancelled.boundary(UnixMs(500_000)),
        Boundary::No { recheck: None }
    );
    cancelled.sources[0] = pending_user(2_000);
    assert!(cancelled.boundary(UnixMs(2_000)).is_now());
}

#[test]
fn fresh_mail_revives_failure_but_old_mail_and_job_completion_do_not() {
    let mut failed_agent = ask(with(idle_queues(), [finished_job(2, 0, 2_000)]));
    failed_agent.phase = Phase::Idle {
        owed: Vec::new(),
        standing: Standing::Failed {
            at: UnixMs(1_000),
            error: Arc::from("boom"),
        },
    };
    assert_eq!(
        failed_agent.boundary(UnixMs(2_000)),
        Boundary::No { recheck: None }
    );
    failed_agent.sources[1] = pending_mail(500, 500);
    assert_eq!(
        failed_agent.boundary(UnixMs(2_000)),
        Boundary::No { recheck: None }
    );
    failed_agent.sources[1] = pending_mail(500, 1_000);
    assert!(failed_agent.boundary(UnixMs(2_000)).is_now());
}

#[test]
fn what_a_restart_owes_and_when_it_pays_are_two_questions() {
    let mut owing = ask(idle_queues());
    owing.turn = None;
    owing.phase = Phase::Idle {
        owed: vec![call("gone")],
        standing: Standing::Nothing,
    };
    assert_eq!(
        owing.boundary(UnixMs(0)),
        Boundary::No { recheck: None },
        "what is owed is not a reason to send"
    );
    owing.phase = Phase::Idle {
        owed: vec![call("gone")],
        standing: Standing::Asked,
    };
    assert_eq!(owing.wake(UnixMs(0)).trigger, WakeTrigger::Asked);
}

#[test]
fn provider_retries_use_boundary_backoff_even_with_fresh_command_output() {
    let mut schedule = ask(with(idle_queues(), [failed(finished_job(2, 0, 100))]));
    schedule.phase = Phase::Idle {
        owed: Vec::new(),
        standing: Standing::Retry {
            since: UnixMs(100),
            failed_at: UnixMs(100),
            attempts: 1,
            error: Arc::from("overloaded"),
        },
    };
    assert_eq!(schedule.recheck(UnixMs(100)), Some(UnixMs(1_100)));
    let wake = schedule.wake(UnixMs(1_100));
    assert_eq!(wake.trigger, WakeTrigger::Retry);
    assert_eq!(
        wake.events.len(),
        1,
        "the retry carries what finished meanwhile"
    );
    assert_eq!(
        schedule.boundary(UnixMs(100 + 8 * 60 * 60 * 1000)),
        Boundary::RetryExhausted
    );
    schedule.sources[0] = pending_user(101);
    assert_eq!(schedule.wake(UnixMs(101)).trigger, WakeTrigger::Retry);
}

#[test]
fn the_wake_records_the_room() {
    let mut room = ask(with(
        idle_queues(),
        [
            older(running_cell(1, 2)),
            running_job(1, 0),
            returned_cell(2, 2, 10),
            running_job(2, 10),
            finished_job(2, 11, 1_000),
            pending_user(1_000),
        ],
    ));
    let wake = room.wake(UnixMs(1_500));
    assert_eq!(wake.trigger, WakeTrigger::User);
    assert_eq!(wake.foreground_running, 1);
    assert_eq!(wake.background_running, 2, "the old cell and its job");
    assert_eq!(wake.checkin_at, Some(UnixMs(millis(DEFAULT_WAIT))));
    assert!(!wake.tools_suppressed);
    assert_eq!(wake.events.len(), 1);
    assert_eq!(wake.events[0].cell, 2);
    assert_eq!(wake.events[0].source, 11);
}

#[test]
fn python_accepts_one_exec_or_final_prose_not_multiple_calls() {
    let exec = call("exec-1");
    assert!(validate_tool_calls(&[]).is_ok());
    assert!(validate_tool_calls(std::slice::from_ref(&exec)).is_ok());
    assert!(validate_tool_calls(&[exec.clone(), exec.clone()]).is_err());
    let mut other = call("other");
    other.name = ToolName::try_from("not-exec").unwrap();
    assert!(validate_tool_calls(&[other]).is_err());
}

// -- tool plumbing ----------------------------------------------------------

#[tokio::test]
async fn a_wake_that_lands_while_the_core_is_busy_is_not_lost() {
    let notify = Arc::new(Notify::new());
    let waker = SourceWaker::new(Arc::clone(&notify));

    // The tool signals before anyone is listening.
    waker.wake();

    let woken = tokio::time::timeout(Duration::from_millis(50), notify.notified());
    assert!(woken.await.is_ok(), "the permit survives until awaited");
}

fn python_tool(directory: &tempfile::TempDir) -> rho_agent_tools::PythonTool {
    rho_agent_tools::PythonTool::new(
        rho_tool_shell::ShellTools::in_directory(
            Duration::from_secs(20),
            directory.path().to_str().unwrap().into(),
            rho_fs_view::PathOverrides::default(),
        ),
        Vec::new(),
    )
    .unwrap()
}

/// The call's sources as the loop reports them for the model's latest cell.
fn latest_sources(running: &RunningTool) -> Vec<SourceKind> {
    running
        .sources()
        .map(|source| match source {
            SourceKind::Cell { facts, .. } => SourceKind::Cell {
                facts,
                latest: true,
            },
            other => other,
        })
        .collect()
}

async fn until_jobs_registered(running: &RunningTool, wake: &Arc<Notify>, count: usize) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let jobs = running
                .session
                .sources()
                .into_iter()
                .filter(|(_, facts)| matches!(facts, rho_agent_tools::SourceFacts::Job(_)))
                .count();
            if jobs >= count {
                break;
            }
            wake.notified().await;
        }
    })
    .await
    .unwrap()
}

async fn until_job_ends(
    running: &RunningTool,
    wake: &Arc<Notify>,
    registered_index: usize,
) -> UnixMs {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let jobs = running
                .session
                .sources()
                .into_iter()
                .filter_map(|(_, facts)| match facts {
                    rho_agent_tools::SourceFacts::Job(job) => Some(job),
                    rho_agent_tools::SourceFacts::Cell(_) => None,
                })
                .collect::<Vec<_>>();
            if let Some(end) = jobs.get(registered_index).and_then(|job| job.finished) {
                break end.at;
            }
            wake.notified().await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn python_commands_are_independent_boundary_sources_even_after_exec_answers() {
    let directory = tempfile::tempdir().unwrap();
    let tool = python_tool(&directory);
    let wake = Arc::new(Notify::new());
    let mut invocation = call("python-sources");
    invocation.arguments = "command('echo first')\nsecond = command('while [ ! -f release ]; do sleep 0.01; done; echo second')\nawait second\ncommand('while [ ! -f finish ]; do sleep 0.01; done; echo third')".into();
    let mut running = RunningTool {
        session: tool.run(invocation.clone(), SourceWaker::new(wake.clone())),
        call: invocation,
        started_at: UnixMs::now(),
        answer: ToolCallAnswer::Owed,
    };
    // The cell registers its second command a moment after the first, which
    // can end before then; the announcement below needs both on the books.
    until_jobs_registered(&running, &wake, 2).await;
    let first_at = until_job_ends(&running, &wake, 0).await;
    let mut schedule = ask(latest_sources(&running));
    schedule.turn = None;
    let due = first_at + FOREGROUND_PATIENCE;
    assert_eq!(
        schedule.recheck(first_at),
        Some(due),
        "the finished command waits for its sibling and the cell awaiting it",
    );
    assert!(schedule.boundary(due).is_now());

    // Exactly the normal request drain: the provider gets one exec result,
    // and each source has contributed what it had.
    running.answer = ToolCallAnswer::Sent;
    let first = running.session.first_output();
    assert!(
        first
            .output
            .contains("Process exited with code 0\nOutput:\nfirst"),
        "{}",
        first.output
    );
    assert!(
        first
            .output
            .contains("Command running in background with session ID"),
        "the sibling is announced: {}",
        first.output
    );
    assert!(
        running.sources().all(|source| matches!(
            source,
            SourceKind::Cell { .. }
                | SourceKind::Job {
                    facts: JobFacts { finished: None, .. }
                }
        )),
        "a delivered job is forgotten; the rest is still running"
    );

    std::fs::write(directory.path().join("release"), "").unwrap();
    let second_at = until_job_ends(&running, &wake, 0).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while running
            .session
            .python_exec()
            .unwrap()
            .facts()
            .returned
            .is_none()
        {
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    let mut schedule = ask(latest_sources(&running));
    schedule.turn = None;
    assert_eq!(
        schedule.recheck(second_at),
        Some(second_at + FOREGROUND_PATIENCE),
        "the third command was registered by the same cell: still foreground"
    );

    std::fs::write(directory.path().join("finish"), "").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !running.session.python_exec().unwrap().quiescent() {
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    let mut schedule = ask(latest_sources(&running));
    schedule.turn = None;
    assert!(
        schedule.boundary(UnixMs::now()).is_now(),
        "nothing to batch once every command ends"
    );
    let last = running.session.more_output().unwrap();
    assert_eq!(
        last.output.matches("Process exited with code 0").count(),
        2,
        "{}",
        last.output
    );
    assert!(running.session.done());
}

#[tokio::test]
async fn python_output_order_puts_latest_first_then_older_cells_in_execution_order() {
    let directory = tempfile::tempdir().unwrap();
    let tool = python_tool(&directory);
    let mut running = Vec::new();
    // Deliberately oppose call-ID order. Some calls already have a reply:
    // that must not move their updates behind an older unacknowledged call.
    for id in ["z-oldest", "a-older", "m-latest"] {
        let invocation = call(id);
        running.push(RunningTool {
            session: tool.run(invocation.clone(), SourceWaker::new(Default::default())),
            call: invocation,
            started_at: UnixMs(0),
            answer: if id == "z-oldest" {
                ToolCallAnswer::Owed
            } else {
                ToolCallAnswer::Sent
            },
        });
    }
    let latest = running[2].call.id.clone();
    running.sort_by_key(|tool| tool.output_order(Some(&latest)));
    assert_eq!(
        running
            .iter()
            .map(|tool| tool.call.id.as_str())
            .collect::<Vec<_>>(),
        ["m-latest", "z-oldest", "a-older"],
    );
}
