//! End-to-end tests: synthetic protocol frames in, rendered editor state out.

use std::sync::Arc;

use editor::display_map::{Block, DisplayPoint, DisplayRow};
use editor::{Copy, Editor, MoveRight, SelectionEffects};
use gpui::{
    App, AppContext as _, Entity, Focusable as _, InputEvent as _, Modifiers, MouseButton,
    MouseDownEvent, MouseUpEvent, TestAppContext, TouchEvent, TouchId, TouchPhase, WindowHandle,
    point, px, size,
};
use rho_agents::state::{
    UiAgentState, UiAgentStatus, UiBlock, UiMessagePhase, UiTool, UiToolStatus,
};
use rho_agents::transcript::elisions::{ElisionSpec, ElisionState, ElisionSync};
use rho_core::UnixMs;
use rho_hosts::connection::ConnEvent;
use rho_ui_proto::AgentId;
use settings::{Settings, SettingsStore};
use story::ready_with;

mod dashboard_cost;
mod editor_shutdown;
mod elision_block_geometry;
mod elision_tail_rebuild;
mod elision_unfold;
mod find_cost;
mod fold_accounting_streaming;
mod fold_cost;
mod fold_tail_rows;
mod fold_widen_bias;
mod fold_widen_underflow;
mod fold_widening_check;
mod history;
mod inlay_cost;
mod minibuffer;
mod record_anchors;
mod removing_a_turn_after_growth;
mod running_turn_elapsed;
mod scene_walk;
mod story;
mod syntax_parsed_in_frame;
mod tool_output_not_drawn;
mod turn_is_one_buffer;
mod wrap_rows;
mod wrap_under_tab;
use rho_agents::HostId;

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

#[gpui::test]
fn image_inlays_are_fixed_cell_decorations(cx: &mut TestAppContext) {
    cx.update(init_test_app);
    let editor = cx.add_window(|window, cx| {
        let mut editor = Editor::multi_line(window, cx);
        editor.set_text("ab", window, cx);
        window.focus(&editor.focus_handle(cx), cx);
        editor
    });
    let replacement = std::sync::Arc::new(gpui::RenderImage::new(smallvec::SmallVec::new()));

    let (id, original_anchor) = editor
        .update(cx, |editor, window, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let anchor = snapshot.anchor_after(editor::MultiBufferOffset(1));
            let image = std::sync::Arc::new(gpui::RenderImage::new(smallvec::SmallVec::new()));
            let id = editor
                .add_image_inlay(anchor, image, 129, cx)
                .expect("positive-width image inlay");

            // 129 cells crosses Rope's 128-byte dependency chunk boundary, but
            // remains one logical and rendered image inlay.
            assert_eq!(editor.display_snapshot(cx).line_len(DisplayRow(0)), 131);
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections
                    .select_ranges([editor::MultiBufferOffset(1)..editor::MultiBufferOffset(1)]);
            });
            editor.move_right(&MoveRight, window, cx);
            let display = editor.display_snapshot(cx);
            assert_eq!(
                editor.selections.newest_display(&display).head(),
                DisplayPoint::new(DisplayRow(0), 131)
            );
            (id, anchor)
        })
        .expect("add image inlay");

    cx.update_window(*editor, |_, window, cx| window.simulate_next_frame(cx))
        .expect("render image inlay");
    cx.run_until_parked();
    editor
        .update(cx, |editor, window, cx| {
            assert_eq!(editor.image_renderer_element_count(id), 1);
            assert!(editor.replace_image_inlay(id, replacement, cx));
            let replaced_anchor = editor
                .all_inlays(cx)
                .into_iter()
                .find(|inlay| inlay.id == id)
                .expect("replaced image inlay")
                .position;
            assert_eq!(replaced_anchor, original_anchor);

            // This width falls inside the image. The inlay must move as one
            // wide glyph rather than split across soft-wrapped rows.
            editor.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
            window.refresh();
        })
        .expect("replace and wrap image inlay");
    cx.simulate_window_resize(*editor, gpui::size(gpui::px(100.), gpui::px(200.)));
    cx.run_until_parked();
    cx.update_window(*editor, |_, window, cx| window.simulate_next_frame(cx))
        .expect("render wrapped image inlay");
    cx.run_until_parked();

    editor
        .update(cx, |editor, _, cx| {
            let display = editor.display_snapshot(cx);
            let line_lengths = (0..=display.max_point().row().0)
                .map(|row| display.line_len(DisplayRow(row)))
                .collect::<Vec<_>>();
            assert!(line_lengths.contains(&129), "{line_lengths:?}");
            assert!(
                line_lengths
                    .iter()
                    .all(|len| *len == 0 || *len == 1 || *len == 129)
            );
            assert_eq!(editor.image_renderer_element_count(id), 1);
        })
        .expect("inspect wrapped image inlay");

    cx.dispatch_action(*editor, Copy);
    assert_eq!(
        cx.read_from_clipboard().and_then(|item| item.text()),
        Some("ab\n".into())
    );
    editor
        .update(cx, |editor, _, cx| {
            assert!(editor.remove_image_inlay(id, cx));
            assert_eq!(editor.display_snapshot(cx).line_len(DisplayRow(0)), 2);
        })
        .expect("remove image inlay");
}

/// A spec whose anchors do not resolve is not a fold, so it must not take
/// a fold's crease id. The ids come back for the resolved specs only, and
/// pairing them against every spec by position hands each spec after an
/// unresolved one its neighbour's crease and drops the last one — after
/// which the editor's record of which crease belongs to which turn is wrong
/// and the next reconcile unfolds turns that should have stayed folded. A
/// transcript still composing its history hands this path unresolved specs
/// as a matter of course, so it is not a corner.
#[gpui::test]
fn an_elision_that_cannot_resolve_takes_no_crease(cx: &mut TestAppContext) {
    cx.update(init_test_app);
    let text = (0..9)
        .map(|row| format!("line {row}"))
        .collect::<Vec<_>>()
        .join("\n");
    let buffer = cx.update(|cx| cx.new(|cx| language::Buffer::local(text, cx)));
    // Never an excerpt of the multibuffer, so its anchors resolve to
    // nothing — the same answer an excerpt that has not been composed yet
    // gives.
    let elsewhere = cx.update(|cx| cx.new(|cx| language::Buffer::local("a\nb\nc", cx)));
    let multi_buffer = cx.update(|cx| {
        cx.new(|cx| {
            let mut multi_buffer =
                multi_buffer::MultiBuffer::without_headers(language::Capability::ReadWrite);
            multi_buffer.set_excerpts_for_path(
                multi_buffer::PathKey::sorted(0),
                buffer.clone(),
                [language::Point::zero()..buffer.read(cx).max_point()],
                0,
                cx,
            );
            multi_buffer
        })
    });
    let window = cx.add_window(|window, cx| {
        Editor::new(
            editor::EditorMode::Full {
                scale_ui_elements_with_buffer_font_size: true,
                show_active_line_background: false,
                sizing_behavior: editor::SizingBehavior::ExcludeOverscrollMargin,
            },
            multi_buffer.clone(),
            None,
            window,
            cx,
        )
    });
    let editor = window.root(cx).expect("editor");

    let spec = |anchors: (text::Anchor, text::Anchor), tool_count: usize| ElisionSpec {
        range: anchors.0..anchors.1,
        tool_count,
        tail_rows: 0,
    };
    let (first, unresolvable, last) = cx.update(|cx| {
        let buffer = buffer.read(cx);
        let elsewhere = elsewhere.read(cx);
        (
            spec((buffer.anchor_before(7), buffer.anchor_after(21)), 2),
            spec((elsewhere.anchor_before(0), elsewhere.anchor_after(3)), 3),
            spec((buffer.anchor_before(35), buffer.anchor_after(49)), 4),
        )
    });

    let host = cx.update(|cx| cx.new(|_| ()));
    let mut sync = ElisionSync::default();
    let mut state = ElisionState::default();
    sync.set_specs(vec![first.clone(), unresolvable.clone(), last.clone()]);
    cx.update(|cx| {
        host.update(cx, |_, cx| {
            sync.apply(&mut state, &multi_buffer, &editor, cx)
        })
    });
    assert_eq!(
        state.active_specs().cloned().collect::<Vec<_>>(),
        vec![first.clone(), last.clone()],
        "the two folds that happened are the two the editor is carrying"
    );

    // The first turn changes, so its fold is the only stale one; the fold
    // that never moved keeps the crease it was given.
    sync.set_specs(vec![unresolvable, last.clone()]);
    cx.update(|cx| {
        host.update(cx, |_, cx| {
            sync.apply(&mut state, &multi_buffer, &editor, cx)
        })
    });
    assert_eq!(
        state.active_specs().cloned().collect::<Vec<_>>(),
        vec![last],
        "only the stale fold left"
    );
}

/// A fold placeholder stands in for buffer text, so it draws in the
/// buffer's face. The editor's prepaint pushes the buffer's font size and
/// line height onto the text style stack but not its family, so a
/// placeholder that does not name the family draws in the window's UI font
/// — a proportional caption in the middle of monospace rows, which is what
/// the transcript's "N tools" rows did.
#[gpui::test]
fn a_fold_placeholder_wears_the_buffer_s_face(cx: &mut TestAppContext) {
    cx.update(init_test_app);
    cx.update(|cx| {
        let buffer_font = theme_settings::ThemeSettings::get_global(cx)
            .buffer_font
            .clone();
        let mut row = rho_agents::transcript::elisions::elision_row("2 tools", cx);
        let style = gpui::Styled::text_style(&mut row);
        assert_eq!(style.font_family, Some(buffer_font.family));
    });
}

pub(super) fn init_test_app(cx: &mut App) {
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

#[gpui::test]
fn shared_modal_init_preserves_bundled_helix_default(cx: &mut TestAppContext) {
    cx.update(|cx| {
        assets::Assets.load_test_fonts(cx);
        let store = SettingsStore::new(cx, crate::rho_assets::RHO_DEFAULT_SETTINGS);
        cx.set_global(store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);

        crate::init_vim_mode(cx).expect("initialize Vim mode");

        assert!(!vim_mode_setting::VimModeSetting::get_global(cx).0);
        assert!(vim_mode_setting::HelixModeSetting::get_global(cx).0);
    });
}

#[gpui::test]
fn phone_entry_disables_modal_editing_app_wide(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    cx.update(|cx| {
        assert!(
            vim_mode_setting::HelixModeSetting::get_global(cx).0,
            "desktop default is Helix on"
        );
    });

    cx.simulate_window_resize(*workspace, gpui::size(gpui::px(400.), gpui::px(800.)));
    cx.update_window(*workspace, |_, window, cx| {
        window.simulate_next_frame(cx);
    })
    .expect("draw phone frame");
    cx.run_until_parked();

    cx.update(|cx| {
        assert!(
            !vim_mode_setting::HelixModeSetting::get_global(cx).0,
            "entering phone mode must strip Helix app-wide"
        );
        assert!(!vim_mode_setting::VimModeSetting::get_global(cx).0);
    });
}

#[gpui::test]
fn phone_entry_opens_the_feed_and_one_finger_flicks_to_the_next_card(cx: &mut TestAppContext) {
    // Two notes that want attention: what the feed deals are nodes.
    let mut desk = DeskFixture::new();
    desk.due_note(None, "First phone card");
    desk.due_note(None, "Second phone card");

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    cx.simulate_window_resize(*workspace, gpui::size(gpui::px(400.), gpui::px(800.)));
    cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    cx.run_until_parked();
    let first = workspace
        .update(cx, |workspace, _, cx| {
            assert!(workspace.phone_feed_for_test(cx));
            workspace.current_deal_card_for_test(cx).unwrap().0
        })
        .unwrap();

    cx.update_window(*workspace, |_, window, cx| {
        for event in [
            TouchEvent {
                id: TouchId(1),
                phase: TouchPhase::Started,
                position: point(px(200.), px(600.)),
                timestamp: std::time::Duration::ZERO,
                ..Default::default()
            },
            TouchEvent {
                id: TouchId(1),
                phase: TouchPhase::Moved,
                position: point(px(200.), px(300.)),
                timestamp: std::time::Duration::from_millis(80),
                ..Default::default()
            },
        ] {
            window.dispatch_event(event.to_platform_input(), cx);
        }
    })
    .unwrap();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.phone_motion_for_test(), (-300., None));
        })
        .unwrap();
    cx.update_window(*workspace, |_, window, cx| {
        window.dispatch_event(
            TouchEvent {
                id: TouchId(1),
                phase: TouchPhase::Ended,
                position: point(px(200.), px(300.)),
                timestamp: std::time::Duration::from_millis(100),
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
    })
    .unwrap();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(
                workspace.phone_motion_for_test(),
                (0., Some((-300., -800.)))
            );
        })
        .unwrap();
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(200));
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, cx| {
            assert!(workspace.phone_feed_for_test(cx));
            assert_ne!(workspace.current_deal_card_for_test(cx).unwrap().0, first);
            assert_eq!(
                workspace.phone_last_gesture_for_test(),
                Some("flick up · moved")
            );
        })
        .unwrap();
}

#[gpui::test]
fn the_phone_feed_opens_when_the_first_card_arrives_after_it_did(cx: &mut TestAppContext) {
    // A Slack thread becomes a node only once the mirror has synced, so the
    // phone's first draw can find an empty queue. The feed is the deal: it
    // has to open itself when the card lands.
    let workspace = test_workspace(cx);
    cx.simulate_window_resize(*workspace, size(px(400.), px(800.)));
    cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert!(workspace.current_deal_card_for_test(cx).is_none());
        })
        .unwrap();

    let mut desk = DeskFixture::new();
    desk.due_note(None, "Arrived after the feed");
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert!(workspace.current_deal_card_for_test(cx).is_some());
        })
        .unwrap();
}

#[gpui::test]
fn leaving_phone_mode_cancels_a_delayed_flick_commit(cx: &mut TestAppContext) {
    // Two notes that want attention: what the feed deals are nodes.
    let mut desk = DeskFixture::new();
    desk.due_note(None, "First resize card");
    desk.due_note(None, "Second resize card");

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_window_resize(*workspace, size(px(400.), px(800.)));
    cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    cx.run_until_parked();
    let first = workspace
        .update(cx, |workspace, window, cx| {
            let identity = workspace.current_deal_card_for_test(cx).unwrap().0;
            workspace.phone_start_snap_for_test(window, cx);
            identity
        })
        .unwrap();

    cx.simulate_window_resize(*workspace, size(px(800.), px(800.)));
    cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(200));
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(workspace.current_deal_card_for_test(cx).unwrap().0, first);
            assert_eq!(workspace.phone_last_gesture_for_test(), None);
        })
        .unwrap();
}

#[gpui::test]
fn cancelling_phone_file_keeps_the_current_feed_card(cx: &mut TestAppContext) {
    // The feed deals nodes: one note that wants attention.
    let mut desk = DeskFixture::new();
    let node_id = desk.due_note(None, "Keep this phone card");

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    let id = crate::dashboard::DealCardId {
        host: HostId::default(),
        node_id,
    };
    cx.simulate_window_resize(*workspace, gpui::size(gpui::px(400.), gpui::px(800.)));
    cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    cx.run_until_parked();

    cx.dispatch_action(*workspace, crate::DashboardDealFile);
    cx.dispatch_action(*workspace, crate::MinibufferCancel);
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, cx| {
            assert!(workspace.phone_feed_for_test(cx));
            assert_eq!(workspace.current_deal_card_for_test(cx).unwrap().0, id);
        })
        .unwrap();
}

#[gpui::test]
fn phone_back_from_a_surface_reveals_the_hidden_feed_card(cx: &mut TestAppContext) {
    // The feed deals nodes: one note that wants attention.
    let mut desk = DeskFixture::new();
    let node_id = desk.due_note(None, "Feed stays put");

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    let expected = crate::dashboard::DealCardId {
        host: HostId::default(),
        node_id,
    };
    cx.simulate_window_resize(*workspace, gpui::size(gpui::px(400.), gpui::px(800.)));
    cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.enter_draft(None, window, cx);
            workspace.phone_back_for_test(window, cx);
            assert!(workspace.phone_feed_for_test(cx));
            assert!(workspace.phone_feed_is_active_for_test());
            assert_eq!(
                workspace.current_deal_card_for_test(cx).unwrap().0,
                expected
            );
        })
        .unwrap();
}

#[gpui::test]
fn phone_empty_feed_flick_down_undoes_the_last_verdict(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    // The feed deals nodes: one note that wants attention.
    let mut desk = DeskFixture::new();
    let node_id = desk.due_note(None, "Last phone card");

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    let expected = crate::dashboard::DealCardId {
        host: HostId::default(),
        node_id,
    };
    cx.simulate_window_resize(*workspace, gpui::size(gpui::px(400.), gpui::px(800.)));
    cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    cx.run_until_parked();
    cx.dispatch_action(*workspace, crate::DashboardDealDone);
    cx.run_until_parked();
    // A tree verdict lands when the daemon accepts it.
    workspace
        .update(cx, |workspace, window, cx| {
            let stamp = take_desk_mutation(workspace, HostId::default())
                .expect("verdict mutation")
                .stamp;
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::DeskMutationAccepted { stamp },
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(workspace.current_deal_card_for_test(cx), None);
            workspace.phone_remember_last_verdict_for_test();
        })
        .unwrap();

    cx.update_window(*workspace, |_, window, cx| {
        for event in [
            TouchEvent {
                id: TouchId(1),
                phase: TouchPhase::Started,
                position: point(px(200.), px(250.)),
                timestamp: std::time::Duration::ZERO,
                ..Default::default()
            },
            TouchEvent {
                id: TouchId(1),
                phase: TouchPhase::Moved,
                position: point(px(200.), px(550.)),
                timestamp: std::time::Duration::from_millis(80),
                ..Default::default()
            },
            TouchEvent {
                id: TouchId(1),
                phase: TouchPhase::Ended,
                position: point(px(200.), px(550.)),
                timestamp: std::time::Duration::from_millis(100),
                ..Default::default()
            },
        ] {
            window.dispatch_event(event.to_platform_input(), cx);
        }
    })
    .unwrap();
    cx.run_until_parked();
    // The undo is a mutation like any other: the card comes back when the
    // daemon has taken it.
    workspace
        .update(cx, |workspace, window, cx| {
            let stamp = take_desk_mutation(workspace, HostId::default())
                .expect("undo mutation")
                .stamp;
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::DeskMutationAccepted { stamp },
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(
                workspace.current_deal_card_for_test(cx).unwrap().0,
                expected
            );
            assert!(workspace.phone_feed_is_active_for_test());
        })
        .unwrap();
}

#[gpui::test]
fn deleting_the_top_row_leaves_the_cursor_on_a_live_row(cx: &mut TestAppContext) {
    // Nothing sits above the first root, and its child follows it out of the
    // tree, so the cursor has to fall to the next root. Landing on neither
    // used to swallow the next structure keypress.
    let mut desk = DeskFixture::new();
    let first = desk.note(None, "First root");
    desk.note(Some(first.clone()), "Its child");
    let second = desk.note(None, "Second root");

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.take_host_messages_for_test(HostId::default());
            assert_eq!(
                workspace
                    .desk_cells
                    .row_after_delete(HostId::default(), &first),
                Some(second)
            );
        })
        .unwrap();
}

#[gpui::test]
fn phone_blocks_navigation_while_a_tree_verdict_is_pending(cx: &mut TestAppContext) {
    // A note woken long ago, so the dealer offers it as a card.
    let mut desk = DeskFixture::new();
    let note = desk.note(None, "Pending phone verdict");
    desk.set(
        note.clone(),
        rho_desk::cells::Property::DeferUntil(Some(rho_desk::cells::Timestamp {
            unix_ms: 1_577_836_800_000,
            precision: rho_desk::cells::TimestampPrecision::Day,
        })),
    );
    desk.set(note, rho_desk::cells::Property::PaceDays(1));

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();
    cx.simulate_window_resize(*workspace, size(px(400.), px(800.)));
    cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    cx.run_until_parked();

    let identity = workspace
        .update(cx, |workspace, _, cx| {
            workspace.current_deal_card_for_test(cx).unwrap().0
        })
        .unwrap();
    cx.dispatch_action(*workspace, crate::DashboardDealDone);
    cx.dispatch_action(*workspace, crate::UndoVerdict);
    cx.run_until_parked();
    let verdict_stamp = workspace
        .update(cx, |workspace, _, _| {
            take_desk_mutation(workspace, HostId::default())
                .expect("tree verdict mutation")
                .stamp
        })
        .unwrap();

    // Neither another verdict, undo, nor an upward flick may move or mutate
    // the card until the first verdict is acknowledged.
    cx.dispatch_action(*workspace, crate::DashboardDealDone);
    cx.update_window(*workspace, |_, window, cx| {
        for event in [
            TouchEvent {
                id: TouchId(1),
                phase: TouchPhase::Started,
                position: point(px(200.), px(600.)),
                timestamp: std::time::Duration::ZERO,
                ..Default::default()
            },
            TouchEvent {
                id: TouchId(1),
                phase: TouchPhase::Moved,
                position: point(px(200.), px(300.)),
                timestamp: std::time::Duration::from_millis(80),
                ..Default::default()
            },
            TouchEvent {
                id: TouchId(1),
                phase: TouchPhase::Ended,
                position: point(px(200.), px(300.)),
                timestamp: std::time::Duration::from_millis(100),
                ..Default::default()
            },
        ] {
            window.dispatch_event(event.to_platform_input(), cx);
        }
    })
    .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(
                workspace.current_deal_card_for_test(cx).unwrap().0,
                identity
            );
            assert!(
                workspace
                    .take_host_messages_for_test(HostId::default())
                    .into_iter()
                    .all(|message| !matches!(
                        message,
                        rho_ui_proto::ClientMessage::DeskMutationApply { .. }
                    ))
            );
        })
        .unwrap();

    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::DeskMutationAccepted {
                    stamp: verdict_stamp,
                },
                window,
                cx,
            );
        })
        .unwrap();

    cx.update_window(*workspace, |_, window, cx| {
        for event in [
            TouchEvent {
                id: TouchId(2),
                phase: TouchPhase::Started,
                position: point(px(200.), px(250.)),
                timestamp: std::time::Duration::ZERO,
                ..Default::default()
            },
            TouchEvent {
                id: TouchId(2),
                phase: TouchPhase::Moved,
                position: point(px(200.), px(550.)),
                timestamp: std::time::Duration::from_millis(80),
                ..Default::default()
            },
            TouchEvent {
                id: TouchId(2),
                phase: TouchPhase::Ended,
                position: point(px(200.), px(550.)),
                timestamp: std::time::Duration::from_millis(100),
                ..Default::default()
            },
        ] {
            window.dispatch_event(event.to_platform_input(), cx);
        }
    })
    .unwrap();
    cx.run_until_parked();
    let undo_stamp = workspace
        .update(cx, |workspace, _, _| {
            take_desk_mutation(workspace, HostId::default())
                .expect("tree verdict undo mutation")
                .stamp
        })
        .unwrap();
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::DeskMutationAccepted { stamp: undo_stamp },
                window,
                cx,
            );
            assert_eq!(
                workspace.current_deal_card_for_test(cx).unwrap().0,
                identity
            );
        })
        .unwrap();
}

#[gpui::test]
fn touch_editing_strips_vim_from_live_editors(cx: &mut TestAppContext) {
    cx.update(|cx| {
        assets::Assets.load_test_fonts(cx);
        let store = SettingsStore::new(cx, crate::rho_assets::RHO_DEFAULT_SETTINGS);
        cx.set_global(store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        editor::init(cx);
        crate::init_vim_mode(cx).expect("initialize Vim mode");
    });
    // Only full-mode editors opt into modal editing; single-line inputs
    // never had it.
    let editor = cx.add_window(|window, cx| {
        let editor = Editor::multi_line(window, cx);
        window.focus(&editor.focus_handle(cx), cx);
        editor
    });
    let text = |cx: &mut TestAppContext| {
        editor
            .update(cx, |editor, _, cx| editor.text(cx))
            .expect("read editor text")
    };

    // Helix normal mode consumes these as complete commands, not text.
    cx.simulate_keystrokes(*editor, "w b x");
    assert_eq!(text(cx), "", "modal editing should start active");

    cx.update(|cx| crate::workspace::set_touch_modal_editing(false, cx));
    cx.run_until_parked();
    cx.simulate_keystrokes(*editor, "x y z");
    assert_eq!(text(cx), "xyz", "touch editors must accept text directly");
}

pub(super) fn bind_test_keymaps(cx: &mut App) {
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

/// `45sm` is 45 minutes and `2sd` two days: the unit picks the span, the
/// count multiplies it, and the words the bar says name the time it lands on.
#[test]
fn a_snooze_lands_on_its_unit_and_says_the_time() {
    use chrono::TimeZone as _;

    use crate::workspace::{SnoozeUnit, snooze_target};

    let now = chrono::Local
        .with_ymd_and_hms(2026, 9, 3, 14, 30, 0)
        .earliest()
        .expect("a local afternoon");
    let (at, said) = snooze_target(SnoozeUnit::Minutes, 45, now);
    assert_eq!(
        at.unix_ms,
        (now + chrono::Duration::minutes(45)).timestamp_millis()
    );
    assert_eq!(
        at.precision,
        rho_desk::cells::TimestampPrecision::Millisecond
    );
    assert_eq!(said, "snooze until 15:15");

    // Three hours from half past two crosses no day, so the hour is enough.
    let (_, said) = snooze_target(SnoozeUnit::Hours, 3, now);
    assert_eq!(said, "snooze until 17:30");
    // Twelve does cross it, and then the bar names the day as well.
    let (_, said) = snooze_target(SnoozeUnit::Hours, 12, now);
    assert_eq!(said, "snooze until Fri 4 Sep 02:30");

    // Days and weeks land on a date, as a defer always has.
    let (at, said) = snooze_target(SnoozeUnit::Days, 2, now);
    assert_eq!(at.precision, rho_desk::cells::TimestampPrecision::Day);
    assert_eq!(said, "snooze until Sat 5 Sep");
    let (_, said) = snooze_target(SnoozeUnit::Weeks, 1, now);
    assert_eq!(said, "snooze until Thu 10 Sep");
}

/// Deal mode took `d`, `x`, `s`, `t` and `f` from every card, so a card
/// could not be read like the buffer it is. The verdicts moved into the
/// transient `tab` opens, and the letters belong to vim again on
/// every surface. `shift-u` is the exception: undoing a verdict is the same
/// verb wherever the card is.
#[gpui::test]
fn the_verdict_letters_belong_to_vim_on_a_card(cx: &mut TestAppContext) {
    use gpui::{KeyContext, Keystroke};

    cx.update(bind_test_keymaps);
    cx.update(|cx| {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let editor = [
            KeyContext::parse("RhoGui").unwrap(),
            KeyContext::parse("Editor vim_mode=normal vim_operator=none").unwrap(),
        ];
        let verdicts: [&dyn gpui::Action; 4] = [
            &crate::DashboardDealDone,
            &crate::DashboardDealMute,
            &crate::DashboardDealTodo,
            &crate::DashboardDealFile,
        ];
        for key in ["d", "x", "s", "t", "f"] {
            let (bindings, _) =
                keymap.bindings_for_input(&[Keystroke::parse(key).unwrap()], &editor);
            assert!(
                !bindings.iter().any(|binding| verdicts
                    .iter()
                    .any(|verdict| binding.action().partial_eq(*verdict))),
                "{key} still makes a verdict on a card: {bindings:?}"
            );
        }
        let (bindings, _) =
            keymap.bindings_for_input(&[Keystroke::parse("shift-u").unwrap()], &editor);
        assert!(
            bindings
                .first()
                .is_some_and(|binding| binding.action().partial_eq(&crate::UndoVerdict)),
            "shift-u still undoes the last verdict: {bindings:?}"
        );
    });
}

#[gpui::test]
fn undo_verdict_binding_is_confined_to_normal_mode(cx: &mut TestAppContext) {
    use gpui::{KeyContext, Keystroke};

    cx.update(bind_test_keymaps);
    cx.update(|cx| {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let stroke = Keystroke::parse("shift-u").unwrap();
        let resolves = |contexts: &[KeyContext]| {
            keymap
                .bindings_for_input(std::slice::from_ref(&stroke), contexts)
                .0
                .first()
                .is_some_and(|binding| binding.action().partial_eq(&crate::UndoVerdict))
        };
        assert!(resolves(&[
            KeyContext::parse("RhoGui").unwrap(),
            KeyContext::parse("Editor vim_mode=normal vim_operator=none").unwrap(),
        ]));
        assert!(resolves(&[
            KeyContext::parse("RhoGui").unwrap(),
            KeyContext::parse("Editor vim_mode=helix_normal vim_operator=none").unwrap(),
        ]));
        assert!(!resolves(&[
            KeyContext::parse("RhoDashboard").unwrap(),
            KeyContext::parse("Editor vim_mode=insert vim_operator=none").unwrap(),
        ]));
        assert!(!resolves(&[
            KeyContext::parse("RhoGui").unwrap(),
            KeyContext::parse("Editor vim_mode=normal vim_operator=delete").unwrap(),
        ]));
    });
}

#[gpui::test]
fn a_todo_verdict_logs_every_cell_that_makes_the_new_note_a_cadence(cx: &mut TestAppContext) {
    // The daemon validates the log entry against exactly these three
    // changes, and rejects the whole mutation otherwise: a todo that only
    // logged the new note's arrival never reached the tree.
    use rho_desk::cells::{Property, PropertyKey};

    let mut desk = DeskFixture::new();
    let note = desk.note(None, "Named card");
    let woke = rho_desk::cells::Timestamp {
        unix_ms: 1_577_836_800_000,
        precision: rho_desk::cells::TimestampPrecision::Day,
    };
    desk.set(note.clone(), Property::DeferUntil(Some(woke)));
    desk.set(note.clone(), Property::PaceDays(1));

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.pull_card(window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();

    cx.dispatch_action(*workspace, crate::DashboardDealTodo);
    cx.run_until_parked();

    let (created, stamp) = workspace
        .update(cx, |workspace, _, _| {
            let mutation = take_desk_mutation(workspace, HostId::default()).expect("todo mutation");
            let Some((
                verdict_node,
                rho_desk::cells::VerdictEvent::Applied {
                    verdict, changes, ..
                },
            )) = mutation.verdict.clone()
            else {
                panic!("the todo verdict did not log an applied entry");
            };
            assert_eq!(verdict_node, note, "the entry hangs off the dealt heading");
            let rho_desk::cells::Verdict::Todo { note: created } = verdict else {
                panic!("the entry is not a todo");
            };
            assert_eq!(changes.len(), 4);
            assert!(changes.iter().filter(|change| change.id == created).count() == 3);
            let change = |key: PropertyKey| {
                changes
                    .iter()
                    .find(|change| change.key == key)
                    .unwrap_or_else(|| panic!("no change for {key:?}"))
                    .clone()
            };
            let deleted = change(PropertyKey::Deleted);
            assert_eq!(deleted.before, Some(Property::Deleted(true)));
            assert_eq!(deleted.after, Some(Property::Deleted(false)));
            let defer = change(PropertyKey::DeferUntil);
            assert_eq!(defer.before, Some(Property::DeferUntil(None)));
            assert!(matches!(defer.after, Some(Property::DeferUntil(Some(_)))));
            let pace = change(PropertyKey::PaceDays);
            assert_eq!(pace.before, Some(Property::PaceDays(0)));
            assert!(matches!(pace.after, Some(Property::PaceDays(_))));
            // The dealt node is handled by the todo: without this the dealer
            // offers the same card again the moment the note exists.
            let state = change(PropertyKey::State);
            assert_eq!(state.id, note);
            assert_eq!(
                state.after,
                Some(Property::State(rho_desk::cells::State::Done))
            );
            assert!(mutation.writes.iter().any(|write| write.id == note
                && write.property == Property::State(rho_desk::cells::State::Done)));
            // The daemon also requires the note to be parented on the heading.
            assert!(mutation.writes.iter().any(|write| write.id == created
                && write.property == Property::Parent(Some(note.clone()))));
            (created, mutation.stamp)
        })
        .unwrap();

    // A note with no words of its own comes back in a week saying only
    // `defer …`; it carries the words of the card it was written on.
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::DeskMutationAccepted { stamp },
                window,
                cx,
            );
            let buffer = workspace
                .desk_cells
                .buffer(HostId::default(), &created)
                .expect("the todo note has a buffer")
                .clone();
            assert_eq!(buffer.read(cx).text(), "Named card");
        })
        .unwrap();
}

