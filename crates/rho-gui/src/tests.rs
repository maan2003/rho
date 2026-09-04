//! End-to-end tests: synthetic protocol frames in, rendered editor state out.

use editor::display_map::{Block, DisplayPoint, DisplayRow};
use editor::{Copy, Editor, MoveRight, SelectionEffects};
use gpui::{
    App, AppContext as _, Entity, Focusable as _, InputEvent as _, Modifiers, MouseButton,
    MouseDownEvent, MouseUpEvent, TestAppContext, TouchEvent, TouchId, TouchPhase, WindowHandle,
    point, px, size,
};
use rho_core::UnixMs;
use rho_ui_proto::AgentId;
use rho_ui_proto::remote::{
    AgentRemoteFrame, UiAgentState, UiAgentStatus, UiBlock, UiBlockDiff, UiBlockUpdate,
    UiBlocksDiff, UiMessagePhase, UiTextDiff, UiTool, UiToolDiff, UiToolStatus,
};
use settings::{Settings, SettingsStore};

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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    cx.simulate_window_resize(*workspace, gpui::size(gpui::px(400.), gpui::px(800.)));
    cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    cx.run_until_parked();
    let first = workspace
        .update(cx, |workspace, _, _| {
            assert!(workspace.phone_feed_for_test());
            workspace.current_deal_card_for_test().unwrap().0
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
        .update(cx, |workspace, _, _| {
            assert!(workspace.phone_feed_for_test());
            assert_ne!(workspace.current_deal_card_for_test().unwrap().0, first);
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
        .update(cx, |workspace, _, _| {
            assert!(workspace.current_deal_card_for_test().is_none());
        })
        .unwrap();

    let mut desk = DeskFixture::new();
    desk.due_note(None, "Arrived after the feed");
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert!(workspace.current_deal_card_for_test().is_some());
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_window_resize(*workspace, size(px(400.), px(800.)));
    cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    cx.run_until_parked();
    let first = workspace
        .update(cx, |workspace, window, cx| {
            let identity = workspace.current_deal_card_for_test().unwrap().0;
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
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_deal_card_for_test().unwrap().0, first);
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
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
        .update(cx, |workspace, _, _| {
            assert!(workspace.phone_feed_for_test());
            assert_eq!(workspace.current_deal_card_for_test().unwrap().0, id);
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
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
            assert!(workspace.phone_feed_for_test());
            assert!(workspace.phone_feed_is_active_for_test());
            assert_eq!(workspace.current_deal_card_for_test().unwrap().0, expected);
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
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
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskMutationAccepted { stamp },
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_deal_card_for_test(), None);
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
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskMutationAccepted { stamp },
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_deal_card_for_test().unwrap().0, expected);
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();
    cx.simulate_window_resize(*workspace, size(px(400.), px(800.)));
    cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    cx.run_until_parked();

    let identity = workspace
        .update(cx, |workspace, _, _| {
            workspace.current_deal_card_for_test().unwrap().0
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
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_deal_card_for_test().unwrap().0, identity);
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
            workspace.handle_event(
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
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskMutationAccepted { stamp: undo_stamp },
                window,
                cx,
            );
            assert_eq!(workspace.current_deal_card_for_test().unwrap().0, identity);
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

/// `s` is the snooze operator: on its own it waits, and the unit after it
/// picks the span. Shared by the deal-routing tests.
fn assert_snooze_operator(keymap: &gpui::Keymap, contexts: &[gpui::KeyContext]) {
    use gpui::Keystroke;

    let (bindings, pending) =
        keymap.bindings_for_input(&[Keystroke::parse("s").unwrap()], contexts);
    assert!(pending, "`s` should wait for its unit: {bindings:?}");
    assert!(bindings.is_empty(), "`s` alone should snooze nothing");
    for (unit, action) in [
        ("m", "rho_gui::DashboardDealSnoozeMinutes"),
        ("h", "rho_gui::DashboardDealSnoozeHours"),
        ("d", "rho_gui::DashboardDealSnooze"),
        ("s", "rho_gui::DashboardDealSnooze"),
        ("w", "rho_gui::DashboardDealSnoozeWeeks"),
    ] {
        let strokes = [
            Keystroke::parse("s").unwrap(),
            Keystroke::parse(unit).unwrap(),
        ];
        let (bindings, _) = keymap.bindings_for_input(&strokes, contexts);
        assert_eq!(
            bindings.first().map(|binding| binding.action().name()),
            Some(action),
            "`s{unit}` did not route to {action}",
        );
    }
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
/// transient a tap of `shift` opens, and the letters belong to vim again on
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
                .bindings_for_input(&[stroke.clone()], contexts)
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.open_deal_mode(window, cx);
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
            workspace.handle_event(
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
                .bindings_for_input(&[stroke.clone()], contexts)
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

#[gpui::test]
fn the_first_heading_can_be_written_on_an_empty_desk(cx: &mut TestAppContext) {
    // No row means no heading line to stand on, and the verb still has to
    // produce the first note rather than fall through to the editor.
    let desk = DeskFixture::new();
    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();

    cx.dispatch_action(*workspace, crate::DashboardNewSibling);
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
    let workspace = overview_workspace(cx);

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
        snapshot_frame(state(vec![user("read this first")], Vec::new())),
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
    "alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\ngolf\nhotel\nindia\njuliet\nkilo\nlima\nmike\nnovember\noscar\npapa\n".to_owned()
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
fn initial_transcript_preserves_line_endings_when_placing_spans(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(
            vec![user("first\r\nsecond\rthird")],
            vec![assistant(
                "answer\r\ncontinued\rfinished",
                Some(UiMessagePhase::FinalAnswer),
            )],
        )),
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
        snapshot_frame(state(vec![user("old transcript")], Vec::new())),
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
        snapshot_frame(state(vec![user("replacement")], Vec::new())),
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
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(blocks, Vec::new())),
    );

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
        snapshot_frame(state(
            vec![
                user("replacement"),
                assistant("done", Some(UiMessagePhase::FinalAnswer)),
            ],
            Vec::new(),
        )),
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
fn notices_append_to_messages_without_changing_the_transcript(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        snapshot_frame(state(vec![user("first")], Vec::new())),
    );
    let transcript_before = buffer_text(&workspace, cx);
    workspace
        .update(cx, |workspace, _, cx| {
            workspace.notice_for_test(Some(&agent(1)), "boom", cx);
        })
        .expect("post notice");
    assert!(
        workspace
            .update(cx, |workspace, _, _| workspace
                .message_log_texts()
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
            workspace.handle_event(
                HostId::default(),
                ConnEvent::ServerError("first".to_owned()),
                window,
                cx,
            );
            workspace.handle_event(
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
            workspace.handle_event(
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
        snapshot_frame(state(vec![user("agent transcript")], Vec::new())),
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
    cx.simulate_keystrokes(*workspace, "f20");
    assert!(buffer_text(&workspace, cx).contains("message log"));
}

#[gpui::test]
fn scrolled_messages_viewport_stays_put_across_append(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.seed_messages_for_test(
                (0..200).map(|index| {
                    (
                        crate::style::StyleClass::SystemInfo,
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
                crate::style::StyleClass::SystemInfo,
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
                    crate::style::StyleClass::SystemImportant,
                    "important".to_owned(),
                ))
                .chain((1..crate::workspace::MESSAGE_LOG_CAP).map(|index| {
                    (
                        crate::style::StyleClass::SystemInfo,
                        format!("ordinary-{index}"),
                    )
                })),
                cx,
            );
            workspace.cmd_messages(window, cx);
            workspace.append_test_message(
                "ordinary-new".to_owned(),
                crate::style::StyleClass::SystemInfo,
                cx,
            );
        })
        .expect("evict the important message");
    let important_color = workspace
        .update(cx, |_, _, cx| {
            crate::style::StyleClass::SystemImportant.resolve(cx).color
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
        .update(cx, |workspace, _, _| {
            for index in 0..=crate::workspace::MESSAGE_LOG_CAP {
                workspace.append_test_log_entry(format!("message-{index}"));
            }
            let messages = workspace.message_log_texts();
            assert_eq!(messages.len(), crate::workspace::MESSAGE_LOG_CAP);
            assert_eq!(messages.first(), Some(&"message-1"));
            let expected_last = format!("message-{}", crate::workspace::MESSAGE_LOG_CAP);
            assert_eq!(messages.last(), Some(&expected_last.as_str()));
        })
        .expect("fill message log");
}

#[gpui::test]
fn capped_message_buffer_periodically_rebases_its_edit_history(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let original = workspace
        .update(cx, |workspace, _, cx| {
            workspace.seed_messages_for_test(
                (0..crate::workspace::MESSAGE_LOG_CAP).map(|index| {
                    (
                        crate::style::StyleClass::SystemInfo,
                        format!("initial-{index}"),
                    )
                }),
                cx,
            );
            workspace.messages_buffer_id()
        })
        .expect("seed capped messages");
    workspace
        .update(cx, |workspace, _, cx| {
            for index in 0..crate::workspace::MESSAGE_REBASE_EVICTIONS {
                workspace.append_test_message(
                    format!("replacement-{index}"),
                    crate::style::StyleClass::SystemInfo,
                    cx,
                );
            }
        })
        .expect("append enough evictions to rebase");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert_ne!(workspace.messages_buffer_id(), original);
            assert_eq!(
                workspace.message_log_texts().len(),
                crate::workspace::MESSAGE_LOG_CAP
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
    workspace
        .update(cx, |workspace, _, _| {
            let notices = workspace.message_log_texts();
            assert!(notices.iter().any(|text| text.contains("reconnecting")));
            assert!(notices.iter().any(|text| text.contains("connected")));
            assert!(notices.iter().any(|text| text.contains("disconnected")));
        })
        .expect("inspect connection notices");
}

#[gpui::test]
fn ordinary_ready_does_not_replay_but_reconnect_resubscribes_retained_transcript(
    cx: &mut TestAppContext,
) {
    use rho_ui_proto::{
        AgentDisposition, AgentRole, AuthState, ClientMessage, UiAgentSummary, UiAttention,
        WorkspaceInfo,
    };

    let workspace = test_workspace(cx);
    let agent_id = agent(1);
    let summary = || UiAgentSummary {
        agent_id,
        parent_agent: None,
        display_name: Some("retained transcript".to_owned()),
        created_at: UnixMs(1),
        updated_at: UnixMs(1),
        role: AgentRole::default(),
        workspace: WorkspaceInfo::UserCheckout {
            repo: "/tmp".into(),
        },
        attention: UiAttention::Quiet,
        last_active: UnixMs(1),
        facts: Default::default(),
        hidden: false,
        disposition: AgentDisposition::Pending,
        last_user_message_text: String::new(),
        activity: None,
        turn_report: None,
        labels: Vec::new(),
    };
    let ready = || ConnEvent::Ready {
        agents: vec![summary()],
        iris_agent: None,
        projects: Vec::new(),
        auth: AuthState {
            namespaces: Vec::new(),
            disabled_namespaces: Vec::new(),
            active_namespace: None,
        },
        machine_seed: 0,
        agent_counter: 2,
    };

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), ready(), window, cx);
            workspace.take_host_messages_for_test(HostId::default());
            workspace.handle_event(HostId::default(), ready(), window, cx);
            let refresh_messages = workspace.take_host_messages_for_test(HostId::default());
            assert!(
                !refresh_messages
                    .iter()
                    .any(|message| { matches!(message, ClientMessage::SubscribeAgents { .. }) })
            );
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Disconnected("link dropped".to_owned()),
                window,
                cx,
            );
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Recovering(std::time::Duration::from_millis(500)),
                window,
                cx,
            );
            workspace.handle_event(HostId::default(), ready(), window, cx);

            let messages = workspace.take_host_messages_for_test(HostId::default());
            assert!(messages.iter().any(|message| {
                matches!(
                    message,
                    ClientMessage::SubscribeAgents { agent_ids }
                        if agent_ids == &vec![agent_id]
                )
            }));
        })
        .expect("reconnect fake daemon");
}

#[gpui::test]
fn dealer_recompute_keeps_only_top_three_agent_cards_warm(cx: &mut TestAppContext) {
    use rho_ui_proto::{
        AgentDisposition, AgentRole, AuthState, ClientMessage, UiAgentFacts, UiAgentSummary,
        UiAttention, WorkspaceInfo,
    };

    let ids = [agent(21), agent(22), agent(23), agent(24)];
    let mut desk = DeskFixture::new();
    for (index, agent_id) in ids.iter().copied().enumerate() {
        let heading = desk.note(None, &format!("Warm agent {}", index + 1));
        desk.agent_row(heading, agent_id);
    }
    let summaries = ids
        .iter()
        .copied()
        .enumerate()
        .map(|(index, agent_id)| UiAgentSummary {
            agent_id,
            parent_agent: None,
            display_name: Some(format!("warm {}", index + 1)),
            created_at: UnixMs(1),
            updated_at: UnixMs(1),
            role: AgentRole::default(),
            workspace: WorkspaceInfo::UserCheckout {
                repo: "/tmp".into(),
            },
            attention: UiAttention::Pending,
            last_active: UnixMs((ids.len() - index) as u64),
            facts: UiAgentFacts {
                last_turn_ended: Some(UnixMs(1)),
                last_user_message_at: UnixMs(0),
                needs_you_hint: true,
                ..Default::default()
            },
            hidden: false,
            disposition: AgentDisposition::Pending,
            last_user_message_text: String::new(),
            activity: None,
            turn_report: None,
            labels: Vec::new(),
        })
        .collect();

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Ready {
                    agents: summaries,
                    iris_agent: None,
                    projects: Vec::new(),
                    auth: AuthState {
                        namespaces: Vec::new(),
                        disabled_namespaces: Vec::new(),
                        active_namespace: None,
                    },
                    machine_seed: 0,
                    agent_counter: 30,
                },
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            workspace.take_host_messages_for_test(HostId::default());
            // The highest card is already warm. The recompute must not resend
            // it or expand the warm set to the fourth card.
            for agent_id in ids.into_iter().skip(1) {
                workspace.forget_agent_subscription_for_test(agent_id);
            }
            workspace.invalidate_dealer_signals(cx);
        })
        .unwrap();
    cx.run_until_parked();
    let subscribed = workspace
        .update(cx, |workspace, _, _| {
            workspace
                .take_host_messages_for_test(HostId::default())
                .into_iter()
                .filter_map(|message| match message {
                    ClientMessage::SubscribeAgents { agent_ids } => Some(agent_ids),
                    _ => None,
                })
                .flatten()
                .collect::<Vec<_>>()
        })
        .unwrap();
    assert_eq!(subscribed, vec![ids[1], ids[2]]);

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Disconnected("test".into()),
                window,
                cx,
            );
            for agent_id in ids {
                workspace.forget_agent_subscription_for_test(agent_id);
            }
            workspace.take_host_messages_for_test(HostId::default());
            workspace.invalidate_dealer_signals(cx);
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert!(
                workspace
                    .take_host_messages_for_test(HostId::default())
                    .into_iter()
                    .all(|message| !matches!(message, ClientMessage::SubscribeAgents { .. }))
            );
        })
        .unwrap();
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
            .update(cx, |workspace, _, _| workspace
                .message_log_texts()
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
            .update(cx, |workspace, _, _| workspace
                .message_log_texts()
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
    assert_eq!(spans, "62k", "transcript row keeps only context usage");
}

#[gpui::test]
fn transcript_status_omits_internal_ids_but_keeps_human_chips(cx: &mut TestAppContext) {
    use rho_ui_proto::{
        AgentDisposition, AgentRole, AuthState, UiAgentSummary, UiAttention, WorkspaceId,
        WorkspaceIdDomain, WorkspaceInfo,
    };

    let workspace = test_workspace(cx);
    let agent_id = agent(1);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Ready {
                    agents: vec![UiAgentSummary {
                        agent_id,
                        parent_agent: None,
                        display_name: Some("worker".to_owned()),
                        created_at: UnixMs(1),
                        updated_at: UnixMs(1),
                        role: AgentRole::default(),
                        workspace: WorkspaceInfo::Workspace {
                            repo: "/tmp/rho".into(),
                            id: WorkspaceId::from_counter(1, &WorkspaceIdDomain(0)).unwrap(),
                        },
                        attention: UiAttention::Quiet,
                        last_active: UnixMs(1),
                        facts: Default::default(),
                        hidden: false,
                        disposition: AgentDisposition::Pending,
                        last_user_message_text: String::new(),
                        activity: None,
                        turn_report: None,
                        labels: Vec::new(),
                    }],
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
        })
        .expect("register managed-workspace agent");
    feed_frame(
        &workspace,
        cx,
        agent_id,
        AgentRemoteFrame::Snapshot(UiAgentState {
            blocks: vec![user("go")],
            status: UiAgentStatus::Idle,
            context_used: Some(62_300),
            usage: rho_ui_proto::remote::UiAgentUsage {
                provider: "fable".to_owned(),
                total: rho_ui_proto::AgentUsageBucket {
                    input_tokens: 1_000_000,
                    ..Default::default()
                },
            },
        }),
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

#[test]
fn filing_completion_keeps_duplicate_heading_identity() {
    assert_eq!(
        crate::minibuffer::completion_start("Project Al", true),
        0,
        "filing completion replaces the whole partial title"
    );
    let first = rho_desk::cells::Id::Note(rho_desk::cells::Uuid([(1) as u8; 16]));
    let second = rho_desk::cells::Id::Note(rho_desk::cells::Uuid([(2) as u8; 16]));
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
fn deal_file_bare_enter_files_the_dealt_node_under_the_offered_heading(cx: &mut TestAppContext) {
    // Filing is a verdict on the card's own node: one `Parent` write, and
    // an undo that puts the node back where it was.
    let mut desk = DeskFixture::new();
    let destination = desk.note(None, "Verdict agent");
    let dealt = desk.due_note(None, "Deal QA note");

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.open_deal_mode(window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(
                workspace.current_deal_card_for_test().map(|card| card.0),
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
            assert!(mutation.writes.iter().any(|write| write.id == dealt
                && write.property == rho_desk::cells::Property::Parent(Some(destination.clone()))));
            mutation.stamp
        })
        .unwrap();
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskMutationAccepted { stamp },
                window,
                cx,
            );
            assert_eq!(workspace.verdict_undo_count_for_test(), 1);
            assert_eq!(
                workspace.echo_text_for_test(),
                Some("file under Verdict agent: Deal QA note")
            );
        })
        .unwrap();

    cx.dispatch_action(*workspace, crate::UndoVerdict);
    workspace
        .update(cx, |workspace, _, _| {
            let mutation =
                take_desk_mutation(workspace, HostId::default()).expect("filing undo mutation");
            assert!(mutation.writes.iter().any(|write| write.id == dealt
                && write.property == rho_desk::cells::Property::Parent(None)));
        })
        .unwrap();
}

/// One tap of `shift` is the verdicts, over the card in view, and the
/// letters they used to steal stay vim's. This is the whole of the change:
/// the tap opens a menu that says what the keys are, and `d` in it writes
/// exactly what deal mode's `d` wrote.
#[gpui::test]
fn a_tap_of_shift_opens_the_verdicts_over_the_card_in_view(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let dealt = desk.due_note(None, "Card in view");

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.open_deal_mode(window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();
    cx.run_until_parked();

    cx.simulate_keystrokes(*workspace, "shift");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert!(
                workspace.verdict_transient_open(),
                "a tap of shift over a card opens the verdicts"
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

/// Snooze end to end through the transient. It was an operator in deal
/// mode, `ss` for a day and `45sm` for forty-five minutes; the count now
/// goes inside the menu, where the units are written down, and lands on
/// exactly the same time.
#[gpui::test]
fn a_snooze_goes_through_the_transient_with_its_count(cx: &mut TestAppContext) {
    use crate::workspace::{SnoozeUnit, snooze_target};

    cx.update(bind_test_keymaps);
    // The keys as fingers type them, and the time the same span lands on
    // when the workspace works it out for itself.
    for (keys, unit, count) in [
        ("shift s s", SnoozeUnit::Days, 1usize),
        ("shift s 7 d", SnoozeUnit::Days, 7),
        ("shift s 4 5 m", SnoozeUnit::Minutes, 45),
    ] {
        // A card apiece: a verdict waits on the daemon before the deal
        // moves on, so one desk cannot hold three of them.
        let mut desk = DeskFixture::new();
        desk.due_note(None, "Card to snooze");
        let workspace = test_workspace(cx);
        workspace
            .update(cx, |workspace, window, cx| {
                workspace.handle_event(HostId::default(), desk.synced(), window, cx);
                workspace.open_deal_mode(window, cx);
                workspace.take_host_messages_for_test(HostId::default());
            })
            .unwrap();
        cx.run_until_parked();

        let (expected, said) = snooze_target(unit, count as i64, chrono::Local::now());
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

/// The second tap of `shift` is Home: the first tap put the verdicts on
/// screen and the menu's own `shift` row says the next one leaves. There
/// is no timer on it, unlike the old double tap, because the menu is on
/// screen saying what it does.
#[gpui::test]
fn a_second_tap_of_shift_leaves_the_card_for_home(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    desk.due_note(None, "Card in view");

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.open_deal_mode(window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    cx.simulate_keystrokes(*workspace, "shift");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert!(workspace.verdict_transient_open());
            assert_ne!(workspace.current_surface_name_for_test(), "home");
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "shift");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert!(
                !workspace.verdict_transient_open(),
                "the second tap closes the verdicts"
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.open_deal_mode(window, cx);
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.open_deal_mode(window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();
    cx.run_until_parked();

    cx.dispatch_action(*workspace, crate::DashboardDealFile);
    cx.dispatch_action(*workspace, crate::MinibufferCancel);
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            assert!(
                take_desk_mutation(workspace, HostId::default()).is_none(),
                "a cancelled prompt files nothing"
            );
            assert_eq!(
                workspace.current_deal_card_for_test().map(|card| card.0),
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.open_deal_mode(window, cx);
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
                    workspace.handle_event(
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
                    workspace.handle_event(
                        HostId::default(),
                        ConnEvent::DeskMutationAccepted { stamp: undo_stamp },
                        window,
                        cx,
                    );
                    assert_eq!(
                        workspace.current_deal_card_for_test().map(|card| card.0),
                        Some(crate::dashboard::DealCardId {
                            host: HostId::default(),
                            node_id: note.clone(),
                        })
                    );
                    assert_eq!(
                        workspace.rendered_deal_card_for_test(),
                        workspace.current_deal_card_for_test()
                    );
                    assert!(workspace.dashboard_deal_mode_for_test());
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
            let mut replacement = workspace.current_deal_card_value_for_test().unwrap();
            let other = rho_desk::cells::Id::Note(rho_desk::cells::Uuid([231; 16]));
            replacement.identity = crate::dashboard::DealCardId {
                host: HostId::default(),
                node_id: other.clone(),
            };
            replacement.topic_node_id = other.clone();
            replacement.agent_id = None;
            workspace.reopen_deal_for_test(replacement);
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskMutationAccepted { stamp: delayed },
                window,
                cx,
            );
            assert_eq!(
                workspace.current_deal_card_for_test().map(|card| card.0),
                Some(crate::dashboard::DealCardId {
                    host: HostId::default(),
                    node_id: other,
                })
            );
            assert_eq!(workspace.echo_text_for_test(), Some("done: Named card"));
        })
        .unwrap();
}

#[gpui::test]
fn double_shift_opens_home(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["current"], window, cx);
            assert_eq!(workspace.current_surface_name_for_test(), "current");
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "shift shift");
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "home");
        })
        .unwrap();
    // Pressing it again is the way back to what the reader was reading.
    cx.simulate_keystrokes(*workspace, "shift shift");
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
    let folds = concealed_ranges(&workspace, &editor, cx);
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
    assert_eq!(concealed_ranges(&workspace, &editor, cx), folds);

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
        snapshot_frame(state(
            vec![user("go")],
            vec![assistant(original, Some(UiMessagePhase::FinalAnswer))],
        )),
    );
    cx.run_until_parked();

    let editor = active_editor(&workspace, cx);
    let before = concealed_ranges(&workspace, &editor, cx);
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

#[gpui::test]
fn tree_desk_composes_one_native_buffer_per_node(cx: &mut TestAppContext) {
    // A note with a child note and a machine agent row: one buffer each,
    // composed into the one editor the user types in.
    let mut desk = DeskFixture::new();
    let parent = desk.note(None, "Parent");
    let child = desk.note(Some(parent.clone()), "body");
    let agent_row = desk.agent_row(parent.clone(), agent(31));
    desk.set(
        parent.clone(),
        rho_desk::cells::Property::DeferUntil(Some(rho_desk::cells::Timestamp {
            unix_ms: 1_772_323_200_000,
            precision: rho_desk::cells::TimestampPrecision::Day,
        })),
    );
    desk.set(
        child.clone(),
        rho_desk::cells::Property::Deadline(Some(rho_desk::cells::Timestamp {
            unix_ms: 1_772_323_200_000,
            precision: rho_desk::cells::TimestampPrecision::Day,
        })),
    );

    cx.update(bind_test_keymaps);
    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
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
        .unwrap();
    assert_eq!(text, "Parent\nbody\n");
    workspace
        .update(cx, |workspace, _, cx| {
            // The dated note and its dated child each carry one hint.
            assert_eq!(workspace.dashboard_editor().read(cx).eol_hints().len(), 2);
            assert!(
                workspace
                    .tree_buffer_for_test(HostId::default(), agent_row.clone())
                    .is_some(),
                "the machine row still gets its own derived buffer"
            );
        })
        .unwrap();

    // `* ` at the start of a line is the one recognition kept: it creates a
    // note rather than leaving stars in the text.
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.focus_tree_node_for_test(HostId::default(), child, window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "i * space F a s t escape");
    cx.run_until_parked();
    let recognized = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .dashboard_editor()
                .read(cx)
                .buffer()
                .read(cx)
                .snapshot(cx)
                .text()
        })
        .unwrap();
    assert!(!recognized.contains("* "), "tree text: {recognized:?}");
    let created = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .tree_nodes_for_test(HostId::default(), cx)
                .into_iter()
                .into_iter()
                .find_map(|(node_id, _, text)| {
                    (matches!(node_id, rho_desk::cells::Id::Note(_)) && text.is_empty())
                        .then_some(node_id)
                })
                .expect("recognition created a note")
        })
        .unwrap();

    // Vim search is hosted by the composed editor; its query is never input.
    let before_search = workspace
        .update(cx, |workspace, _, cx| {
            workspace.tree_nodes_for_test(HostId::default(), cx)
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "/ P a r enter");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(
                workspace.tree_nodes_for_test(HostId::default(), cx),
                before_search
            );
        })
        .unwrap();

    // `dd` on a row is one cell write, and `u` puts that cell back.
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.focus_tree_node_for_test(HostId::default(), created.clone(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "escape d d");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert!(
                !workspace
                    .tree_nodes_for_test(HostId::default(), cx)
                    .iter()
                    .any(|(node_id, _, _)| *node_id == created)
            );
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "u");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert!(
                workspace
                    .tree_nodes_for_test(HostId::default(), cx)
                    .iter()
                    .any(|(node_id, _, _)| *node_id == created),
                "undo restores the deleted cell"
            );
        })
        .unwrap();

    // alt-enter is the structural `o`: a new note, undone by one `u`.
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.focus_tree_node_for_test(HostId::default(), parent.clone(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "alt-enter n e w escape");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert!(
                workspace
                    .tree_nodes_for_test(HostId::default(), cx)
                    .iter()
                    .any(
                        |(id, _, text)| matches!(id, rho_desk::cells::Id::Note(_)) && text == "new"
                    )
            );
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "u");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert!(
                !workspace
                    .tree_nodes_for_test(HostId::default(), cx)
                    .iter()
                    .any(
                        |(id, _, text)| matches!(id, rho_desk::cells::Id::Note(_)) && text == "new"
                    )
            );
        })
        .unwrap();

    // Deleting a note leaves its machine row alone: the materializer roots
    // the orphan instead of the client tombstoning what it does not own.
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.take_host_messages_for_test(HostId::default());
            workspace.focus_tree_node_for_test(HostId::default(), parent.clone(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "escape d d");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            let nodes = workspace.tree_nodes_for_test(HostId::default(), cx);
            assert!(
                nodes
                    .iter()
                    .any(|(node_id, parent_id, _)| *node_id == agent_row && parent_id.is_none()),
                "post-delete nodes: {nodes:?}"
            );
        })
        .unwrap();

    // A rejected mutation takes its optimistic view back.
    let rejected = workspace
        .update(cx, |workspace, _, _| {
            take_desk_mutation(workspace, HostId::default())
                .expect("delete mutation")
                .stamp
        })
        .unwrap();
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskMutationRejected {
                    stamp: rejected,
                    reason: "test conflict".into(),
                },
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            let nodes = workspace.tree_nodes_for_test(HostId::default(), cx);
            assert!(
                nodes.iter().any(|(node_id, _, _)| *node_id == parent),
                "rejection restores the last merged cells: {nodes:?}"
            );
        })
        .unwrap();
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
                workspace.handle_event(HostId::default(), desk.synced(), window, cx);
                workspace.open_deal_mode(window, cx);
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
            workspace.handle_event(
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
            workspace.handle_event(
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
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

fn assert_rendered_deal_matches_current(
    workspace: &WindowHandle<Workspace>,
    cx: &mut TestAppContext,
) -> crate::dashboard::DealCardKind {
    workspace
        .update(cx, |workspace, _, _| {
            let current = workspace
                .current_deal_card_for_test()
                .expect("current deal card");
            let rendered = workspace
                .rendered_deal_card_for_test()
                .expect("rendered deal body");
            assert_eq!(
                rendered, current,
                "rendered body diverged from verdict target"
            );
            current.1
        })
        .unwrap()
}

#[gpui::test]
fn unnamed_legacy_gpt_quota_is_visible_to_the_status_line(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
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

#[gpui::test]
fn history_back_twice_then_forward_once_lands_on_the_middle_entry(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["one", "two", "three"], window, cx);
            workspace.step_surface_back_for_test(window, cx);
            workspace.step_surface_back_for_test(window, cx);
            assert!(workspace.step_surface_forward_for_test(window, cx));
            assert_eq!(workspace.current_surface_name_for_test(), "two");
        })
        .unwrap();
}

