use std::io;

use rho_agent::AgentStateKind;
use rho_cli_term_raw::{CursorShape, Term};
use rho_core::{
    AStr, StreamingContextItem, StreamingContextItemState, ToolCall, ToolCallId, ToolName,
    ToolOutput, ToolOutputStatus, ToolResult, ToolType,
};

use super::*;

fn chat_args() -> ChatArgs {
    ChatArgs {
        model: "gpt-test".to_owned(),
        auth: "default".to_owned(),
        session: DEFAULT_SESSION_NAME.to_owned(),
        session_path: None,
        prompt_stdin: false,
        no_store: false,
    }
}

#[test]
fn no_store_disables_inference_prompt_cache_key() {
    let mut args = chat_args();
    args.no_store = true;

    let service = build_inference_session(&args).unwrap();

    assert!(service.prompt_cache_key().is_none());
}

#[test]
fn stored_sessions_use_session_name_as_inference_prompt_cache_key() {
    let mut args = chat_args();
    args.session = "work".to_owned();

    let service = build_inference_session(&args).unwrap();

    assert_eq!(service.prompt_cache_key(), Some("work"));
}

fn test_call() -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from("call-1").unwrap(),
        name: ToolName::try_from("shell_command").unwrap(),
        tool_type: ToolType::Function,
        arguments: serde_json::json!({"command": "printf hi"}).to_string(),
    }
}

#[test]
fn streaming_tool_call_keeps_tool_block_live_until_turn_finish() {
    let (_term, handle, _input) = Term::new_virtual(
        80,
        24,
        prompt_text(),
        Box::new(io::sink()),
        CursorShape::Bar,
    );
    let mut renderer = StreamingRenderer::new(handle);
    let call = test_call();
    let mut rendered = 0;

    renderer.handle_state_kind(
        &AgentStateKind::ApiStreaming {
            pending_response: rho_core::PendingInferenceResponse {
                items: vec![StreamingContextItemState::Pending(
                    StreamingContextItem::ToolCall {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        tool_type: call.tool_type,
                        arguments: AStr::from(call.arguments.clone()),
                    },
                )],
            },
            previous_attempt: None,
        },
        &mut rendered,
    );
    assert_eq!(renderer.tool_blocks.len(), 1);

    renderer.render_tool_finished(&ToolResult {
        call_id: call.id,
        tool_type: ToolType::Function,
        body: ToolOutput {
            output: Arc::new("hi".to_owned()),
            status: ToolOutputStatus::Success,
        },
    });
    assert_eq!(renderer.tool_blocks.len(), 1);

    renderer.finish_turn();
    assert!(renderer.tool_blocks.is_empty());
}

#[test]
fn streaming_text_response_finalizes_on_idle() {
    let (_term, handle, _input) = Term::new_virtual(
        80,
        24,
        prompt_text(),
        Box::new(io::sink()),
        CursorShape::Bar,
    );
    let mut renderer = StreamingRenderer::new(handle);
    let mut rendered = 0;

    renderer.handle_state_kind(
        &AgentStateKind::ApiStreaming {
            pending_response: rho_core::PendingInferenceResponse {
                items: vec![StreamingContextItemState::Pending(
                    StreamingContextItem::AssistantMessage {
                        content: vec![AStr::from("done")],
                        phase: None,
                    },
                )],
            },
            previous_attempt: None,
        },
        &mut rendered,
    );
    assert!(renderer.assistant_block.is_some());

    renderer.handle_state_kind(&AgentStateKind::Idle, &mut rendered);
    assert!(renderer.assistant_block.is_none());
}
