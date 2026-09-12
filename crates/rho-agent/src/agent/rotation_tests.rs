use rho_core::MessagePhase;

use super::streaming::tests::agent;
use super::*;
use crate::{ContextChange, WakeTrigger};

fn input(text: &str, at: u64) -> QueuedInput {
    QueuedInput {
        source: MessageSender::User,
        kind: InputKind::Message {
            content: vec![ContentPart::Text { text: text.into() }],
        },
        delivery: MessageDelivery::NextRequest,
        at: UnixMs(at),
    }
}

fn message(text: &str) -> InferenceResponseItem {
    InferenceResponseItem::AssistantMessage {
        provider_specific: Box::new(rho_inference::OpenAiResponsesProviderData::Message {
            item_id: rho_core::ProviderResponseItemId::try_from("message").unwrap(),
        }),
        content: vec![ContentPart::Text { text: text.into() }],
        phase: Some(MessagePhase::FinalAnswer),
    }
}

fn exec(id: &str, source: &str) -> InferenceResponseItem {
    InferenceResponseItem::ToolCall {
        provider_specific: Box::new(rho_inference::OpenAiResponsesProviderData::CustomToolCall {
            item_id: rho_core::ProviderResponseItemId::try_from(id).unwrap(),
        }),
        id: ToolCallId::try_from(id).unwrap(),
        name: ToolName::try_from("exec").unwrap(),
        tool_type: rho_core::ToolType::Custom,
        arguments: source.into(),
    }
}

async fn reply(agent: &mut Agent, items: Vec<InferenceResponseItem>, used: u64) {
    agent
        .finish_request(
            items,
            None,
            Some(rho_core::TokenUsage {
                input_tokens: used,
                cached_input_tokens: 0,
                cache_write_input_tokens: 0,
                output_tokens: 0,
            }),
            UnixMs::now(),
        )
        .await;
}

async fn cell_returned(agent: &Agent) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if agent
                .latest_python_exec
                .as_ref()
                .unwrap()
                .1
                .facts()
                .returned
                .is_some()
            {
                return;
            }
            agent.wake.notified().await;
        }
    })
    .await
    .unwrap();
}

fn latest_send(agent: &Agent) -> (Option<crate::ContextChange>, Vec<ContextBlock>) {
    let (_, events) = agent.db.read().agent_events(agent.agent_id);
    events
        .into_iter()
        .rev()
        .find_map(|event| match event {
            AgentEvent::ContextSent { change, blocks, .. } => {
                Some((Some(change), blocks.into_owned()))
            }
            AgentEvent::Sent { blocks, .. } => Some((None, blocks.into_owned())),
            _ => None,
        })
        .unwrap()
}

#[tokio::test]
async fn marker_preparation_rotation_and_replay_preserve_queued_input() {
    let directory = tempfile::tempdir().unwrap();
    let mut agent = agent(directory.path()).await;
    agent.phase = Phase::Idle {
        owed: Vec::new(),
        standing: Standing::Asked,
    };
    let limit = agent.session.auto_compact_token_limit().unwrap();
    agent.context_used = Some(limit - context::RETAIN_TOKENS);
    agent
        .handle_control(Control::User(input("original task", 1), None), UnixMs(1))
        .await;
    agent.start_request(UnixMs(2), None).await;
    let (change, _) = latest_send(&agent);
    assert_eq!(change, Some(ContextChange::Marked { retain_from: 1 }));
    let marker = agent.context.marker.unwrap();
    reply(&mut agent, vec![message("working")], limit).await;

    agent
        .handle_control(
            Control::User(input("new task held during preparation", 3), None),
            UnixMs(3),
        )
        .await;
    agent.start_request(UnixMs(4), None).await;
    let (change, blocks) = latest_send(&agent);
    assert_eq!(
        change,
        Some(ContextChange::Preparing {
            retain_from: marker as u64,
            repair: false
        })
    );
    assert!(
        blocks
            .iter()
            .all(|block| matches!(block, ContextBlock::DeveloperMessage { .. }))
    );
    assert_eq!(agent.user.len(), 1);
    assert_eq!(
        rho_core::context_window_start(&agent.history),
        0,
        "preparation still sees the old context"
    );
    let (_, events) = agent.db.read().agent_events(agent.agent_id);
    let replayed = replay::replay(events);
    assert_eq!(
        replayed.user, agent.user,
        "preparation must not acknowledge queued input"
    );
    assert!(
        replayed.context.preparation.is_none(),
        "restart must not replay preparation"
    );
    assert_eq!(replayed.context.marker, Some(marker));

    reply(&mut agent, vec![message("notes are ready")], limit).await;
    assert!(
        matches!(agent.decide(UnixMs::now()), Boundary::Now { wake } if wake.trigger == WakeTrigger::ContextRotation)
    );
    agent.start_request(UnixMs::now(), None).await;
    assert_eq!(rho_core::context_window_start(&agent.history), marker);
    assert!(agent.context.marker.is_none());
    assert!(agent.user.is_empty());
    assert_eq!(agent.context_used, None);
    assert!(
        latest_send(&agent)
            .1
            .iter()
            .any(|block| matches!(block, ContextBlock::ContextRotation { .. }))
    );
    assert!(
        matches!(&*agent.history[0], ContextBlock::UserMessage { .. }),
        "full history is untouched"
    );
    let (_, events) = agent.db.read().agent_events(agent.agent_id);
    let replayed = replay::replay(events);
    assert_eq!(rho_core::context_window_start(&replayed.history), marker);
    assert_eq!(replayed.history, agent.history);
    assert_eq!(replayed.context_used, None);
    assert!(replayed.user.is_empty());
}

