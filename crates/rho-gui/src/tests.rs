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
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, _, _| {
            for text in ["First phone card", "Second phone card"] {
                let id = workspace.append_inbox_for_test(crate::inbox::InboxDraft {
                    kind: crate::inbox::InboxKind::Capture,
                    text: text.into(),
                    source: crate::inbox::SourceReference::None,
                    context: crate::inbox::CapturedContext::default(),
                    waiting_on: None,
                });
                workspace.age_inbox_for_test(&id, 0);
            }
        })
        .unwrap();

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
fn leaving_phone_mode_cancels_a_delayed_flick_commit(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, _, _| {
            for text in ["First resize card", "Second resize card"] {
                let id = workspace.append_inbox_for_test(crate::inbox::InboxDraft {
                    kind: crate::inbox::InboxKind::Capture,
                    text: text.into(),
                    source: crate::inbox::SourceReference::None,
                    context: crate::inbox::CapturedContext::default(),
                    waiting_on: None,
                });
                workspace.age_inbox_for_test(&id, 0);
            }
        })
        .unwrap();
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
    let workspace = test_workspace(cx);
    let id = workspace
        .update(cx, |workspace, _, _| {
            let id = workspace.append_inbox_for_test(crate::inbox::InboxDraft {
                kind: crate::inbox::InboxKind::Capture,
                text: "Keep this phone card".into(),
                source: crate::inbox::SourceReference::None,
                context: crate::inbox::CapturedContext::default(),
                waiting_on: None,
            });
            workspace.age_inbox_for_test(&id, 0);
            id
        })
        .unwrap();
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
            assert_eq!(
                workspace.current_deal_card_for_test().unwrap().0,
                crate::dashboard::DealCardIdentity::Inbox(id.0)
            );
        })
        .unwrap();
}

#[gpui::test]
fn phone_back_from_a_surface_reveals_the_hidden_feed_card(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let expected = workspace
        .update(cx, |workspace, _, _| {
            let id = workspace.append_inbox_for_test(crate::inbox::InboxDraft {
                kind: crate::inbox::InboxKind::Capture,
                text: "Feed stays put".into(),
                source: crate::inbox::SourceReference::None,
                context: crate::inbox::CapturedContext::default(),
                waiting_on: None,
            });
            workspace.age_inbox_for_test(&id, 0);
            crate::dashboard::DealCardIdentity::Inbox(id.0)
        })
        .unwrap();
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
    let workspace = test_workspace(cx);
    let expected = workspace
        .update(cx, |workspace, _, _| {
            let id = workspace.append_inbox_for_test(crate::inbox::InboxDraft {
                kind: crate::inbox::InboxKind::Capture,
                text: "Last phone card".into(),
                source: crate::inbox::SourceReference::None,
                context: crate::inbox::CapturedContext::default(),
                waiting_on: None,
            });
            workspace.age_inbox_for_test(&id, 0);
            crate::dashboard::DealCardIdentity::Inbox(id.0)
        })
        .unwrap();
    cx.simulate_window_resize(*workspace, gpui::size(gpui::px(400.), gpui::px(800.)));
    cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
        .unwrap();
    cx.run_until_parked();
    cx.dispatch_action(*workspace, crate::DashboardDealDone);
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
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.current_deal_card_for_test().unwrap().0, expected);
            assert!(workspace.phone_feed_is_active_for_test());
        })
        .unwrap();
}

#[gpui::test]
fn phone_blocks_navigation_while_a_tree_verdict_is_pending(cx: &mut TestAppContext) {
    use rho_desk::{
        BatchOpRecord, Document, NodeId, NodeKind, NodeOwner, OrderKey, Replica, ReplicaAuthor,
        TextOperation, TreeClock, TreeOperation,
    };

    let mut document = Document::default();
    document.add_replica(Replica {
        replica_id: 1,
        author: ReplicaAuthor::Machine,
    });
    let heading = NodeId {
        replica_id: 1,
        counter: 1,
    };
    document
        .apply(TreeOperation::Create {
            timestamp: TreeClock {
                value: 1,
                replica_id: 1,
            },
            node_id: heading,
            kind: NodeKind::Heading,
            owner: NodeOwner::User,
            parent: None,
            order: OrderKey(vec![100]),
        })
        .unwrap();
    document
        .apply(TreeOperation::SetTemporal {
            timestamp: TreeClock {
                value: 2,
                replica_id: 1,
            },
            node_id: heading,
            kind: rho_desk::TemporalKind::Todo,
            value: Some(rho_desk::TemporalMark {
                year: 2020,
                month: 1,
                day: 1,
                minute_of_day: None,
                pace_days: 1,
            }),
        })
        .unwrap();
    let mut title = text::Buffer::new(text::ReplicaId::new(1), text::BufferId::new(1).unwrap(), "");
    document
        .apply_text(
            heading,
            TextOperation::from_text(&title.edit([(0..0, "Pending phone verdict")])),
            None,
        )
        .unwrap();

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskTreeSnapshot {
                    snapshot: document.snapshot(),
                    replica_id: 42,
                },
                window,
                cx,
            );
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
    let verdict_batch = workspace
        .update(cx, |workspace, _, _| {
            workspace
                .take_host_messages_for_test(HostId::default())
                .into_iter()
                .find_map(|message| match message {
                    rho_ui_proto::ClientMessage::DeskTreeBatchApply { batch } => Some(batch),
                    _ => None,
                })
                .expect("tree verdict batch")
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
                        rho_ui_proto::ClientMessage::DeskTreeBatchApply { .. }
                    ))
            );
        })
        .unwrap();

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskTreeBatchApplied(BatchOpRecord {
                    sequence: 1,
                    timestamp_ms: 1,
                    batch: verdict_batch,
                    daemon_tree_operations: Vec::new(),
                }),
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
    let undo_batch = workspace
        .update(cx, |workspace, _, _| {
            workspace
                .take_host_messages_for_test(HostId::default())
                .into_iter()
                .find_map(|message| match message {
                    rho_ui_proto::ClientMessage::DeskTreeBatchApply { batch } => Some(batch),
                    _ => None,
                })
                .expect("tree verdict undo batch")
        })
        .unwrap();
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskTreeBatchApplied(BatchOpRecord {
                    sequence: 2,
                    timestamp_ms: 2,
                    batch: undo_batch,
                    daemon_tree_operations: Vec::new(),
                }),
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

