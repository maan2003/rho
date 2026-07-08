//! End-to-end tests: synthetic protocol frames in, rendered editor state out.

use editor::Editor;
use editor::display_map::{Block, DisplayRow};
use gpui::{App, Entity, Focusable as _, TestAppContext, WindowHandle};
use rho_core::UnixMs;
use rho_ui_proto::AgentId;
use rho_ui_proto::remote::{
    AgentRemoteFrame, UiAgentState, UiAgentStatus, UiBlock, UiBlockDiff, UiBlockUpdate,
    UiBlocksDiff, UiMessagePhase, UiTextDiff, UiTool, UiToolDiff, UiToolStatus,
};
use settings::SettingsStore;

use crate::connection::ConnEvent;
use crate::workspace::{AttachTarget, Workspace};

fn init_test_app(cx: &mut App) {
    gpui_tokio::init(cx);
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
    crate::bind_rho_key_overrides(cx);
}

fn test_workspace(cx: &mut TestAppContext) -> WindowHandle<Workspace> {
    cx.update(init_test_app);
    let target = AttachTarget {
        socket_path: std::env::temp_dir().join("rho-gui-test-nonexistent.sock"),
    };
    cx.add_window(|window, cx| Workspace::new(target, window, cx))
}

fn agent(id: u64) -> AgentId {
    AgentId::from_counter(id, &rho_ui_proto::AgentIdDomain(0)).unwrap()
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
        .update(cx, |workspace, _, cx| workspace.active_editor(cx))
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

fn tool(
    id: &str,
    status: UiToolStatus,
    started_at: Option<u64>,
    finished_at: Option<u64>,
) -> UiTool {
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

fn state(history: Vec<UiBlock>, live: Vec<UiBlock>) -> UiAgentState {
    let mut blocks = history;
    blocks.extend(live);
    UiAgentState {
        blocks,
        status: UiAgentStatus::Streaming,
        context_used: None,
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
            vec![assistant("hel", Some(UiMessagePhase::FinalAnswer))],
        )),
    );
    assert!(display_text(&workspace, cx).contains("hel"));

    feed_frame(
        &workspace,
        cx,
        agent(1),
        AgentRemoteFrame::Diff {
            blocks: UiBlocksDiff {
                truncate_to: None,
                updates: vec![UiBlockUpdate {
                    index: 1,
                    block: UiBlockDiff::AssistantText(UiTextDiff {
                        keep_bytes: 3,
                        value: "lo world".to_owned(),
                    }),
                }],
            },
            status: None,
            context_used: None,
        },
    );
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("hello world"),
        "streamed suffix should append to the frontier: {text:?}"
    );
}

#[gpui::test]
fn streaming_update_keeps_prompt_cursor_editable(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("go")],
            vec![assistant("hel", Some(UiMessagePhase::FinalAnswer))],
        )),
    );

    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| editor.insert("draft", window, cx));
        })
        .expect("type prompt");

    feed_frame(
        &workspace,
        cx,
        agent(1),
        AgentRemoteFrame::Diff {
            blocks: UiBlocksDiff {
                truncate_to: None,
                updates: vec![UiBlockUpdate {
                    index: 1,
                    block: UiBlockDiff::AssistantText(UiTextDiff {
                        keep_bytes: 3,
                        value: "lo".to_owned(),
                    }),
                }],
            },
            status: None,
            context_used: None,
        },
    );

    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| editor.insert("!", window, cx));
        })
        .expect("continue typing prompt");

    let text = display_text(&workspace, cx);
    assert!(
        text.contains("hello"),
        "streaming text should update: {text:?}"
    );
    assert!(
        text.contains("draft!"),
        "prompt cursor should remain in the prompt after streaming update: {text:?}"
    );
}

