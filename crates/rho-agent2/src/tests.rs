use std::time::Duration;

use rho_core::{AgentIdDomain, ContentPart, ToolType};

use super::*;

fn peer(counter: u64) -> PeerId {
    PeerId::from_counter(counter, &AgentIdDomain(7)).unwrap()
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
    QueuedInput {
        source: InputSource::User,
        kind: InputKind::Message {
            content: vec![ContentPart::Text {
                text: "hello".to_owned(),
            }],
        },
        delivery,
        at: UnixMs(at),
    }
}

/// A schedule in which the model last spoke at t=0.
fn schedule(pending: Vec<PendingSource>) -> Schedule {
    Schedule {
        pending,
        inference_active: false,
        wants_interrupt: false,
        standing: Standing::Normal,
        last_response_at: UnixMs(0),
    }
}

fn running_tool(last_output_at: u64) -> PendingSource {
    PendingSource::talking(Rhythm::TOOL, UnixMs(last_output_at))
}

fn exited_tool() -> PendingSource {
    PendingSource::done(Rhythm::TOOL)
}

fn pending_mail(last_at: u64) -> PendingSource {
    PendingSource::talking(Rhythm::MAIL, UnixMs(last_at))
}

fn pending_user() -> PendingSource {
    PendingSource::done(Rhythm::USER)
}

// -- the one decision -------------------------------------------------------

#[test]
fn nothing_pending_means_no_request() {
    let schedule = schedule(Vec::new());
    assert_eq!(schedule.boundary(UnixMs(5_000)), Boundary::No);
    assert_eq!(schedule.next_deadline(UnixMs(5_000)), None);
}

#[test]
fn a_typed_message_is_settled_so_an_idle_agent_goes_at_once() {
    let schedule = schedule(vec![pending_user()]);
    assert_eq!(schedule.boundary(UnixMs(1)), Boundary::Now);
}

#[test]
fn interrupt_discards_the_in_flight_request_and_a_plain_send_waits_for_it() {
    let mut schedule = schedule(vec![pending_user()]);
    schedule.inference_active = true;

    assert_eq!(schedule.boundary(UnixMs(1)), Boundary::No);

    schedule.wants_interrupt = true;
    assert_eq!(schedule.boundary(UnixMs(1)), Boundary::AbortNow);
}

#[test]
fn a_chattering_tool_defers_the_boundary_until_it_goes_quiet() {
    let schedule = schedule(vec![running_tool(1_000)]);

    // Still producing, so waiting will produce a better request.
    assert_eq!(schedule.boundary(UnixMs(1_100)), Boundary::No);
    assert_eq!(
        schedule.next_deadline(UnixMs(1_100)),
        Some(UnixMs(1_250)),
        "wake when quiet_after expires"
    );

    assert_eq!(schedule.boundary(UnixMs(1_250)), Boundary::Now);
}

#[test]
fn an_exited_tool_never_has_to_be_waited_for() {
    // Exit is certain where quiet is only a guess, so no quiet window applies.
    let schedule = schedule(vec![exited_tool()]);
    assert_eq!(schedule.boundary(UnixMs(0)), Boundary::Now);
}

#[test]
fn a_queued_message_does_not_wait_out_a_tool_that_keeps_talking() {
    // The tool has produced steadily since the model last spoke, so it never
    // goes quiet; max_hold is what breaks the tie.
    let user_hold = Rhythm::USER.max_hold.as_millis() as u64;

    let early = schedule(vec![pending_user(), running_tool(0)]);
    assert_eq!(early.boundary(UnixMs(0)), Boundary::No);

    // The loop wakes first at the tool's quiet expiry, since that is the
    // soonest moment the answer could change...
    let quiet = Rhythm::TOOL.quiet_after.as_millis() as u64;
    assert_eq!(early.next_deadline(UnixMs(0)), Some(UnixMs(quiet)));

    // ...but the user's shorter bound is what actually caps the wait, and it
    // fires even though the tool is still going.
    assert_eq!(early.hold_deadline(), Some(UnixMs(user_hold)));
    let still_talking = schedule(vec![pending_user(), running_tool(user_hold)]);
    assert_eq!(still_talking.boundary(UnixMs(user_hold)), Boundary::Now);
}

#[test]
fn the_most_impatient_pending_source_sets_the_deadline() {
    let user_hold = Rhythm::USER.max_hold.as_millis() as u64;
    let mail_hold = Rhythm::MAIL.max_hold.as_millis() as u64;
    assert!(user_hold < mail_hold);

    // Mail alone would wait out its own window...
    let mail_only = schedule(vec![pending_mail(100), running_tool(100)]);
    assert_eq!(mail_only.hold_deadline(), Some(UnixMs(mail_hold)));

    // ...but a waiting user drags it into the earlier request.
    let with_user = schedule(vec![pending_mail(100), running_tool(100), pending_user()]);
    assert_eq!(with_user.hold_deadline(), Some(UnixMs(user_hold)));
}

#[test]
fn mail_waits_a_beat_so_a_chatty_peer_costs_one_request() {
    let quiet = Rhythm::MAIL.quiet_after.as_millis() as u64;
    let schedule = schedule(vec![pending_mail(1_000)]);

    assert_eq!(schedule.boundary(UnixMs(1_000)), Boundary::No);
    assert_eq!(schedule.boundary(UnixMs(1_000 + quiet)), Boundary::Now);
}

