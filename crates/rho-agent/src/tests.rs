use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use rho_core::{
    InferenceResponse, ItemKind, Message, ProviderItem, ProviderItemKind, Role, ToolCall,
    ToolCallId, ToolType,
};
use rho_store_cbor::CborLog;
use rho_store_redb::RedbLog;
use serde_json::json;

use super::*;

fn test_items(blocks: &[ItemBlock]) -> Vec<&rho_core::Item> {
    blocks
        .iter()
        .flat_map(|block| match block {
            ItemBlock::Local { items } | ItemBlock::InferenceResponse { items, .. } => items,
        })
        .collect()
}

fn test_provider(
    complete: impl Fn(InferenceRequest) -> BoxFuture<'static, Result<InferenceResponse>>
    + Send
    + Sync
    + 'static,
) -> AgentInference {
    let complete = Arc::new(complete);
    AgentInference::Test {
        stream: Arc::new(move |request| {
            let future = complete(request);
            futures::stream::once(async move { future.await.map(InferenceUpdate::Finished) })
                .boxed()
        }),
    }
}

fn test_streaming_provider(
    stream: impl Fn(InferenceRequest) -> InferenceStream + Send + Sync + 'static,
) -> AgentInference {
    let stream = Arc::new(stream);
    AgentInference::Test {
        stream: Arc::new(move |request| stream(request)),
    }
}

fn text_response(content: impl Into<String>) -> InferenceResponse {
    InferenceResponse {
        items: vec![ItemKind::Message(Message::text(Role::Assistant, content))],
        usage: None,
        provider_response_id: None,
    }
}

fn tool_call_response(
    id: impl Into<String>,
    name: impl Into<String>,
    arguments: serde_json::Value,
) -> InferenceResponse {
    InferenceResponse {
        items: vec![ItemKind::ToolCall(ToolCall {
            id: ToolCallId(id.into()),
            name: name.into(),
            tool_type: ToolType::Function,
            arguments,
        })],
        usage: None,
        provider_response_id: None,
    }
}

fn custom_tool_call_response(
    id: impl Into<String>,
    name: impl Into<String>,
    input: impl Into<String>,
) -> InferenceResponse {
    InferenceResponse {
        items: vec![ItemKind::ToolCall(ToolCall {
            id: ToolCallId(id.into()),
            name: name.into(),
            tool_type: ToolType::Custom,
            arguments: json!(input.into()),
        })],
        usage: None,
        provider_response_id: None,
    }
}

fn has_tool_result(input: &[ItemBlock]) -> bool {
    input.iter().any(|block| match block {
        ItemBlock::Local { items } | ItemBlock::InferenceResponse { items, .. } => items
            .iter()
            .any(|item| matches!(item.kind, ItemKind::ToolResult(_))),
    })
}

#[tokio::test]
async fn records_message_and_provider_response() {
    let provider = test_provider(|_request| async { Ok(text_response("done")) }.boxed());
    let mut agent = Agent::new(provider, Vec::new());

    agent.push_user_message("hello");
    agent.step().await.unwrap();
    agent.step().await.unwrap();

    assert!(matches!(agent.state, AgentState::Idle));
    assert_eq!(test_items(agent.blocks()).len(), 2);
}

#[tokio::test]
async fn run_until_idle_drives_full_text_turn() {
    let provider = test_provider(|_request| async { Ok(text_response("done")) }.boxed());
    let mut agent = Agent::new(provider, Vec::new());

    agent.push_user_message("hello");
    let steps = agent.run_until_idle(4).await.unwrap();

    assert_eq!(steps, 2);
    assert!(agent.is_idle());
    assert_eq!(test_items(agent.blocks()).len(), 2);
}

#[tokio::test]
async fn run_until_idle_returns_zero_when_already_idle() {
    let provider = test_provider(|_request| async { Ok(text_response("done")) }.boxed());
    let mut agent = Agent::new(provider, Vec::new());

    let steps = agent.run_until_idle(4).await.unwrap();

    assert_eq!(steps, 0);
    assert!(agent.is_idle());
}

