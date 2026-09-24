//! What a call ran, drawn as it was run.
//!
//! A call's line shares its buffer with the model's markdown now, held in
//! a code span so the parser reads it as text. Two things can break that
//! from inside the call: backticks of its own, which would close the span
//! early, and a blank line, which would end the paragraph the span lives
//! in and leave the delimiters on screen.

use gpui::TestAppContext;

use super::{
    UiBlock, UiTool, UiToolStatus, agent, display_text, feed_frame, state, test_workspace, user,
};

fn ran(command: &str) -> UiBlock {
    UiBlock::Tool(UiTool {
        timing: Default::default(),
        id: "tool-1".to_owned(),
        name: "shell".to_owned(),
        arguments: serde_json::json!({ "command": command }).to_string(),
        format: rho_agent_host_proto::transcript::ArgumentsFormat::Json,
        preview: None,
        status: UiToolStatus::Success,
        output: None,
        error: None,
        started_at: Some(rho_agent_types::UnixMs(10)),
        finished_at: Some(rho_agent_types::UnixMs(20)),
        metadata: None,
    })
}

/// A command with backticks in it closes nothing early: the span is opened
/// with one more backtick than the longest run inside it.
#[gpui::test]
fn a_command_with_backticks_is_drawn_whole(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(
            Vec::new(),
            vec![user("go"), ran("echo `date` and ``x`` done")],
        ),
    );
    cx.run_until_parked();

    let text = display_text(&workspace, cx);
    assert!(
        text.contains("$ echo `date` and ``x`` done"),
        "the command's own backticks did not survive: {text:?}"
    );
}

/// A command holding a blank line keeps a buffer to itself, because a code
/// span cannot cross one.
#[gpui::test]
fn a_command_with_a_blank_line_keeps_its_own_buffer(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(Vec::new(), vec![user("go"), ran("echo one\n\necho two")]),
    );
    cx.run_until_parked();

    let editor = super::active_editor(&workspace, cx);
    let languageless = workspace
        .update(cx, |_, _, cx| {
            editor
                .read(cx)
                .buffer()
                .read(cx)
                .all_buffers()
                .into_iter()
                .filter(|buffer| {
                    let buffer = buffer.read(cx);
                    buffer.text().contains("echo one") && buffer.language().is_none()
                })
                .count()
        })
        .expect("inspect the buffers");
    assert_eq!(
        languageless, 1,
        "a call a code span cannot hold was left in a markdown buffer"
    );
}

#[gpui::test]
fn exec_provider_phases_reach_the_editor_without_execution_duration(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let UiBlock::Tool(mut tool) = ran("pass") else {
        unreachable!()
    };
    tool.name = "exec".into();
    tool.arguments = "print('hello')".into();
    tool.timing = rho_agent_types::ExecTiming {
        first_block_at: Some(rho_agent_types::UnixMs(100)),
        arguments_finished_at: Some(rho_agent_types::UnixMs(2100)),
        response_finished_at: Some(rho_agent_types::UnixMs(2600)),
        boundary_at: Some(rho_agent_types::UnixMs(5600)),
        handed_off_at: Some(rho_agent_types::UnixMs(5620)),
    };
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(Vec::new(), vec![user("go"), UiBlock::Tool(tool)]),
    );
    cx.run_until_parked();
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("args 2s response 500ms wait 3s handoff 20ms"),
        "{text:?}"
    );
}
