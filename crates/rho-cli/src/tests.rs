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

fn tool_block(status: UiToolStatus) -> UiBlock {
    let call = test_call();
    UiBlock::Tool(UiTool {
        id: call.id.as_str().to_owned(),
        name: call.name.as_str().to_owned(),
        arguments: call.arguments.clone(),
        preview: None,
        status,
        output: None,
        error: None,
        started_at: None,
        finished_at: None,
        metadata: None,
    })
}

fn assistant(text: &str) -> UiBlock {
    UiBlock::AssistantMessage {
        text: text.to_owned(),
        phase: None,
    }
}

fn agent_state(blocks: Vec<UiBlock>, status: UiAgentStatus) -> UiAgentState {
    UiAgentState { blocks, status }
}

fn streaming_state(blocks: Vec<UiBlock>) -> UiAgentState {
    agent_state(blocks, UiAgentStatus::Streaming)
}

fn completion_candidates(buffer: &str, cursor: usize) -> Vec<rho_cli_term_raw::Candidate> {
    completion::completion_candidates(buffer, cursor, &[], &[])
}

#[test]
fn colon_completion_lists_matching_commands() {
    let candidates = completion_candidates(":c", 2);
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.label.as_str())
            .collect::<Vec<_>>(),
        ["cancel", "clear"]
    );
}

#[test]
fn completion_includes_relative_paths() {
    let candidates = completion_candidates("see ./Cargo", "see ./Cargo".len());
    assert!(candidates.iter().any(|candidate| {
        candidate.label == "./Cargo.toml" && candidate.replacement == "see ./Cargo.toml"
    }));
}

#[test]
fn command_completion_preserves_suffix() {
    let candidates = completion_candidates("  :ver now", "  :ver".len());
    assert!(candidates.iter().any(|candidate| {
        candidate.label == "version" && candidate.replacement == "  :version now"
    }));
}

#[test]
fn argument_completion_uses_live_workdirs() {
    let workdirs = vec![("rho".to_owned(), "/home/u/src/rho".to_owned())];
    let buffer = ":agent new rh";
    let candidates = completion::completion_candidates(buffer, buffer.len(), &workdirs, &[]);
    assert!(candidates.iter().any(|candidate| {
        candidate.label == "rho" && candidate.replacement == ":agent new rho"
    }));
}

#[test]
fn command_path_argument_completion_is_filesystem() {
    let buffer = ":workdirs add /t";
    let candidates = completion_candidates(buffer, buffer.len());
    assert!(candidates.iter().any(|candidate| {
        candidate.label == "/tmp/" && candidate.replacement == ":workdirs add /tmp/"
    }));
}

