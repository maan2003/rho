use std::time::Duration;

use rho_core::ToolType;

use super::boundary::{
    DEFAULT_WAIT, MAIL_BURST, MAIL_PATIENCE, PROGRESS_PATIENCE, TOOL_PATIENCE, USER_PATIENCE,
};
use super::*;

fn call(id: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from(id).unwrap(),
        name: ToolName::try_from("shell").unwrap(),
        tool_type: ToolType::Function,
        arguments: "{}".to_owned(),
    }
}

/// The decision as an idle, uninterrupted agent asks it: the model made the
/// calls it is waiting on at 0, and said nothing about when to look at them.
/// Each test varies only what it is about.
#[derive(Clone)]
struct Ask {
    sources: Vec<SourceKind>,
    turn: Option<ModelTurn>,
    phase: Phase,
}

/// Every call listed is one the model asked for at 0 and has not answered
/// yet; [`answered`] is how a test says otherwise.
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
    }
}

impl Ask {
    fn boundary(&self, now: UnixMs) -> Boundary {
        boundary(&self.sources, self.turn.as_ref(), &self.phase, now)
    }

    /// When the decision says to come back, if it can change by itself.
    fn recheck(&self, now: UnixMs) -> Option<UnixMs> {
        match self.boundary(now) {
            Boundary::No { recheck } => recheck,
            Boundary::Now | Boundary::AbortAndResend => None,
        }
    }

    /// The model replied in prose and asked for nothing.
    fn replied(mut self, at: u64) -> Self {
        self.turn = Some(ModelTurn {
            spoke_at: UnixMs(at),
            asked: ModelAsked::Nothing,
        });
        self
    }

    /// The model issued calls at this instant. Which of them have answered
    /// since is [`answered`]'s business, source by source.
    fn called(mut self, at: u64) -> Self {
        self.turn = Some(ModelTurn {
            spoke_at: UnixMs(at),
            asked: ModelAsked::Calls,
        });
        self
    }

    /// ...and asked to be left alone for this long, which is what `wait` will
    /// do once it exists.
    fn waiting(mut self, seconds: u64) -> Self {
        self.turn = self.turn.map(|turn| ModelTurn {
            asked: ModelAsked::Wait(Duration::from_secs(seconds)),
            ..turn
        });
        self
    }
}

/// A call that still owes the model its one answer. The names in the tables
/// above are labels for the reader; a source carries no id, because nothing
/// about the decision depends on which call it is.
fn tool(haste: ToolHaste) -> SourceKind {
    SourceKind::Tool {
        answer: ToolCallAnswer::Owed,
        haste,
    }
}

/// ...and the same call once it has answered, so everything after is an update.
fn answered(call: SourceKind) -> SourceKind {
    match call {
        SourceKind::Tool { haste, .. } => SourceKind::Tool {
            answer: ToolCallAnswer::Sent,
            haste,
        },
        _ => panic!("only a generic call has an answer"),
    }
}

/// Called and working, and has not produced a byte.
fn silent_call() -> SourceKind {
    tool(ToolHaste::None)
}

/// Called and working, holding output it is in the middle of.
fn partial_call(since: u64) -> SourceKind {
    tool(ToolHaste::Eventually {
        since: UnixMs(since),
    })
}

/// Called and working, holding output it says stands on its own.
fn settled_call(since: u64) -> SourceKind {
    tool(ToolHaste::Soon {
        since: UnixMs(since),
    })
}

