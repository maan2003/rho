use rho_core::MessagePhase;

use super::streaming::tests::agent as standard_agent;
use super::*;
use crate::{ContextChange, WakeTrigger};

async fn agent(directory: &std::path::Path) -> super::streaming::tests::TestAgent {
    let agent = standard_agent(directory).await;
    agent.head.write().unwrap().config.role = AgentRole::Engineer {
        intelligence: EngineerIntelligence::HighNotes,
    };
    agent
}

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
        .await
        .unwrap();
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

async fn latest_send(
    agent: &super::streaming::tests::TestAgent,
) -> (Option<crate::ContextChange>, Vec<ContextBlock>) {
    agent.writer.flush().await.unwrap();
    let (_, events) = agent.db.read().agent_events(agent.agent_id);
    events
        .into_iter()
        .rev()
        .find_map(|event| {
            let native = event.native_event()?;
            match native {
                NativeEvent::RequestStarted { input, context, .. } => {
                    Some((context.clone(), input.clone()))
                }
                _ => None,
            }
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
        .await
        .unwrap();
    agent.start_request(UnixMs(2), None).await.unwrap();
    let (change, _) = latest_send(&agent).await;
    assert_eq!(change, Some(ContextChange::Marked { retain_from: 1 }));
    let marker = agent.context.marker.unwrap();
    reply(&mut agent, vec![message("working")], limit).await;

    agent
        .handle_control(
            Control::User(input("new task held during preparation", 3), None),
            UnixMs(3),
        )
        .await
        .unwrap();
    agent.start_request(UnixMs(4), None).await.unwrap();
    let (change, blocks) = latest_send(&agent).await;
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
        rho_core::context_window_start(&agent.provider_input().await.unwrap()),
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
    agent.start_request(UnixMs::now(), None).await.unwrap();
    assert_eq!(
        rho_core::context_window_start(&agent.provider_input().await.unwrap()),
        marker
    );
    assert!(agent.context.marker.is_none());
    assert!(agent.user.is_empty());
    assert_eq!(agent.context_used, None);
    assert!(
        latest_send(&agent)
            .await
            .1
            .iter()
            .any(|block| matches!(block, ContextBlock::ContextRotation { .. }))
    );
    assert!(
        matches!(
            &*agent.provider_input().await.unwrap()[0],
            ContextBlock::UserMessage { .. }
        ),
        "full history is untouched"
    );
    let (_, events) = agent.db.read().agent_events(agent.agent_id);
    let replayed = replay::replay(events);
    assert_eq!(rho_core::context_window_start(&replayed.history), marker);
    assert_eq!(replayed.history, agent.provider_input().await.unwrap());
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
    agent.start_request(UnixMs::now(), None).await.unwrap();
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
        .await
        .unwrap();
    assert!(matches!(agent.decide(UnixMs::now()), Boundary::No { .. }));
    cell_returned(&agent).await;
    assert!(matches!(agent.decide(UnixMs::now()), Boundary::Now { .. }));
    agent.start_request(UnixMs::now(), None).await.unwrap();
    assert!(matches!(
        latest_send(&agent).await.0,
        Some(ContextChange::Preparing { repair: true, .. })
    ));
    assert_eq!(agent.user.len(), 1);
    assert!(latest_send(&agent).await.1.iter().any(|block| matches!(block,
        ContextBlock::ToolResults { results } if results.iter().any(|result| result.body.output.contains("write failed")))));

    reply(
        &mut agent,
        vec![exec("repair", "raise ValueError('failed again')")],
        limit,
    )
    .await;
    cell_returned(&agent).await;
    agent.start_request(UnixMs::now(), None).await.unwrap();
    assert!(
        latest_send(&agent)
            .await
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
    agent.start_request(UnixMs::now(), None).await.unwrap();
    reply(&mut agent, vec![message("continue")], limit).await;
    agent.context_used = agent.session.auto_compact_token_limit();
    agent.start_request(UnixMs::now(), None).await.unwrap();
    reply(
        &mut agent,
        vec![exec("prepare", "assert remembered == 41\nremembered += 1")],
        limit,
    )
    .await;
    cell_returned(&agent).await;
    agent.start_request(UnixMs::now(), None).await.unwrap();
    assert!(
        latest_send(&agent)
            .await
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
    agent.start_request(UnixMs::now(), None).await.unwrap();
    reply(
        &mut agent,
        vec![exec("prepare", "import asyncio\nawait asyncio.sleep(60)")],
        limit,
    )
    .await;
    agent
        .handle_control(Control::User(input("held", 0), None), UnixMs(0))
        .await
        .unwrap();
    agent
        .handle_control(Control::Cancel, UnixMs::now())
        .await
        .unwrap();
    assert!(agent.context.preparation.is_none());
    assert!(agent.user.is_empty());
    assert!(matches!(
        agent.decide(UnixMs::now()),
        Boundary::No { recheck: None }
    ));
}

#[test]
fn preparation_mirror_does_not_drain_the_ui_queue() {
    let event = AgentEvent::Native(crate::native::NativeEvent::RequestStarted {
        context: Some(ContextChange::Preparing {
            retain_from: 0,
            repair: false,
        }),
        input: Vec::from(Vec::new()),
        at: UnixMs(5),
        wake: None,
    });
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
        agent.start_request(UnixMs::now(), None).await.unwrap();
        agent
            .fail(
                UnixMs(10),
                PendingInferenceResponse::default(),
                "terminal error".into(),
            )
            .await
            .unwrap();
        assert!(agent.context.preparation.is_none());
        assert!(matches!(
            agent.decide(UnixMs(11)),
            Boundary::No { recheck: None }
        ));
        if retry {
            agent
                .handle_control(Control::Retry, UnixMs(12))
                .await
                .unwrap();
        } else {
            agent
                .handle_control(Control::User(input("try again", 12), None), UnixMs(12))
                .await
                .unwrap();
        }
        assert!(matches!(agent.decide(UnixMs(13)), Boundary::Now { .. }));
        agent.start_request(UnixMs(13), None).await.unwrap();
        assert!(matches!(
            latest_send(&agent).await.0,
            Some(ContextChange::Preparing { repair: false, .. })
        ));
    }
}

#[tokio::test]
async fn manual_compaction_in_notes_role_uses_provider_and_cancels_rotation() {
    for preparing in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut agent = agent(directory.path()).await;
        agent.phase = Phase::Idle {
            owed: Vec::new(),
            standing: Standing::Asked,
        };
        let change = if preparing {
            ContextChange::Preparing {
                retain_from: 0,
                repair: false,
            }
        } else {
            ContextChange::Marked { retain_from: 0 }
        };
        agent
            .persist(AgentEvent::Native(
                crate::native::NativeEvent::RequestStarted {
                    input: Vec::from(vec![]),
                    context: Some(change.clone()),
                    at: UnixMs(1),
                    wake: None,
                },
            ))
            .await
            .unwrap();
        agent.context.sent(&change);
        agent.context_used = agent.session.auto_compact_token_limit();
        agent
            .handle_control(
                Control::User(
                    QueuedInput {
                        kind: InputKind::Compaction,
                        ..input("", 2)
                    },
                    None,
                ),
                UnixMs(2),
            )
            .await
            .unwrap();
        agent.start_request(UnixMs(3), None).await.unwrap();
        let (change, blocks) = latest_send(&agent).await;
        assert!(change.is_none());
        assert!(matches!(
            blocks.last(),
            Some(ContextBlock::CompactionTrigger)
        ));
        assert!(blocks.iter().any(|block| matches!(block,
            ContextBlock::DeveloperMessage { text } if text == context::MANUAL_COMPACTION)));
        assert!(agent.context.marker.is_none());
        assert!(agent.context.preparation.is_none());
        assert!(agent.user.is_empty());
        let (_, events) = agent.db.read().agent_events(agent.agent_id);
        let restored = replay::replay(events);
        assert!(restored.context.marker.is_none());
        assert!(restored.context.preparation.is_none());
        assert!(
            matches!(&agent.phase, Phase::Requesting(request) if !request.compaction_owes_reply)
        );
        agent.session.abort();
    }
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
    agent.start_request(UnixMs::now(), None).await.unwrap();
    let used = agent.session.context_window().unwrap() - context::REPAIR_HEADROOM + 1;
    reply(
        &mut agent,
        vec![exec("prepare", "raise ValueError('failed write')")],
        used,
    )
    .await;
    cell_returned(&agent).await;
    agent.start_request(UnixMs::now(), None).await.unwrap();
    assert!(
        latest_send(&agent)
            .await
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

#[tokio::test]
async fn ordinary_roles_keep_standard_manual_and_automatic_compaction() {
    for manual in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut agent = standard_agent(directory.path()).await;
        let limit = agent.session.auto_compact_token_limit().unwrap();
        agent.context_used = Some(if manual { 100 } else { limit });
        if manual {
            agent.user.push(QueuedInput {
                source: MessageSender::User,
                kind: InputKind::Compaction,
                delivery: MessageDelivery::NextRequest,
                at: UnixMs::now(),
            });
        }
        agent.start_request(UnixMs::now(), None).await.unwrap();
        assert!(matches!(
            &**agent.provider_input().await.unwrap().last().unwrap(),
            ContextBlock::CompactionTrigger
        ));
        assert!(agent.context.marker.is_none());
        assert!(agent.context.preparation.is_none());
        assert!(
            !agent
                .provider_input()
                .await
                .unwrap()
                .iter()
                .any(|block| matches!(&**block, ContextBlock::DeveloperMessage { .. }))
        );
        let Phase::Requesting(request) = &agent.phase else {
            panic!("expected request")
        };
        assert_eq!(request.compaction_owes_reply, !manual);
        agent.session.abort();
    }
}

#[tokio::test]
async fn role_switches_preserve_python_and_refresh_instructions() {
    let directory = tempfile::tempdir().unwrap();
    let mut agent = standard_agent(directory.path()).await;
    let surface = Arc::clone(&agent.surface);
    reply(
        &mut agent,
        vec![exec("initialize", "remembered = [41]")],
        100,
    )
    .await;

    for (index, intelligence) in [
        EngineerIntelligence::High,
        EngineerIntelligence::HighNotes,
        EngineerIntelligence::High,
        EngineerIntelligence::Cheap,
        EngineerIntelligence::Low,
        EngineerIntelligence::Medium,
    ]
    .into_iter()
    .enumerate()
    {
        cell_returned(&agent).await;
        assert!(
            !agent.latest_python_exec.as_ref().unwrap().1.facts().failed,
            "switch {index}: {:?}",
            agent
                .execs
                .values_mut()
                .map(|tool| tool.session.first_output())
                .collect::<Vec<_>>()
        );
        // Drain tool output, then settle the synthetic provider response.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                agent.start_request(UnixMs::now(), None).await.unwrap();
                agent.session.abort();
                reply(&mut agent, vec![message("done")], 100).await;
                if agent.execs.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let role = AgentRole::Engineer { intelligence };
        agent.change_role(role).await.unwrap();
        assert!(Arc::ptr_eq(&surface, &agent.surface));
        let instructions = agent
            .surface
            .get()
            .await
            .unwrap()
            .prompt
            .render(role)
            .await
            .unwrap();
        assert_eq!(instructions.notes.is_some(), role.uses_notes_rotation());
        assert_eq!(
            instructions.text.contains("# Notes and context rotation"),
            role.uses_notes_rotation()
        );
        assert_eq!(
            agent.model,
            role.session_profile().unwrap().deep_model().unwrap()
        );

        agent.start_request(UnixMs::now(), None).await.unwrap();
        reply(
            &mut agent,
            vec![exec(
                &format!("after-{index}"),
                &format!("assert remembered == [{}]\nremembered[0] += 1", 41 + index),
            )],
            100,
        )
        .await;
    }
    cell_returned(&agent).await;
    assert!(!agent.latest_python_exec.as_ref().unwrap().1.facts().failed);
}

#[tokio::test]
async fn role_switches_cancel_pending_rotation_durably() {
    let directory = tempfile::tempdir().unwrap();
    let mut agent = standard_agent(directory.path()).await;
    agent.phase = Phase::Idle {
        owed: Vec::new(),
        standing: Standing::Nothing,
    };
    let notes_role = AgentRole::Engineer {
        intelligence: EngineerIntelligence::HighNotes,
    };
    agent.change_role(notes_role).await.unwrap();
    assert_eq!(agent.model, InferenceModel::Gpt6Astra);

    // Model the marker and an interrupted preparation.
    let change = ContextChange::Preparing {
        retain_from: agent.provider_input().await.unwrap().len() as u64,
        repair: false,
    };
    let blocks = vec![ContextBlock::DeveloperMessage {
        text: context::MARKER.into(),
    }];
    agent
        .persist(AgentEvent::Native(
            crate::native::NativeEvent::RequestStarted {
                input: blocks.clone(),
                context: Some(change.clone()),
                at: UnixMs::now(),
                wake: None,
            },
        ))
        .await
        .unwrap();
    agent.context.sent(&change);
    agent
        .change_role(AgentRole::Engineer {
            intelligence: EngineerIntelligence::High,
        })
        .await
        .unwrap();
    assert!(agent.context.marker.is_none());
    assert!(agent.context.preparation.is_none());

    let (_, events) = agent.db.read().agent_events(agent.agent_id);
    let restored = replay::replay(events);
    assert_eq!(
        restored.history.len(),
        agent.provider_input().await.unwrap().len()
    );
    assert!(restored.context.marker.is_none());
    assert!(restored.context.preparation.is_none());
    assert!(restored.recovery_notes.is_empty());
    assert!(matches!(&**restored.history.last().unwrap(),
        ContextBlock::DeveloperMessage { text } if text == context::POLICY_CHANGED));
    agent.change_role(notes_role).await.unwrap();
    assert!(agent.context.marker.is_none());
    assert!(agent.context.preparation.is_none());
}

#[tokio::test]
async fn compaction_retries_preserve_task_obligations_and_manual_override() {
    for manual in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut agent = if manual {
            agent(directory.path()).await
        } else {
            standard_agent(directory.path()).await
        };
        agent.context_used = agent.session.auto_compact_token_limit();
        if manual {
            agent.user.push(QueuedInput {
                kind: InputKind::Compaction,
                ..input("", 1)
            });
        }
        agent.start_request(UnixMs::now(), None).await.unwrap();
        agent
            .handle(Event::Inference(InferenceEvent::TemporaryFailure {
                error: Arc::new(anyhow::anyhow!("disconnected")),
                retrying_at: std::time::Instant::now(),
            }))
            .await
            .unwrap();
        agent.start_request(UnixMs::now(), None).await.unwrap();
        assert!(agent.context.preparation.is_none());
        assert!(
            matches!(&agent.phase, Phase::Requesting(request) if request.compaction_owes_reply == !manual)
        );
        agent.session.abort();
        reply(
            &mut agent,
            vec![InferenceResponseItem::Compaction {
                provider_specific: Box::new(
                    rho_inference::OpenAiResponsesProviderData::Compaction {
                        item_id: "compaction".try_into().unwrap(),
                        encrypted_content: "summary".into(),
                    },
                ),
            }],
            100,
        )
        .await;
        assert!(
            matches!(
                &agent.phase,
                Phase::Idle {
                    standing: Standing::Asked,
                    ..
                }
            ) == !manual
        );
        if manual {
            // The fulfilled manual override must not disable future automatic rotation.
            agent.context_used = agent.session.auto_compact_token_limit();
            agent.start_request(UnixMs::now(), None).await.unwrap();
            assert!(agent.context.preparation.is_some());
            agent.session.abort();
        }
    }
}