#[gpui::test]
fn native_page_deal_context_routes_verdict_keys(cx: &mut TestAppContext) {
    use gpui::{KeyContext, Keystroke};

    cx.update(bind_test_keymaps);
    cx.update(|cx| {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let contexts = [
            KeyContext::parse("RhoGuiDeal").unwrap(),
            KeyContext::parse("RhoBrowser").unwrap(),
        ];
        macro_rules! assert_route {
            ($key:literal, $action:expr) => {{
                let (bindings, pending) =
                    keymap.bindings_for_input(&[Keystroke::parse($key).unwrap()], &contexts);
                assert!(!pending);
                assert!(
                    bindings
                        .first()
                        .is_some_and(|binding| binding.action().partial_eq(&$action)),
                    "{} did not route to {} in a dealt browser page: {bindings:?}",
                    $key,
                    gpui::Action::name(&$action),
                );
            }};
        }
        assert_route!("q", crate::SurfaceClose);
        assert_route!("d", crate::DashboardDealDone);
        assert_route!("x", crate::DashboardDealDiscard);
        assert_route!("s", crate::DashboardDealSnooze);
        assert_route!("t", crate::DashboardDealTodo);
        assert_route!("shift-u", crate::UndoVerdict);
    });
}

#[gpui::test]
fn undo_verdict_binding_is_confined_to_deal_normal_mode(cx: &mut TestAppContext) {
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
            KeyContext::parse("Editor VimDeal vim_mode=normal vim_operator=none").unwrap(),
        ]));
        assert!(resolves(&[
            KeyContext::parse("RhoGui").unwrap(),
            KeyContext::parse("Editor VimDeal vim_mode=helix_normal vim_operator=none").unwrap(),
        ]));
        assert!(!resolves(&[
            KeyContext::parse("RhoDashboard").unwrap(),
            KeyContext::parse("Editor VimDeal vim_mode=insert vim_operator=none").unwrap(),
        ]));
        assert!(!resolves(&[
            KeyContext::parse("RhoGui").unwrap(),
            KeyContext::parse("Editor VimDeal vim_mode=normal vim_operator=delete").unwrap(),
        ]));
    });
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
    let workspace = test_workspace(cx);
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
    use rho_desk::{
        Binding, BindingKind, Document, NodeId, NodeKind, NodeOwner, OrderKey, Replica,
        ReplicaAuthor, TextOperation, TreeClock, TreeOperation,
    };
    use rho_ui_proto::{
        AgentDisposition, AgentRole, AuthState, ClientMessage, UiAgentFacts, UiAgentSummary,
        UiAttention, WorkspaceInfo,
    };

    let ids = [agent(21), agent(22), agent(23), agent(24)];
    let mut document = Document::default();
    document.add_replica(Replica {
        replica_id: 1,
        author: ReplicaAuthor::Machine,
    });
    for (index, agent_id) in ids.iter().copied().enumerate() {
        let heading = NodeId {
            replica_id: 1,
            counter: index as u64 * 2 + 1,
        };
        let row = NodeId {
            replica_id: 1,
            counter: index as u64 * 2 + 2,
        };
        for (offset, node_id, kind, owner, parent) in [
            (0, heading, NodeKind::Heading, NodeOwner::User, None),
            (1, row, NodeKind::Agent, NodeOwner::Machine, Some(heading)),
        ] {
            document
                .apply(TreeOperation::Create {
                    timestamp: TreeClock {
                        value: (index * 3 + offset + 1) as u32,
                        replica_id: 1,
                    },
                    node_id,
                    kind,
                    owner,
                    parent,
                    order: OrderKey(vec![(index as u16 + 1) * 20]),
                })
                .unwrap();
        }
        document
            .apply(TreeOperation::SetBinding {
                timestamp: TreeClock {
                    value: (index * 3 + 3) as u32,
                    replica_id: 1,
                },
                node_id: row,
                kind: BindingKind::Agent,
                value: Some(Binding::Agent(agent_id)),
            })
            .unwrap();
        let mut title = text::Buffer::new(
            text::ReplicaId::new(1),
            text::BufferId::new(index as u64 + 1).unwrap(),
            "",
        );
        document
            .apply_text(
                heading,
                TextOperation::from_text(
                    &title.edit([(0..0, format!("Warm agent {}", index + 1))]),
                ),
                None,
            )
            .unwrap();
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
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskTreeSnapshot {
                    snapshot: document.snapshot(),
                    replica_id: 42,
                },
                window,
                cx,
            );
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
            let inbox = workspace.append_inbox_for_test(crate::inbox::InboxDraft {
                kind: crate::inbox::InboxKind::Capture,
                text: "not a transcript".into(),
                source: crate::inbox::SourceReference::None,
                context: crate::inbox::CapturedContext::default(),
                waiting_on: None,
            });
            workspace.age_inbox_for_test(&inbox, 0);
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
    let workspace = test_workspace(cx);
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
    let first = rho_desk::NodeId {
        replica_id: 7,
        counter: 1,
    };
    let second = rho_desk::NodeId {
        replica_id: 7,
        counter: 2,
    };
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
            second,
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
fn deal_file_bare_enter_uses_the_offered_heading_completion(cx: &mut TestAppContext) {
    use rho_desk::{
        Document, NodeId, NodeKind, NodeOwner, OrderKey, Replica, ReplicaAuthor, TextOperation,
        TreeClock, TreeOperation,
    };

    let mut document = Document::default();
    document.add_replica(Replica {
        replica_id: 1,
        author: ReplicaAuthor::Machine,
    });
    let destination = NodeId {
        replica_id: 1,
        counter: 1,
    };
    document
        .apply(TreeOperation::Create {
            timestamp: TreeClock {
                value: 1,
                replica_id: 1,
            },
            node_id: destination,
            kind: NodeKind::Heading,
            owner: NodeOwner::User,
            parent: None,
            order: OrderKey(vec![100]),
        })
        .unwrap();
    let mut title = text::Buffer::new(text::ReplicaId::new(1), text::BufferId::new(1).unwrap(), "");
    document
        .apply_text(
            destination,
            TextOperation::from_text(&title.edit([(0..0, "Verdict agent")])),
            None,
        )
        .unwrap();

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    let inbox_id = workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskTreeSnapshot {
                    snapshot: document.snapshot(),
                    replica_id: 42,
                },
                window,
                cx,
            );
            let id = workspace.append_inbox_for_test(crate::inbox::InboxDraft {
                kind: crate::inbox::InboxKind::Capture,
                text: "Inbox QA item".into(),
                source: crate::inbox::SourceReference::None,
                context: crate::inbox::CapturedContext::default(),
                waiting_on: None,
            });
            workspace.age_inbox_for_test(&id, 0);
            workspace.open_deal_mode(window, cx);
            id
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert!(matches!(
                workspace.current_deal_card_for_test(),
                Some((
                    crate::dashboard::DealCardIdentity::Inbox(_),
                    crate::dashboard::DealCardKind::Inbox(_)
                ))
            ));
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();

    cx.dispatch_action(*workspace, crate::DashboardDealFile);
    cx.run_until_parked();
    // The completion is visibly selected but untouched, exactly as in the
    // dealer flow: bare Enter must accept it rather than submit an empty name.
    cx.dispatch_action(*workspace, crate::MinibufferConfirm);
    cx.run_until_parked();
    let messages = workspace
        .update(cx, |workspace, _, _| {
            workspace.take_host_messages_for_test(HostId::default())
        })
        .unwrap();
    assert!(messages.iter().any(|message| matches!(
        message,
        rho_ui_proto::ClientMessage::DeskTreeApply {
            operation: TreeOperation::Create {
                parent: Some(parent),
                ..
            }
        } if *parent == destination
    )));
    assert!(messages.iter().any(|message| matches!(
        message,
        rho_ui_proto::ClientMessage::DeskNodeTextApply { .. }
    )));
    let created = messages
        .iter()
        .find_map(|message| match message {
            rho_ui_proto::ClientMessage::DeskTreeApply {
                operation: TreeOperation::Create { node_id, .. },
            } => Some(*node_id),
            _ => None,
        })
        .unwrap();
    let (title_operation, title_transaction) = messages
        .iter()
        .find_map(|message| match message {
            rho_ui_proto::ClientMessage::DeskNodeTextApply {
                node_id,
                operation,
                transaction,
            } if *node_id == created => Some((operation.clone(), transaction.clone())),
            _ => None,
        })
        .expect("filing title edit");
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskNodeTextApplied(rho_desk::TextOpRecord {
                    sequence: 1,
                    timestamp_ms: 1,
                    node_id: created,
                    operation: title_operation,
                    transaction: title_transaction,
                }),
                window,
                cx,
            );
            assert_eq!(workspace.verdict_undo_count_for_test(), 1);
            assert!(workspace.inbox_item_for_test(&inbox_id).is_none());
        })
        .unwrap();
    cx.dispatch_action(*workspace, crate::UndoVerdict);
    let undo_batch = workspace
        .update(cx, |workspace, _, _| {
            workspace
                .take_host_messages_for_test(HostId::default())
                .into_iter()
                .find_map(|message| match message {
                    rho_ui_proto::ClientMessage::DeskTreeBatchApply { batch } => Some(batch),
                    _ => None,
                })
                .expect("filing undo batch")
        })
        .unwrap();
    assert!(undo_batch.operations.iter().any(|operation| matches!(
        operation,
        rho_desk::BatchOperation::Tree(TreeOperation::Delete { node_ids, .. })
            if node_ids == &vec![created]
    )));
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskTreeBatchApplied(rho_desk::BatchOpRecord {
                    sequence: 2,
                    timestamp_ms: 2,
                    batch: undo_batch,
                    daemon_tree_operations: Vec::new(),
                }),
                window,
                cx,
            );
            assert!(workspace.inbox_item_for_test(&inbox_id).is_some());
            assert_eq!(
                workspace.echo_text_for_test(),
                Some("undid file: Inbox QA item")
            );
            assert_eq!(
                workspace.current_deal_card_for_test().map(|card| card.0),
                Some(crate::dashboard::DealCardIdentity::Inbox(
                    inbox_id.0.clone()
                ))
            );
            assert_eq!(
                workspace.rendered_deal_card_for_test(),
                workspace.current_deal_card_for_test()
            );
            assert!(workspace.dashboard_deal_mode_for_test());
        })
        .unwrap();

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.prepare_deal_filing_for_test(inbox_id.clone());
            workspace.complete_filing_for_test(
                HostId::default(),
                destination,
                "Verdict agent",
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();
    let second_created = workspace
        .update(cx, |workspace, _, _| {
            workspace
                .take_host_messages_for_test(HostId::default())
                .into_iter()
                .find_map(|message| match message {
                    rho_ui_proto::ClientMessage::DeskTreeApply {
                        operation: TreeOperation::Create { node_id, .. },
                    } => Some(node_id),
                    _ => None,
                })
                .unwrap()
        })
        .unwrap();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.verdict_undo_count_for_test(), 1);
        })
        .unwrap();
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskTreeApplied(rho_desk::TreeOpRecord {
                    sequence: 3,
                    timestamp_ms: 3,
                    operation: TreeOperation::Create {
                        timestamp: TreeClock {
                            value: 10_000,
                            replica_id: 42,
                        },
                        node_id: NodeId {
                            replica_id: 42,
                            counter: 10_000,
                        },
                        kind: NodeKind::Heading,
                        owner: NodeOwner::User,
                        parent: Some(second_created),
                        order: OrderKey(vec![100]),
                    },
                }),
                window,
                cx,
            );
            workspace.undo_verdict(window, cx);
            assert_eq!(
                workspace.echo_text_for_test(),
                Some("cannot undo filing: Inbox QA item was edited")
            );
            assert_eq!(workspace.verdict_undo_count_for_test(), 0);
            assert!(
                workspace
                    .take_host_messages_for_test(HostId::default())
                    .iter()
                    .all(|message| !matches!(
                        message,
                        rho_ui_proto::ClientMessage::DeskTreeBatchApply { .. }
                    ))
            );
            let nodes =
                Document::from_snapshot(workspace.desk_snapshot_for_test(HostId::default()))
                    .unwrap()
                    .materialize();
            assert!(nodes.iter().any(|node| node.id == second_created));
            assert!(nodes.iter().any(|node| node.parent == Some(second_created)));
        })
        .unwrap();
}

