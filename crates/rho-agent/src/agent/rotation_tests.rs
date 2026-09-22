use rho_core::MessagePhase;

use super::streaming::tests::agent as standard_agent;
use super::*;

async fn high_agent(directory: &std::path::Path) -> super::streaming::tests::TestAgent {
    let agent = standard_agent(directory).await;
    agent.head.write().unwrap().config.role = AgentRole::Engineer {
        intelligence: EngineerIntelligence::High,
    };
    agent
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
        EngineerIntelligence::Mini,
        EngineerIntelligence::High,
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
        context::evict_tools(&history, &live, 40000, &Default::default())
            .call_ids
            .is_empty()
    );
    // "pass" costs ceil(4/3) + 8, and the result costs 150000/3 + 8:
    // one exchange frees 50018. At 90018 it reaches exactly 40000;
    // one token above that requires the next eligible exchange too.
    let above = context::evict_tools(&history, &live, 90019, &Default::default());
    assert_eq!(above.call_ids, vec![id("oldest"), id("next")]);
    assert_eq!(above.freed_tokens, 100036);
    let eviction = context::evict_tools(&history, &live, 90018, &Default::default());
    assert_eq!(
        eviction.call_ids,
        vec![id("oldest")],
        "stop once enough space has been freed"
    );
    assert_eq!(eviction.freed_tokens, 50018);
    history.push(Arc::new(ContextBlock::ToolHistoryEvicted {
        call_ids: eviction.call_ids,
    }));
    let next = context::evict_tools(&history, &live, 90018, &Default::default());
    assert_eq!(next.call_ids, vec![id("next")]);
    history.push(Arc::new(ContextBlock::ToolHistoryEvicted {
        call_ids: next.call_ids,
    }));
    assert!(
        context::evict_tools(&history, &live, 90018, &Default::default())
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
        context::evict_tools(&history, &Default::default(), 100000, &Default::default())
            .call_ids
            .is_empty()
    );
}

fn measured_output_events(delta: i64) -> Vec<AgentEvent<'static>> {
    let response = |output, total, generated| {
        AgentEvent::Native(NativeEvent::ResponseFinished {
            output,
            context_used: Some(total),
            usage: Some(crate::db::AgentUsageBucket {
                model: crate::db::AgentUsageModel::ASTRA,
                // Billing input excludes cache hits; the cap must use raw
                // context_used minus generated output instead.
                input_tokens: 2000,
                cache_read_tokens: 8000,
                output_tokens: generated,
                requests: 1,
                ..Default::default()
            }),
            at: UnixMs(0),
        })
    };
    vec![
        response(
            vec![ContextBlock::InferenceResponse {
                items: vec![exec("first", "pass"), exec("second", "pass")],
                provider_response_id: None,
            }],
            11000,
            1000,
        ),
        AgentEvent::Native(NativeEvent::RequestStarted {
            input: vec![
                ContextBlock::ToolResults {
                    results: [("first", 2976), ("second", 5976)]
                        .into_iter()
                        .map(|(id, size)| rho_core::ToolResult {
                            call_id: ToolCallId::try_from(id).unwrap(),
                            tool_type: rho_core::ToolType::Custom,
                            body: ToolOutput {
                                output: Arc::new("x".repeat(size)),
                                full_output: None,
                                images: Default::default(),
                                status: ToolOutputStatus::Success,
                            },
                            started_at: UnixMs(0),
                            finished_at: UnixMs(1),
                            metadata: None,
                        })
                        .collect(),
                },
                ContextBlock::ToolUpdate(rho_core::ToolUpdate {
                    call_id: ToolCallId::try_from("first").unwrap(),
                    tool_type: rho_core::ToolType::Custom,
                    output: Arc::new("y".repeat(2976)),
                    full_output: None,
                    images: Default::default(),
                    status: None,
                    at: UnixMs(1),
                }),
                ContextBlock::DeveloperMessage {
                    text: "r".repeat(120000),
                },
            ],
            context: None,
            wake: None,
            at: UnixMs(1),
        }),
        response(vec![], (11500i64 + delta) as u64, 500),
    ]
}

