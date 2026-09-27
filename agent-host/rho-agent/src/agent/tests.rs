//! The loop end to end: a scripted model, a real notebook, and the agent
//! host's services over the worker socket.

use std::time::Duration;

use rho_agent_types::{AgentRole, UnixMs};
use rho_db::RhoDb;
use rho_inference::step::Item;
use rho_inference::step::scripted::Scripted;

use super::*;
use crate::db::{
    AgentProfileWriteTxnExt as _, AgentReadTxnExt as _, AgentRoleSessionProfile as _, AgentRuntime,
    AgentWriteTxnExt as _,
};
use crate::entry::{Block, Entry, Notice, Party};

struct Harness {
    _directory: tempfile::TempDir,
    db: RhoDb,
    agent: AgentId,
    host: Arc<crate::worker::Host>,
    inference: Inference,
    view: Arc<Lazy<Arc<View>>>,
}

impl Harness {
    async fn new() -> Self {
        // The worker installs one at startup; the notebook's web client needs it.
        rho_inference::ensure_crypto_provider();
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("rho.redb"));
        let mut write = db.write().await;
        write.init_agent_tables();
        let agent = write.alloc_agent_id();
        let role = AgentRole::default();
        write.create_agent(
            UnixMs(1),
            agent,
            None,
            crate::db::tests::test_workspace(),
            role,
            role.session_profile(),
            AgentRuntime::Rho {
                prompt_cache_key: rho_inference::PromptCacheKey::generate(),
            },
            crate::db::AgentOrigin::User,
        );
        write.commit();
        let inference = Inference::new_with_config(
            db.clone(),
            rho_inference::InferenceConfig::with_responses_base_url("http://127.0.0.1:1").unwrap(),
        )
        .await
        .unwrap();
        let host = crate::worker::local_services(
            db.clone(),
            inference.clone(),
            agent,
            std::sync::Weak::new(),
        );
        let worksets = rho_fs_view::Worksets::open(
            directory.path().join("state"),
            rho_fs_view::UserEnvironment::new(std::env::vars_os().collect()),
            Default::default(),
            rho_fs_view::StoreService::None,
        )
        .await
        .unwrap();
        let view = worksets
            .create()
            .await
            .unwrap()
            .enter(
                rho_fs_view::Mode::View {
                    home_skeleton: None,
                },
                camino::Utf8Path::new("/src"),
            )
            .unwrap();
        Self {
            _directory: directory,
            db,
            agent,
            host,
            inference,
            view: Arc::new(Lazy::ready(view)),
        }
    }

    /// Load the agent and run its loop against `script`.
    async fn start(&self, script: &Arc<Scripted>) -> (AgentHandle, tokio::task::JoinHandle<()>) {
        let (handle, mut agent) = Agent::load(
            self.agent,
            self.host.clone(),
            self.inference.clone(),
            self.view.clone(),
        )
        .await
        .unwrap();
        agent.script(script.clone());
        let task = tokio::spawn(async move {
            let result = agent.run().await;
            agent.shutdown().await.unwrap();
            result.unwrap();
        });
        (handle, task)
    }

    fn entries(&self) -> Vec<Entry> {
        let (_, rows) = self.db.read().agent_event_records(self.agent);
        rows.into_iter()
            .filter_map(|(_, event)| match event {
                AgentEvent::Entry(entry) => Some(entry),
                _ => None,
            })
            .collect()
    }

    /// The visible log, once `done` holds of it.
    async fn until(&self, what: &str, done: impl Fn(&[Entry]) -> bool) -> Vec<Entry> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let entries = self.entries();
            if done(&entries) {
                return entries;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}: {entries:#?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

async fn say(handle: &AgentHandle, text: &str) {
    handle
        .send_user_content_accepted(vec![ContentPart::Text { text: text.into() }])
        .await
        .unwrap();
}

/// Everything a request tells the model, besides replayed steps.
fn told(request: &rho_inference::step::Request) -> String {
    request
        .items()
        .iter()
        .filter_map(|item| match item {
            Item::Result { text, .. } | Item::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn requests(script: &Scripted, count: usize) -> Vec<rho_inference::step::Request> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let requests = script.requests();
        if requests.len() >= count {
            return requests;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the model was asked {} times, not {count}",
            requests.len()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn received(entries: &[Entry], text: &str) -> bool {
    entries.iter().any(|entry| {
        matches!(entry, Entry::Received { from: Party::Human, body, .. }
            if body.iter().any(|block| matches!(block, Block::Text(t) if t == text)))
    })
}

#[tokio::test]
async fn a_message_wakes_the_model_and_what_it_sends_is_logged() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    script
        .then("human.send('hello there')\nawait human.reply()")
        .then("human.status('reading')\nawait human.reply()");
    let (handle, _task) = harness.start(&script).await;

    say(&handle, "hi").await;
    let entries = harness
        .until("the reply and the wait", |entries| {
            entries.iter().any(|entry| {
                matches!(entry, Entry::Sent { to: Party::Human, text, .. } if text == "hello there")
            }) && entries
                .iter()
                .any(|entry| matches!(entry, Entry::Awaiting { since: Some(_), .. }))
        })
        .await;
    assert!(received(&entries, "hi"));
    let first = requests(&script, 1).await;
    assert!(told(&first[0]).contains("hi"), "{}", told(&first[0]));
    // Awaiting the human is not work: the turn is over.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(handle.status().runtime.inference, InferenceState::Idle);
    assert_eq!(script.requests().len(), 1);

    // The answer ends the wait; the cell finishing wakes the model with it.
    say(&handle, "second").await;
    let second = requests(&script, 2).await;
    let told = told(&second[1]);
    assert!(told.contains("second"), "{told}");
    assert!(
        second[1].items().iter().any(
            |item| matches!(item, Item::Result { call_id, .. } if call_id.as_str() == "call_1")
        ),
        "the first call's result is reported"
    );
    harness
        .until("the status", |entries| {
            entries
                .iter()
                .any(|entry| matches!(entry, Entry::Status { text, .. } if text == "reading"))
        })
        .await;
}

#[tokio::test]
async fn a_restart_does_not_resume_a_running_cell_by_itself() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    script.then("import asyncio\nawait asyncio.sleep(600)");
    let (handle, task) = harness.start(&script).await;
    say(&handle, "first").await;
    requests(&script, 1).await;
    harness
        .until("the step", |entries| {
            entries
                .iter()
                .any(|entry| matches!(entry, Entry::Step { .. }))
        })
        .await;
    drop(handle);
    task.await.unwrap();

    let script = Arc::new(Scripted::new());
    script.then("await human.reply()");
    let (handle, _task) = harness.start(&script).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(script.requests().is_empty(), "coming up is not a wake");

    say(&handle, "second").await;
    let told = told(&requests(&script, 1).await[0]);
    assert!(told.contains("rho restarted"), "{told}");
    assert!(harness.entries().iter().any(|entry| matches!(
        entry,
        Entry::Notice {
            notice: Notice::Restarted,
            ..
        }
    )));
}

#[tokio::test]
async fn a_restart_leaves_a_waiting_model_until_the_human_speaks() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    script.then("await human.reply()");
    let (handle, task) = harness.start(&script).await;
    say(&handle, "first").await;
    harness
        .until("the wait", |entries| {
            entries
                .iter()
                .any(|entry| matches!(entry, Entry::Awaiting { since: Some(_), .. }))
        })
        .await;
    drop(handle);
    task.await.unwrap();

    let script = Arc::new(Scripted::new());
    script.then("await human.reply()");
    let (handle, _task) = harness.start(&script).await;
    // What awaited the human went with the old notebook.
    harness
        .until("the wait ending", |entries| {
            matches!(
                entries
                    .iter()
                    .filter(|entry| matches!(entry, Entry::Awaiting { .. }))
                    .nth(1),
                Some(Entry::Awaiting { since: None, .. })
            )
        })
        .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(script.requests().is_empty(), "a reload is not a wake");
    assert!(!harness.entries().iter().any(|entry| matches!(
        entry,
        Entry::Notice {
            notice: Notice::Restarted,
            ..
        }
    )));

    say(&handle, "second").await;
    let told = told(&requests(&script, 1).await[0]);
    assert!(told.contains("rho restarted"), "{told}");
    assert!(told.contains("second"), "{told}");
}

#[tokio::test]
async fn rewind_branches_before_the_last_human_message() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    script
        .then("await human.reply()")
        .then("await human.reply()")
        .then("await human.reply()");
    let (handle, _task) = harness.start(&script).await;
    say(&handle, "one").await;
    requests(&script, 1).await;
    harness
        .until("the first wait", |entries| {
            entries
                .iter()
                .any(|entry| matches!(entry, Entry::Awaiting { since: Some(_), .. }))
        })
        .await;
    say(&handle, "two").await;
    requests(&script, 2).await;
    harness
        .until("the second step", |entries| {
            entries
                .iter()
                .filter(|entry| matches!(entry, Entry::Step { .. }))
                .count()
                == 2
        })
        .await;

    handle.rewind(1).await.unwrap();
    let third = requests(&script, 3).await;
    let told = told(&third[2]);
    assert!(told.contains("rewound"), "{told}");
    assert!(told.contains("one"), "{told}");
    assert!(!told.contains("two"), "{told}");
    let entries = harness.entries();
    assert!(received(&entries, "one"));
    assert!(!received(&entries, "two"), "the rewound message is hidden");
}

#[tokio::test]
async fn failing_requests_stop_the_agent_until_a_retry() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    let (handle, _task) = harness.start(&script).await;
    say(&handle, "hi").await;
    harness
        .until("permanent failure", |entries| {
            entries
                .iter()
                .filter(|entry| {
                    matches!(
                        entry,
                        Entry::Notice {
                            notice: Notice::Error(_),
                            ..
                        }
                    )
                })
                .count()
                == 1
        })
        .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !matches!(
        handle.status().runtime.inference,
        InferenceState::Failed { .. }
    ) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{:?}",
            handle.status()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(script.requests().len(), 1);

    script.then("await human.reply()");
    handle.retry();
    let requests = requests(&script, 2).await;
    assert!(told(requests.last().unwrap()).contains("hi"));
}