#[tokio::test]
async fn run_until_idle_errors_when_step_limit_is_exhausted() {
    let provider = test_provider(|_request| async { Ok(text_response("done")) }.boxed());
    let mut agent = Agent::new(provider, Vec::new());

    agent.push_user_message("hello");
    let error = agent.run_until_idle(1).await.unwrap_err();

    assert!(error.to_string().contains("within 1 steps"));
    assert!(!agent.is_idle());
}

#[tokio::test]
async fn cancel_current_turn_records_assistant_cancellation() {
    let provider = test_provider(|_request| async { Ok(text_response("done")) }.boxed());
    let mut agent = Agent::new(provider, Vec::new());

    agent.push_user_message("hello");
    agent.step().await.unwrap();
    agent.cancel_current_turn("cancelled").await.unwrap();

    assert!(agent.is_idle());
    assert!(
        matches!(test_items(agent.blocks()).last().map(|item| &item.kind), Some(ItemKind::Message(message)) if message.role == Role::Assistant && message.text_content() == "cancelled")
    );
}

#[tokio::test]
async fn cancel_current_turn_records_cancelled_tool_results() {
    let provider = test_provider(|_request| {
        async {
            Ok(tool_call_response(
                "call-1",
                "shell_command",
                json!({"command": "sleep 30"}),
            ))
        }
        .boxed()
    });
    let mut agent = Agent::new(
        provider,
        vec![AgentTools::Shell(ShellTools::new(Duration::from_secs(120)))],
    );

    agent.push_user_message("hello");
    agent.run_until_idle(2).await.unwrap_err();
    agent.cancel_current_turn("cancelled").await.unwrap();

    assert!(agent.is_idle());
    assert!(test_items(agent.blocks()).iter().any(|item| {
        matches!(&item.kind, ItemKind::ToolResult(result) if matches!(&result.status, rho_core::ToolResultStatus::Cancelled { reason } if reason == "cancelled"))
    }));
}

#[tokio::test]
async fn idle_step_after_final_answer_does_not_call_provider_again() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider_calls = Arc::clone(&calls);
    let provider = test_provider(move |_request| {
        let calls = Arc::clone(&provider_calls);
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(text_response("done"))
        }
        .boxed()
    });
    let mut agent = Agent::new(provider, Vec::new());

    agent.push_user_message("hello");
    agent.step().await.unwrap();
    agent.step().await.unwrap();
    agent.step().await.unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(test_items(agent.blocks()).len(), 2);
    assert!(matches!(agent.state, AgentState::Idle));
}

#[tokio::test]
async fn inference_request_keeps_full_block_history() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen_requests = Arc::clone(&requests);
    let provider = test_provider(move |request: InferenceRequest| {
        let seen_requests = Arc::clone(&seen_requests);
        async move {
            seen_requests
                .lock()
                .expect("request log lock")
                .push(request.input.clone());
            let mut response = text_response("done");
            response.provider_response_id = Some(format!(
                "resp_{}",
                seen_requests.lock().expect("request log lock").len()
            ));
            Ok(response)
        }
        .boxed()
    });
    let mut agent = Agent::new(provider, Vec::new());

    agent.push_user_message("first");
    agent.run_until_idle(4).await.unwrap();
    agent.push_user_message("second");
    agent.run_until_idle(4).await.unwrap();

    let requests = requests.lock().expect("request log lock");
    assert_eq!(requests[0].len(), 1);
    assert_eq!(requests[1].len(), 3);
    assert!(
        matches!(&requests[1][0], ItemBlock::Local { items } if matches!(&items[0].kind, ItemKind::Message(message) if message.text_content() == "first"))
    );
    assert!(
        matches!(&requests[1][2], ItemBlock::Local { items } if matches!(&items[0].kind, ItemKind::Message(message) if message.text_content() == "second"))
    );
}

#[tokio::test]
async fn records_provider_response_block() {
    let provider = test_provider(|_request| {
        async {
            let mut response = text_response("done");
            response.provider_response_id = Some("resp_1".to_owned());
            Ok(response)
        }
        .boxed()
    });
    let mut agent = Agent::new(provider, Vec::new());

    agent.push_user_message("first");
    agent.run_until_idle(4).await.unwrap();

    assert!(matches!(
        agent.blocks()[1],
        ItemBlock::InferenceResponse { .. }
    ));
}