#[gpui::test]
fn cancelled_filing_cannot_leak_its_card_into_the_next_item(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    let (first, second) = workspace
        .update(cx, |workspace, window, cx| {
            let first = workspace.append_inbox_for_test(crate::inbox::InboxDraft {
                kind: crate::inbox::InboxKind::Capture,
                text: "First filing".into(),
                source: crate::inbox::SourceReference::None,
                context: crate::inbox::CapturedContext::default(),
                waiting_on: None,
            });
            let second = workspace.append_inbox_for_test(crate::inbox::InboxDraft {
                kind: crate::inbox::InboxKind::Capture,
                text: "Second filing".into(),
                source: crate::inbox::SourceReference::None,
                context: crate::inbox::CapturedContext::default(),
                waiting_on: None,
            });
            workspace.age_inbox_for_test(&first, 0);
            workspace.open_deal_mode(window, cx);
            (first, second)
        })
        .unwrap();

    cx.dispatch_action(*workspace, crate::DashboardDealFile);
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(
                workspace.pending_filing_card_for_test(),
                Some((first.clone(), "First filing".into()))
            );
        })
        .unwrap();
    cx.dispatch_action(*workspace, crate::MinibufferCancel);
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.pending_filing_card_for_test(), None);
            workspace.prepare_deal_filing_for_test(second);
            assert_eq!(workspace.pending_filing_card_for_test(), None);
        })
        .unwrap();
}