#[tokio::test]
async fn waiting_still_checks_in_and_archive_revival_has_a_fresh_notebook() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    script
        .then("remembered = 41\nset_max_wait(1)\nawait human.reply()")
        .then("human.send('checking in')\narchive()")
        .then(
            "human.send(str('remembered' in globals()))\nset_max_wait(86400)\nawait human.reply()",
        );
    let (handle, _task) = harness.start(&script).await;
    say(&handle, "start").await;
    let entries = harness
        .until("a live task waiting for the human", |entries| {
            entries
                .iter()
                .any(|entry| matches!(entry, Entry::Awaiting { since: Some(_), .. }))
                && {
                    let state = handle.status().runtime;
                    state.inference == InferenceState::Idle
                        && state.running_tasks > 0
                        && state.checkin_at.is_some()
                        && !state.archived
                        && state.awaiting_human
                }
        })
        .await;
    assert!(
        !entries
            .iter()
            .any(|entry| matches!(entry, Entry::Sent { .. }))
    );
    let entries = harness
        .until("archive after check-in", |_| {
            let state = handle.status().runtime;
            state.archived && state.running_tasks == 0
        })
        .await;
    assert!(
        !entries
            .iter()
            .any(|entry| matches!(entry, Entry::Activity { .. }))
    );
    assert!(entries.iter().any(|entry| matches!(
        entry,
        Entry::Woken {
            why: Wake::Checkin,
            ..
        }
    )));
    assert!(
        entries
            .iter()
            .any(|entry| matches!(entry, Entry::Sent { text, .. } if text == "checking in"))
    );
    say(&handle, "revive").await;
    harness
        .until("fresh globals", |entries| {
            entries
                .iter()
                .any(|entry| matches!(entry, Entry::Sent { text, .. } if text == "False"))
        })
        .await;
}

