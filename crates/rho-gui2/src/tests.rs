//! End-to-end tests: synthetic protocol frames in, rendered editor state out.

use std::str::FromStr as _;

use editor::Editor;
use editor::display_map::{Block, DisplayRow};
use gpui::{App, Entity, Focusable as _, TestAppContext, WindowHandle};
use rho_core::UnixMs;
use rho_ui_proto::AgentId;
use rho_ui_proto::remote::{
    AgentRemoteFrame, UiAgentState, UiAgentStatus, UiBlock, UiBlocksDiff, UiMessagePhase,
    UiPendingResponseDiff, UiStreamingItem, UiStreamingItemDiff, UiStreamingItemUpdate,
    UiTextDiff, UiTool, UiToolStatus,
};
use settings::SettingsStore;

use crate::connection::ConnEvent;
use crate::workspace::{AttachTarget, Workspace};

fn init_test_app(cx: &mut App) {
    assets::Assets.load_test_fonts(cx);
    let store = SettingsStore::new(cx, settings::default_settings().as_ref());
    cx.set_global(store);
    theme_settings::init(theme::LoadThemes::JustBase, cx);
    release_channel::init(semver::Version::new(0, 0, 0), cx);
    editor::init(cx);
    command_palette::init(cx);
    search::init(cx);
    vim::init(cx);
}

fn bind_test_keymaps(cx: &mut App) {
    let default_key_bindings =
        settings::KeymapFile::load_asset_allow_partial_failure(settings::DEFAULT_KEYMAP_PATH, cx)
            .expect("load default keymap");
    cx.bind_keys(default_key_bindings);
    let vim_key_bindings =
        settings::KeymapFile::load_asset_allow_partial_failure(settings::VIM_KEYMAP_PATH, cx)
            .expect("load vim keymap");
    cx.bind_keys(vim_key_bindings);
}

fn test_workspace(cx: &mut TestAppContext) -> WindowHandle<Workspace> {
    cx.update(init_test_app);
    let target = AttachTarget {
        socket_path: std::env::temp_dir().join("rho-gui2-test-nonexistent.sock"),
        project_root: std::env::temp_dir(),
    };
    cx.add_window(|window, cx| Workspace::new(target, window, cx))
}

fn agent(id: u64) -> AgentId {
    AgentId::from_str(&format!("agent-{id}")).expect("valid agent id")
}

fn snapshot_frame(state: UiAgentState) -> AgentRemoteFrame {
    AgentRemoteFrame::Snapshot(state)
}

fn feed_frame(
    workspace: &WindowHandle<Workspace>,
    cx: &mut TestAppContext,
    agent_id: AgentId,
    frame: AgentRemoteFrame,
) {
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(ConnEvent::Frame { agent_id, frame }, window, cx);
        })
        .expect("update workspace");
}

fn active_editor(workspace: &WindowHandle<Workspace>, cx: &mut TestAppContext) -> Entity<Editor> {
    workspace
        .update(cx, |workspace, _, cx| {
            workspace.active_view().read(cx).editor().clone()
        })
        .expect("read workspace")
}

fn display_text(workspace: &WindowHandle<Workspace>, cx: &mut TestAppContext) -> String {
    let editor = active_editor(workspace, cx);
    workspace
        .update(cx, |_, _, cx| {
            editor.update(cx, |editor, cx| editor.display_text(cx))
        })
        .expect("read display text")
}

fn has_display_elision(workspace: &WindowHandle<Workspace>, cx: &mut TestAppContext) -> bool {
    let editor = active_editor(workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.snapshot(window, cx);
                snapshot
                    .blocks_in_range(DisplayRow(0)..snapshot.max_point().row() + 1)
                    .any(|(_, block)| matches!(block, Block::DisplayElision(_)))
            })
        })
        .expect("inspect blocks")
}

fn user(text: &str) -> UiBlock {
    UiBlock::UserMessage {
        text: text.to_owned(),
    }
}

fn assistant(text: &str, phase: Option<UiMessagePhase>) -> UiBlock {
    UiBlock::AssistantMessage {
        text: text.to_owned(),
        phase,
    }
}