/// Ended at `at`, with the model not yet told.
fn ended_call(at: u64) -> SourceKind {
    tool(ToolHaste::Ended { at: UnixMs(at) })
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

fn millis(duration: Duration) -> u64 {
    duration.as_millis() as u64
}

// -- the one decision -------------------------------------------------------
//
// Each test reads as a timeline: time down the page, one column per source, and
// what the decision says at each moment on the right. The `model` column is
// what the model's own turn said, which is where the pace comes from.

//  ms      model      user    mail    boundary
//  0       answers                    hold — nobody has anything
//  5000                               hold, and no timer: only an event
#[test]
fn nothing_pending_means_no_request() {
    assert_eq!(
        ask(idle_queues()).replied(0).boundary(UnixMs(5_000)),
        Boundary::No { recheck: None },
        "and nothing to wake up for"
    );
    assert_eq!(
        ask(Vec::new()).replied(0).boundary(UnixMs(5_000)),
        Boundary::No { recheck: None }
    );
}

//  ms      model        cargo build        boundary
//  0       calls it     call               hold — nothing to send yet
//  2000                 "Compiling foo"    hold
//  10000                                   SEND — the model asked to see it
#[test]
fn the_model_is_shown_what_its_calls_have_at_the_interval_it_set() {
    // Nothing about the output causes this. A running tool has no urgency of
    // its own, because there is nothing to see at 2000 that would look any
    // different if this were a dev server nobody is waiting for.
    let schedule = ask(vec![partial_call(2_000)]);
    assert_eq!(
        schedule.boundary(UnixMs(2_000)),
        Boundary::No {
            recheck: Some(UnixMs(millis(DEFAULT_WAIT)))
        }
    );
    assert_eq!(
        schedule.boundary(UnixMs(millis(DEFAULT_WAIT))),
        Boundary::Now
    );
}

//  ms      model        rg TODO       boundary
//  0       calls it     call          hold
//  10000                (silent)      SEND — with nothing at all in it
#[test]
fn the_check_in_happens_even_when_nothing_arrived() {
    // The empty request is the point: it is how a model that has nothing to do
    // finds out it has nothing to do, and asks for a longer interval next time.
    // Everything else here refuses to make a request with nothing in it.
    let schedule = ask(vec![silent_call()]);
    assert_eq!(
        schedule.recheck(UnixMs(0)),
        Some(UnixMs(millis(DEFAULT_WAIT))),
        "and it is a timer, so nothing has to happen for it to fire"
    );
    assert_eq!(
        schedule.boundary(UnixMs(millis(DEFAULT_WAIT))),
        Boundary::Now
    );
}

//  ms      model                 cargo build   boundary
//  0       calls build           call          hold
//  10000                                       SEND — its one look-in
//  10500   "the build is going"                hold, forever
//  300000                        exit          SEND — the ending speaks itself
#[test]
fn a_look_in_lasts_exactly_one_turn() {
    // Otherwise a model that answers in prose is asked for another opinion
    // every ten seconds for as long as its build runs. Asking again is how it
    // gets looked at again — by calling something, or by naming an interval.
    let building = ask(vec![silent_call()]);
    assert_eq!(
        building.recheck(UnixMs(0)),
        Some(UnixMs(millis(DEFAULT_WAIT)))
    );
    assert_eq!(
        building.clone().replied(10_500).boundary(UnixMs(600_000)),
        Boundary::No { recheck: None },
        "it asked for nothing, so it is left alone"
    );
    assert_eq!(
        building
            .replied(10_500)
            .waiting(300)
            .recheck(UnixMs(10_500)),
        Some(UnixMs(310_500)),
        "unless it says when"
    );

    // And an agent that has finished everything asked of it stays quiet.
    assert_eq!(
        ask(idle_queues()).replied(0).boundary(UnixMs(600_000)),
        Boundary::No { recheck: None }
    );
}

//  ms      user    mail    cargo build   boundary
//  0                       call          hold
//  1000                    "Compiling"   hold — the queues add no impatience
//  10000                                 SEND — the check-in, not the queues
#[test]
fn an_empty_queue_is_not_a_deadline() {
    // In the list like every other source, but with nothing queued there is no
    // arrival to count from, so it cannot drag a request forward.
    let mut sources = idle_queues();
    sources.push(partial_call(1_000));
    assert_eq!(
        ask(sources).recheck(UnixMs(1_100)),
        Some(UnixMs(millis(DEFAULT_WAIT)))
    );
}

//  ms      user    boundary
//  0       typed   SEND — nothing else is due, so waiting cannot improve it
#[test]
fn a_typed_message_goes_at_once_when_nothing_is_due() {
    assert_eq!(
        ask(vec![pending_user(0)]).boundary(UnixMs(1)),
        Boundary::Now
    );
}

//  ms      user    rg TODO   boundary
//  0               call      hold — nothing to send
//  1000    typed             hold — briefly, in case the search lands too
//  1500                      SEND — a person does not wait on a machine
#[test]
fn a_person_does_not_sit_out_the_models_own_interval() {
    let typed_at = 1_000;
    let schedule = ask(vec![pending_user(typed_at), silent_call()]);
    let patience_ends = typed_at + millis(USER_PATIENCE);
    assert_eq!(
        schedule.boundary(UnixMs(typed_at)),
        Boundary::No {
            recheck: Some(UnixMs(patience_ends))
        },
        "the check-in is later, and the least patient holder wins"
    );
    assert_eq!(schedule.boundary(UnixMs(patience_ends)), Boundary::Now);
}

//  ms      user    boundary
//  0       typed   in flight — wait, the model finishes what it is saying
//  0       !typed  ABORT — worth throwing the request away for
#[test]
fn interrupt_discards_the_in_flight_request_and_a_plain_send_waits_for_it() {
    let mut schedule = ask(vec![pending_user(0)]);
    schedule.phase = Phase::Requesting(InFlight::default());
    assert_eq!(schedule.boundary(UnixMs(1)), Boundary::No { recheck: None });

    // Interrupting is a property of the message, so it rides in with the
    // source rather than being asked about separately.
    schedule.sources = vec![interrupting_user()];
    assert_eq!(schedule.boundary(UnixMs(1)), Boundary::AbortAndResend);
}

//  ms      mail    call-1    call-2    boundary
//  0       line    out       flag      in flight — none of them may interrupt
#[test]
fn only_a_typed_message_can_interrupt_an_in_flight_request() {
    // Peers and tools wait their turn however loud they are, and a tool that
    // flags its output as standing alone has bought itself nothing here.
    let mut schedule = ask(vec![pending_mail(0, 0), settled_call(0), ended_call(0)]);
    schedule.phase = Phase::Requesting(InFlight::default());
    assert_eq!(
        schedule.boundary(UnixMs(5_000)),
        Boundary::No { recheck: None }
    );
}

//  ms      model     rg a      rg b      rg c      boundary
//  0       calls     call      call      call      hold
//  200               exit                          hold — two still to come
//  3000                        exit                hold — one still to come
//  3100                                  exit      SEND — nothing more is due
//  10200                                           (had they not:
// TOOL_PATIENCE)
#[test]
fn one_round_of_parallel_calls_arrives_in_one_request() {
    // Three shells called together; the quick one finishes while the others
    // have not printed a byte. A call that has been made is certain to speak,
    // so the finished result waits rather than going out alone.
    let waiting = ask(vec![ended_call(200), silent_call(), silent_call()]);
    assert_eq!(
        waiting.recheck(UnixMs(200)),
        Some(UnixMs(200 + millis(TOOL_PATIENCE))),
        "a finished result waits for its siblings, but only so long"
    );

    // ...and the moment the last one lands they all go, without sitting out
    // either patience. This is the whole reason the decision asks whether
    // anything is still due: without it every tool call would cost ten seconds.
    let finished = ask(vec![ended_call(200), ended_call(3_000), ended_call(3_100)]);
    assert_eq!(finished.boundary(UnixMs(3_100)), Boundary::Now);
    assert_eq!(
        ask(vec![ended_call(100)]).boundary(UnixMs(100)),
        Boundary::Now,
        "a single fast call is not made to wait for company that is not coming"
    );
}

//  ms      model        rg a      rg b      boundary
//  0       calls both   call      call      hold
//  200                  exit                hold — b might still speak
//  10200                                    SEND — TOOL_PATIENCE, not the
//                                           model's interval, which is later
//
//  ...and the same with the model having asked for longer:
//  0       wait(300)
//  10200                                    SEND — but not a whole wait
#[test]
fn a_call_that_never_speaks_does_not_hold_a_finished_sibling_forever() {
    // A finished result will not wait forever for a sibling, and this is the
    // number that says so; the model's own check-in is further off.
    let schedule = ask(vec![ended_call(200), silent_call()]);
    assert_eq!(
        schedule.recheck(UnixMs(200)),
        Some(UnixMs(200 + millis(TOOL_PATIENCE)))
    );

    // Asking to be left alone for longer does not change that.
    let patient = schedule.waiting(300);
    let give_up_at = 200 + millis(TOOL_PATIENCE);
    assert!(matches!(
        patient.boundary(UnixMs(give_up_at - 1)),
        Boundary::No { .. }
    ));
    assert_eq!(patient.boundary(UnixMs(give_up_at)), Boundary::Now);
}

//  ms      npm run dev    boundary
//  0       call           hold
//  1000    out            hold — and asking again later does not move it
//  9000    out            hold
//  10000                  SEND
#[test]
fn talking_does_not_buy_a_call_anything() {
    // Every wait is measured from something that already happened, so a source
    // cannot extend one by continuing to produce, and cannot shorten one
    // either. This is what stops one chatty tool from pinning everybody else.
    let schedule = ask(vec![partial_call(1_000)]);
    for now in [1_000, 5_000, 9_000] {
        assert_eq!(
            schedule.recheck(UnixMs(now)),
            Some(UnixMs(millis(DEFAULT_WAIT))),
            "the same instant however much it says in between"
        );
    }
}

//  ms      model       npm run dev    curl :3000   boundary
//  0       calls dev   call                        hold
//  500                 "listening"                 SEND — dev has answered
//  1000    calls curl                 call         hold
//  1500                "GET /"                     hold — and says nothing new
//  2000                               exit         SEND — dev is not due
#[test]
fn a_call_that_has_already_answered_does_not_hold_up_one_that_has_not() {
    // Without this, curl's result would sit for a full TOOL_PATIENCE waiting
    // for a dev server that is never going to finish — and the wait for a tool
    // that never ends is a wait nobody can end.
    let schedule = ask(vec![answered(partial_call(1_500)), ended_call(2_000)]).called(1_000);
    assert_eq!(schedule.boundary(UnixMs(2_000)), Boundary::Now);

    // While dev still owes its answer, the same two sources wait for each
    // other.
    let still_on_it = ask(vec![silent_call(), ended_call(2_000)]);
    assert_eq!(
        still_on_it.recheck(UnixMs(2_000)),
        Some(UnixMs(2_000 + millis(TOOL_PATIENCE))),
        "waiting for it, but only TOOL_PATIENCE"
    );
}

//  ms      model         npm run dev    boundary
//  0       calls dev     call           hold
//  500                   "listening"    SEND — dev answers
//  1100    replies                      hold — and asks for no look-in
//  1500                  "GET /"        hold — nobody asked for this
//  61500                                SEND — but it is not left unsent
// forever
#[test]
fn a_call_that_has_already_answered_is_never_itself_a_reason_to_send() {
    // A minute is the longest anything sits unsent, and it is the only thing
    // plain output from a call nobody is owed an answer from ever buys. It
    // cannot shorten a wait for anybody: there is no impatience here, just a
    // sweep.
    let schedule = ask(vec![answered(partial_call(1_500))])
        .called(1_000)
        .replied(1_100);
    assert_eq!(
        schedule.recheck(UnixMs(1_500)),
        Some(UnixMs(1_500 + millis(PROGRESS_PATIENCE))),
        "no look-in, and nothing about the log line asks for one"
    );
    assert_eq!(
        schedule.boundary(UnixMs(1_500 + millis(PROGRESS_PATIENCE))),
        Boundary::Now
    );

    // ...and a finished sibling does not sit out that minute, because a sweep
    // is not company anybody is waiting for.
    let mut with_result = schedule.clone();
    with_result.sources.push(ended_call(2_000));
    assert_eq!(with_result.boundary(UnixMs(2_000)), Boundary::Now);
}

//  ms      model              cargo test         user    boundary
//  0       calls test         call                       hold
//  500                        "Compiling foo"            hold
//  1000                                          typed   hold
//  1500                                                  SEND — user patience
//  1500    "still waiting",
//          calls nothing
//  2000                       "test foo ... ok"          hold — none asked for
//  62000                                                 SEND — the sweep
#[test]
fn a_turn_that_issues_no_calls_asks_for_nothing_and_moves_nothing() {
    // The drain at 1500 answers every outstanding call, because a provider
    // takes one result per call id — so nothing about being answered can be
    // allowed to mean the model stopped waiting. Only the model saying so does.
    let schedule = ask(vec![partial_call(2_000)]).replied(1_500);
    assert_eq!(
        schedule.recheck(UnixMs(2_000)),
        Some(UnixMs(2_000 + millis(PROGRESS_PATIENCE))),
        "no look-in, because it asked for none"
    );

    // ...and it is still the call the model is waiting on, so a sibling that
    // finishes waits for it rather than going out alone.
    let with_sibling = ask(vec![partial_call(2_000), ended_call(2_500)]).replied(1_500);
    assert_eq!(
        with_sibling.recheck(UnixMs(2_500)),
        Some(UnixMs(2_500 + millis(TOOL_PATIENCE))),
        "a person typing must not demote somebody else's call"
    );
}

//  ms      model        npm run dev          boundary
//  0       calls dev    call                 hold
//  1000    wait(300)                         hold — 300 seconds of quiet
//  60000                Settled "panicked"   hold — collecting
//  60100                exit                 hold — rides along
//  70000                                     SEND, both at once
#[test]
fn a_call_that_ends_or_stands_alone_interrupts_a_wait() {
    // The two things a tool can say that are true whatever the model is doing.
    // They are safe to honour because they are bounded: a call can only end
    // once, and one flag per call per wait is all this can ever cost.
    let waiting = ask(Vec::new()).called(0).replied(1_000).waiting(300);

    let mut panicked = waiting.clone();
    panicked.sources = vec![settled_call(60_000)];
    assert_eq!(
        panicked.boundary(UnixMs(60_000)),
        Boundary::Now,
        "flagged output does not sit out the wait it was flagged during"
    );

    // With a sibling still running it takes the ordinary collecting window
    // first, so the two arrive together rather than a request apiece.
    let mut with_sibling = panicked.clone();
    with_sibling.sources.push(silent_call());
    assert_eq!(
        with_sibling.recheck(UnixMs(60_000)),
        Some(UnixMs(60_000 + millis(TOOL_PATIENCE))),
        "but it waits for company, which is not the same as waiting out a wait"
    );

    let mut ended = waiting.clone();
    ended.sources = vec![tool(ToolHaste::Ended { at: UnixMs(60_100) })];
    assert_eq!(
        ended.boundary(UnixMs(60_100)),
        Boundary::Now,
        "and once it has ended there is nothing left to collect, so it goes"
    );
}

//  ms      model        npm run dev    boundary
//  0       calls dev    call           hold
//  1000    wait(300)                   hold
//  5000                 "GET /"        hold — nobody asked for this
//  305000                              SEND — but not for a whole wait, either
//  601000                              (the wait's own end, had it got there)
#[test]
fn a_wait_is_worth_at_most_five_minutes_of_quiet_while_a_tool_is_talking() {
    // The one number a tool's plain output still buys. It cannot fire while the
    // model is being looked in on every DEFAULT_WAIT, so it only bites once the
    // model has asked for a longer interval than PROGRESS_PATIENCE.
    let schedule = ask(vec![partial_call(5_000)]).replied(1_000).waiting(600);
    assert_eq!(
        schedule.recheck(UnixMs(5_000)),
        Some(UnixMs(5_000 + millis(PROGRESS_PATIENCE)))
    );
    assert_eq!(
        schedule.boundary(UnixMs(5_000 + millis(PROGRESS_PATIENCE))),
        Boundary::Now
    );

    // With nothing to show, the wait runs its full length.
    let quiet = ask(vec![silent_call()]).replied(1_000).waiting(600);
    assert_eq!(quiet.recheck(UnixMs(5_000)), Some(UnixMs(601_000)));
    assert_eq!(quiet.boundary(UnixMs(601_000)), Boundary::Now);
}

// -- the wait tool ----------------------------------------------------------

#[test]
fn a_wait_call_names_the_interval_and_is_answered_on_the_spot() {
    let (interval, answer) = read_wait(r#"{"seconds": 90}"#);
    assert_eq!(interval, Some(Duration::from_secs(90)));
    assert_eq!(answer.status, ToolOutputStatus::Success);
    assert!(answer.output.contains("90"), "{}", answer.output);
}

#[test]
fn a_wait_call_is_bounded_and_a_bad_one_is_an_error_not_a_pace() {
    let (interval, _) = read_wait(r#"{"seconds": 86400}"#);
    assert_eq!(interval, Some(MAX_WAIT));
    for arguments in [r#"{"seconds": 0}"#, "{}", "not json"] {
        let (interval, answer) = read_wait(arguments);
        assert_eq!(interval, None, "{arguments}");
        assert_eq!(answer.status, ToolOutputStatus::Error, "{arguments}");
    }
}

#[test]
fn the_longest_wait_in_a_turn_sets_the_pace() {
    let calls = [
        (WAIT_TOOL_NAME, r#"{"seconds": 30}"#),
        ("shell", "{}"),
        (WAIT_TOOL_NAME, r#"{"seconds": 300}"#),
    ]
    .into_iter()
    .enumerate()
    .map(|(i, (name, arguments))| ToolCall {
        id: ToolCallId::try_from(format!("c{i}").as_str()).unwrap(),
        name: ToolName::try_from(name).unwrap(),
        tool_type: ToolType::Function,
        arguments: arguments.to_owned(),
    })
    .collect::<Vec<_>>();
    assert_eq!(asked_of(&calls), ModelAsked::Wait(Duration::from_secs(300)));
    assert_eq!(asked_of(&calls[1..2]), ModelAsked::Calls);
    assert_eq!(asked_of(&[]), ModelAsked::Nothing);
    // A wait the core could not read is a call like any other: the model
    // gets the error at the ordinary check-in and can try again.
    let mut bad = calls[..1].to_vec();
    bad[0].arguments = "{}".to_owned();
    assert_eq!(asked_of(&bad), ModelAsked::Calls);
}

//  ms      model        npm run dev          user    boundary
//  0       calls dev    call                         hold
//  1000    wait(300)                                 hold
//  60000                Settled "panicked"           SEND — even left behind
//
//  ...and a person is never held by a wait either:
//  60000                                     typed   SEND at 60500
#[test]
fn a_wait_does_not_muffle_a_crash_or_a_person() {
    // Dev answered long ago, so nothing it *says* reaches anybody in a hurry.
    // Flagging is how it says the difference.
    let crashed = ask(vec![answered(settled_call(60_000))])
        .called(1_000)
        .waiting(300);
    assert_eq!(crashed.boundary(UnixMs(60_000)), Boundary::Now);

    let typed = ask(vec![silent_call(), pending_user(60_000)])
        .replied(1_000)
        .waiting(300);
    assert_eq!(
        typed.recheck(UnixMs(60_000)),
        Some(UnixMs(60_000 + millis(USER_PATIENCE))),
        "a person waits their own half second and not a second more"
    );
}

//  ms      model        rg TODO    boundary
//  0       calls rg     call       hold
//  400000               (silent)   SEND — the wait ended long ago
#[test]
fn a_check_in_that_has_already_passed_is_not_a_reason_to_keep_waiting() {
    let schedule = ask(vec![silent_call()]).waiting(300);
    assert_eq!(schedule.boundary(UnixMs(400_000)), Boundary::Now);
}

//  ms      peer    boundary
//  1000    line    hold — a peer usually has more right behind it
//  2000            SEND — both lines in one request
#[test]
fn mail_waits_a_beat_so_a_chatty_peer_costs_one_request() {
    let schedule = ask(vec![pending_mail(1_000, 1_000)]);
    assert!(matches!(
        schedule.boundary(UnixMs(1_000)),
        Boundary::No { .. }
    ));
    assert_eq!(
        schedule.boundary(UnixMs(1_000 + millis(MAIL_BURST))),
        Boundary::Now,
        "the burst is the only expectation that has to be waited out on a clock"
    );

    // A peer that keeps typing does not get to hold the floor forever; the
    // patience runs from its first unsent line, not its latest.
    let endless = |now: u64| ask(vec![pending_mail(1_000, now)]).boundary(UnixMs(now));
    let patience = 1_000 + millis(MAIL_PATIENCE);
    assert!(matches!(endless(patience - 1), Boundary::No { .. }));
    assert_eq!(endless(patience), Boundary::Now);
}

//  ms      peer a    peer b    boundary
//  0       line                hold — a may have more right behind it
//  500               line      hold — a's window lapsed, but b's has not
//  1500                        SEND — nothing more is due, and a is owed
#[test]
fn the_last_peer_to_speak_sets_how_long_anything_is_still_due() {
    let schedule = ask(vec![pending_mail(0, 0), pending_mail(500, 500)]);
    assert_eq!(
        schedule.recheck(UnixMs(1_000)),
        Some(UnixMs(500 + millis(MAIL_BURST))),
        "the later burst wins, because it is the one still open"
    );
    assert_eq!(
        schedule.boundary(UnixMs(500 + millis(MAIL_BURST))),
        Boundary::Now,
        "and rule 3 sends it there, before either patience runs out"
    );
}

//  ms      model       rg TODO    peer    boundary
//  0       calls rg    call               hold
//  5000                (silent)   line    hold — rg might land with it
//  7000                                   SEND
#[test]
fn mail_sits_out_its_patience_for_a_call_the_model_is_on() {
    let schedule = ask(vec![silent_call(), pending_mail(5_000, 5_000)]);
    assert_eq!(
        schedule.recheck(UnixMs(5_000)),
        Some(UnixMs(5_000 + millis(MAIL_PATIENCE))),
        "the check-in at 10000 is further off than the peer's own patience"
    );
    assert_eq!(
        schedule.boundary(UnixMs(5_000 + millis(MAIL_PATIENCE))),
        Boundary::Now
    );
}

//  ms      user    peer    rg TODO   boundary
//  1000            line    call      hold — both are still owed something
//  1500    typed                     SEND — the least patient ends it for all
#[test]
fn the_least_patient_holder_ends_the_wait_for_everyone() {
    let waiting = vec![pending_mail(1_000, 1_000), silent_call()];
    assert!(matches!(
        ask(waiting.clone()).boundary(UnixMs(1_000)),
        Boundary::No { .. }
    ));

    let mut with_user = waiting;
    with_user.push(pending_user(1_000));
    assert_eq!(
        ask(with_user).boundary(UnixMs(1_000 + millis(USER_PATIENCE))),
        Boundary::Now
    );
}

//  ms      call-1    boundary
//  0       exit      SEND — certainty, not a guess: nothing to wait for
#[test]
fn typed_input_and_finished_tools_go_at_once() {
    // Neither can say more, so neither sits out a wait meant for something that
    // still might.
    for settled in [pending_user(0), ended_call(0)] {
        let schedule = ask(vec![settled]);
        assert_eq!(schedule.boundary(UnixMs(0)), Boundary::Now);
        assert_eq!(schedule.recheck(UnixMs(0)), None);
    }
}

//  ms      boundary
//  0       SEND — a retry or a resume, with nothing pending at all
#[test]
fn at_once_sends_even_with_nothing_pending() {
    let mut schedule = ask(Vec::new());
    schedule.phase = Phase::Idle {
        owed: Vec::new(),
        standing: Standing::Asked,
    };
    assert_eq!(schedule.boundary(UnixMs(0)), Boundary::Now);
}

//  ms      call-1    boundary
//  0       exit      SEND
//  0       exit      cancelled: hold, no timer — its words land later
#[test]
fn a_cancelled_agent_is_not_woken_by_its_tools_dying_words() {
    let mut schedule = ask(vec![ended_call(0)]);
    assert_eq!(
        schedule.boundary(UnixMs(0)),
        Boundary::Now,
        "an exited tool would normally go at once"
    );

    schedule.phase = Phase::Idle {
        owed: Vec::new(),
        standing: Standing::Cancelled { at: UnixMs(0) },
    };
    assert_eq!(
        schedule.boundary(UnixMs(0)),
        Boundary::No { recheck: None },
        "and no timer either, so its own output cannot wake it"
    );

    // Not even the check-in, which is the one thing that fires with nothing to
    // send: a cancelled agent must stay stopped until a person says otherwise.
    let mut running = ask(vec![silent_call()]);
    running.phase = Phase::Idle {
        owed: Vec::new(),
        standing: Standing::Cancelled { at: UnixMs(0) },
    };
    assert_eq!(running.recheck(UnixMs(0)), None);
}

//  ms      stop   input   boundary
//  0       cancel  —      hold, no timer
//  1       cancel  mail   still held — a peer is not a person
//  2       cancel  user   SEND
#[test]
fn fresh_user_or_mail_lifts_a_stop() {
    let stopped = |sources| {
        let mut schedule = ask(sources);
        schedule.phase = Phase::Idle {
            owed: Vec::new(),
            standing: Standing::Cancelled { at: UnixMs(0) },
        };
        schedule
    };

    assert_eq!(stopped(Vec::new()).recheck(UnixMs(2)), None);
    assert_eq!(
        stopped(vec![pending_mail(1, 1)]).recheck(UnixMs(2)),
        Some(UnixMs(1_001)),
        "fresh mail revives the agent and gets normal burst batching"
    );
    // Input from before the stop is what the stop was about, so it is only
    // input that arrived after it that counts.
    assert_eq!(stopped(vec![pending_user(0)]).recheck(UnixMs(2)), None);
    assert_eq!(
        stopped(vec![pending_user(2)]).boundary(UnixMs(2)),
        Boundary::Now
    );
}

//  ms      call-1    boundary
//  0       —         restarted, request cut short: SEND, with nothing pending
//  0       —         restarted, nothing cut short: hold, and admit it later
//  0       —         ...and cancelled before it could: hold, no timer
#[test]
fn what_a_restart_owes_and_when_it_pays_are_two_questions() {
    let owed = vec![call("call-1")];
    let restarted = |standing| {
        let mut schedule = ask(Vec::new());
        schedule.phase = Phase::Idle {
            owed: owed.clone(),
            standing,
        };
        schedule
    };

    assert_eq!(
        restarted(Standing::Asked).boundary(UnixMs(0)),
        Boundary::Now,
        "a retry hurries the request without changing what is in it"
    );
    // Owing an explanation is not itself a reason to speak: the call is gone
    // either way, and there is nobody waiting on the answer. What is left is
    // the ordinary check-in, which what is owed neither brought forward nor
    // put off.
    assert_eq!(
        restarted(Standing::Nothing).recheck(UnixMs(0)),
        Some(UnixMs(0) + DEFAULT_WAIT),
    );
    assert_eq!(
        restarted(Standing::Cancelled { at: UnixMs(0) }).recheck(UnixMs(0)),
        None,
        "a cancel stops the agent without cancelling the debt"
    );
}

//  ms      call-1    boundary
//  0       exit      failed: hold, no timer — nothing may retry on its own
#[test]
fn a_failed_request_is_not_retried_by_whatever_finishes_next() {
    // A request that failed for good fails the same way when the next tool
    // ends, so an agent left to itself would hammer the provider for as long
    // as it had tools. Somebody has to look at it: `Retry`, or fresh input.
    let mut schedule = ask(vec![ended_call(0)]);
    schedule.phase = Phase::Idle {
        owed: Vec::new(),
        standing: Standing::Failed {
            at: UnixMs(0),
            error: Arc::from("provider said no"),
        },
    };
    assert_eq!(
        schedule.boundary(UnixMs(0)),
        Boundary::No { recheck: None },
        "the ending is heard at the next boundary, but it does not cause one"
    );
}

#[test]
fn no_timer_is_armed_while_a_request_is_in_flight() {
    let mut schedule = ask(vec![pending_user(0), partial_call(0)]);
    schedule.phase = Phase::Requesting(InFlight::default());
    assert_eq!(schedule.recheck(UnixMs(0)), None);
}

#[test]
fn the_loop_can_never_sleep_past_a_boundary() {
    // A recheck is always strictly ahead of now, and once the moment arrives
    // the answer is Now rather than another wait — so the loop that sleeps
    // until the recheck always wakes to a decision it can act on.
    for sources in [
        vec![silent_call()],
        vec![partial_call(0)],
        vec![ended_call(0), silent_call()],
        vec![pending_mail(0, 0)],
        vec![pending_user(0), silent_call()],
    ] {
        let schedule = ask(sources);
        let Some(recheck) = schedule.recheck(UnixMs(0)) else {
            continue;
        };
        assert!(recheck > UnixMs(0));
        assert_eq!(schedule.boundary(recheck), Boundary::Now);
    }
}

// -- request assembly -------------------------------------------------------

#[test]
fn the_waits_rank_people_above_peers_above_machines() {
    assert!(USER_PATIENCE < MAIL_PATIENCE);
    assert!(MAIL_PATIENCE < TOOL_PATIENCE);
    // ...and last of all comes a call that is still working, because half an
    // answer is worth less than a whole one and can afford to wait for it.
    assert!(TOOL_PATIENCE < PROGRESS_PATIENCE);
    // The check-in has to be sooner than that, or the model would be shown
    // half a build log before it had a chance to say how often it wants one.
    assert!(DEFAULT_WAIT < PROGRESS_PATIENCE);
    // A peer's beat has to fit inside its patience, or nothing would ever
    // collapse into one request.
    assert!(MAIL_BURST < MAIL_PATIENCE);
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

//  ms      model      rg      dev server   a peer   boundary
//  0       calls rg   call    (running)             hold
//  4000                       "GET /"               hold — nobody asked for it
//  4900                       flags a crash         hold
//  5000                exit                         SEND — at once
//  5000                                    writes   ...unless one is mid-burst
#[test]
fn a_finished_call_answers_at_once_however_much_is_running_behind_it() {
    // Every background tool has already answered its own call, so none of them
    // is a result still to come and none can make rg wait for it. Their own
    // moments are all later than rg's, and a deadline is a minimum.
    let running_behind = || {
        vec![
            answered(silent_call()),
            answered(partial_call(4_000)),
            answered(settled_call(4_900)),
        ]
    };
    let mut alone = running_behind();
    alone.push(ended_call(5_000));
    assert_eq!(ask(alone).called(0).boundary(UnixMs(5_000)), Boundary::Now);

    // The one thing that does hold it: a peer that may still be mid-burst,
    // because that is a source with more genuinely on the way. It costs rg the
    // rest of the burst and nothing more — once the burst lapses the patience
    // collapses and rg's own moment, already past, is the deadline.
    let mut mid_burst = running_behind();
    mid_burst.push(ended_call(5_000));
    mid_burst.push(pending_mail(5_000, 5_000));
    let schedule = ask(mid_burst).called(0);
    assert_eq!(
        schedule.recheck(UnixMs(5_000)),
        Some(UnixMs(5_000 + millis(MAIL_BURST)))
    );
    assert_eq!(
        schedule.boundary(UnixMs(5_000 + millis(MAIL_BURST))),
        Boundary::Now
    );
}

#[test]
fn patience_setter_completion_does_not_defeat_its_interval() {
    let control = SourceKind::PythonExec {
        answer: ToolCallAnswer::Owed,
        latest: true,
        facts: rho_agent_tools::PythonExecFacts {
            started: true,
            returned: Some(UnixMs(1)),
            completion: Some(rho_agent_tools::PythonCompletion {
                at: UnixMs(1),
                failed: false,
                produced_output: false,
                dispatched: false,
                set_patience: true,
            }),
            output: Default::default(),
            patience: Some(Duration::from_secs(300)),
        },
    };
    let scenario = ask(vec![control.clone()]).waiting(300);
    assert_eq!(scenario.recheck(UnixMs(1)), Some(UnixMs(300_000)));
    assert_eq!(scenario.boundary(UnixMs(300_000)), Boundary::Now);
    let meaningful = tool(ToolHaste::Soon { since: UnixMs(5) });
    assert_eq!(
        ask(vec![control, meaningful])
            .waiting(300)
            .boundary(UnixMs(5)),
        Boundary::Now
    );
    assert_eq!(
        ask(vec![tool(ToolHaste::Ended { at: UnixMs(5) })])
            .waiting(300)
            .boundary(UnixMs(5)),
        Boundary::Now
    );
}

#[cfg(feature = "code-mode")]
#[tokio::test]
async fn python_surface_has_exec_only_and_direct_surface_keeps_wait() {
    let temp = tempfile::tempdir().unwrap();
    let repo = Arc::new(
        rho_workspaces::Repo::open_plain_with_path_overrides(temp.path(), Default::default())
            .unwrap(),
    );
    let view = rho_workspaces::View::new(vec![repo.user_checkout().await.unwrap()]).unwrap();
    for role in [
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::High,
        },
        AgentRole::Advisor {
            intelligence: crate::db::AdvisorIntelligence::High,
        },
    ] {
        let python = render_agent_surface(view.clone(), role).unwrap();
        assert_eq!(
            python
                .tools
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            vec!["exec"]
        );
        assert!(python.system_prompt.contains("## Python Code Mode"));
        assert!(!python.system_prompt.contains("## JavaScript Code Mode"));
    }
    for role in [
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Medium,
        },
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Cheap,
        },
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Low,
        },
        AgentRole::Advisor {
            intelligence: crate::db::AdvisorIntelligence::Medium,
        },
        AgentRole::Advisor {
            intelligence: crate::db::AdvisorIntelligence::Cheap,
        },
    ] {
        let javascript = render_agent_surface(view.clone(), role).unwrap();
        assert!(javascript.system_prompt.contains("## JavaScript Code Mode"));
        assert!(!javascript.system_prompt.contains("## Python Code Mode"));
        assert!(!javascript.system_prompt.contains("set_patience"));
        assert_eq!(
            javascript
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            vec!["exec", "wait"]
        );
    }
    let direct = render_agent_surface(
        view,
        AgentRole::Engineer {
            intelligence: EngineerIntelligence::Mini,
        },
    )
    .unwrap();
    assert!(direct.tools.iter().any(|t| t.name.as_str() == "wait"));
    assert!(!direct.system_prompt.contains("## Python Code Mode"));
    assert!(!direct.system_prompt.contains("## JavaScript Code Mode"));
}

#[cfg(feature = "code-mode")]
#[tokio::test]
async fn python_commands_are_independent_boundary_sources_even_after_exec_answers() {
    use rho_agent_tools::PythonTool;
    use rho_tool_shell::ShellTools;
    use rho_workspaces::PathOverrides;

    let directory = tempfile::tempdir().unwrap();
    let tool = PythonTool::new(
        ShellTools::in_directory(
            Duration::from_secs(20),
            directory.path().to_str().unwrap().into(),
            PathOverrides::default(),
        ),
        Vec::new(),
    )
    .unwrap();
    let wake = Arc::new(Notify::new());
    let mut invocation = call("python-sources");
    invocation.arguments = "command('echo first')\nsecond = command('while [ ! -f release ]; do sleep 0.01; done; echo second')\nawait second\ncommand('while [ ! -f finish ]; do sleep 0.01; done; echo third')".into();
    let mut running = RunningTool {
        session: tool.run(invocation.clone(), SourceWaker::new(wake.clone())),
        call: invocation,
        started_at: UnixMs::now(),
        answer: ToolCallAnswer::Owed,
        answered_sources: Default::default(),
    };
    let first_at = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let sources = running.session.sources();
            if sources.len() == 3
                && let Some((
                    _,
                    rho_agent_tools::SourceFacts::PythonOperation(
                        rho_agent_tools::PythonOperationFacts {
                            finished: Some(at), ..
                        },
                    ),
                )) = sources.iter().find(|(id, _)| *id == 0)
            {
                break *at;
            }
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    let mut schedule = ask(running.sources().collect());
    schedule.turn = None;
    assert_eq!(
        schedule.boundary(first_at),
        Boundary::No {
            recheck: Some(first_at + TOOL_PATIENCE)
        },
        "the finished command must wait for its sibling, not wake through the cell",
    );
    assert_eq!(schedule.boundary(first_at + TOOL_PATIENCE), Boundary::Now);

    // Exactly the normal request drain: the provider gets one exec result, but
    // each current source has independently contributed to it.
    running.answered_sources = running
        .session
        .sources()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    running.answer = ToolCallAnswer::Sent;
    let first = running.session.first_output();
    assert!(first.output.contains("Process exited with code"));
    assert!(first.output.contains("Command running with session ID"));
    assert!(!first.output.contains("Python cell"));
    assert!(running.sources().all(|source| matches!(
        source,
        SourceKind::PythonExec {
            answer: ToolCallAnswer::Sent,
            ..
        } | SourceKind::PythonOperation {
            answer: ToolCallAnswer::Sent,
            ..
        }
    )));

    std::fs::write(directory.path().join("release"), "").unwrap();
    let second_at = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let sources = running.session.sources();
            if sources.iter().any(|(id, _)| *id == 2)
                && let Some((
                    _,
                    rho_agent_tools::SourceFacts::PythonOperation(
                        rho_agent_tools::PythonOperationFacts {
                            finished: Some(at), ..
                        },
                    ),
                )) = sources.iter().find(|(id, _)| *id == 1)
            {
                break *at;
            }
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    assert!(
        running.sources().any(|source| matches!(
            source,
            SourceKind::PythonOperation {
                answer: ToolCallAnswer::Owed,
                facts: rho_agent_tools::PythonOperationFacts { finished: None, .. },
            }
        )),
        "a command registered after exec's reply still owes its own first result"
    );
    let mut schedule = ask(running.sources().collect());
    schedule.turn = None;
    assert_eq!(
        schedule.boundary(second_at),
        Boundary::No {
            recheck: Some(second_at + TOOL_PATIENCE)
        }
    );

    std::fs::write(directory.path().join("finish"), "").unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if running.session.python_exec().unwrap().quiescent() {
                break;
            }
            wake.notified().await;
        }
    })
    .await
    .unwrap();
    let mut schedule = ask(running.sources().collect());
    schedule.turn = None;
    assert_eq!(
        schedule.boundary(UnixMs::now()),
        Boundary::Now,
        "nothing to batch once every command ends"
    );
    let last = running.session.more_output().unwrap();
    assert_eq!(last.output.matches("Process exited with code").count(), 2);
    assert!(!last.output.contains("Python cell"));
    assert!(running.session.done());
}