#[tokio::test]
async fn transient_failures_recover_beyond_three_attempts() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    for _ in 0..4 {
        script.then_transient();
    }
    script.then("human.send('recovered')\nawait human.reply()");
    let (handle, _task) = harness.start(&script).await;
    say(&handle, "original task").await;
    harness
        .until("recovered message", |entries| {
            entries
                .iter()
                .any(|entry| matches!(entry, Entry::Sent { text, .. } if text == "recovered"))
        })
        .await;
    let attempts = script.requests();
    assert_eq!(attempts.len(), 5);
    assert!(told(&attempts[4]).contains("original task"));
    assert!(!matches!(
        handle.status().runtime.inference,
        InferenceState::Failed { .. }
    ));
}

#[tokio::test]
async fn explicit_retry_after_restart_resumes_with_recovery_notice() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    let (handle, task) = harness.start(&script).await;
    say(&handle, "original task").await;
    harness
        .until("failure", |entries| {
            entries.iter().any(|entry| {
                matches!(
                    entry,
                    Entry::Notice {
                        notice: Notice::Error(_),
                        ..
                    }
                )
            })
        })
        .await;
    drop(handle);
    task.await.unwrap();

    let script = Arc::new(Scripted::new());
    script.then("await human.reply()");
    let (handle, _task) = harness.start(&script).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(script.requests().is_empty());
    handle.retry();
    let attempts = requests(&script, 1).await;
    assert!(told(&attempts[0]).contains("rho restarted"));
    assert!(told(&attempts[0]).contains("original task"));
}

