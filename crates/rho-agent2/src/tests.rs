use std::time::Duration;

use rho_core::{AgentIdDomain, ContentPart, ToolType};

use super::*;
use crate::boundary::{
    DEFAULT_WAIT, MAIL_BURST, MAIL_PATIENCE, PROGRESS_PATIENCE, TOOL_PATIENCE, USER_PATIENCE,
};
use crate::source::SourceKind;
use crate::tool::Told;

fn peer(counter: u64) -> AgentId {
    AgentId::from_counter(counter, &AgentIdDomain(7)).unwrap()
}

fn call(id: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from(id).unwrap(),
        name: ToolName::try_from("shell").unwrap(),
        tool_type: ToolType::Function,
        arguments: "{}".to_owned(),
    }
}

fn user_input(delivery: Delivery, at: u64) -> QueuedInput {
    user_message("hello", delivery, at)
}

fn user_message(text: &str, delivery: Delivery, at: u64) -> QueuedInput {
    QueuedInput {
        source: InputSource::User,
        kind: InputKind::Message {
            content: vec![ContentPart::Text {
                text: text.to_owned(),
            }],
        },
        delivery,
        at: UnixMs(at),
    }
}

fn compaction(at: u64) -> QueuedInput {
    QueuedInput {
        source: InputSource::User,
        kind: InputKind::Compaction,
        delivery: Delivery::NextRequest,
        at: UnixMs(at),
    }
}

/// The decision as an idle, uninterrupted agent asks it: the model made the
/// calls it is waiting on at 0, and said nothing about when to look at them.
/// Each test varies only what it is about.
#[derive(Clone)]
struct Ask {
    sources: Vec<SourceKind>,
    turn: Option<ModelTurn>,
    inference_active: bool,
    standing: Standing,
}

/// Every call listed is one the model asked for at 0 and is still waiting on;
/// [`Ask::called`] is how a test says otherwise.
fn ask(sources: Vec<SourceKind>) -> Ask {
    let waiting_on = sources
        .iter()
        .filter_map(|source| match source {
            SourceKind::Tool { id, .. } => Some(id.clone()),
            SourceKind::User { .. } | SourceKind::Mail { .. } => None,
        })
        .collect();
    Ask {
        sources,
        turn: Some(ModelTurn {
            spoke_at: UnixMs(0),
            asked: ModelAsked::Calls,
            waiting_on,
        }),
        inference_active: false,
        standing: Standing::Normal,
    }
}

impl Ask {
    fn boundary(&self, now: UnixMs) -> Boundary {
        boundary(
            &self.sources,
            self.turn.as_ref(),
            self.inference_active,
            self.standing,
            now,
        )
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
        self.turn = self.turn.map(|turn| ModelTurn {
            spoke_at: UnixMs(at),
            asked: ModelAsked::Nothing,
            ..turn
        });
        self
    }