#[gpui::test]
fn undo_verdict_reaches_the_desk_tree_outside_a_deal(cx: &mut TestAppContext) {
    use gpui::{KeyContext, Keystroke};

    cx.update(bind_test_keymaps);
    cx.update(|cx| {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let stroke = Keystroke::parse("shift-u").unwrap();
        let resolves = |contexts: &[KeyContext]| {
            keymap
                .bindings_for_input(std::slice::from_ref(&stroke), contexts)
                .0
                .first()
                .is_some_and(|binding| binding.action().partial_eq(&crate::UndoVerdict))
        };
        // The desk itself, with no card on screen: vim binds `shift-u` one
        // level up, so without this the verb is lost on the tree.
        for mode in ["normal", "helix_normal"] {
            assert!(
                resolves(&[
                    KeyContext::parse("RhoGui").unwrap(),
                    KeyContext::parse("RhoDashboard").unwrap(),
                    KeyContext::parse(&format!(
                        "Editor VimControl vim_mode={mode} vim_operator=none"
                    ))
                    .unwrap(),
                ]),
                "shift-u did not reach UndoVerdict on the tree in {mode}"
            );
        }
        // Typing is still typing.
        assert!(!resolves(&[
            KeyContext::parse("RhoGui").unwrap(),
            KeyContext::parse("RhoDashboard").unwrap(),
            KeyContext::parse("Editor VimControl vim_mode=insert vim_operator=none").unwrap(),
        ]));
    });
}

/// Written when the map's `DashboardNewSibling` was the verb and the
/// question was whether it could work with no row to stand on. The map is
/// gone and `n n` is the route, but the subject is the same one: a reader
/// with an empty desk can still write its first note, and it lands at the
/// root rather than nowhere.
#[gpui::test]
fn the_first_heading_can_be_written_on_an_empty_desk(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let desk = DeskFixture::new();
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();
    cx.run_until_parked();

    cx.simulate_keystrokes(*workspace, "space n n");
    cx.run_until_parked();
    cx.dispatch_action(*workspace, crate::MinibufferConfirm);
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            let mutation =
                take_desk_mutation(workspace, HostId::default()).expect("first note mutation");
            assert!(
                mutation
                    .writes
                    .iter()
                    .any(|write| matches!(write.id, rho_desk::cells::Id::Note(_))),
                "the first row on an empty desk is not a note"
            );
            assert!(
                mutation
                    .writes
                    .iter()
                    .any(|write| write.property == rho_desk::cells::Property::Parent(None)),
                "the first row on an empty desk is not a root"
            );
        })
        .unwrap();
}

/// A workspace sitting on the desk map. Cold start lands on Home now, so
/// tests about the map, the prompt, or the tree open it the way Home's root
/// menu does.
fn overview_workspace(cx: &mut TestAppContext) -> WindowHandle<Workspace> {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_startup_overview_for_test(window, cx);
        })
        .unwrap();
    workspace
}

/// `tab`, as a keystroke: the verdicts over a card, Home over none, and
/// Home again from the verdict menu's own row.
fn press_tab(workspace: &WindowHandle<Workspace>, cx: &mut TestAppContext) {
    cx.simulate_keystrokes(**workspace, "tab");
    cx.run_until_parked();
}

/// A frame, so what a row put off to the next one (the desk rebuild it
/// schedules) has run before the test reads the desk.
fn next_frame(cx: &mut TestAppContext, workspace: WindowHandle<Workspace>) {
    cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
        .expect("draw a frame");
    cx.run_until_parked();
}

pub(super) fn test_workspace(cx: &mut TestAppContext) -> WindowHandle<Workspace> {
    story::reset();
    cx.update(init_test_app);
    let target = AttachTarget::Unix(std::env::temp_dir().join("rho-gui-test-nonexistent.sock"));
    let specs = vec![HostSpec {
        name: "local".to_owned(),
        target,
    }];
    cx.add_window(|window, cx| Workspace::new(specs.clone(), window, cx))
}

/// An overlay chain — transient to minibuffer to a Git approval and back —
/// leaves the reader in the mode it found them in. Written when the map
/// gave the reader a second mode to be in at rest; the map is gone, so the
/// mode under every overlay here is the surface's, and the rule that the
/// chain does not quietly change it is the same rule.
#[gpui::test]
fn modal_overlays_preserve_surface_mode(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = overview_workspace(cx);

    workspace
        .update(cx, |workspace, window, cx| {
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
            story::feed(
                workspace,
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
            story::feed(
                workspace,
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

/// The transcript the workspace holds for this agent, to edit and feed back.
fn transcript_of(
    workspace: &WindowHandle<Workspace>,
    cx: &mut TestAppContext,
    agent_id: AgentId,
) -> UiAgentState {
    workspace
        .update(cx, |workspace, _, _| {
            workspace.transcript_for_test(agent_id)
        })
        .expect("read transcript")
}

/// Feeds the transcript back with one change, as a daemon that saw more
/// of the turn would.
fn feed_edit(
    workspace: &WindowHandle<Workspace>,
    cx: &mut TestAppContext,
    agent_id: AgentId,
    edit: impl FnOnce(&mut UiAgentState),
) {
    let mut state = transcript_of(workspace, cx, agent_id);
    edit(&mut state);
    feed_frame(workspace, cx, agent_id, state);
}

/// Applies an edit and hands back the state after it, for a run of frames
/// built up one on the last.
fn edited_in(state: &mut UiAgentState, edit: impl FnOnce(&mut UiAgentState)) -> UiAgentState {
    edit(state);
    state.clone()
}

/// The assistant message at `index` keeps its first `keep_bytes` and grows
/// by `value`; past the end, a new message begins.
fn stream_text(state: &mut UiAgentState, index: usize, keep_bytes: usize, value: &str) {
    if index == state.blocks.len() {
        state.blocks.push(Arc::new(assistant("", None)));
    }
    let UiBlock::AssistantMessage { text, .. } = Arc::make_mut(&mut state.blocks[index]) else {
        panic!("block {index} is not an assistant message");
    };
    text.truncate(keep_bytes);
    text.push_str(value);
}

fn stream_tool_arguments(state: &mut UiAgentState, index: usize, keep_bytes: usize, value: &str) {
    let UiBlock::Tool(tool) = Arc::make_mut(&mut state.blocks[index]) else {
        panic!("block {index} is not a tool");
    };
    tool.arguments.truncate(keep_bytes);
    tool.arguments.push_str(value);
}

fn replace_block(state: &mut UiAgentState, index: usize, block: UiBlock) {
    if index == state.blocks.len() {
        state.blocks.push(Arc::new(block));
    } else {
        state.blocks[index] = Arc::new(block);
    }
}

fn feed_frame(
    workspace: &WindowHandle<Workspace>,
    cx: &mut TestAppContext,
    agent_id: AgentId,
    state: UiAgentState,
) {
    workspace
        .update(cx, |workspace, window, cx| {
            // Transcript rendering tests use a selected agent explicitly;
            // production startup no longer derives selection from a frame.
            if workspace.is_startup_pane() {
                workspace.select_agent(Some(agent_id), window, cx);
            }
            workspace.seed_transcript_for_test(agent_id, state, window, cx);
        })
        .expect("update workspace");
    cx.run_until_parked();
}

fn feed_frames(
    workspace: &WindowHandle<Workspace>,
    cx: &mut TestAppContext,
    frames: impl IntoIterator<Item = (AgentId, UiAgentState)>,
) {
    let frames = frames.into_iter().collect::<Vec<_>>();
    workspace
        .update(cx, |workspace, window, cx| {
            if workspace.is_startup_pane()
                && let Some((agent_id, _)) = frames.first()
            {
                workspace.select_agent(Some(*agent_id), window, cx);
            }
            for (agent_id, state) in frames {
                workspace.seed_transcript_for_test(agent_id, state, window, cx);
            }
        })
        .expect("update workspace");
    cx.update_window((*workspace).into(), |_, window, cx| {
        window.simulate_next_frame(cx);
    })
    .expect("flush queued frames");
    cx.run_until_parked();
}

#[gpui::test]
fn phone_transcript_waits_for_a_tap_to_focus_the_reply_editor(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    cx.simulate_window_resize(*workspace, size(px(500.), px(800.)));
    cx.update_window(*workspace, |_, window, cx| {
        window.simulate_next_frame(cx);
    })
    .expect("paint phone Desk");
    cx.run_until_parked();

    let agent_id = agent(1);
    feed_frame(
        &workspace,
        cx,
        agent_id,
        state(vec![user("read this first")], Vec::new()),
    );
    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            assert!(
                !editor.read(cx).focus_handle(cx).is_focused(window),
                "opening a phone transcript must not focus its reply editor"
            );
        })
        .expect("inspect initial transcript focus");

    let reply_position = editor.read_with(cx, |editor, _| {
        let bounds = *editor.last_bounds().expect("painted reply editor bounds");
        gpui::point(bounds.center().x, bounds.bottom() - px(48.))
    });
    cx.update_window(*workspace, |_, window, cx| {
        window.dispatch_event(
            MouseDownEvent {
                position: reply_position,
                modifiers: Modifiers::none(),
                button: MouseButton::Left,
                click_count: 1,
                first_mouse: false,
            }
            .to_platform_input(),
            cx,
        );
        window.dispatch_event(
            MouseUpEvent {
                position: reply_position,
                modifiers: Modifiers::none(),
                button: MouseButton::Left,
                click_count: 1,
            }
            .to_platform_input(),
            cx,
        );
    })
    .expect("tap reply editor");
    cx.run_until_parked();

    workspace
        .update(cx, |_, window, cx| {
            assert!(
                editor.read(cx).focus_handle(cx).is_focused(window),
                "tapping the reply line must focus its editor"
            );
        })
        .expect("inspect tapped transcript focus");
}

#[gpui::test]
fn phone_modal_override_survives_settings_recompute(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    cx.simulate_window_resize(*workspace, size(px(500.), px(800.)));
    cx.update_window(*workspace, |_, window, cx| {
        window.simulate_next_frame(cx);
    })
    .expect("paint phone Desk");
    cx.run_until_parked();
    cx.update(|cx| {
        assert!(
            !vim_mode_setting::HelixModeSetting::get_global(cx).0,
            "phone entry disables modal editing"
        );
    });

    // Anything that recomputes settings — a language registering semantic
    // token rules, a settings file reload — rebuilds the globals from file
    // contents and drops `override_global` values.
    cx.update(|cx| {
        use gpui::UpdateGlobal as _;
        SettingsStore::update_global(cx, |store, cx| {
            let _ = store.set_user_settings("{}", cx);
        });
    });
    cx.run_until_parked();
    cx.update(|cx| {
        assert!(
            !vim_mode_setting::HelixModeSetting::get_global(cx).0,
            "a settings recompute must not re-enable modal editing while phone mode is active"
        );
    });
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

fn concealed_ranges(
    workspace: &WindowHandle<Workspace>,
    editor: &Entity<Editor>,
    cx: &mut TestAppContext,
) -> Vec<std::ops::Range<multi_buffer::MultiBufferOffset>> {
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.snapshot(window, cx);
                snapshot.inlay_snapshot().concealed_ranges()
            })
        })
        .expect("read concealment ranges")
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

/// The folds this transcript elided history with, by id, so a test can ask
/// both whether history is elided and whether the same folds survived a
/// rebuild. They are folds rather than blocks because the fold map is below
/// the wrap map: elided history leaves the wrap's input entirely.
fn history_folds(
    editor: &Entity<editor::Editor>,
    cx: &mut TestAppContext,
) -> rustc_hash::FxHashSet<editor::display_map::FoldId> {
    cx.update(|cx| {
        editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            snapshot
                .folds_in_range(
                    multi_buffer::MultiBufferOffset(0)..snapshot.buffer_snapshot().len(),
                )
                .filter(|fold| {
                    fold.placeholder.type_tag
                        == Some(std::any::TypeId::of::<rho_agents::transcript::HistoryFold>())
                })
                .map(|fold| fold.id)
                .collect()
        })
    })
}

fn has_display_elision(workspace: &WindowHandle<Workspace>, cx: &mut TestAppContext) -> bool {
    let editor = active_editor(workspace, cx);
    !history_folds(&editor, cx).is_empty()
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

#[gpui::test]
fn dashboard_has_no_persistent_masthead_block(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            let editor = workspace.dashboard_editor();
            editor.update(cx, |editor, cx| {
                let snapshot = editor.snapshot(window, cx);
                let custom_blocks = snapshot
                    .blocks_in_range(DisplayRow(0)..snapshot.max_point().row() + 1)
                    .filter(|(_, block)| matches!(block, Block::Custom(_)))
                    .count();
                assert_eq!(custom_blocks, 0);
            });
        })
        .unwrap();
}

/// Slice 3's claim, read off the display map itself: a `Changed` for one
/// agent costs no display-map resync at all. Before the slice it rebuilt the
/// whole tree, and every buffer disabled its header one at a time.
#[gpui::test]
fn one_agents_change_costs_no_display_map_resync(cx: &mut TestAppContext) {
    // The trace is a process-wide ring shared with any test running beside
    // this one, so the counting is by thread. Nothing turns it back off:
    // the flag is only ever set, and the ring is bounded.
    gpui::profiler::set_editor_trace_enabled(true);
    let _claim = gpui::profiler::claim_editor_trace_for_this_thread();
    // SAFETY: gettid has no arguments or memory-safety preconditions.
    let tid = unsafe { libc::syscall(libc::SYS_gettid) as u64 };
    let mut timings = gpui::profiler::EditorTimingCollector::new();
    let mut block_map_syncs = move || {
        let mine = timings
            .collect_unseen()
            .into_iter()
            .filter(|timing| {
                timing.tid == tid
                    && matches!(timing.kind, gpui::profiler::EditorTimingKind::BlockMapSync)
            })
            .collect::<Vec<_>>();
        let rows: u64 = mine
            .iter()
            .map(|timing| timing.old_rows.max(timing.new_rows))
            .sum();
        (mine.len(), rows)
    };

    let mut desk = DeskFixture::new();
    let parent = desk.note(None, "Desk");
    let agents = (1..=8).map(agent).collect::<Vec<_>>();
    for (nth, id) in agents.iter().enumerate() {
        desk.agent_row(parent.clone(), *id);
        desk.note(Some(parent.clone()), &format!("note {nth}"));
    }

    let workspace = overview_workspace(cx);
    cx.run_until_parked();
    block_map_syncs();

    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(
                    agents
                        .iter()
                        .enumerate()
                        .map(|(nth, id)| story::UiAgentHead {
                            generated_title: Some(format!("agent {nth}")),
                            ..ui_head(*id)
                        })
                        .collect(),
                    100,
                ),
                window,
                cx,
            );
        })
        .expect("build the desk");
    cx.run_until_parked();
    // What the build costs is the rig's question, not this test's: the
    // pathology needed thousands of filed agents, not eight.
    block_map_syncs();

    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::Log {
                    entries: story::head_entries(story::UiAgentHead {
                        generated_title: Some("renamed".to_owned()),
                        ..ui_head(agents[2])
                    }),
                },
                window,
                cx,
            );
        })
        .expect("one agent's title changes");
    cx.run_until_parked();
    let (syncs, rows) = block_map_syncs();
    assert_eq!(
        (syncs, rows),
        (0, 0),
        "a change to one agent resynced the display map"
    );
}

/// A burst of quiet cells costs the rows it names, whatever the desk is
/// sized. Every one of these verdicts moves a note's state where it sits
/// and moves no shape, so each is patched into the dealer's source where
/// it landed; none of them takes the source again, and none of them reads
/// the whole map. A hundred rows is where the whole-map copy this path
/// used to make was worth 25 microseconds an event, and the user's desk
/// is ten times that.
#[gpui::test]
async fn a_burst_of_quiet_cells_costs_its_own_rows(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let parent = desk.note(None, "Desk");
    let notes = (0..100)
        .map(|nth| desk.note(Some(parent.clone()), &format!("note {nth}")))
        .collect::<Vec<_>>();

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .expect("build the desk");
    cx.run_until_parked();

    let (taken, patched) = workspace
        .update(cx, |workspace, _, _| {
            workspace.dashboard.deal_work_for_test()
        })
        .expect("read the dealer's work");
    let burst = 8;
    for note in notes.iter().take(burst) {
        workspace
            .update(cx, |workspace, window, cx| {
                assert!(workspace.apply_verdict_for_test(
                    HostId::default(),
                    note,
                    crate::desk_view::DeskVerdict::Done,
                    window,
                    cx,
                ));
            })
            .expect("mark a note done");
    }
    cx.run_until_parked();

    let (taken_after, patched_after) = workspace
        .update(cx, |workspace, _, _| {
            workspace.dashboard.deal_work_for_test()
        })
        .expect("read the dealer's work");
    assert_eq!(
        taken_after, taken,
        "a hundred quiet rows made the source be taken again"
    );
    assert_eq!(
        patched_after - patched,
        burst,
        "each quiet cell is one patch and no more"
    );
}

/// A note that is its own parent does not hang the desk.
///
/// The store can be told anything: a Parent cell naming the row it sits on
/// is one write, and two devices filing each other's notes under one
/// another is two. Every walk up the tree — a breadcrumb, a heading's
/// context, an agent finding the note above it — follows the parent field
/// without a guard, so a chain that comes back around is not a wrong answer
/// but no answer at all: the frame never ends. The store client resolves it
/// where the tree is built, so what the desk hands on is a forest and the
/// row is drawn at the top, which is where a thing with nowhere above it
/// belongs. If this ever fails it fails by hanging, which is the fault
/// itself.
#[gpui::test]
async fn a_note_that_is_its_own_parent_is_drawn_at_the_root(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let looped = desk.note(None, "the loop");
    let under = desk.note(Some(looped.clone()), "beneath it");
    // The cell the daemon could hand over: the row filed under itself.
    desk.set(
        looped.clone(),
        rho_desk::cells::Property::Parent(Some(looped.clone())),
    );

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .expect("build the desk");
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            let nodes = workspace.desk_cells.nodes(HostId::default()).to_vec();
            let loop_node = nodes
                .iter()
                .find(|node| node.id == looped)
                .expect("the self-filed row is on the desk");
            assert_eq!(
                loop_node.parent, None,
                "a parent that comes back to the row is no parent at all"
            );
            assert!(
                nodes.iter().any(|node| node.id == under),
                "what was filed under it is still on the desk"
            );
        })
        .expect("read the desk");
}

/// A shape that moved is the one case that composes the map. Filing a
/// note writes its parent, which is where the row sits, so the dealer
/// cannot patch it where it was: the source is taken again. This is the
/// other half of the quiet-cell rule, and it is here so that cutting the
/// cost of a quiet delta cannot quietly cut the correctness of a loud one.
#[gpui::test]
async fn filing_a_note_moves_the_shape_and_takes_the_source(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let parent = desk.note(None, "Desk");
    let other = desk.note(Some(parent.clone()), "elsewhere");
    let notes = (0..16)
        .map(|nth| desk.note(Some(parent.clone()), &format!("note {nth}")))
        .collect::<Vec<_>>();

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .expect("build the desk");
    cx.run_until_parked();

    let (taken, _) = workspace
        .update(cx, |workspace, _, _| {
            workspace.dashboard.deal_work_for_test()
        })
        .expect("read the dealer's work");
    workspace
        .update(cx, |workspace, window, cx| {
            assert!(workspace.apply_verdict_for_test(
                HostId::default(),
                &notes[3],
                crate::desk_view::DeskVerdict::File {
                    parent: other.clone()
                },
                window,
                cx,
            ));
        })
        .expect("file one note");
    cx.run_until_parked();

    let (taken_after, _) = workspace
        .update(cx, |workspace, _, _| {
            workspace.dashboard.deal_work_for_test()
        })
        .expect("read the dealer's work");
    assert!(
        taken_after > taken,
        "a row moved and the source was patched where it no longer is"
    );
}

/// A verdict costs the cells it writes. Marking one note done moves that
/// note's state where it sits and moves no shape, so the dealer's source
/// is patched where the verdict landed rather than taken again. Written
/// against the map's compose-and-redraw counters; the map is gone and the
/// dealer's source is what stands behind the cards now, but the rule the
/// test guards — one event, one row's worth of work — is the same one.
#[gpui::test]
async fn one_verdict_costs_its_own_row(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let parent = desk.note(None, "Desk");
    let notes = (0..16)
        .map(|nth| desk.note(Some(parent.clone()), &format!("note {nth}")))
        .collect::<Vec<_>>();

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .expect("build the desk");
    cx.run_until_parked();

    // What the build costs is not the question; what one verdict costs
    // after it is. Marking a note done writes that note's cells and
    // nothing else, so it must draw that note's row and nothing else.
    let (taken, patched) = workspace
        .update(cx, |workspace, _, _| {
            workspace.dashboard.deal_work_for_test()
        })
        .expect("read the dealer's work");
    workspace
        .update(cx, |workspace, window, cx| {
            assert!(workspace.apply_verdict_for_test(
                HostId::default(),
                &notes[7],
                crate::desk_view::DeskVerdict::Done,
                window,
                cx,
            ));
        })
        .expect("mark one note done");
    cx.run_until_parked();

    let (taken_after, patched_after) = workspace
        .update(cx, |workspace, _, _| {
            workspace.dashboard.deal_work_for_test()
        })
        .expect("read the dealer's work");
    assert_eq!(
        taken_after, taken,
        "a verdict takes the source again: the rows and their order did not move"
    );
    assert!(
        patched_after > patched,
        "a verdict patched nothing, so the card it wrote is stale"
    );
    // And it is the verdict the reader sees, not just cheap work.
    workspace
        .update(cx, |workspace, _, _| {
            let node = workspace
                .desk_cells
                .node(HostId::default(), &notes[7])
                .expect("the note is still on the map");
            assert_eq!(
                node.state,
                rho_desk::cells::State::Done,
                "the row the verdict named says so"
            );
        })
        .expect("read the map");
}

/// The ranking is kept, not made again. A `Changed` for one agent makes
/// that agent's card and nothing else: the desk it is filed on, the notes
/// beside it and the other agents under the same note all stand. Before
/// this the read rebuilt every card from a walk of every node, so the cost
/// of one agent moving was the size of the desk.
#[gpui::test]
fn one_agents_change_makes_one_card(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let parent = desk.note(None, "Desk");
    let agents = (1..=8).map(agent).collect::<Vec<_>>();
    for (nth, id) in agents.iter().enumerate() {
        desk.agent_row(parent.clone(), *id);
        desk.note(Some(parent.clone()), &format!("note {nth}"));
    }

    let workspace = overview_workspace(cx);
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(
                    agents
                        .iter()
                        .enumerate()
                        .map(|(nth, id)| story::UiAgentHead {
                            generated_title: Some(format!("agent {nth}")),
                            ..ui_head(*id)
                        })
                        .collect(),
                    100,
                ),
                window,
                cx,
            );
            for id in &agents {
                story::feed(
                    workspace,
                    HostId::default(),
                    story_wanting(*id, UnixMs(1)),
                    window,
                    cx,
                );
            }
        })
        .expect("build the desk");
    cx.run_until_parked();

    // What the build costs is not this test's question; what one change
    // costs after it is. Every agent is asking, so every one of them has a
    // card to make again.
    let before = workspace
        .update(cx, |workspace, _, _| {
            workspace.dashboard.cards_made_for_test()
        })
        .expect("read the count");

    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::Log {
                    entries: story::head_entries(story::UiAgentHead {
                        generated_title: Some("renamed".to_owned()),
                        ..ui_head(agents[2])
                    }),
                },
                window,
                cx,
            );
        })
        .expect("one agent's title changes");
    cx.run_until_parked();

    let after = workspace
        .update(cx, |workspace, _, _| {
            workspace.dashboard.cards_made_for_test()
        })
        .expect("read the count");
    assert!(
        before >= agents.len(),
        "the desk was supposed to make a card per asking agent, made {before}"
    );
    assert_eq!(
        after - before,
        1,
        "a change to one agent made {} cards",
        after - before
    );
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
    let blocks = history.into_iter().chain(live).map(Arc::new).collect();
    UiAgentState {
        blocks,
        status: UiAgentStatus::Streaming,
        context_used: None,
        usage: Default::default(),
    }
}

fn long_working_text() -> String {
    "alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\ngolf\nhotel\nindia\njuliet\nkilo\nlima\nmike\nnovember\noscar\npapa\n".to_owned()
}

#[gpui::test]
fn user_messages_render_with_turn_gaps_and_gutters(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(
            vec![
                user("first"),
                assistant("answer", Some(UiMessagePhase::FinalAnswer)),
                user("second"),
            ],
            Vec::new(),
        ),
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
fn initial_transcript_preserves_line_endings_when_placing_spans(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(
            vec![user("first\r\nsecond\rthird")],
            vec![assistant(
                "answer\r\ncontinued\rfinished",
                Some(UiMessagePhase::FinalAnswer),
            )],
        ),
    );

    let text = display_text(&workspace, cx);
    assert!(text.contains("first\r\nsecond\rthird"), "{text:?}");
    assert!(text.contains("answer\r\ncontinued\rfinished"), "{text:?}");
}

#[gpui::test]
fn selection_actions_recover_cursor_from_replaced_transcript_excerpt(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(vec![user("old transcript")], Vec::new()),
    );

    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let offset = snapshot.text().find("old transcript").expect("transcript");
                editor.change_selections(
                    editor::SelectionEffects::no_scroll(),
                    window,
                    cx,
                    |selections| {
                        let offset = editor::MultiBufferOffset(offset);
                        selections.select_ranges([offset..offset]);
                    },
                );
            });
        })
        .expect("place cursor");

    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(vec![user("replacement")], Vec::new()),
    );

    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                let transcript_id = editor
                    .buffer()
                    .read(cx)
                    .all_buffers()
                    .into_iter()
                    .find(|buffer| buffer.read(cx).text().contains("replacement"))
                    .expect("replacement transcript buffer")
                    .read(cx)
                    .remote_id();
                editor.fold_buffer(transcript_id, cx);
                editor.prepare_for_insert(window, cx);
                let snapshot = editor.display_snapshot(cx);
                let selection = editor.selections.newest_anchor();
                assert!(snapshot.can_resolve(&selection.start));
                assert!(snapshot.can_resolve(&selection.end));
            });
        })
        .expect("prepare for insert");
}

#[gpui::test]
fn last_response_has_a_blank_line_before_the_prompt(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(vec![user("question")], vec![assistant("answer", None)]),
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
        state(vec![user("last user")], Vec::new()),
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
        state(
            vec![user("local"), agent_message(agent(2), "remote")],
            Vec::new(),
        ),
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
        state(
            vec![user("go")],
            vec![assistant("hel", Some(UiMessagePhase::FinalAnswer))],
        ),
    );
    assert!(display_text(&workspace, cx).contains("hel"));

    feed_edit(&workspace, cx, agent(1), |state| {
        stream_text(state, 1, 3, "lo world")
    });
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
    feed_frame(&workspace, cx, agent(1), state(blocks, Vec::new()));
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
        feed_edit(&workspace, cx, agent(1), |state| {
            stream_text(state, index, *keep_bytes, value)
        });
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
    feed_frame(&workspace, cx, agent(1), state(transcript(1), Vec::new()));
    phase("attach", start.elapsed(), 1);

    let start = std::time::Instant::now();
    feed_frame(&workspace, cx, agent(2), state(transcript(2), Vec::new()));
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
        feed_edit(&workspace, cx, agent(1), |state| {
            replace_block(
                state,
                index,
                UiBlock::Tool(UiTool {
                    id: format!("t1.{}", blocks_count - 1),
                    name: "shell_command".to_owned(),
                    arguments: format!("echo {tick}"),
                    preview: None,
                    status: UiToolStatus::Running,
                    output: None,
                    error: None,
                    started_at: None,
                    finished_at: None,
                    metadata: None,
                }),
            )
        });
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
        state(
            vec![assistant(
                "**bold** and `code` and plain\n",
                Some(UiMessagePhase::FinalAnswer),
            )],
            Vec::new(),
        ),
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
        state(
            vec![user("**user markup renders**")],
            vec![assistant(
                "## Heading\n\n**bold** and `code`.\n",
                Some(UiMessagePhase::FinalAnswer),
            )],
        ),
    );
    cx.run_until_parked();

    let text = display_text(&workspace, cx);
    assert!(
        text.contains("Heading\n\nbold and code.\n"),
        "markup should not reach the screen: {text:?}"
    );
    // The user's own markup renders like anyone else's: they are writing
    // markdown too, and a turn is one buffer end to end.
    assert!(
        text.contains("user markup renders") && !text.contains("**user markup renders**"),
        "the user's markup was left on screen: {text:?}"
    );
    let buffer = buffer_text(&workspace, cx);
    assert!(
        buffer.contains("## Heading\n\n**bold** and `code`.\n"),
        "the buffer keeps the markdown source for copy and search: {buffer:?}"
    );

    // Streaming past a concealed range refolds it in place.
    feed_edit(&workspace, cx, agent(1), |state| {
        stream_text(
            state,
            1,
            "## Heading\n\n**bold** and `code`.\n".len(),
            "*more*\n",
        )
    });
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
        state(
            vec![user("show a table")],
            vec![assistant(table, Some(UiMessagePhase::FinalAnswer))],
        ),
    );
    cx.run_until_parked();

    let buffer = buffer_text(&workspace, cx);
    assert!(
        buffer.contains(table),
        "source should remain unchanged: {buffer:?}"
    );
    assert_table_pipes_align(&workspace, 3, cx);

    feed_edit(&workspace, cx, agent(1), |state| {
        stream_text(state, 1, table.len(), "| longest name | failed |\n")
    });
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
        state(
            vec![user("show it")],
            vec![assistant(tag, Some(UiMessagePhase::FinalAnswer))],
        ),
    );

    assert!(buffer_text(&workspace, cx).contains(tag));
    assert!(!display_text(&workspace, cx).contains(tag));
    assert!(has_custom_block(&workspace, cx));

    feed_edit(&workspace, cx, agent(1), |state| {
        replace_block(
            state,
            1,
            assistant("ordinary text", Some(UiMessagePhase::FinalAnswer)),
        )
    });
    assert!(!has_custom_block(&workspace, cx));
}

#[gpui::test]
fn queued_streaming_updates_to_one_block_render_once_to_final_state(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(
            vec![user("go")],
            vec![assistant("hel", Some(UiMessagePhase::FinalAnswer))],
        ),
    );

    let mut streamed = transcript_of(&workspace, cx, agent(1));
    let mut update = |keep_bytes, value: &str| {
        edited_in(&mut streamed, |state| {
            stream_text(state, 1, keep_bytes, value)
        })
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
        state(
            vec![user("write a long response")],
            vec![assistant(&original, Some(UiMessagePhase::FinalAnswer))],
        ),
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

    feed_edit(&workspace, cx, agent(1), |state| {
        stream_text(state, 1, original.len(), " appended suffix")
    });

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
    let mut streamed = transcript_of(&workspace, cx, agent(1));
    let first = edited_in(&mut streamed, |state| {
        stream_text(
            state,
            1,
            original.len() + " appended suffix".len(),
            first_suffix,
        )
    });
    let second = edited_in(&mut streamed, |state| {
        stream_text(
            state,
            1,
            original.len() + " appended suffix".len() + first_suffix.len(),
            second_suffix,
        )
    });
    feed_frames(&workspace, cx, [(agent(1), first), (agent(1), second)]);
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
        state(
            vec![user("do work")],
            vec![assistant(
                &long_working_text(),
                Some(UiMessagePhase::Commentary),
            )],
        ),
    );
    let preview = workspace
        .update(cx, |workspace, window, cx| {
            let model = workspace.active_agent_model().expect("agent view");
            model.update(cx, |model, cx| model.preview_editor(window, cx))
        })
        .expect("open document preview");
    let folded_elisions = |cx: &mut TestAppContext| history_folds(&preview, cx);
    let initial_elisions = folded_elisions(cx);
    assert_eq!(initial_elisions.len(), 1);

    // The existing decorated response becomes an interior excerpt, but is not
    // rebuilt. Its concrete editor decoration and reconciliation state must
    // remain paired rather than inserting a duplicate.
    feed_edit(&workspace, cx, agent(1), |state| {
        replace_block(state, 2, user("continue"))
    });
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
        state(
            vec![user("do work")],
            vec![
                assistant(&long_working_text(), Some(UiMessagePhase::Commentary)),
                UiBlock::Reasoning {
                    text: String::new(),
                },
            ],
        ),
    );
    let preview = workspace
        .update(cx, |workspace, window, cx| {
            let model = workspace.active_agent_model().expect("agent view");
            model.update(cx, |model, cx| model.preview_editor(window, cx))
        })
        .expect("open document preview");
    let folded_elisions = |cx: &mut TestAppContext| history_folds(&preview, cx);
    let initial_elisions = folded_elisions(cx);
    assert_eq!(initial_elisions.len(), 1);

    // Only the invisible terminal reasoning buffer is rebuilt. Cropping the
    // preceding composed document tail must not discard decoration state for
    // its surviving excerpt and insert a duplicate editor object.
    feed_edit(&workspace, cx, agent(1), |state| {
        state.status = UiAgentStatus::Idle
    });
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
        state(
            vec![user(&user_text)],
            vec![assistant(&response, Some(UiMessagePhase::FinalAnswer))],
        ),
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
    feed_edit(&workspace, cx, agent(1), |state| {
        stream_text(state, 1, response.len(), "\nappended response");
        replace_block(
            state,
            2,
            UiBlock::Tool(tool("tool-1", UiToolStatus::Running, Some(1), None)),
        );
    });

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