#[tokio::test]
async fn backoff_caps_delay_and_expires_at_eight_hours() {
    let mut retry = Backoff::failed(None, "first".into());
    let mut delays = vec![retry.delay];
    for _ in 0..4 {
        retry = Backoff::failed(Some(retry), "again".into());
        delays.push(retry.delay);
    }
    assert_eq!(delays, [1, 1, 2, 3, 5]);
    for _ in 0..40 {
        retry = Backoff::failed(Some(retry), "again".into());
    }
    assert_eq!(retry.delay, 1800);
    retry.since = tokio::time::Instant::now() - Backoff::WINDOW;
    let before = UnixMs::now();
    let retry = Backoff::failed(Some(retry), "again".into());
    assert!(retry.at <= UnixMs::now());
    assert!(retry.at >= before);
    assert!(retry.since.elapsed() >= Backoff::WINDOW);
}

#[tokio::test]
async fn cancel_and_messages_remain_responsive_during_long_backoff() {
    for cancel in [false, true] {
        let harness = Harness::new().await;
        let script = Arc::new(Scripted::new());
        script.then("human.send('fresh input seen')\nawait human.reply()");
        let (handle, mut agent) = Agent::load(
            harness.agent,
            harness.host.clone(),
            harness.inference.clone(),
            harness.view.clone(),
        )
        .await
        .unwrap();
        agent.script(script.clone());
        let mut retry = Backoff::failed(None, "temporary outage".into());
        retry.at = UnixMs::now() + Duration::from_secs(1800);
        agent.backoff = Some(retry);
        let task = tokio::spawn(async move {
            agent.run().await.unwrap();
            agent.shutdown().await.unwrap();
        });
        if cancel {
            handle.cancel();
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(script.requests().is_empty(), "cancel must not retry");
            assert!(
                !handle.status().runtime.is_working(),
                "cancel must clear the pending retry"
            );
        }
        // This acknowledgement must not wait for the 30-minute retry timer.
        tokio::time::timeout(Duration::from_secs(2), say(&handle, "fresh steering"))
            .await
            .unwrap();
        let attempts = requests(&script, 1).await;
        assert!(told(&attempts[0]).contains("fresh steering"));
        assert_eq!(attempts.len(), 1);
        drop(handle);
        task.await.unwrap();
    }
}

