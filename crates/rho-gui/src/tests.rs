//! End-to-end tests: synthetic protocol frames in, rendered editor state out.

use editor::Editor;
use editor::display_map::{Block, DisplayRow};
use gpui::{App, AppContext as _, Entity, Focusable as _, TestAppContext, WindowHandle};
use rho_core::UnixMs;
use rho_ui_proto::AgentId;
use rho_ui_proto::remote::{
    AgentRemoteFrame, UiAgentState, UiAgentStatus, UiBlock, UiBlockDiff, UiBlockUpdate,
    UiBlocksDiff, UiMessagePhase, UiTextDiff, UiTool, UiToolDiff, UiToolStatus,
};
use settings::SettingsStore;

use crate::connection::{ConnEvent, HostEvent};
use crate::registry::HostId;
use crate::workspace::{AttachTarget, HostSpec, Workspace};

#[test]
fn frame_distribution_reports_nearest_rank_percentiles() {
    let distribution = crate::distribution([1, 2, 3, 4, 100], 1.0);
    assert_eq!(distribution.count, 5);
    assert_eq!(distribution.mean, 22.0);
    assert_eq!(distribution.p50, 3.0);
    assert_eq!(distribution.p95, 100.0);
    assert_eq!(distribution.p99, 100.0);
    assert_eq!(distribution.max, 100.0);
}

fn init_test_app(cx: &mut App) {
    gpui_tokio::init(cx);
    assets::Assets.load_test_fonts(cx);
    // The vendored defaults, same as production — this also guards the
    // vendored file against edits that would fail to parse at startup.
    let store = SettingsStore::new(cx, crate::rho_assets::RHO_DEFAULT_SETTINGS);
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
    let target = AttachTarget::Unix(std::env::temp_dir().join("rho-gui-test-nonexistent.sock"));
    let specs = vec![HostSpec {
        name: "local".to_owned(),
        target,
    }];
    cx.add_window(|window, cx| Workspace::new(specs.clone(), window, cx))
}

#[gpui::test]
fn modal_overlays_preserve_dashboard_and_surface_modes(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);

    workspace
        .update(cx, |workspace, window, cx| {
            assert!(workspace.is_dashboard_mode(window, cx));
            workspace.open_transient(crate::transient::root_menu(), window, cx);
        })
        .expect("open dashboard transient");
    cx.simulate_keystrokes(*workspace, "p r");
    workspace
        .update(cx, |workspace, window, cx| {
            assert!(
                workspace.is_dashboard_mode(window, cx),
                "transient-to-minibuffer handoff should remain in dashboard mode"
            );
        })
        .expect("inspect dashboard prompt");
    cx.dispatch_action(*workspace, crate::MinibufferCancel);
    workspace
        .update(cx, |workspace, window, cx| {
            assert!(workspace.is_dashboard_mode(window, cx));
            workspace.prompt_open_file(window, cx);
        })
        .expect("open dashboard prompt");
    cx.dispatch_action(*workspace, crate::MinibufferConfirm);
    workspace
        .update(cx, |workspace, window, cx| {
            assert!(workspace.is_dashboard_mode(window, cx));
            let (response, _decision) = tokio::sync::oneshot::channel();
            workspace.handle_event(
                HostId::default(),
                ConnEvent::GitTransportApproval {
                    request_id: 1,
                    prompt: "approve dashboard Git operation".to_owned(),
                    response,
                },
                window,
                cx,
            );
            assert!(workspace.is_dashboard_mode(window, cx));
        })
        .expect("open dashboard Git approval");
    cx.dispatch_action(*workspace, crate::GitApprovalDeny);
    workspace
        .update(cx, |workspace, window, cx| {
            assert!(workspace.is_dashboard_mode(window, cx));
            workspace.select_agent(None, window, cx);
            assert!(!workspace.is_dashboard_mode(window, cx));
            workspace.prompt_open_file(window, cx);
            assert!(!workspace.is_dashboard_mode(window, cx));
        })
        .expect("open surface prompt");
    cx.dispatch_action(*workspace, crate::MinibufferCancel);
    workspace
        .update(cx, |workspace, window, cx| {
            assert!(!workspace.is_dashboard_mode(window, cx));
            let (response, _decision) = tokio::sync::oneshot::channel();
            workspace.handle_event(
                HostId::default(),
                ConnEvent::GitTransportApproval {
                    request_id: 2,
                    prompt: "approve surface Git operation".to_owned(),
                    response,
                },
                window,
                cx,
            );
            assert!(!workspace.is_dashboard_mode(window, cx));
            workspace.handle_event(
                HostId::default(),
                ConnEvent::GitTransportDone { request_id: 2 },
                window,
                cx,
            );
            assert!(!workspace.is_dashboard_mode(window, cx));
        })
        .expect("inspect restored surface mode");
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
            // Transcript rendering tests use a selected agent explicitly;
            // production startup no longer derives selection from a frame.
            if workspace.is_startup_pane() {
                workspace.select_agent(Some(agent_id), window, cx);
            }
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Frame {
                    agent_id,
                    frame,
                    allocation: None,
                },
                window,
                cx,
            );
        })
        .expect("update workspace");
    cx.run_until_parked();
}

fn feed_frames(
    workspace: &WindowHandle<Workspace>,
    cx: &mut TestAppContext,
    frames: impl IntoIterator<Item = (AgentId, AgentRemoteFrame)>,
) {
    let events: Vec<_> = frames
        .into_iter()
        .map(|(agent_id, frame)| HostEvent {
            host: HostId::default(),
            event: ConnEvent::Frame {
                agent_id,
                frame,
                allocation: None,
            },
        })
        .collect();
    workspace
        .update(cx, |workspace, window, cx| {
            if workspace.is_startup_pane()
                && let Some(agent_id) = events.iter().find_map(|event| match &event.event {
                    ConnEvent::Frame { agent_id, .. } => Some(*agent_id),
                    _ => None,
                })
            {
                workspace.select_agent(Some(agent_id), window, cx);
            }
            workspace.handle_events(events, window, cx);
        })
        .expect("update workspace");
    cx.update_window((*workspace).into(), |_, window, cx| {
        window.simulate_next_frame(cx);
    })
    .expect("flush queued frames");
    cx.run_until_parked();
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

fn concealed_fold_ids(
    workspace: &WindowHandle<Workspace>,
    editor: &Entity<Editor>,
    cx: &mut TestAppContext,
) -> Vec<editor::display_map::FoldId> {
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.snapshot(window, cx);
                snapshot
                    .folds_in_range(
                        multi_buffer::MultiBufferOffset(0)..snapshot.buffer_snapshot().len(),
                    )
                    .filter(|fold| fold.placeholder.is_concealed())
                    .map(|fold| fold.id)
                    .collect()
            })
        })
        .expect("read concealment folds")
}

fn buffer_text(workspace: &WindowHandle<Workspace>, cx: &mut TestAppContext) -> String {
    let editor = active_editor(workspace, cx);
    workspace
        .update(cx, |_, _, cx| {
            editor.update(cx, |editor, cx| editor.text(cx))
        })
        .expect("read buffer text")
}

/// The visible text with the highlight colour applied to it, one entry per
/// run of identical styling.
fn styled_runs(
    workspace: &WindowHandle<Workspace>,
    cx: &mut TestAppContext,
) -> Vec<(String, Option<gpui::Hsla>)> {
    let editor = active_editor(workspace, cx);
    workspace
        .update(cx, |_, _, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.display_map.update(cx, |map, cx| map.snapshot(cx));
                let rows = DisplayRow(0)..DisplayRow(snapshot.max_point().row().0 + 1);
                let mut runs: Vec<(String, Option<gpui::Hsla>)> = Vec::new();
                for chunk in snapshot.chunks(
                    rows,
                    language::LanguageAwareStyling {
                        tree_sitter: false,
                        diagnostics: false,
                    },
                    editor::display_map::HighlightStyles::default(),
                ) {
                    let color = chunk.highlight_style.and_then(|style| style.color);
                    match runs.last_mut() {
                        Some((text, last)) if *last == color => text.push_str(chunk.text),
                        _ => runs.push((chunk.text.to_owned(), color)),
                    }
                }
                runs
            })
        })
        .expect("read styled runs")
}

fn syntax_highlights_for_text(
    workspace: &WindowHandle<Workspace>,
    needle: &str,
    cx: &mut TestAppContext,
) -> Vec<Option<language::HighlightId>> {
    let editor = active_editor(workspace, cx);
    workspace
        .update(cx, |_, _, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let text = snapshot.text();
                let start = text
                    .find(needle)
                    .unwrap_or_else(|| panic!("{needle:?} in buffer text {text:?}"));
                snapshot
                    .chunks(
                        multi_buffer::MultiBufferOffset(start)
                            ..multi_buffer::MultiBufferOffset(start + needle.len()),
                        language::LanguageAwareStyling {
                            tree_sitter: true,
                            diagnostics: false,
                        },
                    )
                    .map(|chunk| chunk.syntax_highlight_id)
                    .collect()
            })
        })
        .expect("read buffer syntax highlights")
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

fn has_custom_block(workspace: &WindowHandle<Workspace>, cx: &mut TestAppContext) -> bool {
    let editor = active_editor(workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.snapshot(window, cx);
                snapshot
                    .blocks_in_range(DisplayRow(0)..snapshot.max_point().row() + 1)
                    .any(|(_, block)| matches!(block, Block::Custom(_)))
            })
        })
        .expect("inspect custom blocks")
}

fn excerpt_boundary_count(workspace: &WindowHandle<Workspace>, cx: &mut TestAppContext) -> usize {
    let editor = active_editor(workspace, cx);
    editor_excerpt_boundary_count(workspace, &editor, cx)
}

fn editor_excerpt_boundary_count(
    workspace: &WindowHandle<Workspace>,
    editor: &Entity<Editor>,
    cx: &mut TestAppContext,
) -> usize {
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.snapshot(window, cx);
                snapshot
                    .blocks_in_range(DisplayRow(0)..snapshot.max_point().row() + 1)
                    .filter(|(_, block)| {
                        matches!(
                            block,
                            Block::ExcerptBoundary { .. } | Block::BufferHeader { .. }
                        )
                    })
                    .count()
            })
        })
        .expect("inspect excerpt boundaries")
}

fn user(text: &str) -> UiBlock {
    UiBlock::UserMessage {
        text: text.to_owned(),
    }
}

fn agent_message(sender: AgentId, text: &str) -> UiBlock {
    UiBlock::AgentMessage {
        sender,
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
        usage: Default::default(),
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
        "user messages should retain their vertical gutter lines: {gutter_highlights:?}"
    );
    assert_eq!(
        excerpt_boundary_count(&workspace, cx),
        0,
        "turn buffers should not render horizontal excerpt boundaries"
    );
}

#[gpui::test]
fn last_response_has_a_blank_line_before_the_prompt(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("question")],
            vec![assistant("answer", None)],
        )),
    );

    let text = display_text(&workspace, cx);
    assert!(
        text.contains("answer\n\nWrite a message…"),
        "the prompt should have a blank row after the last response: {text:?}"
    );

    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(vec![user("last user")], Vec::new())),
    );
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("last user\n\nWrite a message…")
            && !text.contains("last user\n\n\nWrite a message…"),
        "a user message should keep exactly one blank row before the prompt: {text:?}"
    );
}

#[gpui::test]
fn agent_messages_use_their_text_color_in_the_gutter(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("local"), agent_message(agent(2), "remote")],
            Vec::new(),
        )),
    );

    let editor = active_editor(&workspace, cx);
    let gutter_highlights = workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| editor.all_gutter_highlights(window, cx))
        })
        .expect("read gutters");
    assert!(gutter_highlights.len() >= 2);
    assert!(
        gutter_highlights
            .iter()
            .any(|(_, color)| *color != gutter_highlights[0].1)
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
            usage: None,
        },
    );
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("hello world"),
        "streamed suffix should append to the frontier: {text:?}"
    );
}

/// Times a transcript being attached and then streamed into, and prints
/// where the time went. Not a check, so it stays out of the suite:
///
/// ```text
/// PERF_BLOCKS=400 cargo test --release -p rho-gui --bin rho-gui \
///     bench_markdown_transcript -- --ignored --nocapture
/// ```
#[gpui::test]
#[ignore = "benchmark"]
fn bench_markdown_transcript(cx: &mut TestAppContext) {
    let blocks_count: usize = std::env::var("PERF_BLOCKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(400);
    let paragraph = "The **fast path** in `crates/fastc/src/lib.rs` aggregates \
`callback_stats` before **cancellation**, so *counts* stay deterministic and \
`Instant::now()` never runs when tracing is off.\n";
    let body = paragraph.repeat(4);

    let workspace = test_workspace(cx);
    let mut blocks = Vec::new();
    for index in 0..blocks_count {
        blocks.push(user(&format!("request {index}")));
        blocks.push(assistant(&body, Some(UiMessagePhase::FinalAnswer)));
    }
    crate::sampler::start(2000);
    let start = std::time::Instant::now();
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(blocks, Vec::new())),
    );
    let initial = start.elapsed();
    let attach_samples = crate::sampler::stop();

    // Stream a message into the tail of that transcript, one delta at a time.
    let mut text = String::new();
    let mut deltas = Vec::new();
    for word in body.split_inclusive(' ') {
        let keep_bytes = text.len();
        text.push_str(word);
        deltas.push((keep_bytes, word.to_owned()));
    }
    let index = blocks_count * 2 - 1;
    let mut worst = std::time::Duration::ZERO;
    crate::sampler::start(2000);
    let start = std::time::Instant::now();
    for (keep_bytes, value) in &deltas {
        let delta = std::time::Instant::now();
        feed_frame(
            &workspace,
            cx,
            agent(1),
            AgentRemoteFrame::Diff {
                blocks: UiBlocksDiff {
                    truncate_to: None,
                    updates: vec![UiBlockUpdate {
                        index,
                        block: UiBlockDiff::AssistantText(UiTextDiff {
                            keep_bytes: *keep_bytes,
                            value: value.clone(),
                        }),
                    }],
                },
                status: None,
                context_used: None,
                usage: None,
            },
        );
        worst = worst.max(delta.elapsed());
    }
    let streaming = start.elapsed();
    let stream_samples = crate::sampler::stop();
    let count = deltas.len() as u32;
    println!(
        "blocks={blocks_count} initial={initial:?} deltas={count} mean={:?} worst={worst:?}",
        streaming / count
    );
    crate::sampler::report(&attach_samples, "attach");
    crate::sampler::report(&stream_samples, "streaming");
}