#[gpui::test]
fn whole_transcript_rebuild_batches_multibuffer_events(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let blocks = (0..100)
        .map(|index| {
            if index % 2 == 0 {
                user(&format!("user turn {index}"))
            } else {
                assistant(
                    &format!("assistant turn {index}"),
                    Some(UiMessagePhase::FinalAnswer),
                )
            }
        })
        .collect::<Vec<_>>();
    feed_frame(&workspace, cx, agent(1), state(blocks, Vec::new()));

    let editor = active_editor(&workspace, cx);
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    workspace
        .update(cx, |_, _, cx| {
            let buffer = editor.read(cx).buffer().clone();
            let events = events.clone();
            cx.subscribe(&buffer, move |_, _, event, _| {
                events.lock().unwrap().push(event.clone());
            })
            .detach();
        })
        .expect("subscribe to transcript multibuffer");

    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(
            vec![
                user("replacement"),
                assistant("done", Some(UiMessagePhase::FinalAnswer)),
            ],
            Vec::new(),
        ),
    );

    let events = events.lock().unwrap();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, multi_buffer::Event::BufferRangesUpdated { .. })),
        "transcript rebuild emitted per-buffer range events: {events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, multi_buffer::Event::BufferRangesUpdatedBatch { .. }))
    );
    assert!(
        events
            .iter()
            .filter(|event| matches!(event, multi_buffer::Event::Edited { .. }))
            .count()
            <= 2,
        "transcript rebuild emitted too many edit events: {events:#?}"
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
        state(
            vec![user("write a long response")],
            vec![assistant(&streamed, Some(UiMessagePhase::FinalAnswer))],
        ),
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
        feed_edit(&workspace, cx, agent(1), |state| {
            stream_text(state, 1, keep_bytes, suffix)
        });
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
        state(
            vec![user("go")],
            vec![assistant("hel", Some(UiMessagePhase::FinalAnswer))],
        ),
    );

    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| editor.insert("draft", window, cx));
        })
        .expect("type prompt");

    feed_edit(&workspace, cx, agent(1), |state| {
        stream_text(state, 1, 3, "lo")
    });

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
        state(
            vec![user("go")],
            vec![
                assistant("hello", Some(UiMessagePhase::FinalAnswer)),
                UiBlock::Tool(tool("t1", UiToolStatus::Running, None, None)),
            ],
        ),
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
        state(
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
        ),
    );

    feed_edit(&workspace, cx, agent(1), |state| {
        stream_tool_arguments(state, 1, 4, " ok")
    });

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
        state(
            vec![user("do work")],
            vec![assistant(
                &long_working_text(),
                Some(UiMessagePhase::Commentary),
            )],
        ),
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
        state(
            vec![user("do work")],
            vec![assistant(
                &long_working_text(),
                Some(UiMessagePhase::FinalAnswer),
            )],
        ),
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
        state(vec![user("run tools")], pending),
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
        state(
            vec![
                user("go"),
                UiBlock::Tool(tool("t1", UiToolStatus::Success, Some(1_000), Some(3_500))),
            ],
            Vec::new(),
        ),
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
        state(
            vec![
                user("go"),
                UiBlock::Tool(tool("t1", UiToolStatus::Running, Some(started), None)),
            ],
            Vec::new(),
        ),
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
fn empty_prompt_shows_placeholder_and_gutter(cx: &mut TestAppContext) {
    let workspace = overview_workspace(cx);
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
        state(vec![user("previous agent")], Vec::new()),
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
        state(
            vec![
                user("previous agent"),
                assistant("background update", Some(UiMessagePhase::FinalAnswer)),
            ],
            Vec::new(),
        ),
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
    let workspace = overview_workspace(cx);
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
        state(vec![user("background agent")], Vec::new()),
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
fn notices_append_to_messages_without_changing_the_transcript(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(vec![user("first")], Vec::new()),
    );
    let transcript_before = buffer_text(&workspace, cx);
    workspace
        .update(cx, |workspace, _, cx| {
            workspace.notice_for_test(Some(&agent(1)), "boom", cx);
        })
        .expect("post notice");
    assert!(
        workspace
            .update(cx, |workspace, _, cx| workspace
                .message_log_texts(cx)
                .iter()
                .any(|message| message.ends_with(": boom")))
            .expect("read messages"),
        "notice should be retained in the message log"
    );
    assert_eq!(buffer_text(&workspace, cx), transcript_before);
}

#[gpui::test]
fn messages_surface_renders_in_order_and_follows_new_entries(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::ServerError("first".to_owned()),
                window,
                cx,
            );
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::ServerError("second".to_owned()),
                window,
                cx,
            );
            workspace.cmd_messages(window, cx);
        })
        .expect("open messages");
    let initial = buffer_text(&workspace, cx);
    assert!(
        initial.find("first").unwrap() < initial.find("second").unwrap(),
        "messages should render oldest to newest: {initial:?}"
    );

    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::ServerError("third".to_owned()),
                window,
                cx,
            );
            assert!(workspace.messages_following(cx));
        })
        .expect("append while messages are open");
    assert!(buffer_text(&workspace, cx).ends_with("[rho daemon error: third]\n"));
}

#[gpui::test]
fn first_messages_open_joins_surface_history(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = overview_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(vec![user("agent transcript")], Vec::new()),
    );
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.notice_for_test(None, "message log", cx);
            workspace.cmd_messages(window, cx);
        })
        .expect("open messages outside the overview");
    assert!(buffer_text(&workspace, cx).contains("message log"));

    cx.simulate_keystrokes(*workspace, "f21");
    assert!(buffer_text(&workspace, cx).contains("agent transcript"));
}

#[gpui::test]
fn scrolled_messages_viewport_stays_put_across_append(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.seed_messages_for_test(
                (0..200).map(|index| {
                    (
                        rho_window::style::StyleClass::SystemInfo,
                        format!("message-{index}"),
                    )
                }),
                cx,
            );
            workspace.cmd_messages(window, cx);
        })
        .expect("open a long messages buffer");
    cx.simulate_window_resize(*workspace, size(px(800.), px(400.)));
    cx.update_window(*workspace, |_, window, cx| {
        let _ = window.draw(cx);
    })
    .expect("draw messages");
    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_scroll_position(point(0., 0.), window, cx);
            });
        })
        .expect("scroll away from the bottom");
    cx.update_window(*workspace, |_, window, cx| {
        let _ = window.draw(cx);
    })
    .expect("draw scrolled messages");
    let before = workspace
        .update(cx, |_, _, cx| {
            editor.update(cx, |editor, cx| editor.scroll_position(cx).y)
        })
        .expect("read scroll position");

    workspace
        .update(cx, |workspace, _, cx| {
            workspace.append_test_message(
                "new message".to_owned(),
                rho_window::style::StyleClass::SystemInfo,
                cx,
            );
        })
        .expect("append while scrolled away");
    cx.update_window(*workspace, |_, window, cx| {
        let _ = window.draw(cx);
    })
    .expect("draw appended messages");
    let after = workspace
        .update(cx, |_, _, cx| {
            editor.update(cx, |editor, cx| editor.scroll_position(cx).y)
        })
        .expect("read scroll position");
    assert_eq!(after, before);
}

#[gpui::test]
fn evicting_the_last_message_of_a_class_clears_its_highlight(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.seed_messages_for_test(
                std::iter::once((
                    rho_window::style::StyleClass::SystemImportant,
                    "important".to_owned(),
                ))
                .chain((1..rho_agents::messages::LOG_CAP).map(|index| {
                    (
                        rho_window::style::StyleClass::SystemInfo,
                        format!("ordinary-{index}"),
                    )
                })),
                cx,
            );
            workspace.cmd_messages(window, cx);
            workspace.append_test_message(
                "ordinary-new".to_owned(),
                rho_window::style::StyleClass::SystemInfo,
                cx,
            );
        })
        .expect("evict the important message");
    let important_color = workspace
        .update(cx, |_, _, cx| {
            rho_window::style::StyleClass::SystemImportant
                .resolve(cx)
                .color
        })
        .expect("resolve important color");
    assert!(
        styled_runs(&workspace, cx)
            .iter()
            .all(|(_, color)| *color != important_color),
        "the evicted class highlight must not remain on ordinary messages"
    );
}

#[gpui::test]
fn message_log_cap_evicts_the_oldest_entries(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, _, cx| {
            for index in 0..=rho_agents::messages::LOG_CAP {
                workspace.append_test_log_entry(format!("message-{index}"), cx);
            }
            let messages = workspace.message_log_texts(cx);
            assert_eq!(messages.len(), rho_agents::messages::LOG_CAP);
            assert_eq!(messages.first().map(String::as_str), Some("message-1"));
            let expected_last = format!("message-{}", rho_agents::messages::LOG_CAP);
            assert_eq!(messages.last(), Some(&expected_last));
        })
        .expect("fill message log");
}

#[gpui::test]
fn capped_message_buffer_periodically_rebases_its_edit_history(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let original = workspace
        .update(cx, |workspace, _, cx| {
            workspace.seed_messages_for_test(
                (0..rho_agents::messages::LOG_CAP).map(|index| {
                    (
                        rho_window::style::StyleClass::SystemInfo,
                        format!("initial-{index}"),
                    )
                }),
                cx,
            );
            workspace.messages_buffer_id(cx)
        })
        .expect("seed capped messages");
    workspace
        .update(cx, |workspace, _, cx| {
            for index in 0..rho_agents::messages::REBASE_EVICTIONS {
                workspace.append_test_message(
                    format!("replacement-{index}"),
                    rho_window::style::StyleClass::SystemInfo,
                    cx,
                );
            }
        })
        .expect("append enough evictions to rebase");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert_ne!(workspace.messages_buffer_id(cx), original);
            assert_eq!(
                workspace.message_log_texts(cx).len(),
                rho_agents::messages::LOG_CAP
            );
        })
        .expect("inspect rebased messages");
}

#[gpui::test]
fn turn_cancelled_ack_is_not_persisted_as_notice(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(vec![user("first")], Vec::new()),
    );
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::TurnCancelled,
                window,
                cx,
            );
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
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::Recovering(std::time::Duration::from_secs(17)),
                window,
                cx,
            );
            assert_eq!(
                workspace.connection_status_label().as_deref(),
                Some("recovering 17s")
            );
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::Recovered,
                window,
                cx,
            );
            assert_eq!(workspace.connection_status_label(), None);
            story::feed(
                workspace,
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
    workspace
        .update(cx, |workspace, _, cx| {
            let notices = workspace.message_log_texts(cx);
            assert!(notices.iter().any(|text| text.contains("reconnecting")));
            assert!(notices.iter().any(|text| text.contains("connected")));
            assert!(notices.iter().any(|text| text.contains("disconnected")));
        })
        .expect("inspect connection notices");
}

/// The point of eliding history with folds rather than blocks: a fold is
/// below the wrap map, so the rows it covers never reach the wrap at all.
/// The block map, which is above the wrap, could only hide rows that had
/// already been wrapped.
#[gpui::test]
fn elided_history_leaves_the_wrap_maps_input(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(
            vec![user("do work")],
            vec![assistant(
                &long_working_text(),
                Some(UiMessagePhase::Commentary),
            )],
        ),
    );
    let editor = active_editor(&workspace, cx);
    assert!(
        !history_folds(&editor, cx).is_empty(),
        "the working text is elided"
    );

    let (buffer_rows, wrap_input_rows) = cx.update(|cx| {
        editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            (
                snapshot.buffer_snapshot().max_point().row,
                snapshot.fold_snapshot().max_point().row(),
            )
        })
    });
    assert!(
        wrap_input_rows < buffer_rows,
        "the elided rows are gone before the wrap sees them: \
         {wrap_input_rows} of {buffer_rows} rows reach it"
    );
}

#[gpui::test]
fn elided_history_still_soft_wraps_the_rows_it_leaves(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let long_line = "wrap me ".repeat(200);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(
            vec![user(&long_line)],
            vec![assistant(
                &long_working_text(),
                Some(UiMessagePhase::Commentary),
            )],
        ),
    );
    let editor = active_editor(&workspace, cx);
    assert!(
        !history_folds(&editor, cx).is_empty(),
        "the working text is elided"
    );

    // A fold takes rows out of the wrap's input; it must not take the wrap
    // away from the rows that are left. The long user line is not inside a
    // fold, so it still has to come out of the display as several rows.
    let (fold_rows, display_rows) = cx.update(|cx| {
        editor.update(cx, |editor, cx| {
            editor
                .display_map
                .update(cx, |map, cx| map.set_wrap_width(Some(gpui::px(160.)), cx));
            let snapshot = editor.display_snapshot(cx);
            (
                snapshot.fold_snapshot().max_point().row(),
                snapshot.max_point().row().0,
            )
        })
    });
    cx.run_until_parked();
    let display_rows_after_parking = cx.update(|cx| {
        editor.update(cx, |editor, cx| {
            editor.display_snapshot(cx).max_point().row().0
        })
    });
    assert!(
        display_rows_after_parking > fold_rows,
        "the rows a fold leaves still wrap: {display_rows} rows before the \
         rewrap and {display_rows_after_parking} after, over {fold_rows} \
         rows of wrap input"
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
        state(
            vec![user("do work")],
            vec![assistant(
                &long_working_text(),
                Some(UiMessagePhase::Commentary),
            )],
        ),
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
    let workspace = overview_workspace(cx);
    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| editor.insert("hello rho", window, cx));
        })
        .expect("type into prompt");

    cx.dispatch_action(*workspace, crate::SubmitPrompt);

    // Not connected, so the submission reaches the message log — proving the
    // action reached the workspace handler without changing the draft.
    let text = display_text(&workspace, cx);
    assert!(
        workspace
            .update(cx, |workspace, _, cx| workspace
                .message_log_texts(cx)
                .iter()
                .any(|message| message.contains("not connected to rho-daemon")))
            .expect("read messages"),
        "submit should reach the workspace and report the failed send"
    );
    // Draft submissions keep the buffer until the daemon confirms creation,
    // so a failed send never loses the message.
    assert!(
        text.contains("hello rho"),
        "a failed draft submit should keep the message: {text:?}"
    );
}

#[gpui::test]
fn upload_gui_telemetry_action_reports_when_no_daemon_is_connected(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    cx.dispatch_action(*workspace, crate::UploadGuiTelemetry);
    assert!(
        workspace
            .update(cx, |workspace, _, cx| workspace
                .message_log_texts(cx)
                .iter()
                .any(
                    |message| message.contains("performance snapshot: no daemon is connected")
                ))
            .expect("read messages"),
        "telemetry action should reach the workspace and fail nonfatally"
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
        UiAgentState {
            blocks: vec![
                Arc::new(user("go")),
                Arc::new(assistant("done", Some(UiMessagePhase::FinalAnswer))),
            ],
            status: UiAgentStatus::Idle,
            context_used: Some(194_816),
            usage: Default::default(),
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
        UiAgentState {
            blocks: vec![Arc::new(user("go"))],
            status: UiAgentStatus::Idle,
            context_used: Some(62_300),
            usage: Default::default(),
        },
    );
    feed_edit(&workspace, cx, agent(1), |state| {
        state.usage = rho_agents::state::UiAgentUsage {
            provider: "fable".to_owned(),
            total: rho_ui_proto::AgentUsageBucket {
                input_tokens: 1_000_000,
                cache_read_tokens: 1_000_000,
                cache_write_tokens: 1_000_000,
                output_tokens: 1_000_000,
                ..Default::default()
            },
        }
    });

    let spans = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .active_agent_model()
                .expect("agent view")
                .read(cx)
                .status_span_text()
        })
        .expect("read spans");
    assert_eq!(spans, "62k", "transcript row keeps only context usage");
}

#[gpui::test]
fn transcript_status_omits_internal_ids_but_keeps_human_chips(cx: &mut TestAppContext) {
    use rho_ui_proto::{WorkspaceId, WorkspaceIdDomain, WorkspaceInfo};

    let workspace = test_workspace(cx);
    let agent_id = agent(1);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ready_with(
                    vec![story::UiAgentHead {
                        spawn_name: Some("worker".to_owned()),
                        workdirs: vec![WorkspaceInfo::Workspace {
                            repo: "/tmp/rho".into(),
                            id: WorkspaceId::from_counter(1, &WorkspaceIdDomain(0)).unwrap(),
                        }],
                        ..ui_head(agent_id)
                    }],
                    100,
                ),
                window,
                cx,
            );
        })
        .expect("register managed-workspace agent");
    feed_frame(
        &workspace,
        cx,
        agent_id,
        UiAgentState {
            blocks: vec![Arc::new(user("go"))],
            status: UiAgentStatus::Idle,
            context_used: Some(62_300),
            usage: rho_agents::state::UiAgentUsage {
                provider: "fable".to_owned(),
                total: rho_ui_proto::AgentUsageBucket {
                    input_tokens: 1_000_000,
                    ..Default::default()
                },
            },
        },
    );

    let status = |cx: &mut TestAppContext| {
        workspace
            .update(cx, |workspace, _, cx| {
                workspace
                    .active_agent_model()
                    .expect("agent view")
                    .read(cx)
                    .status_span_text()
            })
            .expect("read status")
    };
    assert!(!status(cx).contains("ws-"), "desktop hides workspace id");

    cx.simulate_window_resize(*workspace, size(px(500.), px(800.)));
    cx.run_until_parked();
    let phone = status(cx);
    assert!(!phone.contains("ws-"), "phone status: {phone:?}");
    assert!(!phone.contains("eng"), "phone hides role ids: {phone:?}");
    assert!(phone.contains("62k"), "phone keeps tokens: {phone:?}");
    assert!(!phone.contains('$'), "phone hides cost: {phone:?}");

    cx.simulate_window_resize(*workspace, size(px(1200.), px(800.)));
    cx.run_until_parked();
    assert!(
        !status(cx).contains("ws-"),
        "desktop keeps workspace ids hidden after leaving phone mode"
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

#[test]
fn filing_completion_keeps_duplicate_heading_identity() {
    assert_eq!(
        crate::minibuffer::completion_start("Project Al", true),
        0,
        "filing completion replaces the whole partial title"
    );
    let first = rho_desk::cells::Id::Note(rho_desk::cells::Uuid([1_u8; 16]));
    let second = rho_desk::cells::Id::Note(rho_desk::cells::Uuid([2_u8; 16]));
    let destinations = vec![
        (
            "Project Alpha".into(),
            "Work / Project Alpha".into(),
            HostId(3),
            first,
        ),
        (
            "Project Alpha".into(),
            "Work / Project Alpha".into(),
            HostId(3),
            second.clone(),
        ),
    ];
    let candidate = crate::minibuffer::Candidate {
        value: "Project Alpha".into(),
        description: "Work / Project Alpha".into(),
    };
    assert_eq!(
        crate::workspace::resolve_filing_destination(&destinations, &candidate, 1),
        Some((HostId(3), second))
    );
    assert!(!candidate.value.contains("7:2"));
    assert!(!candidate.description.contains("7:2"));
}

#[gpui::test]
fn deal_file_bare_enter_files_the_dealt_node_under_the_offered_label(cx: &mut TestAppContext) {
    // Filing is a verdict on the card's own node: one `Labeled` write, and
    // an undo that takes the label off again.
    let mut desk = DeskFixture::new();
    let destination = desk.label("rho");
    let dealt = desk.due_note(None, "Deal QA note");

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.pull_card(window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(
                workspace.current_deal_card_for_test(cx).map(|card| card.0),
                Some(crate::dashboard::DealCardId {
                    host: HostId::default(),
                    node_id: dealt.clone(),
                })
            );
        })
        .unwrap();

    cx.dispatch_action(*workspace, crate::DashboardDealFile);
    cx.run_until_parked();
    // The completion is visibly selected but untouched, exactly as in the
    // dealer flow: bare Enter accepts it rather than submitting an empty name.
    cx.dispatch_action(*workspace, crate::MinibufferConfirm);
    cx.run_until_parked();
    let stamp = workspace
        .update(cx, |workspace, _, _| {
            let mutation =
                take_desk_mutation(workspace, HostId::default()).expect("filing mutation");
            assert!(
                mutation.writes.iter().any(|write| write.id == dealt
                    && write.property
                        == rho_desk::cells::Property::Labeled {
                            label: destination.clone(),
                            present: true,
                        }),
                "the card is filed by the label it was dealt under: {:?}",
                mutation.writes
            );
            mutation.stamp
        })
        .unwrap();
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::DeskMutationAccepted { stamp },
                window,
                cx,
            );
            assert_eq!(workspace.verdict_undo_count_for_test(), 1);
            assert_eq!(workspace.echo_text_for_test(), Some("label: rho"));
        })
        .unwrap();

    cx.dispatch_action(*workspace, crate::UndoVerdict);
    workspace
        .update(cx, |workspace, _, _| {
            let mutation =
                take_desk_mutation(workspace, HostId::default()).expect("filing undo mutation");
            assert!(
                mutation.writes.iter().any(|write| write.id == dealt
                    && write.property
                        == rho_desk::cells::Property::Labeled {
                            label: destination.clone(),
                            present: false,
                        }),
                "undoing takes the label back off: {:?}",
                mutation.writes
            );
        })
        .unwrap();
}

/// `tab` is the verdicts, over the card in view, and the letters they used
/// to steal stay vim's. This is the whole of the change: the key opens a
/// menu that says what the keys are, and `d` in it writes exactly what deal
/// mode's `d` wrote.
#[gpui::test]
fn tab_opens_the_verdicts_over_the_card_in_view(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let dealt = desk.due_note(None, "Card in view");

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.pull_card(window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();
    cx.run_until_parked();

    press_tab(&workspace, cx);
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert!(
                workspace.verdict_transient_open(),
                "tab over a card opens the verdicts"
            );
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "d");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert!(!workspace.verdict_transient_open(), "the menu closes on d");
            let mutation = take_desk_mutation(workspace, HostId::default()).expect("done mutation");
            assert!(mutation.writes.iter().any(|write| write.id == dealt
                && write.property
                    == rho_desk::cells::Property::State(rho_desk::cells::State::Done)));
        })
        .unwrap();
}

/// Filing says where a card is shown, not whether it exists. An agent
/// nobody put under a note still asks for the user, so it is dealt, at the
/// root and with no breadcrumb.
#[gpui::test]
fn an_unfiled_agent_that_wants_the_user_is_still_dealt(cx: &mut TestAppContext) {
    let agent_id = agent(41);
    let desk = DeskFixture::new();

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(
                    vec![story::UiAgentHead {
                        story_pos: story::UiStoryPos(4),
                        spawn_name: Some("nobody filed me".to_owned()),
                        ..ui_head(agent_id)
                    }],
                    50,
                ),
                window,
                cx,
            );
            story::feed(
                workspace,
                HostId::default(),
                story_wanting(agent_id, UnixMs(1)),
                window,
                cx,
            );
        })
        .unwrap();
    next_frame(cx, workspace);
    workspace
        .update(cx, |workspace, window, cx| workspace.pull_card(window, cx))
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            let (identity, kind) = workspace
                .current_deal_card_for_test(cx)
                .expect("the unfiled agent is dealt");
            assert_eq!(kind, crate::dashboard::DealCardKind::Agent);
            assert_eq!(
                identity.node_id,
                rho_desk::cells::Id::Agent(agent_id),
                "the card stands for the agent itself, not a note over it"
            );
            // Opening it must reach the transcript. With no note behind the
            // card, a target looked up by node alone finds nothing and
            // opens an empty page instead.
            assert_eq!(
                workspace.card_target_for_test(identity),
                crate::dashboard::CardTarget::Agent(agent_id)
            );
        })
        .unwrap();
}

/// Snooze end to end through the transient, every unit the fingers know.
/// It was an operator in deal mode, `ss` for a day and `45sm` for
/// forty-five minutes; the count now goes inside the menu, where the units
/// are written down, and lands on exactly the same time. `s` is still a
/// day on its own, which is what makes the habit survive the move.
#[gpui::test]
fn a_snooze_goes_through_the_transient_with_its_count(cx: &mut TestAppContext) {
    use crate::workspace::{SnoozeUnit, snooze_target};

    cx.update(bind_test_keymaps);
    // The keys as fingers type them, and the time the same span lands on
    // when the workspace works it out for itself.
    for (keys, unit, count) in [
        ("s s", SnoozeUnit::Days, 1usize),
        ("s 7 d", SnoozeUnit::Days, 7),
        ("s 4 5 m", SnoozeUnit::Minutes, 45),
        ("s 3 h", SnoozeUnit::Hours, 3),
        ("s 2 w", SnoozeUnit::Weeks, 2),
    ] {
        // A card apiece: a verdict waits on the daemon before the deal
        // moves on, so one desk cannot hold three of them.
        let mut desk = DeskFixture::new();
        desk.due_note(None, "Card to snooze");
        let workspace = test_workspace(cx);
        workspace
            .update(cx, |workspace, window, cx| {
                story::feed(workspace, HostId::default(), desk.synced(), window, cx);
                workspace.pull_card(window, cx);
                workspace.take_host_messages_for_test(HostId::default());
            })
            .unwrap();
        cx.run_until_parked();

        let (expected, said) = snooze_target(unit, count as i64, chrono::Local::now());
        press_tab(&workspace, cx);
        cx.simulate_keystrokes(*workspace, keys);
        cx.run_until_parked();
        workspace
            .update(cx, |workspace, _, _| {
                assert!(
                    !workspace.verdict_transient_open(),
                    "{keys}: the menu closes once the unit lands"
                );
                let mutation = take_desk_mutation(workspace, HostId::default())
                    .unwrap_or_else(|| panic!("{keys}: snooze mutation"));
                let wrote = mutation
                    .writes
                    .iter()
                    .find_map(|write| match &write.property {
                        rho_desk::cells::Property::DeferUntil(Some(at)) => Some(*at),
                        _ => None,
                    })
                    .unwrap_or_else(|| panic!("{keys}: a snooze writes a wake time"));
                assert_eq!(wrote.precision, expected.precision, "{keys}");
                // A minute count is worked out twice a moment apart, so the
                // two answers differ by the time the test itself took.
                assert!(
                    (wrote.unix_ms - expected.unix_ms).abs() < 5_000,
                    "{keys}: woke at {wrote:?}, expected about {expected:?}"
                );
                // The words the bar will say once the daemon takes it.
                assert_eq!(
                    workspace.pending_verdict_echo_for_test(),
                    Some(format!("{said}: Card to snooze").as_str()),
                    "{keys}"
                );
            })
            .unwrap();
    }
}

/// The verdicts are a buffer under the point, not a strip at the bottom:
/// they open beside the row the reader is on and the point does not move to
/// make room for them. Both halves matter — a menu that stole the point
/// would answer about the wrong card when it closed.
#[gpui::test]
fn the_verdicts_open_under_the_point_and_leave_it_where_it_was(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    desk.due_note(None, "Card in view");

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.pull_card(window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    let before = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .active_editor(cx)
                .read(cx)
                .selections
                .newest_anchor()
                .head()
        })
        .unwrap();

    press_tab(&workspace, cx);
    cx.run_until_parked();
    let after = workspace
        .update(cx, |workspace, _, cx| {
            assert!(workspace.verdict_transient_open(), "the verdicts are open");
            workspace
                .active_editor(cx)
                .read(cx)
                .selections
                .newest_anchor()
                .head()
        })
        .unwrap();
    assert_eq!(before, after, "the point did not move to open the menu");

    cx.simulate_keystrokes(*workspace, "escape");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert!(
                !workspace.verdict_transient_open(),
                "escape closes the verdicts"
            );
            assert_eq!(
                workspace
                    .active_editor(cx)
                    .read(cx)
                    .selections
                    .newest_anchor()
                    .head(),
                before,
                "and the point came back to where it was"
            );
        })
        .unwrap();
}

/// The root menu takes the keyboard, so `tab` no longer reaches Home
/// while it is open: with a menu up, the key belongs to the menu. Three steps
/// down and three escapes back, with the point where it started — the whole
/// way back, not one step of it. The buffer is not touched on the way: the
/// menu is drawn at the bottom of the window, over the surface, so no row is
/// added to make room for it.
#[gpui::test]
fn the_root_menu_opens_at_the_bottom_and_escape_retraces_it(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    desk.due_note(None, "Card in view");

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.pull_card(window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    let point = |workspace: &Workspace, cx: &App| {
        workspace
            .active_editor(cx)
            .read(cx)
            .selections
            .newest_anchor()
            .head()
    };
    // How many rows the surface draws. A block in the buffer would show up
    // here as rows the buffer does not have; a thing drawn over the window
    // cannot.
    let drawn_rows = |workspace: &mut Workspace, window: &mut gpui::Window, cx: &mut App| {
        workspace
            .active_editor(cx)
            .update(cx, |editor, cx| editor.snapshot(window, cx))
            .display_snapshot
            .max_point()
            .row()
            .0
    };
    let before = workspace
        .update(cx, |workspace, _, cx| point(workspace, cx))
        .unwrap();
    let rows_before = workspace
        .update(cx, |workspace, window, cx| {
            drawn_rows(workspace, window, cx)
        })
        .unwrap();

    workspace
        .update(cx, |workspace, window, cx| {
            let subject = workspace.subject(window, cx);
            workspace.open_menu(crate::transient::root_menu(&subject), window, cx);
            assert_eq!(workspace.menu_title_for_test(), Some("rho"));
            assert_eq!(
                drawn_rows(workspace, window, cx),
                rows_before,
                "the menu drew no rows into the buffer"
            );
            assert!(
                !workspace.verdict_transient_open(),
                "the root menu is not the verdicts, so tab is not Home"
            );
            assert_eq!(point(workspace, cx), before, "the point did not move");
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "h");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(
                workspace.menu_title_for_test(),
                Some("hosts"),
                "h replaces the root menu over the same row"
            );
            assert_eq!(point(workspace, cx), before, "and still does not move it");
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "escape");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(
                workspace.menu_title_for_test(),
                Some("rho"),
                "escape goes back to the menu it came from, not out"
            );
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "escape");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, window, cx| {
            assert_eq!(workspace.menu_title_for_test(), None, "and then out");
            assert_eq!(point(workspace, cx), before, "the point came back");
            assert_eq!(
                drawn_rows(workspace, window, cx),
                rows_before,
                "and the buffer is the length it always was"
            );
        })
        .unwrap();
}

/// The phone draws the same menu. Not the same picture — a thumb needs a
/// target, not a row — but the same items, reached by tapping the row a key
/// would have run, with the same stack behind `back`.
#[gpui::test]
fn the_phone_sheet_is_the_same_menu_as_the_block(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    desk.due_note(None, "Card in view");

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.simulate_window_resize(*workspace, gpui::size(gpui::px(400.), gpui::px(800.)));
    cx.update_window(*workspace, |_, window, cx| {
        window.simulate_next_frame(cx);
    })
    .expect("draw phone frame");
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_menu(crate::transient::phone_root_menu(), window, cx);
            let sheet = workspace.menu_sheet().expect("a sheet to draw");
            assert_eq!(sheet.title, "menu");
            assert_eq!(
                sheet
                    .rows
                    .iter()
                    .map(|row| row.description.as_str())
                    .collect::<Vec<_>>(),
                ["Slack", "Agents", "Status"]
            );
            assert!(
                !sheet.has_back,
                "the root of the sheet has nothing under it"
            );
        })
        .unwrap();

    // Tapping "Status" is the same step `i` would have taken.
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.run_menu_at(2, window, cx);
            let sheet = workspace.menu_sheet().expect("the submenu draws");
            assert_eq!(sheet.title, "status");
            assert!(sheet.has_back, "and the sheet says there is a way back");
        })
        .unwrap();

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.menu_dismiss(window, cx);
            assert_eq!(workspace.menu_title_for_test(), Some("menu"));
            workspace.menu_dismiss(window, cx);
            assert_eq!(workspace.menu_title_for_test(), None, "and then out");
        })
        .unwrap();
}

