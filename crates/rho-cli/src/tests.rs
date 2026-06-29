use std::io;

use rho_cli_term_raw::{CursorShape, Term};
use rho_core::{ToolCall, ToolCallId, ToolName, ToolType};

use super::*;

fn test_call() -> ToolCall {
    ToolCall {
        id: ToolCallId::try_from("call-1").unwrap(),
        name: ToolName::try_from("shell_command").unwrap(),
        tool_type: ToolType::Function,
        arguments: serde_json::json!({"command": "printf hi"}).to_string(),
    }
}

fn streaming_state(items: Vec<UiStreamingItem>) -> UiAgentState {
    UiAgentState {
        blocks: Vec::new(),
        status: UiAgentStatus::Streaming,
        pending_response: items,
    }
}

fn status_state(status: UiAgentStatus) -> UiAgentState {
    UiAgentState {
        blocks: Vec::new(),
        status,
        pending_response: Vec::new(),
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
fn slash_command_completion_preserves_suffix() {
    let candidates = completion_candidates("  /ver now", "  /ver".len());
    assert!(candidates.iter().any(|candidate| {
        candidate.label == "/version" && candidate.replacement == "  /version now"
    }));
}

#[test]
fn slash_argument_completion_replaces_current_arg() {
    let candidates = completion_candidates("/theme tau-p", "/theme tau-p".len());
    assert!(candidates.iter().any(|candidate| {
        candidate.label == "tau-plain-dark" && candidate.replacement == "/theme tau-plain-dark"
    }));
}

#[test]
fn non_leading_absolute_path_completion_is_filesystem() {
    let candidates = completion_candidates("open /t", "open /t".len());
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.label == "/tmp/")
    );
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
fn ui_io_tracker_reports_rolling_max_rate() {
    let mut tracker = UiIoTracker::new(rho_ui_proto::IoStats {
        sent: 10,
        received: 20,
    });

    assert_eq!(
        tracker.sample(rho_ui_proto::IoStats {
            sent: 110,
            received: 70,
        }),
        UiIoRates {
            sent_per_sec: 100,
            received_per_sec: 50,
        }
    );
    assert_eq!(
        tracker.sample(rho_ui_proto::IoStats {
            sent: 120,
            received: 220,
        }),
        UiIoRates {
            sent_per_sec: 100,
            received_per_sec: 150,
        }
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
    renderer.handle_state(&streaming_state(vec![UiStreamingItem::ToolCall {
        id: call.id.as_str().to_owned(),
        name: call.name.as_str().to_owned(),
        arguments: call.arguments.clone(),
    }]));
    assert_eq!(renderer.active_blocks.len(), 1);

    renderer.render_tool_finished(&UiToolResult {
        call_id: call.id.as_str().to_owned(),
        status: UiToolStatus::Success,
    });
    assert_eq!(renderer.active_blocks.len(), 1);

    renderer.handle_state(&streaming_state(Vec::new()));
    assert_eq!(renderer.active_blocks.len(), 1);

    renderer.finish_turn();
    assert!(renderer.active_blocks.is_empty());
}

#[test]
fn streaming_blocks_follow_pending_item_order() {
    let (_term, handle, _input) = Term::new_virtual(
        80,
        24,
        prompt_text(),
        Box::new(io::sink()),
        CursorShape::Bar,
    );
    let mut renderer = StreamingRenderer::new(handle);
    let call = test_call();
    renderer.handle_state(&streaming_state(vec![
        UiStreamingItem::ToolCall {
            id: call.id.as_str().to_owned(),
            name: call.name.as_str().to_owned(),
            arguments: call.arguments.clone(),
        },
        UiStreamingItem::AssistantMessage {
            text: "done".to_owned(),
        },
    ]));

    assert_eq!(
        renderer.active_blocks.keys().copied().collect::<Vec<_>>(),
        vec![0, 1]
    );

    renderer.handle_state(&status_state(UiAgentStatus::ToolCalling {
        results: Vec::new(),
    }));
    renderer.handle_state(&streaming_state(vec![UiStreamingItem::AssistantMessage {
        text: "after tool".to_owned(),
    }]));
    assert_eq!(
        renderer.active_blocks.keys().copied().collect::<Vec<_>>(),
        vec![0, 1, 2]
    );

    renderer.finish_turn();
    assert!(renderer.active_blocks.is_empty());
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
    renderer.handle_state(&streaming_state(vec![UiStreamingItem::AssistantMessage {
        text: "done".to_owned(),
    }]));
    assert_eq!(renderer.active_blocks.len(), 1);

    renderer.handle_state(&status_state(UiAgentStatus::Idle));
    assert!(renderer.active_blocks.is_empty());
}
