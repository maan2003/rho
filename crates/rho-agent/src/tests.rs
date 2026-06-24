use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use rho_core::{
    IInferenceSession, InferenceRequest, InferenceResponse, ItemKind, Message, ProviderItem,
    ProviderItemKind, Role, ToolCall, ToolCallId, ToolType,
};
use rho_store_cbor::CborLog;
use rho_store_redb::RedbLog;
use serde_json::json;

use super::*;

type UpdateStream = BoxStream<'static, Result<InferenceUpdate>>;

/// A test inference session driven through the `request`/`run` actor API: each
/// `request` installs a fresh update stream that `run` drains one item at a
/// time, pending once exhausted (mirroring a real idle session).
struct TestInferenceSession {
    make_stream: Arc<dyn Fn(InferenceRequest) -> UpdateStream + Send + Sync>,
    active: Option<UpdateStream>,
}

impl IInferenceSession for TestInferenceSession {
    fn request(&mut self, request: InferenceRequest) {
        self.active = Some((self.make_stream)(request));
    }

    fn run(&mut self) -> BoxFuture<'_, Result<InferenceUpdate>> {
        Box::pin(async move {
            let next = match self.active.as_mut() {
                Some(stream) => stream.next().await,
                None => None,
            };
            match next {
                Some(update) => update,
                None => {
                    self.active = None;
                    std::future::pending().await
                }
            }
        })
    }

    fn abort(&mut self) {
        self.active = None;
    }
}

fn test_session(
    make_stream: impl Fn(InferenceRequest) -> UpdateStream + Send + Sync + 'static,
) -> Box<dyn IInferenceSession> {
    Box::new(TestInferenceSession {
        make_stream: Arc::new(make_stream),
        active: None,
    })
}

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
) -> Box<dyn IInferenceSession> {
    let complete = Arc::new(complete);
    test_session(move |request| {
        let future = complete(request);
        futures::stream::once(async move { future.await.map(InferenceUpdate::Finished) }).boxed()
    })
}

fn test_streaming_provider(
    stream: impl Fn(InferenceRequest) -> UpdateStream + Send + Sync + 'static,
) -> Box<dyn IInferenceSession> {
    test_session(stream)
}

/// A provider whose turn never completes, so a test can cancel it mid-flight.
fn pending_provider() -> Box<dyn IInferenceSession> {
    test_streaming_provider(|_request| futures::stream::pending().boxed())
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

async fn wait_idle(agent: &Agent) {
    let (_, mut changes) = agent.subscribe_status();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut saw_running = false;
        while let Ok(status) = changes.recv().await {
            match status {
                AgentStatus::Running => saw_running = true,
                AgentStatus::Idle if saw_running => return,
                AgentStatus::Idle => {}
            }
        }
    })
    .await
    .expect("agent turn timed out");
}

#[tokio::test]
async fn records_message_and_provider_response() {
    let provider = test_provider(|_request| async { Ok(text_response("done")) }.boxed());
    let agent = Agent::builder(provider, Vec::new()).spawn();

    agent.send("hello");

    wait_idle(&agent).await;

    assert_eq!(test_items(&agent.blocks()).len(), 2);
}

#[tokio::test]
async fn drives_full_text_turn() {
    let provider = test_provider(|_request| async { Ok(text_response("done")) }.boxed());
    let agent = Agent::builder(provider, Vec::new()).spawn();

    agent.send("hello");

    wait_idle(&agent).await;

    assert_eq!(test_items(&agent.blocks()).len(), 2);
}

#[tokio::test]
async fn cancel_without_tool_calls_only_stops_turn() {
    let agent = Agent::builder(pending_provider(), Vec::new()).spawn();

    agent.send("hello");
    agent.cancel();
    wait_idle(&agent).await;

    assert_eq!(test_items(&agent.blocks()).len(), 1);
}

#[tokio::test]
async fn cancel_records_cancelled_tool_results() {
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
    let agent = Agent::builder(
        provider,
        vec![AgentTools::Shell(ShellTools::new(Duration::from_secs(120)))],
    )
    .spawn();

    agent.send("hello");
    agent.cancel();
    wait_idle(&agent).await;

    assert!(test_items(&agent.blocks()).iter().any(|item| {
        matches!(&item.kind, ItemKind::ToolResult(result) if matches!(&result.status, rho_core::ToolResultStatus::Cancelled { reason } if reason == "cancelled"))
    }));
}

