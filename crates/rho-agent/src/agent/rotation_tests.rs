use rho_core::MessagePhase;

use super::streaming::tests::agent as standard_agent;
use super::*;

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
        assert!(!instructions.text.contains("# Notes and context rotation"));
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
async fn eviction_preserves_input_and_history_and_falls_back_when_needed() {
    // The middle case can get below the compaction threshold but cannot reach
    // 40k: it must compact without committing a partial eviction.
    for (output_bytes, enough) in [(750000, true), (150000, false), (300, false)] {
        let directory = tempfile::tempdir().unwrap();
        let mut agent = agent(directory.path()).await;
        agent.phase = Phase::Idle {
            owed: Vec::new(),
            standing: Standing::Asked,
        };
        let old_id = ToolCallId::try_from("old").unwrap();
        let history = vec![
            Arc::new(ContextBlock::InferenceResponse {
                items: vec![exec("old", "print('old')")],
                provider_response_id: None,
            }),
            Arc::new(ContextBlock::ToolResults {
                results: vec![rho_core::ToolResult {
                    call_id: old_id.clone(),
                    tool_type: rho_core::ToolType::Custom,
                    started_at: UnixMs(0),
                    finished_at: UnixMs(1),
                    metadata: None,
                    body: ToolOutput {
                        output: Arc::new("x".repeat(output_bytes)),
                        full_output: None,
                        images: Default::default(),
                        status: ToolOutputStatus::Success,
                    },
                }],
            }),
            Arc::new(ContextBlock::DeveloperMessage {
                text: "recent".repeat(25000),
            }),
        ];
        agent.provider_history = Some(Vec::new());
        for block in &history {
            let event = if matches!(&**block, ContextBlock::InferenceResponse { .. }) {
                NativeEvent::ResponseFinished {
                    output: vec![(**block).clone()],
                    context_used: None,
                    usage: None,
                    at: UnixMs(0),
                }
            } else {
                NativeEvent::RequestStarted {
                    input: vec![(**block).clone()],
                    context: None,
                    wake: None,
                    at: UnixMs(0),
                }
            };
            agent.persist(AgentEvent::Native(event)).await.unwrap();
        }
        let limit = agent.session.auto_compact_token_limit().unwrap();
        agent.context_used = Some(limit + 1000);
        agent
            .handle_control(
                Control::User(input("continue with this constraint", 1), None),
                UnixMs(1),
            )
            .await
            .unwrap();
        agent.start_request(UnixMs(2), None).await.unwrap();
        let (change, blocks) = latest_send(&agent).await;
        assert_eq!(change, None);
        assert_eq!(blocks.iter().any(|b| matches!(b, ContextBlock::ToolHistoryEvicted { call_ids } if call_ids == &vec![old_id.clone()])), enough);
        assert_eq!(
            blocks.iter().any(
                |b| matches!(b, ContextBlock::DeveloperMessage { text } if text == context::EVICTED)
            ),
            enough
        );
        assert_eq!(blocks.contains(&ContextBlock::CompactionTrigger), !enough);
        if enough {
            assert!(agent.context_used.unwrap() <= 40000);
        } else {
            assert_eq!(agent.context_used, Some(limit + 1000));
        }
        assert!(blocks.iter().any(|b| matches!(b, ContextBlock::UserMessage { content, .. } if rho_core::text_content(content) == "continue with this constraint")));
        assert!(agent.user.is_empty());
        assert!(agent.context.preparation.is_none());
        assert_eq!(
            &agent.provider_input().await.unwrap()[..history.len()],
            &history
        );
        let (_, events) = agent.db.read().agent_events(agent.agent_id);
        let replayed = replay::replay(events);
        assert_eq!(
            replayed
                .history
                .iter()
                .any(|b| matches!(&**b, ContextBlock::ToolHistoryEvicted { .. })),
            enough
        );
        assert!(replayed.user.is_empty());
        assert_eq!(&replayed.history[..history.len()], &history);
        if !enough {
            reply(
                &mut agent,
                vec![InferenceResponseItem::Compaction {
                    provider_specific: Box::new(
                        rho_inference::OpenAiResponsesProviderData::Compaction {
                            item_id: rho_core::ProviderResponseItemId::try_from("compact").unwrap(),
                            encrypted_content: "summary".into(),
                        },
                    ),
                }],
                100,
            )
            .await;
            agent.start_request(UnixMs::now(), None).await.unwrap();
        }
        reply(&mut agent, vec![exec("recover-history", &format!(
            "original = next(item for item in transcript if item.kind == 'tool_result' and item.call_id == 'old')\nassert original.text == 'x' * {}\nassert any(item.kind == 'tool_history_evicted' for item in transcript) == {}",
            output_bytes, if enough { "True" } else { "False" }
        ))], 100).await;
        cell_returned(&agent).await;
        assert!(!agent.latest_python_exec.as_ref().unwrap().1.facts().failed);
    }
}