#[tokio::test]
async fn restored_provider_response_block_forwards_full_history_with_boundary() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen_requests = Arc::clone(&requests);
    let provider = test_provider(move |request: InferenceRequest| {
        let seen_requests = Arc::clone(&seen_requests);
        async move {
            seen_requests
                .lock()
                .expect("request log lock")
                .push(request.input.clone());
            Ok(text_response("done"))
        }
        .boxed()
    });
    let mut agent = Agent::from_blocks(
        provider,
        Vec::new(),
        vec![
            ItemBlock::Local {
                items: vec![Item::message("item-0", Role::User, "first")],
            },
            ItemBlock::InferenceResponse {
                provider_response_id: Some("resp_1".to_owned()),
                items: vec![Item::message("item-1", Role::Assistant, "done")],
            },
        ],
    );

    agent.push_user_message("second");
    agent.run_until_idle(4).await.unwrap();

    let requests = requests.lock().expect("request log lock");
    assert_eq!(requests[0].len(), 3);
    assert!(
        matches!(&requests[0][0], ItemBlock::Local { items } if matches!(&items[0].kind, ItemKind::Message(message) if message.text_content() == "first"))
    );
    assert!(
        matches!(&requests[0][2], ItemBlock::Local { items } if matches!(&items[0].kind, ItemKind::Message(message) if message.text_content() == "second"))
    );
}

#[tokio::test]
async fn compaction_response_id_replays_full_history_on_next_turn() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen_requests = Arc::clone(&requests);
    let provider = test_provider(move |request: InferenceRequest| {
        let seen_requests = Arc::clone(&seen_requests);
        async move {
            let call_index = seen_requests.lock().expect("request log lock").len();
            seen_requests
                .lock()
                .expect("request log lock")
                .push(request.input.clone());
            if call_index == 0 {
                Ok(InferenceResponse {
                    items: vec![
                        ItemKind::ProviderItem(ProviderItem {
                            kind: ProviderItemKind::Compaction,
                            payload: json!({"type": "compaction", "id": "cmp_1"}),
                        }),
                        ItemKind::Message(Message::text(Role::Assistant, "compacted")),
                    ],
                    usage: None,
                    provider_response_id: Some("resp_compaction".to_owned()),
                })
            } else {
                Ok(text_response("done"))
            }
        }
        .boxed()
    });
    let mut agent = Agent::new(provider, Vec::new());

    agent.push_user_message("first");
    agent.run_until_idle(4).await.unwrap();
    agent.push_user_message("second");
    agent.run_until_idle(4).await.unwrap();

    let requests = requests.lock().expect("request log lock");
    assert_eq!(requests[1].len(), 3);
}

#[tokio::test]
async fn forwards_streaming_inference_updates() {
    let updates = Arc::new(Mutex::new(Vec::new()));
    let seen_updates = Arc::clone(&updates);
    let provider = test_streaming_provider(|_request| {
        futures::stream::iter([
            Ok(InferenceUpdate::TextDelta {
                output_index: 0,
                text: "do".to_owned(),
            }),
            Ok(InferenceUpdate::TextDelta {
                output_index: 0,
                text: "ne".to_owned(),
            }),
            Ok(InferenceUpdate::Finished(text_response("done"))),
        ])
        .boxed()
    });
    let mut agent = Agent::new(provider, Vec::new()).with_inference_updates(move |update| {
        seen_updates
            .lock()
            .expect("provider update log lock")
            .push(update);
    });

    agent.push_user_message("hello");
    agent.run_until_idle(4).await.unwrap();

    let updates = updates.lock().expect("provider update log lock");
    assert!(matches!(
        &updates[0],
        InferenceUpdate::TextDelta { output_index: 0, text } if text == "do"
    ));
    assert!(matches!(
        &updates[1],
        InferenceUpdate::TextDelta { output_index: 0, text } if text == "ne"
    ));
}