/// The sheet has to be *drawn*, not merely open. The menu keeps its rows and
/// its title whether or not anything puts them on screen, so a test that asks
/// the workspace what the sheet says passes while the phone shows nothing at
/// all — which is exactly what happened: the overlay was drawn only for the
/// bottom strip the menus used to be, and a migrated menu opened into an
/// empty screen. This taps
/// where the last row lands, which fails if nothing is drawn there.
#[gpui::test]
fn the_phone_sheet_is_drawn_where_a_thumb_can_reach_it(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    desk.due_note(None, "Card in view");

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.simulate_window_resize(*workspace, gpui::size(gpui::px(400.), gpui::px(800.)));
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_menu(crate::transient::phone_root_menu(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    // The sheet sits on the bottom edge and its last row is "Status", so the
    // bottom of the screen is that row — the target a thumb actually has.
    let mut visual = gpui::VisualTestContext::from_window(*workspace, cx);
    visual.simulate_click(
        gpui::point(gpui::px(200.), gpui::px(790.)),
        gpui::Modifiers::none(),
    );
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(
                workspace.menu_title_for_test(),
                Some("status"),
                "a tap on the drawn sheet ran the row it landed on"
            );
        })
        .unwrap();
}

/// Escape out of the snooze units goes back to the verdicts, not out of the
/// menu: back returns, here as everywhere.
#[gpui::test]
fn escape_in_a_submenu_returns_to_the_menu_it_came_from(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    desk.due_note(None, "Card in view");

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.pull_card(window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    press_tab(&workspace, cx);
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "s");
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "escape");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert!(
                workspace.verdict_transient_open(),
                "escape left the units and came back to the verdicts"
            );
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "escape");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert!(
                !workspace.verdict_transient_open(),
                "the second escape leaves the menu"
            );
        })
        .unwrap();
}

/// A second `tab` is Home: the first put the verdicts on screen and the
/// menu's own `tab` row says the next one leaves. The second press reaches
/// the menu, not the editor, so it is the menu's row that answers it.
#[gpui::test]
fn a_second_tab_leaves_the_card_for_home(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    desk.due_note(None, "Card in view");

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.pull_card(window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    press_tab(&workspace, cx);
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert!(workspace.verdict_transient_open());
            assert_ne!(workspace.current_surface_name_for_test(), "home");
        })
        .unwrap();

    press_tab(&workspace, cx);
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert!(
                !workspace.verdict_transient_open(),
                "the second tab closes the verdicts"
            );
            assert_eq!(
                workspace.current_surface_name_for_test(),
                "home",
                "and leaves the card for Home"
            );
        })
        .unwrap();
}

/// A snoozed todo comes back from zero: the pace it climbed at before goes
/// with the verdict, or the card would return already halfway up the curve.
#[gpui::test]
fn a_snooze_zeroes_the_pace_it_was_climbing_at(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let dealt = desk.due_note(None, "Paced card");
    desk.set(dealt.clone(), rho_desk::cells::Property::PaceDays(7));

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.pull_card(window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();
    cx.run_until_parked();

    cx.dispatch_action(*workspace, crate::DashboardDealSnooze);
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            let mutation =
                take_desk_mutation(workspace, HostId::default()).expect("snooze mutation");
            assert!(mutation.writes.iter().any(|write| write.id == dealt
                && write.property == rho_desk::cells::Property::PaceDays(0)));
            let Some((_, rho_desk::cells::VerdictEvent::Applied { changes, .. })) =
                mutation.verdict
            else {
                panic!("the snooze records an applied verdict");
            };
            // The entry says what it put back, so an undo restores the pace.
            let paced = changes
                .iter()
                .find(|change| change.key == rho_desk::cells::PropertyKey::PaceDays)
                .expect("the pace is part of the verdict");
            assert_eq!(paced.before, Some(rho_desk::cells::Property::PaceDays(7)));
            assert_eq!(paced.after, Some(rho_desk::cells::Property::PaceDays(0)));
        })
        .unwrap();
}

#[gpui::test]
fn cancelling_the_file_prompt_writes_nothing_and_keeps_the_card(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    desk.note(None, "Somewhere to file");
    let dealt = desk.due_note(None, "First filing");

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.pull_card(window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();
    cx.run_until_parked();

    cx.dispatch_action(*workspace, crate::DashboardDealFile);
    cx.dispatch_action(*workspace, crate::MinibufferCancel);
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, cx| {
            assert!(
                take_desk_mutation(workspace, HostId::default()).is_none(),
                "a cancelled prompt files nothing"
            );
            assert_eq!(
                workspace.current_deal_card_for_test(cx).map(|card| card.0),
                Some(crate::dashboard::DealCardId {
                    host: HostId::default(),
                    node_id: dealt,
                }),
                "the card the reader was looking at is still the one dealt"
            );
        })
        .unwrap();
}

#[gpui::test]
fn undo_verdict_with_empty_stack_echoes(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    cx.dispatch_action(*workspace, crate::UndoVerdict);
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.echo_text_for_test(), Some("nothing to undo"));
        })
        .unwrap();
}

#[gpui::test]
fn tree_verdict_echoes_name_and_undo_restores_temporal_state(cx: &mut TestAppContext) {
    // A woken note with a cadence: dealt, then undone back to exactly the
    // cells the verdict replaced.
    let mut desk = DeskFixture::new();
    let note = desk.note(None, "Named card");
    let woke = rho_desk::cells::Timestamp {
        unix_ms: 1_577_836_800_000,
        precision: rho_desk::cells::TimestampPrecision::Day,
    };
    desk.set(
        note.clone(),
        rho_desk::cells::Property::DeferUntil(Some(woke)),
    );
    desk.set(note.clone(), rho_desk::cells::Property::PaceDays(1));

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.pull_card(window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();

    macro_rules! verdict_and_undo {
        ($action:expr, $echo:expr) => {{
            cx.dispatch_action(*workspace, $action);
            cx.run_until_parked();
            let stamp = workspace
                .update(cx, |workspace, _, _| {
                    let mutation = take_desk_mutation(workspace, HostId::default())
                        .expect("verdict mutation");
                    assert!(matches!(
                        mutation.verdict,
                        Some((_, rho_desk::cells::VerdictEvent::Applied { .. }))
                    ));
                    mutation.stamp
                })
                .unwrap();
            workspace
                .update(cx, |workspace, window, cx| {
                    story::feed(workspace,
                        HostId::default(),
                        ConnEvent::DeskMutationAccepted { stamp },
                        window,
                        cx,
                    );
                    assert_eq!(workspace.echo_text_for_test(), Some($echo));
                })
                .unwrap();
            cx.dispatch_action(*workspace, crate::UndoVerdict);
            let undo_stamp = workspace
                .update(cx, |workspace, _, _| {
                    let mutation =
                        take_desk_mutation(workspace, HostId::default()).expect("undo mutation");
                    // Undo is the log's own inverse, not a replayed edit.
                    assert!(matches!(
                        mutation.verdict,
                        Some((_, rho_desk::cells::VerdictEvent::Undone { of })) if of == stamp
                    ));
                    let node = workspace
                        .desk_cells_snapshot_for_test(HostId::default())
                        .into_iter()
                        .find(|candidate| candidate.id == note)
                        .unwrap();
                    assert_eq!(node.state, rho_desk::cells::State::Open);
                    assert_eq!(node.defer_until, Some(woke));
                    assert_eq!(node.pace_days, 1);
                    mutation.stamp
                })
                .unwrap();
            workspace
                .update(cx, |workspace, window, cx| {
                    story::feed(workspace,
                        HostId::default(),
                        ConnEvent::DeskMutationAccepted { stamp: undo_stamp },
                        window,
                        cx,
                    );
                    assert_eq!(
                        workspace.current_deal_card_for_test(cx).map(|card| card.0),
                        Some(crate::dashboard::DealCardId {
                            host: HostId::default(),
                            node_id: note.clone(),
                        })
                    );
                    assert!(workspace.dashboard_deal_mode_for_test(cx));
                })
                .unwrap();
        }};
    }

    verdict_and_undo!(crate::DashboardDealDone, "done: Named card");
    verdict_and_undo!(crate::DashboardDealMute, "mute: Named card");
    // The snooze operator's default unit is a day, and the bar says the day
    // it comes back on rather than the distance.
    let tomorrow = (chrono::Local::now().date_naive() + chrono::Duration::days(1))
        .format("%a %-d %b")
        .to_string();
    let snoozed = format!("snooze until {tomorrow}: Named card");
    verdict_and_undo!(crate::DashboardDealSnooze, snoozed.as_str());
    verdict_and_undo!(crate::DashboardDealTodo, "todo: Named card");

    // A delayed acknowledgement belongs to the submitted card, even if the
    // user has moved on to another deal in the meantime.
    cx.dispatch_action(*workspace, crate::DashboardDealDone);
    let delayed = workspace
        .update(cx, |workspace, _, _| {
            take_desk_mutation(workspace, HostId::default())
                .expect("delayed verdict mutation")
                .stamp
        })
        .unwrap();
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::DeskMutationAccepted { stamp: delayed },
                window,
                cx,
            );
            // The echo names the card the verdict was about, not whatever
            // the reader has moved on to.
            assert_eq!(workspace.echo_text_for_test(), Some("done: Named card"));
        })
        .unwrap();
}

/// Over a surface that is no card, `tab` has no verdicts to open, so it
/// goes Home in one press; from Home it is the way back.
#[gpui::test]
fn tab_over_no_card_opens_home(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["current"], window, cx);
            assert_eq!(workspace.current_surface_name_for_test(), "current");
        })
        .unwrap();
    press_tab(&workspace, cx);
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "home");
        })
        .unwrap();
    // Pressing it again is the way back to what the reader was reading.
    press_tab(&workspace, cx);
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "current");
        })
        .unwrap();
}

#[gpui::test]
fn f24_alias_opens_home(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["current"], window, cx);
            assert_eq!(workspace.current_surface_name_for_test(), "current");
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "f24");
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "home");
        })
        .unwrap();
}

#[gpui::test]
fn two_finger_swipe_down_opens_home(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["current"], window, cx);
            window.simulate_next_frame(cx);
            assert_eq!(workspace.current_surface_name_for_test(), "current");
        })
        .unwrap();
    cx.update_window(*workspace, |_, window, cx| {
        let touch = |id, phase, y, milliseconds| TouchEvent {
            id: TouchId(id),
            phase,
            position: point(px(100.), px(y)),
            timestamp: std::time::Duration::from_millis(milliseconds),
            ..Default::default()
        };
        for event in [
            touch(1, TouchPhase::Started, 500., 0),
            touch(2, TouchPhase::Started, 500., 1),
            touch(1, TouchPhase::Moved, 600., 20),
            touch(2, TouchPhase::Moved, 600., 21),
            touch(1, TouchPhase::Ended, 600., 30),
            touch(2, TouchPhase::Ended, 600., 31),
        ] {
            window.dispatch_event(event.to_platform_input(), cx);
        }
    })
    .unwrap();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "home");
        })
        .unwrap();
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
        state(
            vec![user("go")],
            vec![assistant(
                "```\n**bold**\nplain\n```\n",
                Some(UiMessagePhase::FinalAnswer),
            )],
        ),
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
        state(
            vec![user("go")],
            vec![assistant(&markup, Some(UiMessagePhase::FinalAnswer))],
        ),
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
    let folds = concealed_ranges(&workspace, &editor, cx);
    assert!(
        folds.len() >= 1_000,
        "every composed row is concealed: {}",
        folds.len()
    );

    // Scrolling to the top composes the history above the opening tail,
    // which is more text and so more concealment; what it must not do is
    // remove and recreate the concealment already there.
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_scroll_position(gpui::point(0., 0.), window, cx);
            });
        })
        .expect("scroll to transcript start");
    cx.run_until_parked();
    let composed = concealed_ranges(&workspace, &editor, cx);
    assert!(
        composed.len() >= folds.len(),
        "history composed on the way up is concealed too: {} then {}",
        folds.len(),
        composed.len()
    );
    let folds = composed;

    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_scroll_position(gpui::point(0., 400.), window, cx);
            });
        })
        .expect("scroll through transcript");
    cx.run_until_parked();
    assert_eq!(concealed_ranges(&workspace, &editor, cx), folds);
}

/// Two hundred turns of transcript, the shape a long-running agent has.
fn long_history() -> UiAgentState {
    let mut blocks = Vec::new();
    for turn in 0..200 {
        blocks.push(user(&format!("ask {turn}")));
        blocks.push(assistant(
            &format!("turn {turn} line one\nturn {turn} line two\nturn {turn} line three\n"),
            Some(UiMessagePhase::FinalAnswer),
        ));
    }
    state(blocks, Vec::new())
}

fn uncomposed_blocks(
    workspace: &WindowHandle<Workspace>,
    cx: &mut TestAppContext,
    agent_id: AgentId,
) -> usize {
    workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .agent_model_for_test(agent_id)
                .read(cx)
                .uncomposed_blocks()
        })
        .expect("read how much history is composed nowhere")
}

/// The block the reader is told the gap sits above, if there is a gap.
fn gap_marker_block(
    workspace: &WindowHandle<Workspace>,
    cx: &mut TestAppContext,
    agent_id: AgentId,
) -> Option<usize> {
    workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .agent_model_for_test(agent_id)
                .read(cx)
                .gap_marker_block()
        })
        .expect("read where the gap marker is")
}

fn transcript_point_block(
    workspace: &WindowHandle<Workspace>,
    cx: &mut TestAppContext,
    agent_id: AgentId,
) -> Option<usize> {
    let editor = active_editor(workspace, cx);
    workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .agent_model_for_test(agent_id)
                .read(cx)
                .store_point(&editor, cx)
                .map(|point| point.block)
        })
        .expect("read the point as the store sees it")
}

/// A screen opens at the cost of what it draws. The tail is composed and
/// laid out; the history above it is rendered nowhere and laid out nowhere
/// until a reader asks for it.
#[gpui::test]
fn a_long_transcript_opens_on_its_tail(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(&workspace, cx, agent(1), long_history());

    let text = buffer_text(&workspace, cx);
    assert!(
        text.contains("turn 199 line one"),
        "the tail is what a transcript opens on"
    );
    assert!(
        !text.contains("turn 0 line one"),
        "history the reader has not asked for is composed nowhere"
    );
    assert!(
        uncomposed_blocks(&workspace, cx, agent(1)) > 0,
        "history is waiting to be composed"
    );
}

/// Reading upward composes history as the reader reaches it, and what was
/// already composed keeps its place.
#[gpui::test]
fn scrolling_into_history_composes_it(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(&workspace, cx, agent(1), long_history());
    let opened = uncomposed_blocks(&workspace, cx, agent(1));

    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_scroll_position(gpui::point(0., 0.), window, cx);
            });
        })
        .expect("scroll to the top of what is composed");
    cx.run_until_parked();

    let after = uncomposed_blocks(&workspace, cx, agent(1));
    assert!(
        after < opened,
        "reaching the top composes more history: {opened} then {after}"
    );
    assert!(
        buffer_text(&workspace, cx).contains("turn 199 line one"),
        "the tail is still where it was"
    );
}

/// `gg` is the top of the transcript, which is the top of its history: the
/// reader asked for everything, so everything is composed, and the point
/// lands when the top exists.
#[gpui::test]
fn going_to_the_top_composes_every_row(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    feed_frame(&workspace, cx, agent(1), long_history());
    assert!(uncomposed_blocks(&workspace, cx, agent(1)) > 0);

    cx.simulate_keystrokes(*workspace, "escape g g");
    cx.run_until_parked();

    assert_eq!(
        uncomposed_blocks(&workspace, cx, agent(1)),
        0,
        "everything the reader asked for is composed"
    );
    assert!(
        buffer_text(&workspace, cx).contains("turn 0 line one"),
        "the top of the history is in the buffer"
    );
    assert_eq!(
        transcript_point_block(&workspace, cx, agent(1)),
        Some(0),
        "the point is on the first block"
    );
}

/// A surface remembers the point as a store position, never a buffer
/// offset: left deep in history and returned to, the point is on the same
/// block, whatever had to be composed again to place it.
#[gpui::test]
fn returning_to_a_transcript_returns_to_the_block_it_was_left_on(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    feed_frame(&workspace, cx, agent(1), long_history());

    cx.simulate_keystrokes(*workspace, "escape g g");
    cx.run_until_parked();
    let left_on = transcript_point_block(&workspace, cx, agent(1)).expect("a point in history");

    cx.simulate_keystrokes(*workspace, "ctrl-shift-backspace");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_agent(agent(1), window, cx);
        })
        .expect("open the transcript again");
    cx.run_until_parked();

    assert_eq!(
        transcript_point_block(&workspace, cx, agent(1)),
        Some(left_on),
        "returning puts the point back on the block it was left on"
    );
}

/// `/` in a transcript is the buffer's search, so it searches the whole
/// transcript: the history it has not composed yet is composed first, and
/// the point lands on the match.
#[gpui::test]
fn searching_a_transcript_composes_the_history_it_looks_through(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    feed_frame(&workspace, cx, agent(1), long_history());
    assert!(uncomposed_blocks(&workspace, cx, agent(1)) > 0);

    cx.simulate_keystrokes(*workspace, "escape / t u r n space 3 space l i n e enter");
    cx.run_until_parked();

    assert_eq!(
        uncomposed_blocks(&workspace, cx, agent(1)),
        0,
        "a search looks through the whole transcript"
    );
    let block = transcript_point_block(&workspace, cx, agent(1)).expect("the point is on a match");
    assert_eq!(
        block, 7,
        "the point is on the answer of the fourth turn, which is where the match is"
    );
}

/// A key means one thing per context. `n` and `N` are the search repeat on
/// the two surfaces that have a search, and the next unread on the two that
/// read rooms — by their own contexts, not by which binding was loaded
/// last.
#[gpui::test]
fn n_is_the_search_repeat_where_there_is_a_search_and_the_next_unread_where_there_is_a_room(
    cx: &mut TestAppContext,
) {
    use gpui::{KeyContext, Keystroke};

    cx.update(bind_test_keymaps);
    cx.update(|cx| {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let routes = |key: &str, contexts: &[KeyContext]| {
            let stroke = Keystroke::parse(key).unwrap();
            keymap
                .bindings_for_input(&[stroke], contexts)
                .0
                .first()
                .map(|binding| binding.action().name())
        };
        let surface = |name: &str, mode: &str| {
            [
                KeyContext::parse("RhoGui").unwrap(),
                KeyContext::parse(name).unwrap(),
                KeyContext::parse(&format!(
                    "Editor VimControl vim_mode={mode} vim_operator=none"
                ))
                .unwrap(),
            ]
        };
        for mode in ["normal", "helix_normal"] {
            for name in ["RhoTranscript", "RhoDashboard"] {
                let searchable = surface(name, mode);
                assert_eq!(
                    routes("n", &searchable),
                    Some("rho_gui::SearchRepeat"),
                    "`n` repeats the search on {name}"
                );
                assert_eq!(
                    routes("shift-n", &searchable),
                    Some("rho_gui::SearchRepeatReverse"),
                    "`shift-n` repeats it backwards on {name}"
                );
            }

            let room = surface("RhoSlackConversation", mode);
            assert_eq!(routes("shift-n", &room), Some("rho_gui::SlackNextUnread"));
            assert_ne!(routes("n", &room), Some("rho_gui::SearchRepeat"));

            let inbox = [
                KeyContext::parse("RhoGui").unwrap(),
                KeyContext::parse("RhoZulipInbox").unwrap(),
                KeyContext::parse(&format!("Editor VimControl vim_mode={mode}")).unwrap(),
            ];
            assert_eq!(routes("n", &inbox), Some("rho_gui::ZulipNextUnread"));

            // A surface with neither a search nor a room keeps vim's own,
            // which in this app means nothing at all.
            let note = surface("RhoNote", mode);
            assert_ne!(routes("n", &note), Some("rho_gui::SearchRepeat"));
        }
    });
}

/// A search that cannot be repeated is half a search: `n` runs the last
/// query again from the point, and `shift-n` runs it the other way.
#[gpui::test]
fn n_repeats_a_transcript_search_and_shift_n_runs_it_backwards(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    feed_frame(&workspace, cx, agent(1), long_history());

    cx.simulate_keystrokes(*workspace, "escape / l i n e space t w o enter");
    cx.run_until_parked();
    let first = transcript_point_block(&workspace, cx, agent(1)).expect("the point is on a match");

    cx.simulate_keystrokes(*workspace, "n");
    cx.run_until_parked();
    let second = transcript_point_block(&workspace, cx, agent(1)).expect("the point is on a match");
    assert!(
        second > first,
        "`n` moves on to the next match, not back to the same one"
    );

    cx.simulate_keystrokes(*workspace, "shift-n");
    cx.run_until_parked();
    assert_eq!(
        transcript_point_block(&workspace, cx, agent(1)),
        Some(first),
        "`shift-n` runs the same search the other way"
    );
}

/// The one thing a reader has to be told about a repeat is that it went
/// round the end of the buffer.
#[gpui::test]
fn a_repeat_that_wraps_says_so(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    feed_frame(&workspace, cx, agent(1), long_history());

    // The last turn's third line occurs once, so the repeat has nowhere to
    // go but round.
    cx.simulate_keystrokes(
        *workspace,
        "escape / t u r n space 1 9 9 space l i n e space t h r e e enter",
    );
    cx.run_until_parked();
    let only = transcript_point_block(&workspace, cx, agent(1)).expect("the point is on the match");

    cx.simulate_keystrokes(*workspace, "n");
    cx.run_until_parked();
    assert_eq!(
        transcript_point_block(&workspace, cx, agent(1)),
        Some(only),
        "the only match is where a wrapped search lands"
    );
    assert_eq!(
        workspace
            .update(cx, |workspace, _, _| workspace
                .echo_text_for_test()
                .map(str::to_owned))
            .expect("read the echo line"),
        Some("search: wrapped to the top".to_owned()),
        "the echo line says a search wrapped"
    );
}

/// `n` with nothing to repeat is vim's `n`, which does nothing here: the
/// action gives the key back rather than moving the point.
#[gpui::test]
fn n_with_nothing_to_repeat_leaves_the_point_alone(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    feed_frame(&workspace, cx, agent(1), long_history());
    cx.run_until_parked();

    let before = transcript_point_block(&workspace, cx, agent(1));
    cx.simulate_keystrokes(*workspace, "escape n");
    cx.run_until_parked();
    assert_eq!(transcript_point_block(&workspace, cx, agent(1)), before);
}

#[gpui::test]
fn prompt_typing_keeps_transcript_concealment_folds(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(
            vec![user("go")],
            vec![assistant(
                "**bold** and `code`\n",
                Some(UiMessagePhase::FinalAnswer),
            )],
        ),
    );
    cx.run_until_parked();

    let editor = active_editor(&workspace, cx);
    let before = concealed_ranges(&workspace, &editor, cx);
    assert!(!before.is_empty());

    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| editor.insert("x", window, cx));
        })
        .expect("type in prompt");
    cx.run_until_parked();

    assert_eq!(concealed_ranges(&workspace, &editor, cx), before);
}

#[gpui::test]
fn plain_assistant_streaming_keeps_existing_concealment_folds(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let original = "**bold** and `code`\n";
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(
            vec![user("go")],
            vec![assistant(original, Some(UiMessagePhase::FinalAnswer))],
        ),
    );
    cx.run_until_parked();

    let editor = active_editor(&workspace, cx);
    let before = concealed_ranges(&workspace, &editor, cx);
    assert!(!before.is_empty());

    feed_edit(&workspace, cx, agent(1), |state| {
        stream_text(state, 1, original.len(), "more plain text\n")
    });
    cx.run_until_parked();

    let after = concealed_ranges(&workspace, &editor, cx);
    assert!(
        before.starts_with(&after) || after.starts_with(&before),
        "streaming must preserve the settled concealment prefix: {before:?} -> {after:?}"
    );
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
        state(
            vec![user("go")],
            vec![assistant(&lines, Some(UiMessagePhase::FinalAnswer))],
        ),
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
        state(
            vec![user("go")],
            vec![assistant(
                &format!("{lines}line 40 of the answer\n"),
                Some(UiMessagePhase::FinalAnswer),
            )],
        ),
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
        state(
            vec![user("my question")],
            vec![assistant("the answer", Some(UiMessagePhase::FinalAnswer))],
        ),
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
                    rho_window::style::USER_MESSAGE_SCALE,
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
        state(vec![user("go")], vec![assistant("**bold text**", None)]),
    );

    for _ in 0..64 {
        cx.run_until_parked();
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(20));
    }
    feed_edit(&replaced, cx, agent(2), |state| {
        stream_text(state, 1, 0, "plain text")
    });
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
        state(
            vec![user("go")],
            vec![assistant(
                "target **bold text**",
                Some(UiMessagePhase::FinalAnswer),
            )],
        ),
    );

    let after_unclosed_fence = test_workspace(cx);
    feed_frame(
        &after_unclosed_fence,
        cx,
        agent(2),
        state(
            vec![
                user("first"),
                assistant("```text\nunclosed", Some(UiMessagePhase::FinalAnswer)),
                user("next"),
            ],
            vec![assistant(
                "target **bold text**",
                Some(UiMessagePhase::FinalAnswer),
            )],
        ),
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
        state(
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
        ),
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
        state(
            vec![user("first")],
            vec![assistant(
                "settled **bold text**",
                Some(UiMessagePhase::FinalAnswer),
            )],
        ),
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

    feed_edit(&workspace, cx, agent(1), |state| {
        replace_block(state, 2, user("second"));
        replace_block(state, 3, assistant("new response", None));
    });

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
        state(
            vec![user("first line\nsecond line\nthird line")],
            vec![assistant(
                "## Heading\n\n**bold** answer\n",
                Some(UiMessagePhase::FinalAnswer),
            )],
        ),
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
                        rho_window::style::USER_MESSAGE_SCALE,
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
        state(vec![user("go")], vec![assistant("", None)]),
    );

    let message = "Here is **bold** text, `code`, and **more strong** words.\n";
    let mut sent = 0;
    while sent < message.len() {
        let mut next = (sent + 3).min(message.len());
        while !message.is_char_boundary(next) {
            next += 1;
        }
        feed_edit(&workspace, cx, agent(1), |state| {
            stream_text(state, 1, sent, &message[sent..next])
        });
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
        state(
            vec![
                user("go"),
                assistant("first", Some(UiMessagePhase::Commentary)),
            ],
            vec![assistant("", None)],
        ),
    );
    feed_edit(&workspace, cx, agent(1), |state| {
        stream_text(state, 2, 0, "second")
    });

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
        state(
            vec![
                user("first"),
                assistant("", Some(UiMessagePhase::FinalAnswer)),
                user("second"),
            ],
            Vec::new(),
        ),
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
        state(vec![user("last user")], Vec::new()),
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
        state(vec![user("first")], vec![assistant("second", None)]),
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
        state(history, vec![assistant(initial, None)]),
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

    feed_edit(&workspace, cx, agent(1), |state| {
        stream_text(
            state,
            active_index,
            initial.len(),
            "\n\n## New heading\n\n**new bold**",
        )
    });

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

#[gpui::test]
fn a_verdict_on_one_device_reaches_the_other_after_cells_available(cx: &mut TestAppContext) {
    // Two GUIs on one desk: the first deals a verdict, the daemon accepts
    // it, and the second sees it only because the poke made it sync.
    let mut desk = DeskFixture::new();
    let note = desk.note(None, "Shared card");
    desk.set(
        note.clone(),
        rho_desk::cells::Property::DeferUntil(Some(rho_desk::cells::Timestamp {
            unix_ms: 1_577_836_800_000,
            precision: rho_desk::cells::TimestampPrecision::Day,
        })),
    );

    let first = test_workspace(cx);
    let second = test_workspace(cx);
    for workspace in [&first, &second] {
        workspace
            .update(cx, |workspace, window, cx| {
                story::feed(workspace, HostId::default(), desk.synced(), window, cx);
                workspace.pull_card(window, cx);
                workspace.take_host_messages_for_test(HostId::default());
            })
            .unwrap();
    }
    cx.run_until_parked();

    cx.dispatch_action(*first, crate::DashboardDealDone);
    cx.run_until_parked();
    let mutation = first
        .update(cx, |workspace, _, _| {
            take_desk_mutation(workspace, HostId::default()).expect("verdict mutation")
        })
        .unwrap();
    first
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::DeskMutationAccepted {
                    stamp: mutation.stamp,
                },
                window,
                cx,
            );
        })
        .unwrap();

    // The daemon now holds the verdict; the second device is only poked.
    desk.store.apply_mutation(&mutation).unwrap();
    let frontier = desk.store.version().clone();
    second
        .update(cx, |workspace, window, cx| {
            assert_eq!(
                workspace
                    .desk_cells_snapshot_for_test(HostId::default())
                    .into_iter()
                    .find(|node| node.id == note)
                    .map(|node| node.state),
                Some(rho_desk::cells::State::Open),
                "the poke has not arrived yet"
            );
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::DeskCellsAvailable { frontier },
                window,
                cx,
            );
            let sync = workspace
                .take_host_messages_for_test(HostId::default())
                .into_iter()
                .any(|message| matches!(message, rho_ui_proto::ClientMessage::DeskSync { .. }));
            assert!(sync, "a poke asks for the delta rather than carrying it");
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            assert_eq!(
                workspace
                    .desk_cells_snapshot_for_test(HostId::default())
                    .into_iter()
                    .find(|node| node.id == note)
                    .map(|node| node.state),
                Some(rho_desk::cells::State::Done),
                "the verdict from the other device is visible here"
            );
        })
        .unwrap();
}

#[gpui::test]
fn unnamed_legacy_gpt_quota_is_visible_to_the_status_line(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::ChatGptUsage {
                    used_percent: 60.,
                    reset_at_unix: 1,
                },
                window,
                cx,
            );
            assert_eq!(
                workspace.merged_quota_summaries_for_test(),
                vec![rho_ui_proto::QuotaSummary {
                    model: "gpt".to_owned(),
                    auth_namespace: None,
                    remaining_percent: 40,
                    burn_10m: 0,
                    burn_2h: 0,
                    burn_1d: 0,
                    burn_3d: 0,
                    reset_at_unix: Some(1),
                }]
            );
        })
        .unwrap();
}

#[gpui::test]
fn f21_steps_through_three_surfaces(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["one", "two", "three"], window, cx);
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "f21");
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "two");
        })
        .unwrap();
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.step_surface_back_for_test(window, cx)
        })
        .unwrap();

    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "three");
        })
        .unwrap();
}

/// Opening a surface that is already behind the reader is an ordinary open:
/// where they were goes on the stack, and the older entry for the place they
/// went to is passed over rather than walked back through twice.
#[gpui::test]
fn opening_a_surface_already_in_history_leaves_it_reachable_once(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["one", "two", "three"], window, cx);
            workspace.open_named_surface_for_test("three", cx);
            assert_eq!(workspace.current_surface_name_for_test(), "three");
            assert_eq!(
                workspace.surface_history_for_test(),
                vec!["one".to_owned(), "two".to_owned()],
                "back returns to where the reader was, and three is not behind them twice"
            );
        })
        .unwrap();
}

/// Dealing or opening from the overview the surface already on the glass is
/// not a place to come back to: it pushes nothing, however often it happens.
#[gpui::test]
fn dealing_the_surface_already_shown_pushes_nothing(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["one", "two", "three"], window, cx);
            workspace.step_surface_back_for_test(window, cx);
            assert_eq!(workspace.current_surface_name_for_test(), "two");
            workspace.show_current_history_for_test(rho_journal::SurfaceShowMethod::Deal, cx);
            assert_eq!(
                workspace.surface_history_for_test(),
                vec!["three".to_owned()]
            );

            workspace.show_current_history_for_test(rho_journal::SurfaceShowMethod::Overview, cx);
            assert_eq!(
                workspace.surface_history_for_test(),
                vec!["three".to_owned()],
                "showing what is already shown never grows the stack"
            );
        })
        .unwrap();
}

/// `q` on a standalone draft closes it and goes back one, like `q` on
/// anything else: the composer is not a special case with its own exit.
/// Home is the floor under it, not the answer to closing it.
#[gpui::test]
fn q_closes_a_standalone_draft_and_goes_back(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["previous"], window, cx);
            workspace.select_agent(None, window, cx);
            assert_eq!(workspace.current_surface_name_for_test(), "draft");
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "q");
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "previous");
            assert!(
                !workspace
                    .surface_history_for_test()
                    .contains(&"draft".to_owned()),
                "the closed draft is still somewhere back can land"
            );
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "f24");
    workspace
        .update(cx, |workspace, _, _| {
            assert_ne!(
                workspace.current_surface_name_for_test(),
                "draft",
                "the home key resurrected the closed draft"
            )
        })
        .unwrap();
}

