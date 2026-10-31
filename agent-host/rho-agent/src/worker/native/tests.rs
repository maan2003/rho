//! The loop end to end: a scripted model, a real notebook, the test
//! policy Python, and the agent host's services over the worker socket.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rho_agent_types::{Address, SendKind, WorksetId, WorksetKind};
use rho_db::RhoDb;

use super::scripted::Scripted;
use super::*;
use crate::inference::Item;
use crate::inference::config::ReasoningEffort;
use crate::record::AgentSpec;
use crate::store::{StoreRead as _, StoreWrite as _};

struct Harness {
    _directory: tempfile::TempDir,
    db: RhoDb,
    agent: AgentId,
    host: Arc<crate::worker::host_client::HostClient>,
    inference: Inference,
    root: camino::Utf8PathBuf,
}

impl Harness {
    async fn new() -> Self {
        // The worker installs one at startup; the notebook's web client needs
        // it.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        // The test policy answers `_rho` by agent across every test in the
        // process: each harness has an agent of its own.
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let agent = AgentId::from_counter(
            NEXT.fetch_add(1, Ordering::Relaxed),
            &rho_agent_types::AgentIdDomain(9),
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let db = RhoDb::open(directory.path().join("rho.redb"));
        let workset = WorksetId([1; 6]);
        let mut write = db.write().await;
        write.init_store_tables();
        write.create_workset(workset, UnixMs(1), WorksetKind::Isolated, String::new());
        write
            .append(
                workset,
                &Record {
                    at: UnixMs(2),
                    event: Event::AgentCreated {
                        agent,
                        spec: AgentSpec {
                            runtime: Runtime::Native {
                                model: InferenceModel::Gpt61Sol,
                                profile: InferenceProfile {
                                    effort: ReasoningEffort::High,
                                    fast_mode: false,
                                },
                                cache_key: PromptCacheKey::generate(),
                            },
                            props: Default::default(),
                        },
                    },
                },
            )
            .unwrap();
        write.commit();
        crate::testing::python(agent, &db);
        let host = crate::testing::services_pair(db.clone(), agent, std::sync::Weak::new());
        let root = camino::Utf8PathBuf::from_path_buf(directory.path().join("work")).unwrap();
        std::fs::create_dir(&root).unwrap();
        Self {
            _directory: directory,
            db,
            agent,
            host,
            inference: crate::inference::testing::backend(),
            root,
        }
    }

    /// Load the agent and run its loop against `script`.
    async fn start(&self, script: &Arc<Scripted>) -> (AgentHandle, tokio::task::JoinHandle<()>) {
        let (handle, mut agent) = self.load().await;
        agent.script(script.clone());
        let task = tokio::spawn(async move {
            let result = agent.run().await;
            agent.shutdown().await.unwrap();
            result.unwrap();
        });
        (handle, task)
    }

    async fn load(&self) -> (AgentHandle, Agent) {
        Agent::load(
            self.agent,
            self.host.clone(),
            self.inference.clone(),
            self.root.clone(),
        )
        .await
        .unwrap()
    }

    /// The agent's visible rows.
    fn events(&self) -> Vec<Event<'static>> {
        let read = self.db.read();
        let created = read.agent(self.agent).unwrap().created;
        read.agent_rows(self.agent, created)
            .into_iter()
            .map(|(_, record)| record.event)
            .collect()
    }