#[gpui::test]
fn history_forward_entries_do_not_reappear_as_backward_duplicates(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["one", "two", "three"], window, cx);
            workspace.step_surface_back_for_test(window, cx);
            workspace.step_surface_back_for_test(window, cx);
            workspace.step_surface_forward_for_test(window, cx);
            workspace.step_surface_back_for_test(window, cx);
            assert_eq!(workspace.current_surface_name_for_test(), "three");
            assert_eq!(
                workspace.surface_history_for_test(),
                (vec!["three".into(), "two".into(), "one".into()], 0)
            );
        })
        .unwrap();
}

#[gpui::test]
fn ordinary_open_of_recorded_surface_moves_the_history_cursor_without_reordering(
    cx: &mut TestAppContext,
) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["one", "two", "three"], window, cx);
            workspace.open_history_index_for_test(0, cx);
            assert_eq!(workspace.current_surface_name_for_test(), "three");
            assert_eq!(workspace.surface_history_for_test().1, 0);
            assert!(workspace.step_surface_forward_for_test(window, cx));
            assert_eq!(workspace.current_surface_name_for_test(), "two");
        })
        .unwrap();
}

#[gpui::test]
fn deal_and_overview_append_at_the_end_and_dedupe_existing_entries(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["one", "two", "three"], window, cx);
            workspace.step_surface_back_for_test(window, cx);
            workspace.show_current_history_for_test(crate::journal::SurfaceShowMethod::Deal, cx);
            assert_eq!(
                workspace.surface_history_for_test(),
                (vec!["three".into(), "one".into(), "two".into()], 2)
            );

            workspace
                .show_current_history_for_test(crate::journal::SurfaceShowMethod::Overview, cx);
            assert_eq!(
                workspace.surface_history_for_test(),
                (vec!["three".into(), "one".into(), "two".into()], 2)
            );
        })
        .unwrap();
}