/// A discarded draft must not stay in surface history: `q` on it takes it
/// out, and stepping back must not walk into the thing that was thrown
/// away. Written on a heading draft when `r` on a map row was what opened
/// one; a draft is now opened from the composer, and the rule is the same.
#[gpui::test]
fn q_discards_a_draft_from_surface_history(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    desk.note(None, "unstaffed heading");
    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.configure_surface_history_for_test(&["previous"], window, cx);
            workspace.select_agent(None, window, cx);
            assert_eq!(workspace.current_surface_name_for_test(), "draft");
        })
        .unwrap();
    cx.run_until_parked();

    cx.simulate_keystrokes(*workspace, "q");
    workspace
        .update(cx, |workspace, _, _| {
            assert_ne!(workspace.current_surface_name_for_test(), "draft");
            assert!(
                !workspace
                    .surface_history_for_test()
                    .contains(&"draft".to_owned()),
                "discarded draft remained in surface history"
            );
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "f24");
    workspace
        .update(cx, |workspace, _, _| {
            assert_ne!(
                workspace.current_surface_name_for_test(),
                "draft",
                "history reopened the discarded draft"
            )
        })
        .unwrap();
}

/// Discarding a draft leaves the history cursor where it was, on the
/// surface the reader was actually reading.
#[gpui::test]
fn discarding_a_draft_preserves_non_draft_history_cursor(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    desk.note(None, "unstaffed heading");
    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.configure_surface_history_for_test(&["current"], window, cx);
            workspace.select_agent(None, window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "q");
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(
                workspace.current_surface_name_for_test(),
                "current",
                "discarding the draft left the reader somewhere other than what was under it"
            );
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "f24");
    workspace
        .update(cx, |workspace, _, _| {
            assert_ne!(
                workspace.current_surface_name_for_test(),
                "draft",
                "forward walked back into the discarded draft"
            );
        })
        .unwrap();
}

/// Closing after stepping back lands on the next one back. There is no
/// forward: a stack the reader has walked past is a stack that is shorter,
/// which is what "history is a stack" costs and buys.
#[gpui::test]
fn q_after_stepping_back_lands_on_the_next_surface_back(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["one", "two", "three"], window, cx);
            workspace.step_surface_back_for_test(window, cx);
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "q");

    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "three");
            // Nothing is behind three: one was left behind by the step back
            // and two was closed.
            assert!(workspace.surface_history_for_test().is_empty());
        })
        .unwrap();
}

#[gpui::test]
fn typing_does_not_reorder_history(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["one", "two", "three"], window, cx);
            workspace.step_surface_back_for_test(window, cx);
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "i x escape");
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "two");
            assert_eq!(
                workspace.surface_history_for_test(),
                vec!["three".to_owned()]
            );
        })
        .unwrap();
}

#[gpui::test]
fn q_closes_current_surface_and_reveals_previous(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["current", "previous"], window, cx);
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "q");

    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "previous");
        })
        .unwrap();
}

#[gpui::test]
fn q_on_last_surface_lands_on_home(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["only"], window, cx);
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "q");

    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "home");
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "f24");
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(
                workspace.current_surface_name_for_test(),
                "home",
                "the home key resurrected a closed surface"
            )
        })
        .unwrap();
}

#[gpui::test]
fn q_on_home_is_a_no_op(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    // Home is the floor: there is nothing under it to reveal.
    let workspace = test_workspace(cx);
    cx.simulate_keystrokes(*workspace, "q");
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "home");
        })
        .unwrap();
}

/// The key table in `SLACK-DESIGN.md`, one assertion per row. A key that
/// stops being bound is a documentation bug as much as a behaviour one, so
/// this is the test that fails when the two drift apart.
#[gpui::test]
fn every_key_in_the_slack_table_is_bound(cx: &mut TestAppContext) {
    use gpui::{KeyContext, Keystroke};

    cx.update(bind_test_keymaps);
    cx.update(|cx| {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let routes = |key: &str, contexts: &[KeyContext]| {
            let stroke = Keystroke::parse(key).unwrap();
            keymap
                .bindings_for_input(&[stroke], contexts)
                .0
                .first()
                .map(|binding| binding.action().name())
        };
        // Both normal modes, because a helix reader is reading the same
        // conversation and the table does not have a second column.
        let surface = |name: &str, mode: &str| {
            [
                KeyContext::parse("RhoGui").unwrap(),
                KeyContext::parse(name).unwrap(),
                KeyContext::parse(&format!(
                    "Editor VimControl vim_mode={mode} vim_operator=none"
                ))
                .unwrap(),
            ]
        };
        for mode in ["normal", "helix_normal"] {
            let list = surface("RhoSlackList", mode);
            assert_eq!(routes("enter", &list), Some("rho_gui::SlackOpenRow"));
            assert_eq!(routes("s", &list), Some("rho_gui::SlackSearch"));
            assert_eq!(routes("shift-n", &list), Some("rho_gui::SlackNextUnread"));
            assert_eq!(routes("m", &list), Some("rho_gui::SlackMarkReadBefore"));
            assert_eq!(routes("q", &list), Some("rho_gui::SurfaceClose"));
            // The composer and the rewrite belong to a conversation. On the
            // list the keys go back to vim, the way they do on every other
            // surface that has no use for them: a binding that does nothing
            // is worse than no binding.
            assert_ne!(routes("i", &list), Some("rho_gui::SlackCompose"));
            assert_ne!(routes("e", &list), Some("rho_gui::SlackEditMessage"));
            // Reacting belongs to a message, and the list has none.
            assert_ne!(routes("r", &list), Some("rho_gui::SlackReactTo"));

            let conversation = surface("RhoSlackConversation", mode);
            assert_eq!(
                routes("enter", &conversation),
                Some("rho_gui::SlackOpenRow")
            );
            assert_eq!(routes("i", &conversation), Some("rho_gui::SlackCompose"));
            assert_eq!(routes("s", &conversation), Some("rho_gui::SlackSearch"));
            assert_eq!(routes("r", &conversation), Some("rho_gui::SlackReactTo"));
            assert_eq!(
                routes("e", &conversation),
                Some("rho_gui::SlackEditMessage")
            );
            assert_eq!(
                routes("shift-n", &conversation),
                Some("rho_gui::SlackNextUnread")
            );
            assert_eq!(routes("q", &conversation), Some("rho_gui::SurfaceClose"));
            // Out of a thread and back to the channel it was opened from,
            // which is the same key that walks back anywhere else in rho.
            assert_eq!(
                routes("ctrl-k", &conversation),
                Some("rho_gui::SurfaceBack")
            );
        }

        let composing = [
            KeyContext::parse("RhoGui").unwrap(),
            KeyContext::parse("RhoSlackConversation").unwrap(),
            KeyContext::parse("Editor vim_mode=insert").unwrap(),
        ];
        assert_eq!(routes("enter", &composing), Some("rho_gui::SubmitPrompt"));
        assert_eq!(routes("shift-enter", &composing), Some("editor::Newline"));
        assert_eq!(routes("up", &composing), Some("rho_gui::SlackEditLast"));
        assert_eq!(
            routes("escape", &composing),
            Some("rho_gui::SlackCancelEdit")
        );
    });
}

#[gpui::test]
fn a_slack_card_is_read_with_the_conversations_own_keys(cx: &mut TestAppContext) {
    use gpui::{KeyContext, Keystroke};

    cx.update(bind_test_keymaps);
    cx.update(|cx| {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let routes = |key: &str, contexts: &[KeyContext]| {
            let stroke = Keystroke::parse(key).unwrap();
            keymap
                .bindings_for_input(&[stroke], contexts)
                .0
                .first()
                .map(|binding| binding.action().name())
        };
        // A Slack card is read with the conversation's own keys: deal mode
        // used to take `d`, `s` and `i` from it, and the verdicts are in the
        // transient now.
        let reading = [
            KeyContext::parse("RhoGui").unwrap(),
            KeyContext::parse("RhoSlackConversation").unwrap(),
            KeyContext::parse("Editor VimControl vim_mode=normal vim_operator=none").unwrap(),
        ];
        assert_eq!(routes("i", &reading), Some("rho_gui::SlackCompose"));
        assert_eq!(routes("s", &reading), Some("rho_gui::SlackSearch"));
        assert_eq!(routes("e", &reading), Some("rho_gui::SlackEditMessage"));
        // `shift-n` walks the unread conversations. `n` is left to the
        // search the reader just ran in the transcript.
        assert_eq!(
            routes("shift-n", &reading),
            Some("rho_gui::SlackNextUnread")
        );

        // In the composer, `up` is the Slack habit of editing the last
        // message and `escape` cancels an open edit. Both fall through to
        // the editor's own answer when there is nothing to edit.
        let composing = [
            KeyContext::parse("RhoGui").unwrap(),
            KeyContext::parse("RhoSlackConversation").unwrap(),
            KeyContext::parse("Editor vim_mode=insert").unwrap(),
        ];
        assert_eq!(routes("up", &composing), Some("rho_gui::SlackEditLast"));
        assert_eq!(
            routes("escape", &composing),
            Some("rho_gui::SlackCancelEdit")
        );
        assert_eq!(routes("enter", &composing), Some("rho_gui::SubmitPrompt"));
        // A second line is written with shift-enter; without the binding the
        // prompt's own `enter` would take the key and send the half message.
        assert_eq!(routes("shift-enter", &composing), Some("editor::Newline"));

        // With the completion menu open the same keys are its: `enter`
        // takes the name being offered instead of posting half of it, and
        // `up` walks the list instead of reaching for the last message.
        let completing = [
            KeyContext::parse("RhoGui").unwrap(),
            KeyContext::parse("RhoSlackConversation").unwrap(),
            KeyContext::parse("Editor vim_mode=insert showing_completions").unwrap(),
        ];
        assert_eq!(
            routes("enter", &completing),
            Some("editor::ConfirmCompletion")
        );
        assert_eq!(
            routes("up", &completing),
            Some("editor::ContextMenuPrevious")
        );
    });
}

#[gpui::test]
fn a_verdict_ends_the_deal_even_when_the_node_went_quiet(cx: &mut TestAppContext) {
    // Reading a Slack conversation elsewhere quiets the thread while the
    // deal is still open: its node loses the facts the card was drawn from.
    // The verdict must still land and still end the deal.
    let mut desk = DeskFixture::new();
    let dealt = desk.due_note(None, "can you look at the deploy?");

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.pull_card(window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, window, cx| {
            assert!(workspace.dashboard_deal_mode_for_test(cx));
            desk.set(dealt, rho_desk::cells::Property::DeferUntil(None));
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();

    cx.dispatch_action(*workspace, crate::DashboardDealDone);
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, window, cx| {
            let stamp = take_desk_mutation(workspace, HostId::default())
                .expect("verdict mutation")
                .stamp;
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::DeskMutationAccepted { stamp },
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, cx| {
            assert!(
                !workspace.dashboard_deal_mode_for_test(cx),
                "a verdict on a card that went quiet still ends the deal"
            );
        })
        .unwrap();
}

/// A `thread` node is the thread's identity and its verdicts; what the
/// card says comes from the Slack mirror. With nothing in the mirror there
/// is nothing to say, so the node is not dealt as a blank card.
#[gpui::test]
fn a_thread_node_without_its_mirror_is_not_dealt(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let dealable = desk.due_note(None, "a note that does want attention");
    desk.thread_row(None, "C1", "500.0");

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.pull_card(window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(
                workspace.current_deal_card_for_test(cx).map(|card| card.0),
                Some(crate::dashboard::DealCardId {
                    host: HostId::default(),
                    node_id: dealable,
                }),
                "the only card is the one with something to say"
            );
        })
        .unwrap();
}

/// Whether a Slack unit is a card is the crate's answer and nothing else:
/// the reason it sends is already the join of rho's own cursor and Slack's
/// read mark, so a unit it has stopped asking about is not a card here,
/// whatever the store holds. The backlog command, which is the one place
/// that acts on every card at once, sees only what is still open.
#[gpui::test]
fn a_slack_unit_the_crate_has_stopped_asking_about_is_not_a_card(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let open = desk.thread_row(None, "C1", "500.0");
    let settled = desk.thread_row(None, "C1", "600.0");

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            let sources = desk
                .slack_sources()
                .into_iter()
                .map(|source| match source.unit.thread.as_deref() {
                    // Dealt with: `d` here, or read on the phone. Either
                    // way the crate has stopped asking.
                    Some("600.0") => crate::desk_view::SlackSource {
                        reason: None,
                        ..source
                    },
                    _ => source,
                })
                .collect();
            workspace.set_slack_sources_for_test(HostId::default(), sources, window, cx);
            let cards = workspace.dashboard.open_thread_cards();
            assert_eq!(
                cards
                    .iter()
                    .map(|(card, _)| card.node_id.clone())
                    .collect::<Vec<_>>(),
                vec![open],
                "the thread nothing is asking about is not a card"
            );
            // The closed one is still findable, which is what lets a newer
            // message rebind and reopen it.
            assert_eq!(
                workspace
                    .dashboard
                    .thread_card_id(&crate::dashboard::SlackUnit {
                        workspace: "acme".to_owned(),
                        channel: "C1".to_owned(),
                        thread: Some("600.0".to_owned()),
                    })
                    .map(|card| card.node_id),
                Some(settled)
            );
        })
        .unwrap();
}

/// Creation: `n` from anywhere, the area always asked, and the labels the
/// thing in context carries offered first, so Enter alone files the new
/// thing where the reader already is. The area is a label: a thing is
/// placed by what it carries and carries no parent.
#[gpui::test]
fn new_note_takes_the_label_the_thing_in_context_carries(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    let elsewhere = desk.label("elsewhere");
    let here = desk.label("here");
    let context = desk.note(None, "the area in view");
    desk.labelled(context.clone(), here.clone());

    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.focus_tree_node_for_test(HostId::default(), context.clone(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            let areas = workspace.areas_for_test(cx);
            assert!(areas.contains(&("here".to_owned(), "label".to_owned())));
            assert!(areas.contains(&("elsewhere".to_owned(), "label".to_owned())));
            assert!(
                !areas.iter().any(|(path, _)| path == "the area in view"),
                "a note is not an area a thing can be filed under: {areas:?}"
            );
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();

    // `space n n`, then bare Enter on the offered context row.
    cx.simulate_keystrokes(*workspace, "space n n");
    cx.run_until_parked();
    cx.dispatch_action(*workspace, crate::MinibufferConfirm);
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            let mutation =
                take_desk_mutation(workspace, HostId::default()).expect("new note mutation");
            assert!(
                mutation.writes.iter().any(|write| write.property
                    == rho_desk::cells::Property::Labeled {
                        label: here.clone(),
                        present: true,
                    }),
                "Enter on the first row gives the note the label the cursor's thing carries"
            );
            assert!(
                !mutation.writes.iter().any(|write| matches!(
                    write.property,
                    rho_desk::cells::Property::Parent(Some(_))
                )),
                "nothing is placed by a parent: {:?}",
                mutation.writes
            );
            let _ = &elsewhere;
            let _ = &context;
        })
        .unwrap();
}

/// Create from here, on a Slack message. The reader is looking at a
/// thread that carries a label; `n n` and Enter make the note without
/// asking where it goes: it takes the labels the message carries, which is
/// the same place, and says it is about the message. About is provenance —
/// the label is what puts the note on the map.
#[gpui::test]
fn a_note_made_from_a_slack_message_is_about_it_and_wears_its_labels(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    let label = desk.label("rho");
    let node = desk.thread_row(None, "C1", "600.0");
    desk.labelled(node.clone(), label.clone());

    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.set_slack_sources_for_test(
                HostId::default(),
                desk.slack_sources(),
                window,
                cx,
            );
            // The message is the only card, so Home's cursor is on it: the
            // reader is looking at the thread.
            workspace.sync_tree_dashboard(HostId::default(), window, cx);
            // The reader is on the message. The Slack conversation surface
            // wants a live session, which this harness has none of; the
            // message's own surface names the same node, which is what
            // `here` reads.
            workspace.open_note(HostId::default(), node.clone(), window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(
                workspace.context_area(cx),
                Some((HostId::default(), node.clone())),
                "the message the reader is on is what `here` means"
            );
        })
        .unwrap();

    // `space n n`, then bare Enter: the first row is `here`.
    cx.simulate_keystrokes(*workspace, "space n n");
    cx.run_until_parked();
    cx.dispatch_action(*workspace, crate::MinibufferConfirm);
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            let mutation =
                take_desk_mutation(workspace, HostId::default()).expect("new note mutation");
            assert!(
                mutation
                    .writes
                    .iter()
                    .any(|write| write.property == rho_desk::cells::Property::About(node.clone())),
                "the note says what it came from: {:?}",
                mutation.writes
            );
            assert!(
                mutation.writes.iter().any(|write| write.property
                    == rho_desk::cells::Property::Labeled {
                        label: label.clone(),
                        present: true,
                    }),
                "and is placed where the message is, without being asked: {:?}",
                mutation.writes
            );
            assert!(
                !mutation.writes.iter().any(|write| matches!(
                    write.property,
                    rho_desk::cells::Property::Parent(Some(_))
                )),
                "nothing is placed by a parent: {:?}",
                mutation.writes
            );
        })
        .unwrap();
}

/// The finder's candidate source over a real desk tree: every node arrives
/// as its full path, and submitting one opens the surface that path names.
#[gpui::test]
fn find_offers_every_node_as_a_path_and_opens_the_one_chosen(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    // A page node makes the dashboard look at the browser, which is a
    // global rather than a field.
    cx.update(|cx| {
        let dir = std::env::temp_dir();
        rho_browser::init(&dir, dir.join("rho-gui-test-nonexistent-browser.sock"), cx);
    });
    let agent_id = agent(31);
    let page_id = rho_browser::PageId(uuid::Uuid::from_u128(7));

    let mut desk = DeskFixture::new();
    let root = desk.note(None, "nixos");
    let topic = desk.note(Some(root), "poco on linux");
    desk.agent_row(topic.clone(), agent_id);
    // With no live browser record, a page row says only that it is one.
    desk.page_row(topic, page_id);

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(
                    vec![story::UiAgentHead {
                        story_pos: story::UiStoryPos(1),
                        spawn_name: Some("warm agent".into()),
                        ..ui_head(agent_id)
                    }],
                    40,
                ),
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, cx| {
            let paths = workspace
                .find_candidates(cx)
                .into_iter()
                .map(|candidate| (candidate.path, candidate.kind))
                .collect::<Vec<_>>();
            for expected in [
                ("nixos".to_owned(), "topic"),
                ("nixos › poco on linux".to_owned(), "topic"),
                ("nixos › poco on linux › warm agent".to_owned(), "agent"),
                ("nixos › poco on linux › page".to_owned(), "page"),
            ] {
                assert!(
                    paths.contains(&expected),
                    "{expected:?} missing from {paths:?}"
                );
            }
            // The whole point of the path: initials across segments find it.
            assert_eq!(
                crate::find::rank(
                    &paths
                        .iter()
                        .map(|(path, _)| (path.clone(), 0))
                        .collect::<Vec<_>>(),
                    "nixpocowarm",
                )
                .first()
                .map(|index| paths[*index].0.clone()),
                Some("nixos › poco on linux › warm agent".to_owned())
            );
        })
        .unwrap();

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_find(window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "n i x p o c o w a r m");
    cx.run_until_parked();
    cx.dispatch_action(*workspace, crate::MinibufferConfirm);
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert!(
                workspace
                    .current_surface_name_for_test()
                    .starts_with("warm agent"),
                "enter on an agent's path opens that agent, not {}",
                workspace.current_surface_name_for_test()
            );
        })
        .unwrap();
}

/// The finder's chord must survive the bundled keymaps: `ctrl-shift-f` is
/// Zed's project search and vim binds a great deal at this depth.
#[gpui::test]
fn the_find_chord_wins_against_the_bundled_keymaps(cx: &mut TestAppContext) {
    use gpui::{KeyContext, Keystroke};

    cx.update(bind_test_keymaps);
    cx.update(|cx| {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let routes = |key: &str, contexts: &[KeyContext]| {
            let stroke = Keystroke::parse(key).unwrap();
            keymap
                .bindings_for_input(&[stroke], contexts)
                .0
                .first()
                .map(|binding| binding.action().name())
        };
        for surface in ["RhoGuiDashboard", "RhoSlackConversation", "RhoGuiAgent"] {
            let contexts = [
                KeyContext::parse("RhoGui").unwrap(),
                KeyContext::parse(surface).unwrap(),
                KeyContext::parse("Editor VimControl vim_mode=normal vim_operator=none").unwrap(),
            ];
            assert_eq!(
                routes("ctrl-shift-f", &contexts),
                Some("rho_gui::FindNode"),
                "find must reach the prompt from {surface}"
            );
        }
    });
}

/// A note is one text, not one line: the body runs as long as it wants and
/// the first line is what everything else calls it.
#[gpui::test]
fn a_notes_title_is_its_first_line_and_the_body_is_the_note(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    let root = desk.note(None, "nixos");
    desk.note(
        Some(root),
        "poco on linux\nthe screen is the hard part\nand the modem after it",
    );

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, cx| {
            let paths = workspace
                .find_candidates(cx)
                .into_iter()
                .map(|candidate| candidate.path)
                .collect::<Vec<_>>();
            assert!(
                paths.contains(&"nixos › poco on linux".to_owned()),
                "the path is the first line, not the whole note: {paths:?}"
            );
            assert!(
                !paths.iter().any(|path| path.contains("hard part")),
                "the body never reaches a path: {paths:?}"
            );
        })
        .unwrap();
}

/// The note surface: the body is the node's own text, and the children hang
/// under it, so a note is read where it lives rather than on the map.
#[gpui::test]
fn a_note_opens_as_its_own_surface_with_its_children_under_it(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    let topic = desk.note(None, "poco on linux\nthe screen is the hard part");
    let first = desk.note(Some(topic.clone()), "modem firmware");
    let second = desk.note(Some(topic.clone()), "battery calibration");

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, window, cx| {
            assert!(workspace.open_note(HostId::default(), topic.clone(), window, cx));
            assert_eq!(
                workspace.current_surface_key_for_test(),
                crate::pane::SurfaceKey::DeskNode {
                    host: HostId::default(),
                    node_id: topic.clone(),
                }
            );
            assert_eq!(
                workspace.note_children_for_test(HostId::default(), topic),
                vec![first, second]
            );
        })
        .unwrap();
}

/// The thing behind a surface can go while the surface sits in history: a
/// daemon is detached and its agents go with it. Back must not land on a
/// transcript of an agent that no longer exists.
///
/// Two things keep that, and this test asks only for the result. The close
/// paths call the machine's `forget`, which is where the invariant is
/// tested (`rho_window::history`); and an agent's context is that agent's,
/// so when the agent goes the whole arrangement goes with it and there is
/// no stack left to walk. Belt and braces, deliberately: `forget` is one
/// call on every death path so no path has to know which of the two saved
/// it.
#[gpui::test]
fn back_never_lands_on_a_transcript_whose_daemon_is_gone(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let agent_id = agent(91);
    let mut desk = DeskFixture::new();
    let topic = desk.note(None, "nixos");
    desk.agent_row(topic, agent_id);

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(
                    vec![story::UiAgentHead {
                        story_pos: story::UiStoryPos(1),
                        spawn_name: Some("doomed agent".into()),
                        ..ui_head(agent_id)
                    }],
                    40,
                ),
                window,
                cx,
            );
            workspace.open_agent(agent_id, window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, window, cx| {
            assert_eq!(
                workspace.current_surface_key_for_test(),
                crate::pane::SurfaceKey::Transcript(agent_id)
            );
            // Somewhere else, so the transcript is behind the reader rather
            // than under them.
            workspace.open_home(window, cx);
            assert_eq!(workspace.current_surface_name_for_test(), "home");
            workspace.cmd_host_detach("local", window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.step_surface_back_for_test(window, cx);
            assert_ne!(
                workspace.current_surface_key_for_test(),
                crate::pane::SurfaceKey::Transcript(agent_id),
                "back showed a transcript whose daemon had been detached"
            );
        })
        .unwrap();
}

/// "Notes for this" from a surface that is not a note: the note says it is
/// about the thing on screen and is placed where that thing is, and
/// pressing the key again returns to it rather than making a second one.
/// Nothing is filed under the agent — a thing carries no parent — so
/// About is what the second press finds it by.
#[gpui::test]
fn notes_for_this_makes_a_note_about_the_surfaces_node(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let agent_id = agent(77);
    let mut desk = DeskFixture::new();
    let label = desk.label("nixos");
    let topic = desk.note(None, "the old outline");
    let agent_node = desk.agent_row(topic, agent_id);
    desk.labelled(agent_node.clone(), label.clone());

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(
                    vec![story::UiAgentHead {
                        story_pos: story::UiStoryPos(1),
                        spawn_name: Some("warm agent".into()),
                        ..ui_head(agent_id)
                    }],
                    40,
                ),
                window,
                cx,
            );
            workspace.open_agent(agent_id, window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    let created = workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_notes_for_surface(window, cx);
            let crate::pane::SurfaceKey::DeskNode { node_id, .. } =
                workspace.current_surface_key_for_test()
            else {
                panic!("notes for this opens the note surface");
            };
            let facts = workspace
                .desk_cells
                .facts(HostId::default(), &node_id)
                .expect("the note the key just made");
            assert_eq!(
                facts.about,
                Some(agent_node.clone()),
                "the note says what it is about"
            );
            assert!(
                facts.labels.contains(&label),
                "and is where the agent is: {:?}",
                facts.labels
            );
            assert_eq!(
                facts.parent, None,
                "nothing is placed by a parent, least of all a note for a thing"
            );
            node_id
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_agent(agent_id, window, cx);
            workspace.open_notes_for_surface(window, cx);
            assert_eq!(
                workspace.current_surface_key_for_test(),
                crate::pane::SurfaceKey::DeskNode {
                    host: HostId::default(),
                    node_id: created,
                },
                "the second press opens the note that already exists"
            );
        })
        .unwrap();
}

/// A desk as the daemon would hand it over: cells the client merges, plus a
/// text history per note. Tests build one and send it as `DeskSynced`.
/// A head as `Ready` carries it: the least an agent can say about itself,
/// with the story empty. Tests that care about a title or a running turn
/// set those fields with struct update syntax.
fn ui_head(agent_id: AgentId) -> story::UiAgentHead {
    story::UiAgentHead {
        agent_id,
        story_pos: story::UiStoryPos(0),
        role: rho_ui_proto::AgentRole::default(),
        runtime_kind: story::UiRuntimeKind::Rho,
        workdirs: vec![rho_ui_proto::WorkspaceInfo::UserCheckout {
            repo: "/tmp".into(),
        }],
        spawned_by: story::UiSpawnedBy::Direct,
        parent: None,
        spawn_name: None,
        generated_title: None,
        activity: None,
        turn_running: false,
        created_at: UnixMs(1),
    }
}

/// The whole story of an agent that has finished a turn and asked for the
/// user: the least a card needs to rank as waiting on a reply.
fn story_wanting(agent_id: AgentId, at: UnixMs) -> ConnEvent {
    use story::UiStoryEvent;
    story::story(
        agent_id,
        vec![
            UiStoryEvent::UserMessage {
                text: "go".to_owned(),
                at: UnixMs(0),
            },
            UiStoryEvent::TurnStarted { at: UnixMs(0) },
            UiStoryEvent::Wants {
                want: story::UiAgentWant::Ask,
                summary: None,
                at,
            },
            UiStoryEvent::TurnEnded {
                outcome: story::UiTurnOutcome::Completed,
                at,
            },
        ],
    )
}

struct DeskFixture {
    store: rho_desk::cells::Store,
    bodies: Vec<rho_desk::cells::BodySnapshot>,
    next_node: u64,
    /// The Slack units the fixture made rows for, with the newest message
    /// the mirror would report for each.
    slack_units: Vec<(rho_desk::cells::SlackUnit, String)>,
}

impl DeskFixture {
    /// The text replica namespace the daemon gives this connection.
    const NAMESPACE: u16 = 42;
    const DAEMON_NAMESPACE: u16 = 1;

    fn new() -> Self {
        let device = rho_desk::cells::DeviceId([9; 16]);
        Self {
            store: rho_desk::cells::Store::new(device),
            bodies: Vec::new(),
            next_node: 0,
            slack_units: Vec::new(),
        }
    }

    fn note(&mut self, parent: Option<rho_desk::cells::Id>, text: &str) -> rho_desk::cells::Id {
        self.next_node += 1;
        let id = rho_desk::cells::Id::Note(Self::uuid(self.next_node));
        self.file(id.clone(), parent);
        if !text.is_empty() {
            let mut buffer = text::Buffer::new(
                text::ReplicaId::new(Self::DAEMON_NAMESPACE),
                text::BufferId::new(self.next_node + 1).unwrap(),
                "",
            );
            let operation = rho_desk::TextOperation::from_text(&buffer.edit([(0..0, text)]));
            self.bodies.push(rho_desk::cells::BodySnapshot {
                id: id.clone(),
                operations: vec![operation],
                transactions: Vec::new(),
            });
        }
        id
    }

    /// A note the dealer will deal. A plain note is never dealt, so the
    /// mark that makes it want attention is part of the seed.
    fn due_note(&mut self, parent: Option<rho_desk::cells::Id>, text: &str) -> rho_desk::cells::Id {
        let id = self.note(parent, text);
        self.set(
            id.clone(),
            rho_desk::cells::Property::DeferUntil(Some(rho_desk::cells::Timestamp {
                unix_ms: 1_600_000_000_000,
                precision: rho_desk::cells::TimestampPrecision::Day,
            })),
        );
        id
    }

    /// A Slack thread the user filed. Nothing creates it: the unit is the
    /// id, and filing it is the only fact the store holds.
    fn thread_row(
        &mut self,
        parent: Option<rho_desk::cells::Id>,
        channel: &str,
        thread_ts: &str,
    ) -> rho_desk::cells::Id {
        let unit = rho_desk::cells::SlackUnit {
            workspace: "acme".to_owned(),
            channel: channel.to_owned(),
            thread: Some(thread_ts.to_owned()),
        };
        self.slack_units.push((unit.clone(), thread_ts.to_owned()));
        let id = rho_desk::cells::Id::Slack(unit);
        self.file(id.clone(), parent);
        id
    }

    /// A conversation unit: a direct message or a channel someone mentioned
    /// the user in, which is a card in its own right rather than a thread.
    fn conversation_row(
        &mut self,
        parent: Option<rho_desk::cells::Id>,
        channel: &str,
        newest: &str,
    ) -> rho_desk::cells::Id {
        let unit = rho_desk::cells::SlackUnit {
            workspace: "acme".to_owned(),
            channel: channel.to_owned(),
            thread: None,
        };
        self.slack_units.push((unit.clone(), newest.to_owned()));
        let id = rho_desk::cells::Id::Slack(unit);
        self.file(id.clone(), parent);
        id
    }

    /// What the mirror says about the rows `thread_row` made: every unit
    /// has one message from someone else and nothing handled yet, which is
    /// the state a card is dealt in.
    fn slack_sources(&self) -> Vec<crate::desk_view::SlackSource> {
        self.slack_units
            .iter()
            .map(|(unit, newest)| crate::desk_view::SlackSource {
                unit: unit.clone(),
                title: "any update?".to_owned(),
                newest: rho_desk::cells::SlackTs(newest.clone()),
                newest_from_other: Some(rho_desk::cells::SlackTs(newest.clone())),
                reason: Some(rho_slack::model::Attention::FollowedThread),
            })
            .collect()
    }

    /// A registered project: a label that names a workdir. Projects live
    /// in the store rather than on the wire, so this is where a test says
    /// one exists.
    fn project(&mut self, name: &str, path: &str) -> rho_desk::cells::Id {
        self.next_node += 1;
        let id = rho_desk::cells::Id::Label(Self::uuid(self.next_node));
        self.file(id.clone(), None);
        self.set(id.clone(), rho_desk::cells::Property::Name(name.to_owned()));
        self.set(
            id.clone(),
            rho_desk::cells::Property::Project(Some(rho_desk::cells::Project {
                host: 0,
                path: path.into(),
            })),
        );
        id
    }

    /// A label the user made, and the act of putting one on a row. A label
    /// is placement: the row keeps whatever parent it already had.
    fn label(&mut self, name: &str) -> rho_desk::cells::Id {
        self.next_node += 1;
        let id = rho_desk::cells::Id::Label(Self::uuid(self.next_node));
        self.file(id.clone(), None);
        self.set(id.clone(), rho_desk::cells::Property::Name(name.to_owned()));
        id
    }