#[gpui::test]
fn page_filing_undo_stays_on_the_stack_until_unbind_exists(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let inbox_id = workspace
        .update(cx, |workspace, window, cx| {
            let page_id = uuid::Uuid::new_v4();
            let id = workspace.append_inbox_for_test(crate::inbox::InboxDraft {
                kind: crate::inbox::InboxKind::Capture,
                text: "Filed research page".into(),
                source: crate::inbox::SourceReference::Page {
                    id: page_id.to_string(),
                },
                context: crate::inbox::CapturedContext::default(),
                waiting_on: None,
            });
            workspace.reopen_deal_for_test(crate::dashboard::DealCard {
                label: "Filed research page".into(),
                priority: 1.0,
                host: HostId::default(),
                subject_node_id: None,
                topic_node_id: None,
                agent_id: None,
                agent_tag: None,
                breadcrumb: "Filed research page".into(),
                room: None,
                kind: crate::dashboard::DealCardKind::Inbox(
                    crate::dashboard::DealerInboxKind::Capture,
                ),
                identity: crate::dashboard::DealCardIdentity::Inbox(id.0.clone()),
                inbox_source: Some(crate::dashboard::DealerInboxSource::Page(
                    rho_browser::PageId(page_id),
                )),
            });
            workspace.prepare_deal_filing_for_test(id.clone());
            workspace.complete_filing_for_test(
                HostId::default(),
                rho_desk::NodeId {
                    replica_id: 1,
                    counter: 1,
                },
                "Research",
                window,
                cx,
            );
            id
        })
        .unwrap();
    let request_id = workspace
        .update(cx, |workspace, _, _| {
            workspace
                .take_host_messages_for_test(HostId::default())
                .into_iter()
                .find_map(|message| match message {
                    rho_ui_proto::ClientMessage::DeskPageBind { request_id, .. } => {
                        Some(request_id)
                    }
                    _ => None,
                })
                .expect("page binding request")
        })
        .unwrap();
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskPageBindingResult {
                    request_id,
                    error: None,
                },
                window,
                cx,
            );
            assert!(workspace.inbox_item_for_test(&inbox_id).is_none());
            assert_eq!(workspace.verdict_undo_count_for_test(), 1);

            workspace.undo_verdict(window, cx);

            assert_eq!(
                workspace.echo_text_for_test(),
                Some("cannot undo page filing yet: Filed research page")
            );
            assert_eq!(workspace.verdict_undo_count_for_test(), 1);
        })
        .unwrap();
}

#[gpui::test]
fn inbox_verdict_echo_names_card_and_undo_restores_it(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let id = workspace
        .update(cx, |workspace, window, cx| {
            let id = workspace.append_inbox_for_test(crate::inbox::InboxDraft {
                kind: crate::inbox::InboxKind::Capture,
                text: "Remember the title".into(),
                source: crate::inbox::SourceReference::None,
                context: crate::inbox::CapturedContext::default(),
                waiting_on: None,
            });
            workspace.age_inbox_for_test(&id, 0);
            workspace.open_deal_mode(window, cx);
            id
        })
        .unwrap();
    cx.dispatch_action(*workspace, crate::DashboardDealDiscard);
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(
                workspace.echo_text_for_test(),
                Some("discard: Remember the title")
            );
            assert!(workspace.inbox_item_for_test(&id).is_none());
            assert_eq!(workspace.verdict_undo_count_for_test(), 1);
        })
        .unwrap();

    cx.dispatch_action(*workspace, crate::UndoVerdict);
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert!(workspace.inbox_item_for_test(&id).is_some());
            assert_eq!(
                workspace.echo_text_for_test(),
                Some("undid discard: Remember the title")
            );
            assert_eq!(
                workspace.current_deal_card_for_test().map(|card| card.0),
                Some(crate::dashboard::DealCardIdentity::Inbox(id.0.clone()))
            );
        })
        .unwrap();
}