#[test]
fn python_exec_pending_is_not_the_same_as_provider_reply_sent() {
    let exec = SourceKind::PythonExec {
        answer: ToolCallAnswer::Sent,
        latest: true,
        facts: rho_agent_tools::PythonExecFacts {
            started: false,
            returned: None,
            completion: None,
            output: Default::default(),
            patience: None,
        },
    };
    let scenario = ask(vec![exec, ended_call(1_000)]);
    assert_eq!(scenario.recheck(UnixMs(1_000)), Some(UnixMs(11_000)));
    assert_eq!(scenario.boundary(UnixMs(11_000)), Boundary::Now);
}

#[test]
fn only_current_python_exec_controls_checkin_and_old_monitor_does_not_delay() {
    let old = SourceKind::PythonExec {
        answer: ToolCallAnswer::Sent,
        latest: false,
        facts: rho_agent_tools::PythonExecFacts {
            started: true,
            returned: None,
            completion: None,
            output: Default::default(),
            patience: Some(Duration::from_secs(3600)),
        },
    };
    let current = SourceKind::PythonExec {
        answer: ToolCallAnswer::Sent,
        latest: true,
        facts: rho_agent_tools::PythonExecFacts {
            started: true,
            returned: Some(UnixMs(1)),
            completion: None,
            output: Default::default(),
            patience: Some(Duration::from_secs(300)),
        },
    };
    assert_eq!(
        ask(vec![old.clone(), current.clone()]).recheck(UnixMs(1)),
        Some(UnixMs(300_000))
    );
    assert_eq!(
        ask(vec![old, current, ended_call(5)]).boundary(UnixMs(5)),
        Boundary::Now
    );
}