#[tokio::test]
async fn preparation_waits_for_python_and_allows_only_one_repair() {
    let directory = tempfile::tempdir().unwrap();
    let mut agent = agent(directory.path()).await;
    agent.phase = Phase::Idle {
        owed: Vec::new(),
        standing: Standing::Asked,
    };
    let limit = agent.session.auto_compact_token_limit().unwrap();
    agent.context_used = Some(limit);
    agent.start_request(UnixMs::now(), None).await;
    reply(
        &mut agent,
        vec![exec(
            "prepare",
            "import asyncio\nawait asyncio.sleep(0.1)\nraise ValueError('write failed')",
        )],
        limit,
    )
    .await;
    agent
        .handle_control(
            Control::User(
                QueuedInput {
                    delivery: MessageDelivery::Immediate,
                    ..input("queued", 5)
                },
                None,
            ),
            UnixMs(5),
        )
        .await;
    assert!(matches!(agent.decide(UnixMs::now()), Boundary::No { .. }));
    cell_returned(&agent).await;
    assert!(matches!(agent.decide(UnixMs::now()), Boundary::Now { .. }));
    agent.start_request(UnixMs::now(), None).await;
    assert!(matches!(
        latest_send(&agent).0,
        Some(ContextChange::Preparing { repair: true, .. })
    ));
    assert_eq!(agent.user.len(), 1);
    assert!(latest_send(&agent).1.iter().any(|block| matches!(block,
        ContextBlock::ToolResults { results } if results.iter().any(|result| result.body.output.contains("write failed")))));

    reply(
        &mut agent,
        vec![exec("repair", "raise ValueError('failed again')")],
        limit,
    )
    .await;
    cell_returned(&agent).await;
    agent.start_request(UnixMs::now(), None).await;
    assert!(
        latest_send(&agent)
            .1
            .iter()
            .any(|block| matches!(block, ContextBlock::ContextRotation { .. }))
    );
    assert!(agent.user.is_empty());
}

#[tokio::test]
async fn preparation_preserves_python_state_and_does_not_wait_for_old_jobs() {
    let directory = tempfile::tempdir().unwrap();
    let mut agent = agent(directory.path()).await;
    let limit = agent.session.auto_compact_token_limit().unwrap();
    reply(
        &mut agent,
        vec![exec(
            "old",
            "remembered = 41\njob = command('sleep 0.3; echo old-output')",
        )],
        100,
    )
    .await;
    cell_returned(&agent).await;
    agent.start_request(UnixMs::now(), None).await;
    reply(&mut agent, vec![message("continue")], limit).await;
    agent.context_used = agent.session.auto_compact_token_limit();
    agent.start_request(UnixMs::now(), None).await;
    reply(
        &mut agent,
        vec![exec("prepare", "assert remembered == 41\nremembered += 1")],
        limit,
    )
    .await;
    cell_returned(&agent).await;
    agent.start_request(UnixMs::now(), None).await;
    assert!(
        latest_send(&agent)
            .1
            .iter()
            .any(|block| matches!(block, ContextBlock::ContextRotation { .. }))
    );
    reply(
        &mut agent,
        vec![exec(
            "after",
            "assert remembered == 42\nprint('state survived')",
        )],
        100,
    )
    .await;
    cell_returned(&agent).await;
    assert!(!agent.latest_python_exec.as_ref().unwrap().1.facts().failed);
}

#[tokio::test]
async fn cancellation_stops_preparation_even_with_buffered_input() {
    let directory = tempfile::tempdir().unwrap();
    let mut agent = agent(directory.path()).await;
    let limit = agent.session.auto_compact_token_limit().unwrap();
    agent.phase = Phase::Idle {
        owed: Vec::new(),
        standing: Standing::Asked,
    };
    agent.context_used = agent.session.auto_compact_token_limit();
    agent.start_request(UnixMs::now(), None).await;
    reply(
        &mut agent,
        vec![exec("prepare", "import asyncio\nawait asyncio.sleep(60)")],
        limit,
    )
    .await;
    agent
        .handle_control(Control::User(input("held", 0), None), UnixMs(0))
        .await;
    agent.handle_control(Control::Cancel, UnixMs::now()).await;
    assert!(agent.context.preparation.is_none());
    assert!(agent.user.is_empty());
    assert!(matches!(
        agent.decide(UnixMs::now()),
        Boundary::No { recheck: None }
    ));
}