/// Times the flows a session actually spends its day in - switching
/// agents, typing, tool traffic, the dashboard - and prints where each
/// one goes. Not a check, so it stays out of the suite:
///
/// ```text
/// PERF_BLOCKS=200 cargo test --release -p rho-gui --bin rho-gui \\
///     bench_rho_gui_flows -- --ignored --nocapture
/// ```
#[gpui::test]
#[ignore = "benchmark"]
fn bench_rho_gui_flows(cx: &mut TestAppContext) {
    let blocks_count: usize = std::env::var("PERF_BLOCKS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let paragraph = "The **fast path** in `crates/fastc/src/lib.rs` aggregates \
`callback_stats` before **cancellation**, so *counts* stay deterministic and \
`Instant::now()` never runs when tracing is off.\n";
    let transcript = |seed: usize| {
        let mut blocks = Vec::new();
        for index in 0..blocks_count {
            // Every message is its own text, as a real transcript's are.
            let body = format!("Answer {seed}.{index}:\n{}", paragraph.repeat(4));
            blocks.push(user(&format!("request {seed}.{index}")));
            blocks.push(assistant(&body, Some(UiMessagePhase::FinalAnswer)));
            blocks.push(UiBlock::Tool(tool(
                &format!("t{seed}.{index}"),
                UiToolStatus::Success,
                Some(1_000),
                Some(1_200),
            )));
            blocks.push(UiBlock::Notice {
                text: format!("notice {index}"),
            });
        }
        blocks
    };

    let workspace = test_workspace(cx);
    let phase = |label: &str, elapsed: std::time::Duration, count: u32| {
        println!(
            "{label}: total={elapsed:?} each={:?}",
            elapsed / count.max(1)
        );
    };

    // Attaching to an agent for the first time.
    let start = std::time::Instant::now();
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(transcript(1), Vec::new())),
    );
    phase("attach", start.elapsed(), 1);

    let start = std::time::Instant::now();
    feed_frame(
        &workspace,
        cx,
        agent(2),
        snapshot_frame(state(transcript(2), Vec::new())),
    );
    phase("second agent frame", start.elapsed(), 1);
    // The user takes a moment before switching; the parse ahead of that view
    // runs in it.
    let start = std::time::Instant::now();
    cx.run_until_parked();
    phase("parse ahead settles", start.elapsed(), 1);

    // Switching between two agents that both carry a transcript.
    crate::sampler::start(2000);
    let start = std::time::Instant::now();
    for index in 0..10 {
        let id = agent(if index % 2 == 0 { 2 } else { 1 });
        let one = std::time::Instant::now();
        workspace
            .update(cx, |workspace, window, cx| {
                workspace.select_agent(Some(id), window, cx);
            })
            .expect("select agent");
        println!("  switch {index}: {:?}", one.elapsed());
    }
    phase("agent switch", start.elapsed(), 10);
    let switch_samples = crate::sampler::stop();

    // Typing into the prompt with that transcript on screen.
    let editor = active_editor(&workspace, cx);
    crate::sampler::start(2000);
    let start = std::time::Instant::now();
    for character in "the quick brown fox jumps over the lazy dog".chars() {
        workspace
            .update(cx, |_, window, cx| {
                editor.update(cx, |editor, cx| {
                    editor.insert(&character.to_string(), window, cx)
                });
            })
            .expect("type prompt");
    }
    phase("prompt keystroke", start.elapsed(), 43);
    let typing_samples = crate::sampler::stop();

    // Tool traffic: one running tool ticking its status.
    let index = blocks_count * 4 - 2;
    let start = std::time::Instant::now();
    for tick in 0..50u64 {
        feed_frame(
            &workspace,
            cx,
            agent(1),
            AgentRemoteFrame::Diff {
                blocks: UiBlocksDiff {
                    truncate_to: None,
                    updates: vec![UiBlockUpdate {
                        index,
                        block: UiBlockDiff::Tool(UiToolDiff {
                            id: format!("t1.{}", blocks_count - 1),
                            name: "shell_command".to_owned(),
                            arguments: Some(UiTextDiff {
                                keep_bytes: 0,
                                value: format!("echo {tick}"),
                            }),
                            preview: None,
                            status: Some(UiToolStatus::Running),
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
                usage: None,
            },
        );
    }
    phase("tool update", start.elapsed(), 50);

    crate::sampler::report(&switch_samples, "agent switch");
    crate::sampler::report(&typing_samples, "prompt keystroke");
}

#[gpui::test]
fn highlights_survive_the_folds_that_conceal_markup(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![assistant(
                "**bold** and `code` and plain\n",
                Some(UiMessagePhase::FinalAnswer),
            )],
            Vec::new(),
        )),
    );

    // Highlight text that spans and follows concealed markup. The chunk
    // iterator seeks past every concealed run, and each seek has to keep
    // the highlights it is in the middle of.
    let red = gpui::rgb(0xff0000);
    let blue = gpui::rgb(0x0000ff);
    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, _, cx| {
            let buffer = editor.read(cx).buffer().clone();
            let snapshot = buffer.read(cx).snapshot(cx);
            let text = snapshot.text();
            let anchors = |needle: &str| {
                let start = text.find(needle).expect("highlighted text in buffer");
                vec![
                    snapshot.anchor_after(multi_buffer::MultiBufferOffset(start))
                        ..snapshot
                            .anchor_before(multi_buffer::MultiBufferOffset(start + needle.len())),
                ]
            };
            // The first range spans four concealed runs, so it has to stay
            // active across every seek the fold map makes inside it.
            let bold = anchors("**bold** and `code`");
            let plain = anchors("plain");
            editor.update(cx, |editor, cx| {
                editor.highlight_text(
                    editor::display_map::HighlightKey::DocumentHighlightRead,
                    bold,
                    gpui::HighlightStyle::color(red.into()),
                    cx,
                );
                editor.highlight_text(
                    editor::display_map::HighlightKey::DocumentHighlightWrite,
                    plain,
                    gpui::HighlightStyle::color(blue.into()),
                    cx,
                );
            });
        })
        .expect("highlight words around concealed markup");
    cx.run_until_parked();

    let runs = styled_runs(&workspace, cx);
    let text: String = runs.iter().map(|(text, _)| text.as_str()).collect();
    assert!(
        text.starts_with("bold and code and plain\n"),
        "concealed markup should stay hidden: {text:?}"
    );
    let styled: Vec<_> = runs
        .iter()
        .filter(|(_, color)| color.is_some())
        .map(|(text, color)| (text.as_str(), *color))
        .collect();
    assert_eq!(
        styled,
        vec![
            ("bold and code", Some(red.into())),
            ("plain", Some(blue.into())),
        ],
        "highlights should cover their own words and nothing else: {runs:?}"
    );
}

#[gpui::test]
fn markdown_markup_is_hidden_on_screen_but_kept_in_the_buffer(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("**user markup stays visible**")],
            vec![assistant(
                "## Heading\n\n**bold** and `code`.\n",
                Some(UiMessagePhase::FinalAnswer),
            )],
        )),
    );
    cx.run_until_parked();

    let text = display_text(&workspace, cx);
    assert!(
        text.contains("Heading\n\nbold and code.\n"),
        "markup should not reach the screen: {text:?}"
    );
    assert!(text.contains("**user markup stays visible**"));
    let buffer = buffer_text(&workspace, cx);
    assert!(
        buffer.contains("## Heading\n\n**bold** and `code`.\n"),
        "the buffer keeps the markdown source for copy and search: {buffer:?}"
    );

    // Streaming past a concealed range refolds it in place.
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
                        keep_bytes: "## Heading\n\n**bold** and `code`.\n".len(),
                        value: "*more*\n".to_owned(),
                    }),
                }],
            },
            status: None,
            context_used: None,
            usage: None,
        },
    );
    cx.run_until_parked();
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("bold and code.\nmore\n"),
        "streamed markup should conceal too: {text:?}"
    );

    // Concealed markup is decoration, not something the reader folded: an
    // unfold leaves it hidden.
    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.unfold_all(&editor::actions::UnfoldAll, window, cx);
            });
        })
        .expect("unfold all");
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("bold and code.\nmore\n"),
        "unfolding should not reveal markup: {text:?}"
    );
}

#[gpui::test]
fn markdown_tables_align_with_virtual_tabs_but_keep_their_source(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let table = "| Name | Outcome |\n| --- | --- |\n| one | passed |\n";
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("show a table")],
            vec![assistant(table, Some(UiMessagePhase::FinalAnswer))],
        )),
    );
    cx.run_until_parked();

    let buffer = buffer_text(&workspace, cx);
    assert!(
        buffer.contains(table),
        "source should remain unchanged: {buffer:?}"
    );
    assert_table_pipes_align(&workspace, 3, cx);

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
                        keep_bytes: table.len(),
                        value: "| longest name | failed |\n".to_owned(),
                    }),
                }],
            },
            status: None,
            context_used: None,
            usage: None,
        },
    );
    cx.run_until_parked();
    assert_table_pipes_align(&workspace, 4, cx);
}

fn assert_table_pipes_align(
    workspace: &WindowHandle<Workspace>,
    expected_rows: usize,
    cx: &mut TestAppContext,
) {
    let rows = display_text(workspace, cx)
        .lines()
        .filter(|line| line.starts_with('|'))
        .map(|line| {
            line.chars()
                .enumerate()
                .filter_map(|(column, character)| (character == '|').then_some(column))
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rows.len(),
        expected_rows,
        "the whole table should remain visible: {rows:?}"
    );
    assert!(
        rows.windows(2).all(|rows| rows[0] == rows[1]),
        "virtual tabs should align every source pipe: {rows:?}"
    );
}

#[gpui::test]
fn visualization_refs_become_inline_editor_blocks(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let tag = "```visualization\nref=0123456789abcdef0123456789abcdef rows=12\n```";
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("show it")],
            vec![assistant(tag, Some(UiMessagePhase::FinalAnswer))],
        )),
    );

    assert!(buffer_text(&workspace, cx).contains(tag));
    assert!(!display_text(&workspace, cx).contains(tag));
    assert!(has_custom_block(&workspace, cx));

    feed_frame(
        &workspace,
        cx,
        agent(1),
        AgentRemoteFrame::Diff {
            blocks: UiBlocksDiff {
                truncate_to: None,
                updates: vec![UiBlockUpdate {
                    index: 1,
                    block: UiBlockDiff::Replace(assistant(
                        "ordinary text",
                        Some(UiMessagePhase::FinalAnswer),
                    )),
                }],
            },
            status: None,
            context_used: None,
            usage: None,
        },
    );
    assert!(!has_custom_block(&workspace, cx));
}

#[gpui::test]
fn queued_streaming_updates_to_one_block_render_once_to_final_state(cx: &mut TestAppContext) {
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

    let update = |keep_bytes, value: &str| AgentRemoteFrame::Diff {
        blocks: UiBlocksDiff {
            truncate_to: None,
            updates: vec![UiBlockUpdate {
                index: 1,
                block: UiBlockDiff::AssistantText(UiTextDiff {
                    keep_bytes,
                    value: value.to_owned(),
                }),
            }],
        },
        status: None,
        context_used: None,
        usage: None,
    };
    feed_frames(
        &workspace,
        cx,
        [(agent(1), update(3, "lo")), (agent(1), update(5, " world"))],
    );

    let text = display_text(&workspace, cx);
    assert!(
        text.contains("hello world"),
        "queued updates should render their final merged state: {text:?}"
    );
}

#[gpui::test]
fn streaming_suffix_only_reaches_wrap_map_as_the_tail_row(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let mut original = (0..160)
        .map(|row| format!("streamed response row {row}: **settled markdown** stays concealed"))
        .collect::<Vec<_>>()
        .join("\n");
    original.push('\n');
    original.push_str(&"one long streamed markdown paragraph ".repeat(300));
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("write a long response")],
            vec![assistant(&original, Some(UiMessagePhase::FinalAnswer))],
        )),
    );

    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, _, cx| {
            editor.update(cx, |editor, cx| {
                editor.display_map.update(cx, |map, cx| {
                    map.snapshot(cx);
                    map.take_wrap_sync_traces(cx);
                    map.take_wrap_width_changes(cx);
                });
            });
        })
        .expect("clear initial wrap edits");

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
                        keep_bytes: original.len(),
                        value: " appended suffix".to_owned(),
                    }),
                }],
            },
            status: None,
            context_used: None,
            usage: None,
        },
    );

    let (batches, width_changes) = workspace
        .update(cx, |_, _, cx| {
            editor.update(cx, |editor, cx| {
                editor.display_map.update(cx, |map, cx| {
                    map.snapshot(cx);
                    (
                        map.take_wrap_sync_traces(cx),
                        map.take_wrap_width_changes(cx),
                    )
                })
            })
        })
        .expect("read wrap edits");
    assert_incremental_wrap(&batches, &width_changes, 160, 1);

    let first_suffix = " first";
    let second_suffix = " second";
    feed_frames(
        &workspace,
        cx,
        [
            (
                agent(1),
                AgentRemoteFrame::Diff {
                    blocks: UiBlocksDiff {
                        truncate_to: None,
                        updates: vec![UiBlockUpdate {
                            index: 1,
                            block: UiBlockDiff::AssistantText(UiTextDiff {
                                keep_bytes: original.len() + " appended suffix".len(),
                                value: first_suffix.to_owned(),
                            }),
                        }],
                    },
                    status: None,
                    context_used: None,
                    usage: None,
                },
            ),
            (
                agent(1),
                AgentRemoteFrame::Diff {
                    blocks: UiBlocksDiff {
                        truncate_to: None,
                        updates: vec![UiBlockUpdate {
                            index: 1,
                            block: UiBlockDiff::AssistantText(UiTextDiff {
                                keep_bytes: original.len()
                                    + " appended suffix".len()
                                    + first_suffix.len(),
                                value: second_suffix.to_owned(),
                            }),
                        }],
                    },
                    status: None,
                    context_used: None,
                    usage: None,
                },
            ),
        ],
    );
    let (batches, width_changes) = workspace
        .update(cx, |_, _, cx| {
            editor.update(cx, |editor, cx| {
                editor.display_map.update(cx, |map, cx| {
                    map.snapshot(cx);
                    (
                        map.take_wrap_sync_traces(cx),
                        map.take_wrap_width_changes(cx),
                    )
                })
            })
        })
        .expect("read coalesced wrap edits");
    assert_incremental_wrap(&batches, &width_changes, 160, 1);
}

#[gpui::test]
fn document_preview_reconciles_decorations_when_appending_a_user_turn(cx: &mut TestAppContext) {
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
    let preview = workspace
        .update(cx, |workspace, window, cx| {
            let model = workspace.active_agent_model().expect("agent view");
            model.update(cx, |model, cx| model.preview_editor(window, cx))
        })
        .expect("open document preview");
    let folded_elisions = |cx: &mut TestAppContext| {
        workspace
            .update(cx, |_, _, cx| {
                preview.update(cx, |editor, cx| {
                    let snapshot = editor.display_snapshot(cx);
                    snapshot
                        .folded_display_elisions_intersecting_range(
                            multi_buffer::MultiBufferOffset(0)..snapshot.buffer_snapshot().len(),
                            true,
                        )
                        .into_iter()
                        .collect::<rustc_hash::FxHashSet<_>>()
                })
            })
            .expect("inspect document preview elisions")
    };
    let initial_elisions = folded_elisions(cx);
    assert_eq!(initial_elisions.len(), 1);

    // The existing decorated response becomes an interior excerpt, but is not
    // rebuilt. Its concrete editor decoration and reconciliation state must
    // remain paired rather than inserting a duplicate.
    feed_frame(
        &workspace,
        cx,
        agent(1),
        AgentRemoteFrame::Diff {
            blocks: UiBlocksDiff {
                truncate_to: None,
                updates: vec![UiBlockUpdate {
                    index: 2,
                    block: UiBlockDiff::Replace(user("continue")),
                }],
            },
            status: None,
            context_used: None,
            usage: None,
        },
    );
    assert_eq!(folded_elisions(cx), initial_elisions);
}