    /// The model asked for exactly these calls at this instant, so anything
    /// else it called is something it has moved past — and an empty list is a
    /// model waiting on nothing at all, whatever is still running.
    fn called(mut self, at: u64, calls: &[&str]) -> Self {
        self.turn = Some(ModelTurn {
            spoke_at: UnixMs(at),
            asked: ModelAsked::Calls,
            waiting_on: calls.iter().map(|id| call_id(id)).collect(),
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

fn call_id(id: &str) -> ToolCallId {
    ToolCallId::try_from(id).unwrap()
}

fn tool(id: &str, told: Told, activity: ToolActivity, unsent: Unsent) -> SourceKind {
    SourceKind::Tool {
        id: call_id(id),
        told,
        activity,
        unsent,
    }
}

/// Called and working, and has not produced a byte.
fn silent_call(id: &str) -> SourceKind {
    tool(id, Told::Nothing, ToolActivity::Running, Unsent::Nothing)
}

/// Called and working, holding output it is in the middle of.
fn partial_call(id: &str, since: u64) -> SourceKind {
    tool(
        id,
        Told::Nothing,
        ToolActivity::Running,
        Unsent::Waiting {
            since: UnixMs(since),
        },
    )
}

/// Called and working, holding output it says stands on its own.
fn settled_call(id: &str, since: u64) -> SourceKind {
    tool(
        id,
        Told::Nothing,
        ToolActivity::Running,
        Unsent::Settled {
            since: UnixMs(since),
        },
    )
}

/// Ended at `at`, with the model not yet told.
fn ended_call(id: &str, at: u64) -> SourceKind {
    tool(
        id,
        Told::Nothing,
        ToolActivity::Exited { at: UnixMs(at) },
        Unsent::Nothing,
    )
}

/// Ended, and the model has been told so. Nothing will ever be said about it
/// again; it is only still here because it has not been reaped yet.
fn spent_call(id: &str) -> SourceKind {
    tool(
        id,
        Told::Exit,
        ToolActivity::Exited { at: UnixMs(0) },
        Unsent::Nothing,
    )
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
    let schedule = ask(vec![partial_call("build", 2_000)]);
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
    let schedule = ask(vec![silent_call("rg")]);
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
    let building = ask(vec![silent_call("build")]);
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
    assert_eq!(
        ask(vec![spent_call("build")])
            .replied(0)
            .boundary(UnixMs(600_000)),
        Boundary::No { recheck: None },
        "a call already told it has ended is not something to look in on"
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
    sources.push(partial_call("build", 1_000));
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
    let schedule = ask(vec![pending_user(typed_at), silent_call("rg")]);
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
    schedule.inference_active = true;
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
    let mut schedule = ask(vec![
        pending_mail(0, 0),
        settled_call("call-2", 0),
        ended_call("call-1", 0),
    ]);
    schedule.inference_active = true;
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
#[test]
fn one_round_of_parallel_calls_arrives_in_one_request() {
    // Three shells called together; the quick one finishes while the others
    // have not printed a byte. A call that has been made is certain to speak,
    // so the finished result waits rather than going out alone.
    let waiting = ask(vec![
        ended_call("a", 200),
        silent_call("b"),
        silent_call("c"),
    ]);
    assert_eq!(
        waiting.recheck(UnixMs(200)),
        Some(UnixMs(millis(DEFAULT_WAIT))),
        "the model's own interval comes first, so nothing goes out alone"
    );

    // ...and the moment the last one lands they all go, without sitting out
    // either patience. This is the whole reason the decision asks whether
    // anything is still due: without it every tool call would cost ten seconds.
    let finished = ask(vec![
        ended_call("a", 200),
        ended_call("b", 3_000),
        ended_call("c", 3_100),
    ]);
    assert_eq!(finished.boundary(UnixMs(3_100)), Boundary::Now);
    assert_eq!(
        ask(vec![ended_call("a", 100)]).boundary(UnixMs(100)),
        Boundary::Now,
        "a single fast call is not made to wait for company that is not coming"
    );
}

//  ms      model        rg a      rg b      boundary
//  0       calls both   call      call      hold
//  200                  exit                hold — b might still speak
//  10000                                    SEND — the model's own interval
//
//  ...and with the model having asked for longer:
//  0       wait(300)
//  10200                                    SEND — but not a whole wait
#[test]
fn a_call_that_never_speaks_does_not_hold_a_finished_sibling_forever() {
    let schedule = ask(vec![ended_call("a", 200), silent_call("b")]);
    assert_eq!(
        schedule.recheck(UnixMs(200)),
        Some(UnixMs(millis(DEFAULT_WAIT)))
    );

    // TOOL_PATIENCE is only visible once the model has asked to be left alone
    // for longer than it: a finished result still will not wait forever, and
    // this is the number that says so.
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
    let schedule = ask(vec![partial_call("dev", 1_000)]);
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
//  1000    calls curl                 call         hold — dev is left behind
//  1500                "GET /"                     hold — and says nothing new
//  2000                               exit         SEND — dev is not due
#[test]
fn a_call_the_model_moved_past_does_not_hold_up_the_one_it_is_on() {
    // Without this, curl's result would sit for a full TOOL_PATIENCE waiting
    // for a dev server that is never going to finish — and the wait for a tool
    // that never ends is a wait nobody can end.
    let schedule =
        ask(vec![partial_call("dev", 1_500), ended_call("curl", 2_000)]).called(1_000, &["curl"]);
    assert_eq!(schedule.boundary(UnixMs(2_000)), Boundary::Now);

    // While the model is still on it, the same two sources wait for each other.
    let still_on_it = ask(vec![silent_call("dev"), ended_call("curl", 2_000)]);
    assert_eq!(
        still_on_it.recheck(UnixMs(2_000)),
        Some(UnixMs(millis(DEFAULT_WAIT))),
        "waiting for it, until the model is looked in on"
    );
}

//  ms      model         npm run dev    boundary
//  0       calls dev     call           hold
//  1000    calls rg      "GET /"        hold — nobody asked for this
//  61500                                SEND — but it is not left unsent
// forever
#[test]
fn a_call_the_model_moved_past_is_never_itself_a_reason_to_send() {
    // A minute is the longest anything sits unsent, and it is the only thing
    // plain output from a tool nobody is waiting on ever buys. It cannot
    // shorten a wait for anybody: there is no impatience here, just a sweep.
    let schedule = ask(vec![partial_call("dev", 1_500)])
        .called(1_000, &["rg"])
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
    with_result.sources.push(ended_call("rg", 2_000));
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
    let schedule = ask(vec![partial_call("test", 2_000)]).replied(1_500);
    assert_eq!(
        schedule.recheck(UnixMs(2_000)),
        Some(UnixMs(2_000 + millis(PROGRESS_PATIENCE))),
        "no look-in, because it asked for none"
    );

    // ...and it is still the call the model is waiting on, so a sibling that
    // finishes waits for it rather than going out alone.
    let with_sibling =
        ask(vec![partial_call("test", 2_000), ended_call("rg", 2_500)]).replied(1_500);
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
    let waiting = ask(Vec::new())
        .called(0, &["dev", "test"])
        .replied(1_000)
        .waiting(300);

    let mut panicked = waiting.clone();
    panicked.sources = vec![settled_call("dev", 60_000)];
    assert_eq!(
        panicked.boundary(UnixMs(60_000)),
        Boundary::Now,
        "flagged output does not sit out the wait it was flagged during"
    );

    // With a sibling still running it takes the ordinary collecting window
    // first, so the two arrive together rather than a request apiece.
    let mut with_sibling = panicked.clone();
    with_sibling.sources.push(silent_call("test"));
    assert_eq!(
        with_sibling.recheck(UnixMs(60_000)),
        Some(UnixMs(60_000 + millis(TOOL_PATIENCE))),
        "but it waits for company, which is not the same as waiting out a wait"
    );

    let mut ended = waiting.clone();
    ended.sources = vec![tool(
        "dev",
        Told::Nothing,
        ToolActivity::Exited { at: UnixMs(60_100) },
        Unsent::Settled {
            since: UnixMs(60_000),
        },
    )];
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
//  65000                               SEND — but not for a whole wait, either
//  301000                              (the wait's own end, had it got there)
#[test]
fn a_wait_is_worth_at_most_a_minute_of_quiet_while_a_tool_is_talking() {
    // The one number a tool's plain output still buys. It cannot fire while the
    // model is being looked in on every DEFAULT_WAIT, so it only bites once the
    // model has asked for a longer interval than a minute.
    let schedule = ask(vec![partial_call("dev", 5_000)])
        .replied(1_000)
        .waiting(300);
    assert_eq!(
        schedule.recheck(UnixMs(5_000)),
        Some(UnixMs(5_000 + millis(PROGRESS_PATIENCE)))
    );
    assert_eq!(
        schedule.boundary(UnixMs(5_000 + millis(PROGRESS_PATIENCE))),
        Boundary::Now
    );

    // With nothing to show, the wait runs its full length.
    let quiet = ask(vec![silent_call("dev")]).replied(1_000).waiting(300);
    assert_eq!(quiet.recheck(UnixMs(5_000)), Some(UnixMs(301_000)));
    assert_eq!(quiet.boundary(UnixMs(301_000)), Boundary::Now);
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
    // The model moved on from dev long ago, so nothing it *says* would reach
    // anybody. Flagging is how it says the difference.
    let crashed = ask(vec![settled_call("dev", 60_000)])
        .called(1_000, &[])
        .waiting(300);
    assert_eq!(crashed.boundary(UnixMs(60_000)), Boundary::Now);

    let typed = ask(vec![silent_call("dev"), pending_user(60_000)])
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
    let schedule = ask(vec![silent_call("rg")]).waiting(300);
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
    let schedule = ask(vec![silent_call("rg"), pending_mail(5_000, 5_000)]);
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
    let waiting = vec![pending_mail(1_000, 1_000), silent_call("rg")];
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
    for settled in [pending_user(0), ended_call("call-1", 0)] {
        let schedule = ask(vec![settled]);
        assert_eq!(schedule.boundary(UnixMs(0)), Boundary::Now);
        assert_eq!(schedule.recheck(UnixMs(0)), None);
    }
}

//  ms      boundary
//  0       SEND — a retry or a resume, with nothing pending at all
#[test]
fn a_must_send_standing_sends_even_with_nothing_pending() {
    let mut schedule = ask(Vec::new());
    schedule.standing = Standing::MustSend;
    assert_eq!(schedule.boundary(UnixMs(0)), Boundary::Now);
}

//  ms      call-1    boundary
//  0       exit      SEND
//  0       exit      cancelled: hold, no timer — its words land later
#[test]
fn a_cancelled_agent_is_not_woken_by_its_tools_dying_words() {
    let mut schedule = ask(vec![ended_call("call-1", 0)]);
    assert_eq!(
        schedule.boundary(UnixMs(0)),
        Boundary::Now,
        "an exited tool would normally go at once"
    );

    schedule.standing = Standing::Halted;
    assert_eq!(
        schedule.boundary(UnixMs(0)),
        Boundary::No { recheck: None },
        "and no timer either, so its own output cannot wake it"
    );

    // Not even the check-in, which is the one thing that fires with nothing to
    // send: a cancelled agent must stay stopped until a person says otherwise.
    let mut running = ask(vec![silent_call("call-1")]);
    running.standing = Standing::Halted;
    assert_eq!(running.recheck(UnixMs(0)), None);
}

#[test]
fn no_timer_is_armed_while_a_request_is_in_flight() {
    let mut schedule = ask(vec![pending_user(0), partial_call("call-1", 0)]);
    schedule.inference_active = true;
    assert_eq!(schedule.recheck(UnixMs(0)), None);
}

#[test]
fn the_loop_can_never_sleep_past_a_boundary() {
    // A recheck is always strictly ahead of now, and once the moment arrives
    // the answer is Now rather than another wait — so the loop that sleeps
    // until the recheck always wakes to a decision it can act on.
    for sources in [
        vec![silent_call("a")],
        vec![partial_call("a", 0)],
        vec![ended_call("a", 0), silent_call("b")],
        vec![pending_mail(0, 0)],
        vec![pending_user(0), silent_call("a")],
    ] {
        let schedule = ask(sources);
        let Some(recheck) = schedule.recheck(UnixMs(0)) else {
            continue;
        };
        assert!(recheck > UnixMs(0));
        assert_eq!(schedule.boundary(recheck), Boundary::Now);
    }
}

//  ms      cargo test    boundary
//  0       call          hold — before the model has ever spoken
#[test]
fn a_call_nobody_is_waiting_on_yet_is_not_due() {
    // Only reachable through a restart, which loses its tools anyway. Pinned
    // because "no turn recorded" must not read as "the model is waiting on
    // everything" — that would be a wait with no way to end it.
    let mut schedule = ask(vec![silent_call("test"), ended_call("rg", 200)]);
    schedule.turn = None;
    assert_eq!(
        schedule.boundary(UnixMs(200)),
        Boundary::Now,
        "nothing is due, so what is finished goes"
    );

    let mut alone = ask(vec![silent_call("test")]);
    alone.turn = None;
    assert_eq!(
        alone.boundary(UnixMs(600_000)),
        Boundary::No { recheck: None },
        "and with nobody holding anything there is still no request"
    );
}

// -- draining sources -------------------------------------------------------

#[test]
fn user_drain_is_total_and_ordered() {
    let mut user = UserSource::default();
    user.push(user_message("first", Delivery::NextRequest, 1));
    user.push(user_message("second", Delivery::Interrupt, 2));

    let blocks = user.take();
    assert_eq!(blocks.len(), 2);
    assert!(user.is_empty(), "no item is ever left behind");
    assert_eq!(
        blocks[0],
        ContextBlock::UserMessage {
            sender: rho_core::MessageSender::User,
            content: vec![ContentPart::Text {
                text: "first".to_owned()
            }],
        }
    );
}

#[test]
fn compaction_lands_after_everything_typed_beside_it() {
    let mut user = UserSource::default();
    user.push(compaction(1));
    user.push(user_message("and then do this", Delivery::NextRequest, 2));

    let blocks = user.take();
    assert_eq!(
        blocks.last(),
        Some(&ContextBlock::CompactionTrigger),
        "history has to agree with the request it produced"
    );
    assert_eq!(blocks.len(), 2);
}

#[test]
fn mail_from_one_peer_collapses_into_a_single_block() {
    let mut mail = MailSource::new(peer(1));
    mail.push(
        vec![ContentPart::Text {
            text: "a".to_owned(),
        }],
        UnixMs(10),
    );
    mail.push(
        vec![ContentPart::Text {
            text: "b".to_owned(),
        }],
        UnixMs(20),
    );

    let ContextBlock::UserMessage { sender, content } = mail.take().unwrap() else {
        panic!("expected a user message block")
    };
    assert_eq!(sender, rho_core::MessageSender::Agent { id: peer(1) });
    assert_eq!(content.len(), 2, "one block, both parts");
    assert!(mail.is_empty());
}

// -- replay -----------------------------------------------------------------

#[test]
fn accepted_input_survives_a_restart_that_never_delivered_it() {
    let restored = restore(vec![AgentEvent::Queued(user_input(
        Delivery::NextRequest,
        10,
    ))]);
    let mut restored = restored;
    assert_eq!(restored.user.take().len(), 1);
    assert!(restored.history.is_empty());
}

#[test]
fn a_drain_empties_the_queues_it_touched() {
    let restored = restore(vec![
        AgentEvent::Queued(user_input(Delivery::NextRequest, 10)),
        AgentEvent::Queued(QueuedInput {
            source: InputSource::Mail { sender: peer(1) },
            kind: InputKind::Message {
                content: vec![ContentPart::Text {
                    text: "from a peer".to_owned(),
                }],
            },
            delivery: Delivery::NextRequest,
            at: UnixMs(11),
        }),
        AgentEvent::Appended {
            blocks: Cow::Owned(vec![ContextBlock::CompactionTrigger]),
            drained: true,
        },
    ]);

    assert_eq!(restored.history.len(), 1);
    assert!(restored.user.is_empty());
    assert!(restored.mail[&peer(1)].is_empty());
}

#[test]
fn a_tool_still_running_at_shutdown_is_reported_as_lost() {
    let restored = restore(vec![
        AgentEvent::ToolSpawned {
            call: Cow::Owned(call("call-1")),
        },
        AgentEvent::ToolSpawned {
            call: Cow::Owned(call("call-2")),
        },
        AgentEvent::ToolReaped {
            call_id: ToolCallId::try_from("call-1").unwrap(),
        },
    ]);

    // Only the one that never finished; a reaped tool is genuinely done.
    assert_eq!(restored.orphan_tools.len(), 1);
    assert_eq!(restored.orphan_tools[0].id.as_str(), "call-2");
}

#[test]
fn a_lost_tool_is_admitted_to_at_the_next_request_not_at_load() {
    // Loading an agent to look at it must not write to its transcript; the note
    // is only worth making when there is a request to put it in.
    let restored = restore(vec![
        AgentEvent::ToolSpawned {
            call: Cow::Owned(call("call-1")),
        },
        AgentEvent::RequestEnded { context_used: None },
    ]);

    assert!(restored.history.is_empty(), "replay itself appends nothing");
    assert_eq!(restored.orphan_tools.len(), 1, "but the call is remembered");
}

#[test]
fn an_unanswered_lost_tool_gets_a_result_and_an_answered_one_gets_an_update() {
    let now = UnixMs(1_000);
    assert!(matches!(
        lost_to_restart(&call("call-1"), false, now),
        ContextBlock::ToolResults { .. }
    ));
    assert!(matches!(
        lost_to_restart(&call("call-1"), true, now),
        ContextBlock::ToolUpdate(_)
    ));
}

#[test]
fn a_request_cut_short_by_shutdown_resumes() {
    let restored = restore(vec![
        AgentEvent::Queued(user_input(Delivery::NextRequest, 10)),
        AgentEvent::Appended {
            blocks: Cow::Owned(vec![ContextBlock::UserMessage {
                sender: rho_core::MessageSender::User,
                content: vec![ContentPart::Text {
                    text: "hello".to_owned(),
                }],
            }]),
            drained: true,
        },
        AgentEvent::RequestStarted,
    ]);

    assert!(restored.request_active, "resumes on load");
    assert!(restored.user.is_empty(), "the message already landed");
}

#[test]
fn cancelling_drops_queued_input_durably() {
    let restored = restore(vec![
        AgentEvent::Queued(user_input(Delivery::NextRequest, 10)),
        AgentEvent::QueueCleared,
    ]);
    assert!(restored.user.is_empty());
}

#[test]
fn a_bare_compact_stops_there_and_anything_alongside_it_carries_on() {
    let trigger = [ContextBlock::CompactionTrigger];
    assert!(
        !compaction_owes_reply(false, &trigger),
        "the user asked for a compaction and got one"
    );

    let with_message = [
        ContextBlock::UserMessage {
            sender: rho_core::MessageSender::User,
            content: vec![ContentPart::Text {
                text: "and then do this".to_owned(),
            }],
        },
        ContextBlock::CompactionTrigger,
    ];
    assert!(
        compaction_owes_reply(false, &with_message),
        "the message is inside the summary now, not answered"
    );

    assert!(
        compaction_owes_reply(true, &trigger),
        "a compaction the core asked for displaced real work"
    );
}

// -- request assembly -------------------------------------------------------

#[test]
fn compaction_is_not_re_triggered_within_the_same_request() {
    let history = vec![
        Arc::new(ContextBlock::CompactionTrigger),
        Arc::new(ContextBlock::UserMessage {
            sender: rho_core::MessageSender::User,
            content: Vec::new(),
        }),
    ];
    assert!(latest_request_has_compaction_trigger(&history));

    // A response since the trigger closes that request out.
    let history = vec![
        Arc::new(ContextBlock::CompactionTrigger),
        Arc::new(ContextBlock::InferenceResponse {
            items: Vec::new(),
            provider_response_id: None,
        }),
    ];
    assert!(!latest_request_has_compaction_trigger(&history));
}

#[test]
fn a_call_has_its_result_once_one_exists_in_history() {
    let history = vec![Arc::new(ContextBlock::ToolResults {
        results: vec![ToolResult {
            call_id: ToolCallId::try_from("call-1").unwrap(),
            tool_type: ToolType::Function,
            body: ToolOutput {
                output: Arc::new("done".to_owned()),
                status: ToolOutputStatus::Success,
            },
            started_at: UnixMs(0),
            finished_at: UnixMs(1),
            metadata: None,
        }],
    })];

    assert!(result_sent(
        &history,
        &ToolCallId::try_from("call-1").unwrap()
    ));
    assert!(!result_sent(
        &history,
        &ToolCallId::try_from("call-2").unwrap()
    ));
}

// -- the numbers themselves -------------------------------------------------

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

// -- storage ----------------------------------------------------------------

#[tokio::test]
async fn events_round_trip_through_the_store() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("agent2.redb"));
    let record = AgentRecord {
        instructions: "be useful".to_owned(),
        profile: InferenceProfile::default(),
        model: crate::store::PersistedModel::Gpt56Sol,
        prompt_cache_key: PromptCacheKey::generate(),
        next_event: 0,
    };
    let id = store.create_record(&record).await;

    let written = [
        AgentEvent::Queued(user_input(Delivery::Interrupt, 10)),
        AgentEvent::ToolSpawned {
            call: Cow::Owned(call("call-1")),
        },
        AgentEvent::Appended {
            blocks: Cow::Owned(vec![ContextBlock::CompactionTrigger]),
            drained: true,
        },
        AgentEvent::RequestEnded {
            context_used: Some(1_234),
        },
    ];
    for (sequence, event) in written.iter().enumerate() {
        store.append(id, sequence as u64, event).await;
    }

    let (loaded, events) = store.load(id).unwrap();
    assert_eq!(loaded.instructions, "be useful");
    assert_eq!(loaded.next_event, written.len() as u64);
    assert_eq!(events, written, "every event survives the encoder verbatim");

    // And the restored state is what those events describe.
    let restored = restore(events);
    assert_eq!(restored.history.len(), 1);
    assert!(restored.user.is_empty(), "the queue was drained");
    assert_eq!(restored.context_used, Some(1_234));
    assert_eq!(restored.orphan_tools.len(), 1, "the tool never finished");
}

// -- running tools ----------------------------------------------------------

/// Accumulates on its own and hands everything over when asked — the shape
/// every real tool has, since the core pulls rather than being pushed to.
#[derive(Default)]
struct FakeSession {
    unsent: String,
    unsent_since: UnixMs,
    last_output_at: UnixMs,
    exited: Option<UnixMs>,
    cancels: u32,
}

impl FakeSession {
    fn tool_status(&self) -> ToolStatus {
        ToolStatus {
            unsent: if self.unsent.is_empty() {
                Unsent::Nothing
            } else {
                Unsent::Waiting {
                    since: self.unsent_since,
                }
            },
            activity: match self.exited {
                Some(at) => ToolActivity::Exited { at },
                None => ToolActivity::Running,
            },
            last_output_at: self.last_output_at,
        }
    }

    fn produce(&mut self, text: &str, at: UnixMs) {
        if self.unsent.is_empty() {
            self.unsent_since = at;
        }
        self.unsent.push_str(text);
        self.last_output_at = at;
    }
}

impl ToolSession for FakeSession {
    fn status(&self) -> ToolStatus {
        self.tool_status()
    }

    fn take_output(&mut self) -> Option<ToolOutput> {
        let text = std::mem::take(&mut self.unsent);
        (!text.is_empty()).then(|| ToolOutput {
            output: Arc::new(text),
            status: ToolOutputStatus::Success,
        })
    }

    fn cancel(&mut self) {
        self.cancels += 1;
        self.exited = Some(self.last_output_at);
    }
}

/// A session the test can keep poking after handing it to the core.
#[derive(Clone, Default)]
struct SharedSession(Arc<std::sync::Mutex<FakeSession>>);

impl SharedSession {
    fn produce(&self, text: &str, at: UnixMs) {
        self.0.lock().unwrap().produce(text, at);
    }

    fn cancels(&self) -> u32 {
        self.0.lock().unwrap().cancels
    }

    /// Finish without being asked to, the way a real tool ends on its own.
    fn exit(&self, at: UnixMs) {
        self.0.lock().unwrap().exited = Some(at);
    }
}

impl ToolSession for SharedSession {
    fn status(&self) -> ToolStatus {
        self.0.lock().unwrap().status()
    }

    fn take_output(&mut self) -> Option<ToolOutput> {
        self.0.lock().unwrap().take_output()
    }

    fn cancel(&mut self) {
        self.0.lock().unwrap().cancel();
    }
}

fn running(id: &str, session: SharedSession) -> (ToolCallId, RunningTool) {
    let call = call(id);
    (
        call.id.clone(),
        RunningTool::new(call, Box::new(session), UnixMs(0)),
    )
}

#[test]
fn first_take_answers_the_call_and_later_takes_annotate_it() {
    let session = SharedSession::default();
    let (_, mut tool) = running("call-1", session.clone());
    session.produce("one", UnixMs(10));

    let Some(ToolTake::Result(result)) = tool.take(UnixMs(20)) else {
        panic!("first take must answer the call")
    };
    assert_eq!(*result.body.output, "one");

    // The same tool produces more later; a provider accepts only one result
    // per call, so this has to arrive as an update.
    session.produce("two", UnixMs(30));
    let Some(ToolTake::Update(update)) = tool.take(UnixMs(40)) else {
        panic!("later takes must annotate")
    };
    assert_eq!(*update.output, "two");
}

#[test]
fn a_tool_that_exits_silently_still_owes_a_result() {
    let session = SharedSession::default();
    let (_, mut tool) = running("call-1", session.clone());
    session.exit(UnixMs(5));

    assert!(!tool.reapable(), "the call is unanswered");
    assert_eq!(
        ask(vec![tool.source()]).boundary(UnixMs(10)),
        Boundary::Now,
        "and a bare result is worth a request"
    );

    assert!(matches!(tool.take(UnixMs(10)), Some(ToolTake::Result(_))));
    assert!(tool.reapable(), "answered and exited, safe to forget");
}

#[test]
fn a_running_tool_with_nothing_to_say_is_no_reason_to_send() {
    let (_, mut tool) = running("call-1", SharedSession::default());
    // It is still a source — it was called, so it will speak — but nothing
    // about it causes a request. The only timer here is the model's own.
    assert_eq!(
        ask(vec![tool.source()]).recheck(UnixMs(10)),
        Some(UnixMs(millis(DEFAULT_WAIT)))
    );
    assert_eq!(
        ask(vec![tool.source()])
            .replied(600)
            .boundary(UnixMs(10_000)),
        Boundary::No { recheck: None },
        "and once the model has asked for nothing, not even that"
    );
    assert!(tool.take(UnixMs(10)).is_none());
    assert_eq!(tool.told, Told::Nothing, "and its call stays open");
}

#[test]
fn a_tool_that_exits_at_once_pays_no_wait() {
    let session = SharedSession::default();
    let (_, tool) = running("call-1", session.clone());
    session.produce("done", UnixMs(1_000));
    session.exit(UnixMs(1_000));

    let schedule = ask(vec![tool.source()]);
    assert_eq!(
        schedule.boundary(UnixMs(1_000)),
        Boundary::Now,
        "no five second tax on a finished tool"
    );
}

//  ms      build     boundary
//  0       call      hold — nothing to send
//  1000    out       hold — until the model is looked in on
//  10000             SEND — its one result
//  200000  out ...   held: everything after the result is an update
//  400000  exit      SEND — the end is news
#[test]
fn a_call_hands_over_what_it_has_once_and_then_waits_for_the_end() {
    let session = SharedSession::default();
    let (_, mut tool) = running("call-1", session.clone());
    session.produce("compiling...", UnixMs(1_000));

    let floor = millis(DEFAULT_WAIT);
    assert!(matches!(
        ask(vec![tool.source()]).boundary(UnixMs(floor - 1)),
        Boundary::No { .. }
    ));
    assert_eq!(
        ask(vec![tool.source()]).boundary(UnixMs(floor)),
        Boundary::Now
    );
    assert!(matches!(
        tool.take(UnixMs(floor)),
        Some(ToolTake::Result(_))
    ));

    // Answered, and by 200_000 the model has long since gone on to something
    // else, so nothing it says is worth a request of its own — only the sweep
    // that stops output sitting unsent forever.
    session.produce("still compiling...", UnixMs(200_000));
    let moved_on = ask(vec![tool.source()]).replied(150_000);
    assert_eq!(
        moved_on.recheck(UnixMs(200_000)),
        Some(UnixMs(200_000 + millis(PROGRESS_PATIENCE)))
    );

    // ...until it ends, which is news whatever it has to say.
    session.exit(UnixMs(400_000));
    assert_eq!(
        ask(vec![tool.source()]).boundary(UnixMs(400_000)),
        Boundary::Now
    );
}

//  ms      model       call-1    call-2    boundary
//  0       wait(300)   call      call      hold
//  1000                out                 hold — mid-thought
//  40000               exit                hold — ready, and waits from here
//  50000                                   SEND
#[test]
fn a_tool_that_ends_after_a_long_run_still_waits_for_its_siblings() {
    let session = SharedSession::default();
    let (_, tool) = running("call-1", session.clone());
    session.produce("...", UnixMs(1_000));
    session.exit(UnixMs(40_000));

    // Dating the wait from the output rather than from the ending would leave
    // it looking thirty-nine seconds overdue, and it would walk out on the
    // sibling it ought to be leaving with.
    let schedule = ask(vec![tool.source(), partial_call("call-2", 39_000)]).waiting(300);
    let waits_until = 40_000 + millis(TOOL_PATIENCE);
    assert_eq!(
        schedule.boundary(UnixMs(40_000)),
        Boundary::No {
            recheck: Some(UnixMs(waits_until))
        }
    );
    assert_eq!(schedule.boundary(UnixMs(waits_until)), Boundary::Now);
}

#[test]
fn a_running_tool_can_say_its_output_stands_on_its_own() {
    /// Reports everything it holds as settled — what a long-lived process does
    /// when it flags a line as worth reading now rather than eventually.
    struct Flagged(SharedSession);

    impl ToolSession for Flagged {
        fn status(&self) -> ToolStatus {
            let status = self.0.status();
            ToolStatus {
                unsent: match status.unsent {
                    // Flagging says something about output it is holding, so a
                    // tool with nothing to hand over still says so.
                    Unsent::Nothing => Unsent::Nothing,
                    Unsent::Waiting { since } | Unsent::Settled { since } => {
                        Unsent::Settled { since }
                    }
                },
                ..status
            }
        }

        fn take_output(&mut self) -> Option<ToolOutput> {
            self.0.take_output()
        }

        fn cancel(&mut self) {
            self.0.cancel();
        }
    }

    let session = SharedSession::default();
    session.produce("PANIC: disk full", UnixMs(1_000));
    let tool = RunningTool::new(
        call("call-1"),
        Box::new(Flagged(session.clone())),
        UnixMs(0),
    );

    let schedule = ask(vec![tool.source()]);
    assert_eq!(
        schedule.boundary(UnixMs(1_000)),
        Boundary::Now,
        "a line that matters does not sit out the burst gap"
    );
    assert_eq!(
        tool.status().activity,
        ToolActivity::Running,
        "and the tool carries on regardless"
    );
}

#[test]
fn answering_a_call_does_not_stop_the_model_waiting_on_it() {
    let session = SharedSession::default();
    let (_, mut tool) = running("call-1", session.clone());
    session.produce("Compiling foo v0.1.0", UnixMs(500));

    // A person types, and their patience ends the wait for everyone.
    assert_eq!(
        ask(vec![pending_user(1_000), tool.source()]).boundary(UnixMs(1_500)),
        Boundary::Now
    );
    assert!(matches!(
        tool.take(UnixMs(1_500)),
        Some(ToolTake::Result(_))
    ));
    assert_eq!(tool.told, Told::Result);

    // Writing that result is what the provider demands, not a statement that
    // anyone stopped waiting. Only the model's own next turn says that, so the
    // five minute test run keeps its place until the model moves on from it.
    session.produce("test foo ... ok", UnixMs(2_000));
    let after = ask(vec![tool.source(), ended_call("call-2", 2_500)]).replied(1_500);
    assert_eq!(
        after.recheck(UnixMs(2_500)),
        Some(UnixMs(2_500 + millis(TOOL_PATIENCE))),
        "a person typing must not demote somebody else's call"
    );
}

//  ms      dev       boundary
//  0       call      hold
//  200     out       answered, and past its floor: a background job
//  40000   exit      SEND — an ending is news, answered or not
#[test]
fn an_answered_call_that_ends_still_says_so() {
    let session = SharedSession::default();
    let (_, mut tool) = running("call-1", session.clone());
    session.produce("listening on :3000", UnixMs(100));
    assert!(matches!(tool.take(UnixMs(200)), Some(ToolTake::Result(_))));
    assert_eq!(
        ask(vec![tool.source()])
            .replied(300)
            .boundary(UnixMs(100_000)),
        Boundary::No { recheck: None },
        "nothing was asked for, and it is holding nothing anyway"
    );

    // Hours of logs later it dies holding nothing. Without this the model would
    // never learn the server stopped: the call would simply be forgotten.
    session.exit(UnixMs(40_000));
    assert_eq!(
        ask(vec![tool.source()]).boundary(UnixMs(40_000)),
        Boundary::Now
    );
    let Some(ToolTake::Update(update)) = tool.take(UnixMs(40_000)) else {
        panic!("the ending has to reach the model")
    };
    assert!(update.output.contains("exited"));
    assert!(tool.reapable(), "and only then is it safe to forget");
}

#[test]
fn the_core_never_sees_output_it_did_not_ask_for() {
    let session = SharedSession::default();
    let (_, mut tool) = running("call-1", session.clone());

    // Minutes of chatter accumulate inside the tool, costing the core nothing.
    for line in 0..1_000 {
        session.produce(&format!("line {line}\n"), UnixMs(line));
    }
    assert!(
        matches!(tool.status().unsent, Unsent::Waiting { .. }),
        "the core knows there is something, not what"
    );

    // One pull, one block: the tool chose how to represent all of it.
    let Some(ToolTake::Result(result)) = tool.take(UnixMs(2_000)) else {
        panic!("expected a result")
    };
    assert!(result.body.output.starts_with("line 0"));
    assert_eq!(
        tool.status().unsent,
        Unsent::Nothing,
        "nothing left buffered anywhere"
    );
}

#[test]
fn cancel_reaches_the_tool_and_lets_it_have_the_last_word() {
    let session = SharedSession::default();
    let (_, mut tool) = running("call-1", session.clone());

    tool.cancel();
    assert_eq!(session.cancels(), 1);

    // Winding down produced a parting note, which still reaches the model.
    session.produce("cleaned up", UnixMs(10));
    assert!(matches!(
        tool.status().activity,
        ToolActivity::Exited { .. }
    ));
    let Some(ToolTake::Result(result)) = tool.take(UnixMs(20)) else {
        panic!("the call must still be answered")
    };
    assert_eq!(*result.body.output, "cleaned up");
    assert!(tool.reapable(), "and then it is forgotten");
}

#[tokio::test]
async fn a_wake_that_lands_while_the_core_is_busy_is_not_lost() {
    let notify = Arc::new(Notify::new());
    let waker = SourceWaker::new(Arc::clone(&notify));

    // The tool signals before anyone is listening.
    waker.wake();

    let woken = tokio::time::timeout(Duration::from_millis(50), notify.notified());
    assert!(woken.await.is_ok(), "the permit survives until awaited");
}

#[test]
fn elide_middle_keeps_both_ends() {
    let text = "x".repeat(1_000);
    let elided = elide_middle(&text, 100);
    assert!(elided.len() < 200);
    assert!(elided.contains("bytes elided"));
    assert_eq!(elide_middle("short", 100), "short");
}

// -- previews ---------------------------------------------------------------

#[test]
fn a_tool_can_show_whatever_it_likes_in_a_preview() {
    let session = SharedSession::default();
    session.produce("hello", UnixMs(5));

    // The default names the call it belongs to, so a flat list of previews
    // needs no parallel labelling.
    let data = session.preview(&call("call-1"));
    let default = data
        .as_any()
        .downcast_ref::<ToolPreview>()
        .expect("default tool preview");
    assert_eq!(default.call_id.as_str(), "call-1");
    assert_eq!(default.activity, ToolActivity::Running);
    assert_eq!(default.unsent, Unsent::Waiting { since: UnixMs(5) });
    assert_eq!(default.last_output_at, UnixMs(5));

    // ...and a queue preview is a different type behind the same field, which
    // is the point of making it open.
    let queued: Box<dyn PreviewData> = Box::new(UserPreview {
        items: vec![PendingItem {
            at: UnixMs(7),
            text: "hello".to_owned(),
        }],
    });
    assert!(queued.as_any().downcast_ref::<ToolPreview>().is_none());
    assert_eq!(
        queued.as_any().downcast_ref::<UserPreview>().unwrap().items[0].at,
        UnixMs(7)
    );
}