#[test]
fn preparation_mirror_does_not_drain_the_ui_queue() {
    let event = AgentEvent::ContextSent {
        change: ContextChange::Preparing {
            retain_from: 0,
            repair: false,
        },
        blocks: Cow::Owned(Vec::new()),
        at: UnixMs(5),
        wake: None,
    };
    assert_eq!(
        crate::mirror::strip(&event),
        Some(rho_ui_proto::mirror::MirrorEvent::Results {
            results: Vec::new(),
            at: UnixMs(5),
        })
    );
}

#[tokio::test]
async fn terminal_preparation_failure_allows_fresh_input_and_explicit_retry() {
    for retry in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut agent = agent(directory.path()).await;
        agent.phase = Phase::Idle {
            owed: Vec::new(),
            standing: Standing::Asked,
        };
        agent.context_used = agent.session.auto_compact_token_limit();
        agent.start_request(UnixMs::now(), None).await;
        agent
            .fail(
                UnixMs(10),
                PendingInferenceResponse::default(),
                "terminal error".into(),
            )
            .await;
        assert!(agent.context.preparation.is_none());
        assert!(matches!(
            agent.decide(UnixMs(11)),
            Boundary::No { recheck: None }
        ));
        if retry {
            agent.handle_control(Control::Retry, UnixMs(12)).await;
        } else {
            agent
                .handle_control(Control::User(input("try again", 12), None), UnixMs(12))
                .await;
        }
        assert!(matches!(agent.decide(UnixMs(13)), Boundary::Now { .. }));
        agent.start_request(UnixMs(13), None).await;
        assert!(matches!(
            latest_send(&agent).0,
            Some(ContextChange::Preparing { repair: false, .. })
        ));
    }
}

#[tokio::test]
async fn manual_rotation_includes_recovery_without_losing_compact_request() {
    let directory = tempfile::tempdir().unwrap();
    let mut agent = agent(directory.path()).await;
    agent.phase = Phase::Idle {
        owed: Vec::new(),
        standing: Standing::Asked,
    };
    agent
        .recovery_notes
        .push("Rho restarted; inspect existing notes.".into());
    agent
        .handle_control(
            Control::User(
                QueuedInput {
                    kind: InputKind::Compaction,
                    ..input("", 1)
                },
                None,
            ),
            UnixMs(1),
        )
        .await;
    agent.start_request(UnixMs(2), None).await;
    let (change, blocks) = latest_send(&agent);
    assert!(matches!(
        change,
        Some(ContextChange::Preparing { repair: false, .. })
    ));
    assert!(blocks.iter().any(
        |block| matches!(block, ContextBlock::UserMessage { content, .. }
        if rho_core::text_content(content).contains("Rho restarted"))
    ));
    assert!(blocks.iter().any(
        |block| matches!(block, ContextBlock::DeveloperMessage { text }
        if text.contains("No early retention notice"))
    ));
    assert_eq!(agent.user.len(), 1);
    reply(&mut agent, vec![message("prepared")], 100).await;
    agent.start_request(UnixMs(3), None).await;
    assert!(
        latest_send(&agent)
            .1
            .iter()
            .any(|block| matches!(block, ContextBlock::ContextRotation { .. }))
    );
    assert!(agent.user.is_empty());
}

#[tokio::test]
async fn failed_preparation_without_headroom_rotates_without_repair() {
    let directory = tempfile::tempdir().unwrap();
    let mut agent = agent(directory.path()).await;
    agent.phase = Phase::Idle {
        owed: Vec::new(),
        standing: Standing::Asked,
    };
    agent.context_used = agent.session.auto_compact_token_limit();
    agent.start_request(UnixMs::now(), None).await;
    let used = agent.session.context_window().unwrap() - context::REPAIR_HEADROOM + 1;
    reply(
        &mut agent,
        vec![exec("prepare", "raise ValueError('failed write')")],
        used,
    )
    .await;
    cell_returned(&agent).await;
    agent.start_request(UnixMs::now(), None).await;
    assert!(
        latest_send(&agent)
            .1
            .iter()
            .any(|block| matches!(block, ContextBlock::ContextRotation { .. }))
    );
}

#[tokio::test]
async fn ordinary_python_writes_notes_without_a_prebound_variable() {
    let directory = tempfile::tempdir().unwrap();
    let mut agent = agent(directory.path()).await;
    let notes_path = directory.path().join("notes");
    std::fs::create_dir(&notes_path).unwrap();
    let source = format!(
        "assert 'notes' not in globals()\nfrom pathlib import Path\nPath({}).joinpath('progress.md').write_text('goal\\nnext step')",
        serde_json::to_string(&notes_path.to_string_lossy()).unwrap()
    );
    reply(&mut agent, vec![exec("write-notes", &source)], 100).await;
    cell_returned(&agent).await;
    assert!(!agent.latest_python_exec.as_ref().unwrap().1.facts().failed);
    let inventory = notes::inventory(&notes_path);
    assert!(inventory.contains("\"progress.md\" (2 lines, 14 bytes)"));
    assert!(!inventory.contains("next step"));
}
