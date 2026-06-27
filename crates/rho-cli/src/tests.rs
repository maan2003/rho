use std::io;

use rho_agent::AgentStateKind;
use rho_cli_term_raw::{CursorShape, Term};
use rho_core::{
    AStr, StreamingContextItem, StreamingContextItemState, ToolCall, ToolCallId, ToolName,
    ToolOutput, ToolOutputStatus, ToolResult, ToolType,
};

use super::*;

fn test_call() -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from("call-1").unwrap(),
        name: ToolName::try_from("shell_command").unwrap(),
        tool_type: ToolType::Function,
        arguments: serde_json::json!({"command": "printf hi"}).to_string(),
    }
}

#[test]
fn slash_completion_lists_matching_commands() {
    let candidates = completion_candidates("/c", 2);
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.label.as_str())
            .collect::<Vec<_>>(),
        ["/cancel", "/compact", "/clear"]
    );
}

#[test]
fn completion_includes_relative_paths() {
    let candidates = completion::completion_candidates("see ./Cargo", "see ./Cargo".len());
    assert!(candidates.iter().any(|candidate| {
        candidate.label == "./Cargo.toml" && candidate.replacement == "see ./Cargo.toml"
    }));
}

#[test]
fn slash_command_parse_known_and_unknown() {
    assert!(matches!(SlashCommand::parse("hello"), None));
    assert!(matches!(
        SlashCommand::parse("/quit"),
        Some(SlashCommand::Quit)
    ));
    assert!(matches!(
        SlashCommand::parse("/model"),
        Some(SlashCommand::Unsupported(command)) if command == "/model"
    ));
    assert!(matches!(
        SlashCommand::parse("/wat"),
        Some(SlashCommand::Unknown(command)) if command == "/wat"
    ));
}

#[test]
fn markdown_renderer_styles_common_markdown() {
    let rendered = markdown::markdown_text("# Title\nhello `code` and **bold**");
    let spans = rendered.spans();
    assert!(
        spans
            .iter()
            .any(|span| span.text == "Title" && span.style.bold)
    );
    assert!(
        spans.iter().any(|span| span.text == "code"
            && span.style.fg == Some(rho_cli_term_raw::Color::DarkYellow))
    );
    assert!(
        spans
            .iter()
            .any(|span| span.text == "bold" && span.style.bold)
    );
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
    renderer.handle_state_kind(&AgentStateKind::ApiStreaming {
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
    });
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
    renderer.handle_state_kind(&AgentStateKind::ApiStreaming {
        pending_response: rho_core::PendingInferenceResponse {
            items: vec![StreamingContextItemState::Pending(
                StreamingContextItem::AssistantMessage {
                    content: vec![AStr::from("done")],
                    phase: None,
                },
            )],
        },
        previous_attempt: None,
    });
    assert!(renderer.assistant_block.is_some());

    renderer.handle_state_kind(&AgentStateKind::Idle);
    assert!(renderer.assistant_block.is_none());
}