fn tool(id: &str, status: UiToolStatus, started_at: Option<u64>, finished_at: Option<u64>) -> UiTool {
    UiTool {
        id: id.to_owned(),
        name: "shell_command".to_owned(),
        arguments: "echo ok".to_owned(),
        preview: None,
        status,
        output: None,
        error: None,
        started_at: started_at.map(UnixMs),
        finished_at: finished_at.map(UnixMs),
        metadata: None,
    }
}

fn state(blocks: Vec<UiBlock>, pending: Vec<UiStreamingItem>) -> UiAgentState {
    UiAgentState {
        blocks,
        status: UiAgentStatus::Streaming,
        pending_response: pending,
    }
}

fn long_working_text() -> String {
    "alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\ngolf\nhotel\nindia\njuliet\nkilo\nlima\nmike\nnovember\noscar\npapa\n"
        .to_owned()
}

#[gpui::test]
fn user_messages_render_with_turn_gaps_and_gutters(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![
                user("first"),
                assistant("answer", Some(UiMessagePhase::FinalAnswer)),
                user("second"),
            ],
            Vec::new(),
        )),
    );

    let text = display_text(&workspace, cx);
    assert!(
        text.contains("first\n\nanswer\n\nsecond\n\n"),
        "subsequent user messages should start a new turn with a blank line: {text:?}"
    );
    // Leading newlines are the banner block's display rows; the transcript
    // itself must start directly with the first user message.
    assert!(
        text.trim_start_matches('\n').starts_with("first"),
        "first user message should not get a leading gap: {text:?}"
    );

    let editor = active_editor(&workspace, cx);
    let gutter_highlights = workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| editor.all_gutter_highlights(window, cx))
        })
        .expect("read gutters");
    assert!(
        gutter_highlights.len() >= 2,
        "user messages should render gutter highlights: {gutter_highlights:?}"
    );
}

#[gpui::test]
fn streaming_text_appends_through_item_diffs(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("go")],
            vec![UiStreamingItem::AssistantMessage {
                text: "hel".to_owned(),
                phase: Some(UiMessagePhase::FinalAnswer),
            }],
        )),
    );
    assert!(display_text(&workspace, cx).contains("hel"));

    feed_frame(
        &workspace,
        cx,
        agent(1),
        AgentRemoteFrame::Diff {
            blocks: UiBlocksDiff {
                updates: Vec::new(),
                truncate_to: None,
                append: Vec::new(),
            },
            status: None,
            pending_response: UiPendingResponseDiff::Items(vec![UiStreamingItemUpdate {
                index: 0,
                item: UiStreamingItemDiff::AssistantMessage {
                    text: UiTextDiff {
                        keep_bytes: 3,
                        value: "lo world".to_owned(),
                    },
                },
            }]),
        },
    );
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("hello world"),
        "streamed suffix should append to the frontier: {text:?}"
    );
}

#[gpui::test]
fn pending_commentary_elides_but_final_answer_does_not(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("do work")],
            vec![UiStreamingItem::AssistantMessage {
                text: long_working_text(),
                phase: None,
            }],
        )),
    );
    assert!(has_display_elision(&workspace, cx));
    let text = display_text(&workspace, cx);
    assert!(text.contains("do work"), "user prompt should render: {text:?}");
    assert!(
        !text.contains("alpha"),
        "unknown-phase pending assistant should be elided: {text:?}"
    );
    assert!(
        text.contains("echo"),
        "limited elision should leave tail rows visible: {text:?}"
    );

    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("do work")],
            vec![UiStreamingItem::AssistantMessage {
                text: long_working_text(),
                phase: Some(UiMessagePhase::FinalAnswer),
            }],
        )),
    );
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("alpha") && text.contains("foxtrot"),
        "final answer should not be elided: {text:?}"
    );
}

#[gpui::test]
fn burst_of_pending_tools_elides_early_tools(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let pending = (0..16)
        .map(|ix| {
            UiStreamingItem::Tool(UiTool {
                id: format!("tool-{ix}"),
                name: format!("tool_{ix}"),
                arguments: format!("arg-{ix}"),
                preview: None,
                status: UiToolStatus::Running,
                output: None,
                error: None,
                started_at: None,
                finished_at: None,
                metadata: None,
            })
        })
        .collect();
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(vec![user("run tools")], pending)),
    );

    assert!(has_display_elision(&workspace, cx));
    let text = display_text(&workspace, cx);
    assert!(
        !text.contains("tool_0"),
        "burst of pending tools should elide earliest tools: {text:?}"
    );
    assert!(
        text.contains("tool_15"),
        "burst of pending tools should keep the tail visible: {text:?}"
    );
}