    /// The loop's own rows.
    fn entries(&self) -> Vec<Entry> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                Event::Native { entry, .. } => Some(entry),
                _ => None,
            })
            .collect()
    }

    /// Whether a person's input with `text` is still history.
    fn received(&self, text: &str) -> bool {
        self.events().iter().any(|event| {
            matches!(event, Event::Input { wake: InputWake::Person, content, .. }
                if rho_agent_types::transcript::text_content(content) == text)
        })
    }

    /// What the model mailed the user, oldest first, with its kind.
    fn mail(&self) -> Vec<(String, Option<SendKind>)> {
        self.db
            .read()
            .mail_since(MailId(0), usize::MAX)
            .into_iter()
            .filter(|mail| mail.to == Address::User)
            .map(|mail| {
                (
                    rho_agent_types::transcript::text_content(&mail.content),
                    mail.kind,
                )
            })
            .collect()
    }

    fn sent(&self) -> Vec<String> {
        self.mail().into_iter().map(|(text, _)| text).collect()
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

    /// Until the model has mailed the user `text`.
    async fn until_sent(&self, text: &str) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let sent = self.sent();
            if sent.iter().any(|sent| sent == text) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for the mail {text:?}: {sent:?} {:#?}",
                self.entries()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

async fn say(handle: &AgentHandle, text: &str) {
    handle
        .input(
            vec![ContentPart::Text { text: text.into() }],
            InputWake::Person,
            None,
        )
        .await
        .unwrap();
}

fn words(text: &str) -> Vec<ContentPart> {
    vec![ContentPart::Text { text: text.into() }]
}

/// Everything a request tells the model, besides replayed steps.
fn told(request: &crate::inference::Request) -> String {
    request
        .items()
        .iter()
        .filter_map(|item| match item {
            Item::Report { text, .. } => Some(text.as_str()),
            Item::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What a request tells the model that no earlier request did: the input
/// after its last replayed step.
fn told_new(request: &crate::inference::Request) -> String {
    let items = request.items();
    let start = items
        .iter()
        .rposition(|item| matches!(item, Item::Step { .. }))
        .map_or(0, |last| last + 1);
    items[start..]
        .iter()
        .filter_map(|item| match item {
            Item::Report { text, .. } => Some(text.as_str()),
            Item::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn requests(script: &Scripted, count: usize) -> Vec<crate::inference::Request> {
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

async fn wait_for(what: &str, done: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_message_wakes_the_model_and_what_it_sends_is_logged() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    script
        .then("human.send('hello there', kind='result')\nend_turn()")
        .then("human.send('reading', kind='status')\nend_turn()");
    let (handle, _task) = harness.start(&script).await;

    say(&handle, "hi").await;
    harness.until_sent("hello there").await;
    wait_for("the wait", || handle.status().runtime.awaiting_human).await;
    assert!(harness.received("hi"));
    let first = requests(&script, 1).await;
    assert!(told(&first[0]).contains("hi"), "{}", told(&first[0]));
    // Awaiting the human is not work: the turn is over.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(handle.status().runtime.inference, InferenceState::Idle);
    assert_eq!(script.requests().len(), 1);

    // The answer ends the wait and wakes the model with the call's result.
    say(&handle, "second").await;
    let second = requests(&script, 2).await;
    let told = told(&second[1]);
    assert!(told.contains("second"), "{told}");
    assert!(
        second[1]
            .items()
            .iter()
            .any(|item| matches!(item, Item::Report {reply_to:Some(carry),..} if carry.display_calls()[0].display_id() == "call_1")),
        "the first call's result is reported"
    );
    harness.until_sent("reading").await;
    assert_eq!(
        harness.mail(),
        [
            ("hello there".to_owned(), Some(SendKind::Result)),
            ("reading".to_owned(), Some(SendKind::Status)),
        ]
    );
}

#[tokio::test]
async fn an_ended_turn_wakes_only_for_news() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    script
        .then(
            "import asyncio\nasync def watch():\n    await asyncio.sleep(2)\n    notify('ci green')\nasyncio.create_task(watch())\nset_max_wait(1)\nend_turn()",
        )
        .then("end_turn()");
    let (handle, _task) = harness.start(&script).await;
    say(&handle, "watch ci").await;
    requests(&script, 1).await;

    // Neither the cell returning nor the requested check-in wakes it.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(script.requests().len(), 1);
    let state = handle.status().runtime;
    assert_eq!(state.checkin_at, None);
    assert!(state.awaiting_human);

    let second = requests(&script, 2).await;
    assert!(
        told(&second[1]).contains("ci green"),
        "{}",
        told(&second[1])
    );
    assert!(harness.entries().iter().any(|entry| matches!(
        entry,
        Entry::RequestSent {
            why: Wake::Notify,
            ..
        }
    )));
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
    script.then("end_turn()");
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
    script.then("end_turn()");
    let (handle, task) = harness.start(&script).await;
    say(&handle, "first").await;
    harness
        .until("the wait", |_| handle.status().runtime.awaiting_human)
        .await;
    drop(handle);
    task.await.unwrap();

    let script = Arc::new(Scripted::new());
    script.then("end_turn()");
    let (handle, _task) = harness.start(&script).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(script.requests().is_empty(), "a reload is not a wake");
    // What awaited the human went with the old notebook, but the agent
    // still waits on them: nothing moves until they write.
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

/// A client that never heard the answer sends the same message again,
/// maybe after the worker restarted; it lands in the log once.

#[tokio::test]
async fn rewind_branches_before_the_last_human_message() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    script
        .then("end_turn()")
        .then("end_turn()")
        .then("end_turn()");
    let (handle, _task) = harness.start(&script).await;
    say(&handle, "one").await;
    requests(&script, 1).await;
    harness
        .until("the first wait", |_| handle.status().runtime.awaiting_human)
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
    assert!(harness.received("one"));
    assert!(!harness.received("two"), "the rewound message is hidden");
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

    script.then("end_turn()");
    handle.retry();
    let requests = requests(&script, 2).await;
    assert!(told(requests.last().unwrap()).contains("hi"));
}

#[tokio::test]
async fn waiting_never_checks_in_and_archive_revival_has_a_fresh_notebook() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    script
        .then("remembered = 41\nset_max_wait(1)\nend_turn()")
        .then("human.send('archiving', kind='result')\narchive()")
        .then("human.send(str('remembered' in globals()), kind='result')\nend_turn()");
    let (handle, _task) = harness.start(&script).await;
    say(&handle, "start").await;
    harness
        .until("the model waiting for the human", |_| {
            let state = handle.status().runtime;
            state.inference == InferenceState::Idle
                && state.checkin_at.is_none()
                && !state.archived
                && state.awaiting_human
        })
        .await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(
        script.requests().len(),
        1,
        "an ended turn does not check in"
    );

    say(&handle, "archive yourself").await;
    harness
        .until("archive", |_| {
            let state = handle.status().runtime;
            state.archived && state.running_tasks == 0 && !state.awaiting_human
        })
        .await;
    say(&handle, "revive").await;
    harness.until_sent("False").await;
    assert_eq!(harness.sent(), ["archiving", "False"]);
}

#[tokio::test]
async fn transient_failures_recover_beyond_three_attempts() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    for _ in 0..4 {
        script.then_transient();
    }
    script.then("human.send('recovered', kind='result')\nend_turn()");
    let (handle, _task) = harness.start(&script).await;
    say(&handle, "original task").await;
    harness.until_sent("recovered").await;
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
    script.then("end_turn()");
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
        script.then("human.send('fresh input seen', kind='result')\nend_turn()");
        let (handle, mut agent) = harness.load().await;
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
async fn a_send_outside_simplified_technical_english_raises_and_is_not_mailed() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    script
        .then(
            "human.send(\"I'm reading\", kind='status')\nhuman.send(\"It's done.\", kind='result')",
        )
        .then("human.send('It is done.', kind='result')\nend_turn()");
    let (handle, _task) = harness.start(&script).await;

    say(&handle, "hi").await;
    // The raised cell returns at once, not after a failure's 20 seconds.
    let second = tokio::time::timeout(Duration::from_secs(10), requests(&script, 2))
        .await
        .unwrap();
    let told = told(&second[1]);
    assert!(
        told.contains("line 1, contraction: `It's` is a contraction."),
        "{told}"
    );
    harness.until_sent("It is done.").await;
    // A status is not held to the standard.
    assert_eq!(harness.sent(), ["I'm reading", "It is done."]);
    drop(handle);
}

#[tokio::test]
async fn exhausted_retry_window_stops_without_another_request() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    script.then("human.send('must not run', kind='result')");
    let (handle, mut agent) = harness.load().await;
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
    let (admit, cut) = tokio::sync::oneshot::channel();
    script.then_cut_after(
        "counter = globals().get('counter', 0) + 1\nhuman.send('admitted', kind='result')\n",
        cut,
    );
    script.then("human.send(str(counter), kind='result')\nend_turn()");
    let (handle, _task) = harness.start(&script).await;
    say(&handle, "count once").await;
    harness.until_sent("admitted").await;
    admit.send(()).unwrap();
    harness.until_sent("1").await;
    let attempts = script.requests();
    assert_eq!(attempts.len(), 2);
    assert!(told(&attempts[1]).contains("cut off"));
    assert!(
        attempts[1]
            .items()
            .iter()
            .any(|item| matches!(item, Item::Step {carry,..} if carry.has_call())),
        "the next request must record admitted code, not replay the failed request"
    );
}

#[tokio::test]
async fn code_fragments_wait_for_a_publication_frame() {
    let harness = Harness::new().await;
    let (_, mut agent) = harness.load().await;
    agent.notebook().await.unwrap();
    agent.responding = true;
    agent.response_id = "response-id".into();
    let mut streaming = None;
    agent.stream(
        &mut streaming,
        (
            Some(scripted::carry(Call::new("call-id", String::new()))),
            String::new(),
        ),
    );
    for _ in 0..100 {
        agent.stream(&mut streaming, (None, "# fragment\n".into()));
    }
    assert!(
        agent.status.read().unwrap().response.is_none(),
        "fragments must not copy accumulated code into status"
    );
    agent.publish_stream(streaming.as_ref(), true);
    let published = agent.status.read().unwrap().response.clone().unwrap();
    assert_eq!(published.id, "response-id");
    assert_eq!(
        published.items,
        vec![rho_agents_client::protocol::transcript::Item::ToolCall {
            id: "call-id".into(),
            name: "exec".into(),
            arguments: "# fragment\n".repeat(100),
            format: ArgumentsFormat::Text,
        }]
    );
    agent.stream(&mut streaming, (None, "# final\n".into()));
    assert_eq!(
        agent.status.read().unwrap().response.as_ref(),
        Some(&published)
    );
    agent.publish_stream(streaming.as_ref(), false);
    assert!(
        matches!(&agent.status.read().unwrap().response.as_ref().unwrap().items[0],
        rho_agents_client::protocol::transcript::Item::ToolCall { arguments, .. }
        if arguments.ends_with("# final\n"))
    );
    agent.shutdown().await.unwrap();
}

#[tokio::test]
async fn live_response_is_replaced_only_after_its_step_is_durable() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    // Enough separate streamed lines to observe several distinct snapshots.
    let code = format!("value = 17\n{}end_turn()", "# still writing\n".repeat(40));
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
                    arguments,
                    ..
                }) = response.items.first()
                {
                    assert!(arguments.starts_with(&previous));
                    grew |= !previous.is_empty() && arguments.len() > previous.len();
                    previous = arguments.clone();
                }
            } else if response_id.is_some() {
                assert!(
                    harness.entries().iter().any(|entry| matches!(
                        entry, Entry::Step { exec, .. } if exec.as_deref() == Some(code.as_str())
                    )),
                    "clearing the live response must follow the durable step"
                );
                assert_eq!(status.runtime.inference, InferenceState::Idle);
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(grew, "must exercise multiple streamed replacements");
    handle.cancel();
    drop(handle);
    task.await.unwrap();
}

/// Exercise the agent-owned boundary through the same injected session API used
/// in production. Provider wire replay is tested independently in
/// rho-inference.

/// Exercise the agent-owned boundary through the same injected session API used
/// in production. Provider wire replay is tested independently in
/// rho-inference.
#[tokio::test]
async fn warm_suffix_and_database_fallback_keep_the_original_request_boundary() {
    use crate::inference::{Continuation, Request};
    struct Controlled(mpsc::UnboundedSender<(Request, mpsc::UnboundedSender<Streamed>)>);
    impl crate::inference::Session for Controlled {
        fn start(&self, request: Request) -> Response {
            let (events, response) = mpsc::unbounded_channel();
            self.0.send((request, events)).unwrap();
            response
        }
    }
    fn answer(events: mpsc::UnboundedSender<Streamed>, id: &str) {
        events
            .send(Streamed::Completed(Step {
                continuation: Some(Continuation::new(id.into())),
                call: None,
                prose: format!("ack-{id}"),
                carry: Carry::new(serde_json::json!(format!("ack-{id}")), vec![], false),
                usage: crate::inference::Usage::default(),
            }))
            .unwrap();
    }
    let (submitted, mut requests) =
        mpsc::unbounded_channel::<(Request, mpsc::UnboundedSender<Streamed>)>();
    let (reconnecting, reconnect) = oneshot::channel();
    let (resume, resumed) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (first, events) = requests.recv().await.unwrap();
        assert!(first.continuation.is_none());
        assert!(told(&first).contains("first-message"));
        answer(events, "r1");
        let (second, events) = requests.recv().await.unwrap();
        assert_eq!(second.continuation, Some(Continuation::new("r1".into())));
        assert!(told(&second).contains("second-message"));
        assert!(!told(&second).contains("first-message"));
        answer(events, "r2");
        let (third, events) = requests.recv().await.unwrap();
        assert_eq!(third.continuation, Some(Continuation::new("r2".into())));
        reconnecting.send(()).unwrap();
        resumed.await.unwrap();
        events.send(Streamed::NeedsContext).unwrap();
        let (replay, events) = requests.recv().await.unwrap();
        assert!(replay.continuation.is_none());
        let text = format!("{:?}", replay.items);
        for expected in [
            "first-message",
            "second-message",
            "third-message",
            "ack-r1",
            "ack-r2",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        assert!(!text.contains("late-message"));
        assert!(
            !replay
                .items
                .iter()
                .any(|i| matches!(i, Item::CompactionTrigger))
        );
        answer(events, "r3");
        let (fourth, events) = requests.recv().await.unwrap();
        assert_eq!(fourth.continuation, Some(Continuation::new("r3".into())));
        assert!(told(&fourth).contains("late-message"));
        assert!(matches!(fourth.items.last(), Some(Item::CompactionTrigger)));
        answer(events, "r4");
    });
    let harness = Harness::new().await;
    let (handle, mut agent) = harness.load().await;
    agent.session = Arc::new(Controlled(submitted));
    let instructions: Arc<str> = "test instructions".into();
    for text in ["first-message", "second-message"] {
        agent
            .receive(words(text), InputWake::Person, None)
            .await
            .unwrap();
        agent
            .wake_with(
                Wake::Message,
                instructions.clone(),
                Report::default(),
                false,
            )
            .await
            .unwrap();
        assert!(
            agent.context.input().is_empty(),
            "completed input retained in RAM"
        );
    }
    agent
        .receive(words("third-message"), InputWake::Person, None)
        .await
        .unwrap();
    let control = async {
        reconnect.await.unwrap();
        handle.compact();
        handle
            .input(words("late-message"), InputWake::Agent, None)
            .await
            .unwrap();
        resume.send(()).unwrap();
    };
    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            agent.wake_with(
                Wake::Message,
                instructions.clone(),
                Report::default(),
                false
            ),
            control
        )
    })
    .await
    .unwrap();
    result.unwrap();
    assert!(
        agent.compaction.pending,
        "concurrent compaction must remain pending"
    );
    agent
        .wake_with(Wake::Message, instructions, Report::default(), false)
        .await
        .unwrap();
    assert!(agent.context.input().is_empty());
    agent.shutdown().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn retry_logs_only_new_contributions_but_builds_one_combined_input() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    script.then_transient().then("end_turn()");
    let (_handle, mut agent) = harness.load().await;
    agent.script(script.clone());
    agent.notebook().await.unwrap();
    agent
        .append(Entry::Step {
            exec: Some("pass".into()),
            prose: String::new(),
            usage: None,
            carry: scripted::carry(Call::new("preceding-exec", "pass".into())),
        })
        .await
        .unwrap();
    agent
        .receive(words("initial-message"), InputWake::Person, None)
        .await
        .unwrap();
    let first = Report {
        notebook: rho_notebook::Report::from_text("alpha-output".into(), vec![]),
        ..Report::default()
    };
    agent
        .wake_with(Wake::Notify, "system".into(), first, false)
        .await
        .unwrap();
    assert!(agent.backoff.is_some());

    agent
        .receive(words("arrived-after-failure"), InputWake::Person, None)
        .await
        .unwrap();
    let second = Report {
        notebook: rho_notebook::Report::from_text("beta-output".into(), vec![]),
        ..Report::default()
    };
    agent
        .wake_with(Wake::Notify, "system".into(), second, false)
        .await
        .unwrap();
    agent.flush().await.unwrap();
    let attempts = script.requests();
    assert_eq!(attempts.len(), 2);
    let result = attempts[1]
        .items()
        .iter()
        .filter_map(|i| match i {
            Item::Report {
                text,
                images,
                reply_to: Some(carry),
            } => Some((text, images, carry)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(result.len(), 1);
    assert_eq!(
        result[0].2.display_calls()[0].display_id(),
        "preceding-exec"
    );
    assert_eq!(result[0].0, "alpha-output\n\nbeta-output");
    let text = told(&attempts[1]);
    assert_eq!(text.matches("initial-message").count(), 1);
    assert_eq!(text.matches("arrived-after-failure").count(), 1);
    let logged = harness
        .entries()
        .into_iter()
        .filter_map(|entry| match entry {
            Entry::RequestSent { report, .. } => Some(report.render(false).text),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(logged, ["alpha-output", "beta-output"]);
    agent.shutdown().await.unwrap();
}

#[tokio::test]
async fn cold_load_and_queued_boundary_preserve_inputs_across_compaction() {
    let harness = Harness::new().await;
    let (_, mut agent) = harness.load().await;
    // These recovery facts precede the retained replay range.
    agent
        .append(Entry::Notice {
            notice: Notice::FreshNotebook,
        })
        .await
        .unwrap();
    agent
        .receive(words("old-discarded"), InputWake::Person, None)
        .await
        .unwrap();
    agent
        .append(Entry::CompactionTrigger { manual: false })
        .await
        .unwrap();
    agent
        .append(Entry::RequestSent {
            why: Wake::Compaction,
            compact: true,
            report: Report::default(),
        })
        .await
        .unwrap();
    agent
        .receive(words("during-compaction"), InputWake::Person, None)
        .await
        .unwrap();
    agent
        .append(Entry::Step {
            exec: None,
            prose: String::new(),
            usage: None,
            carry: Carry::new(serde_json::json!("retained-compaction"), vec![], true),
        })
        .await
        .unwrap();
    agent
        .receive(words("after-compaction"), InputWake::Agent, None)
        .await
        .unwrap();
    agent.shutdown().await.unwrap();
    drop(agent);

    let (_, mut agent) = harness.load().await;
    assert!(agent.restarted);
    assert!(
        agent.compaction.reply,
        "automatic compaction still owes a reply"
    );
    assert!(agent.compaction_owed, "the compaction's texts are owed");
    assert_eq!(
        agent
            .unread
            .iter()
            .map(|(wake, _)| *wake)
            .collect::<Vec<_>>(),
        [InputWake::Person, InputWake::Agent],
        "the inputs after the compacting request are unread"
    );

    // Freeze while persistence is blocked. A later request and compaction must
    // not move this prepared turn's cutoff or its replay start.
    agent.flush().await.unwrap();
    let write = harness.db.write().await;
    agent
        .append(Entry::RequestSent {
            why: Wake::Message,
            compact: false,
            report: Report::default(),
        })
        .await
        .unwrap();
    let mut turn = agent.prepare_turn("system".into()).await.unwrap();
    agent
        .receive(words("too-late"), InputWake::Person, None)
        .await
        .unwrap();
    agent
        .append(Entry::RequestSent {
            why: Wake::Compaction,
            compact: true,
            report: Report::default(),
        })
        .await
        .unwrap();
    agent
        .append(Entry::Step {
            exec: None,
            prose: String::new(),
            usage: None,
            carry: Carry::new(serde_json::json!("later-compaction"), vec![], true),
        })
        .await
        .unwrap();
    drop(write);
    agent.flush().await.unwrap();

    let request = turn.request(&harness.host).await.unwrap();
    let text = format!("{:?}", request.items());
    assert!(text.contains("retained-compaction"), "{text}");
    for message in ["during-compaction", "after-compaction"] {
        assert_eq!(text.matches(message).count(), 1, "{text}");
    }
    for excluded in ["old-discarded", "too-late", "later-compaction"] {
        assert!(!text.contains(excluded), "{text}");
    }
    agent.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_agent_that_ended_its_turn_can_be_retired_once_its_tasks_finish() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    script.then("import asyncio\nasyncio.create_task(asyncio.sleep(1))\nend_turn()");
    let (handle, _task) = harness.start(&script).await;

    say(&handle, "hi").await;
    harness
        .until("the wait", |_| handle.status().runtime.awaiting_human)
        .await;
    assert!(handle.retire().await.is_err(), "its task is still running");
    harness
        .until("the task to finish", |_| handle.status().settled())
        .await;
    handle.retire().await.unwrap();
}

/// A loaded skill is shown when loaded, shown again as it reads then after
/// a compaction, and not repeated on requests in between or after.

/// Compacting an agent that ended its turn starts no turn: the cell that
/// ended it goes to the model with the next input, after the compaction.
#[tokio::test]
async fn compacting_an_agent_that_ended_its_turn_starts_no_turn() {
    let harness = Harness::new().await;
    let script = Arc::new(Scripted::new());
    script
        .then("print('cell-output-77')\nend_turn()")
        .then_compaction()
        .then("end_turn()");
    let (handle, _task) = harness.start(&script).await;
    say(&handle, "hi").await;
    harness
        .until("the wait", |_| handle.status().runtime.awaiting_human)
        .await;

    handle.compact();
    harness
        .until("the compaction", |entries| {
            entries
                .iter()
                .any(|entry| matches!(entry, Entry::Step { carry, .. } if carry.has_compaction()))
        })
        .await;
    let compaction = &script.requests()[1];
    assert!(
        !told_new(compaction).contains("cell-output-77"),
        "the compaction carries nothing to answer"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(script.requests().len(), 2, "no turn after the compaction");
    assert!(
        handle.status().runtime.awaiting_human,
        "still waiting on the user"
    );

    say(&handle, "next").await;
    let all = requests(&script, 3).await;
    let next = told_new(&all[2]);
    assert!(next.contains("next"), "{next}");
    assert!(next.contains("cell-output-77"), "{next}");
}

/// A compaction owes the model what policy gives back: the texts are quiet
/// inputs logged after the compaction and before the next request, which
/// tells them to the model once.
#[tokio::test]
async fn a_compaction_is_followed_by_the_policys_texts_once() {
    let harness = Harness::new().await;
    let restored = format!("Kept after compaction: {}", harness.agent.encoded());
    let script = Arc::new(Scripted::new());
    script
        .then("pass")
        .then_compaction()
        .then("end_turn()")
        .then("end_turn()")
        .then("end_turn()");
    let (handle, _task) = harness.start(&script).await;
    say(&handle, "hi").await;
    requests(&script, 2).await;
    harness
        .until("the compaction", |entries| {
            entries
                .iter()
                .any(|entry| matches!(entry, Entry::Step { carry, .. } if carry.has_compaction()))
        })
        .await;
    assert!(
        !told(&script.requests()[1]).contains(&restored),
        "nothing is restored before a compaction"
    );

    say(&handle, "after").await;
    let all = requests(&script, 3).await;
    let after = told_new(&all[2]);
    assert!(after.contains(&restored), "{after}");
    // In the log: compaction, then the quiet input, then the request that
    // delivers it.
    let events = harness.events();
    let compacted = events
        .iter()
        .position(|event| {
            matches!(event, Event::Native { entry: Entry::Step { carry, .. }, .. } if carry.has_compaction())
        })
        .unwrap();
    let quiet = events
        .iter()
        .position(|event| {
            matches!(event, Event::Input { wake: InputWake::Quiet, content, .. }
                if rho_agent_types::transcript::text_content(content) == restored)
        })
        .expect("the restored text is a quiet input");
    let delivered = events[compacted..]
        .iter()
        .position(|event| {
            matches!(
                event,
                Event::Native {
                    entry: Entry::RequestSent { .. },
                    ..
                }
            )
        })
        .unwrap()
        + compacted;
    assert!(compacted < quiet && quiet < delivered, "{events:#?}");
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                Event::Input {
                    wake: InputWake::Quiet,
                    ..
                }
            ))
            .count(),
        1,
        "restored once"
    );
    wait_for("the turn to end", || handle.status().runtime.awaiting_human).await;

    say(&handle, "again").await;
    let all = requests(&script, 4).await;
    let later = told_new(all.last().unwrap());
    assert!(later.contains("again"), "{later}");
    assert!(!later.contains(&restored), "{later}");
}