#[gpui::test]
fn missing_inbox_item_is_not_a_successful_verdict(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let id = workspace
        .update(cx, |workspace, window, cx| {
            let id = workspace.append_inbox_for_test(crate::inbox::InboxDraft {
                kind: crate::inbox::InboxKind::Capture,
                text: "Retired elsewhere".into(),
                source: crate::inbox::SourceReference::None,
                context: crate::inbox::CapturedContext::default(),
                waiting_on: None,
            });
            workspace.age_inbox_for_test(&id, 0);
            workspace.open_deal_mode(window, cx);
            workspace.retire_inbox_for_test(&id);
            id
        })
        .unwrap();

    cx.dispatch_action(*workspace, crate::DashboardDealDone);
    workspace
        .update(cx, |workspace, _, _| {
            assert!(workspace.inbox_item_for_test(&id).is_none());
            assert_eq!(workspace.verdict_undo_count_for_test(), 0);
            assert_eq!(
                workspace.echo_text_for_test(),
                Some("done: nothing under the deal: the inbox item is unavailable")
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
    use rho_desk::{
        BatchOpRecord, Document, NodeId, NodeKind, NodeOwner, OrderKey, Replica, ReplicaAuthor,
        TemporalKind, TemporalMark, TextOperation, TreeClock, TreeOperation,
    };

    let mut document = Document::default();
    document.add_replica(Replica {
        replica_id: 1,
        author: ReplicaAuthor::Machine,
    });
    let heading = NodeId {
        replica_id: 1,
        counter: 1,
    };
    document
        .apply(TreeOperation::Create {
            timestamp: TreeClock {
                value: 1,
                replica_id: 1,
            },
            node_id: heading,
            kind: NodeKind::Heading,
            owner: NodeOwner::User,
            parent: None,
            order: OrderKey(vec![100]),
        })
        .unwrap();
    let prior = TemporalMark {
        year: 2020,
        month: 1,
        day: 1,
        minute_of_day: None,
        pace_days: 1,
    };
    document
        .apply(TreeOperation::SetTemporal {
            timestamp: TreeClock {
                value: 2,
                replica_id: 1,
            },
            node_id: heading,
            kind: TemporalKind::Todo,
            value: Some(prior),
        })
        .unwrap();
    let mut title = text::Buffer::new(text::ReplicaId::new(1), text::BufferId::new(1).unwrap(), "");
    document
        .apply_text(
            heading,
            TextOperation::from_text(&title.edit([(0..0, "Named card")])),
            None,
        )
        .unwrap();

    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskTreeSnapshot {
                    snapshot: document.snapshot(),
                    replica_id: 42,
                },
                window,
                cx,
            );
            workspace.open_deal_mode(window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();

    macro_rules! verdict_and_undo {
        ($action:expr, $echo:literal) => {{
            cx.dispatch_action(*workspace, $action);
            cx.run_until_parked();
            let batch = workspace
                .update(cx, |workspace, _, _| {
                    workspace
                        .take_host_messages_for_test(HostId::default())
                        .into_iter()
                        .find_map(|message| match message {
                            rho_ui_proto::ClientMessage::DeskTreeBatchApply { batch } => {
                                Some(batch)
                            }
                            _ => None,
                        })
                        .expect("verdict batch")
                })
                .unwrap();
            workspace
                .update(cx, |workspace, window, cx| {
                    workspace.handle_event(
                        HostId::default(),
                        ConnEvent::DeskTreeBatchApplied(BatchOpRecord {
                            sequence: 1,
                            timestamp_ms: 1,
                            batch,
                            daemon_tree_operations: Vec::new(),
                        }),
                        window,
                        cx,
                    );
                    assert_eq!(workspace.echo_text_for_test(), Some($echo));
                })
                .unwrap();
            cx.dispatch_action(*workspace, crate::UndoVerdict);
            let undo_batch = workspace
                .update(cx, |workspace, _, _| {
                    let node = Document::from_snapshot(
                        workspace.desk_snapshot_for_test(HostId::default()),
                    )
                    .unwrap()
                    .materialize()
                    .into_iter()
                    .find(|node| node.id == heading)
                    .unwrap();
                    assert_eq!(node.temporal.get(&TemporalKind::Todo), Some(&prior));
                    workspace
                        .take_host_messages_for_test(HostId::default())
                        .into_iter()
                        .find_map(|message| match message {
                            rho_ui_proto::ClientMessage::DeskTreeBatchApply { batch } => {
                                Some(batch)
                            }
                            _ => None,
                        })
                        .expect("undo batch")
                })
                .unwrap();
            workspace
                .update(cx, |workspace, window, cx| {
                    workspace.handle_event(
                        HostId::default(),
                        ConnEvent::DeskTreeBatchApplied(BatchOpRecord {
                            sequence: 2,
                            timestamp_ms: 2,
                            batch: undo_batch,
                            daemon_tree_operations: Vec::new(),
                        }),
                        window,
                        cx,
                    );
                    assert_eq!(
                        workspace.current_deal_card_for_test().map(|card| card.0),
                        Some(crate::dashboard::DealCardIdentity::Tree {
                            host: HostId::default(),
                            node_id: heading,
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
    verdict_and_undo!(crate::DashboardDealDiscard, "discard: Named card");
    verdict_and_undo!(crate::DashboardDealSnooze, "snooze 1d: Named card");
    verdict_and_undo!(crate::DashboardDealTodo, "todo: Named card");

    // A delayed acknowledgement belongs to the submitted card, even if the
    // user has moved on to another deal in the meantime.
    cx.dispatch_action(*workspace, crate::DashboardDealDone);
    let delayed = workspace
        .update(cx, |workspace, _, _| {
            workspace
                .take_host_messages_for_test(HostId::default())
                .into_iter()
                .find_map(|message| match message {
                    rho_ui_proto::ClientMessage::DeskTreeBatchApply { batch } => Some(batch),
                    _ => None,
                })
                .unwrap()
        })
        .unwrap();
    workspace
        .update(cx, |workspace, window, cx| {
            let mut replacement = workspace.current_deal_card_value_for_test().unwrap();
            replacement.identity = crate::dashboard::DealCardIdentity::Inbox("replacement".into());
            replacement.kind =
                crate::dashboard::DealCardKind::Inbox(crate::dashboard::DealerInboxKind::Capture);
            replacement.topic_node_id = None;
            replacement.subject_node_id = None;
            replacement.agent_id = None;
            workspace.reopen_deal_for_test(replacement);
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskTreeBatchApplied(BatchOpRecord {
                    sequence: 3,
                    timestamp_ms: 3,
                    batch: delayed,
                    daemon_tree_operations: Vec::new(),
                }),
                window,
                cx,
            );
            assert_eq!(
                workspace.current_deal_card_for_test().map(|card| card.0),
                Some(crate::dashboard::DealCardIdentity::Inbox(
                    "replacement".into()
                ))
            );
            assert_eq!(workspace.echo_text_for_test(), Some("done: Named card"));
        })
        .unwrap();
}

#[gpui::test]
fn double_shift_toggles_desk_overview(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.select_agent(None, window, cx);
            assert!(!workspace.is_dashboard_mode(window, cx));
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "shift shift");
    workspace
        .update(cx, |workspace, window, cx| {
            assert!(workspace.is_dashboard_mode(window, cx));
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "shift shift");
    workspace
        .update(cx, |workspace, window, cx| {
            assert!(!workspace.is_dashboard_mode(window, cx));
        })
        .unwrap();
}

#[gpui::test]
fn f24_alias_toggles_desk_overview(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.select_agent(None, window, cx);
            assert!(!workspace.is_dashboard_mode(window, cx));
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "f24");
    workspace
        .update(cx, |workspace, window, cx| {
            assert!(workspace.is_dashboard_mode(window, cx));
        })
        .unwrap();
}

#[gpui::test]
fn two_finger_swipe_down_toggles_desk_overview(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.select_agent(None, window, cx);
            window.simulate_next_frame(cx);
            assert!(!workspace.is_dashboard_mode(window, cx));
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
        .update(cx, |workspace, window, cx| {
            assert!(workspace.is_dashboard_mode(window, cx));
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
    use rho_desk::{
        Document, NodeId, NodeKind, NodeOwner, OrderKey, Replica, ReplicaAuthor, TextOperation,
        TreeClock, TreeOperation,
    };

    let mut document = Document::default();
    document.add_replica(Replica {
        replica_id: 1,
        author: ReplicaAuthor::Machine,
    });
    let heading = NodeId {
        replica_id: 1,
        counter: 1,
    };
    let prose = NodeId {
        replica_id: 1,
        counter: 2,
    };
    let agent_row = NodeId {
        replica_id: 1,
        counter: 3,
    };
    for (clock, id, kind, owner, parent) in [
        (1, heading, NodeKind::Heading, NodeOwner::User, None),
        (2, prose, NodeKind::Prose, NodeOwner::User, Some(heading)),
        (
            3,
            agent_row,
            NodeKind::Agent,
            NodeOwner::Machine,
            Some(heading),
        ),
    ] {
        document
            .apply(TreeOperation::Create {
                timestamp: TreeClock {
                    value: clock,
                    replica_id: 1,
                },
                node_id: id,
                kind,
                owner,
                parent,
                order: OrderKey(vec![100]),
            })
            .unwrap();
    }
    for (clock, id, kind) in [
        (4, heading, rho_desk::TemporalKind::Todo),
        (5, prose, rho_desk::TemporalKind::Deadline),
    ] {
        document
            .apply(TreeOperation::SetTemporal {
                timestamp: TreeClock {
                    value: clock,
                    replica_id: 1,
                },
                node_id: id,
                kind,
                value: Some(rho_desk::TemporalMark {
                    year: 2026,
                    month: 3,
                    day: 1,
                    minute_of_day: None,
                    pace_days: 1,
                }),
            })
            .unwrap();
    }
    for (id, value) in [(heading, "Parent"), (prose, "body\n"), (agent_row, "agent")] {
        let mut buffer = text::Buffer::new(
            text::ReplicaId::new(1),
            text::BufferId::new(id.counter).unwrap(),
            "",
        );
        document
            .apply_text(
                id,
                TextOperation::from_text(&buffer.edit([(0..0, value)])),
                None,
            )
            .unwrap();
    }

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskTreeSnapshot {
                    snapshot: document.snapshot(),
                    replica_id: 42,
                },
                window,
                cx,
            );
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
    assert_eq!(text, "Parent\nbody\n\nagent");
    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(workspace.dashboard_editor().read(cx).eol_hints().len(), 2);
        })
        .unwrap();
    workspace
        .update(cx, |workspace, window, cx| {
            let editor = workspace.dashboard_editor();
            editor.update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    let offset = editor::MultiBufferOffset(3);
                    selections.select_ranges([offset..offset]);
                });
            });
            let mut replacement = document.snapshot();
            replacement.sequence = 1;
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskTreeReplaced(replacement),
                window,
                cx,
            );
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "i X escape");
    cx.run_until_parked();
    let after_replacement = workspace
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
    assert!(
        after_replacement.starts_with("ParXent"),
        "replacement cursor moved: {after_replacement:?}"
    );
    workspace
        .update(cx, |workspace, window, cx| {
            let editor = workspace.dashboard_editor();
            window.focus(&editor.read(cx).focus_handle(cx), cx);
            editor.update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    let offset = editor::MultiBufferOffset(0);
                    selections.select_ranges([offset..offset]);
                });
            });
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "escape tab");
    cx.run_until_parked();
    let folded = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .dashboard_editor()
                .update(cx, |editor, cx| editor.display_text(cx))
        })
        .unwrap();
    assert!(folded.contains("ParXent"), "folded display: {folded:?}");
    assert!(!folded.contains("body"), "folded display: {folded:?}");

    // Marker recognition runs between the space and the next keystroke: the
    // remaining title input must land in the optimistically-created heading
    // buffer, not in the prose buffer that contained the marker.
    cx.simulate_keystrokes(*workspace, "tab");
    workspace
        .update(cx, |workspace, window, cx| {
            let editor = workspace.dashboard_editor();
            editor.update(cx, |editor, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    let offset = editor::MultiBufferOffset("ParXent\n".len());
                    selections.select_ranges([offset..offset]);
                });
            });
        })
        .unwrap();
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
    assert!(recognized.contains("Fastbody"), "tree text: {recognized:?}");
    assert!(!recognized.contains("* "), "tree text: {recognized:?}");

    // Vim search is hosted by the composed editor even without a Zed pane;
    // its query must never become document input.
    let before_search = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .tree_nodes_for_test(HostId::default(), cx)
                .to_vec()
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "/ F a s t enter");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(
                workspace.tree_nodes_for_test(HostId::default(), cx),
                before_search.as_slice()
            );
        })
        .unwrap();

    // A composed heading is a semantic row: dd removes its node/subtree and
    // an immediate p pastes the captured subtree relative to the surviving
    // row selected after deletion. Undoing paste and delete restores it too.
    cx.simulate_keystrokes(*workspace, "d d");
    cx.run_until_parked();
    assert!(!display_text(&workspace, cx).contains("Fastbody"));
    cx.simulate_keystrokes(*workspace, "p");
    cx.run_until_parked();
    let pasted_display = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .dashboard_editor()
                .update(cx, |editor, cx| editor.display_text(cx))
        })
        .unwrap();
    assert_eq!(
        pasted_display.matches("Fastbody").count(),
        1,
        "paste left a stale composed row: {pasted_display:?}"
    );
    workspace
        .update(cx, |workspace, _, cx| {
            let pasted = workspace
                .tree_nodes_for_test(HostId::default(), cx)
                .iter()
                .find_map(|(node_id, kind, _, text)| {
                    (*kind == rho_desk::NodeKind::Heading && text == "Fastbody").then_some(*node_id)
                })
                .expect("pasted heading");
            assert_eq!(
                workspace
                    .tree_cursor_for_test(cx)
                    .map(|(_, node_id, _)| node_id),
                Some(pasted),
                "paste must focus the restored subtree root"
            );
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "u");
    cx.run_until_parked();
    assert!(!display_text(&workspace, cx).contains("Fastbody"));
    cx.simulate_keystrokes(*workspace, "u");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert!(
                workspace
                    .tree_nodes_for_test(HostId::default(), cx)
                    .iter()
                    .any(|(_, kind, _, text)| *kind == rho_desk::NodeKind::Heading
                        && text == "Fastbody")
            );
        })
        .unwrap();
    let restored = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .tree_nodes_for_test(HostId::default(), cx)
                .iter()
                .find(|(_, kind, _, text)| {
                    *kind == rho_desk::NodeKind::Heading && text == "Fastbody"
                })
                .unwrap()
                .0
        })
        .unwrap();
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.focus_tree_node_for_test(HostId::default(), restored, window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    // Enter in a heading is one semantic split: it creates a prose child,
    // and one `u` removes that child while restoring the original title.
    let prose_children_before_split = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .tree_nodes_for_test(HostId::default(), cx)
                .iter()
                .filter(|(_, kind, parent, _)| {
                    *kind == rho_desk::NodeKind::Prose && *parent == Some(restored)
                })
                .count()
        })
        .unwrap();
    workspace
        .update(cx, |workspace, window, cx| {
            workspace
                .dashboard_editor()
                .update(cx, |editor, cx| editor.handle_input("\n", window, cx));
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            let nodes = workspace.tree_nodes_for_test(HostId::default(), cx);
            assert_eq!(
                nodes
                    .iter()
                    .filter(|(_, kind, parent, _)| *kind == rho_desk::NodeKind::Prose
                        && *parent == Some(restored))
                    .count(),
                prose_children_before_split + 1,
                "split nodes: {nodes:?}"
            );
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "u");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, window, cx| {
            let nodes = workspace.tree_nodes_for_test(HostId::default(), cx);
            assert!(nodes.iter().any(
                |(_, kind, _, text)| *kind == rho_desk::NodeKind::Heading && text == "Fastbody"
            ));
            assert_eq!(
                nodes
                    .iter()
                    .filter(|(_, kind, parent, _)| *kind == rho_desk::NodeKind::Prose
                        && *parent == Some(restored))
                    .count(),
                prose_children_before_split
            );
            workspace.focus_tree_node_for_test(HostId::default(), restored, window, cx);
        })
        .unwrap();
    cx.run_until_parked();

    // Backspace on an empty structural row merges it away. Its inverse must
    // recreate an empty heading even though tombstoned CRDT ids cannot be
    // reused.
    let headings_before_merge = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .tree_nodes_for_test(HostId::default(), cx)
                .iter()
                .filter(|(_, kind, _, _)| *kind == rho_desk::NodeKind::Heading)
                .count()
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "alt-enter escape backspace");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(
                workspace
                    .tree_nodes_for_test(HostId::default(), cx)
                    .iter()
                    .filter(|(_, kind, _, _)| *kind == rho_desk::NodeKind::Heading)
                    .count(),
                headings_before_merge
            );
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "u");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(
                workspace
                    .tree_nodes_for_test(HostId::default(), cx)
                    .iter()
                    .filter(|(_, kind, _, _)| *kind == rho_desk::NodeKind::Heading)
                    .count(),
                headings_before_merge + 1
            );
        })
        .unwrap();

    workspace
        .update(cx, |workspace, window, cx| {
            workspace.focus_tree_node_for_test(HostId::default(), restored, window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "alt-enter n e w escape");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert!(workspace
                .tree_nodes_for_test(HostId::default(), cx)
                .iter()
                .any(|(_, kind, _, text)|
                    *kind == rho_desk::NodeKind::Heading && text == "new"));
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "u");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert!(!workspace
                .tree_nodes_for_test(HostId::default(), cx)
                .iter()
                .any(|(_, kind, _, text)|
                    *kind == rho_desk::NodeKind::Heading && text == "new"));
        })
        .unwrap();
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.focus_tree_node_for_test(HostId::default(), restored, window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "shift-o a b o v e escape");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            let nodes = workspace.tree_nodes_for_test(HostId::default(), cx);
            assert!(nodes.iter().any(|(_, kind, _, text)|
                *kind == rho_desk::NodeKind::Prose && text == "above"));
            assert!(nodes.iter().any(
                |(_, kind, _, text)| *kind == rho_desk::NodeKind::Heading && text == "Fastbody"
            ));
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "u");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, window, cx| {
            assert!(!workspace
                .tree_nodes_for_test(HostId::default(), cx)
                .iter()
                .any(|(_, kind, _, text)|
                    *kind == rho_desk::NodeKind::Prose && text == "above"));
            workspace.focus_tree_node_for_test(HostId::default(), restored, window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(*workspace, "y y p");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(
                workspace
                    .tree_nodes_for_test(HostId::default(), cx)
                    .iter()
                    .filter(|(_, kind, _, text)| {
                        *kind == rho_desk::NodeKind::Heading && text == "Fastbody"
                    })
                    .count(),
                2
            );
        })
        .unwrap();
    let before_agent_flow = workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .tree_nodes_for_test(HostId::default(), cx)
                .to_vec()
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "shift-r c");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert!(workspace.dashboard_has_new_draft_for_test());
            assert_eq!(
                workspace.tree_nodes_for_test(HostId::default(), cx),
                before_agent_flow.as_slice()
            );
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "q");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert!(!workspace.dashboard_has_new_draft_for_test());
            assert!(!workspace.has_new_agent_configuration_for_test());
            assert!(
                workspace
                    .tree_nodes_for_test(HostId::default(), cx)
                    .iter()
                    .all(|(_, _, _, text)| text != "q")
            );
        })
        .unwrap();

    // Deleting a user heading never tombstones its machine-owned agent row.
    // The same batch reparents the row, tells the user, and undo moves it
    // beneath the fresh restored heading id after daemon acceptance (covered
    // by the daemon's constrained-relocation test).
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.focus_tree_node_for_test(HostId::default(), heading, window, cx);
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "escape d d");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            let nodes = workspace.tree_nodes_for_test(HostId::default(), cx);
            assert!(
                nodes.iter().any(|(id, kind, parent, _)| *id == agent_row
                    && *kind == NodeKind::Agent
                    && parent.is_none()),
                "post-delete nodes: {nodes:?}"
            );
            assert_eq!(
                workspace.echo_text_for_test(),
                Some("moved 1 agent rows to root")
            );
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "u");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            let nodes = workspace.tree_nodes_for_test(HostId::default(), cx);
            assert!(
                nodes
                    .iter()
                    .any(|(_, kind, _, text)| *kind == NodeKind::Heading && text == "ParXent")
            );
            assert!(nodes.iter().any(|(id, kind, parent, _)| *id == agent_row
                && *kind == NodeKind::Agent
                && parent.is_none()));
        })
        .unwrap();

    // If a retryable split conflict cannot be replayed against the fresh
    // snapshot, its external undo entry is discarded instead of poisoning
    // the next ordinary `u`.
    let (before_failed_retry, undo_count) = workspace
        .update(cx, |workspace, _, _| {
            workspace.take_host_messages_for_test(HostId::default());
            (
                workspace.desk_snapshot_for_test(HostId::default()),
                workspace.semantic_undo_count_for_test(),
            )
        })
        .unwrap();
    workspace
        .update(cx, |workspace, window, cx| {
            workspace
                .dashboard_editor()
                .update(cx, |editor, cx| editor.handle_input("\n", window, cx));
        })
        .unwrap();
    cx.run_until_parked();
    let rejected_id = workspace
        .update(cx, |workspace, _, _| {
            workspace
                .take_host_messages_for_test(HostId::default())
                .into_iter()
                .find_map(|message| match message {
                    rho_ui_proto::ClientMessage::DeskTreeBatchApply { batch } => Some(batch.id),
                    _ => None,
                })
                .expect("split batch")
        })
        .unwrap();
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskTreeBatchRejected {
                    id: rejected_id,
                    retryable: true,
                    reason: "test conflict".into(),
                    snapshot: before_failed_retry,
                },
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            assert_eq!(workspace.semantic_undo_count_for_test(), undo_count);
        })
        .unwrap();
}