    fn labelled(&mut self, id: rho_desk::cells::Id, label: rho_desk::cells::Id) {
        self.set(
            id,
            rho_desk::cells::Property::Labeled {
                label,
                present: true,
            },
        );
    }

    /// An agent the user filed under a note.
    fn agent_row(&mut self, parent: rho_desk::cells::Id, agent_id: AgentId) -> rho_desk::cells::Id {
        let id = rho_desk::cells::Id::Agent(agent_id);
        self.file(id.clone(), Some(parent));
        id
    }

    /// A page the user filed under a note.
    fn page_row(
        &mut self,
        parent: rho_desk::cells::Id,
        page_id: rho_browser::PageId,
    ) -> rho_desk::cells::Id {
        let id = rho_desk::cells::Id::Page(rho_desk::PageId(*page_id.0.as_bytes()));
        self.file(id.clone(), Some(parent));
        id
    }

    /// The two facts a filing is: where the user put it, and when.
    fn file(&mut self, id: rho_desk::cells::Id, parent: Option<rho_desk::cells::Id>) {
        self.next_node += 1;
        let created_at = rho_desk::cells::Timestamp {
            unix_ms: 1_600_000_000_000 + self.next_node as i64,
            precision: rho_desk::cells::TimestampPrecision::Millisecond,
        };
        self.set(id.clone(), rho_desk::cells::Property::Parent(parent));
        self.set(id, rho_desk::cells::Property::CreatedAt(created_at));
    }

    /// A distinct id for every row, however many there are.
    ///
    /// This used to be one byte repeated, which was fine until a desk with
    /// more rows than a byte counts: the counter wrapped, a note was handed
    /// the root's id, and filing it under the root made the root its own
    /// parent. Every walk up the tree then ran forever, which read as the
    /// desk hanging above a certain size rather than as an id collision.
    fn uuid(counter: u64) -> rho_desk::cells::Uuid {
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&counter.to_be_bytes());
        rho_desk::cells::Uuid(bytes)
    }

    fn set(&mut self, id: rho_desk::cells::Id, property: rho_desk::cells::Property) {
        self.store.write(id, property).unwrap();
    }

    fn synced(&self) -> ConnEvent {
        ConnEvent::DeskSynced {
            node_namespace: Self::NAMESPACE,
            delta: self.store.snapshot(),
            bodies: self.bodies.clone(),
        }
    }
}

/// The daemon's answer to the one mutation the GUI just sent.
fn take_desk_mutation(
    workspace: &mut Workspace,
    host: HostId,
) -> Option<rho_desk::cells::CellMutation> {
    workspace
        .take_host_messages_for_test(host)
        .into_iter()
        .find_map(|message| match message {
            rho_ui_proto::ClientMessage::DeskMutationApply { mutation } => Some(mutation),
            _ => None,
        })
}

#[gpui::test]
fn home_reads_as_next_running_and_later(cx: &mut TestAppContext) {
    cx.update(init_test_app);
    let home = cx.add_window(crate::home::HomeView::new);

    // Empty first: the glance still answers, in the deal bar's own words.
    let text = home
        .update(cx, |home, _, cx| {
            let editor = home.editor().clone();
            editor.read(cx).buffer().read(cx).snapshot(cx).text()
        })
        .unwrap();
    assert_eq!(text, "nothing needs attention\n");

    let card = |node: u64| crate::dashboard::DealCardId {
        host: HostId::default(),
        node_id: rho_desk::cells::Id::Note(DeskFixture::uuid(node)),
    };
    let rows = crate::home::HomeRows {
        next: vec![crate::home::HomeRow {
            title: "#design › release date".to_owned(),
            label: "needs reply · 1.9h".to_owned(),
            card: card(1),
            skipped: false,
        }],
        running: vec![crate::home::RunningRow {
            agent_id: agent(1),
            name: "eng-5pha".to_owned(),
            topic: "phone feed".to_owned(),
            elapsed: "12m".to_owned(),
            last_line: "wiring the flick recogniser".to_owned(),
        }],
        later: vec![crate::home::HomeRow {
            title: "#random".to_owned(),
            label: "quiet · 5.4d".to_owned(),
            card: card(2),
            skipped: false,
        }],
    };
    let text = home
        .update(cx, |home, _, cx| {
            home.set_rows(rows, cx);
            let editor = home.editor().clone();
            editor.read(cx).buffer().read(cx).snapshot(cx).text()
        })
        .unwrap();
    assert_eq!(
        text,
        concat!(
            "next\n",
            "  #design › release date  needs reply · 1.9h\n",
            "running\n",
            "  eng-5pha  phone feed  12m   wiring the flick recogniser\n",
            "later\n",
            "  #random  quiet · 5.4d\n",
        ),
        "later comes last so the periphery falls off the bottom"
    );
}

#[gpui::test]
fn a_cold_start_lands_on_home_and_says_what_is_waiting(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    desk.due_note(None, "Ship the release");
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(workspace.current_surface_name_for_test(), "home");
            assert!(
                workspace.current_deal_card_for_test(cx).is_none(),
                "sitting down deals nothing"
            );
        })
        .unwrap();
    let text = buffer_text(&workspace, cx);
    assert!(text.contains("next"), "home text: {text:?}");
    assert!(text.contains("Ship the release"), "home text: {text:?}");
    // The same words the deal bar uses for the same card.
    assert!(text.contains("deferred · woke"), "home text: {text:?}");
}

#[gpui::test]
fn a_pull_opens_the_top_card_and_the_next_pull_passes_over_it(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let first = desk.due_note(None, "Ship the release");
    let second = desk.due_note(None, "Book the venue");
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.pull_card(window, cx);
            assert_eq!(
                workspace.current_deal_card_for_test(cx).map(|card| card.0),
                Some(crate::dashboard::DealCardId {
                    host: HostId::default(),
                    node_id: first.clone(),
                }),
                "one pull opens the most important card"
            );
            // The second pull is a fresh ranking of the same world: the card
            // just read is passed over, so the next one comes up.
            workspace.pull_card(window, cx);
            assert_eq!(
                workspace.current_deal_card_for_test(cx).map(|card| card.0),
                Some(crate::dashboard::DealCardId {
                    host: HostId::default(),
                    node_id: second,
                }),
                "the second pull moves on rather than cycling on one card"
            );
            // Both cards have been passed over now, so there is nothing left
            // to open and the glance is the answer.
            workspace.pull_card(window, cx);
            assert_eq!(workspace.current_surface_name_for_test(), "home");
        })
        .unwrap();
}

#[gpui::test]
fn a_skipped_card_is_marked_on_home_and_comes_back_when_its_source_moves(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let note = desk.due_note(None, "Ship the release");
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.pull_card(window, cx);
            // Nothing else is waiting, so passing over the only card lands
            // on Home.
            workspace.pull_card(window, cx);
            assert_eq!(workspace.current_surface_name_for_test(), "home");
        })
        .unwrap();
    cx.run_until_parked();

    let text = buffer_text(&workspace, cx);
    assert!(
        text.contains("Ship the release"),
        "a skipped card is still open, so Home still shows it: {text:?}"
    );
    assert!(
        text.contains("skipped"),
        "and Home says the reader has passed over it: {text:?}"
    );

    // The note wakes again at a new time: the skip was against the position
    // the card had, and that position has moved.
    workspace
        .update(cx, |workspace, window, cx| {
            desk.set(
                note.clone(),
                rho_desk::cells::Property::DeferUntil(Some(rho_desk::cells::Timestamp {
                    unix_ms: 1_600_086_400_000,
                    precision: rho_desk::cells::TimestampPrecision::Day,
                })),
            );
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.pull_card(window, cx);
            assert_eq!(
                workspace.current_deal_card_for_test(cx).map(|card| card.0),
                Some(crate::dashboard::DealCardId {
                    host: HostId::default(),
                    node_id: note,
                }),
                "the skip is void once the card's own position moves past it"
            );
        })
        .unwrap();
}

#[gpui::test]
fn space_j_pulls_a_card_even_with_a_surface_ahead_in_history(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    desk.due_note(None, "Ship the release");
    desk.due_note(None, "Book the venue");
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            // Two surfaces recorded, so there is a surface behind the
            // reader that a history step would land on.
            workspace.configure_surface_history_for_test(&["one", "two"], window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    let behind = workspace
        .update(cx, |workspace, _, _| {
            workspace.surface_history_for_test()[0].clone()
        })
        .unwrap();

    cx.dispatch_action(*workspace, crate::DealOpen);
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, cx| {
            assert_ne!(
                workspace.current_surface_name_for_test(),
                behind,
                "space j gives the most important thing, never what history holds"
            );
            assert!(
                workspace.current_deal_card_for_test(cx).is_some(),
                "it opened a card: {:?}",
                workspace.current_surface_name_for_test()
            );
        })
        .unwrap();
}
#[gpui::test]
fn a_verdict_on_a_home_row_closes_that_card_and_stays_on_home(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    let top = desk.due_note(None, "Ship the release");
    desk.due_note(None, "Book the venue");
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, window, cx| {
            assert_eq!(workspace.current_surface_name_for_test(), "home");
            // The row under Home's cursor is the card a verdict is about,
            // the same rule as the row under the map's cursor.
            assert!(workspace.open_verdict_transient(window, cx));
        })
        .unwrap();
    cx.dispatch_action(*workspace, crate::DashboardDealDone);
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            let mutation = take_desk_mutation(workspace, HostId::default()).expect("verdict");
            assert!(
                mutation.writes.iter().any(|write| write.id == top
                    && write.property
                        == rho_desk::cells::Property::State(rho_desk::cells::State::Done)),
                "the row under the cursor is what closes"
            );
            assert_eq!(
                workspace.current_surface_name_for_test(),
                "home",
                "and the reader is left on the list they were reading"
            );
        })
        .unwrap();
}

#[gpui::test]
fn enter_on_a_home_row_deals_that_card(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    desk.due_note(None, "First in the queue");
    let second = desk.due_note(None, "Second in the queue");
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    // The cursor picks a row out of the hand rather than taking the top.
    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let offset = snapshot
                    .text()
                    .find("Second in the queue")
                    .expect("second row");
                editor.change_selections(
                    editor::SelectionEffects::no_scroll(),
                    window,
                    cx,
                    |selections| {
                        let offset = editor::MultiBufferOffset(offset);
                        selections.select_ranges([offset..offset]);
                    },
                );
            });
        })
        .expect("place cursor on the second row");

    cx.dispatch_action(*workspace, crate::HomeOpenRow);
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, cx| {
            assert!(
                workspace.dashboard_deal_mode_for_test(cx),
                "a row opens as a deal in every respect"
            );
            let (identity, _) = workspace
                .current_deal_card_for_test(cx)
                .expect("the row was dealt");
            assert_eq!(
                identity,
                crate::dashboard::DealCardId {
                    host: HostId::default(),
                    node_id: second,
                },
                "the dealt card is the row the cursor was on"
            );
        })
        .unwrap();
}

/// A Home row that names a thing can take a verdict, filed or not. An
/// agent the user has not filed has no row on the desk, so the lookup that
/// answers "which node is this agent" found nothing and the row fell
/// through to the surface — and Home is a list that stands for no node, so
/// the row named nothing: `tab` opened nothing and said nothing.
/// Filing an agent is one of the things that key is for, so the row that
/// most needs the menu was the one that could not open it.
/// The draft's start field stops offering an agent the user muted, and the
/// handle still reaches it. A mute is about what rho draws, not about
/// taking the agent away: without the second half of that, muting would be
/// a one-way door with no way back except remembering the handle.
#[gpui::test]
fn a_muted_agent_is_not_offered_as_a_start_target(cx: &mut TestAppContext) {
    let hidden = agent(43);
    let seen = agent(44);
    let mut desk = DeskFixture::new();
    let topic = desk.note(None, "phone feed");
    let hidden_node = desk.agent_row(topic.clone(), hidden);
    desk.agent_row(topic, seen);
    desk.set(
        hidden_node,
        rho_desk::cells::Property::State(rho_desk::cells::State::Muted),
    );

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(vec![ui_head(hidden), ui_head(seen)], 40),
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            let hidden_handle = workspace.registry.agent_id_label(hidden);
            let offered = workspace
                .live_agent_targets()
                .into_iter()
                .map(|candidate| candidate.value)
                .collect::<Vec<_>>();
            assert!(
                offered.contains(&workspace.registry.agent_id_label(seen)),
                "the agent that was not muted is still offered: {offered:?}"
            );
            assert!(
                !offered.contains(&hidden_handle),
                "the muted agent is offered as a target: {offered:?}"
            );
            assert_eq!(
                workspace.registry.agent_by_label(&hidden_handle),
                Some(hidden),
                "and the handle still reaches it, so a mute is not a one-way door"
            );
        })
        .unwrap();
}

/// The other half of the same rule: an agent the user muted, whose turn
/// ended asking for them, is not a card either. The mute and the asking
/// arrive from different places and only the card says which won.
#[gpui::test]
fn a_muted_agent_asking_for_the_user_is_not_a_card(cx: &mut TestAppContext) {
    let hidden = agent(41);
    let seen = agent(42);
    let mut desk = DeskFixture::new();
    let topic = desk.note(None, "phone feed");
    let hidden_node = desk.agent_row(topic.clone(), hidden);
    desk.agent_row(topic, seen);
    desk.set(
        hidden_node,
        rho_desk::cells::Property::State(rho_desk::cells::State::Muted),
    );

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(vec![ui_head(hidden), ui_head(seen)], 40),
                window,
                cx,
            );
            story::feed(
                workspace,
                HostId::default(),
                story_wanting(hidden, UnixMs(1)),
                window,
                cx,
            );
            story::feed(
                workspace,
                HostId::default(),
                story_wanting(seen, UnixMs(1)),
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            assert!(
                workspace
                    .dashboard
                    .agent_card_id(seen)
                    .is_some_and(|card| workspace.dashboard.node_is_open(card)),
                "the agent that was not muted is asking and has its card"
            );
            assert!(
                !workspace
                    .dashboard
                    .agent_card_id(hidden)
                    .is_some_and(|card| workspace.dashboard.node_is_open(card)),
                "the muted agent is carded despite the mute"
            );
        })
        .unwrap();
}

/// The same rule for the other verdict that is not a cursor: an agent
/// snoozed until a time still ahead is off Home even when it starts a turn
/// and says something. Snoozing a working agent left it on Home, so
/// "not until tomorrow" lasted until the agent's next move.
#[gpui::test]
fn a_snoozed_agent_is_off_home_when_its_turn_runs(cx: &mut TestAppContext) {
    let snoozed = agent(51);
    let seen = agent(52);
    let mut desk = DeskFixture::new();
    let topic = desk.note(None, "phone feed");
    let snoozed_node = desk.agent_row(topic.clone(), snoozed);
    desk.agent_row(topic, seen);
    desk.set(
        snoozed_node,
        rho_desk::cells::Property::DeferUntil(Some(rho_desk::cells::Timestamp {
            unix_ms: 4_000_000_000_000,
            precision: rho_desk::cells::TimestampPrecision::Day,
        })),
    );

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(
                    vec![
                        story::UiAgentHead {
                            activity: Some("wiring the flick recogniser".to_owned()),
                            turn_running: true,
                            ..ui_head(snoozed)
                        },
                        story::UiAgentHead {
                            activity: Some("reading the mirror".to_owned()),
                            turn_running: true,
                            ..ui_head(seen)
                        },
                    ],
                    40,
                ),
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();

    let text = workspace
        .update(cx, |workspace, _, cx| {
            let home = workspace.home_view().expect("home is in view");
            home.update(cx, |home, cx| {
                let editor = home.editor().clone();
                editor.read(cx).buffer().read(cx).snapshot(cx).text()
            })
        })
        .unwrap();
    assert!(
        text.contains("reading the mirror"),
        "the agent nobody snoozed is still running on Home: {text}"
    );
    assert!(
        !text.contains("wiring the flick recogniser"),
        "the snoozed agent is drawn on Home while it runs: {text}"
    );
}

/// An agent the user muted is not on Home while it runs. A mute is not a
/// cursor: it is the user saying "not this agent", and a running turn
/// decides how loudly an agent may ask, not whether it may ask at all.
/// Home's running list asked neither, so muting a working agent did
/// nothing a reader could see until the turn ended.
#[gpui::test]
fn a_muted_agent_is_off_home_even_while_its_turn_runs(cx: &mut TestAppContext) {
    let hidden = agent(31);
    let seen = agent(32);
    let mut desk = DeskFixture::new();
    let topic = desk.note(None, "phone feed");
    let hidden_node = desk.agent_row(topic.clone(), hidden);
    desk.agent_row(topic, seen);
    // Muting an agent is a state on its node; that is the whole of it.
    desk.set(
        hidden_node,
        rho_desk::cells::Property::State(rho_desk::cells::State::Muted),
    );

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(
                    vec![
                        story::UiAgentHead {
                            activity: Some("wiring the flick recogniser".to_owned()),
                            turn_running: true,
                            ..ui_head(hidden)
                        },
                        story::UiAgentHead {
                            activity: Some("reading the mirror".to_owned()),
                            turn_running: true,
                            ..ui_head(seen)
                        },
                    ],
                    40,
                ),
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();

    let text = workspace
        .update(cx, |workspace, _, cx| {
            let home = workspace.home_view().expect("home is in view");
            home.update(cx, |home, cx| {
                let editor = home.editor().clone();
                editor.read(cx).buffer().read(cx).snapshot(cx).text()
            })
        })
        .unwrap();
    assert!(
        text.contains("reading the mirror"),
        "the agent the user did not mute is still running on Home: {text}"
    );
    assert!(
        !text.contains("wiring the flick recogniser"),
        "the muted agent is drawn on Home while it runs: {text}"
    );
}

#[gpui::test]
fn the_verdicts_open_over_an_unfiled_running_agents_home_row(cx: &mut TestAppContext) {
    let running = agent(31);
    let mut desk = DeskFixture::new();
    desk.note(None, "phone feed");

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(
                    vec![story::UiAgentHead {
                        activity: Some("wiring the flick recogniser".to_owned()),
                        turn_running: true,
                        ..ui_head(running)
                    }],
                    40,
                ),
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();

    // The cursor on the running row, which is the last one Home draws here.
    let home = workspace
        .update(cx, |workspace, _, _| workspace.home_view())
        .unwrap()
        .expect("home is in view");
    workspace
        .update(cx, |_, _, cx| {
            home.update(cx, |home, cx| {
                let editor = home.editor().clone();
                editor.update(cx, |editor, cx| {
                    let snapshot = editor.display_snapshot(cx);
                    let text = snapshot.buffer_snapshot().text();
                    let row = text
                        .lines()
                        .position(|line| line.contains("wiring the flick recogniser"))
                        .expect("the running row is drawn") as u32;
                    editor.selections.change_with(&snapshot, |selections| {
                        selections.select_ranges([
                            language::Point::new(row, 0)..language::Point::new(row, 0)
                        ]);
                    });
                });
            });
        })
        .unwrap();

    workspace
        .update(cx, |workspace, window, cx| {
            assert_eq!(
                workspace.context_area(cx),
                Some((HostId::default(), rho_desk::cells::Id::Agent(running))),
                "the row names the agent it watches, filed or not"
            );
            assert!(
                workspace.open_verdict_transient(window, cx),
                "tab opens the verdicts over it"
            );
        })
        .unwrap();
}

/// The runtime titles an agent from the first thing said to it, which is a
/// guess; a name is the user saying which agent this is. It is written on
/// the desk, so it reaches the row the same way the labels do, and the
/// runtime's title is what a nameless agent still reads as.
#[gpui::test]
fn a_renamed_running_agents_home_row_reads_the_new_name(cx: &mut TestAppContext) {
    let running = agent(31);
    let mut desk = DeskFixture::new();
    let heading = desk.note(None, "phone feed");
    desk.agent_row(heading, running);

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(
                    vec![story::UiAgentHead {
                        activity: Some("wiring the flick recogniser".to_owned()),
                        turn_running: true,
                        ..ui_head(running)
                    }],
                    40,
                ),
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();
    assert!(
        !buffer_text(&workspace, cx).contains("the recogniser rig"),
        "the agent has no name yet"
    );

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.name_agent(running, "the recogniser rig".to_owned(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    let text = buffer_text(&workspace, cx);
    assert!(
        text.contains("the recogniser rig"),
        "the running row reads the name the user gave it, got {text:?}"
    );

    // Taking the name off gives the runtime's title back rather than
    // leaving the row nameless.
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.name_agent(running, String::new(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    let text = buffer_text(&workspace, cx);
    assert!(
        !text.contains("the recogniser rig  "),
        "the name is gone from the row, got {text:?}"
    );
}

/// Where the user filed an agent is half of which agent it is: two of them
/// on the same kind of work read as one name said twice until the row says
/// where each one lives. The labels are the ones the registry already holds
/// for the agent, so the row costs a lookup and a join.
#[gpui::test]
fn a_labelled_agents_home_row_reads_its_labels(cx: &mut TestAppContext) {
    let running = agent(31);
    let mut desk = DeskFixture::new();
    let heading = desk.note(None, "phone feed");
    let row = desk.agent_row(heading, running);
    let rho = desk.label("rho");
    let cargo = desk.label("cargo");
    desk.labelled(row.clone(), rho);
    desk.labelled(row, cargo);

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(
                    vec![story::UiAgentHead {
                        activity: Some("wiring the flick recogniser".to_owned()),
                        turn_running: true,
                        ..ui_head(running)
                    }],
                    40,
                ),
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();

    let tag = workspace
        .update(cx, |workspace, _, _| {
            workspace.registry.agent_id_label(running)
        })
        .unwrap();
    let text = buffer_text(&workspace, cx);
    assert!(
        text.contains(&format!("{tag} · rho › cargo")),
        "the running row names the agent and then where it is filed, got {text:?}"
    );
}

#[gpui::test]
fn a_running_agents_row_follows_its_last_line(cx: &mut TestAppContext) {
    let running = agent(31);
    let mut desk = DeskFixture::new();
    let heading = desk.note(None, "phone feed");
    desk.agent_row(heading, running);
    let head = |activity: &str| story::UiAgentHead {
        activity: Some(activity.to_owned()),
        turn_running: true,
        ..ui_head(running)
    };

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(vec![head("wiring the flick recogniser")], 40),
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();

    let tag = workspace
        .update(cx, |workspace, _, _| {
            workspace.registry.agent_id_label(running)
        })
        .unwrap();
    let text = buffer_text(&workspace, cx);
    assert!(text.contains("running"), "home text: {text:?}");
    assert!(text.contains(&tag), "home text: {text:?}");
    assert!(text.contains("phone feed"), "home text: {text:?}");
    assert!(
        text.contains("wiring the flick recogniser"),
        "home text: {text:?}"
    );

    // The next line the agent says edits that row and nothing else.
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ready_with(vec![head("unfurl box: background tint")], 40),
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();
    let text = buffer_text(&workspace, cx);
    assert!(
        text.contains("unfurl box: background tint"),
        "home text: {text:?}"
    );
    assert!(
        !text.contains("wiring the flick recogniser"),
        "the row kept a stale line: {text:?}"
    );
}

#[gpui::test]
fn an_empty_queue_lands_on_home(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["current"], window, cx);
            assert_eq!(workspace.current_surface_name_for_test(), "current");
            workspace.pull_card(window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(workspace.current_surface_name_for_test(), "home");
            assert!(
                workspace
                    .message_log_texts(cx)
                    .iter()
                    .any(|message| message.contains("nothing needs attention"))
            );
            // Home says it in the buffer, so the echo area stays quiet and
            // the title still reads "home".
            assert_eq!(workspace.echo_text_for_test(), None);
        })
        .unwrap();
    let text = buffer_text(&workspace, cx);
    assert!(
        text.contains("nothing needs attention"),
        "home text: {text:?}"
    );
    // The cursor sits on the one line there is, never on the blank row
    // after it, which would read as an editable line.
    let editor = active_editor(&workspace, cx);
    let row = workspace
        .update(cx, |_, _, cx| {
            editor.update(cx, |editor, cx| {
                editor
                    .selections
                    .newest::<language::Point>(&editor.display_snapshot(cx))
                    .head()
                    .row
            })
        })
        .unwrap();
    assert_eq!(row, 0, "the cursor left the only line");
}

#[gpui::test]
fn the_phone_feed_is_home_when_there_is_nothing_to_deal(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    cx.simulate_window_resize(*workspace, gpui::size(gpui::px(400.), gpui::px(800.)));
    cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, cx| {
            assert!(workspace.current_deal_card_for_test(cx).is_none());
            assert_eq!(workspace.current_surface_name_for_test(), "home");
            assert!(
                !workspace.phone_has_surface_for_test(&crate::pane::SurfaceKey::Home),
                "home is the feed's own empty state, not a card on the stack"
            );
        })
        .unwrap();
    let text = buffer_text(&workspace, cx);
    assert!(
        text.contains("nothing needs attention"),
        "home text: {text:?}"
    );
}

#[gpui::test]
fn home_starts_with_the_cursor_on_the_first_row(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    let first = desk.due_note(None, "First in the queue");
    desk.due_note(None, "Second in the queue");
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    // No motion: the first Enter deals rather than landing on a heading.
    cx.dispatch_action(*workspace, crate::HomeOpenRow);
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            let (identity, _) = workspace
                .current_deal_card_for_test(cx)
                .expect("the top row was dealt");
            assert_eq!(
                identity,
                crate::dashboard::DealCardId {
                    host: HostId::default(),
                    node_id: first,
                }
            );
        })
        .unwrap();
}

#[gpui::test]
fn new_agent_opens_the_draft_page_and_files_under_the_area(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    // The one registered project is the workdir the draft inherits, and
    // the label the thing in context carries is the area offered first: an
    // area is a label now, and a thing carries no parent.
    let area = desk.project("rho", "/tmp/rho-test-repo");
    let context = desk.due_note(None, "the area in view");
    desk.labelled(context.clone(), area.clone());
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(Vec::new(), 1),
                window,
                cx,
            );
            workspace.force_host_online(HostId::default());
        })
        .unwrap();
    cx.run_until_parked();

    // `space n a`, then bare Enter on the offered context row: the row the
    // cursor is on in Home.
    cx.simulate_keystrokes(*workspace, "space n a");
    cx.run_until_parked();
    cx.dispatch_action(*workspace, crate::MinibufferConfirm);
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "draft");
            assert_eq!(
                workspace.draft_area_for_test(),
                Some((HostId::default(), context.clone())),
                "Enter alone is create-from-here: the thing in view, not a place picked for it"
            );
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();

    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.insert("look at the deploy", window, cx)
            });
        })
        .expect("type the first message");
    cx.dispatch_action(*workspace, crate::SubmitPrompt);
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            let sent = workspace.take_host_messages_for_test(HostId::default());
            assert!(
                sent.iter()
                    .any(|message| matches!(message, rho_ui_proto::ClientMessage::NewAgent { .. })),
                "the draft started an agent"
            );
            // The daemon is never told where to file it: the client writes
            // that fact itself once the agent exists.
            assert_eq!(
                workspace.pending_agent_filing_for_test(),
                Some((HostId::default(), context.clone())),
                "the agent was not filed from where it was made"
            );
            // And what it writes when the agent arrives is the place the
            // thing in view is in, plus where the agent came from.
            let cells = workspace.new_thing_cells(
                HostId::default(),
                Some(&(HostId::default(), context.clone())),
            );
            assert!(
                cells.contains(&rho_desk::cells::Property::Labeled {
                    label: area.clone(),
                    present: true,
                }) && cells.contains(&rho_desk::cells::Property::About(context.clone())),
                "the agent wears the note's label and says what it is about: {cells:?}"
            );
        })
        .unwrap();
}

#[gpui::test]
fn a_cold_start_leaves_no_draft_in_the_timeline(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "home");
            // 1.24: the compose buffer nobody asked for was the only way
            // out of a conversation, because it sat in the timeline from
            // startup. Now it exists only while `n a` is composing.
            assert!(
                workspace
                    .find_surface(|surface| surface.key == crate::pane::SurfaceKey::Draft)
                    .is_none(),
                "a cold start left a draft surface behind"
            );
            assert!(
                !workspace
                    .surface_history_for_test()
                    .contains(&"draft".to_owned())
            );
        })
        .unwrap();
}

#[gpui::test]
fn shift_r_no_longer_writes_a_desk_draft(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    let heading = desk.note(None, "unstaffed heading");
    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.focus_tree_node_for_test(HostId::default(), heading, window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    cx.simulate_keystrokes(*workspace, "shift-r");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert!(
                !workspace.dashboard_has_new_draft_for_test(),
                "shift-r still makes an agent; `space n a` is the one way"
            );
        })
        .unwrap();
}

// `marking_the_backlog_moves_every_cursor_and_undoes_as_one` was here. It
// is in `slack_tests.rs` now, against the fake server: what it asserts is
// the cursor each unit lands on, and the cursor lives in the session.

// `a_done_slack_unit_is_not_reopened_by_anything_slack_replays` was here.
// It asserted the replay property through the store's cursor, and the
// cursor is the Slack mirror's now: the property is
// `a_unit_the_reader_marked_done_stays_closed_until_someone_writes_past_it`
// in rho-slack's model, where both halves of the join are.

/// A snooze leaves the cursor alone, so the messages the user has not
/// handled are still theirs when it ends — and it is not a cursor either:
/// a message arriving during the snooze does not bring the card back. A
/// snooze used to be voided by the next reply, which made "not until
/// Monday" mean "until somebody writes".
#[gpui::test]
fn a_snooze_outlasts_a_newer_message_from_someone_else(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let node = desk.thread_row(None, "C1", "500.0");
    let unit = rho_desk::cells::SlackUnit {
        workspace: "acme".to_owned(),
        channel: "C1".to_owned(),
        thread: Some("500.0".to_owned()),
    };
    let card = crate::dashboard::DealCardId {
        host: HostId::default(),
        node_id: node.clone(),
    };
    let source = |from_other: &str| {
        vec![crate::desk_view::SlackSource {
            unit: unit.clone(),
            title: "any update?".to_owned(),
            newest: rho_desk::cells::SlackTs("600.0".to_owned()),
            newest_from_other: Some(rho_desk::cells::SlackTs(from_other.to_owned())),
            reason: Some(rho_slack::model::Attention::FollowedThread),
        }]
    };

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.set_slack_sources_for_test(HostId::default(), source("600.0"), window, cx);
            assert!(workspace.apply_verdict_for_test(
                HostId::default(),
                &node,
                crate::desk_view::DeskVerdict::Defer {
                    until: rho_desk::cells::Timestamp {
                        unix_ms: 4_000_000_000_000,
                        precision: rho_desk::cells::TimestampPrecision::Day,
                    },
                },
                window,
                cx,
            ));
            let facts = workspace
                .desk_cells
                .facts_of_slack_unit(Some(HostId::default()), &unit)
                .unwrap();
            assert_eq!(
                facts.slack_handled_through, None,
                "a snooze is not a close: what was unhandled is still theirs"
            );
            assert_eq!(
                facts.slack_snoozed_at,
                Some(rho_desk::cells::SlackTs("600.0".to_owned())),
                "and where the unit stood when they looked away is recorded with it"
            );
            assert!(
                workspace.dashboard.node_defer_until(card.clone()).is_some(),
                "the card is put down until the snooze ends"
            );

            workspace.set_slack_sources_for_test(HostId::default(), source("700.0"), window, cx);
            assert!(
                workspace.dashboard.node_defer_until(card.clone()).is_some(),
                "somebody writing during the snooze does not end it"
            );
            assert!(
                workspace.dashboard.node_is_open(card),
                "the unit is still open — the snooze is when it is asked about, not whether"
            );
        })
        .unwrap();
}

// `a_done_on_another_device_closes_the_card_here` was here. A done does
// not cross machines through the store any more: rho's own half of the
// cursor is local to the machine, and what crosses is Slack's read mark,
// which the outbox pushes for exactly this reason. That path is end to end
// in rho-slack's transport tests, in
// `a_mark_from_another_client_lands_here_and_survives_a_reconnect`.