#[gpui::test]
fn document_preview_preserves_decorations_across_invisible_tail_status_change(
    cx: &mut TestAppContext,
) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("do work")],
            vec![
                assistant(&long_working_text(), Some(UiMessagePhase::Commentary)),
                UiBlock::Reasoning {
                    text: String::new(),
                },
            ],
        )),
    );
    let preview = workspace
        .update(cx, |workspace, window, cx| {
            let model = workspace.active_agent_model().expect("agent view");
            model.update(cx, |model, cx| model.preview_editor(window, cx))
        })
        .expect("open document preview");
    let folded_elisions = |cx: &mut TestAppContext| {
        workspace
            .update(cx, |_, _, cx| {
                preview.update(cx, |editor, cx| {
                    let snapshot = editor.display_snapshot(cx);
                    snapshot
                        .folded_display_elisions_intersecting_range(
                            multi_buffer::MultiBufferOffset(0)..snapshot.buffer_snapshot().len(),
                            true,
                        )
                        .into_iter()
                        .collect::<rustc_hash::FxHashSet<_>>()
                })
            })
            .expect("inspect document preview elisions")
    };
    let initial_elisions = folded_elisions(cx);
    assert_eq!(initial_elisions.len(), 1);

    // Only the invisible terminal reasoning buffer is rebuilt. Cropping the
    // preceding composed document tail must not discard decoration state for
    // its surviving excerpt and insert a duplicate editor object.
    feed_frame(
        &workspace,
        cx,
        agent(1),
        AgentRemoteFrame::Diff {
            blocks: UiBlocksDiff {
                truncate_to: None,
                updates: Vec::new(),
            },
            status: Some(UiAgentStatus::Idle),
            context_used: None,
            usage: None,
        },
    );
    assert_eq!(folded_elisions(cx), initial_elisions);
}

#[gpui::test]
fn suffix_rebuild_does_not_rewrap_settled_user_rows(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let user_text = (0..120)
        .map(|row| format!("settled user row {row}"))
        .collect::<Vec<_>>()
        .join("\n");
    let response = (0..100)
        .map(|row| format!("response row {row}"))
        .collect::<Vec<_>>()
        .join("\n");
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user(&user_text)],
            vec![assistant(&response, Some(UiMessagePhase::FinalAnswer))],
        )),
    );

    let editor = active_editor(&workspace, cx);
    let preview = workspace
        .update(cx, |workspace, window, cx| {
            let model = workspace.active_agent_model().expect("agent view");
            model.update(cx, |model, cx| model.preview_editor(window, cx))
        })
        .expect("open document preview");
    workspace
        .update(cx, |_, _, cx| {
            for editor in [&editor, &preview] {
                editor.update(cx, |editor, cx| {
                    editor.display_map.update(cx, |map, cx| {
                        map.snapshot(cx);
                        map.take_wrap_sync_traces(cx);
                        map.take_wrap_width_changes(cx);
                    });
                });
            }
        })
        .expect("clear initial wrap edits");

    // Updating two blocks deliberately drops the single-block incremental hint and
    // exercises transcript suffix reconstruction. The settled user excerpt must
    // retain its identity.
    feed_frame(
        &workspace,
        cx,
        agent(1),
        AgentRemoteFrame::Diff {
            blocks: UiBlocksDiff {
                truncate_to: None,
                updates: vec![
                    UiBlockUpdate {
                        index: 1,
                        block: UiBlockDiff::AssistantText(UiTextDiff {
                            keep_bytes: response.len(),
                            value: "\nappended response".to_owned(),
                        }),
                    },
                    UiBlockUpdate {
                        index: 2,
                        block: UiBlockDiff::Replace(UiBlock::Tool(tool(
                            "tool-1",
                            UiToolStatus::Running,
                            Some(1),
                            None,
                        ))),
                    },
                ],
            },
            status: None,
            context_used: None,
            usage: None,
        },
    );

    let (traces, width_changes) = workspace
        .update(cx, |_, _, cx| {
            editor.update(cx, |editor, cx| {
                editor.display_map.update(cx, |map, cx| {
                    map.snapshot(cx);
                    (
                        map.take_wrap_sync_traces(cx),
                        map.take_wrap_width_changes(cx),
                    )
                })
            })
        })
        .expect("read suffix rebuild wrap edits");
    assert!(width_changes.is_empty());
    let edits = traces
        .iter()
        .flat_map(|trace| &trace.input)
        .collect::<Vec<_>>();
    assert!(!edits.is_empty(), "suffix rebuild did not reach WrapMap");
    assert!(
        edits
            .iter()
            .all(|edit| { edit.old.start.row() >= 120 && edit.new.start.row() >= 120 }),
        "suffix rebuild invalidated settled user rows: {traces:#?}"
    );

    let preview_traces = workspace
        .update(cx, |_, _, cx| {
            preview.update(cx, |preview, cx| {
                preview.display_map.update(cx, |map, cx| {
                    map.snapshot(cx);
                    assert!(map.take_wrap_width_changes(cx).is_empty());
                    map.take_wrap_sync_traces(cx)
                })
            })
        })
        .expect("read document preview wrap edits");
    let preview_edits = preview_traces
        .iter()
        .flat_map(|trace| &trace.input)
        .collect::<Vec<_>>();
    assert!(!preview_edits.is_empty());
    assert!(
        preview_edits
            .iter()
            .all(|edit| { edit.old.start.row() >= 120 && edit.new.start.row() >= 120 }),
        "suffix rebuild invalidated settled document-preview rows: {preview_traces:#?}"
    );
}

fn assert_incremental_wrap(
    traces: &[editor::display_map::WrapSyncTrace],
    width_changes: &[(Option<gpui::Pixels>, Option<gpui::Pixels>)],
    first_changed_row: u32,
    expected_input_edits: usize,
) {
    assert!(
        width_changes.is_empty(),
        "streaming changed the editor wrap width: {width_changes:?}"
    );
    assert!(!traces.is_empty(), "the streamed append must reach WrapMap");
    let edits = traces
        .iter()
        .flat_map(|trace| &trace.input)
        .collect::<Vec<_>>();
    assert_eq!(
        edits.len(),
        expected_input_edits,
        "unexpected wrap input edits: {traces:#?}"
    );
    assert!(
        edits.iter().all(|edit| {
            edit.old.start.row() >= first_changed_row
                && edit.old.start.row() == edit.old.end.row()
                && edit.new.start.row() == edit.new.end.row()
        }),
        "streaming invalidated unchanged physical rows: {traces:#?}"
    );
    assert!(
        traces.iter().all(|trace| {
            trace
                .output
                .iter()
                .all(|edit| edit.old.start >= trace.old_input_row_start)
        }),
        "WrapMap invalidated display rows before its input row: {traces:#?}"
    );
}

/// Manual end-to-end benchmark for the path guarded above. It intentionally
/// uses protocol diffs and the production workspace/transcript/editor stack;
/// run with:
///
/// ```text
/// cargo test --release -p rho-gui --bin rho-gui \
///     benchmark_streaming_suffix_wrap_pipeline -- --ignored --nocapture
/// ```
#[gpui::test]
#[ignore = "manual streaming benchmark"]
fn benchmark_streaming_suffix_wrap_pipeline(cx: &mut TestAppContext) {
    const APPENDS: usize = 50;
    let workspace = test_workspace(cx);
    let mut streamed = "one long streamed markdown paragraph ".repeat(2_000);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("write a long response")],
            vec![assistant(&streamed, Some(UiMessagePhase::FinalAnswer))],
        )),
    );

    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, _, cx| {
            editor.update(cx, |editor, cx| {
                editor.display_map.update(cx, |map, cx| {
                    map.snapshot(cx);
                    map.take_wrap_sync_traces(cx);
                    map.take_wrap_width_changes(cx);
                });
            });
        })
        .expect("clear initial wrap edits");

    let started = std::time::Instant::now();
    for _ in 0..APPENDS {
        let keep_bytes = streamed.len();
        let suffix = " next";
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
                            keep_bytes,
                            value: suffix.to_owned(),
                        }),
                    }],
                },
                status: None,
                context_used: None,
                usage: None,
            },
        );
        streamed.push_str(suffix);
    }
    let elapsed = started.elapsed();

    let (batches, width_changes) = workspace
        .update(cx, |_, _, cx| {
            editor.update(cx, |editor, cx| {
                editor.display_map.update(cx, |map, cx| {
                    map.snapshot(cx);
                    (
                        map.take_wrap_sync_traces(cx),
                        map.take_wrap_width_changes(cx),
                    )
                })
            })
        })
        .expect("read benchmark wrap edits");
    assert!(
        width_changes.is_empty(),
        "streaming changed wrap width: {width_changes:?}"
    );
    let edit_count = batches.iter().map(|trace| trace.input.len()).sum::<usize>();
    assert_eq!(edit_count, APPENDS, "unexpected wrap edits: {batches:#?}");
    assert!(batches.iter().flat_map(|trace| &trace.input).all(|edit| {
        edit.old.start.row() == edit.old.end.row() && edit.new.start.row() == edit.new.end.row()
    }));
    let output_rows = batches
        .iter()
        .flat_map(|trace| &trace.output)
        .map(|edit| edit.old.end.0.saturating_sub(edit.old.start.0))
        .collect::<Vec<_>>();
    let output_rows_total = output_rows.iter().copied().map(u64::from).sum::<u64>();
    let output_rows_min = output_rows.iter().copied().min().unwrap_or(0);
    let output_rows_max = output_rows.iter().copied().max().unwrap_or(0);
    let output_patch_starts_at_physical_row = batches.iter().all(|trace| {
        trace
            .output
            .iter()
            .all(|edit| edit.old.start == trace.old_input_row_start)
    });
    eprintln!(
        "streaming_wrap_pipeline bytes={} appends={} input_edits={} output_patches={} output_rows_mean={:.1} output_rows_min={} output_rows_max={} output_starts_at_physical_row={} wrap_width_changes={} total={elapsed:?} per_append={:?}",
        streamed.len(),
        APPENDS,
        edit_count,
        output_rows.len(),
        output_rows_total as f64 / output_rows.len().max(1) as f64,
        output_rows_min,
        output_rows_max,
        output_patch_starts_at_physical_row,
        width_changes.len(),
        elapsed / APPENDS as u32,
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
            usage: None,
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

    // A streamed tool/status frame can rebuild the active turn instead of
    // taking the text-only fast path. The prompt excerpt and its cursor must
    // remain stable across that replacement too.
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("go")],
            vec![
                assistant("hello", Some(UiMessagePhase::FinalAnswer)),
                UiBlock::Tool(tool("t1", UiToolStatus::Running, None, None)),
            ],
        )),
    );
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| editor.insert("?", window, cx));
        })
        .expect("continue typing after turn rebuild");
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("draft!?"),
        "prompt cursor moved during active-turn replacement: {text:?}"
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
            usage: None,
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
            let view = workspace.active_agent_model().expect("agent view");
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
            let view = workspace.active_agent_model().expect("agent view");
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
fn subscribed_hidden_views_stay_warm(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(vec![user("one")], Vec::new())),
    );

    // Agent 2 has never been focused; its subscription still creates a warm
    // model so selecting it later does no transcript work.
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
                .agent_model(&agent(2))
                .expect("agent 2 view exists")
        })
        .expect("read workspace");
    let hidden_text = workspace
        .update(cx, |_, _, cx| {
            hidden_view.update(cx, |view, cx| view.buffer_text(cx))
        })
        .expect("read hidden view");
    assert!(
        hidden_text.contains("done"),
        "subscribed hidden views should stay synchronized: {hidden_text:?}"
    );

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.select_agent(Some(agent(2)), window, cx);
        })
        .expect("select agent 2");
    let text = display_text(&workspace, cx);
    assert!(
        text.contains("two") && text.contains("done"),
        "selecting a hidden agent should reuse its warm model: {text:?}"
    );
}

#[gpui::test]
fn frames_coalesce_while_subscribed_model_is_loading(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Frame {
                    agent_id: agent(2),
                    frame: snapshot_frame(state(
                        vec![user("go"), assistant("hel", None)],
                        Vec::new(),
                    )),
                    allocation: None,
                },
                window,
                cx,
            );
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Frame {
                    agent_id: agent(2),
                    frame: AgentRemoteFrame::Diff {
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
                        usage: None,
                    },
                    allocation: None,
                },
                window,
                cx,
            );
        })
        .expect("queue frames during initial load");
    cx.run_until_parked();

    let text = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .agent_model(&agent(2))
                .expect("subscribed model")
                .update(cx, |view, cx| view.buffer_text(cx))
        })
        .expect("read warmed model");
    assert!(
        text.contains("hello"),
        "queued diff was not applied: {text:?}"
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
            workspace.handle_event(
                HostId::default(),
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
            workspace.handle_event(HostId::default(), ConnEvent::TurnCancelled, window, cx);
        })
        .expect("handle cancellation acknowledgement");

    let text = display_text(&workspace, cx);
    assert!(
        !text.contains("[turn cancelled]"),
        "turn cancellation acknowledgement should not become persistent transcript text: {text:?}"
    );
}

#[gpui::test]
fn connection_recovery_is_transient_workspace_chrome(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Recovering(std::time::Duration::from_secs(17)),
                window,
                cx,
            );
            assert_eq!(
                workspace.connection_status_label().as_deref(),
                Some("recovering 17s")
            );
            workspace.handle_event(HostId::default(), ConnEvent::Recovered, window, cx);
            assert_eq!(workspace.connection_status_label(), None);
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Disconnected("timed out".to_owned()),
                window,
                cx,
            );
            assert_eq!(
                workspace.connection_status_label().as_deref(),
                Some("disconnected timed out")
            );
        })
        .expect("update connection status");
    assert!(
        !display_text(&workspace, cx).contains("disconnected"),
        "connection status belongs in workspace chrome, not transcript content"
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
            usage: Default::default(),
        }),
    );
    let spans = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .active_agent_model()
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

