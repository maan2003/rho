use std::io;

use rho_cli_term_raw::{CursorShape, Term};
use rho_core::{InferenceResponse, InferenceUpdate, ToolCall, ToolCallId, ToolResult, ToolType};

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

#[test]
fn inference_tool_call_response_keeps_tool_block_live_until_turn_finish() {
    let (_term, handle, _input) = Term::new_virtual(
        80,
        24,
        prompt_text(),
        Box::new(io::sink()),
        CursorShape::Bar,
    );
    let mut renderer = StreamingRenderer::new(handle);
    let call = ToolCall {
        id: ToolCallId("call-1".to_owned()),
        name: "shell_command".to_owned(),
        tool_type: ToolType::Function,
        arguments: serde_json::json!({"command": "printf hi"}),
    };

    renderer.handle_inference(InferenceUpdate::ToolCall {
        output_index: 0,
        call: call.clone(),
    });
    assert_eq!(renderer.tool_blocks.len(), 1);

    renderer.handle_inference(InferenceUpdate::Finished(InferenceResponse {
        items: vec![ItemKind::ToolCall(call.clone())],
        usage: None,
        provider_response_id: None,
    }));
    assert_eq!(renderer.tool_blocks.len(), 1);

    renderer.handle_agent(AgentUpdate::ToolCallStarted(call.clone()));
    assert_eq!(renderer.tool_blocks.len(), 1);
    renderer.handle_agent(AgentUpdate::ToolCallFinished(ToolResult::success(
        call.id, "hi",
    )));
    assert_eq!(renderer.tool_blocks.len(), 1);

    renderer.finish_turn();
    assert!(renderer.tool_blocks.is_empty());
}

#[test]
fn inference_text_response_finalizes_without_outer_loop() {
    let (_term, handle, _input) = Term::new_virtual(
        80,
        24,
        prompt_text(),
        Box::new(io::sink()),
        CursorShape::Bar,
    );
    let mut renderer = StreamingRenderer::new(handle);

    renderer.handle_inference(InferenceUpdate::TextDelta {
        output_index: 0,
        text: "done".to_owned(),
    });
    assert!(renderer.assistant_block.is_some());

    renderer.handle_inference(InferenceUpdate::Finished(InferenceResponse {
        items: Vec::new(),
        usage: None,
        provider_response_id: None,
    }));
    assert!(renderer.assistant_block.is_none());
}