/// A mute is not a cursor. `d` says "up to here", so the next message is
/// news again; `x` says "not this unit", and nothing arriving in it is
/// news until the user opens it. Opening is the only thing that clears it,
/// and it leaves the cursor alone, so what was already read stays read.
#[gpui::test]
fn a_muted_slack_unit_stays_off_home_until_it_is_opened(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let node = desk.conversation_row(None, "D1", "600.0");
    let unit = rho_desk::cells::SlackUnit {
        workspace: "acme".to_owned(),
        channel: "D1".to_owned(),
        thread: None,
    };
    let card = crate::dashboard::DealCardId {
        host: HostId::default(),
        node_id: node.clone(),
    };
    let source = |newest: &str| {
        vec![crate::desk_view::SlackSource {
            unit: rho_desk::cells::SlackUnit {
                workspace: "acme".to_owned(),
                channel: "D1".to_owned(),
                thread: None,
            },
            title: "lunch?".to_owned(),
            newest: rho_desk::cells::SlackTs(newest.to_owned()),
            newest_from_other: Some(rho_desk::cells::SlackTs(newest.to_owned())),
            reason: Some(rho_slack::model::Attention::FollowedThread),
        }]
    };

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.set_slack_sources_for_test(HostId::default(), source("600.0"), window, cx);
            assert!(workspace.dashboard.node_is_open(card.clone()));

            assert!(workspace.apply_verdict_for_test(
                HostId::default(),
                &node,
                crate::desk_view::DeskVerdict::Mute,
                window,
                cx,
            ));
            // The state is the whole of what a mute writes: where the
            // reader is done is rho's own cursor beside Slack's mark, and
            // both of those are the Slack mirror's.
            let facts = workspace
                .desk_cells
                .facts_of_slack_unit(Some(HostId::default()), &unit)
                .unwrap();
            assert_eq!(facts.slack_handled_through, None);
            assert_eq!(
                facts.state,
                rho_desk::cells::State::Muted,
                "and it says the unit itself is not wanted"
            );

            // Someone writes again. A done would be a card here.
            workspace.set_slack_sources_for_test(HostId::default(), source("900.0"), window, cx);
            assert!(
                !workspace.dashboard.node_is_open(card.clone()),
                "the mute is about the unit, not about a cursor"
            );

            // Opening it is the user taking the mute back.
            workspace.open_slack_deal(&unit, window, cx);
            let facts = workspace
                .desk_cells
                .facts_of_slack_unit(Some(HostId::default()), &unit)
                .unwrap();
            assert_eq!(facts.state, rho_desk::cells::State::Open);
            workspace.set_slack_sources_for_test(HostId::default(), source("900.0"), window, cx);
            assert!(
                workspace.dashboard.node_is_open(card),
                "the message that arrived while it was muted is a card again"
            );
        })
        .unwrap();
}

/// Two agents filed under one note are two cards. Taking the note as the
/// topic collapsed them into one, so every agent under a note but the
/// loudest was missing from Home entirely. An errored agent is one of
/// them, and it reads as errored rather than as finished work.
#[gpui::test]
fn every_agent_under_a_note_is_its_own_card(cx: &mut TestAppContext) {
    let asking = agent(51);
    let dead = agent(52);
    let mut desk = DeskFixture::new();
    let note = desk.note(None, "rig agents");
    desk.agent_row(note.clone(), asking);
    desk.agent_row(note, dead);

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(vec![ui_head(asking), ui_head(dead)], 60),
                window,
                cx,
            );
            story::feed(
                workspace,
                HostId::default(),
                story_wanting(asking, UnixMs(1)),
                window,
                cx,
            );
            story::feed(
                workspace,
                HostId::default(),
                story::story(
                    dead,
                    vec![
                        story::UiStoryEvent::UserMessage {
                            text: "go".to_owned(),
                            at: UnixMs(0),
                        },
                        story::UiStoryEvent::TurnStarted { at: UnixMs(0) },
                        story::UiStoryEvent::TurnEnded {
                            outcome: story::UiTurnOutcome::Errored {
                                message: "the deploy script exited 1".to_owned(),
                            },
                            at: UnixMs(1),
                        },
                    ],
                ),
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();

    let text = buffer_text(&workspace, cx);
    assert!(
        text.contains("waiting on reply"),
        "the asking agent: {text:?}"
    );
    assert!(
        text.contains("errored ·"),
        "the dead turn waits on the user and says so: {text:?}"
    );
}

/// `d` on an agent closes its card, and the card comes back only when the
/// story tells something past the cursor the verdict wrote. Being filed
/// under a note does not exempt it: filing says where, not whether.
#[gpui::test]
fn done_on_a_filed_agent_closes_its_card_until_the_story_moves(cx: &mut TestAppContext) {
    let agent_id = agent(42);
    let mut desk = DeskFixture::new();
    let note = desk.note(None, "rig agents");
    desk.agent_row(note, agent_id);

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(vec![ui_head(agent_id)], 50),
                window,
                cx,
            );
            story::feed(
                workspace,
                HostId::default(),
                story_wanting(agent_id, UnixMs(1)),
                window,
                cx,
            );
        })
        .unwrap();
    next_frame(cx, workspace);
    workspace
        .update(cx, |workspace, window, cx| workspace.pull_card(window, cx))
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, window, cx| {
            let (identity, _) = workspace
                .current_deal_card_for_test(cx)
                .expect("the agent asks, so it is dealt");
            assert!(workspace.apply_verdict_for_test(
                HostId::default(),
                &identity.node_id,
                crate::desk_view::DeskVerdict::Done,
                window,
                cx,
            ));
            workspace.pull_card(window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, window, cx| {
            assert_eq!(
                workspace
                    .current_deal_card_for_test(cx)
                    .map(|(id, kind)| (id.node_id, kind)),
                None,
                "the verdict handled everything the story had told"
            );
            // Something new past the cursor is the card again.
            story::feed(
                workspace,
                HostId::default(),
                story::story(
                    agent_id,
                    vec![
                        story::UiStoryEvent::Wants {
                            want: story::UiAgentWant::Ask,
                            summary: None,
                            at: UnixMs(2),
                        },
                        story::UiStoryEvent::TurnEnded {
                            outcome: story::UiTurnOutcome::Completed,
                            at: UnixMs(2),
                        },
                    ],
                ),
                window,
                cx,
            );
        })
        .unwrap();
    next_frame(cx, workspace);
    workspace
        .update(cx, |workspace, window, cx| workspace.pull_card(window, cx))
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, cx| {
            assert!(
                workspace.current_deal_card_for_test(cx).is_some(),
                "the agent asked again past the cursor"
            );
        })
        .unwrap();
}

/// A todo on a Slack unit has to write the cursor it says it moved. The
/// daemon rejects a verdict whose entry states a change the mutation does
/// not make, so a missing write is not a stale card, it is a refusal.
#[gpui::test]
fn a_todo_writes_every_change_its_entry_states(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let node = desk.thread_row(None, "C1", "500.0");

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.set_slack_sources_for_test(
                HostId::default(),
                desk.slack_sources(),
                window,
                cx,
            );
            let (writes, (_, event)) = workspace
                .desk_cells
                .verdict_writes(
                    HostId::default(),
                    &node,
                    crate::desk_view::DeskVerdict::Todo {
                        defer_until: rho_desk::cells::Timestamp {
                            unix_ms: 1_000,
                            precision: rho_desk::cells::TimestampPrecision::Minute,
                        },
                        pace: 3,
                    },
                )
                .expect("the unit has a source, so it can take a verdict");
            let rho_desk::cells::VerdictEvent::Applied { changes, .. } = event else {
                panic!("a dealt verdict is applied");
            };
            for change in &changes {
                let after = change.after.clone().expect("a change writes a fact");
                assert!(
                    writes
                        .iter()
                        .any(|write| write.id == change.id && write.property == after),
                    "the mutation is missing {:?} on {:?}",
                    change.key,
                    change.id
                );
            }
        })
        .unwrap();
}

/// Undoing a mute puts the state back, which is what the mute wrote here.
/// The cursor it moved is the Slack mirror's and goes back with it, which
/// is `undo_handled` in rho-slack; the Slack half, following the thread
/// again, is `undoing_a_discard_follows_the_thread_again` in that crate's
/// transport tests.
#[gpui::test]
fn undoing_a_mute_puts_the_unit_back_as_it_was(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let node = desk.thread_row(None, "C1", "500.0");
    let unit = rho_desk::cells::SlackUnit {
        workspace: "acme".to_owned(),
        channel: "C1".to_owned(),
        thread: Some("500.0".to_owned()),
    };
    let card = crate::dashboard::DealCardId {
        host: HostId::default(),
        node_id: node.clone(),
    };

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.set_slack_sources_for_test(
                HostId::default(),
                desk.slack_sources(),
                window,
                cx,
            );
            let (writes, event) = workspace
                .desk_cells
                .verdict_writes(
                    HostId::default(),
                    &node,
                    crate::desk_view::DeskVerdict::Mute,
                )
                .expect("the unit has a source, so it can take a verdict");
            let stamp = workspace
                .apply_desk_writes(HostId::default(), writes, Some(event), window, cx)
                .expect("the mute is written");
            assert!(!workspace.dashboard.node_is_open(card.clone()));

            let (writes, _) = workspace
                .desk_cells
                .undo_verdict_writes(HostId::default(), &node, stamp)
                .expect("the verdict left an entry to undo");
            workspace.apply_desk_writes(HostId::default(), writes, None, window, cx);
            let facts = workspace
                .desk_cells
                .facts_of_slack_unit(Some(HostId::default()), &unit)
                .unwrap();
            assert_eq!(facts.state, rho_desk::cells::State::Open);
            workspace.set_slack_sources_for_test(
                HostId::default(),
                desk.slack_sources(),
                window,
                cx,
            );
            assert!(
                workspace.dashboard.node_is_open(card),
                "the card comes back exactly as it was"
            );
        })
        .unwrap();
}

/// A thread ignored in another client stops being the user's everywhere:
/// Slack says so on the socket, and the card closes here without a keystroke
/// and without an undo entry, because `shift-u` could not take it back in
/// Slack either.
#[gpui::test]
fn a_thread_unfollowed_in_slack_closes_its_card(cx: &mut TestAppContext) {
    use rho_slack::{ChannelId, Ts, WorkspaceName};

    let mut desk = DeskFixture::new();
    let thread = desk.thread_row(None, "C1", "500.0");
    let unit = crate::slack::store_unit_of(&rho_slack::ThreadKey {
        workspace: WorkspaceName("acme".to_owned()),
        channel: ChannelId("C1".to_owned()),
        thread_ts: Ts("500.0".to_owned()),
    });
    let card = crate::dashboard::DealCardId {
        host: HostId::default(),
        node_id: thread,
    };

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.set_slack_sources_for_test(
                HostId::default(),
                desk.slack_sources(),
                window,
                cx,
            );
            assert!(workspace.dashboard.node_is_open(card.clone()));

            workspace.slack_thread_muted(&unit, window, cx);
            assert!(
                !workspace.dashboard.node_is_open(card),
                "the card closes on Slack's word"
            );
            assert_eq!(
                workspace.verdict_undo_count_for_test(),
                0,
                "a verdict made in another client is not this one's to undo"
            );
        })
        .unwrap();
}

/// A body is text: enter is a newline on the map and on the note surface
/// alike. Both used to fall through to the transcript prompt's submit
/// binding, which ate the key and kept every note one line long.
#[gpui::test]
fn enter_writes_a_newline_into_a_note_body(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    let note = desk.note(None, "first line");

    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    let body = |workspace: &Workspace, node_id, cx: &gpui::App| {
        workspace
            .desk_cells
            .buffer(HostId::default(), node_id)
            .expect("the note has a body")
            .read(cx)
            .text()
    };

    workspace
        .update(cx, |workspace, window, cx| {
            assert!(workspace.open_note(HostId::default(), note.clone(), window, cx));
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "a enter s e c o n d");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(body(workspace, &note, cx), "first line\nsecond");
        })
        .unwrap();
}

/// `n n` from Home. A note is its own surface, so that surface has to come
/// into view: with Home as the landing surface the note and its insert
/// cursor were both behind a surface that never appeared, and the title
/// the reader typed went nowhere. Where it files is the cursor's business
/// and `new_note_files_itself_under_the_area_the_cursor_is_on` has it.
#[gpui::test]
fn a_new_note_from_home_opens_the_note_itself(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    desk.note(None, "the area in view");

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.take_host_messages_for_test(HostId::default());
            assert_eq!(workspace.current_surface_name_for_test(), "home");
        })
        .unwrap();
    cx.run_until_parked();

    cx.simulate_keystrokes(*workspace, "space n n");
    cx.run_until_parked();
    cx.dispatch_action(*workspace, crate::MinibufferConfirm);
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            take_desk_mutation(workspace, HostId::default()).expect("new note mutation");
            assert_eq!(
                workspace.current_surface_name_for_test(),
                "note",
                "the note is its own surface, so the note is what the reader sees"
            );
            assert!(
                workspace.insert_when_shown_for_test(),
                "the row is ready for its title rather than reading it as commands"
            );
        })
        .unwrap();
}

/// `n a` from Home: the draft opens ready to type. It used to open in
/// normal mode, so the first characters of the message were read as vim
/// commands and the reader watched the start of their sentence vanish.
#[gpui::test]
fn the_new_agent_draft_opens_ready_to_type(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    desk.note(None, "the area in view");

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    cx.simulate_keystrokes(*workspace, "space n a");
    cx.run_until_parked();
    cx.dispatch_action(*workspace, crate::MinibufferConfirm);
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "draft");
        })
        .unwrap();

    // The insert itself lands on the frame the page is drawn in, which the
    // headless test window never asks for; what is asserted here is that
    // the draft asked for it. The typing was checked in the rig.
    workspace
        .update(cx, |workspace, _, _| {
            assert!(
                workspace.insert_when_shown_for_test(),
                "the draft opened in normal mode, so the message loses its first characters"
            );
        })
        .unwrap();
}

/// `f` names a label by path: `rho/agent` is the label `agent` under the
/// label `rho`, both minted on the spot if they are new, and the label is
/// then where the thing is: it hangs under the label and no longer under
/// the place it was in.
#[gpui::test]
fn a_label_is_named_by_path_and_says_where_the_thing_is(cx: &mut TestAppContext) {
    use rho_desk::cells::{Id, Property};

    let mut desk = DeskFixture::new();
    let area = desk.note(None, "Verdict agent");
    let thing = desk.note(Some(area.clone()), "Deal QA note");

    cx.update(bind_test_keymaps);
    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.take_host_messages_for_test(HostId::default());
            workspace.label_card(HostId::default(), thing.clone(), "rho/agent", window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    let label = workspace
        .update(cx, |workspace, _, _| {
            let mutation =
                take_desk_mutation(workspace, HostId::default()).expect("label mutation");
            // Two labels are minted, the outer one first, and the inner one
            // hangs under it. Nothing the reader sees is an id.
            let names = mutation
                .writes
                .iter()
                .filter_map(|write| match &write.property {
                    Property::Name(name) => Some((write.id.clone(), name.clone())),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(names.len(), 2);
            assert_eq!(names[0].1, "rho");
            assert_eq!(names[1].1, "agent");
            assert!(matches!(names[0].0, Id::Label(_)));
            assert!(mutation.writes.iter().any(|write| write.id == names[1].0
                && write.property == Property::Parent(Some(names[0].0.clone()))));
            assert!(mutation.writes.iter().any(|write| {
                write.id == thing
                    && write.property
                        == Property::Labeled {
                            label: names[1].0.clone(),
                            present: true,
                        }
            }));
            assert_eq!(workspace.echo_text_for_test(), Some("label: rho/agent"));
            names[1].0.clone()
        })
        .unwrap();

    // A thing is placed by the labels it carries: the label it now carries
    // is where it is, and the note it was filed under yields to it. Taking
    // the label off below puts that place back.
    workspace
        .update(cx, |workspace, _, _| {
            let places = workspace
                .desk_cells_snapshot_for_test(HostId::default())
                .into_iter()
                .filter(|node| node.id == thing)
                .map(|node| node.under)
                .collect::<Vec<_>>();
            assert_eq!(places, vec![Some(label.clone())]);
        })
        .unwrap();

    // Both places are reachable. The map drew each as its own row; the
    // finder is what names a thing by where it sits now, and a label is a
    // second name for the same thing rather than a second thing.
    workspace
        .update(cx, |workspace, _, cx| {
            let paths = workspace
                .find_candidates(cx)
                .into_iter()
                .flat_map(|candidate| {
                    std::iter::once(candidate.path)
                        .chain(candidate.labels.into_iter().map(|label| label.name))
                })
                .collect::<Vec<_>>();
            assert!(
                paths.iter().any(|path| path.starts_with("rho/agent › ")),
                "the label path is not offered: {paths:?}"
            );
        })
        .unwrap();

    // The same path a second time is the same two labels, not two more, and
    // naming a label the thing already carries takes it off.
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.label_card(HostId::default(), thing.clone(), "rho/agent", window, cx);
            let mutation = take_desk_mutation(workspace, HostId::default()).expect("label removal");
            assert!(
                !mutation
                    .writes
                    .iter()
                    .any(|write| matches!(write.property, Property::Name(_))),
                "the labels already exist"
            );
            assert!(mutation.writes.iter().any(|write| {
                write.id == thing
                    && write.property
                        == Property::Labeled {
                            label: label.clone(),
                            present: false,
                        }
            }));
            assert_eq!(
                workspace.echo_text_for_test(),
                Some("label removed: rho/agent")
            );
            assert_eq!(
                workspace
                    .desk_cells_snapshot_for_test(HostId::default())
                    .into_iter()
                    .filter(|node| node.id == thing)
                    .count(),
                1,
                "the thing is back in one place"
            );
            assert_eq!(
                workspace
                    .desk_cells_snapshot_for_test(HostId::default())
                    .into_iter()
                    .find(|node| node.id == thing)
                    .and_then(|node| node.under),
                Some(area.clone()),
                "the place it was in is where it is again"
            );
        })
        .unwrap();
}

/// Undo puts back the state the verdict changed, not the one cell it is
/// named after: filing a thing carrying `rho` under `rho/agent` moves two
/// cells, so undoing it takes `rho/agent` off and puts `rho` back on.
#[gpui::test]
fn undoing_a_filing_puts_back_the_label_it_took_off(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let dealt = desk.due_note(None, "Deal QA note");
    let rho = desk.label("rho");
    desk.labelled(dealt.clone(), rho.clone());

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.pull_card(window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.label_card(HostId::default(), dealt.clone(), "rho/agent", window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    let (stamp, agent) = workspace
        .update(cx, |workspace, _, _| {
            let mutation =
                take_desk_mutation(workspace, HostId::default()).expect("filing mutation");
            let agent = workspace
                .desk_cells
                .label_paths(HostId::default())
                .into_iter()
                .find(|(_, path)| path == "rho/agent")
                .map(|(id, _)| id)
                .expect("the deeper label exists");
            assert!(
                mutation.writes.iter().any(|write| write.id == dealt
                    && write.property
                        == rho_desk::cells::Property::Labeled {
                            label: rho.clone(),
                            present: false,
                        }),
                "the shallower label comes off with the filing: {:?}",
                mutation.writes
            );
            (mutation.stamp, agent)
        })
        .unwrap();
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::DeskMutationAccepted { stamp },
                window,
                cx,
            );
        })
        .unwrap();

    cx.dispatch_action(*workspace, crate::UndoVerdict);
    workspace
        .update(cx, |workspace, _, _| {
            let mutation = take_desk_mutation(workspace, HostId::default()).expect("undo mutation");
            let wrote = |label: &rho_desk::cells::Id, present: bool| {
                mutation.writes.iter().any(|write| {
                    write.id == dealt
                        && write.property
                            == rho_desk::cells::Property::Labeled {
                                label: label.clone(),
                                present,
                            }
                })
            };
            assert!(
                wrote(&agent, false),
                "undo takes the label the filing put on back off: {:?}",
                mutation.writes
            );
            assert!(
                wrote(&rho, true),
                "and puts back the one it took off: {:?}",
                mutation.writes
            );
        })
        .unwrap();
}

/// The set kept is the smallest one that says where the thing is. A thing
/// carrying `rho` filed under `rho/agent` is under `rho` by the label's own
/// nesting, so `rho` comes off in the same mutation rather than sitting
/// beside it saying the same thing; and labelling it `rho` again after that
/// says nothing at all, so nothing is written.
#[gpui::test]
fn filing_under_a_deeper_label_takes_the_shallower_one_off(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let thing = desk.note(None, "the flick recogniser");
    let rho = desk.label("rho");
    desk.labelled(thing.clone(), rho.clone());

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.take_host_messages_for_test(HostId::default());
            workspace.label_card(HostId::default(), thing.clone(), "rho/agent", window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    let carried = |workspace: &mut Workspace| {
        let mut paths = workspace
            .desk_cells
            .facts(HostId::default(), &thing)
            .expect("the thing has facts")
            .labels
            .iter()
            .filter_map(|label| {
                workspace
                    .desk_cells
                    .label_paths(HostId::default())
                    .into_iter()
                    .find(|(id, _)| id == label)
                    .map(|(_, path)| path)
            })
            .collect::<Vec<_>>();
        paths.sort();
        paths
    };
    workspace
        .update(cx, |workspace, window, cx| {
            assert_eq!(
                carried(workspace),
                vec!["rho/agent".to_owned()],
                "the deeper label replaces the one it is nested under"
            );

            // The shallower one again is a no-op: the thing is already
            // under it, and the picker says so instead of growing the set.
            workspace.take_host_messages_for_test(HostId::default());
            workspace.label_card(HostId::default(), thing.clone(), "rho", window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(
                carried(workspace),
                vec!["rho/agent".to_owned()],
                "labelling it with a label it is already under writes nothing"
            );
            assert!(
                take_desk_mutation(workspace, HostId::default()).is_none(),
                "and sends nothing"
            );
        })
        .unwrap();
}

/// A thing under two labels is drawn under each: labels are a second axis,
/// and a primary label would be the parent again under another name.
#[gpui::test]
fn a_thing_under_two_labels_is_drawn_under_each(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let thing = desk.note(None, "the flick recogniser");
    let rho = desk.label("rho");
    let phone = desk.label("phone");
    desk.labelled(thing.clone(), rho.clone());
    desk.labelled(thing.clone(), phone.clone());

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            let rows = workspace.desk_cells_snapshot_for_test(HostId::default());
            let under = rows
                .iter()
                .filter(|node| node.id == thing)
                .map(|node| node.under.clone())
                .collect::<Vec<_>>();
            assert_eq!(under.len(), 2, "one row per label, got {under:?}");
            assert!(under.contains(&Some(rho)) && under.contains(&Some(phone)));
        })
        .unwrap();
}

/// The picker offers labels and nothing else: a thing is placed by what it
/// carries and carries no parent, so a note is not a place to file under.
#[gpui::test]
fn filing_offers_labels_and_no_places(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let area = desk.note(None, "Verdict agent");
    let dealt = desk.due_note(None, "Deal QA note");

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.label_card(HostId::default(), area.clone(), "rho", window, cx);
            workspace.pull_card(window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();
    cx.run_until_parked();

    cx.dispatch_action(*workspace, crate::DashboardDealFile);
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert!(
                workspace
                    .filing_destinations_for_test()
                    .iter()
                    .any(|(path, kind, _, id)| path == "rho"
                        && *kind == "label"
                        && matches!(id, rho_desk::cells::Id::Label(_))),
                "the label is a place to file under"
            );
            assert!(
                !workspace
                    .filing_destinations_for_test()
                    .iter()
                    .any(|(path, ..)| path == "Verdict agent"),
                "a note is not a place a thing can be filed under"
            );
            let _ = &dealt;
        })
        .unwrap();
}

/// `f` is the one filing key: a label path in the picker puts that label
/// on the thing, and the same path again takes it off, so a thing carries
/// as many labels as the user says while sitting in one place.
#[gpui::test]
fn filing_under_a_label_puts_it_on_and_the_same_path_takes_it_off(cx: &mut TestAppContext) {
    use rho_desk::cells::{Id, Property};

    let mut desk = DeskFixture::new();
    let dealt = desk.due_note(None, "Deal QA note");

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.pull_card(window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();
    cx.run_until_parked();

    cx.dispatch_action(*workspace, crate::DashboardDealFile);
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "r h o");
    cx.dispatch_action(*workspace, crate::MinibufferConfirm);
    cx.run_until_parked();

    let (label, stamp) = workspace
        .update(cx, |workspace, _, _| {
            let mutation =
                take_desk_mutation(workspace, HostId::default()).expect("label mutation");
            let label = mutation
                .writes
                .iter()
                .find_map(|write| match &write.property {
                    Property::Name(name) if name == "rho" => Some(write.id.clone()),
                    _ => None,
                })
                .expect("the path mints the label it names");
            assert!(matches!(label, Id::Label(_)));
            assert!(
                mutation.writes.iter().any(|write| write.id == dealt
                    && write.property
                        == Property::Labeled {
                            label: label.clone(),
                            present: true,
                        }),
                "picking a label path labels the thing"
            );
            assert!(
                !mutation
                    .writes
                    .iter()
                    .any(|write| write.id == dealt && write.property == Property::Parent(None)),
                "and leaves its place alone"
            );
            (label, mutation.stamp)
        })
        .unwrap();
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::DeskMutationAccepted { stamp },
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();

    cx.dispatch_action(*workspace, crate::DashboardDealFile);
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "r h o");
    cx.dispatch_action(*workspace, crate::MinibufferConfirm);
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            let mutation =
                take_desk_mutation(workspace, HostId::default()).expect("unlabel mutation");
            assert!(
                mutation.writes.iter().any(|write| write.id == dealt
                    && write.property
                        == Property::Labeled {
                            label: label.clone(),
                            present: false,
                        }),
                "the same path again takes the label off"
            );
        })
        .unwrap();
}

/// A label is what a project is: it carries the workdir itself, and a
/// thing made in the label is made in that workdir. There is no project
/// row in between, so the path a new agent inherits is the label's own.
#[gpui::test]
fn a_thing_in_a_label_with_a_project_inherits_its_workdir(cx: &mut TestAppContext) {
    use rho_desk::cells::{Id, Project, Property, Uuid};

    let mut desk = DeskFixture::new();
    let label = Id::Label(Uuid([7; 16]));
    desk.file(label.clone(), None);
    desk.set(label.clone(), Property::Name("rho".to_owned()));
    desk.set(
        label.clone(),
        Property::Project(Some(Project {
            host: 0,
            path: "/src/rho".into(),
        })),
    );
    let area = desk.note(None, "Verdict agent");
    desk.set(
        area.clone(),
        Property::Labeled {
            label: label.clone(),
            present: true,
        },
    );
    let under = desk.note(Some(area.clone()), "Deal QA note");

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(
                workspace
                    .area_workdir_for_test(HostId::default(), area.clone())
                    .map(|workdir| workdir.path.to_string()),
                Some("/src/rho".to_owned()),
                "the label the area carries names the workdir"
            );
            assert_eq!(
                workspace
                    .area_workdir_for_test(HostId::default(), under.clone())
                    .map(|workdir| workdir.path.to_string()),
                Some("/src/rho".to_owned()),
                "and it carries down the ancestry like any other inheritance"
            );
        })
        .unwrap();
}

/// Find ranks over the label paths as well as the place: the reader
/// remembers `rho/agent` as readily as where the thing sits, so `rhoag`
/// reaches it.
#[gpui::test]
fn find_matches_a_thing_by_the_label_it_carries(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let area = desk.note(None, "Verdict agent");
    let thing = desk.note(Some(area.clone()), "Deal QA note");
    let elsewhere = desk.note(None, "Backlog");
    let _ = desk.note(Some(elsewhere.clone()), "something else entirely");

    cx.update(bind_test_keymaps);
    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.label_card(HostId::default(), thing.clone(), "rho/agent", window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, cx| {
            let candidates = workspace.find_candidates(cx);
            let labelled = candidates
                .iter()
                .find(|candidate| candidate.path.ends_with("Deal QA note"))
                .expect("the labelled thing is findable");
            assert!(
                labelled
                    .labels
                    .iter()
                    .any(|name| name.name.starts_with("rho/agent")),
                "the label path is one of its names, got {:?}",
                labelled.labels
            );
            let names = candidates
                .iter()
                .map(|candidate| (candidate.names_for_test(), candidate.recency))
                .collect::<Vec<_>>();
            let best = crate::find::rank_names(&names, "rhoag")
                .first()
                .copied()
                .expect("rhoag matches the label path");
            assert!(
                candidates[best].path.ends_with("Deal QA note"),
                "the label path is what `rhoag` names, got {:?}",
                candidates[best].path
            );
        })
        .unwrap();
}

/// An agent nobody filed is still findable, and it answers to the words
/// the user last said to it as well as to its name: that is what the
/// reader remembers, and a spawn name they never gave it is not.
#[gpui::test]
fn find_reaches_an_unfiled_agent_by_what_the_user_said(cx: &mut TestAppContext) {
    let agent_id = agent(61);
    let desk = DeskFixture::new();

    cx.update(bind_test_keymaps);
    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(vec![ui_head(agent_id)], 70),
                window,
                cx,
            );
            story::feed(
                workspace,
                HostId::default(),
                story::story(
                    agent_id,
                    vec![
                        story::UiStoryEvent::UserMessage {
                            text: "rebuild the search index".to_owned(),
                            at: UnixMs(1),
                        },
                        story::UiStoryEvent::TurnEnded {
                            outcome: story::UiTurnOutcome::Completed,
                            at: UnixMs(2),
                        },
                    ],
                ),
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, cx| {
            let candidates = workspace.find_candidates(cx);
            assert!(
                candidates
                    .iter()
                    .any(|candidate| candidate.target == crate::find::FindTarget::Agent(agent_id)),
                "the unfiled agent is a candidate"
            );
            let names = candidates
                .iter()
                .map(|candidate| (candidate.names_for_test(), candidate.recency))
                .collect::<Vec<_>>();
            let best = crate::find::rank_names(&names, "searchindex")
                .first()
                .copied()
                .expect("the words the user said match");
            assert_eq!(
                candidates[best].target,
                crate::find::FindTarget::Agent(agent_id),
                "what the user said names the agent, got {:?}",
                candidates[best].path
            );
        })
        .unwrap();
}

/// A tab opened from a page belongs under that page. The browser is the
/// only thing that knows where a tab came from, so the map joins it in
/// live; nothing about a tab is ever written to the store.
#[gpui::test]
fn tabs_opened_from_a_page_hang_under_it(cx: &mut TestAppContext) {
    use rho_desk::cells::Id;

    let page = |last: u8| {
        rho_browser::PageId(uuid::Uuid::from_bytes([
            1, 2, 3, 4, 5, 6, 0x47, 8, 0x89, 10, 11, 12, 13, 14, 15, last,
        ]))
    };
    let desk_page = |id: rho_browser::PageId| Id::Page(rho_desk::PageId(*id.0.as_bytes()));
    let origin = page(1);
    let burst = [page(2), page(3), page(4)];
    let alone = page(5);

    // The messages the extension really sends, through the native host's
    // own entry point: a search page, three tabs ctrl-clicked out of it,
    // and one tab the reader opened for its own sake.
    let announce = |id: rho_browser::PageId, opened_from: Option<rho_browser::PageId>| {
        rho_browser::native_host::record_page_metadata(&serde_json::json!({
            "event": "page-metadata",
            "page_id": id.to_string(),
            "title": format!("tab {}", id.0.as_bytes()[15]),
            "url": "https://example.com/",
            "opened_from": opened_from.map(|id| id.to_string()).unwrap_or_default(),
        }));
    };
    announce(origin, None);
    for tab in burst {
        announce(tab, Some(origin));
    }
    announce(alone, None);

    // The browser has to exist in this process for its tabs to mean
    // anything; with none there are no tabs, which is what every other
    // test sees.
    let browser_dir = tempfile::tempdir().unwrap();
    cx.update(|cx| {
        rho_browser::init(
            browser_dir.path(),
            browser_dir.path().join("browser.sock"),
            cx,
        )
    });

    let mut desk = DeskFixture::new();
    let project = desk.label("Release research");
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.sync_tree_dashboard(HostId::default(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    let places = |workspace: &Workspace, id: &Id| {
        workspace
            .desk_cells_snapshot_for_test(HostId::default())
            .into_iter()
            .filter(|node| &node.id == id)
            .map(|node| node.under)
            .collect::<Vec<_>>()
    };
    workspace
        .update(cx, |workspace, _, _| {
            for tab in burst {
                assert_eq!(
                    places(workspace, &desk_page(tab)),
                    vec![Some(desk_page(origin))],
                    "a ctrl-clicked tab reads as a group under the page it came from"
                );
            }
            // The origin is drawn even though the reader has said nothing
            // about it, or the burst would land at the root instead.
            assert_eq!(places(workspace, &desk_page(origin)), vec![None]);
            // A tab opened for its own sake belongs to nothing, and the
            // map does not show every tab.
            assert!(places(workspace, &desk_page(alone)).is_empty());
        })
        .unwrap();

    // Filing the origin carries the group with it: the tabs still derive
    // their place from the origin, wherever the reader puts it.
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.file_page(
                origin,
                Some((HostId::default(), project.clone())),
                rho_journal::CreateMethod::New,
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(
                places(workspace, &desk_page(origin)),
                vec![Some(project.clone())]
            );
            for tab in burst {
                assert_eq!(
                    places(workspace, &desk_page(tab)),
                    vec![Some(desk_page(origin))],
                    "the group moved with the page it hangs under"
                );
            }
        })
        .unwrap();

    // A tab the reader files themselves stops deriving its place: the
    // origin is where it sits until they say otherwise, not after.
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.file_page(
                burst[0],
                Some((HostId::default(), project.clone())),
                rho_journal::CreateMethod::New,
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(places(workspace, &desk_page(burst[0])), vec![Some(project)]);
        })
        .unwrap();
}