#[gpui::test]
fn total_cost_shows_in_status_chips(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        AgentRemoteFrame::Snapshot(UiAgentState {
            blocks: vec![user("go")],
            status: UiAgentStatus::Idle,
            context_used: Some(62_300),
            usage: Default::default(),
        }),
    );
    feed_frame(
        &workspace,
        cx,
        agent(1),
        AgentRemoteFrame::Diff {
            blocks: UiBlocksDiff {
                truncate_to: None,
                updates: Vec::new(),
            },
            status: None,
            context_used: None,
            usage: Some(rho_ui_proto::remote::UiAgentUsage {
                provider: "fable".to_owned(),
                total: rho_ui_proto::AgentUsageBucket {
                    input_tokens: 1_000_000,
                    cache_read_tokens: 1_000_000,
                    cache_write_tokens: 1_000_000,
                    output_tokens: 1_000_000,
                    ..Default::default()
                },
            }),
        },
    );

    let spans = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .active_agent_model()
                .expect("agent view")
                .read(cx)
                .status_span_text()
        })
        .expect("read spans");
    assert!(
        spans.contains("$73.50") && !spans.contains("tok"),
        "cost-only chip missing from status spans: {spans:?}"
    );
    assert!(
        spans.find("62k") < spans.find("$73.50"),
        "cost chip should follow context size: {spans:?}"
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
                .active_agent_model()
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
            usage: Default::default(),
        }),
    );
    let spans = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .active_agent_model()
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

/// `KeyBinding::new` panics at startup on unparseable keystrokes; the
/// terminal escape chord is the only binding with a non-alphanumeric key.
#[test]
fn terminal_escape_chord_parses() {
    for stroke in "ctrl-\\ ctrl-n".split(' ') {
        gpui::Keystroke::parse(stroke).expect("terminal escape chord must parse");
    }
}

/// The inline injection only runs over inline spans, so fenced code keeps
/// punctuation that would be markup in prose.
#[gpui::test]
fn fenced_code_keeps_its_asterisks(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("go")],
            vec![assistant(
                "```\n**bold**\nplain\n```\n",
                Some(UiMessagePhase::FinalAnswer),
            )],
        )),
    );
    cx.run_until_parked();
    assert!(display_text(&workspace, cx).contains("**bold**"));
}

/// Concealment changes display geometry, so it must remain stable when the
/// viewport moves rather than being removed and recreated around the screen.
#[gpui::test]
fn long_transcript_concealments_do_not_change_when_scrolling(cx: &mut TestAppContext) {
    let markup = (0..400)
        .map(|index| format!("line **{index}** of `history`\n"))
        .collect::<String>();
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("go")],
            vec![assistant(&markup, Some(UiMessagePhase::FinalAnswer))],
        )),
    );

    // Parsing and query-backed decoration are both asynchronous.
    for _ in 0..64 {
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(20));
    }
    cx.run_until_parked();
    let settled = display_text(&workspace, cx);
    assert!(settled.contains("line 399 of history"));
    assert!(!settled.contains("line **399** of `history`"));
    assert!(
        buffer_text(&workspace, cx).contains("line **0** of `history`"),
        "the buffer keeps the markup either way"
    );

    let editor = active_editor(&workspace, cx);
    let folds = concealed_fold_ids(&workspace, &editor, cx);
    assert!(
        folds.len() >= 1_000,
        "the whole transcript should be concealed"
    );

    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_scroll_position(gpui::point(0., 0.), window, cx);
            });
        })
        .expect("scroll to transcript start");
    cx.run_until_parked();
    assert_eq!(concealed_fold_ids(&workspace, &editor, cx), folds);

    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_scroll_position(gpui::point(0., 400.), window, cx);
            });
        })
        .expect("scroll through transcript");
    cx.run_until_parked();
    assert_eq!(concealed_fold_ids(&workspace, &editor, cx), folds);
}

#[gpui::test]
fn subscribed_transcript_eagerly_parses_history(cx: &mut TestAppContext) {
    let mut history = Vec::new();
    for turn in 0..40 {
        history.push(user(&format!("request {turn}")));
        history.push(assistant(
            &format!("assistant turn {turn}\n{}", "historical line\n".repeat(12)),
            Some(UiMessagePhase::FinalAnswer),
        ));
    }
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            history,
            vec![assistant(
                "assistant visible tail **bold**",
                Some(UiMessagePhase::FinalAnswer),
            )],
        )),
    );
    for _ in 0..64 {
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(20));
    }

    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, _, cx| {
            let buffers = editor.read(cx).buffer().read(cx).all_buffers();
            let middle = buffers
                .iter()
                .find(|buffer| buffer.read(cx).text().contains("assistant turn 20"))
                .expect("middle response buffer");
            let tail = buffers
                .iter()
                .find(|buffer| buffer.read(cx).text().contains("assistant visible tail"))
                .expect("visible response buffer");
            assert!(
                middle.read(cx).has_syntax_tree(),
                "subscribed history should be parsed before Ready"
            );
            assert!(
                tail.read(cx).has_syntax_tree(),
                "the visible tail should be parsed"
            );
        })
        .expect("inspect eager transcript syntax");
}

#[gpui::test]
fn prompt_typing_keeps_transcript_concealment_folds(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("go")],
            vec![assistant(
                "**bold** and `code`\n",
                Some(UiMessagePhase::FinalAnswer),
            )],
        )),
    );
    cx.run_until_parked();

    let editor = active_editor(&workspace, cx);
    let before = concealed_fold_ids(&workspace, &editor, cx);
    assert!(!before.is_empty());

    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| editor.insert("x", window, cx));
        })
        .expect("type in prompt");
    cx.run_until_parked();

    assert_eq!(concealed_fold_ids(&workspace, &editor, cx), before);
}

#[gpui::test]
fn plain_assistant_streaming_keeps_existing_concealment_folds(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let original = "**bold** and `code`\n";
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("go")],
            vec![assistant(original, Some(UiMessagePhase::FinalAnswer))],
        )),
    );
    cx.run_until_parked();

    let editor = active_editor(&workspace, cx);
    let before = concealed_fold_ids(&workspace, &editor, cx);
    assert!(!before.is_empty());

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
                        keep_bytes: original.len(),
                        value: "more plain text\n".to_owned(),
                    }),
                }],
            },
            status: None,
            context_used: None,
            usage: None,
        },
    );
    cx.run_until_parked();

    let after = concealed_fold_ids(&workspace, &editor, cx);
    assert_eq!(after, before, "streaming must not replace unchanged folds");
    let displayed = display_text(&workspace, cx);
    assert!(!displayed.contains("**bold**"));
    assert!(!displayed.contains("`code`"));
}

/// The block map may not assume display elisions arrive sorted or apart:
/// they are held in the order they were inserted, and two of them can cover
/// rows that meet or overlap. Composing an edit per elision assumed both,
/// and underflowed the row arithmetic when neither held.
#[gpui::test]
fn edits_under_overlapping_elisions_keep_the_block_map_consistent(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let lines = (0..40)
        .map(|index| format!("line {index} of the answer\n"))
        .collect::<String>();
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("go")],
            vec![assistant(&lines, Some(UiMessagePhase::FinalAnswer))],
        )),
    );

    // Two elisions over rows that overlap, inserted latest-first.
    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, _, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let elision = |start: usize, end: usize| editor::DisplayElisionProperties {
                    range: snapshot.anchor_before(multi_buffer::MultiBufferOffset(start))
                        ..snapshot.anchor_before(multi_buffer::MultiBufferOffset(end)),
                    tail_rows: 1,
                    height: Some(1),
                    style: editor::display_map::BlockStyle::Flex,
                    render: std::sync::Arc::new(|_| {
                        gpui::IntoElement::into_any_element(gpui::Empty)
                    }),
                    priority: 0,
                    type_tag: None,
                };
                editor.insert_display_elisions(vec![elision(300, 500)], None, cx);
                editor.insert_display_elisions(vec![elision(100, 320)], None, cx);
            });
        })
        .expect("insert overlapping elisions");

    // An edit inside both of them.
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("go")],
            vec![assistant(
                &format!("{lines}line 40 of the answer\n"),
                Some(UiMessagePhase::FinalAnswer),
            )],
        )),
    );

    let text = display_text(&workspace, cx);
    assert!(
        text.contains("line 40 of the answer"),
        "the edit should render: {text:?}"
    );
}

/// A turn of your own is a couple of lines in a thousand, so it renders
/// larger than the transcript around it - the one cue that survives being
/// seen out of the corner of an eye while scrolling.
#[gpui::test]
fn user_messages_render_larger_than_the_transcript_around_them(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("my question")],
            vec![assistant("the answer", Some(UiMessagePhase::FinalAnswer))],
        )),
    );

    let editor = active_editor(&workspace, cx);
    let lines = display_text(&workspace, cx);
    let row_of = |needle: &str| {
        lines
            .lines()
            .position(|line| line.contains(needle))
            .map(|row| editor::display_map::DisplayRow(row as u32))
            .unwrap_or_else(|| panic!("{needle:?} is not on screen: {lines:?}"))
    };
    let (question, answer) = (row_of("my question"), row_of("the answer"));

    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.snapshot(window, cx);
                assert_eq!(
                    snapshot.row_scale(question),
                    crate::style::USER_MESSAGE_SCALE,
                    "the user's own turn renders larger"
                );
                assert_eq!(
                    snapshot.row_scale(answer),
                    1.0,
                    "everything else renders at the transcript's size"
                );
            })
        })
        .expect("read display snapshot");
}

#[gpui::test]
fn streaming_replacement_does_not_inherit_previous_markdown_syntax(cx: &mut TestAppContext) {
    let replaced = test_workspace(cx);
    feed_frame(
        &replaced,
        cx,
        agent(2),
        snapshot_frame(state(
            vec![user("go")],
            vec![assistant("**bold text**", None)],
        )),
    );

    for _ in 0..64 {
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(20));
    }
    feed_frame(
        &replaced,
        cx,
        agent(2),
        AgentRemoteFrame::Diff {
            blocks: UiBlocksDiff {
                truncate_to: None,
                updates: vec![UiBlockUpdate {
                    index: 1,
                    block: UiBlockDiff::AssistantText(UiTextDiff {
                        keep_bytes: 0,
                        value: "plain text".to_owned(),
                    }),
                }],
            },
            status: None,
            context_used: None,
            usage: None,
        },
    );
    let highlights = syntax_highlights_for_text(&replaced, "plain text", cx);
    assert!(
        highlights.iter().all(Option::is_none),
        "replacement inherited the previous strong-emphasis highlight: {highlights:?}"
    );
}

#[gpui::test]
fn markdown_syntax_is_settled_independently_between_turns(cx: &mut TestAppContext) {
    let isolated = test_workspace(cx);
    feed_frame(
        &isolated,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("go")],
            vec![assistant(
                "target **bold text**",
                Some(UiMessagePhase::FinalAnswer),
            )],
        )),
    );

    let after_unclosed_fence = test_workspace(cx);
    feed_frame(
        &after_unclosed_fence,
        cx,
        agent(2),
        snapshot_frame(state(
            vec![
                user("first"),
                assistant("```text\nunclosed", Some(UiMessagePhase::FinalAnswer)),
                user("next"),
            ],
            vec![assistant(
                "target **bold text**",
                Some(UiMessagePhase::FinalAnswer),
            )],
        )),
    );

    for _ in 0..64 {
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        syntax_highlights_for_text(&after_unclosed_fence, "target **bold text**", cx),
        syntax_highlights_for_text(&isolated, "target **bold text**", cx),
    );
}

#[gpui::test]
fn markdown_and_tool_segments_use_separate_syntax_buffers(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![
                user("first request"),
                assistant("first assistant segment", Some(UiMessagePhase::Commentary)),
                UiBlock::Tool(tool("tool-1", UiToolStatus::Success, Some(10), Some(20))),
                assistant(
                    "second assistant segment",
                    Some(UiMessagePhase::FinalAnswer),
                ),
                user("second request"),
            ],
            vec![assistant("next turn response", None)],
        )),
    );

    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, _, cx| {
            let buffers = editor.read(cx).buffer().read(cx).all_buffers();
            let first = buffers
                .iter()
                .find(|buffer| buffer.read(cx).text().contains("first assistant segment"))
                .expect("first Markdown buffer");
            let second = buffers
                .iter()
                .find(|buffer| buffer.read(cx).text().contains("second assistant segment"))
                .expect("second Markdown buffer");
            let tool = buffers
                .iter()
                .find(|buffer| buffer.read(cx).text().contains("$ echo ok"))
                .expect("tool buffer");
            assert!(
                first.read(cx).language().is_some() && second.read(cx).language().is_some(),
                "assistant messages must retain Markdown syntax"
            );
            assert!(
                tool.read(cx).language().is_none(),
                "tool text must not inherit Markdown syntax or concealment"
            );
        })
        .expect("inspect transcript turn buffers");
}

#[gpui::test]
fn adding_markdown_turn_does_not_blank_settled_highlights(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("first")],
            vec![assistant(
                "settled **bold text**",
                Some(UiMessagePhase::FinalAnswer),
            )],
        )),
    );
    for _ in 0..64 {
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(20));
    }
    let settled = syntax_highlights_for_text(&workspace, "settled **bold text**", cx);
    assert!(settled.iter().any(Option::is_some));

    // Force the settled turn's parser into background-only mode. Adding a new
    // turn must not disturb that independent buffer's published highlights.
    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, _, cx| {
            let buffers = editor.read(cx).buffer().read(cx).all_buffers();
            buffers
                .into_iter()
                .find(|buffer| buffer.read(cx).text().contains("settled **bold text**"))
                .expect("transcript buffer")
                .update(cx, |buffer, _| buffer.set_sync_parse_timeout(None));
        })
        .expect("disable synchronous transcript parsing");

    feed_frame(
        &workspace,
        cx,
        agent(1),
        AgentRemoteFrame::Diff {
            blocks: UiBlocksDiff {
                truncate_to: None,
                updates: vec![
                    UiBlockUpdate {
                        index: 2,
                        block: UiBlockDiff::Replace(user("second")),
                    },
                    UiBlockUpdate {
                        index: 3,
                        block: UiBlockDiff::Replace(assistant("new response", None)),
                    },
                ],
            },
            status: None,
            context_used: None,
            usage: None,
        },
    );

    assert_eq!(
        syntax_highlights_for_text(&workspace, "settled **bold text**", cx),
        settled,
        "adding a turn blanked existing highlights while parsing",
    );
}

/// Every row of a user message scales, not just the one its anchor starts
/// on, and the mapping survives the folds that conceal markdown markup -
/// which shift display rows out of step with buffer rows.
#[gpui::test]
fn every_row_of_a_user_message_renders_larger(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("first line\nsecond line\nthird line")],
            vec![assistant(
                "## Heading\n\n**bold** answer\n",
                Some(UiMessagePhase::FinalAnswer),
            )],
        )),
    );
    cx.run_until_parked();

    let editor = active_editor(&workspace, cx);
    let lines = display_text(&workspace, cx);
    let row_of = |needle: &str| {
        lines
            .lines()
            .position(|line| line.contains(needle))
            .map(|row| editor::display_map::DisplayRow(row as u32))
            .unwrap_or_else(|| panic!("{needle:?} is not on screen: {lines:?}"))
    };
    let mine = ["first line", "second line", "third line"].map(row_of);
    let theirs = ["Heading", "bold answer"].map(row_of);

    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.snapshot(window, cx);
                for row in mine {
                    assert_eq!(
                        snapshot.row_scale(row),
                        crate::style::USER_MESSAGE_SCALE,
                        "every row of the user's turn renders larger: {lines:?}"
                    );
                }
                for row in theirs {
                    assert_eq!(
                        snapshot.row_scale(row),
                        1.0,
                        "the answer renders at the transcript's size: {lines:?}"
                    );
                }
            })
        })
        .expect("read display snapshot");
}