#[tokio::test]
async fn records_streamed_text_when_final_response_is_sparse() {
    let provider = test_streaming_provider(|_request| {
        futures::stream::iter([
            Ok(InferenceUpdate::TextDelta {
                output_index: 0,
                text: "do".to_owned(),
            }),
            Ok(InferenceUpdate::TextDelta {
                output_index: 0,
                text: "ne".to_owned(),
            }),
            Ok(InferenceUpdate::ReasoningTextDelta {
                output_index: 1,
                kind: ReasoningTextKind::Summary,
                text: "thought".to_owned(),
            }),
            Ok(InferenceUpdate::Finished(InferenceResponse {
                items: Vec::new(),
                usage: None,
                provider_response_id: None,
            })),
        ])
        .boxed()
    });
    let mut agent = Agent::new(provider, Vec::new());

    agent.push_user_message("hello");
    agent.run_until_idle(4).await.unwrap();

    assert!(test_items(agent.blocks()).iter().any(|item| {
        matches!(
            &item.kind,
            ItemKind::Message(message)
                if message.role == Role::Assistant && message.text_content() == "done"
        )
    }));
    assert!(test_items(agent.blocks()).iter().any(|item| {
        matches!(
            &item.kind,
            ItemKind::ReasoningText(reasoning)
                if reasoning.kind == ReasoningTextKind::Summary && reasoning.text == "thought"
        )
    }));
}

#[tokio::test]
async fn streamed_text_does_not_duplicate_final_response_items() {
    let provider = test_streaming_provider(|_request| {
        futures::stream::iter([
            Ok(InferenceUpdate::TextDelta {
                output_index: 0,
                text: "done".to_owned(),
            }),
            Ok(InferenceUpdate::Finished(text_response("done"))),
        ])
        .boxed()
    });
    let mut agent = Agent::new(provider, Vec::new());

    agent.push_user_message("hello");
    agent.run_until_idle(4).await.unwrap();

    let assistant_messages = test_items(agent.blocks())
        .into_iter()
        .filter(|item| {
            matches!(
                &item.kind,
                ItemKind::Message(message)
                    if message.role == Role::Assistant && message.text_content() == "done"
            )
        })
        .count();
    assert_eq!(assistant_messages, 1);
}

#[tokio::test]
async fn provider_errors_return_to_caller() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider_calls = Arc::clone(&calls);
    let provider = test_provider(move |_request| {
        let calls = Arc::clone(&provider_calls);
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(anyhow!("provider still down"))
        }
        .boxed()
    });
    let mut agent = Agent::new(provider, Vec::new());

    agent.push_user_message("hello");
    agent.step().await.unwrap();
    let error = agent.step().await.unwrap_err();

    assert!(error.to_string().contains("provider still down"));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn runs_tool_calls_through_agent_policy() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider_calls = Arc::clone(&calls);
    let provider = test_provider(move |request: InferenceRequest| {
        let calls = Arc::clone(&provider_calls);
        async move {
            calls.fetch_add(1, Ordering::SeqCst);
            if has_tool_result(&request.input) {
                Ok(text_response("done"))
            } else {
                Ok(tool_call_response(
                    "call-1",
                    "shell_command",
                    json!({"command": "printf tool-output"}),
                ))
            }
        }
        .boxed()
    });
    let mut agent = Agent::new(
        provider,
        vec![AgentTools::Shell(ShellTools::new(Duration::from_secs(2)))],
    );

    agent.push_user_message("use a tool");
    agent.step().await.unwrap();
    agent.step().await.unwrap();
    agent.step().await.unwrap();
    agent.step().await.unwrap();

    assert!(
        test_items(agent.blocks())
            .into_iter()
            .any(|item| matches!(&item.kind, ItemKind::ToolResult(_)))
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(matches!(agent.state, AgentState::Idle));
}

#[tokio::test]
async fn emits_tool_execution_updates() {
    let provider = test_provider(|request: InferenceRequest| {
        async move {
            if has_tool_result(&request.input) {
                Ok(text_response("done"))
            } else {
                Ok(tool_call_response(
                    "call-1",
                    "shell_command",
                    json!({"command": "printf tool-output"}),
                ))
            }
        }
        .boxed()
    });
    let updates = Arc::new(Mutex::new(Vec::new()));
    let seen_updates = Arc::clone(&updates);
    let mut agent = Agent::new(
        provider,
        vec![AgentTools::Shell(ShellTools::new(Duration::from_secs(2)))],
    )
    .with_agent_updates(move |update| {
        seen_updates
            .lock()
            .expect("agent update log lock")
            .push(update);
    });

    agent.push_user_message("use a tool");
    agent.run_until_idle(6).await.unwrap();

    let updates = updates.lock().expect("agent update log lock");
    assert!(matches!(
        &updates[0],
        AgentUpdate::ToolCallStarted(call)
            if call.id == ToolCallId("call-1".to_owned()) && call.name == "shell_command"
    ));
    assert!(matches!(
        &updates[1],
        AgentUpdate::ToolCallFinished(result)
            if result.call_id == ToolCallId("call-1".to_owned())
                && result.rendered_output().contains("tool-output")
    ));
}