/// A verdict is about the thing the reader is on. The tap opens over a
/// page row the dealer has no card for, `f` files that page, and the card a
/// dealt surface holds is never taken by a cursor sitting somewhere else.
#[gpui::test]
fn a_verdict_follows_the_thing_in_view_not_the_card_in_hand(cx: &mut TestAppContext) {
    use rho_desk::cells::{Id, Property};

    let page = |last: u8| {
        rho_browser::PageId(uuid::Uuid::from_bytes([
            2, 3, 4, 5, 6, 7, 0x47, 9, 0x8a, 11, 12, 13, 14, 15, 16, last,
        ]))
    };
    let desk_page = |id: rho_browser::PageId| Id::Page(rho_desk::PageId(*id.0.as_bytes()));
    let origin = page(1);
    let tab = page(2);
    let announce = |id: rho_browser::PageId, opened_from: Option<rho_browser::PageId>| {
        rho_browser::native_host::record_page_metadata(&serde_json::json!({
            "event": "page-metadata",
            "page_id": id.to_string(),
            "title": format!("tab {}", id.0.as_bytes()[15]),
            "url": "https://example.com/",
            "opened_from": opened_from.map(|id| id.to_string()).unwrap_or_default(),
        }));
    };
    announce(origin, None);
    announce(tab, Some(origin));
    let browser_dir = tempfile::tempdir().unwrap();
    cx.update(|cx| {
        rho_browser::init(
            browser_dir.path(),
            browser_dir.path().join("browser.sock"),
            cx,
        )
    });

    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    let dealt = desk.due_note(None, "Deal QA note");
    // A second card, so Home has a list rather than a single row.
    let queued = desk.due_note(None, "Queued QA note");
    // A note the dealer holds no card for, to open over Home.
    let aside = desk.note(None, "An aside");
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            // The reader opened the search page, which is what puts it on
            // the map; the tab ctrl-clicked out of it needs nothing written.
            workspace.file_page(
                origin,
                None,
                rho_journal::CreateMethod::TabBirth,
                window,
                cx,
            );
            workspace.sync_tree_dashboard(HostId::default(), window, cx);
            workspace.pull_card(window, cx);
            // The page is filed but not opened: the dealt surface is what
            // the reader is on, and the surface in view still decides.
            assert_eq!(
                workspace.label_target(cx),
                Some((HostId::default(), dealt.clone())),
                "the dealt surface keeps its own card"
            );
        })
        .unwrap();
    cx.run_until_parked();

    // The reader leaves the deal for Home and then opens the page. Home
    // keeps a cursor of its own on the queue, which is the trap: the page
    // is the surface in front, so the page is what they are looking at.
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_home(window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, window, cx| {
            // Home puts its own cursor on the most important card, which is
            // the trap: the map is what the reader is looking at.
            let home = workspace.home_view().expect("Home is the surface");
            assert_eq!(
                home.update(cx, |home, cx| home.cursor_target(cx)),
                crate::home::HomeTarget::Card(crate::dashboard::DealCardId {
                    host: HostId::default(),
                    node_id: dealt.clone(),
                }),
                "Home ranks fresh, so the card just read is still on top"
            );
            // The page's own surface is a live browser view, which this
            // harness has no window for; the note beside it is a surface it
            // can show, and "the thing in view is not Home's cursor card"
            // is the same shape either way.
            workspace.open_note(HostId::default(), aside.clone(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, window, cx| {
            assert_eq!(
                workspace.label_target(cx),
                Some((HostId::default(), aside.clone())),
                "the surface in view is what a verdict is about"
            );
            assert!(
                workspace.open_verdict_transient(window, cx),
                "the tap opens over a thing the dealer has no card for"
            );
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();

    cx.dispatch_action(*workspace, crate::DashboardDealFile);
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "r h o");
    cx.dispatch_action(*workspace, crate::MinibufferConfirm);
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            let mutation =
                take_desk_mutation(workspace, HostId::default()).expect("label mutation");
            let label = mutation
                .writes
                .iter()
                .find_map(|write| match &write.property {
                    Property::Name(name) if name == "rho" => Some(write.id.clone()),
                    _ => None,
                })
                .expect("the path mints the label it names");
            assert!(
                mutation.writes.iter().any(|write| write.id == aside
                    && write.property
                        == Property::Labeled {
                            label: label.clone(),
                            present: true,
                        }),
                "the note the reader is on is what gets labelled"
            );
            assert!(
                !mutation
                    .writes
                    .iter()
                    .any(|write| write.id == dealt || write.id == queued),
                "and the cards on Home are left alone"
            );
            // The tab is not written anywhere: it hangs under the origin
            // because the browser says so, wherever the origin is filed.
            assert_eq!(
                workspace
                    .desk_cells_snapshot_for_test(HostId::default())
                    .into_iter()
                    .filter(|node| node.id == desk_page(tab))
                    .map(|node| node.under)
                    .collect::<Vec<_>>(),
                vec![Some(desk_page(origin))],
                "the group the page carries comes with it"
            );
        })
        .unwrap();
}

/// `space n a` after an agent has been read: the draft that opens is the
/// one that submits. Selection is what routes enter, so a stale one sent
/// the message to the agent's own empty prompt and dropped it silently.
#[gpui::test]
fn enter_in_a_new_agent_draft_creates_the_agent(cx: &mut TestAppContext) {
    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.select_agent(Some(agent(1)), window, cx);
            workspace.new_agent_in_area(None, window, cx);
        })
        .expect("open a new-agent draft");
    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.insert("look at the readme", window, cx)
            });
        })
        .expect("type the first message");

    cx.dispatch_action(*workspace, crate::SubmitPrompt);

    // Disconnected in tests, so the send reports itself instead of leaving
    // no trace: reaching the draft's own submit is what is under test.
    assert!(
        workspace
            .update(cx, |workspace, _, cx| workspace
                .message_log_texts(cx)
                .iter()
                .any(|message| message.contains("not connected to rho-daemon")))
            .expect("read messages"),
        "enter in a new-agent draft should submit the draft"
    );
}

/// An agent surface says what the agent is doing, from its head. Borrowing
/// the Home card's words left a running agent's line blank, because a
/// running agent has no card.
#[test]
fn an_agents_state_comes_from_its_head() {
    use rho_agents::AgentFacts;

    let now = chrono::Local::now().fixed_offset();
    let running = AgentFacts {
        turn_running: true,
        turn_started_at: Some(UnixMs((now.timestamp_millis() - 5 * 60_000).unsigned_abs())),
        ..AgentFacts::default()
    };
    assert_eq!(
        crate::dashboard::agent_state_label(&running, now).as_deref(),
        Some("working · 5m")
    );
    let running_since_before = AgentFacts {
        turn_started_at: None,
        ..running
    };
    assert_eq!(
        crate::dashboard::agent_state_label(&running_since_before, now).as_deref(),
        Some("working"),
        "a head that says a turn runs without saying since when still reads as working"
    );
    let errored = AgentFacts {
        turn_running: false,
        turn_started_at: None,
        errored: true,
        last_turn_ended: Some(UnixMs((now.timestamp_millis() - 60_000).unsigned_abs())),
        ..AgentFacts::default()
    };
    assert_eq!(
        crate::dashboard::agent_state_label(&errored, now).as_deref(),
        Some("errored · 1m ago")
    );
}

/// A creation the daemon refuses says why on the draft. The echo area is
/// two seconds long, so the whole cause used to be gone before the reader
/// could act on it, and a creation just quietly did not happen.
#[gpui::test]
fn a_refused_creation_shows_its_cause_on_the_draft(cx: &mut TestAppContext) {
    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ready_with(Vec::new(), 0),
                window,
                cx,
            );
            workspace.new_agent_in_area(None, window, cx);
        })
        .expect("open a new-agent draft");
    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |workspace, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.insert("look at the readme", window, cx)
            });
            workspace
                .draft_model_for_test()
                .update(cx, |draft, cx| draft.set_workdir_text("/tmp/repo", cx));
        })
        .expect("write the draft");

    cx.dispatch_action(*workspace, crate::SubmitPrompt);
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::ServerError(
                    "create managed jj workspace: no such repository".to_owned(),
                ),
                window,
                cx,
            );
        })
        .expect("the daemon refuses");

    let refusal = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .draft_model_for_test()
                .read(cx)
                .refusal()
                .map(str::to_owned)
        })
        .expect("read the draft");
    assert!(
        refusal
            .as_deref()
            .is_some_and(|text| text.contains("no such repository")),
        "the draft keeps the daemon's whole cause: {refusal:?}"
    );
}

/// `n a` into a label makes the agent a member of it, and an area that is
/// not a label files it nowhere: a thing carries no parent, so there is no
/// parent to write instead.
#[test]
fn a_new_thing_is_filed_by_its_label_and_never_by_a_parent() {
    use rho_desk::cells::{Id, Property};

    let label = Id::Label(rho_desk::cells::Uuid([3; 16]));
    assert!(matches!(
        crate::workspace::filing_property(label),
        Some(Property::Labeled { present: true, .. })
    ));
    let note = Id::Note(rho_desk::cells::Uuid([4; 16]));
    assert_eq!(crate::workspace::filing_property(note), None);
}

/// Enter in the workdir row sends the draft. The prompt's own enter is
/// insert-only and tab leaves the cursor in normal mode, so a message
/// written and then corrected in a field could not be sent at all.
#[gpui::test]
fn enter_in_the_workdir_field_sends_the_draft(cx: &mut TestAppContext) {
    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.new_agent_in_area(None, window, cx);
        })
        .expect("open a new-agent draft");
    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.insert("look at the readme", window, cx)
            });
        })
        .expect("type the first message");

    cx.dispatch_action(*workspace, rho_agents::RoleCycle);
    workspace
        .update(cx, |workspace, window, cx| {
            assert!(
                workspace.cursor_in_draft_field_for_test(cx),
                "tab from the body lands in the workdir row"
            );
            workspace.submit_from_draft_field_for_test(window, cx);
        })
        .expect("enter in the field");

    assert!(
        workspace
            .update(cx, |workspace, _, cx| workspace
                .message_log_texts(cx)
                .iter()
                .any(|message| message.contains("not connected to rho-daemon")))
            .expect("read messages"),
        "enter in the workdir row should submit the draft"
    );
}

/// Shift-Tab walks the rows the other way round. It used to cycle the value
/// under the cursor, so there was no way back except forwards through every
/// row.
#[gpui::test]
fn shift_tab_walks_the_draft_fields_backwards(cx: &mut TestAppContext) {
    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.new_agent_in_area(None, window, cx);
        })
        .expect("open a new-agent draft");

    // From the body, backwards is the start row, then the role row.
    cx.dispatch_action(*workspace, rho_agents::RoleCycleGroup);
    workspace
        .update(cx, |workspace, _, cx| {
            assert!(workspace.cursor_in_draft_start_field_for_test(cx));
        })
        .expect("start row");
    cx.dispatch_action(*workspace, rho_agents::RoleCycleGroup);
    workspace
        .update(cx, |workspace, _, cx| {
            assert!(workspace.cursor_in_draft_role_field_for_test(cx));
        })
        .expect("role row");
}

/// A draft stands for no card: it is a message being written, not a thing
/// that was dealt. With no node of its own it wore whichever card the
/// cursor had left behind, label, why and all.
#[gpui::test]
fn a_draft_wears_no_other_cards_label(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    desk.due_note(None, "Card in view");
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            workspace.pull_card(window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert!(
                workspace.open_card_in_view(cx).is_some(),
                "the pulled card is what is in view"
            );
        })
        .unwrap();

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.new_agent_in_area(None, window, cx);
        })
        .expect("open a new-agent draft");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(
                workspace.open_card_in_view(cx).map(|card| card.identity),
                None,
                "the draft in front of it stands for no card"
            );
        })
        .unwrap();
}

/// Where enter goes in a draft's header row, as the keymap resolves it: to
/// the draft's own submit, over vim's normal-mode motion.
#[gpui::test]
fn enter_in_a_draft_field_routes_to_the_drafts_submit(cx: &mut TestAppContext) {
    use gpui::{KeyContext, Keystroke};

    cx.update(bind_test_keymaps);
    cx.update(|cx| {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let draft = [
            KeyContext::parse("RhoGui").unwrap(),
            KeyContext::parse("RhoDraft").unwrap(),
            KeyContext::parse("Editor vim_mode=normal vim_operator=none").unwrap(),
        ];
        let (bindings, _) =
            keymap.bindings_for_input(&[Keystroke::parse("enter").unwrap()], &draft);
        assert_eq!(
            bindings.first().map(|binding| binding.action().name()),
            Some("rho_agents::DraftFieldSubmit"),
            "enter in a draft should reach the draft's submit: {bindings:?}"
        );
    });
}

/// Clearing a header row leaves nothing behind and keeps what is typed
/// next in that row. Vim's own `cc` stops at the row's buffer boundary: it
/// cleared nothing, and the path typed after it was split between the
/// workdir and the role.
#[gpui::test]
fn clearing_a_header_row_keeps_the_typing_in_it(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.new_agent_in_area(None, window, cx);
        })
        .expect("open a new-agent draft");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .draft_model_for_test()
                .update(cx, |view, cx| view.set_workdir_text("wrong", cx));
        })
        .expect("seed the workdir row");

    cx.simulate_keystrokes(*workspace, "escape");
    cx.dispatch_action(*workspace, rho_agents::RoleCycle);
    cx.dispatch_action(*workspace, rho_agents::DraftFieldClear);
    cx.simulate_keystrokes(*workspace, "o k");
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, cx| {
            let draft = workspace.draft_model_for_test().read(cx);
            assert_eq!(draft.workdir_text(cx), "ok", "the row holds what was typed");
            assert_eq!(draft.role_text(cx), "eng", "the row below is untouched");
        })
        .expect("read the rows");
}

/// A verdict says which card it took. The echo used to name the path above
/// the card, so `done` over an agent filed under a label said the label.
#[gpui::test]
fn a_verdict_names_the_agent_it_took(cx: &mut TestAppContext) {
    let agent_id = agent(42);
    let desk = DeskFixture::new();

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(
                    vec![story::UiAgentHead {
                        story_pos: story::UiStoryPos(4),
                        spawn_name: Some("the deploy".to_owned()),
                        ..ui_head(agent_id)
                    }],
                    50,
                ),
                window,
                cx,
            );
            story::feed(
                workspace,
                HostId::default(),
                story_wanting(agent_id, UnixMs(1)),
                window,
                cx,
            );
        })
        .unwrap();
    next_frame(cx, workspace);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.pull_card(window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();
    cx.run_until_parked();

    cx.dispatch_action(*workspace, crate::DashboardDealDone);
    cx.run_until_parked();
    let stamp = workspace
        .update(cx, |workspace, _, _| {
            take_desk_mutation(workspace, HostId::default())
                .expect("verdict mutation")
                .stamp
        })
        .unwrap();
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::DeskMutationAccepted { stamp },
                window,
                cx,
            );
            assert_eq!(workspace.echo_text_for_test(), Some("done: the deploy"));
        })
        .unwrap();
}

/// The tag this test's fold carries, so the fold is this test's and no
/// other's.
enum FoldedInThisTest {}

/// A row takes its scale from the sync that wrote it, so every path to the
/// wrap map has to carry the scales the display map holds — folding above
/// all, because folding is also what flushes a pending buffer edit. A
/// transcript composes history and then folds it, and the fold's own sync is
/// what wraps the rows composition just wrote: wrapped without their scale
/// they come out 1.12 too wide and nothing rewrites them.
#[gpui::test]
fn a_fold_wraps_the_rows_it_flushes_at_their_scale(cx: &mut TestAppContext) {
    cx.update(init_test_app);
    let alpha = "alpha ".repeat(60);
    let charlie = "charlie ".repeat(10);
    let editor = cx.add_window(|window, cx| {
        let mut editor = Editor::multi_line(window, cx);
        editor.set_text(format!("{alpha}\n{charlie}\n"), window, cx);
        editor
    });
    editor
        .update(cx, |editor, window, cx| {
            editor.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
            window.refresh();
        })
        .expect("soft wrap at the editor's width");
    cx.simulate_window_resize(*editor, gpui::size(gpui::px(300.), gpui::px(400.)));
    cx.run_until_parked();
    cx.update_window(*editor, |_, window, cx| window.simulate_next_frame(cx))
        .expect("wrap the text this editor opened on");
    cx.run_until_parked();

    // Compose a row, scale it, and fold something else — in one pass, with
    // no display snapshot in between, which is the order a transcript
    // composing history works in.
    let bravo = "bravo ".repeat(60);
    editor
        .update(cx, |editor, _, cx| {
            let end = editor.buffer().read(cx).len(cx);
            editor.buffer().update(cx, |buffer, cx| {
                buffer.edit([(end..end, format!("{bravo}\n"))], None, cx);
            });
            let end = end.0;
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let scaled = snapshot.anchor_before(multi_buffer::MultiBufferOffset(end))
                ..snapshot.anchor_after(multi_buffer::MultiBufferOffset(end + bravo.len()));
            let folded = snapshot.anchor_before(multi_buffer::MultiBufferOffset(alpha.len() + 1))
                ..snapshot.anchor_after(multi_buffer::MultiBufferOffset(
                    alpha.len() + 1 + charlie.len(),
                ));
            editor.display_map.update(cx, |map, cx| {
                map.set_row_scales(vec![(scaled, 1.5)], cx);
                map.fold(
                    vec![editor::display_map::Crease::simple(
                        folded,
                        editor::FoldPlaceholder::concealed(
                            std::any::TypeId::of::<FoldedInThisTest>(),
                        ),
                    )],
                    cx,
                );
            });
        })
        .expect("compose a scaled row and fold in one pass");
    cx.run_until_parked();

    let (unscaled, scaled) = editor
        .update(cx, |editor, _, cx| {
            let snapshot = editor.display_snapshot(cx);
            let mut unscaled = 0;
            let mut scaled = 0;
            for line in snapshot.text().lines() {
                if line.starts_with("alpha") {
                    unscaled = unscaled.max(line.chars().count());
                } else if line.starts_with("bravo") {
                    scaled = scaled.max(line.chars().count());
                }
            }
            (unscaled, scaled)
        })
        .expect("measure the wrapped rows");
    assert!(
        unscaled > 0 && scaled > 0,
        "both rows wrap: {unscaled} {scaled}"
    );
    assert!(
        scaled < unscaled,
        "the scaled row breaks earlier than the unscaled one: \
         {scaled} characters against {unscaled}"
    );
}

/// The usage charts are a screen, not a strip: `space s u` then a letter
/// opens the usage buffer with the chart in it, and picking another chart
/// redraws that one screen instead of opening a second place to be.
#[gpui::test]
fn the_usage_menu_opens_one_screen_and_another_chart_redraws_it(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_menu(crate::transient::usage_root_menu(), window, cx);
            assert_eq!(workspace.menu_title_for_test(), Some("usage"));
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "c");
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(workspace.current_surface_name_for_test(), "usage");
            assert_eq!(
                workspace.usage_chart_for_test(cx),
                Some((crate::usage::Chart::ModelCost, 1, true)),
                "one screen, with the chart drawn into it as a block"
            );
            assert_eq!(
                workspace.menu_title_for_test(),
                None,
                "the menu closed behind the chart it opened"
            );
        })
        .unwrap();

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_menu(crate::transient::usage_root_menu(), window, cx);
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "shift-a");
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(
                workspace.usage_chart_for_test(cx),
                Some((crate::usage::Chart::AgentCost, 1, true)),
                "the second chart replaced the picture on the same surface"
            );
        })
        .unwrap();
}

/// Rows the wrap map was given to lay out, across a run of sync traces.
fn wrap_rows(traces: &[editor::display_map::WrapSyncTrace]) -> u32 {
    traces
        .iter()
        .flat_map(|trace| &trace.input)
        .map(|edit| edit.new.end.row() - edit.new.start.row() + 1)
        .sum()
}

/// `gg` gives the reader the top of the transcript, not the transcript.
///
/// Going to the top used to compose every block between the tail and the
/// first one before the point could land there: on a long transcript that is
/// the whole history laid out — wrapped, folded, blocked — to show one
/// screen. Composition serves the end the reader asked for now, so the top
/// is composed first, the point lands on it while the middle is still a gap,
/// and the gap closes behind them.
#[gpui::test]
fn going_to_the_top_lays_out_the_top_and_not_the_transcript(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    feed_frame(&workspace, cx, agent(1), long_history());
    let editor = active_editor(&workspace, cx);
    let model = workspace
        .update(cx, |workspace, _, _cx| {
            workspace.agent_model_for_test(agent(1))
        })
        .expect("the agent's model");
    workspace
        .update(cx, |_, _, cx| {
            editor.update(cx, |editor, cx| {
                editor.display_map.update(cx, |map, cx| {
                    map.snapshot(cx);
                    map.take_wrap_sync_traces(cx);
                })
            })
        })
        .expect("the rows the transcript opened on are not the rows gg lays out");

    // What was true the moment the reader's point landed on the top.
    type LandingSnapshot = (usize, usize, u32, Option<usize>);
    let landed: std::rc::Rc<std::cell::RefCell<Option<LandingSnapshot>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let _subscription = {
        let landed = landed.clone();
        let editor = editor.clone();
        cx.update(|cx| {
            cx.subscribe(&model, move |model, event, cx| {
                if !matches!(
                    event,
                    rho_agents::agent_view::AgentModelEvent::HistoryComposed(_)
                ) {
                    return;
                }
                let rows = editor.update(cx, |editor, cx| {
                    editor.display_map.update(cx, |map, cx| {
                        map.snapshot(cx);
                        wrap_rows(&map.take_wrap_sync_traces(cx))
                    })
                });
                let model = model.read(cx);
                *landed.borrow_mut() = Some((
                    model.head_blocks(),
                    model.uncomposed_blocks(),
                    rows,
                    model.gap_marker_block(),
                ));
            })
        })
    };

    cx.simulate_keystrokes(*workspace, "escape g g");
    cx.run_until_parked();

    let (head, uncomposed, rows, marker) = landed.borrow().expect("the point landed on the top");
    assert!(
        rows < 400,
        "the rows laid out to serve gg are the rows the reader is about to \
         read, not every row above them: {rows}"
    );
    assert!(
        head > 0,
        "the top of the transcript is composed for the reader who asked for it"
    );
    assert!(
        uncomposed > 0,
        "and the middle is still a gap when the point lands: the reader waited \
         for the top, not for the transcript"
    );

    assert_eq!(
        marker,
        Some(head + uncomposed),
        "the reader is told the middle is on its way, on the row where it is: \
         a marker above the first block of the tail"
    );

    assert_eq!(
        uncomposed_blocks(&workspace, cx, agent(1)),
        0,
        "the gap closes behind the reader"
    );
    assert_eq!(
        gap_marker_block(&workspace, cx, agent(1)),
        None,
        "and the marker is gone with it"
    );
    assert_eq!(
        transcript_point_block(&workspace, cx, agent(1)),
        Some(0),
        "the point is on the first block"
    );
    assert!(
        buffer_text(&workspace, cx).contains("turn 0 line one"),
        "the top of the history is in the buffer"
    );
}

/// The verdicts open over a running agent on Home, so they have to land on
/// it. An agent nobody has filed has no row on the desk, and a verdict that
/// wanted a row opened its menu over the agent and then refused: `tab s s`
/// said "snooze: nothing under the deal". The verdicts are the one door to
/// a verdict now, so a door that opens and refuses is the whole of it.
#[gpui::test]
fn a_verdict_lands_on_a_running_agent_nobody_filed(cx: &mut TestAppContext) {
    let loose = agent(31);
    // Nothing is filed: the desk has never heard of this agent, which is
    // every agent the user started and did not put anywhere.
    let desk = DeskFixture::new();

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(
                    vec![story::UiAgentHead {
                        turn_running: true,
                        ..ui_head(loose)
                    }],
                    40,
                ),
                window,
                cx,
            );
            workspace.open_home(window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, window, cx| {
            let home = workspace.home_view().expect("Home is the surface");
            assert_eq!(
                home.update(cx, |home, cx| home.cursor_target(cx)),
                crate::home::HomeTarget::Agent(loose),
                "the only row Home has is the running agent"
            );
            assert!(
                workspace.open_verdict_transient(window, cx),
                "the verdicts open over the row"
            );
            workspace.deal_snooze(crate::workspace::SnoozeUnit::Days, None, window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            assert!(
                workspace
                    .dashboard
                    .agent_put_down(loose, chrono::Local::now().fixed_offset()),
                "and what they opened them for is what happens: the agent is \
                 put down until tomorrow"
            );
        })
        .unwrap();
    let text = buffer_text(&workspace, cx);
    assert!(
        !text.contains("running"),
        "the row it was on is gone with it, got {text:?}"
    );
}

/// The user's verdict on an agent lives in the store, so a client that has
/// not read the store yet cannot say the agent is not already put down. It
/// dealt anyway: `loose_agent_card` asked its guard with `is_some_and`, so
/// no desk read as no verdict, and every agent wanting the user went into
/// `next` — including the ones snoozed yesterday. A restart was enough to
/// deal a snoozed agent again, for as long as the first sync took, and on a
/// large desk that is long enough to act on.
#[gpui::test]
fn an_agent_is_not_dealt_before_the_desk_that_holds_its_verdict(cx: &mut TestAppContext) {
    let waiting = agent(31);
    let mut desk = DeskFixture::new();
    let heading = desk.note(None, "rho");
    desk.agent_row(heading, waiting);

    let workspace = test_workspace(cx);
    // The mirror arrives first and the desk has not answered: a client
    // between opening and its first sync.
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ready_with(vec![ui_head(waiting)], 40),
                window,
                cx,
            );
            story::feed(
                workspace,
                HostId::default(),
                story_wanting(waiting, UnixMs(1)),
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();
    let dealt = |cx: &mut TestAppContext| {
        workspace
            .update(cx, |workspace, _, cx| {
                workspace
                    .hand(cx)
                    .cards
                    .iter()
                    .filter_map(|card| card.agent_id)
                    .collect::<Vec<_>>()
            })
            .unwrap()
    };
    assert!(
        dealt(cx).is_empty(),
        "nothing is dealt out of a store nobody has read"
    );

    // The desk answers, and the agent is dealt: waiting for the store is
    // not the same as never dealing.
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    assert_eq!(
        dealt(cx),
        vec![waiting],
        "and once it has answered the card goes out"
    );
}

/// Home's running list reads the same verdict as the dealer and used to
/// read it its own way: `agent_put_down` asked every deal source and there
/// were none, so `any` was false and a snoozed agent read as running. On a
/// rig, on the user's own desk, that put two snoozed agents back on Home
/// for the first half second after the client restarted.
#[gpui::test]
fn a_running_row_is_not_drawn_before_the_desk_that_holds_its_verdict(cx: &mut TestAppContext) {
    let running = agent(31);
    let mut desk = DeskFixture::new();
    let heading = desk.note(None, "rho");
    desk.agent_row(heading, running);

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ready_with(
                    vec![story::UiAgentHead {
                        turn_running: true,
                        ..ui_head(running)
                    }],
                    40,
                ),
                window,
                cx,
            );
            workspace.open_home(window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    assert!(
        !buffer_text(&workspace, cx).contains("running"),
        "no desk has answered, so nothing is said about the agent either way"
    );

    // The desk answers and the row is drawn, because waiting for the store
    // is not the same as never drawing.
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    let text = buffer_text(&workspace, cx);
    assert!(text.contains("running"), "home text: {text:?}");
}

/// The other half of the cold-open question: a card that did go out and
/// is then put down by the store. The hand is derived from the dealer's
/// cards every time it is asked, and a desk that arrives takes the host's
/// cards whole, so the card is not sticky — it is re-derived away rather
/// than left standing in the queue.
#[gpui::test]
fn a_dealt_card_leaves_the_hand_when_the_desk_says_it_was_put_down(cx: &mut TestAppContext) {
    let waiting = agent(31);
    let mut desk = DeskFixture::new();
    let heading = desk.note(None, "rho");
    desk.agent_row(heading, waiting);

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ready_with(vec![ui_head(waiting)], 40),
                window,
                cx,
            );
            story::feed(
                workspace,
                HostId::default(),
                story_wanting(waiting, UnixMs(1)),
                window,
                cx,
            );
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    let dealt = |cx: &mut TestAppContext| {
        workspace
            .update(cx, |workspace, _, cx| {
                workspace
                    .hand(cx)
                    .cards
                    .iter()
                    .filter_map(|card| card.agent_id)
                    .collect::<Vec<_>>()
            })
            .unwrap()
    };
    assert_eq!(
        dealt(cx),
        vec![waiting],
        "the desk said nothing, so it deals"
    );

    // The user's snooze, arriving from the store after the card went out.
    desk.set(
        rho_desk::cells::Id::Agent(waiting),
        rho_desk::cells::Property::DeferUntil(Some(rho_desk::cells::Timestamp {
            unix_ms: 4_000_000_000_000,
            precision: rho_desk::cells::TimestampPrecision::Day,
        })),
    );
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    assert!(
        dealt(cx).is_empty(),
        "a dealt card does not outlive the verdict that put it down"
    );
}

/// Naming is the verdict menu's `n`, and its subject is the card in view.
/// It used to be `space a n`, which took the agent under the point: the
/// same shape as the done that went before it, and the same way to write
/// the user's own words about one thing onto another.
#[gpui::test]
fn the_verdict_menu_names_the_card_in_view(cx: &mut TestAppContext) {
    let waiting = agent(41);
    let mut desk = DeskFixture::new();
    let heading = desk.note(None, "rho");
    desk.agent_row(heading, waiting);

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(vec![ui_head(waiting)], 40),
                window,
                cx,
            );
            story::feed(
                workspace,
                HostId::default(),
                story_wanting(waiting, UnixMs(1)),
                window,
                cx,
            );
        })
        .unwrap();
    next_frame(cx, workspace);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.pull_card(window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();
    cx.run_until_parked();

    press_tab(&workspace, cx);
    cx.simulate_keystrokes(*workspace, "n");
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "f i x space t h e space l i n k e r");
    cx.simulate_keystrokes(*workspace, "enter");
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            let mutation =
                take_desk_mutation(workspace, HostId::default()).expect("the name mutation");
            let wrote = mutation
                .writes
                .iter()
                .find(|write| write.id == rho_desk::cells::Id::Agent(waiting))
                .expect("the name lands on the agent of the card in view");
            assert_eq!(
                wrote.property,
                rho_desk::cells::Property::Name("fix the linker".to_owned())
            );
        })
        .unwrap();
}

/// The shape that would let a snooze be read off the wrong node: an agent
/// filed under a heading and carrying a label, so the dealer can reach it
/// through the heading's subtree as well as on its own. The verdict is on
/// `Id::Agent` either way, so both readers have to see it; a heading path
/// that asked the heading's node instead would deal the card.
#[gpui::test]
fn a_snoozed_agent_under_a_heading_is_dealt_by_neither_path(cx: &mut TestAppContext) {
    let put_away = agent(41);
    let mut desk = DeskFixture::new();
    let heading = desk.note(None, "rho");
    desk.agent_row(heading, put_away);
    let label = desk.label("linker");
    desk.labelled(rho_desk::cells::Id::Agent(put_away), label);
    desk.set(
        rho_desk::cells::Id::Agent(put_away),
        rho_desk::cells::Property::DeferUntil(Some(rho_desk::cells::Timestamp {
            unix_ms: 4_000_000_000_000,
            precision: rho_desk::cells::TimestampPrecision::Day,
        })),
    );

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(vec![ui_head(put_away)], 40),
                window,
                cx,
            );
            story::feed(
                workspace,
                HostId::default(),
                story_wanting(put_away, UnixMs(1)),
                window,
                cx,
            );
        })
        .unwrap();
    next_frame(cx, workspace);
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, cx| {
            let dealt = workspace
                .hand(cx)
                .cards
                .iter()
                .filter_map(|card| card.agent_id)
                .collect::<Vec<_>>();
            assert!(
                !dealt.contains(&put_away),
                "the snooze is on the agent's own row, and every path to it reads that row: {dealt:?}"
            );
        })
        .unwrap();
}