/// Markup that arrives in pieces has to end up concealed like markup that
/// arrived whole: a delimiter is only recognisable once its closing run is
/// there, so every delta re-renders the block and the folds have to follow.
#[gpui::test]
fn streamed_markup_conceals_once_its_delimiters_close(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(vec![user("go")], vec![assistant("", None)])),
    );

    let message = "Here is **bold** text, `code`, and **more strong** words.\n";
    let mut sent = 0;
    while sent < message.len() {
        let mut next = (sent + 3).min(message.len());
        while !message.is_char_boundary(next) {
            next += 1;
        }
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
                            keep_bytes: sent,
                            value: message[sent..next].to_owned(),
                        }),
                    }],
                },
                status: None,
                context_used: None,
                usage: None,
            },
        );
        sent = next;
    }

    for _ in 0..64 {
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(20));
    }
    cx.run_until_parked();
    let text = display_text(&workspace, cx);
    assert!(
        !text.contains("**"),
        "streamed markup should conceal like markup that arrived whole: {text:?}"
    );
}

#[gpui::test]
fn terminal_invisible_assistant_segment_rebuilds_its_turn_when_it_appears(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![
                user("go"),
                assistant("first", Some(UiMessagePhase::Commentary)),
            ],
            vec![assistant("", None)],
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
                    index: 2,
                    block: UiBlockDiff::AssistantText(UiTextDiff {
                        keep_bytes: 0,
                        value: "second".to_owned(),
                    }),
                }],
            },
            status: None,
            context_used: None,
            usage: None,
        },
    );

    let text = display_text(&workspace, cx);
    assert!(
        text.contains("first\nsecond"),
        "newly visible segment lost its turn separator: {text:?}"
    );
}

#[gpui::test]
fn invisible_response_chunk_adds_no_excerpt_boundary(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![
                user("first"),
                assistant("", Some(UiMessagePhase::FinalAnswer)),
                user("second"),
            ],
            Vec::new(),
        )),
    );

    assert_eq!(buffer_text(&workspace, cx), "first\n\nsecond\n\n");
}

#[gpui::test]
fn terminal_user_message_keeps_its_style_at_the_excerpt_boundary(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(vec![user("last user")], Vec::new())),
    );

    let runs = styled_runs(&workspace, cx);
    assert!(
        runs.iter()
            .any(|(text, color)| text.contains("last user") && color.is_some()),
        "terminal user text lost its semantic style: {runs:?}"
    );
}

#[gpui::test]
fn growing_document_preview_omits_the_terminal_blank_row(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(vec![user("first")], vec![assistant("second", None)])),
    );

    let preview = workspace
        .update(cx, |workspace, window, cx| {
            let model = workspace.active_agent_model().expect("agent view");
            model.update(cx, |model, cx| model.preview_editor(window, cx))
        })
        .expect("open preview");
    let text = workspace
        .update(cx, |_, _, cx| {
            preview.update(cx, |preview, cx| preview.text(cx))
        })
        .expect("read preview text");

    assert_eq!(text, "first\n\nsecond");
    assert_eq!(
        editor_excerpt_boundary_count(&workspace, &preview, cx),
        0,
        "attaching a preview should remove already-materialized excerpt boundaries"
    );
}

#[gpui::test]
fn streaming_markdown_parses_the_edited_turn_without_revisiting_history(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let mut history = Vec::new();
    for index in 0..250 {
        history.push(user(&format!("question {index}")));
        history.push(assistant(
            &format!("settled **answer {index}**"),
            Some(UiMessagePhase::FinalAnswer),
        ));
    }
    history.push(user("latest question"));
    let active_index = history.len();
    let initial = "## Initial heading\n\n**initial bold**";
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(history, vec![assistant(initial, None)])),
    );
    for _ in 0..64 {
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(20));
    }
    let first_parse = syntax_highlights_for_text(&workspace, initial, cx);
    assert!(
        first_parse.iter().any(Option::is_some),
        "the visible turn did not activate syntax: {first_parse:?}"
    );

    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, _, cx| {
            editor
                .read(cx)
                .buffer()
                .read(cx)
                .all_buffers()
                .into_iter()
                .find(|buffer| buffer.read(cx).text().contains(initial))
                .expect("transcript buffer")
                .update(cx, |buffer, _| {
                    buffer.set_sync_parse_timeout(Some(std::time::Duration::from_millis(1)))
                });
        })
        .expect("set transcript parse budget");

    feed_frame(
        &workspace,
        cx,
        agent(1),
        AgentRemoteFrame::Diff {
            blocks: UiBlocksDiff {
                truncate_to: None,
                updates: vec![UiBlockUpdate {
                    index: active_index,
                    block: UiBlockDiff::AssistantText(UiTextDiff {
                        keep_bytes: initial.len(),
                        value: "\n\n## New heading\n\n**new bold**".to_owned(),
                    }),
                }],
            },
            status: None,
            context_used: None,
            usage: None,
        },
    );

    let text = display_text(&workspace, cx);
    assert!(
        !text.contains("## New heading"),
        "heading flashed raw: {text:?}"
    );
    assert!(
        !text.contains("**new bold**"),
        "emphasis flashed raw: {text:?}"
    );
}

/// A `:eng-…:` tag written in the heading line itself is a binding: the
/// agent files under that heading with no daemon anchor at all, the raw
/// tag stays in the buffer (so line-wise copy carries it), and the
/// display conceals it behind the pretty decoration.
#[gpui::test]
fn heading_tags_file_agents_and_conceal_in_display(cx: &mut TestAppContext) {
    use rho_ui_proto::desk::{DeskOperation, DeskSnapshot};
    use rho_ui_proto::{
        AgentDisposition, AgentRole, AuthState, UiAgentSummary, UiAttention, WorkspaceInfo,
    };

    let summary = |id: u64, name: &str| UiAgentSummary {
        agent_id: agent(id),
        parent_agent: None,
        display_name: Some(name.to_owned()),
        created_at: UnixMs(id),
        updated_at: UnixMs(id),
        role: AgentRole::default(),
        workspace: WorkspaceInfo::UserCheckout { repo: "/tmp".into() },
        attention: UiAttention::Quiet,
        last_active: UnixMs(id),
        hidden: false,
        disposition: AgentDisposition::Pending,
        last_user_message_text: String::new(),
        activity: None,
        turn_report: None,
        labels: Vec::new(),
    };

    let desk_text = format!("* One :eng-{}:\nbody\n* Two\n", agent(1).encoded());
    let mut source = text::Buffer::new(
        text::ReplicaId::new(8),
        text::BufferId::new(1).unwrap(),
        "",
    );
    let operation = DeskOperation::from_text(&source.edit([(0..0, desk_text.as_str())]));
    let desk_snapshot = DeskSnapshot {
        text: source.snapshot().text(),
        operations: vec![operation],
        transactions: Vec::new(),
        replicas: Vec::new(),
    };

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Ready {
                    agents: vec![summary(1, "planner"), summary(2, "drifter")],
                    iris_agent: None,
                    projects: Vec::new(),
                    auth: AuthState {
                        disabled_namespaces: Vec::new(),
                        active_namespace: None,
                        namespaces: Vec::new(),
                    },
                    machine_seed: 0,
                    agent_counter: 100,
                },
                window,
                cx,
            );
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskSnapshot {
                    snapshot: desk_snapshot,
                    replica_id: 42,
                },
                window,
                cx,
            );
            workspace.sync_dashboard(window, cx);
            let focus_handle = workspace.dashboard_editor().read(cx).focus_handle(cx);
            window.focus(&focus_handle, cx);
            workspace.dashboard_editor().update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    let offset = editor::MultiBufferOffset(2);
                    selections.select_ranges([offset..offset]);
                });
            });
        })
        .expect("update workspace");
    cx.run_until_parked();

    let buffer_text = workspace
        .update(cx, |workspace, _, cx| {
            let editor = workspace.dashboard_editor();
            editor.read(cx).buffer().read(cx).snapshot(cx).text()
        })
        .expect("read dashboard");
    assert_eq!(
        buffer_text,
        format!(
            "* One :eng-{}:\nbody\n* Two\n\nUnfiled · 1\n  · drifter  eng-{}",
            agent(1).encoded(),
            &agent(2).encoded()[..4],
        ),
        "tagged agent should file under its heading, tag intact in the buffer"
    );

    let display = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .dashboard_editor()
                .update(cx, |editor, cx| editor.display_text(cx))
        })
        .expect("read display text");
    assert!(
        !display.contains(":eng-"),
        "raw tag should be concealed: {display:?}"
    );
    assert!(
        display.contains("◉ One"),
        "the bullet and title stay visible: {display:?}"
    );
    // The chip is an end-of-line hint: painted after the line, outside
    // text flow, so it never appears in display text.
    let hints = workspace
        .update(cx, |workspace, _, cx| {
            workspace.dashboard_editor().read(cx).eol_hints().len()
        })
        .expect("read hints");
    assert_eq!(hints, 1, "the staffed heading wears its chip as a hint");

    cx.simulate_keystrokes(*workspace, "escape space e");
    cx.run_until_parked();
    let raw_buffer = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .dashboard_editor()
                .read(cx)
                .buffer()
                .read(cx)
                .snapshot(cx)
                .text()
        })
        .expect("read raw dashboard");
    assert_eq!(raw_buffer, desk_text, "raw mode contains only Desk source");
    let raw_display = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .dashboard_editor()
                .update(cx, |editor, cx| editor.display_text(cx))
        })
        .expect("read raw display");
    assert!(raw_display.contains("* One :eng-"), "raw display: {raw_display:?}");
    let hints = workspace
        .update(cx, |workspace, _, cx| {
            workspace.dashboard_editor().read(cx).eol_hints().len()
        })
        .expect("read raw hints");
    assert_eq!(hints, 0, "raw mode has no generated hints");

    cx.simulate_keystrokes(*workspace, "i x escape");
    cx.run_until_parked();
    let edited_source = workspace
        .update(cx, |workspace, _, cx| {
            let buffer = workspace.desk_buffer_for_test(HostId::default()).unwrap();
            buffer.read(cx).text()
        })
        .expect("read edited Desk source");
    assert!(
        edited_source.starts_with("* xOne :eng-"),
        "raw Desk should be directly editable: {edited_source:?}"
    );

    cx.simulate_keystrokes(*workspace, "space e");
    cx.run_until_parked();
    let composed_display = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .dashboard_editor()
                .update(cx, |editor, cx| editor.display_text(cx))
        })
        .expect("read restored composed display");
    assert!(composed_display.contains("xOne"));
    assert!(!composed_display.contains(":eng-"));
}

/// The preview pane follows the cursor: a staffed heading (or its body)
/// shows its agent, and moving onto an unstaffed heading hides the
/// preview instead of retaining the last agent.
#[gpui::test]
fn preview_clears_when_the_cursor_leaves_a_staffed_heading(cx: &mut TestAppContext) {
    use rho_ui_proto::desk::{DeskOperation, DeskSnapshot};
    use rho_ui_proto::{
        AgentDisposition, AgentRole, AuthState, UiAgentSummary, UiAttention, WorkspaceInfo,
    };

    let summary = |id: u64, name: &str| UiAgentSummary {
        agent_id: agent(id),
        parent_agent: None,
        display_name: Some(name.to_owned()),
        created_at: UnixMs(id),
        updated_at: UnixMs(id),
        role: AgentRole::default(),
        workspace: WorkspaceInfo::UserCheckout { repo: "/tmp".into() },
        attention: UiAttention::Quiet,
        last_active: UnixMs(id),
        hidden: false,
        disposition: AgentDisposition::Pending,
        last_user_message_text: String::new(),
        activity: None,
        turn_report: None,
        labels: Vec::new(),
    };

    let desk_text = format!("* One :eng-{}:\nbody\n* Two\n", agent(1).encoded());
    let mut source = text::Buffer::new(
        text::ReplicaId::new(8),
        text::BufferId::new(1).unwrap(),
        "",
    );
    let operation = DeskOperation::from_text(&source.edit([(0..0, desk_text.as_str())]));
    let desk_snapshot = DeskSnapshot {
        text: source.snapshot().text(),
        operations: vec![operation],
        transactions: Vec::new(),
        replicas: Vec::new(),
    };

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Ready {
                    agents: vec![summary(1, "planner")],
                    iris_agent: None,
                    projects: Vec::new(),
                    auth: AuthState {
                        namespaces: Vec::new(),
                        disabled_namespaces: Vec::new(),
                        active_namespace: None,
                    },
                    machine_seed: 0,
                    agent_counter: 100,
                },
                window,
                cx,
            );
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskSnapshot {
                    snapshot: desk_snapshot,
                    replica_id: 42,
                },
                window,
                cx,
            );
            workspace.sync_dashboard(window, cx);
            let focus_handle = workspace.dashboard_editor().read(cx).focus_handle(cx);
            window.focus(&focus_handle, cx);
        })
        .expect("update workspace");
    cx.update(|cx| cx.refresh_windows());
    cx.run_until_parked();

    let select = |workspace: &gpui::WindowHandle<Workspace>,
                  cx: &mut TestAppContext,
                  offset: usize| {
        workspace
            .update(cx, |workspace, window, cx| {
                let source = workspace.desk_buffer_for_test(HostId::default()).unwrap();
                let source_anchor = source.read(cx).anchor_after(offset);
                workspace.dashboard_editor().update(cx, |editor, cx| {
                    let anchor = editor
                        .buffer()
                        .read(cx)
                        .snapshot(cx)
                        .anchor_in_excerpt(source_anchor)
                        .expect("Desk offset is visible");
                    editor.change_selections(Default::default(), window, cx, |selections| {
                        selections.select_anchor_ranges([anchor..anchor]);
                    });
                });
            })
            .expect("move dashboard cursor");
        cx.run_until_parked();
    };
    let preview = |workspace: &gpui::WindowHandle<Workspace>, cx: &mut TestAppContext| {
        workspace
            .update(cx, |workspace, _, _| workspace.dashboard_preview_agent())
            .expect("read preview")
    };

    // Inside the staffed heading's body.
    select(&workspace, cx, desk_text.find("body").expect("body") + 2);
    assert_eq!(
        preview(&workspace, cx),
        Some(agent(1)),
        "staffed heading's body should preview its agent"
    );

    // On the unstaffed heading below.
    select(&workspace, cx, desk_text.find("* Two").expect("Two") + 2);
    assert_eq!(
        preview(&workspace, cx),
        None,
        "unstaffed heading should hide the preview"
    );
}