#[test]
fn a_must_send_standing_sends_even_with_nothing_pending() {
    let mut schedule = schedule(Vec::new());
    schedule.standing = Standing::MustSend;
    assert_eq!(schedule.boundary(UnixMs(0)), Boundary::Now);
    assert_eq!(
        schedule.next_deadline(UnixMs(0)),
        None,
        "no timer needed when the answer is already yes"
    );
}

#[test]
fn a_cancelled_agent_is_not_woken_by_its_tools_dying_words() {
    // Cancel asks tools to wind down, and their last output still has to reach
    // history — but it must not be the reason a new request happens.
    let mut schedule = schedule(vec![exited_tool()]);
    assert_eq!(
        schedule.boundary(UnixMs(0)),
        Boundary::Now,
        "an exited tool would normally go at once"
    );

    schedule.standing = Standing::Halted;
    assert_eq!(schedule.boundary(UnixMs(0)), Boundary::No);
    assert_eq!(
        schedule.next_deadline(UnixMs(0)),
        None,
        "and no timer either"
    );
}

#[test]
fn no_timer_is_armed_while_a_request_is_in_flight() {
    let mut schedule = schedule(vec![pending_user(), running_tool(0)]);
    schedule.inference_active = true;
    assert_eq!(schedule.next_deadline(UnixMs(0)), None);
}

#[test]
fn an_overdue_deadline_wakes_immediately_rather_than_sleeping_past_it() {
    let schedule = schedule(vec![running_tool(0)]);
    let late = UnixMs(1_000_000);
    assert_eq!(schedule.next_deadline(late), Some(late));
}

// -- replay -----------------------------------------------------------------

#[test]
fn accepted_input_survives_a_restart_that_never_delivered_it() {
    let restored = restore(vec![AgentEvent::Queued(user_input(
        Delivery::NextRequest,
        10,
    ))]);
    assert_eq!(restored.user.len(), 1);
    assert!(restored.history.is_empty());
}

#[test]
fn a_drain_empties_the_queues_it_touched() {
    let restored = restore(vec![
        AgentEvent::Queued(user_input(Delivery::NextRequest, 10)),
        AgentEvent::Queued(QueuedInput {
            source: InputSource::Mail { peer: peer(1) },
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
fn a_call_counts_as_answered_once_a_result_exists_for_it() {
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

    assert!(answered(&history, &ToolCallId::try_from("call-1").unwrap()));
    assert!(!answered(
        &history,
        &ToolCallId::try_from("call-2").unwrap()
    ));
}

// -- rhythms ----------------------------------------------------------------

#[test]
fn rhythms_rank_people_above_peers_and_machines() {
    assert!(Rhythm::USER.max_hold < Rhythm::MAIL.max_hold);
    assert!(Rhythm::USER.max_hold < Rhythm::TOOL.max_hold);
    assert_eq!(
        Rhythm::USER.quiet_after,
        Duration::ZERO,
        "typed input is complete on arrival"
    );
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
    last_output_at: UnixMs,
    exited: bool,
    cancels: u32,
}

impl FakeSession {
    fn produce(&mut self, text: &str, at: UnixMs) {
        self.unsent.push_str(text);
        self.last_output_at = at;
    }
}

impl ToolSession for FakeSession {
    fn status(&self) -> ToolStatus {
        ToolStatus {
            last_output_at: self.last_output_at,
            pending: !self.unsent.is_empty(),
            exited: self.exited,
        }
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
        self.exited = true;
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
        RunningTool::new(call, Rhythm::TOOL, Box::new(session), UnixMs(0)),
    )
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
        tool.pending(),
        "the core knows there is something, not what"
    );

    // One pull, one block: the tool chose how to represent all of it.
    let Some(ToolTake::Result(result)) = tool.take(UnixMs(2_000)) else {
        panic!("expected a result")
    };
    assert!(result.body.output.starts_with("line 0"));
    assert!(!tool.pending(), "nothing left buffered anywhere");
}

#[test]
fn cancel_reaches_the_tool_and_lets_it_have_the_last_word() {
    let session = SharedSession::default();
    let (_, mut tool) = running("call-1", session.clone());

    tool.cancel();
    assert_eq!(session.cancels(), 1);

    // Winding down produced a parting note, which still reaches the model.
    session.produce("cleaned up", UnixMs(10));
    assert!(tool.status().exited);
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

// -- previews ---------------------------------------------------------------

#[test]
fn a_tool_can_show_whatever_it_likes_in_a_preview() {
    let session = SharedSession::default();
    session.produce("hello", UnixMs(5));

    // The default is a plain ToolPreview...
    let data = session.preview();
    let default = data
        .as_any()
        .downcast_ref::<ToolPreview>()
        .expect("default tool preview");
    assert!(default.pending);
    assert_eq!(default.last_output_at, UnixMs(5));

    // ...and a queue preview is a different type behind the same field, which
    // is the point of making it open.
    let queue = Preview {
        label: Cow::Borrowed("user"),
        data: Box::new(QueuePreview {
            pending: 2,
            since: UnixMs(7),
        }),
    };
    assert!(queue.data.as_any().downcast_ref::<ToolPreview>().is_none());
    assert_eq!(
        queue.data.as_any().downcast_ref::<QueuePreview>().unwrap(),
        &QueuePreview {
            pending: 2,
            since: UnixMs(7)
        }
    );
}