#[test]
fn measured_output_budget_is_shared_fixed_and_never_inflates_estimates() {
    for (delta, expected) in [(801, 820), (0, 20), (-1, 4020), (5000, 4020)] {
        let mut replayed = replay::replay(measured_output_events(delta));
        let eviction = context::evict_tools(
            &replayed.history,
            &Default::default(),
            100000,
            &replayed.usage_caps,
        );
        // Output estimates are 1000 + 2000 + 1000. With an 801 budget
        // they become 200 + 400 + 200, plus two uncapped 10-token calls.
        assert_eq!(eviction.freed_tokens, expected, "delta={delta}");
        if delta != 801 {
            continue;
        }
        let first = context::evict_tools(
            &replayed.history,
            &Default::default(),
            40410,
            &replayed.usage_caps,
        );
        assert_eq!(
            first.call_ids,
            vec![ToolCallId::try_from("second").unwrap()]
        );
        assert_eq!(first.freed_tokens, 410);
        let marker = NativeEvent::RequestStarted {
            input: vec![ContextBlock::ToolHistoryEvicted {
                call_ids: first.call_ids,
            }],
            context: None,
            wake: None,
            at: UnixMs(2),
        };
        replayed.usage_caps.observe(&marker, &replayed.history);
        replayed
            .history
            .extend(marker.blocks().iter().cloned().map(Arc::new));
        let remaining = context::evict_tools(
            &replayed.history,
            &Default::default(),
            100000,
            &replayed.usage_caps,
        );
        assert_eq!(
            remaining.freed_tokens, 410,
            "do not reallocate the evicted share"
        );
    }
}

#[test]
fn measured_output_budget_requires_comparable_successful_samples() {
    for boundary in ["model", "approximate", "missing", "failure", "compaction"] {
        let mut events = measured_output_events(0);
        match boundary {
            "failure" => events.insert(
                1,
                AgentEvent::Native(NativeEvent::RequestFailed {
                    partial: Default::default(),
                    error: "failed".into(),
                    retrying: false,
                    at: UnixMs(1),
                }),
            ),
            "compaction" => {
                let AgentEvent::Native(NativeEvent::RequestStarted { input, .. }) = &mut events[1]
                else {
                    unreachable!()
                };
                input.insert(0, ContextBlock::CompactionTrigger);
            }
            _ => {
                let AgentEvent::Native(NativeEvent::ResponseFinished { usage, .. }) =
                    &mut events[2]
                else {
                    unreachable!()
                };
                match boundary {
                    "model" => usage.as_mut().unwrap().model = crate::db::AgentUsageModel::LUNA,
                    "approximate" => usage.as_mut().unwrap().approximate = true,
                    "missing" => *usage = None,
                    _ => unreachable!(),
                }
            }
        }
        let replayed = replay::replay(events);
        assert_eq!(
            context::evict_tools(
                &replayed.history,
                &Default::default(),
                100000,
                &replayed.usage_caps,
            )
            .freed_tokens,
            4020,
            "{boundary}"
        );
    }
}

#[tokio::test]
async fn live_and_replayed_output_caps_agree() {
    let directory = tempfile::tempdir().unwrap();
    let mut agent = high_agent(directory.path()).await;
    agent.provider_history = Some(Vec::new());
    for event in measured_output_events(801) {
        agent.persist(event).await.unwrap();
    }
    let live = context::evict_tools(
        &agent.provider_input().await.unwrap(),
        &Default::default(),
        100000,
        &agent.usage_caps,
    );
    assert_eq!(live.freed_tokens, 820);
    agent.provider_history = None;
    let reloaded = context::evict_tools(
        &agent.provider_input().await.unwrap(),
        &Default::default(),
        100000,
        &agent.usage_caps,
    );
    assert_eq!(reloaded.freed_tokens, 820);
    assert_eq!(live.call_ids, reloaded.call_ids);

    // Without the measured cap this output looks large enough to reach 40k.
    // The live request must instead compact, with no partial eviction.
    let directory = tempfile::tempdir().unwrap();
    let mut agent = self::high_agent(directory.path()).await;
    agent.provider_history = Some(Vec::new());
    for mut event in measured_output_events(801) {
        if let AgentEvent::Native(NativeEvent::RequestStarted { input, .. }) = &mut event {
            if let ContextBlock::ToolResults { results } = &mut input[0] {
                for result in results {
                    result.body.output = Arc::new(result.body.output.repeat(100));
                }
            }
        }
        agent.persist(event).await.unwrap();
    }
    let used = agent.session.auto_compact_token_limit().unwrap() + 1000;
    let history = agent.provider_input().await.unwrap();
    let naive = context::evict_tools(&history, &Default::default(), used, &Default::default());
    assert!(used.saturating_sub(naive.freed_tokens) <= 40000);
    agent.context_used = Some(used);
    agent.start_request(UnixMs(2), None).await.unwrap();
    let (_, blocks) = latest_send(&agent).await;
    assert!(blocks.contains(&ContextBlock::CompactionTrigger));
    assert!(
        !blocks
            .iter()
            .any(|b| matches!(b, ContextBlock::ToolHistoryEvicted { .. }))
    );
}