/// TAB on a staffed heading cycles its fold like any other: the tag
/// conceal (an adjacent zero-width fold ending exactly where the
/// subtree fold starts) must not eat the cycle.
#[gpui::test]
fn tab_cycles_folds_on_a_staffed_heading(cx: &mut TestAppContext) {
    use rho_ui_proto::desk::{DeskOperation, DeskSnapshot};
    use rho_ui_proto::{
        AgentDisposition, AgentRole, AuthState, UiAgentSummary, UiAttention, WorkspaceInfo,
    };

    let summary = UiAgentSummary {
        agent_id: agent(1),
        parent_agent: None,
        display_name: Some("planner".to_owned()),
        created_at: UnixMs(1),
        updated_at: UnixMs(1),
        role: AgentRole::default(),
        workspace: WorkspaceInfo::UserCheckout { repo: "/tmp".into() },
        attention: UiAttention::Quiet,
        last_active: UnixMs(1),
        hidden: false,
        disposition: AgentDisposition::Pending,
        last_user_message_text: String::new(),
        activity: None,
        turn_report: None,
        labels: Vec::new(),
    };

    let desk_text = format!(
        "* One :eng-{}:\nbody\n** Kid\nkid stuff\n* Two\n",
        agent(1).encoded()
    );
    let mut source = text::Buffer::new(
        text::ReplicaId::new(8),
        text::BufferId::new(1).unwrap(),
        "",
    );
    let operation = DeskOperation::from_text(&source.edit([(0..0, desk_text.as_str())]));
    let desk_snapshot = DeskSnapshot {
        text: source.snapshot().text(),
        operations: vec![operation],
        transactions: Vec::new(),
        replicas: Vec::new(),
    };

    let workspace = test_workspace(cx);
    cx.update(bind_test_keymaps);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Ready {
                    agents: vec![summary],
                    iris_agent: None,
                    projects: Vec::new(),
                    auth: AuthState {
                        namespaces: Vec::new(),
                        disabled_namespaces: Vec::new(),
                        active_namespace: None,
                    },
                    machine_seed: 0,
                    agent_counter: 100,
                },
                window,
                cx,
            );
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskSnapshot {
                    snapshot: desk_snapshot,
                    replica_id: 42,
                },
                window,
                cx,
            );
            workspace.sync_dashboard(window, cx);
            let focus_handle = workspace.dashboard_editor().read(cx).focus_handle(cx);
            window.focus(&focus_handle, cx);
            workspace.dashboard_editor().update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    let offset = editor::MultiBufferOffset(2);
                    selections.select_ranges([offset..offset]);
                });
            });
        })
        .expect("update workspace");
    cx.update(|cx| cx.refresh_windows());
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "escape");
    cx.run_until_parked();

    let display = |cx: &mut TestAppContext| {
        workspace
            .update(cx, |workspace, _, cx| {
                workspace
                    .dashboard_editor()
                    .update(cx, |editor, cx| editor.display_text(cx))
            })
            .expect("read display text")
    };

    cx.simulate_keystrokes(*workspace, "tab");
    cx.run_until_parked();
    let folded = display(cx);
    assert!(!folded.contains("body"), "folded: {folded:?}");
    assert!(!folded.contains("Kid"), "folded: {folded:?}");
    assert!(folded.contains('…'), "folded: {folded:?}");

    cx.simulate_keystrokes(*workspace, "tab");
    cx.run_until_parked();
    let children = display(cx);
    assert!(!children.contains("body"), "children: {children:?}");
    assert!(children.contains("Kid"), "children: {children:?}");
    assert!(!children.contains("kid stuff"), "children: {children:?}");

    cx.simulate_keystrokes(*workspace, "tab");
    cx.run_until_parked();
    let expanded = display(cx);
    assert!(expanded.contains("kid stuff"), "expanded: {expanded:?}");

    // `A` on the heading: the position past the concealed tag is not
    // restable, so append lands at the visible title end and the typed
    // text stays ahead of the binding.
    cx.simulate_keystrokes(*workspace, "escape shift-a");
    cx.simulate_keystrokes(*workspace, "space m o r e");
    cx.run_until_parked();
    let text = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .dashboard_editor()
                .read(cx)
                .buffer()
                .read(cx)
                .snapshot(cx)
                .text()
        })
        .expect("read dashboard");
    assert!(
        text.starts_with("* One more :eng-"),
        "append must stay ahead of the tag: {text:?}"
    );
}

/// Org's S-TAB: OVERVIEW folds every top-level subtree, CONTENTS shows
/// every heading line and nothing else, SHOW ALL opens the document.
#[gpui::test]
fn shift_tab_cycles_overview_contents_show_all(cx: &mut TestAppContext) {
    use rho_ui_proto::desk::{DeskOperation, DeskSnapshot};

    let desk_text = "* One\nbody\n** Kid\nkid stuff\n* Two\ntwo body\n";
    let mut source = text::Buffer::new(
        text::ReplicaId::new(8),
        text::BufferId::new(1).unwrap(),
        "",
    );
    let operation = DeskOperation::from_text(&source.edit([(0..0, desk_text)]));
    let workspace = test_workspace(cx);
    cx.update(bind_test_keymaps);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskSnapshot {
                    snapshot: DeskSnapshot {
                        text: source.snapshot().text(),
                        operations: vec![operation],
                        transactions: Vec::new(),
                        replicas: Vec::new(),
                    },
                    replica_id: 42,
                },
                window,
                cx,
            );
            workspace.sync_dashboard(window, cx);
            let focus_handle = workspace.dashboard_editor().read(cx).focus_handle(cx);
            window.focus(&focus_handle, cx);
            workspace.dashboard_editor().update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    let offset = editor::MultiBufferOffset(2);
                    selections.select_ranges([offset..offset]);
                });
            });
        })
        .expect("set up dashboard");
    cx.update(|cx| cx.refresh_windows());
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "escape");
    cx.run_until_parked();

    let display = |cx: &mut TestAppContext| {
        workspace
            .update(cx, |workspace, _, cx| {
                workspace
                    .dashboard_editor()
                    .update(cx, |editor, cx| editor.display_text(cx))
            })
            .expect("read display text")
    };

    cx.simulate_keystrokes(*workspace, "shift-tab");
    cx.run_until_parked();
    let overview = display(cx);
    assert!(!overview.contains("body"), "overview: {overview:?}");
    assert!(!overview.contains("Kid"), "overview: {overview:?}");
    assert!(overview.contains("One"), "overview: {overview:?}");
    assert!(overview.contains("Two"), "overview: {overview:?}");

    cx.simulate_keystrokes(*workspace, "shift-tab");
    cx.run_until_parked();
    let contents = display(cx);
    assert!(contents.contains("One"), "contents: {contents:?}");
    assert!(contents.contains("Kid"), "contents: {contents:?}");
    assert!(contents.contains("Two"), "contents: {contents:?}");
    assert!(!contents.contains("body"), "contents: {contents:?}");
    assert!(!contents.contains("kid stuff"), "contents: {contents:?}");

    cx.simulate_keystrokes(*workspace, "shift-tab");
    cx.run_until_parked();
    let all = display(cx);
    assert!(all.contains("body"), "show all: {all:?}");
    assert!(all.contains("kid stuff"), "show all: {all:?}");
    assert!(all.contains("two body"), "show all: {all:?}");
}

/// Collapse is a display fold over the subtree: TAB cycles folded →
/// children → expanded, the buffer keeps the text throughout, and the
/// fold is anchored — edits above it must not pop it open.
#[gpui::test]
fn collapsed_subtree_folds_in_the_display_and_survives_edits(cx: &mut TestAppContext) {
    use rho_ui_proto::desk::{DeskOperation, DeskSnapshot};

    let desk_text = "* One\nbody\n** Kid\nkid stuff\n* Two\n";
    let mut source = text::Buffer::new(
        text::ReplicaId::new(8),
        text::BufferId::new(1).unwrap(),
        "",
    );
    let operation = DeskOperation::from_text(&source.edit([(0..0, desk_text)]));
    let desk_snapshot = DeskSnapshot {
        text: source.snapshot().text(),
        operations: vec![operation],
        transactions: Vec::new(),
        replicas: Vec::new(),
    };

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskSnapshot {
                    snapshot: desk_snapshot,
                    replica_id: 42,
                },
                window,
                cx,
            );
            workspace.sync_dashboard(window, cx);
        })
        .expect("update workspace");
    cx.run_until_parked();

    let display = |cx: &mut TestAppContext| {
        workspace
            .update(cx, |workspace, _, cx| {
                workspace
                    .dashboard_editor()
                    .update(cx, |editor, cx| editor.display_text(cx))
            })
            .expect("read display text")
    };
    let cycle = |cx: &mut TestAppContext| {
        workspace
            .update(cx, |workspace, window, cx| {
                workspace.dashboard_cycle_fold_for_test(HostId::default(), 0, window, cx);
            })
            .expect("cycle fold");
        cx.run_until_parked();
    };

    // Expanded → folded: the whole subtree hides behind the heading.
    cycle(cx);
    let folded = display(cx);
    assert!(!folded.contains("body"), "folded: {folded:?}");
    assert!(!folded.contains("Kid"), "folded: {folded:?}");
    assert!(folded.contains("One"), "folded: {folded:?}");
    assert!(folded.contains("Two"), "folded: {folded:?}");
    assert!(folded.contains('…'), "folded: {folded:?}");
    // The star prefix conceals behind an org-modern bullet on folded
    // and expanded headings alike; the chevron placeholder carries the
    // fold state.
    assert!(folded.contains("◉ One"), "folded: {folded:?}");
    assert!(folded.contains("◉ Two"), "folded: {folded:?}");

    // Folded → children, org's CHILDREN state: only the child heading
    // line joins the parent — the body and the child's subtree stay
    // hidden.
    cycle(cx);
    let children = display(cx);
    assert!(!children.contains("body"), "children: {children:?}");
    assert!(children.contains("Kid"), "children: {children:?}");
    assert!(!children.contains("kid stuff"), "children: {children:?}");

    // The fold is anchored: an edit above it shifts every offset and
    // the child must stay folded.
    workspace
        .update(cx, |workspace, window, cx| {
            let buffer = workspace
                .desk_buffer_for_test(HostId::default())
                .expect("desk buffer");
            buffer.update(cx, |buffer, cx| {
                buffer.edit([(0..0, "* Zero\nzero body\n")], None, cx);
            });
            workspace.sync_dashboard(window, cx);
        })
        .expect("edit above fold");
    cx.run_until_parked();
    let shifted = display(cx);
    assert!(shifted.contains("zero body"), "shifted: {shifted:?}");
    assert!(shifted.contains("Kid"), "shifted: {shifted:?}");
    assert!(
        !shifted.contains("kid stuff"),
        "edit above the fold must not pop it open: {shifted:?}"
    );

    // Children → fully expanded, from the shifted offset.
    let offset = "* Zero\nzero body\n".len();
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.dashboard_cycle_fold_for_test(HostId::default(), offset, window, cx);
        })
        .expect("cycle fold");
    cx.run_until_parked();
    let expanded = display(cx);
    assert!(expanded.contains("kid stuff"), "expanded: {expanded:?}");
}

#[gpui::test]
fn vim_treats_a_collapsed_subtree_as_one_line(cx: &mut TestAppContext) {
    use rho_ui_proto::desk::{DeskOperation, DeskSnapshot};

    let desk_text = "* One\nbody\n* Two\n";
    let mut source = text::Buffer::new(
        text::ReplicaId::new(8),
        text::BufferId::new(1).unwrap(),
        "",
    );
    let operation = DeskOperation::from_text(&source.edit([(0..0, desk_text)]));
    let workspace = test_workspace(cx);
    cx.update(bind_test_keymaps);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskSnapshot {
                    snapshot: DeskSnapshot {
                        text: source.snapshot().text(),
                        operations: vec![operation],
                        transactions: Vec::new(),
                        replicas: Vec::new(),
                    },
                    replica_id: 42,
                },
                window,
                cx,
            );
            workspace.sync_dashboard(window, cx);
            workspace.dashboard_cycle_fold_for_test(HostId::default(), 0, window, cx);

            let editor = workspace.dashboard_editor();
            window.focus(&editor.read(cx).focus_handle(cx), cx);
            editor.update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    let offset = editor::MultiBufferOffset(0);
                    selections.select_ranges([offset..offset]);
                });
            });
        })
        .expect("set up folded dashboard");
    cx.update(|cx| cx.refresh_windows());
    cx.run_until_parked();

    cx.simulate_keystrokes(*workspace, "escape o x escape");
    cx.run_until_parked();
    let text = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .dashboard_editor()
                .read(cx)
                .buffer()
                .read(cx)
                .snapshot(cx)
                .text()
        })
        .expect("read dashboard");
    assert!(
        text.starts_with("* One\nbody\nx\n* Two\n"),
        "o should insert after the collapsed subtree: {text:?}"
    );
    // The reparsed subtree claims the new line, but a fold never
    // captures the cursor: with the cursor still on it, the typed line
    // stays visible below the fold.
    let display = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .dashboard_editor()
                .update(cx, |editor, cx| editor.display_text(cx))
        })
        .expect("read display text");
    assert!(
        display.contains("\nx"),
        "the line under the cursor must stay visible: {display:?}"
    );
    assert!(
        !display.contains("body"),
        "the rest of the subtree stays folded: {display:?}"
    );

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.dashboard_editor().update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    let offset = editor::MultiBufferOffset(0);
                    selections.select_ranges([offset..offset]);
                });
            });
            workspace.sync_dashboard(window, cx);
        })
        .expect("move cursor to folded heading");
    cx.run_until_parked();
    // The fold is persistent org-style state, rear-nonsticky at its
    // end: the typed line stays outside it and visible even after the
    // cursor leaves, until the heading is cycled again.
    let display = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .dashboard_editor()
                .update(cx, |editor, cx| editor.display_text(cx))
        })
        .expect("read display text");
    assert!(
        display.contains("\nx"),
        "the typed line stays visible off-cursor: {display:?}"
    );
    assert!(
        !display.contains("body"),
        "the original subtree stays folded: {display:?}"
    );
    cx.simulate_keystrokes(*workspace, "escape x d");
    cx.run_until_parked();
    let text = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .dashboard_editor()
                .read(cx)
                .buffer()
                .read(cx)
                .snapshot(cx)
                .text()
        })
        .expect("read dashboard");
    // The typed line sits outside the fold, so the fold-merged line is
    // heading plus original body — deletion takes exactly that.
    assert!(
        text.starts_with("x\n* Two\n"),
        "helix line deletion should remove the folded subtree only: {text:?}"
    );
}