#[gpui::test]
fn finished_tool_renders_duration(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![
                user("go"),
                UiBlock::Tool(tool("t1", UiToolStatus::Success, Some(1_000), Some(3_500))),
            ],
            Vec::new(),
        )),
    );
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("$ echo ok ok 2s"),
        "finished tool should render its duration: {text:?}"
    );
}

#[gpui::test]
fn running_tool_duration_ticks_in_place(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let started = crate::workspace::now_ms();
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![
                user("go"),
                UiBlock::Tool(tool("t1", UiToolStatus::Running, Some(started), None)),
            ],
            Vec::new(),
        )),
    );
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("$ echo ok running"),
        "running tool should render without a duration initially: {text:?}"
    );

    workspace
        .update(cx, |workspace, _, cx| {
            let view = workspace.active_view();
            view.update(cx, |view, cx| {
                assert!(view.has_timers());
                view.tick_timers(started + 5_000, cx);
            });
        })
        .expect("tick timers");
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("$ echo ok running 5s"),
        "ticking should splice the duration in place: {text:?}"
    );

    workspace
        .update(cx, |workspace, _, cx| {
            let view = workspace.active_view();
            view.update(cx, |view, cx| view.tick_timers(started + 65_000, cx));
        })
        .expect("tick timers");
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("$ echo ok running 1m5s"),
        "ticking should replace the previous duration: {text:?}"
    );
}

#[gpui::test]
fn empty_prompt_shows_placeholder_and_gutter(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("Write a message…"),
        "empty prompt should show the placeholder: {text:?}"
    );

    let editor = active_editor(&workspace, cx);
    let gutter_highlights = workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| editor.all_gutter_highlights(window, cx))
        })
        .expect("read gutters");
    assert!(
        !gutter_highlights.is_empty(),
        "empty prompt should have a gutter highlight"
    );
}

#[gpui::test]
fn system_notices_survive_transcript_rerenders(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(vec![user("first")], Vec::new())),
    );
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                ConnEvent::ServerError("boom".to_owned()),
                window,
                cx,
            );
        })
        .expect("post notice");
    assert!(display_text(&workspace, cx).contains("[rho daemon error: boom]"));

    // A full snapshot re-render replaces the entire transcript projection;
    // the local notice must survive.
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("first"), assistant("answer", Some(UiMessagePhase::FinalAnswer))],
            Vec::new(),
        )),
    );
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("[rho daemon error: boom]"),
        "local notices should survive transcript re-renders: {text:?}"
    );
    assert!(text.contains("answer"));
}

#[gpui::test]
fn display_elision_opens_and_closes_with_fold_keys(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    cx.update(bind_test_keymaps);

    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("do work")],
            vec![UiStreamingItem::AssistantMessage {
                text: long_working_text(),
                phase: None,
            }],
        )),
    );
    let collapsed = display_text(&workspace, cx);
    assert!(
        !collapsed.contains("alpha"),
        "working text should start collapsed: {collapsed:?}"
    );

    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            let focus_handle = editor.read(cx).focus_handle(cx);
            window.focus(&focus_handle, cx);
            editor.update(cx, |editor, cx| {
                editor.move_to_beginning(&Default::default(), window, cx);
            });
        })
        .expect("focus editor");
    cx.simulate_keystrokes(*workspace, "escape");
    cx.simulate_keystrokes(*workspace, "j j z o");
    let expanded = display_text(&workspace, cx);
    assert!(
        expanded.contains("alpha"),
        "z o should expand the working elision: {expanded:?}"
    );

    cx.simulate_keystrokes(*workspace, "z c");
    let recollapsed = display_text(&workspace, cx);
    assert!(
        !recollapsed.contains("alpha"),
        "z c should collapse the working elision again: {recollapsed:?}"
    );
}