#[gpui::test]
fn streaming_tool_arguments_update_rendered_label(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("run")],
            vec![UiBlock::Tool(UiTool {
                id: "tool-1".to_owned(),
                name: "shell_command".to_owned(),
                arguments: "echo".to_owned(),
                preview: None,
                status: UiToolStatus::Running,
                output: None,
                error: None,
                started_at: None,
                finished_at: None,
                metadata: None,
            })],
        )),
    );

    feed_frame(
        &workspace,
        cx,
        agent(1),
        AgentRemoteFrame::Diff {
            blocks: UiBlocksDiff {
                truncate_to: None,
                updates: vec![UiBlockUpdate {
                    index: 1,
                    block: UiBlockDiff::Tool(UiToolDiff {
                        id: "tool-1".to_owned(),
                        name: "shell_command".to_owned(),
                        arguments: Some(UiTextDiff {
                            keep_bytes: 4,
                            value: " ok".to_owned(),
                        }),
                        preview: None,
                        status: None,
                        output: None,
                        error: None,
                        started_at: None,
                        finished_at: None,
                        metadata: None,
                    }),
                }],
            },
            status: None,
            context_used: None,
        },
    );

    let text = display_text(&workspace, cx);
    assert!(
        text.contains("$ echo ok"),
        "streamed tool arguments should update the rendered label: {text:?}"
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
            vec![assistant(
                &long_working_text(),
                Some(UiMessagePhase::Commentary),
            )],
        )),
    );
    assert!(has_display_elision(&workspace, cx));
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("do work"),
        "user prompt should render: {text:?}"
    );
    assert!(
        !text.contains("alpha"),
        "explicit commentary assistant should be elided: {text:?}"
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
            vec![assistant(
                &long_working_text(),
                Some(UiMessagePhase::FinalAnswer),
            )],
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
            UiBlock::Tool(UiTool {
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
        text.contains("$ echo ok …"),
        "running tool should render without a duration initially: {text:?}"
    );

    workspace
        .update(cx, |workspace, _, cx| {
            let view = workspace.active_agent_view().expect("agent view");
            view.update(cx, |view, cx| {
                assert!(view.has_timers());
                view.tick_timers(started + 5_000, cx);
            });
        })
        .expect("tick timers");
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("$ echo ok … 5s"),
        "ticking should splice the duration in place: {text:?}"
    );

    workspace
        .update(cx, |workspace, _, cx| {
            let view = workspace.active_agent_view().expect("agent view");
            view.update(cx, |view, cx| view.tick_timers(started + 65_000, cx));
        })
        .expect("tick timers");
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("$ echo ok … 1m5s"),
        "ticking should replace the previous duration: {text:?}"
    );
}

#[gpui::test]
fn hidden_views_defer_rendering_until_selected(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(vec![user("one")], Vec::new())),
    );
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.select_agent(Some(agent(2)), window, cx);
        })
        .expect("select agent 2");
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.select_agent(Some(agent(1)), window, cx);
        })
        .expect("select agent 1");

    // Agent 2 is materialized but hidden; its frames must not render yet.
    feed_frame(
        &workspace,
        cx,
        agent(2),
        snapshot_frame(state(
            vec![
                user("two"),
                assistant("done", Some(UiMessagePhase::FinalAnswer)),
            ],
            Vec::new(),
        )),
    );
    let hidden_view = workspace
        .update(cx, |workspace, _, _| {
            workspace
                .agent_view(&agent(2))
                .expect("agent 2 view exists")
        })
        .expect("read workspace");
    let hidden_text = workspace
        .update(cx, |_, _, cx| {
            hidden_view.update(cx, |view, cx| {
                view.editor()
                    .update(cx, |editor, cx| editor.display_text(cx))
            })
        })
        .expect("read hidden view");
    assert!(
        !hidden_text.contains("done"),
        "hidden views should not render frames eagerly: {hidden_text:?}"
    );

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.select_agent(Some(agent(2)), window, cx);
        })
        .expect("select agent 2");
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("two") && text.contains("done"),
        "selecting a hidden agent should flush its deferred frames: {text:?}"
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
fn previous_agent_frames_do_not_leave_intentional_draft(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(vec![user("previous agent")], Vec::new())),
    );
    assert!(display_text(&workspace, cx).contains("previous agent"));

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.enter_draft(None, window, cx);
        })
        .expect("enter draft");
    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| editor.insert("new draft", window, cx));
        })
        .expect("type draft");

    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![
                user("previous agent"),
                assistant("background update", Some(UiMessagePhase::FinalAnswer)),
            ],
            Vec::new(),
        )),
    );
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("new draft"),
        "incoming frames should keep the intentional draft focused: {text:?}"
    );
    assert!(
        !text.contains("background update"),
        "previous-agent updates should not become the active editor: {text:?}"
    );
}