#[test]
fn fresh_mail_revives_failure_but_old_mail_and_tool_completion_do_not() {
    let mut scenario = ask(vec![pending_mail(10, 10), ended_call(30)]);
    scenario.phase = Phase::Idle {
        owed: Vec::new(),
        standing: Standing::Failed {
            at: UnixMs(20),
            error: Arc::from("failed"),
        },
    };
    assert_eq!(
        scenario.boundary(UnixMs(30)),
        Boundary::No { recheck: None }
    );
    scenario.sources.push(pending_mail(25, 25));
    assert_ne!(
        scenario.boundary(UnixMs(30)),
        Boundary::No { recheck: None }
    );
    scenario.sources.clear();
    assert_eq!(
        scenario.boundary(UnixMs(2_000)),
        Boundary::No { recheck: None }
    );
}

#[test]
fn python_accepts_one_exec_or_final_prose_not_multiple_calls() {
    use rho_agent_tools::CodeMode;
    let mut exec = call("exec-1");
    exec.name = ToolName::try_from("exec").unwrap();
    assert!(validate_tool_calls(Some(CodeMode::Python), &[]).is_ok());
    assert!(validate_tool_calls(Some(CodeMode::Python), &[exec.clone()]).is_ok());
    assert!(validate_tool_calls(Some(CodeMode::Python), &[exec.clone(), exec.clone()]).is_err());
    assert!(validate_tool_calls(Some(CodeMode::Python), &[call("not-exec")]).is_err());
    assert!(validate_tool_calls(Some(CodeMode::JavaScript), &[exec.clone(), exec]).is_ok());
    assert!(validate_tool_calls(None, &[call("a"), call("b")]).is_ok());
}