/// The home view: bound agents stay in compact heading hints until `g t`
/// projects their runtime tree; unbound roots live in the Unfiled tail.
#[gpui::test]
fn home_view_interleaves_document_and_agent_rows(cx: &mut TestAppContext) {
    use rho_ui_proto::desk::{DeskOperation, DeskSnapshot};
    use rho_ui_proto::{
        AgentDisposition, AgentRole, AuthState, UiAgentSummary, UiAttention, WorkspaceInfo,
    };

    let summary = |id: u64, name: &str| UiAgentSummary {
        agent_id: agent(id),
        parent_agent: None,
        display_name: Some(name.to_owned()),
        created_at: UnixMs(id),
        updated_at: UnixMs(id),
        role: AgentRole::default(),
        workspace: WorkspaceInfo::UserCheckout { repo: "/tmp".into() },
        attention: UiAttention::Quiet,
        last_active: UnixMs(id),
        hidden: false,
        disposition: AgentDisposition::Pending,
        last_user_message_text: String::new(),
        activity: None,
        turn_report: None,
        labels: Vec::new(),
    };

    let mut source = text::Buffer::new(
        text::ReplicaId::new(8),
        text::BufferId::new(1).unwrap(),
        "",
    );
    let desk_text = format!("* One :eng-{}:\nbody\n* Two\n", agent(1).encoded());
    let operation = DeskOperation::from_text(&source.edit([(0..0, desk_text.as_str())]));
    let desk_snapshot = DeskSnapshot {
        text: source.snapshot().text(),
        operations: vec![operation],
        transactions: Vec::new(),
        replicas: Vec::new(),
    };

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Ready {
                    agents: vec![summary(1, "planner"), summary(2, "drifter")],
                    iris_agent: None,
                    projects: Vec::new(),
                    auth: AuthState {
                        disabled_namespaces: Vec::new(),
                        active_namespace: None,
                        namespaces: Vec::new(),
                    },
                    machine_seed: 0,
                    agent_counter: 100,
                },
                window,
                cx,
            );
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskSnapshot {
                    snapshot: desk_snapshot,
                    replica_id: 42,
                },
                window,
                cx,
            );
            workspace.sync_dashboard(window, cx);
            let focus_handle = workspace.dashboard_editor().read(cx).focus_handle(cx);
            window.focus(&focus_handle, cx);
            workspace.dashboard_editor().update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    let offset = editor::MultiBufferOffset(2);
                    selections.select_ranges([offset..offset]);
                });
            });
        })
        .expect("update workspace");
    cx.run_until_parked();

    let dashboard_text = |workspace: &WindowHandle<Workspace>, cx: &mut TestAppContext| {
        workspace
            .update(cx, |workspace, _, cx| {
                let editor = workspace.dashboard_editor();
                editor.read(cx).buffer().read(cx).snapshot(cx).text()
            })
            .expect("read dashboard")
    };
    // The tagged agent starts as only a compact heading hint.
    assert_eq!(
        dashboard_text(&workspace, cx),
        format!(
            "* One :eng-{}:\nbody\n* Two\n\nUnfiled · 1\n  · drifter  eng-{}",
            agent(1).encoded(),
            &agent(2).encoded()[..4],
        )
    );
    let hints = workspace
        .update(cx, |workspace, _, cx| {
            workspace.dashboard_editor().read(cx).eol_hints().len()
        })
        .expect("read hints");
    assert_eq!(hints, 1, "the staffed heading wears its chip as a hint");

    cx.simulate_keystrokes(*workspace, "escape g t");
    cx.run_until_parked();
    assert_eq!(
        dashboard_text(&workspace, cx),
        format!(
            "* One :eng-{}:\n  · planner  eng-{}\nbody\n* Two\n\nUnfiled · 1\n  · drifter  eng-{}",
            agent(1).encoded(),
            &agent(1).encoded()[..4],
            &agent(2).encoded()[..4],
        ),
        "g t should explicitly project the runtime tree"
    );
    let hints = workspace
        .update(cx, |workspace, _, cx| {
            workspace.dashboard_editor().read(cx).eol_hints().len()
        })
        .expect("read expanded hints");
    assert_eq!(hints, 0, "the open portal replaces its compact hint");

    cx.simulate_keystrokes(*workspace, "g t");
    cx.run_until_parked();
    assert_eq!(
        dashboard_text(&workspace, cx),
        format!(
            "* One :eng-{}:\nbody\n* Two\n\nUnfiled · 1\n  · drifter  eng-{}",
            agent(1).encoded(),
            &agent(2).encoded()[..4],
        ),
        "a second g t should return to the compact hint-only view"
    );
    let hints = workspace
        .update(cx, |workspace, _, cx| {
            workspace.dashboard_editor().read(cx).eol_hints().len()
        })
        .expect("read collapsed hints");
    assert_eq!(hints, 1, "closing the portal restores its compact hint");

    // Deleting the tag from the text is the unbind: rows rearrange and
    // both agents fall back to Unfiled.
    workspace
        .update(cx, |workspace, window, cx| {
            let buffer = workspace.desk_buffer_for_test(HostId::default()).unwrap();
            buffer.update(cx, |buffer, cx| {
                let tag_start = "* One".len();
                let tag_end = desk_text.find('\n').unwrap();
                buffer.edit([(tag_start..tag_end, "")], None, cx);
            });
            workspace.sync_dashboard(window, cx);
        })
        .expect("update workspace");
    cx.run_until_parked();
    assert_eq!(
        dashboard_text(&workspace, cx),
        format!(
            "* One\nbody\n* Two\n\nUnfiled · 2\n  · drifter  eng-{}\n  · planner  eng-{}",
            &agent(2).encoded()[..4],
            &agent(1).encoded()[..4],
        )
    );
}

/// Insert-mode enter in desk document text is a newline — the submit
/// binding only means "send" inside draft rows.
#[gpui::test]
fn insert_mode_enter_stays_a_newline_in_desk_text(cx: &mut TestAppContext) {
    use rho_ui_proto::desk::{DeskOperation, DeskSnapshot};

    let mut source = text::Buffer::new(
        text::ReplicaId::new(8),
        text::BufferId::new(1).unwrap(),
        "",
    );
    let operation = DeskOperation::from_text(&source.edit([(0..0, "* One\nbody\n* Two\n")]));
    let desk_snapshot = DeskSnapshot {
        text: source.snapshot().text(),
        operations: vec![operation],
        transactions: Vec::new(),
        replicas: Vec::new(),
    };

    let workspace = test_workspace(cx);
    cx.update(bind_test_keymaps);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Ready {
                    agents: Vec::new(),
                    iris_agent: None,
                    projects: Vec::new(),
                    auth: rho_ui_proto::AuthState {
                        disabled_namespaces: Vec::new(),
                        active_namespace: None,
                        namespaces: Vec::new(),
                    },
                    machine_seed: 0,
                    agent_counter: 100,
                },
                window,
                cx,
            );
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskSnapshot {
                    snapshot: desk_snapshot,
                    replica_id: 42,
                },
                window,
                cx,
            );
            workspace.sync_dashboard(window, cx);
        })
        .expect("update workspace");
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, window, cx| {
            let editor = workspace.dashboard_editor();
            let focus_handle = editor.read(cx).focus_handle(cx);
            window.focus(&focus_handle, cx);
            editor.update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    // End of "body" in "* One\nbody\n".
                    let offset = editor::MultiBufferOffset(10);
                    selections.select_ranges([offset..offset]);
                });
            });
        })
        .expect("focus dashboard");
    cx.update(|cx| cx.refresh_windows());
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "escape");
    cx.simulate_keystrokes(*workspace, "i");
    cx.simulate_keystrokes(*workspace, "enter");
    cx.simulate_keystrokes(*workspace, "x");
    cx.run_until_parked();

    let text = workspace
        .update(cx, |workspace, _, cx| {
            let editor = workspace.dashboard_editor();
            editor.read(cx).buffer().read(cx).snapshot(cx).text()
        })
        .expect("read dashboard");
    assert!(
        text.contains("body\nx"),
        "enter should insert a newline in document text: {text:?}"
    );
}

/// Sending a quick-spawn draft removes its row out from under the
/// cursor; the cursor must land somewhere resolvable (the new heading)
/// before vim's NormalBefore touches the selection, or it panics on a
/// dead excerpt anchor.
#[gpui::test]
fn quick_spawn_send_relocates_the_cursor(cx: &mut TestAppContext) {
    use rho_ui_proto::desk::{DeskOperation, DeskSnapshot};

    let mut source = text::Buffer::new(
        text::ReplicaId::new(8),
        text::BufferId::new(1).unwrap(),
        "",
    );
    let operation = DeskOperation::from_text(&source.edit([(0..0, "* One\nbody\n")]));
    let desk_snapshot = DeskSnapshot {
        text: source.snapshot().text(),
        operations: vec![operation],
        transactions: Vec::new(),
        replicas: Vec::new(),
    };

    let workspace = test_workspace(cx);
    cx.update(bind_test_keymaps);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Ready {
                    agents: Vec::new(),
                    iris_agent: None,
                    projects: vec![rho_ui_proto::UiProject {
                        path: "/tmp/repo".into(),
                        name: "repo".to_owned(),
                        description: String::new(),
                    }],
                    auth: rho_ui_proto::AuthState {
                        disabled_namespaces: Vec::new(),
                        active_namespace: None,
                        namespaces: Vec::new(),
                    },
                    machine_seed: 0,
                    agent_counter: 100,
                },
                window,
                cx,
            );
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskSnapshot {
                    snapshot: desk_snapshot,
                    replica_id: 42,
                },
                window,
                cx,
            );
            workspace.sync_dashboard(window, cx);
            let editor = workspace.dashboard_editor();
            let focus_handle = editor.read(cx).focus_handle(cx);
            window.focus(&focus_handle, cx);
        })
        .expect("set up workspace");
    cx.update(|cx| cx.refresh_windows());
    cx.run_until_parked();

    cx.simulate_keystrokes(*workspace, "escape");
    cx.simulate_keystrokes(*workspace, "shift-r");
    cx.simulate_keystrokes(*workspace, "h i");
    // Enter sends: spawns the agent, writes the placeholder heading, and
    // must not leave the cursor on the removed draft row.
    workspace
        .update(cx, |workspace, _, _| {
            workspace.force_host_online(HostId::default());
        })
        .expect("force online");
    cx.simulate_keystrokes(*workspace, "enter");
    cx.run_until_parked();

    let (text, cursor) = workspace
        .update(cx, |workspace, _, cx| {
            let editor = workspace.dashboard_editor();
            let text = editor.read(cx).buffer().read(cx).snapshot(cx).text();
            let cursor = editor.update(cx, |editor, cx| {
                let snapshot = editor.display_snapshot(cx);
                editor
                    .selections
                    .newest::<editor::MultiBufferOffset>(&snapshot)
                    .head()
            });
            (text, cursor)
        })
        .expect("read dashboard");
    assert!(
        text.contains("* …"),
        "quick spawn should write the placeholder heading: {text:?}"
    );
    // The star token is concealed chrome, so the caret rests past it on
    // the title.
    assert_eq!(
        cursor.0,
        text.find("* …").expect("placeholder present") + 2,
        "cursor should land on the new heading's title: {text:?}"
    );

    // `space u` closes the leader transient and arms a one-shot prefix.
    cx.simulate_keystrokes(*workspace, "escape space u");
    workspace
        .update(cx, |workspace, _, _| {
            assert!(workspace.has_universal_argument_for_test());
            assert!(!workspace.has_transient_for_test());
        })
        .expect("inspect universal argument");

    // Its `R` variant takes the configured path. Change the role, compose,
    // and submit; it should finish through the same quick-spawn placement
    // behavior as bare `R`.
    cx.simulate_keystrokes(*workspace, "shift-r");
    workspace
        .update(cx, |workspace, _, _| {
            assert!(!workspace.has_universal_argument_for_test());
            assert!(workspace.has_new_agent_configuration_for_test());
        })
        .expect("inspect configured quick spawn");
    cx.simulate_keystrokes(*workspace, "r c");
    cx.simulate_keystrokes(*workspace, "configured");
    cx.simulate_keystrokes(*workspace, "enter");
    cx.run_until_parked();
    let text = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .dashboard_editor()
                .read(cx)
                .buffer()
                .read(cx)
                .snapshot(cx)
                .text()
        })
        .expect("read configured quick spawn");
    assert_eq!(
        text.matches("* …").count(),
        2,
        "configured quick spawn should add its own placeholder heading: {text:?}"
    );

    // The lowercase modified command scopes the same options transient to
    // the heading under point instead of creating another heading.
    cx.simulate_keystrokes(*workspace, "escape space u r r c");
    workspace
        .update(cx, |workspace, _, _| {
            assert!(workspace.configured_draft_topic_for_test().is_some());
            assert!(workspace.has_new_agent_configuration_for_test());
        })
        .expect("inspect configured staffing draft");
    cx.simulate_keystrokes(*workspace, "staff here");
    cx.simulate_keystrokes(*workspace, "enter");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert!(!workspace.has_new_agent_configuration_for_test());
            let text = workspace
                .dashboard_editor()
                .read(cx)
                .buffer()
                .read(cx)
                .snapshot(cx)
                .text();
            assert_eq!(
                text.matches("* …").count(),
                2,
                "configured staffing should bind to the existing heading: {text:?}"
            );
        })
        .expect("inspect configured staffing send");

    // Unsupported keys clear the prefix instead of leaving it sticky.
    cx.simulate_keystrokes(*workspace, "escape space u j");
    workspace
        .update(cx, |workspace, _, _| {
            assert!(!workspace.has_universal_argument_for_test());
            assert!(!workspace.has_transient_for_test());
        })
        .expect("inspect cleared universal argument");
}