#[tokio::test]
async fn notes_role_without_evictable_tools_compacts_without_preparing() {
    let directory = tempfile::tempdir().unwrap();
    let mut agent = agent(directory.path()).await;
    agent.phase = Phase::Idle {
        owed: Vec::new(),
        standing: Standing::Asked,
    };
    agent.context_used = agent.session.auto_compact_token_limit();
    agent.start_request(UnixMs::now(), None).await.unwrap();
    let (change, blocks) = latest_send(&agent).await;
    assert_eq!(change, None);
    assert!(blocks.contains(&ContextBlock::CompactionTrigger));
    assert!(agent.context.preparation.is_none());
}

#[test]
fn eviction_is_oldest_first_bounded_and_preserves_live_unanswered_and_recent_calls() {
    let tool = |id: &str| {
        Arc::new(ContextBlock::InferenceResponse {
            items: vec![exec(id, "pass")],
            provider_response_id: None,
        })
    };
    let result = |id: &str, count: usize| {
        Arc::new(ContextBlock::ToolResults {
            results: vec![rho_core::ToolResult {
                call_id: ToolCallId::try_from(id).unwrap(),
                tool_type: rho_core::ToolType::Custom,
                body: ToolOutput {
                    output: Arc::new("x".repeat(count)),
                    full_output: None,
                    images: Default::default(),
                    status: ToolOutputStatus::Success,
                },
                started_at: UnixMs(0),
                finished_at: UnixMs(1),
                metadata: None,
            }],
        })
    };
    let id = |v: &str| ToolCallId::try_from(v).unwrap();
    let mut history = vec![
        tool("already"),
        result("already", 150000),
        tool("oldest"),
        result("oldest", 150000),
        tool("live"),
        result("live", 150000),
        tool("next"),
        result("next", 150000),
        tool("unanswered"),
        Arc::new(ContextBlock::ToolHistoryEvicted {
            call_ids: vec![id("already")],
        }),
        Arc::new(ContextBlock::DeveloperMessage {
            text: "r".repeat(120000),
        }),
        tool("recent"),
        result("recent", 300),
    ];
    let live = std::collections::BTreeSet::from([id("live")]);
    assert!(
        context::evict_tools(&history, &live, 40000)
            .call_ids
            .is_empty()
    );
    // "pass" costs ceil(4/3) + 8, and the result costs 150000/3 + 8:
    // one exchange frees 50018. At 90018 it reaches exactly 40000;
    // one token above that requires the next eligible exchange too.
    let above = context::evict_tools(&history, &live, 90019);
    assert_eq!(above.call_ids, vec![id("oldest"), id("next")]);
    assert_eq!(above.freed_tokens, 100036);
    let eviction = context::evict_tools(&history, &live, 90018);
    assert_eq!(
        eviction.call_ids,
        vec![id("oldest")],
        "stop once enough space has been freed"
    );
    assert_eq!(eviction.freed_tokens, 50018);
    history.push(Arc::new(ContextBlock::ToolHistoryEvicted {
        call_ids: eviction.call_ids,
    }));
    let next = context::evict_tools(&history, &live, 90018);
    assert_eq!(next.call_ids, vec![id("next")]);
    history.push(Arc::new(ContextBlock::ToolHistoryEvicted {
        call_ids: next.call_ids,
    }));
    assert!(
        context::evict_tools(&history, &live, 90018)
            .call_ids
            .is_empty()
    );
}

#[test]
fn eviction_does_not_count_summarized_tools_or_evict_a_recent_late_result() {
    let mut history = vec![Arc::new(ContextBlock::InferenceResponse {
        items: vec![
            exec("summarized", "old"),
            InferenceResponseItem::Compaction {
                provider_specific: Box::new(
                    rho_inference::OpenAiResponsesProviderData::Compaction {
                        item_id: rho_core::ProviderResponseItemId::try_from("compact").unwrap(),
                        encrypted_content: "summary".into(),
                    },
                ),
            },
        ],
        provider_response_id: None,
    })];
    for id in ["summarized", "late"] {
        if id == "late" {
            history.push(Arc::new(ContextBlock::InferenceResponse {
                items: vec![exec(id, "pass")],
                provider_response_id: None,
            }));
        }
        history.push(Arc::new(ContextBlock::ToolResults {
            results: vec![rho_core::ToolResult {
                call_id: ToolCallId::try_from(id).unwrap(),
                tool_type: rho_core::ToolType::Custom,
                body: ToolOutput {
                    output: Arc::new("x".repeat(150000)),
                    full_output: None,
                    images: Default::default(),
                    status: ToolOutputStatus::Success,
                },
                started_at: UnixMs(0),
                finished_at: UnixMs(1),
                metadata: None,
            }],
        }));
    }
    history.push(Arc::new(ContextBlock::DeveloperMessage {
        text: "r".repeat(120000),
    }));
    history.push(Arc::new(ContextBlock::ToolUpdate(rho_core::ToolUpdate {
        call_id: ToolCallId::try_from("late").unwrap(),
        tool_type: rho_core::ToolType::Custom,
        output: Arc::new("finished only recently".into()),
        full_output: None,
        status: Some(ToolOutputStatus::Success),
        images: Default::default(),
        at: UnixMs(10),
    })));
    assert!(
        context::evict_tools(&history, &Default::default(), 100000)
            .call_ids
            .is_empty()
    );
}