#[tokio::test]
async fn exposes_shell_command_and_apply_patch_tool_specs() {
    let seen_tools = Arc::new(Mutex::new(Vec::new()));
    let captured_tools = Arc::clone(&seen_tools);
    let provider = test_provider(move |request: InferenceRequest| {
        let captured_tools = Arc::clone(&captured_tools);
        async move {
            captured_tools
                .lock()
                .expect("tool log lock")
                .push(request.tools);
            Ok(text_response("done"))
        }
        .boxed()
    });
    let mut agent = Agent::new(
        provider,
        vec![AgentTools::Shell(ShellTools::new(Duration::from_secs(2)))],
    );

    agent.push_user_message("hello");
    agent.run_until_idle(4).await.unwrap();

    let seen_tools = seen_tools.lock().expect("tool log lock");
    let tools = seen_tools.first().expect("provider request tools");
    assert_eq!(tools.len(), 2);
    assert!(tools.iter().any(|tool| {
        tool.name == "shell_command"
            && tool.tool_type == ToolType::Function
            && tool.format.is_none()
    }));
    assert!(tools.iter().any(|tool| {
        tool.name == "apply_patch" && tool.tool_type == ToolType::Custom && tool.format.is_some()
    }));
}

#[tokio::test]
async fn routes_apply_patch_custom_tool_through_agent_policy() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("agent-patch.txt");
    let patch = format!(
        "*** Begin Patch\n*** Add File: {}\n+patched\n*** End Patch",
        path.display()
    );
    let provider = test_provider(move |request: InferenceRequest| {
        let patch = patch.clone();
        async move {
            if has_tool_result(&request.input) {
                Ok(text_response("done"))
            } else {
                Ok(custom_tool_call_response("call-1", "apply_patch", patch))
            }
        }
        .boxed()
    });
    let mut agent = Agent::new(
        provider,
        vec![AgentTools::Shell(ShellTools::new(Duration::from_secs(2)))],
    );

    agent.push_user_message("patch a file");
    agent.run_until_idle(6).await.unwrap();

    assert_eq!(std::fs::read_to_string(path).unwrap(), "patched\n");
    assert!(test_items(agent.blocks()).iter().any(|item| {
        matches!(
            &item.kind,
            ItemKind::ToolResult(result)
                if result.tool_type == ToolType::Custom
                    && result.rendered_output().contains("A ")
        )
    }));
}

#[tokio::test]
async fn waits_for_tool_calls_concurrently() {
    let provider = test_provider(|request: InferenceRequest| {
        async move {
            if has_tool_result(&request.input) {
                Ok(text_response("done"))
            } else {
                Ok(InferenceResponse {
                    items: vec![
                        ItemKind::ToolCall(ToolCall {
                            id: ToolCallId("call-1".to_owned()),
                            name: "shell_command".to_owned(),
                            tool_type: ToolType::Function,
                            arguments: json!({"command": "sleep 1; printf one"}),
                        }),
                        ItemKind::ToolCall(ToolCall {
                            id: ToolCallId("call-2".to_owned()),
                            name: "shell_command".to_owned(),
                            tool_type: ToolType::Function,
                            arguments: json!({"command": "sleep 1; printf two"}),
                        }),
                    ],
                    usage: None,
                    provider_response_id: None,
                })
            }
        }
        .boxed()
    });
    let mut agent = Agent::new(
        provider,
        vec![AgentTools::Shell(ShellTools::new(Duration::from_secs(3)))],
    );

    agent.push_user_message("use two tools");
    agent.step().await.unwrap();
    agent.step().await.unwrap();

    let started = Instant::now();
    agent.step().await.unwrap();
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(1500),
        "tool calls were not scheduled concurrently: elapsed={elapsed:?}"
    );
    assert_eq!(
        test_items(agent.blocks())
            .into_iter()
            .filter(|item| matches!(&item.kind, ItemKind::ToolResult(_)))
            .count(),
        2
    );
}