#[gpui::test]
fn editing_startup_draft_prevents_first_frame_auto_selection(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| editor.insert("startup draft", window, cx));
        })
        .expect("type startup draft");

    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(vec![user("background agent")], Vec::new())),
    );

    let text = display_text(&workspace, cx);
    assert!(
        text.contains("startup draft"),
        "editing startup draft should make it intentional: {text:?}"
    );
    assert!(
        !text.contains("background agent"),
        "first background frame should not steal an edited startup draft: {text:?}"
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
            workspace.handle_event(ConnEvent::ServerError("boom".to_owned()), window, cx);
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
            vec![
                user("first"),
                assistant("answer", Some(UiMessagePhase::FinalAnswer)),
            ],
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
fn turn_cancelled_ack_is_not_persisted_as_notice(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(vec![user("first")], Vec::new())),
    );
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(ConnEvent::TurnCancelled, window, cx);
        })
        .expect("handle cancellation acknowledgement");

    let text = display_text(&workspace, cx);
    assert!(
        !text.contains("[turn cancelled]"),
        "turn cancellation acknowledgement should not become persistent transcript text: {text:?}"
    );
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
            vec![assistant(
                &long_working_text(),
                Some(UiMessagePhase::Commentary),
            )],
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

#[gpui::test]
fn submit_prompt_bubbles_from_the_editor_to_the_workspace(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| editor.insert("hello rho", window, cx));
        })
        .expect("type into prompt");

    cx.dispatch_action(*workspace, crate::SubmitPrompt);

    // Not connected, so the submission surfaces as a system notice — proving
    // the action reached the workspace handler.
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("not connected to rho-daemon"),
        "submit should reach the workspace and report the failed send: {text:?}"
    );
    // Draft submissions keep the buffer until the daemon confirms creation,
    // so a failed send never loses the message.
    assert!(
        text.contains("hello rho"),
        "a failed draft submit should keep the message: {text:?}"
    );
}

/// Restore flow: the agent's first frame is a snapshot that already carries
/// `context_used` (daemon loaded it from the event log / transcript). The
/// status chips must show it without any live turn happening.
#[gpui::test]
fn restored_context_usage_shows_in_status_chips(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        AgentRemoteFrame::Snapshot(UiAgentState {
            blocks: vec![
                user("go"),
                assistant("done", Some(UiMessagePhase::FinalAnswer)),
            ],
            status: UiAgentStatus::Idle,
            context_used: Some(194_816),
        }),
    );
    let spans = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .active_agent_view()
                .expect("agent view")
                .read(cx)
                .status_span_text()
        })
        .expect("read spans");
    assert!(
        spans.contains("195k"),
        "restored context chip missing from status spans: {spans:?}"
    );
}

/// The view can exist before any frame arrives (agent selected first, load
/// completes later): the chip must appear when the snapshot lands.
#[gpui::test]
fn context_chip_appears_when_frame_arrives_after_selection(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.select_agent(Some(agent(1)), window, cx);
        })
        .expect("select agent");
    let spans_before = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .active_agent_view()
                .expect("agent view")
                .read(cx)
                .status_span_text()
        })
        .expect("read spans");
    assert!(
        !spans_before.contains('k'),
        "no chip expected before any frame: {spans_before:?}"
    );

    feed_frame(
        &workspace,
        cx,
        agent(1),
        AgentRemoteFrame::Snapshot(UiAgentState {
            blocks: vec![user("go")],
            status: UiAgentStatus::Idle,
            context_used: Some(62_300),
        }),
    );
    let spans = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .active_agent_view()
                .expect("agent view")
                .read(cx)
                .status_span_text()
        })
        .expect("read spans");
    assert!(
        spans.contains("62k"),
        "context chip missing after late frame: {spans:?}"
    );
}