#[gpui::test]
fn delayed_title_after_o_heading_recognition_targets_the_replacement(cx: &mut TestAppContext) {
    use rho_desk::{
        BatchOpRecord, BatchOperation, Document, NodeId, NodeKind, NodeOwner, OrderKey, Replica,
        ReplicaAuthor, TextOperation, TreeClock, TreeOperation,
    };

    let mut document = Document::default();
    document.add_replica(Replica {
        replica_id: 1,
        author: ReplicaAuthor::Machine,
    });
    let heading = NodeId {
        replica_id: 1,
        counter: 1,
    };
    document
        .apply(TreeOperation::Create {
            timestamp: TreeClock {
                value: 1,
                replica_id: 1,
            },
            node_id: heading,
            kind: NodeKind::Heading,
            owner: NodeOwner::User,
            parent: None,
            order: OrderKey(vec![100]),
        })
        .unwrap();
    let mut title = text::Buffer::new(text::ReplicaId::new(1), text::BufferId::new(1).unwrap(), "");
    document
        .apply_text(
            heading,
            TextOperation::from_text(&title.edit([(0..0, "Parent")])),
            None,
        )
        .unwrap();

    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskTreeSnapshot {
                    snapshot: document.snapshot(),
                    replica_id: 42,
                },
                window,
                cx,
            );
            workspace.focus_tree_node_for_test(HostId::default(), heading, window, cx);
            workspace.take_host_messages_for_test(HostId::default());
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "o");
    cx.run_until_parked();
    let open_batch = workspace
        .update(cx, |workspace, _, _| {
            workspace
                .take_host_messages_for_test(HostId::default())
                .into_iter()
                .find_map(|message| match message {
                    rho_ui_proto::ClientMessage::DeskTreeBatchApply { batch } => Some(batch),
                    _ => None,
                })
                .expect("o creates a prose row")
        })
        .unwrap();
    let prose = open_batch
        .operations
        .iter()
        .find_map(|operation| match operation {
            BatchOperation::Tree(TreeOperation::Create {
                node_id,
                kind: NodeKind::Prose,
                ..
            }) => Some(*node_id),
            _ => None,
        })
        .unwrap();
    let stale_source = workspace
        .update(cx, |workspace, _, _| {
            workspace
                .tree_buffer_for_test(HostId::default(), prose)
                .expect("opened prose buffer")
        })
        .unwrap();
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskTreeBatchApplied(BatchOpRecord {
                    sequence: 1,
                    timestamp_ms: 1,
                    batch: open_batch,
                    daemon_tree_operations: Vec::new(),
                }),
                window,
                cx,
            );
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "* space");
    cx.run_until_parked();
    let recognition_batch = workspace
        .update(cx, |workspace, _, _| {
            let messages = workspace.take_host_messages_for_test(HostId::default());
            messages
                .iter()
                .cloned()
                .into_iter()
                .find_map(|message| match message {
                    rho_ui_proto::ClientMessage::DeskTreeBatchApply { batch } => Some(batch),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("marker recognition batch; messages: {messages:?}"))
        })
        .unwrap();
    let replacement = recognition_batch
        .operations
        .iter()
        .find_map(|operation| match operation {
            BatchOperation::Tree(TreeOperation::Create {
                node_id,
                kind: NodeKind::Heading,
                ..
            }) => Some(*node_id),
            _ => None,
        })
        .unwrap();
    assert!(recognition_batch.operations.iter().any(|operation| matches!(
        operation,
        BatchOperation::Tree(TreeOperation::Delete { node_ids, .. }) if node_ids == &vec![prose]
    )));
    let mut delayed_ack = recognition_batch.clone();
    delayed_ack.id.value += 100;
    workspace
        .update(cx, |workspace, _, _| {
            workspace.clone_pending_desk_intent_for_test(
                HostId::default(),
                recognition_batch.id,
                delayed_ack.id,
            );
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "a b");
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskTreeBatchApplied(BatchOpRecord {
                    sequence: 2,
                    timestamp_ms: 2,
                    batch: recognition_batch,
                    daemon_tree_operations: Vec::new(),
                }),
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(
                workspace.tree_cursor_for_test(cx),
                Some((HostId::default(), replacement, 2)),
                "accepted recognition must preserve the replacement caret"
            );
        })
        .unwrap();
    let before_ack_edits = workspace
        .update(cx, |workspace, _, _| {
            workspace
                .take_host_messages_for_test(HostId::default())
                .into_iter()
                .filter_map(|message| match message {
                    rho_ui_proto::ClientMessage::DeskNodeTextApply {
                        node_id,
                        operation,
                        transaction,
                    } => Some((node_id, operation, transaction)),
                    _ => None,
                })
                .collect::<Vec<_>>()
        })
        .unwrap();

    let snapshot_after_recognition = workspace
        .update(cx, |workspace, _, _| {
            workspace.desk_snapshot_for_test(HostId::default())
        })
        .unwrap();
    // This is the later stale source-buffer event, after recognition has
    // fully completed and daemon acceptance replaced the prose node. Editing
    // the retained entity exercises Edited -> Operation subscription order.
    stale_source.update(cx, |buffer, cx| {
        buffer.edit([(0..0, "Recognized")], None, cx);
    });
    cx.run_until_parked();
    let mut edits = before_ack_edits;
    edits.extend(
        workspace
            .update(cx, |workspace, _, _| {
                workspace
                    .take_host_messages_for_test(HostId::default())
                    .into_iter()
                    .filter_map(|message| match message {
                        rho_ui_proto::ClientMessage::DeskNodeTextApply {
                            node_id,
                            operation,
                            transaction,
                        } => Some((node_id, operation, transaction)),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap(),
    );
    assert!(
        !edits.is_empty(),
        "late title produced no persisted text edit"
    );
    assert!(edits.iter().all(|(node_id, ..)| *node_id == replacement));
    let mut reconstructed = Document::from_snapshot(snapshot_after_recognition).unwrap();
    for (node_id, operation, transaction) in edits {
        assert!(
            reconstructed
                .apply_text(node_id, operation, transaction)
                .unwrap()
        );
    }
    assert_eq!(
        reconstructed
            .text(replacement, 42, text::BufferId::new(999).unwrap(),)
            .unwrap(),
        "Recognizedab"
    );

    // An acknowledgement may arrive after the user deliberately navigates
    // elsewhere. In that case it must not pull the cursor back to the
    // optimistically-created replacement.
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.focus_tree_node_for_test(HostId::default(), heading, window, cx);
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_event(
                HostId::default(),
                ConnEvent::DeskTreeBatchApplied(BatchOpRecord {
                    sequence: 3,
                    timestamp_ms: 3,
                    batch: delayed_ack,
                    daemon_tree_operations: Vec::new(),
                }),
                window,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(
                workspace
                    .tree_cursor_for_test(cx)
                    .map(|(_, node_id, _)| node_id),
                Some(heading),
                "a delayed acknowledgement stole deliberate navigation"
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
    let workspace = test_workspace(cx);
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
            assert!(workspace.overview_open_for_test())
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "f24");
    workspace
        .update(cx, |workspace, _, _| {
            assert!(
                workspace.overview_open_for_test(),
                "overview toggle resurrected the closed draft"
            )
        })
        .unwrap();
}

#[gpui::test]
fn q_discards_shift_r_draft_from_surface_history(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            // Opening the standalone composer from overview records Draft in
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
    cx.simulate_keystrokes(*workspace, "f24");
    cx.simulate_keystrokes(*workspace, "shift-r");
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
                "history reopened the discarded Shift-R draft"
            )
        })
        .unwrap();
}

#[gpui::test]
fn discarding_shift_r_draft_preserves_non_draft_history_cursor(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.configure_surface_history_for_test(&["current"], window, cx);
        })
        .unwrap();
    cx.simulate_keystrokes(*workspace, "f24 shift-r q f24");
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
fn q_on_last_surface_lands_on_overview(cx: &mut TestAppContext) {
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
            assert!(workspace.overview_open_for_test())
        })
        .unwrap();

    cx.simulate_keystrokes(*workspace, "f24");
    workspace
        .update(cx, |workspace, _, _| {
            assert!(
                workspace.overview_open_for_test(),
                "overview toggle resurrected a closed surface"
            )
        })
        .unwrap();
}

#[gpui::test]
fn q_on_overview_is_a_no_op(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
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