#[tokio::test]
async fn exhausted_retry_window_stops_without_another_request() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    script.then("human.send('must not run')");
    let (handle, mut agent) = Agent::load(
        harness.agent,
        harness.host.clone(),
        harness.inference.clone(),
        harness.view.clone(),
    )
    .await
    .unwrap();
    agent.script(script.clone());
    let mut retry = Backoff::failed(None, "outage".into());
    retry.since = tokio::time::Instant::now() - Backoff::WINDOW;
    agent.backoff = Some(retry);
    let task = tokio::spawn(async move {
        agent.run().await.unwrap();
        agent.shutdown().await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(script.requests().is_empty());
    match handle.status().runtime.inference {
        InferenceState::Failed { error } => assert!(error.contains("retry window exhausted")),
        other => panic!("expected terminal error, got {other:?}"),
    }
    drop(handle);
    task.await.unwrap();
}

#[tokio::test]
async fn a_cut_after_admission_reports_the_executed_prefix_instead_of_retrying() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    script.then_cut("counter = globals().get('counter', 0) + 1\n");
    script.then("human.send(str(counter))\nawait human.reply()");
    let (handle, _task) = harness.start(&script).await;
    say(&handle, "count once").await;
    harness
        .until("counter message", |entries| {
            entries
                .iter()
                .any(|entry| matches!(entry, Entry::Sent { text, .. } if text == "1"))
        })
        .await;
    let attempts = script.requests();
    assert_eq!(attempts.len(), 2);
    assert!(told(&attempts[1]).contains("cut off"));
    assert!(
        attempts[1]
            .items()
            .iter()
            .any(|item| matches!(item, Item::Step(carry) if !carry.call_ids().is_empty())),
        "the next request must record admitted code, not replay the failed request"
    );
}

#[tokio::test]
async fn live_response_is_replaced_only_after_its_step_is_durable() {
    let harness = Harness::new().await;
    let mut updates = crate::journal::feed(&harness.db);
    let script = Arc::new(Scripted::new());
    // Enough separate streamed lines to observe several distinct snapshots.
    let code = format!(
        "value = 17\n{}await human.reply()",
        "# still writing\n".repeat(40)
    );
    script.then(&code);
    let (handle, task) = harness.start(&script).await;
    say(&handle, "run").await;
    let mut response_id = None;
    let mut previous = String::new();
    let mut grew = false;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = handle.status();
            if let Some(response) = status.response {
                assert_eq!(status.runtime.inference, InferenceState::Responding);
                let first = response_id.get_or_insert_with(|| response.id.clone());
                assert_eq!(*first, response.id);
                if let Some(rho_agents_client::protocol::transcript::Item::ToolCall {
                    arguments, ..
                }) = response.items.first() {
                    assert!(arguments.starts_with(&previous));
                    grew |= !previous.is_empty() && arguments.len() > previous.len();
                    previous = arguments.clone();
                }
            } else if response_id.is_some() {
                assert!(harness.entries().iter().any(|entry| matches!(
                    entry, Entry::Step { calls, .. } if calls.first().is_some_and(|call| call.code == code)
                )), "clearing the live response must follow the durable step");
                assert_eq!(status.runtime.inference, InferenceState::Idle);
                assert!(status.runtime.running_tasks > 0, "the cell still awaits the human");
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.unwrap();
    // No GUI focuses this harness. Current state still reaches the host feed,
    // but the potentially large response body is not broadcast.
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut saw_responding = false;
        loop {
            if let crate::journal::Feed::Status { status, .. } = updates.recv().await.unwrap() {
                assert!(
                    status.response.is_none(),
                    "unfocused response bodies must stay private to the worker/host cache"
                );
                saw_responding |= status.runtime.inference == InferenceState::Responding;
                if status.runtime.awaiting_human && status.runtime.inference == InferenceState::Idle
                {
                    assert!(saw_responding);
                    assert!(status.runtime.running_tasks > 0);
                    break;
                }
            }
        }
    })
    .await
    .unwrap();
    assert!(grew, "must exercise multiple streamed replacements");
    assert!(
        !harness
            .entries()
            .iter()
            .any(|entry| matches!(entry, Entry::Activity { .. }))
    );
    handle.cancel();
    drop(handle);
    task.await.unwrap();
}