#[test]
fn command_parse_known_and_unknown() {
    use rho_commands::{Command, Parsed, parse};
    assert!(matches!(parse("hello"), None));
    assert!(matches!(
        parse(":quit"),
        Some(Parsed::Command(Command::Quit))
    ));
    assert!(matches!(
        parse(":wat"),
        Some(Parsed::Unknown(command)) if command == ":wat"
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
fn user_message_renderer_uses_theme_green() {
    let block = user_message_block("hello");
    let spans = block.content.spans();
    assert_eq!(spans.len(), 2);
    assert_eq!(spans[0].style.fg, Some(rho_cli_term_raw::Color::Green));
    assert!(spans[0].style.bold);
    assert_eq!(spans[1].style.fg, Some(rho_cli_term_raw::Color::Green));
}

#[test]
fn shell_command_tool_call_renders_as_grey_prompt() {
    let block = tool_call_block(
        "shell_command",
        &serde_json::json!({ "command": "printf hi" }).to_string(),
        ToolRenderStatus::Running,
    );
    let spans = block.content.spans();

    assert_eq!(
        spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>(),
        ["$ ", "printf hi", " …"]
    );
    assert_eq!(spans[0].style.fg, Some(rho_cli_term_raw::Color::DarkGreen));
    assert_eq!(spans[1].style.fg, None);
    assert_eq!(spans[2].style.fg, Some(rho_cli_term_raw::Color::DarkGreen));
    assert!(spans.iter().all(|span| !span.style.bold));
}

#[test]
fn apply_patch_tool_call_renders_as_edit_without_status() {
    let block = tool_call_block(
        "apply_patch",
        "*** Begin Patch\n*** End Patch",
        ToolRenderStatus::Running,
    );
    let spans = block.content.spans();

    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].text, "edit");
    assert_eq!(spans[0].style.fg, Some(rho_cli_term_raw::Color::DarkGreen));
    assert!(!spans[0].style.bold);
}

#[test]
fn successful_shell_command_omits_subsecond_status() {
    let block = tool_call_block(
        "shell_command",
        &serde_json::json!({ "command": "printf hi" }).to_string(),
        ToolRenderStatus::Done(ToolOutputStatus::Success),
    );
    let spans = block.content.spans();

    assert_eq!(
        spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>(),
        ["$ ", "printf hi"]
    );
}

#[test]
fn failed_shell_command_keeps_error_status() {
    let block = tool_call_block(
        "shell_command",
        &serde_json::json!({ "command": "false" }).to_string(),
        ToolRenderStatus::Done(ToolOutputStatus::Error),
    );
    let spans = block.content.spans();

    assert_eq!(
        spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<Vec<_>>(),
        ["$ ", "false", " x"]
    );
    assert_eq!(spans[2].style.fg, Some(rho_cli_term_raw::Color::Red));
}

#[test]
fn streaming_reasoning_is_not_rendered() {
    let (_term, handle, _input) = Term::new_virtual(
        80,
        24,
        prompt_text(),
        Box::new(io::sink()),
        CursorShape::Bar,
    );
    let mut renderer = StreamingRenderer::new(handle);
    renderer.handle_state(&streaming_state(vec![UiBlock::Reasoning {
        text: "internal reasoning".to_owned(),
    }]));

    assert!(renderer.active_blocks.is_empty());
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
    renderer.handle_state(&streaming_state(vec![tool_block(UiToolStatus::Running)]));
    assert_eq!(renderer.active_blocks.len(), 1);

    // The tool call seals and finishes; the same index updates in place.
    renderer.handle_state(&agent_state(
        vec![tool_block(UiToolStatus::Success)],
        UiAgentStatus::ToolCalling,
    ));
    assert_eq!(renderer.active_blocks.len(), 1);

    renderer.handle_state(&agent_state(
        vec![tool_block(UiToolStatus::Success)],
        UiAgentStatus::Streaming,
    ));
    assert_eq!(renderer.active_blocks.len(), 1);

    renderer.finish_turn();
    assert!(renderer.active_blocks.is_empty());
}

#[test]
fn turn_blocks_stay_live_across_tool_call_legs() {
    let (_term, handle, _input) = Term::new_virtual(
        80,
        24,
        prompt_text(),
        Box::new(io::sink()),
        CursorShape::Bar,
    );
    let mut renderer = StreamingRenderer::new(handle);
    renderer.handle_state(&streaming_state(vec![
        tool_block(UiToolStatus::Running),
        assistant("done"),
    ]));

    assert_eq!(
        renderer.active_blocks.keys().copied().collect::<Vec<_>>(),
        vec![0, 1]
    );

    renderer.handle_state(&agent_state(
        vec![tool_block(UiToolStatus::Success), assistant("done")],
        UiAgentStatus::ToolCalling,
    ));
    renderer.handle_state(&agent_state(
        vec![
            tool_block(UiToolStatus::Success),
            assistant("done"),
            assistant("after tool"),
        ],
        UiAgentStatus::Streaming,
    ));
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
    renderer.handle_state(&streaming_state(vec![assistant("done")]));
    assert_eq!(renderer.active_blocks.len(), 1);

    renderer.handle_state(&agent_state(
        vec![assistant("done")],
        UiAgentStatus::Idle,
    ));
    assert!(renderer.active_blocks.is_empty());
}