#[tokio::test]
async fn does_not_call_provider_again_after_final_answer() {
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
    let agent = Agent::builder(provider, Vec::new()).spawn();

    agent.send("hello");

    wait_idle(&agent).await;

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(test_items(&agent.blocks()).len(), 2);
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
    let agent = Agent::builder(provider, Vec::new()).spawn();

    agent.send("first");

    wait_idle(&agent).await;
    agent.send("second");
    wait_idle(&agent).await;

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
    let agent = Agent::builder(provider, Vec::new()).spawn();

    agent.send("first");

    wait_idle(&agent).await;

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
    let path =
        temp_log_path("restored_provider_response_block_forwards_full_history_with_boundary");
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
    let agent = Agent::builder(provider, Vec::new())
        .with_store_loaded(AgentStore::CborLog(log))
        .await
        .unwrap()
        .spawn();

    agent.send("second");

    wait_idle(&agent).await;

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
    let agent = Agent::builder(provider, Vec::new()).spawn();

    agent.send("first");

    wait_idle(&agent).await;
    agent.send("second");
    wait_idle(&agent).await;

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
    let agent = Agent::builder(provider, Vec::new()).spawn();
    let (_, mut inference_updates) = agent.subscribe_inference_updates();
    tokio::spawn(async move {
        while let Ok(update) = inference_updates.recv().await {
            seen_updates
                .lock()
                .expect("provider update log lock")
                .push(update);
        }
    });

    agent.send("hello");

    wait_idle(&agent).await;

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
async fn records_final_response_not_streamed_deltas() {
    // The inference layer already assembles the full response, so the agent
    // records exactly the `Finished` items — the streamed delta is not
    // separately recorded (and so not duplicated).
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
    let agent = Agent::builder(provider, Vec::new()).spawn();

    agent.send("hello");

    wait_idle(&agent).await;

    let assistant_messages = test_items(&agent.blocks())
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
async fn provider_error_is_recorded_as_assistant_note() {
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
    let agent = Agent::builder(provider, Vec::new()).spawn();

    agent.send("hello");

    wait_idle(&agent).await;

    // The turn ends (agent idle) with the error surfaced in the conversation.
    assert_eq!(agent.subscribe_status().0, AgentStatus::Idle);
    assert!(
        matches!(test_items(&agent.blocks()).last().map(|item| &item.kind), Some(ItemKind::Message(message)) if message.role == Role::Assistant && message.text_content().contains("provider still down"))
    );
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
    let agent = Agent::builder(
        provider,
        vec![AgentTools::Shell(ShellTools::new(Duration::from_secs(2)))],
    )
    .spawn();

    agent.send("use a tool");

    wait_idle(&agent).await;

    assert!(
        test_items(&agent.blocks())
            .into_iter()
            .any(|item| matches!(&item.kind, ItemKind::ToolResult(_)))
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
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
    let agent = Agent::builder(
        provider,
        vec![AgentTools::Shell(ShellTools::new(Duration::from_secs(2)))],
    )
    .spawn();
    let (_, mut agent_updates) = agent.subscribe_agent_updates();
    tokio::spawn(async move {
        while let Ok(update) = agent_updates.recv().await {
            seen_updates
                .lock()
                .expect("agent update log lock")
                .push(update);
        }
    });

    agent.send("use a tool");

    wait_idle(&agent).await;

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
    let agent = Agent::builder(
        provider,
        vec![AgentTools::Shell(ShellTools::new(Duration::from_secs(2)))],
    )
    .spawn();

    agent.send("hello");

    wait_idle(&agent).await;

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
    let agent = Agent::builder(
        provider,
        vec![AgentTools::Shell(ShellTools::new(Duration::from_secs(2)))],
    )
    .spawn();

    agent.send("patch a file");

    wait_idle(&agent).await;

    assert_eq!(std::fs::read_to_string(path).unwrap(), "patched\n");
    assert!(test_items(&agent.blocks()).iter().any(|item| {
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
    let agent = Agent::builder(
        provider,
        vec![AgentTools::Shell(ShellTools::new(Duration::from_secs(3)))],
    )
    .spawn();

    let started = Instant::now();
    agent.send("use two tools");
    wait_idle(&agent).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_millis(1500),
        "tool calls were not scheduled concurrently: elapsed={elapsed:?}"
    );
    assert_eq!(
        test_items(&agent.blocks())
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
    let agent = Agent::builder(provider, Vec::new())
        .with_store(AgentStore::CborLog(log.clone()))
        .spawn();

    agent.send("hello");

    wait_idle(&agent).await;

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

    let agent = Agent::builder(provider, Vec::new())
        .with_store_loaded(AgentStore::CborLog(log))
        .await
        .unwrap()
        .spawn();

    assert_eq!(test_items(&agent.blocks()).len(), 1);
    assert!(
        matches!(&test_items(&agent.blocks())[0].kind, ItemKind::Message(message) if message.text_content() == "persisted")
    );
    let _ = tokio::fs::remove_file(&path).await;
}

#[tokio::test]
async fn persists_and_loads_transcript_items_from_redb_log() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("agent.redb");
    let log = RedbLog::open(&path).await.unwrap();
    let provider = test_provider(|_request| async { Ok(text_response("done")) }.boxed());
    let agent = Agent::builder(provider, Vec::new())
        .with_store(AgentStore::RedbLog(log.clone()))
        .spawn();

    agent.send("hello");

    wait_idle(&agent).await;

    let persisted = log.read_blocks().await.unwrap();
    assert_eq!(persisted, agent.blocks());

    let provider = test_provider(|_request| async { Ok(text_response("unused")) }.boxed());
    let loaded = Agent::builder(provider, Vec::new())
        .with_store_loaded(AgentStore::RedbLog(log))
        .await
        .unwrap()
        .spawn();

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

    let agent = Agent::builder(provider, Vec::new())
        .with_store_loaded(AgentStore::CborLog(log.clone()))
        .await
        .unwrap()
        .spawn();
    agent.send("second");
    wait_idle(&agent).await;

    {
        let requests = requests.lock().expect("request log lock");
        assert_eq!(requests[0].len(), 3);
    }
    let _ = tokio::fs::remove_file(&path).await;
}

fn temp_log_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("rho-agent-{name}-{}.cbor", std::process::id()))
}