#[tokio::test]
async fn persists_transcript_items_to_cbor_log() {
    let path = temp_log_path("persists_transcript_items_to_cbor_log");
    let _ = tokio::fs::remove_file(&path).await;
    let log = CborLog::new(&path);
    let provider = test_provider(|_request| async { Ok(text_response("done")) }.boxed());
    let mut agent = Agent::new(provider, Vec::new()).with_store(AgentStore::CborLog(log.clone()));

    agent.push_user_message("hello");
    agent.step().await.unwrap();
    agent.step().await.unwrap();

    let persisted = log.read_blocks().await.unwrap();
    assert_eq!(persisted, agent.blocks());
    let _ = tokio::fs::remove_file(&path).await;
}

#[tokio::test]
async fn loads_transcript_items_from_cbor_log() {
    let path = temp_log_path("loads_transcript_items_from_cbor_log");
    let _ = tokio::fs::remove_file(&path).await;
    let log = CborLog::new(&path);
    log.append_block(&ItemBlock::Local {
        items: vec![Item::message("item-0", Role::User, "persisted")],
    })
    .await
    .unwrap();
    let provider = test_provider(|_request| async { Ok(text_response("done")) }.boxed());

    let agent = Agent::from_store(provider, Vec::new(), AgentStore::CborLog(log))
        .await
        .unwrap();

    assert_eq!(test_items(agent.blocks()).len(), 1);
    assert!(
        matches!(&test_items(agent.blocks())[0].kind, ItemKind::Message(message) if message.text_content() == "persisted")
    );
    let _ = tokio::fs::remove_file(&path).await;
}

#[tokio::test]
async fn persists_and_loads_transcript_items_from_redb_log() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("agent.redb");
    let log = RedbLog::open(&path).await.unwrap();
    let provider = test_provider(|_request| async { Ok(text_response("done")) }.boxed());
    let mut agent = Agent::new(provider, Vec::new()).with_store(AgentStore::RedbLog(log.clone()));

    agent.push_user_message("hello");
    agent.run_until_idle(4).await.unwrap();

    let persisted = log.read_blocks().await.unwrap();
    assert_eq!(persisted, agent.blocks());

    let provider = test_provider(|_request| async { Ok(text_response("unused")) }.boxed());
    let loaded = Agent::from_store(provider, Vec::new(), AgentStore::RedbLog(log))
        .await
        .unwrap();

    assert_eq!(loaded.blocks(), persisted);
}

#[tokio::test]
async fn loaded_transcript_replays_full_history_with_provider_block_boundary() {
    let path = temp_log_path("loaded_transcript_replays_full_history_with_provider_block_boundary");
    let _ = tokio::fs::remove_file(&path).await;
    let log = CborLog::new(&path);
    log.append_block(&ItemBlock::Local {
        items: vec![Item::message("item-0", Role::User, "first")],
    })
    .await
    .unwrap();
    log.append_block(&ItemBlock::InferenceResponse {
        provider_response_id: Some("resp_1".to_owned()),
        items: vec![Item::message("item-1", Role::Assistant, "done")],
    })
    .await
    .unwrap();

    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen_requests = Arc::clone(&requests);
    let provider = test_provider(move |request: InferenceRequest| {
        let seen_requests = Arc::clone(&seen_requests);
        async move {
            seen_requests
                .lock()
                .expect("request log lock")
                .push(request.input.clone());
            Ok(text_response("done"))
        }
        .boxed()
    });

    let mut agent = Agent::from_store(provider, Vec::new(), AgentStore::CborLog(log.clone()))
        .await
        .unwrap();
    agent.push_user_message("second");
    agent.run_until_idle(4).await.unwrap();

    {
        let requests = requests.lock().expect("request log lock");
        assert_eq!(requests[0].len(), 3);
    }
    let _ = tokio::fs::remove_file(&path).await;
}

fn temp_log_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("rho-agent-{name}-{}.cbor", std::process::id()))
}