#[test]
fn quiet_python_dispatch_does_not_bypass_operation_batching_when_output_arrives() {
    let exec = SourceKind::PythonExec {
        answer: ToolCallAnswer::Owed,
        latest: true,
        facts: rho_agent_tools::PythonExecFacts {
            started: true,
            returned: Some(UnixMs(1)),
            completion: Some(rho_agent_tools::PythonCompletion {
                at: UnixMs(1),
                failed: false,
                produced_output: false,
                dispatched: true,
                set_patience: false,
            }),
            output: rho_agent_tools::PythonOutput {
                since: Some(UnixMs(30_000)),
                notification: None,
            },
            patience: None,
        },
    };
    let operation = |finished| SourceKind::PythonOperation {
        answer: ToolCallAnswer::Owed,
        facts: rho_agent_tools::PythonOperationFacts {
            finished,
            output: Default::default(),
        },
    };
    let scenario = ask(vec![exec, operation(Some(UnixMs(30_000))), operation(None)]);
    assert_eq!(scenario.recheck(UnixMs(30_000)), Some(UnixMs(40_000)));
}

#[tokio::test]
async fn python_output_order_puts_latest_first_then_older_cells_in_execution_order() {
    use rho_agent_tools::{PythonTool, SourceWaker, Tool};
    use rho_tool_shell::ShellTools;
    use rho_workspaces::PathOverrides;

    let directory = tempfile::tempdir().unwrap();
    let tool = PythonTool::new(
        ShellTools::in_directory(
            Duration::from_secs(5),
            directory.path().to_str().unwrap().into(),
            PathOverrides::default(),
        ),
        Vec::new(),
    )
    .unwrap();
    let mut running = Vec::new();
    // Deliberately oppose call-ID order. Some calls already have a reply:
    // that must not move their updates behind an older unacknowledged call.
    for id in ["z-oldest", "a-older", "m-latest"] {
        let mut invocation = call(id);
        invocation.arguments = "pass".into();
        running.push(RunningTool {
            session: tool.run(invocation.clone(), SourceWaker::new(Default::default())),
            call: invocation,
            started_at: UnixMs(0),
            answer: if id == "z-oldest" {
                ToolCallAnswer::Owed
            } else {
                ToolCallAnswer::Sent
            },
            answered_sources: Default::default(),
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
