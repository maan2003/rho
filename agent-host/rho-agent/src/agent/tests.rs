//! The loop end to end: a scripted model, a real notebook, and the agent
//! host's services over the worker socket.

use std::time::Duration;

use rho_agent_types::{AgentRole, UnixMs};
use rho_db::RhoDb;
use rho_inference2::Item;
use rho_inference2::scripted::Scripted;

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
        .send_user_content_accepted(
            vec![ContentPart::Text { text: text.into() }],
            MessageDelivery::Immediate,
        )
        .await
        .unwrap();
}

/// Everything a request tells the model, besides replayed steps.
fn told(request: &rho_inference2::Request) -> String {
    request
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Result { text, .. } | Item::User { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn requests(script: &Scripted, count: usize) -> Vec<rho_inference2::Request> {
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
    assert_eq!(handle.status().kind, AgentStateKind::Idle);
    assert_eq!(script.requests().len(), 1);

    // The answer ends the wait; the cell finishing wakes the model with it.
    say(&handle, "second").await;
    let second = requests(&script, 2).await;
    let told = told(&second[1]);
    assert!(told.contains("second"), "{told}");
    assert!(
        second[1].items.iter().any(
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
async fn a_restart_tells_the_model_and_keeps_unread_messages() {
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
    let (_handle, _task) = harness.start(&script).await;
    let told = told(&requests(&script, 1).await[0]);
    assert!(told.contains("rho restarted"), "{told}");
    let entries = harness.entries();
    assert!(entries.iter().any(|entry| matches!(
        entry,
        Entry::Notice {
            notice: Notice::Restarted,
            ..
        }
    )));
    // What awaited the human went with the old notebook.
    assert!(matches!(
        entries
            .iter()
            .filter(|entry| matches!(entry, Entry::Awaiting { .. }))
            .nth(1),
        Some(Entry::Awaiting { since: None, .. })
    ));
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
        .until("three failures", |entries| {
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
                == MAX_FAILURES as usize
        })
        .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !matches!(handle.status().kind, AgentStateKind::Error { .. }) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "{:?}",
            handle.status()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(script.requests().len(), MAX_FAILURES as usize);

    script.then("await human.reply()");
    handle.retry();
    let requests = requests(&script, MAX_FAILURES as usize + 1).await;
    assert!(told(requests.last().unwrap()).contains("hi"));
}