/// Quick spawn (`shift-r`) writes a `* …` placeholder heading and binds
/// the agent there; once the agent's generated summary lands, the title
/// fills itself in — but a heading the user has renamed is left alone.
#[gpui::test]
fn quick_spawn_placeholder_takes_the_generated_title(cx: &mut TestAppContext) {
    use rho_ui_proto::desk::{DeskOperation, DeskSnapshot};
    use rho_ui_proto::{
        AgentDisposition, AgentRole, AuthState, UiAgentSummary, UiAttention, WorkspaceInfo,
    };

    let summary = |name: Option<&str>| UiAgentSummary {
        agent_id: agent(1),
        parent_agent: None,
        display_name: name.map(str::to_owned),
        created_at: UnixMs(1),
        updated_at: UnixMs(1),
        role: AgentRole::default(),
        workspace: WorkspaceInfo::UserCheckout { repo: "/tmp".into() },
        attention: UiAttention::Quiet,
        last_active: UnixMs(1),
        hidden: false,
        disposition: AgentDisposition::Pending,
        last_user_message_text: String::new(),
        activity: None,
        turn_report: None,
        labels: Vec::new(),
    };
    let ready = |agents: Vec<UiAgentSummary>| ConnEvent::Ready {
        agents,
        iris_agent: None,
        projects: Vec::new(),
        auth: AuthState {
            disabled_namespaces: Vec::new(),
            active_namespace: None,
            namespaces: Vec::new(),
        },
        machine_seed: 0,
        agent_counter: 100,
    };

    let mut source = text::Buffer::new(
        text::ReplicaId::new(8),
        text::BufferId::new(1).unwrap(),
        "",
    );
    let operation = DeskOperation::from_text(&source.edit([(0..0, "* One\nbody\n")]));
    let desk_snapshot = DeskSnapshot {
        text: source.snapshot().text(),
        operations: vec![operation],
        transactions: Vec::new(),
        replicas: Vec::new(),
    };

    let workspace = test_workspace(cx);
    // The agent exists but has no generated summary yet when it spawns.
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), ready(vec![summary(None)]), window, cx);
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskSnapshot {
                    snapshot: desk_snapshot,
                    replica_id: 42,
                },
                window,
                cx,
            );
            let offset = workspace
                .quick_spawn_heading_for_test(HostId::default(), agent(1), cx)
                .expect("desk is present");
            assert_eq!(offset, "* One\nbody\n".len());
            workspace.sync_dashboard(window, cx);
        })
        .expect("update workspace");
    cx.run_until_parked();

    let desk_text = |workspace: &WindowHandle<Workspace>, cx: &mut TestAppContext| {
        workspace
            .update(cx, |workspace, _, cx| {
                let editor = workspace.dashboard_editor();
                editor.read(cx).buffer().read(cx).snapshot(cx).text()
            })
            .expect("read dashboard")
    };
    assert!(
        desk_text(&workspace, cx).contains("* …"),
        "placeholder heading missing: {:?}",
        desk_text(&workspace, cx)
    );

    // The generated summary arrives: the placeholder becomes the title.
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ready(vec![summary(Some("fix the parser"))]),
                window,
                cx,
            );
            workspace.sync_dashboard(window, cx);
        })
        .expect("update workspace");
    cx.run_until_parked();
    let text = desk_text(&workspace, cx);
    assert!(
        text.contains("* fix the parser") && !text.contains('…'),
        "title did not fill in: {text:?}"
    );

    // Another summary refresh must not clobber the now-real title.
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ready(vec![summary(Some("a newer summary"))]),
                window,
                cx,
            );
            workspace.sync_dashboard(window, cx);
        })
        .expect("update workspace");
    cx.run_until_parked();
    assert!(
        desk_text(&workspace, cx).contains("* fix the parser"),
        "settled title was clobbered: {:?}",
        desk_text(&workspace, cx)
    );
}

/// The daemon retags a heading by inserting the tag at the very spot
/// the caret occupies after typing the title. That edit arrives as a
/// CRDT operation and moves the caret by anchor resolution, outside
/// `change_selections` and its caret-rest constraint — so the sync that
/// conceals the new tag must also nudge the caret off the conceal.
#[gpui::test]
fn daemon_retag_keeps_the_caret_at_the_title_end(cx: &mut TestAppContext) {
    use rho_ui_proto::desk::{DeskOperation, DeskSnapshot, DeskTextOpRecord};
    use rho_ui_proto::{
        AgentDisposition, AgentRole, AuthState, UiAgentSummary, UiAttention, WorkspaceInfo,
    };

    let summary = UiAgentSummary {
        agent_id: agent(1),
        parent_agent: None,
        display_name: Some("planner".to_owned()),
        created_at: UnixMs(1),
        updated_at: UnixMs(1),
        role: AgentRole::default(),
        workspace: WorkspaceInfo::UserCheckout { repo: "/tmp".into() },
        attention: UiAttention::Quiet,
        last_active: UnixMs(1),
        hidden: false,
        disposition: AgentDisposition::Pending,
        last_user_message_text: String::new(),
        activity: None,
        turn_report: None,
        labels: Vec::new(),
    };

    let mut source = text::Buffer::new(
        text::ReplicaId::new(8),
        text::BufferId::new(1).unwrap(),
        "",
    );
    let operation = DeskOperation::from_text(&source.edit([(0..0, "* One\nbody\n* Two\n")]));
    let desk_snapshot = DeskSnapshot {
        text: source.snapshot().text(),
        operations: vec![operation],
        transactions: Vec::new(),
        replicas: Vec::new(),
    };

    let workspace = test_workspace(cx);
    cx.update(bind_test_keymaps);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Ready {
                    agents: vec![summary],
                    iris_agent: None,
                    projects: Vec::new(),
                    auth: AuthState {
                        namespaces: Vec::new(),
                        disabled_namespaces: Vec::new(),
                        active_namespace: None,
                    },
                    machine_seed: 0,
                    agent_counter: 100,
                },
                window,
                cx,
            );
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskSnapshot {
                    snapshot: desk_snapshot,
                    replica_id: 42,
                },
                window,
                cx,
            );
            workspace.sync_dashboard(window, cx);
            let focus_handle = workspace.dashboard_editor().read(cx).focus_handle(cx);
            window.focus(&focus_handle, cx);
            workspace.dashboard_editor().update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    let offset = editor::MultiBufferOffset(5);
                    selections.select_ranges([offset..offset]);
                });
            });
        })
        .expect("update workspace");
    cx.update(|cx| cx.refresh_windows());
    cx.run_until_parked();

    let caret = |cx: &mut TestAppContext| {
        workspace
            .update(cx, |workspace, _, cx| {
                workspace.dashboard_editor().update(cx, |editor, cx| {
                    let snapshot = editor.display_snapshot(cx);
                    editor
                        .selections
                        .newest::<editor::MultiBufferOffset>(&snapshot)
                        .head()
                        .0
                })
            })
            .expect("read caret")
    };
    assert_eq!(caret(cx), 5, "caret starts at the title end");

    let tag_edit = format!(" :eng-{}:", agent(1).encoded());
    let operation = DeskOperation::from_text(&source.edit([(5..5, tag_edit.as_str())]));
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskTextApplied(DeskTextOpRecord {
                    sequence: 2,
                    timestamp_ms: 2,
                    operation,
                    transaction: None,
                }),
                window,
                cx,
            );
        })
        .expect("apply retag");
    cx.update(|cx| cx.refresh_windows());
    cx.run_until_parked();

    let display = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .dashboard_editor()
                .update(cx, |editor, cx| editor.display_text(cx))
        })
        .expect("read display text");
    assert!(
        !display.contains(":eng-"),
        "tag should be concealed: {display:?}"
    );
    assert_eq!(
        caret(cx),
        5,
        "the caret must not strand past the concealed tag"
    );

    // Any anchor-resolution path can strand the caret inside the
    // conceal; the next sync heals it.
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.dashboard_editor().update(cx, |editor, cx| {
                let snapshot = editor.display_snapshot(cx);
                editor.selections.change_with(&snapshot, |selections| {
                    let line_end = editor::MultiBufferOffset(5 + tag_edit.len());
                    selections.select_ranges([line_end..line_end]);
                });
            });
            workspace.sync_dashboard(window, cx);
        })
        .expect("strand caret");
    cx.update(|cx| cx.refresh_windows());
    cx.run_until_parked();
    assert_eq!(caret(cx), 5, "sync must nudge a stranded caret off the conceal");
}

/// Cursor motion must never open a fold: the clamp that lifts a fold
/// when the caret lands inside it exists for genuine jumps (`o`,
/// searches), and hjkl travel around a folded heading must not trip it.
#[gpui::test]
fn hjkl_travel_never_opens_a_fold(cx: &mut TestAppContext) {
    use rho_ui_proto::desk::{DeskOperation, DeskSnapshot};
    use rho_ui_proto::{
        AgentDisposition, AgentRole, AuthState, UiAgentSummary, UiAttention, WorkspaceInfo,
    };

    let summary = UiAgentSummary {
        agent_id: agent(1),
        parent_agent: None,
        display_name: Some("planner".to_owned()),
        created_at: UnixMs(1),
        updated_at: UnixMs(1),
        role: AgentRole::default(),
        workspace: WorkspaceInfo::UserCheckout { repo: "/tmp".into() },
        attention: UiAttention::Quiet,
        last_active: UnixMs(1),
        hidden: false,
        disposition: AgentDisposition::Pending,
        last_user_message_text: String::new(),
        activity: None,
        turn_report: None,
        labels: Vec::new(),
    };

    let desk_text = format!(
        "* One :eng-{}:\none body\n** Kid\nkid stuff\n* Two\ntwo tail\n",
        agent(1).encoded()
    );
    let mut source = text::Buffer::new(
        text::ReplicaId::new(8),
        text::BufferId::new(1).unwrap(),
        "",
    );
    let operation = DeskOperation::from_text(&source.edit([(0..0, desk_text.as_str())]));
    let desk_snapshot = DeskSnapshot {
        text: source.snapshot().text(),
        operations: vec![operation],
        transactions: Vec::new(),
        replicas: Vec::new(),
    };

    let workspace = test_workspace(cx);
    cx.update(bind_test_keymaps);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Ready {
                    agents: vec![summary],
                    iris_agent: None,
                    projects: Vec::new(),
                    auth: AuthState {
                        namespaces: Vec::new(),
                        disabled_namespaces: Vec::new(),
                        active_namespace: None,
                    },
                    machine_seed: 0,
                    agent_counter: 100,
                },
                window,
                cx,
            );
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskSnapshot {
                    snapshot: desk_snapshot,
                    replica_id: 42,
                },
                window,
                cx,
            );
            workspace.sync_dashboard(window, cx);
            let focus_handle = workspace.dashboard_editor().read(cx).focus_handle(cx);
            window.focus(&focus_handle, cx);
            workspace.dashboard_editor().update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    let offset = editor::MultiBufferOffset(2);
                    selections.select_ranges([offset..offset]);
                });
            });
        })
        .expect("update workspace");
    cx.update(|cx| cx.refresh_windows());
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "escape tab");
    cx.run_until_parked();

    let folded_body_hidden = |cx: &mut TestAppContext| {
        let display = workspace
            .update(cx, |workspace, _, cx| {
                workspace
                    .dashboard_editor()
                    .update(cx, |editor, cx| editor.display_text(cx))
            })
            .expect("read display text");
        !display.contains("one body") && !display.contains("kid stuff")
    };
    assert!(folded_body_hidden(cx), "tab folds the subtree");

    for step in [
        "j", "k", "$", "l", "l", "h", "j", "j", "k", "k", "0", "$", "j", "$", "k", "e", "e", "b",
        "w", "g g", "shift-g", "k",
    ] {
        cx.simulate_keystrokes(*workspace, step);
        cx.run_until_parked();
        assert!(
            folded_body_hidden(cx),
            "motion {step:?} must not open the fold"
        );
    }
}

/// End-of-line commands stop at the logical line end — in front of the
/// concealed tag and the collapsed subtree — and helix's one-column
/// cursor can never sit on that chrome either.
#[gpui::test]
fn helix_append_on_a_folded_heading_lands_at_the_title(cx: &mut TestAppContext) {
    use rho_ui_proto::desk::{DeskOperation, DeskSnapshot};
    use rho_ui_proto::{
        AgentDisposition, AgentRole, AuthState, UiAgentSummary, UiAttention, WorkspaceInfo,
    };

    let summary = UiAgentSummary {
        agent_id: agent(1),
        parent_agent: None,
        display_name: Some("planner".to_owned()),
        created_at: UnixMs(1),
        updated_at: UnixMs(1),
        role: AgentRole::default(),
        workspace: WorkspaceInfo::UserCheckout { repo: "/tmp".into() },
        attention: UiAttention::Quiet,
        last_active: UnixMs(1),
        hidden: false,
        disposition: AgentDisposition::Pending,
        last_user_message_text: String::new(),
        activity: None,
        turn_report: None,
        labels: Vec::new(),
    };

    let desk_text = format!(
        "* One :eng-{}:\none body\n** Kid\nkid stuff\n* Two\n",
        agent(1).encoded()
    );
    let mut source = text::Buffer::new(
        text::ReplicaId::new(8),
        text::BufferId::new(1).unwrap(),
        "",
    );
    let operation = DeskOperation::from_text(&source.edit([(0..0, desk_text.as_str())]));
    let desk_snapshot = DeskSnapshot {
        text: source.snapshot().text(),
        operations: vec![operation],
        transactions: Vec::new(),
        replicas: Vec::new(),
    };

    let workspace = test_workspace(cx);
    cx.update(bind_test_keymaps);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Ready {
                    agents: vec![summary],
                    iris_agent: None,
                    projects: Vec::new(),
                    auth: AuthState {
                        namespaces: Vec::new(),
                        disabled_namespaces: Vec::new(),
                        active_namespace: None,
                    },
                    machine_seed: 0,
                    agent_counter: 100,
                },
                window,
                cx,
            );
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskSnapshot {
                    snapshot: desk_snapshot,
                    replica_id: 42,
                },
                window,
                cx,
            );
            workspace.sync_dashboard(window, cx);
            let focus_handle = workspace.dashboard_editor().read(cx).focus_handle(cx);
            window.focus(&focus_handle, cx);
            workspace.dashboard_editor().update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    let offset = editor::MultiBufferOffset(2);
                    selections.select_ranges([offset..offset]);
                });
            });
        })
        .expect("update workspace");
    cx.update(|cx| cx.refresh_windows());
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "escape tab");
    cx.run_until_parked();

    let cursor = |cx: &mut TestAppContext| {
        workspace
            .update(cx, |workspace, _, cx| {
                workspace.dashboard_editor().update(cx, |editor, cx| {
                    let snapshot = editor.display_snapshot(cx);
                    let selection = editor
                        .selections
                        .newest::<editor::MultiBufferOffset>(&snapshot);
                    (selection.start.0, selection.end.0)
                })
            })
            .expect("read cursor")
    };

    // `$` rests the cursor on the title's last character. `l` steps to
    // the logical line end — the append position, rendered as a
    // line-end block — and no further: the concealed tag, the chip,
    // and the chevron are not cursor positions.
    cx.simulate_keystrokes(*workspace, "$");
    cx.run_until_parked();
    assert_eq!(cursor(cx), (4, 4), "$ rests on the title's last character");
    cx.simulate_keystrokes(*workspace, "l");
    cx.run_until_parked();
    assert_eq!(cursor(cx), (5, 5), "l stops at the logical line end");
    cx.simulate_keystrokes(*workspace, "l");
    cx.run_until_parked();
    assert_eq!(cursor(cx), (5, 5), "the cursor never enters line chrome");

    // Append lands ahead of the tag, and the subtree stays folded.
    cx.simulate_keystrokes(*workspace, "shift-a");
    cx.simulate_keystrokes(*workspace, "space m o r e");
    cx.run_until_parked();
    let text = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .dashboard_editor()
                .read(cx)
                .buffer()
                .read(cx)
                .snapshot(cx)
                .text()
        })
        .expect("read dashboard");
    assert!(
        text.starts_with("* One more :eng-"),
        "append must stay ahead of the tag: {text:?}"
    );
    let display = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .dashboard_editor()
                .update(cx, |editor, cx| editor.display_text(cx))
        })
        .expect("read display text");
    assert!(
        !display.contains("one body") && !display.contains("kid stuff"),
        "the subtree stays folded through the append: {display:?}"
    );
}