#[gpui::test]
fn q_closes_unlisted_standalone_draft_surface(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["previous"], window, cx);
            workspace.select_agent(None, window, cx);
            assert!(!workspace.overview_open_for_test());
            assert_eq!(workspace.current_surface_name_for_test(), "draft");
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
            assert_ne!(
                workspace.current_surface_name_for_test(),
                "draft",
                "the home key resurrected the closed draft"
            )
        })
        .unwrap();
}

#[gpui::test]
fn q_discards_a_heading_draft_from_surface_history(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    let heading = desk.note(None, "unstaffed heading");
    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            // Opening the composer from the overview records Draft in
            // history, matching the state that exposed the human QA failure.
            workspace.select_agent(None, window, cx);
            assert!(
                workspace
                    .surface_history_for_test()
                    .0
                    .contains(&"draft".to_owned())
            );
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, window, cx| {
            // Heading drafts live on the map, which the home key no longer
            // opens; `r` on an unstaffed heading is what writes one now.
            workspace.open_overview(window, cx);
            workspace.focus_tree_node_for_test(HostId::default(), heading, window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "r");
    workspace
        .update(cx, |workspace, _, _| {
            assert!(workspace.dashboard_has_new_draft_for_test())
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "q");
    workspace
        .update(cx, |workspace, _, _| {
            assert!(workspace.overview_open_for_test());
            assert!(!workspace.dashboard_has_new_draft_for_test());
            assert!(
                !workspace
                    .surface_history_for_test()
                    .0
                    .contains(&"draft".to_owned()),
                "discarded draft remained in surface history"
            );
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "f24");
    workspace
        .update(cx, |workspace, _, _| {
            assert!(
                workspace.overview_open_for_test(),
                "history reopened the discarded draft"
            )
        })
        .unwrap();
}

#[gpui::test]
fn discarding_a_heading_draft_preserves_non_draft_history_cursor(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    let heading = desk.note(None, "unstaffed heading");
    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.configure_surface_history_for_test(&["current"], window, cx);
            workspace.open_overview(window, cx);
            workspace.focus_tree_node_for_test(HostId::default(), heading, window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "r q f24");
    workspace
        .update(cx, |workspace, _, _| {
            assert!(!workspace.overview_open_for_test());
            assert_eq!(workspace.current_surface_name_for_test(), "current");
        })
        .unwrap();
}

#[gpui::test]
fn q_mid_list_removes_current_and_keeps_newer_entries_forward(cx: &mut TestAppContext) {
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
        .update(cx, |workspace, window, cx| {
            assert_eq!(workspace.current_surface_name_for_test(), "three");
            assert!(workspace.step_surface_forward_for_test(window, cx));
            assert_eq!(workspace.current_surface_name_for_test(), "one");
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
            assert_eq!(
                workspace.surface_history_for_test(),
                (vec!["three".into(), "two".into(), "one".into()], 1)
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
            assert!(!workspace.overview_open_for_test());
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
fn q_on_home_or_the_map_is_a_no_op(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    // Home is the floor: there is nothing under it to reveal.
    let workspace = test_workspace(cx);
    cx.simulate_keystrokes(*workspace, "q");
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "home");
        })
        .unwrap();

    let workspace = overview_workspace(cx);
    let before = workspace
        .update(cx, |workspace, _, _| {
            workspace.current_surface_name_for_test()
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "q");
    workspace
        .update(cx, |workspace, _, _| {
            assert!(workspace.overview_open_for_test());
            assert_eq!(workspace.current_surface_name_for_test(), before);
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

            let conversation = surface("RhoSlackConversation", mode);
            assert_eq!(
                routes("enter", &conversation),
                Some("rho_gui::SlackOpenRow")
            );
            assert_eq!(routes("i", &conversation), Some("rho_gui::SlackCompose"));
            assert_eq!(routes("s", &conversation), Some("rho_gui::SlackSearch"));
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
        // A whole sequence, for the keys that take a second stroke.
        let routes_all = |keys: &str, contexts: &[KeyContext]| {
            let strokes: Vec<Keystroke> = keys
                .split(' ')
                .map(|key| Keystroke::parse(key).unwrap())
                .collect();
            keymap
                .bindings_for_input(&strokes, contexts)
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.open_deal_mode(window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, window, cx| {
            assert!(workspace.dashboard_deal_mode_for_test());
            desk.set(dealt, rho_desk::cells::Property::DeferUntil(None));
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();

    cx.dispatch_action(*workspace, crate::DashboardDealDone);
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, window, cx| {
            let stamp = take_desk_mutation(workspace, HostId::default())
                .expect("verdict mutation")
                .stamp;
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskMutationAccepted { stamp },
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            assert!(
                !workspace.dashboard_deal_mode_for_test(),
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.open_deal_mode(window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(
                workspace.current_deal_card_for_test().map(|card| card.0),
                Some(crate::dashboard::DealCardId {
                    host: HostId::default(),
                    node_id: dealable,
                }),
                "the only card is the one with something to say"
            );
        })
        .unwrap();
}

/// Where a Slack unit's verdict lives: on the unit in the store, not
/// beside the mirror. A unit the user is done with has a cursor past
/// everything Slack has to say, and the backlog command, which is the one
/// place that acts on every card at once, sees only what is still open.
#[gpui::test]
fn a_slack_verdict_is_read_from_the_store_not_from_slack(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let open = desk.thread_row(None, "C1", "500.0");
    let settled = desk.thread_row(None, "C1", "600.0");
    desk.set(
        settled.clone(),
        rho_desk::cells::Property::SlackHandledThrough(rho_desk::cells::SlackTs(
            "600.0".to_owned(),
        )),
    );

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.set_slack_sources_for_test(
                HostId::default(),
                desk.slack_sources(),
                window,
                cx,
            );
            let cards = workspace.dashboard.open_thread_cards();
            assert_eq!(
                cards
                    .iter()
                    .map(|(card, _)| card.node_id.clone())
                    .collect::<Vec<_>>(),
                vec![open],
                "the done thread is closed by its node, whatever Slack still says"
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

/// Creation: `n` from anywhere, the area always asked with the node in
/// context as the first answer, so Enter alone files the new thing where
/// the reader already is.
#[gpui::test]
fn new_note_files_itself_under_the_area_the_cursor_is_on(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    let elsewhere = desk.note(None, "elsewhere");
    let context = desk.note(None, "the area in view");

    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.focus_tree_node_for_test(HostId::default(), context.clone(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            let areas = workspace
                .dashboard
                .area_candidates(&workspace.registry, &Default::default(), cx)
                .into_iter()
                .map(|(path, kind, _, _)| (path, kind))
                .collect::<Vec<_>>();
            assert!(areas.contains(&("the area in view".to_owned(), "note")));
            assert!(areas.contains(&("elsewhere".to_owned(), "note")));
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
                    == rho_desk::cells::Property::Parent(Some(context.clone()))),
                "Enter on the first row files the note where the cursor is"
            );
            assert!(
                !mutation.writes.iter().any(|write| write.property
                    == rho_desk::cells::Property::Parent(Some(elsewhere.clone()))),
                "no other area is written to"
            );
        })
        .unwrap();
}

/// The finder's candidate source over a real desk tree: every node arrives
/// as its full path, and submitting one opens the surface that path names.
#[gpui::test]
fn find_offers_every_node_as_a_path_and_opens_the_one_chosen(cx: &mut TestAppContext) {
    use rho_ui_proto::{
        AgentDisposition, AgentRole, AuthState, UiAgentFacts, UiAgentSummary, UiAttention,
        WorkspaceInfo,
    };

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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Ready {
                    agents: vec![UiAgentSummary {
                        agent_id,
                        parent_agent: None,
                        display_name: Some("warm agent".into()),
                        created_at: UnixMs(1),
                        updated_at: UnixMs(1),
                        role: AgentRole::default(),
                        workspace: WorkspaceInfo::UserCheckout {
                            repo: "/tmp".into(),
                        },
                        attention: UiAttention::Pending,
                        last_active: UnixMs(5),
                        facts: UiAgentFacts::default(),
                        hidden: false,
                        disposition: AgentDisposition::Pending,
                        last_user_message_text: String::new(),
                        activity: None,
                        turn_report: None,
                        labels: Vec::new(),
                    }],
                    iris_agent: None,
                    projects: Vec::new(),
                    auth: AuthState {
                        namespaces: Vec::new(),
                        disabled_namespaces: Vec::new(),
                        active_namespace: None,
                    },
                    machine_seed: 0,
                    agent_counter: 40,
                },
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
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

/// "Notes for this" from a surface that is not a note: the note is created
/// under the thing on screen, and pressing the key again returns to it
/// rather than making a second one.
#[gpui::test]
fn notes_for_this_files_a_note_under_the_surfaces_node(cx: &mut TestAppContext) {
    use rho_ui_proto::{
        AgentDisposition, AgentRole, AuthState, UiAgentFacts, UiAgentSummary, UiAttention,
        WorkspaceInfo,
    };

    cx.update(bind_test_keymaps);
    let agent_id = agent(77);
    let mut desk = DeskFixture::new();
    let topic = desk.note(None, "nixos");
    let agent_node = desk.agent_row(topic, agent_id);

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Ready {
                    agents: vec![UiAgentSummary {
                        agent_id,
                        parent_agent: None,
                        display_name: Some("warm agent".into()),
                        created_at: UnixMs(1),
                        updated_at: UnixMs(1),
                        role: AgentRole::default(),
                        workspace: WorkspaceInfo::UserCheckout {
                            repo: "/tmp".into(),
                        },
                        attention: UiAttention::Pending,
                        last_active: UnixMs(5),
                        facts: UiAgentFacts::default(),
                        hidden: false,
                        disposition: AgentDisposition::Pending,
                        last_user_message_text: String::new(),
                        activity: None,
                        turn_report: None,
                        labels: Vec::new(),
                    }],
                    iris_agent: None,
                    projects: Vec::new(),
                    auth: AuthState {
                        namespaces: Vec::new(),
                        disabled_namespaces: Vec::new(),
                        active_namespace: None,
                    },
                    machine_seed: 0,
                    agent_counter: 40,
                },
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
            assert_eq!(
                workspace
                    .desk_cells
                    .node(HostId::default(), &node_id)
                    .and_then(|node| node.parent),
                Some(agent_node),
                "the note is filed under the agent on screen"
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
struct DeskFixture {
    device: rho_desk::cells::DeviceId,
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
            device,
            store: rho_desk::cells::Store::new(device),
            bodies: Vec::new(),
            next_node: 0,
            slack_units: Vec::new(),
        }
    }

    fn note(&mut self, parent: Option<rho_desk::cells::Id>, text: &str) -> rho_desk::cells::Id {
        self.next_node += 1;
        let id = rho_desk::cells::Id::Note(rho_desk::cells::Uuid([self.next_node as u8; 16]));
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
            })
            .collect()
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
    let home = cx.add_window(|window, cx| crate::home::HomeView::new(window, cx));

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
        node_id: rho_desk::cells::Id::Note(rho_desk::cells::Uuid([(node) as u8; 16])),
    };
    let rows = crate::home::HomeRows {
        next: vec![crate::home::HomeRow {
            title: "#design › release date".to_owned(),
            label: "needs reply · 1.9h".to_owned(),
            card: card(1),
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "home");
            assert!(
                workspace.current_deal_card_for_test().is_none(),
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
fn enter_on_a_home_row_deals_that_card(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    desk.due_note(None, "First in the queue");
    let second = desk.due_note(None, "Second in the queue");
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
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
        .update(cx, |workspace, _, _| {
            assert!(
                workspace.dashboard_deal_mode_for_test(),
                "a row opens as a deal in every respect"
            );
            let (identity, _) = workspace
                .current_deal_card_for_test()
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

#[gpui::test]
fn a_running_agents_row_follows_its_last_line(cx: &mut TestAppContext) {
    use rho_ui_proto::{
        AgentDisposition, AgentRole, AuthState, UiAgentFacts, UiAgentSummary, UiAttention,
        WorkspaceInfo,
    };

    let running = agent(31);
    let mut desk = DeskFixture::new();
    let heading = desk.note(None, "phone feed");
    desk.agent_row(heading, running);
    let summary = |activity: &str| UiAgentSummary {
        agent_id: running,
        parent_agent: None,
        display_name: None,
        created_at: UnixMs(1),
        updated_at: UnixMs(1),
        role: AgentRole::default(),
        workspace: WorkspaceInfo::UserCheckout {
            repo: "/tmp".into(),
        },
        attention: UiAttention::Working,
        last_active: UnixMs(1),
        facts: UiAgentFacts {
            turn_running: true,
            last_user_message_at: UnixMs(chrono::Local::now().timestamp_millis() as u64),
            ..Default::default()
        },
        hidden: false,
        disposition: AgentDisposition::Pending,
        last_user_message_text: String::new(),
        activity: Some(activity.to_owned()),
        turn_report: None,
        labels: Vec::new(),
    };

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Ready {
                    agents: vec![summary("wiring the flick recogniser")],
                    iris_agent: None,
                    projects: Vec::new(),
                    auth: AuthState {
                        namespaces: Vec::new(),
                        disabled_namespaces: Vec::new(),
                        active_namespace: None,
                    },
                    machine_seed: 0,
                    agent_counter: 40,
                },
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
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Ready {
                    agents: vec![summary("unfurl box: background tint")],
                    iris_agent: None,
                    projects: Vec::new(),
                    auth: AuthState {
                        namespaces: Vec::new(),
                        disabled_namespaces: Vec::new(),
                        active_namespace: None,
                    },
                    machine_seed: 0,
                    agent_counter: 40,
                },
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
            workspace.open_deal_mode(window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_surface_name_for_test(), "home");
            assert!(
                workspace
                    .message_log_texts()
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
        .update(cx, |workspace, _, _| {
            assert!(workspace.current_deal_card_for_test().is_none());
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    // No motion: the first Enter deals rather than landing on a heading.
    cx.dispatch_action(*workspace, crate::HomeOpenRow);
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            let (identity, _) = workspace
                .current_deal_card_for_test()
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
    let area = desk.due_note(None, "the area in view");
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.handle_event(
                HostId::default(),
                ConnEvent::Ready {
                    agents: Vec::new(),
                    iris_agent: None,
                    // The one registered project is the workdir the draft
                    // inherits when the area names none.
                    projects: vec![rho_ui_proto::UiProject {
                        path: "/tmp/rho-test-repo".into(),
                        name: "rho".to_owned(),
                        description: String::new(),
                    }],
                    auth: rho_ui_proto::AuthState {
                        namespaces: Vec::new(),
                        disabled_namespaces: Vec::new(),
                        active_namespace: None,
                    },
                    machine_seed: 0,
                    agent_counter: 1,
                },
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
            assert!(
                !workspace.has_transient_for_test(),
                "the new-agent transient is retired: the draft page carries the fields"
            );
            assert_eq!(workspace.current_surface_name_for_test(), "draft");
            assert_eq!(
                workspace.draft_area_for_test(),
                Some((HostId::default(), area.clone()))
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
                Some((HostId::default(), area)),
                "the agent was not filed under the area"
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
                    .0
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
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

/// `mark read before` closes a backlog of threads in one keystroke, so it
/// comes back in one: `shift-u` reopens every node it closed, not the last
/// of them. Entered below the prompt, whose other half (the marking) is
/// tested against the fake Slack server.
#[gpui::test]
fn marking_the_backlog_closes_every_old_card_and_undoes_as_one(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let old = desk.thread_row(None, "C1", "100.0");
    let older = desk.thread_row(None, "C1", "50.0");

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.set_slack_sources_for_test(
                HostId::default(),
                desk.slack_sources(),
                window,
                cx,
            );
            let closed = workspace.mark_cards_done(
                HostId::default(),
                vec![old.clone(), older.clone()],
                "mark read before".to_owned(),
                window,
                cx,
            );
            assert_eq!(closed, 2);
            // Every unit's cursor moved, which is what "read before" means
            // for a Slack card: nothing is a state, and a page loading
            // under either of them cannot bring it back.
            for (channel, thread_ts) in [("C1", "100.0"), ("C1", "50.0")] {
                let facts = workspace
                    .desk_cells
                    .facts_of_slack_unit(
                        Some(HostId::default()),
                        &rho_desk::cells::SlackUnit {
                            workspace: "acme".to_owned(),
                            channel: channel.to_owned(),
                            thread: Some(thread_ts.to_owned()),
                        },
                    )
                    .unwrap();
                assert_eq!(
                    facts.slack_handled_through,
                    Some(rho_desk::cells::SlackTs(thread_ts.to_owned()))
                );
            }
            for node_id in [old.clone(), older.clone()] {
                assert!(
                    !workspace
                        .dashboard
                        .node_is_open(crate::dashboard::DealCardId {
                            host: HostId::default(),
                            node_id,
                        }),
                    "every old card is closed"
                );
            }
            assert_eq!(
                workspace.verdict_undo_count_for_test(),
                1,
                "one keystroke leaves one thing to undo"
            );

            workspace.undo_verdict(window, cx);
            for node_id in [old.clone(), older.clone()] {
                assert!(
                    workspace
                        .dashboard
                        .node_is_open(crate::dashboard::DealCardId {
                            host: HostId::default(),
                            node_id,
                        }),
                    "the undo reopens the whole batch"
                );
            }
            assert_eq!(workspace.verdict_undo_count_for_test(), 0);
        })
        .unwrap();
}

/// The whole point of the unit model: a card the user closed stays closed
/// whatever Slack sends next. A history page, a reconnect, a feed poll and
/// a restart all replay messages that were already there, and the only
/// question the card asks is whether there is something from them past the
/// cursor.
#[gpui::test]
fn a_done_slack_unit_is_not_reopened_by_anything_slack_replays(cx: &mut TestAppContext) {
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
    let source = |newest: &str, from_other: &str| {
        vec![crate::desk_view::SlackSource {
            unit: unit.clone(),
            title: "any update?".to_owned(),
            newest: rho_desk::cells::SlackTs(newest.to_owned()),
            newest_from_other: Some(rho_desk::cells::SlackTs(from_other.to_owned())),
        }]
    };

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.set_slack_sources_for_test(
                HostId::default(),
                source("600.0", "600.0"),
                window,
                cx,
            );
            assert!(workspace.dashboard.node_is_open(card.clone()));

            assert!(workspace.apply_verdict_for_test(
                HostId::default(),
                &node,
                crate::desk_view::DeskVerdict::Done,
                window,
                cx,
            ));
            assert!(
                !workspace.dashboard.node_is_open(card.clone()),
                "done closes the card"
            );
            // The cursor is what closed it, and the cursor is what a restart
            // reads back: no state was written on the unit at all.
            let facts = workspace
                .desk_cells
                .facts_of_slack_unit(Some(HostId::default()), &unit)
                .unwrap();
            assert_eq!(
                facts.slack_handled_through,
                Some(rho_desk::cells::SlackTs("600.0".to_owned()))
            );
            assert_eq!(facts.state, rho_desk::cells::State::Open);

            // A history page arriving under the card, and a feed poll
            // repeating an item at the cursor. Neither is news.
            workspace.set_slack_sources_for_test(
                HostId::default(),
                source("600.0", "300.0"),
                window,
                cx,
            );
            assert!(!workspace.dashboard.node_is_open(card.clone()));
            workspace.set_slack_sources_for_test(
                HostId::default(),
                source("600.0", "600.0"),
                window,
                cx,
            );
            assert!(!workspace.dashboard.node_is_open(card.clone()));

            // Someone writing past the cursor is news, and only that.
            workspace.set_slack_sources_for_test(
                HostId::default(),
                source("700.0", "700.0"),
                window,
                cx,
            );
            assert!(
                workspace.dashboard.node_is_open(card),
                "a message past the cursor raises the card again"
            );
        })
        .unwrap();
}

/// A snooze leaves the cursor alone, so the messages the user has not
/// handled are still theirs when it ends. What voids it is somebody writing
/// during the snooze: the card comes straight back.
#[gpui::test]
fn a_snooze_is_voided_by_a_newer_message_from_someone_else(cx: &mut TestAppContext) {
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
        }]
    };

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
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
                "and where the unit stood is what tells a new reply from an old one"
            );
            assert!(
                workspace.dashboard.node_defer_until(card.clone()).is_some(),
                "the card is put down until the snooze ends"
            );

            workspace.set_slack_sources_for_test(HostId::default(), source("700.0"), window, cx);
            assert!(
                workspace.dashboard.node_defer_until(card).is_none(),
                "somebody writing during the snooze voids it"
            );
        })
        .unwrap();
}

/// The store is one store: a done on the phone closes the card on the
/// laptop when the cells arrive, with no keystroke here.
#[gpui::test]
fn a_done_on_another_device_closes_the_card_here(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let node = desk.thread_row(None, "C1", "500.0");
    let card = crate::dashboard::DealCardId {
        host: HostId::default(),
        node_id: node.clone(),
    };

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.set_slack_sources_for_test(
                HostId::default(),
                desk.slack_sources(),
                window,
                cx,
            );
            assert!(workspace.dashboard.node_is_open(card.clone()));
        })
        .unwrap();

    // The other device moved the cursor past everything the mirror holds.
    desk.set(
        node,
        rho_desk::cells::Property::SlackHandledThrough(rho_desk::cells::SlackTs(
            "900.0".to_owned(),
        )),
    );
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            assert!(
                !workspace.dashboard.node_is_open(card),
                "the cursor arrived, so the card is gone here too"
            );
        })
        .unwrap();
}

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
        }]
    };

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.set_slack_sources_for_test(HostId::default(), source("600.0"), window, cx);
            assert!(workspace.dashboard.node_is_open(card.clone()));

            assert!(workspace.apply_verdict_for_test(
                HostId::default(),
                &node,
                crate::desk_view::DeskVerdict::Mute,
                window,
                cx,
            ));
            let facts = workspace
                .desk_cells
                .facts_of_slack_unit(Some(HostId::default()), &unit)
                .unwrap();
            assert_eq!(
                facts.slack_handled_through,
                Some(rho_desk::cells::SlackTs("600.0".to_owned())),
                "a mute handles what was there, like a done"
            );
            assert_eq!(
                facts.state,
                rho_desk::cells::State::Muted,
                "and says the unit itself is not wanted"
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
            assert_eq!(
                facts.slack_handled_through,
                Some(rho_desk::cells::SlackTs("600.0".to_owned())),
                "the cursor stands, so what was read is not offered again"
            );
            workspace.set_slack_sources_for_test(HostId::default(), source("900.0"), window, cx);
            assert!(
                workspace.dashboard.node_is_open(card),
                "the message that arrived while it was muted is a card again"
            );
        })
        .unwrap();
}

/// Undoing a mute puts both facts back: the state that kept the unit quiet
/// and the cursor the mute wrote. The Slack half of the undo, following the
/// thread again, is `undoing_a_discard_follows_the_thread_again` in
/// rho-slack's transport tests.
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
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
            assert_eq!(
                facts.slack_handled_through,
                Some(rho_desk::cells::SlackTs(String::new())),
                "the cursor goes back too, so the thread is owed again"
            );
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
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
    let on_the_map = desk.note(None, "map row");
    let note = desk.note(None, "first line");

    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.focus_tree_node_for_test(HostId::default(), on_the_map.clone(), window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "i t a i l enter escape");
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
        .update(cx, |workspace, _, cx| {
            assert_eq!(body(workspace, &on_the_map, cx), "tail\nmap row");
        })
        .unwrap();

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

/// `n n` from Home. A new note is a row on the map, so the map has to come
/// into view: with Home as the landing surface the row and its insert
/// cursor were both behind a surface that never appeared, and the title
/// the reader typed went nowhere.
#[gpui::test]
fn a_new_note_from_home_brings_the_map_into_view(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let mut desk = DeskFixture::new();
    let area = desk.note(None, "the area in view");

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.take_host_messages_for_test(HostId::default());
            assert_eq!(workspace.current_surface_name_for_test(), "home");
            assert!(!workspace.overview_open_for_test());
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
                take_desk_mutation(workspace, HostId::default()).expect("new note mutation");
            assert!(mutation.writes.iter().any(
                |write| write.property == rho_desk::cells::Property::Parent(Some(area.clone()))
            ));
            assert!(
                workspace.overview_open_for_test(),
                "the map is what the new row is on, so the map is what the reader sees"
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
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
/// label `rho`, both minted on the spot if they are new, and the thing then
/// hangs on the map in its place and under the label as well.
#[gpui::test]
fn a_label_is_named_by_path_and_puts_the_thing_in_a_second_place(cx: &mut TestAppContext) {
    use rho_desk::cells::{Id, Property};

    let mut desk = DeskFixture::new();
    let area = desk.note(None, "Verdict agent");
    let thing = desk.note(Some(area.clone()), "Deal QA note");

    cx.update(bind_test_keymaps);
    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
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

    // The map is a DAG drawn as a tree: the note is under the area it was
    // filed in and under the label it now carries, and both rows are real.
    workspace
        .update(cx, |workspace, _, _| {
            let places = workspace
                .desk_cells_snapshot_for_test(HostId::default())
                .into_iter()
                .filter(|node| node.id == thing)
                .map(|node| node.under)
                .collect::<Vec<_>>();
            assert_eq!(places, vec![Some(area.clone()), Some(label.clone())]);
        })
        .unwrap();

    // Both places are drawn, each with its own bullet. One buffer in two
    // excerpts is the thing that used to go wrong here: the prefix is
    // positioned per excerpt, so the second row is not left bare.
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_overview(window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    let map = workspace
        .update(cx, |workspace, _, cx| {
            workspace.dashboard_display_text_for_test(cx)
        })
        .unwrap();
    // Each place carries its own bullet, and each row starts past the row
    // it hangs under: the label `agent` under `rho`, and the note under
    // `agent` past that. A label nests the way a note does.
    assert_eq!(
        map,
        "* Verdict agent\n** Deal QA note\n  ◦ rho\n      ◦ agent\n        * Deal QA note"
    );

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
        })
        .unwrap();
}

/// The one picker offers both axes: the labels a thing can carry and the
/// places it can sit in, so `f` is the only filing key there is.
#[gpui::test]
fn filing_offers_labels_as_well_as_notes(cx: &mut TestAppContext) {
    let mut desk = DeskFixture::new();
    let area = desk.note(None, "Verdict agent");
    let dealt = desk.due_note(None, "Deal QA note");

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.label_card(HostId::default(), area.clone(), "rho", window, cx);
            workspace.open_deal_mode(window, cx);
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
                workspace
                    .filing_destinations_for_test()
                    .iter()
                    .any(|(path, ..)| path == "Verdict agent"),
                "notes are still offered"
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            workspace.open_deal_mode(window, cx);
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
            workspace.handle_event(
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
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
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
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
                    .any(|name| name.starts_with("rho/agent")),
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
    let project = desk.note(None, "Release research");
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
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
                crate::journal::CreateMethod::New,
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
                crate::journal::CreateMethod::New,
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
    // A second card so the queue is not emptied by the skip that leaving the
    // deal for Home performs: Home needs a cursor of its own for this.
    let queued = desk.due_note(None, "Queued QA note");
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(HostId::default(), desk.synced(), window, cx);
            // The reader opened the search page, which is what puts it on
            // the map; the tab ctrl-clicked out of it needs nothing written.
            workspace.file_page(
                origin,
                None,
                crate::journal::CreateMethod::TabBirth,
                window,
                cx,
            );
            workspace.sync_tree_dashboard(HostId::default(), window, cx);
            workspace.open_deal_mode(window, cx);
            // The cursor is left on the page row while the card is dealt:
            // the surface in view still decides, in both directions.
            workspace.focus_tree_node_for_test(HostId::default(), desk_page(origin), window, cx);
            assert_eq!(
                workspace.label_target(cx),
                Some((HostId::default(), dealt.clone())),
                "the dealt surface keeps its own card"
            );
        })
        .unwrap();
    cx.run_until_parked();

    // The reader leaves the deal for Home and opens the map over it. Home
    // keeps a cursor of its own on the queue; the map is the overlay in
    // front, so the row under its cursor is what the reader is looking at.
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.open_home(window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, window, cx| {
            // Home puts its own cursor on the top of the queue, which is the
            // trap: the map is what the reader is looking at.
            let home = workspace.home_view().expect("Home is the surface");
            assert_eq!(
                home.update(cx, |home, cx| home.cursor_target(cx)),
                crate::home::HomeTarget::Card(crate::dashboard::DealCardId {
                    host: HostId::default(),
                    node_id: queued.clone(),
                }),
                "Home is left holding the queue's top card"
            );
            workspace.open_overview(window, cx);
            workspace.focus_tree_node_for_test(HostId::default(), desk_page(origin), window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, window, cx| {
            assert_eq!(
                workspace.label_target(cx),
                Some((HostId::default(), desk_page(origin))),
                "the row under the cursor is what a verdict is about"
            );
            assert!(
                workspace.open_verdict_transient(window, cx),
                "the tap opens over a row the dealer has no card for"
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
                mutation
                    .writes
                    .iter()
                    .any(|write| write.id == desk_page(origin)
                        && write.property
                            == Property::Labeled {
                                label: label.clone(),
                                present: true,
                            }),
                "the page the reader is on is what gets labelled"
            );
            assert!(
                !mutation
                    .writes
                    .iter()
                    .any(|write| write.id == dealt || write.id == queued),
                "and the cards the dealer and Home held are left alone"
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
