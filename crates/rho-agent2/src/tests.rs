use std::time::Duration;

use rho_core::{ContentPart, ToolType, UnknownProviderSpecificData};

use super::*;
use crate::boundary::{
    DEFAULT_WAIT, MAIL_BURST, MAIL_PATIENCE, PROGRESS_PATIENCE, TOOL_PATIENCE, USER_PATIENCE,
};

fn call(id: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from(id).unwrap(),
        name: ToolName::try_from("shell").unwrap(),
        tool_type: ToolType::Function,
        arguments: "{}".to_owned(),
    }
}

/// A turn in which the model made exactly these calls.
fn called_blocks(ids: &[&str]) -> Vec<ContextBlock> {
    vec![ContextBlock::InferenceResponse {
        items: ids
            .iter()
            .map(|id| InferenceResponseItem::ToolCall {
                provider_specific: Box::new(UnknownProviderSpecificData {
                    tag: "test".to_owned(),
                }),
                id: ToolCallId::try_from(*id).unwrap(),
                name: ToolName::try_from("shell").unwrap(),
                tool_type: ToolType::Function,
                arguments: "{}".to_owned(),
            })
            .collect(),
        provider_response_id: None,
    }]
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
        SourceKind::User { .. } | SourceKind::Mail { .. } => panic!("only a call has an answer"),
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
#[test]
fn one_round_of_parallel_calls_arrives_in_one_request() {
    // Three shells called together; the quick one finishes while the others
    // have not printed a byte. A call that has been made is certain to speak,
    // so the finished result waits rather than going out alone.
    let waiting = ask(vec![ended_call(200), silent_call(), silent_call()]);
    assert_eq!(
        waiting.recheck(UnixMs(200)),
        Some(UnixMs(millis(DEFAULT_WAIT))),
        "the model's own interval comes first, so nothing goes out alone"
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
//  10000                                    SEND — the model's own interval
//
//  ...and with the model having asked for longer:
//  0       wait(300)
//  10200                                    SEND — but not a whole wait
#[test]
fn a_call_that_never_speaks_does_not_hold_a_finished_sibling_forever() {
    let schedule = ask(vec![ended_call(200), silent_call()]);
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
        Some(UnixMs(millis(DEFAULT_WAIT))),
        "waiting for it, until the model is looked in on"
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
//  65000                               SEND — but not for a whole wait, either
//  301000                              (the wait's own end, had it got there)
#[test]
fn a_wait_is_worth_at_most_a_minute_of_quiet_while_a_tool_is_talking() {
    // The one number a tool's plain output still buys. It cannot fire while the
    // model is being looked in on every DEFAULT_WAIT, so it only bites once the
    // model has asked for a longer interval than a minute.
    let schedule = ask(vec![partial_call(5_000)]).replied(1_000).waiting(300);
    assert_eq!(
        schedule.recheck(UnixMs(5_000)),
        Some(UnixMs(5_000 + millis(PROGRESS_PATIENCE)))
    );
    assert_eq!(
        schedule.boundary(UnixMs(5_000 + millis(PROGRESS_PATIENCE))),
        Boundary::Now
    );

    // With nothing to show, the wait runs its full length.
    let quiet = ask(vec![silent_call()]).replied(1_000).waiting(300);
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
fn only_a_person_lifts_a_stop() {
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
        None,
        "mail is queued and waits there"
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

// -- storage ----------------------------------------------------------------

async fn new_agent(store: &Store) -> (AgentId, EventPos) {
    let (id, at, _) = store
        .create_agent(
            InferenceProfile::default(),
            crate::db::PersistedModel::Gpt56Sol,
            PromptCacheKey::generate(),
        )
        .await;
    (id, at)
}

#[tokio::test]
async fn events_round_trip_through_the_store() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("agent2.redb"));
    let (id, mut at) = new_agent(&store).await;

    let written = [
        AgentEvent::Queued(user_input(Delivery::Interrupt, 10)),
        AgentEvent::Sent {
            blocks: Cow::Owned(called_blocks(&["call-1"])),
        },
        AgentEvent::Replied {
            blocks: Cow::Owned(Vec::new()),
            context_used: Some(1_234),
        },
    ];
    for event in &written {
        at = store.append(at, event).await;
    }

    let (loaded, next, events) = store.load(id).unwrap();
    assert!(matches!(loaded.model, crate::db::PersistedModel::Gpt56Sol));
    assert_eq!(next, at, "and the log carries on where it left off");
    assert_eq!(
        events, written,
        "every event survives the encoder verbatim, in the order it was written"
    );
}

#[tokio::test]
async fn a_log_is_read_back_in_order_and_only_its_own() {
    // Loading is a range over `(lineage, seq)`, so both of these are redb's
    // ordering rather than a counted loop's: a sequence past a byte boundary,
    // and where one agent's branch stops.
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("agent2.redb"));
    let (id, mut at) = new_agent(&store).await;
    let (other, elsewhere) = new_agent(&store).await;

    for sequence in 0..300 {
        let event = AgentEvent::Replied {
            blocks: Cow::Owned(Vec::new()),
            context_used: Some(sequence),
        };
        at = store.append(at, &event).await;
    }
    store.append(elsewhere, &AgentEvent::QueueCleared).await;

    assert_eq!(counted(&store, id), (0..300).collect::<Vec<_>>());
    assert_eq!(store.load(other).unwrap().2.len(), 1);
}

//  lineage   seq   boundary
//  1         0..3  "a" "b" "c"
//  2         0..1  forked at 1, so it inherits "a" and adds "d"
#[tokio::test]
async fn a_fork_inherits_its_parent_up_to_the_branch_point_and_no_further() {
    // Rewinding is a new branch rather than a hole: what the agent walked away
    // from is still in the log, and still readable by whoever remembers it.
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("agent2.redb"));
    let (id, at) = new_agent(&store).await;

    let mut positions = vec![at];
    for sequence in 0..3 {
        let event = AgentEvent::Replied {
            blocks: Cow::Owned(Vec::new()),
            context_used: Some(sequence),
        };
        positions.push(store.append(*positions.last().unwrap(), &event).await);
    }
    assert_eq!(counted(&store, id), vec![0, 1, 2]);

    // Branch after the first event; the second and third are left behind.
    let branch = store.fork(id, positions[1]).await;
    let event = AgentEvent::Replied {
        blocks: Cow::Owned(Vec::new()),
        context_used: Some(9),
    };
    store.append(branch, &event).await;
    assert_eq!(counted(&store, id), vec![0, 9]);

    // ...and a branch off a branch inherits the whole path back to the root.
    let (_, next, _) = store.load(id).unwrap();
    store.fork(id, next).await;
    assert_eq!(counted(&store, id), vec![0, 9]);
}

/// The agent's history as the numbers its events were stamped with.
fn counted(store: &Store, id: AgentId) -> Vec<u64> {
    store
        .load(id)
        .unwrap()
        .2
        .iter()
        .map(|event| match event {
            AgentEvent::Replied { context_used, .. } => context_used.unwrap(),
            other => panic!("unexpected event: {other:?}"),
        })
        .collect()
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
