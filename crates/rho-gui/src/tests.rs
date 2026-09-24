//! End-to-end tests: synthetic protocol frames in, rendered editor state out.

use std::collections::HashSet;
use std::sync::Arc;

use editor::display_map::{Block, CustomBlockId, DisplayPoint, DisplayRow};
use editor::{Copy, Editor, MoveRight, SelectionEffects};
use gpui::{
    App, AppContext as _, Entity, Focusable as _, InputEvent as _, Modifiers, MouseButton,
    MouseDownEvent, MouseUpEvent, TestAppContext, TouchEvent, TouchId, TouchPhase, WindowHandle,
    point, px, size,
};
use language::InlayId;
use rho_agent_hosts::connection::ConnEvent;
use rho_agent_types::{AgentId, UnixMs};
use rho_agents_client::state::{
    UiAgentState, UiAgentStatus, UiBlock, UiMessagePhase, UiTool, UiToolStatus,
};
use rho_agents_view::transcript::elisions::{ElisionSpec, ElisionState, ElisionSync};
use settings::{Settings, SettingsStore};
use story::ready_with;

mod call_punctuation;
mod editor_shutdown;
mod elision_block_geometry;
mod elision_caret;
mod elision_paging;
mod elision_unfold;
mod fold_accounting_streaming;
mod fold_cost;
mod fold_widen_bias;
mod fold_widen_underflow;
mod fold_widening_check;
mod history;
mod inlay_cost;
mod minibuffer;
mod prose_buffers;
mod record_anchors;
mod removing_a_turn_after_growth;
mod row_spacing;
mod running_turn_elapsed;
mod scene_walk;
pub(super) mod story;
mod syntax_parsed_in_frame;
mod tool_output_not_drawn;
mod wrap_rows;
mod wrap_under_tab;
use rho_agents_client::HostId;

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
fn gutter_images_reserve_one_and_a_quarter_line_width_without_inserting_text(
    cx: &mut TestAppContext,
) {
    cx.update(init_test_app);
    let window = cx.add_window(|window, cx| {
        let mut editor = Editor::multi_line(window, cx);
        rho_window::editor_config::configure(&mut editor, window, cx);
        editor.set_show_compact_gutter(false, cx);
        editor.set_text("first\nsecond", window, cx);
        editor
    });
    window
        .update(cx, |editor, window, cx| {
            let style = editor.style(cx).clone();
            let font_size = style.text.font_size.to_pixels(window.rem_size());
            let font_id = window.text_system().resolve_font(&style.text.font());
            let line_height = style.text.line_height_in_pixels(window.rem_size());
            let anchor = editor
                .buffer()
                .read(cx)
                .snapshot(cx)
                .anchor_before(editor::MultiBufferOffset(0));
            let before = editor.display_snapshot(cx).text();
            editor.set_reserve_image_gutter(true, cx);
            let reserved = editor
                .snapshot(window, cx)
                .gutter_dimensions(font_id, font_size, &style, window, cx)
                .width;
            assert_eq!(
                reserved,
                line_height * 1.25 + font_size * 0.5,
                "reserve the final width before there are any images"
            );

            let image = std::sync::Arc::new(gpui::RenderImage::new(smallvec::SmallVec::new()));
            editor.set_gutter_image(
                anchor,
                Some(editor::GutterImage {
                    image: Some(image),
                    initials: "AB".into(),
                }),
                cx,
            );
            let dimensions = editor
                .snapshot(window, cx)
                .gutter_dimensions(font_id, font_size, &style, window, cx);
            assert_eq!(dimensions.width, line_height * 1.25 + font_size * 0.5);
            assert_eq!(
                editor.display_snapshot(cx).text(),
                before,
                "avatars consume no text columns or rows"
            );
            editor.set_gutter_image(anchor, None, cx);
            assert_eq!(
                editor
                    .snapshot(window, cx)
                    .gutter_dimensions(font_id, font_size, &style, window, cx)
                    .width,
                reserved
            );
            editor.set_reserve_image_gutter(false, cx);

            let dimensions = editor
                .snapshot(window, cx)
                .gutter_dimensions(font_id, font_size, &style, window, cx);
            assert_eq!(
                dimensions.width,
                gpui::px(0.),
                "removing the last avatar releases its gutter"
            );
        })
        .unwrap();
}

#[gpui::test]
fn centered_rows_and_fractional_gaps_share_paint_and_hit_geometry(cx: &mut TestAppContext) {
    use text::Bias;
    cx.update(init_test_app);
    let window = cx.add_window(|window, cx| {
        let mut editor = Editor::multi_line(window, cx);
        rho_window::editor_config::configure(&mut editor, window, cx);
        editor.set_text(
            "Mon 1 Jun\nshort\nlonger body\nlast\nTue 2 Jun\ntail",
            window,
            cx,
        );
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let anchor = |row| snapshot.anchor_after(language::Point::new(row, 0));
        editor.set_centered_rows(vec![anchor(0), anchor(4)], cx);
        editor.set_row_spacing(
            vec![
                editor::display_map::RowSpacing {
                    range: anchor(1)..anchor(1),
                    minimum_height: 1.25,
                    gap_after: 0.5,
                },
                editor::display_map::RowSpacing {
                    range: anchor(3)..anchor(3),
                    minimum_height: 0.,
                    gap_after: 0.25,
                },
            ],
            cx,
        );
        editor
    });
    for width in [740., 430.] {
        cx.simulate_window_resize(*window, size(px(width), px(600.)));
        cx.run_until_parked();
        cx.draw_window(*window);
        window
            .update(cx, |editor, window, cx| {
                let snapshot = editor.snapshot(window, cx);
                let line_height = editor
                    .style(cx)
                    .text
                    .line_height_in_pixels(window.rem_size());
                let position =
                    |editor: &mut Editor, row, column, window: &mut gpui::Window, cx: &mut App| {
                        editor
                            .window_position_for_display_point(
                                DisplayPoint::new(DisplayRow(row), column),
                                &snapshot,
                                window,
                                cx,
                            )
                            .unwrap()
                    };
                let origin = position(editor, 1, 0, window, cx);
                assert_eq!(snapshot.row_y(1.), 1.125);
                assert_eq!(snapshot.row_padding_before(DisplayRow(1)), 0.125);
                let next = position(editor, 2, 0, window, cx);
                assert!(
                    (f32::from(next.y - origin.y) - f32::from(line_height) * 1.625).abs() < 0.1
                );
                let date = position(editor, 4, 0, window, cx);
                let first_date = position(editor, 0, 0, window, cx);
                assert!(
                    (f32::from(date.y - first_date.y) - f32::from(line_height) * 5.).abs() < 0.1
                );
                assert!(date.x > origin.x + px(80.), "dates center; body stays left");
                for (row, offset) in [(0, 0), (1, 10), (2, 16), (3, 28), (4, 33), (5, 43)] {
                    let pos =
                        position(editor, row, 0, window, cx) + point(px(1.), line_height * 0.5);
                    let (_, _, hit) = editor
                        .buffer_location_for_window_position(pos, Bias::Left)
                        .unwrap();
                    assert_eq!(hit, offset, "row {row}, viewport width {width}");
                }
                let gap = origin + point(px(1.), line_height * 1.25);
                let (_, _, hit) = editor
                    .buffer_location_for_window_position(gap, Bias::Left)
                    .unwrap();
                assert_eq!(hit, 10, "gap clicks map to the preceding text row");
                assert_eq!(
                    snapshot.text(),
                    "Mon 1 Jun\nshort\nlonger body\nlast\nTue 2 Jun\ntail"
                );
            })
            .unwrap();
    }
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

    let spec = |start_block: usize, anchors: (text::Anchor, text::Anchor), tool_count: usize| {
        ElisionSpec {
            start_block,
            range: anchors.0..anchors.1,
            tool_count,
            tail_rows: 0,
        }
    };
    let (first, unresolvable, last) = cx.update(|cx| {
        let buffer = buffer.read(cx);
        let elsewhere = elsewhere.read(cx);
        (
            spec(0, (buffer.anchor_before(7), buffer.anchor_after(21)), 2),
            spec(
                3,
                (elsewhere.anchor_before(0), elsewhere.anchor_after(3)),
                3,
            ),
            spec(6, (buffer.anchor_before(35), buffer.anchor_after(49)), 4),
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
        let mut row = rho_agents_view::transcript::elisions::elision_row("2 tools", cx);
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
            &crate::DealDone,
            &crate::DealMute,
            &crate::DealTodo,
            &crate::DealFile,
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
            KeyContext::parse("RhoHome").unwrap(),
            KeyContext::parse("Editor vim_mode=insert vim_operator=none").unwrap(),
        ]));
        assert!(!resolves(&[
            KeyContext::parse("RhoGui").unwrap(),
            KeyContext::parse("Editor vim_mode=normal vim_operator=delete").unwrap(),
        ]));
    });
}

#[gpui::test]
fn undo_verdict_reaches_home_outside_a_deal(cx: &mut TestAppContext) {
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
        // Home itself, with no card on screen: vim binds `shift-u` one
        // level up, so without this the verb is lost on Home.
        for mode in ["normal", "helix_normal"] {
            assert!(
                resolves(&[
                    KeyContext::parse("RhoGui").unwrap(),
                    KeyContext::parse("RhoHome").unwrap(),
                    KeyContext::parse(&format!(
                        "Editor VimControl vim_mode={mode} vim_operator=none"
                    ))
                    .unwrap(),
                ]),
                "shift-u did not reach UndoVerdict on Home in {mode}"
            );
        }
        // Typing is still typing.
        assert!(!resolves(&[
            KeyContext::parse("RhoGui").unwrap(),
            KeyContext::parse("RhoHome").unwrap(),
            KeyContext::parse("Editor VimControl vim_mode=insert vim_operator=none").unwrap(),
        ]));
    });
}

#[test]
fn a_test_host_connection_has_nothing_dialing_behind_it() {
    assert!(
        !rho_agent_hosts::connection::supervises(),
        "a test binary starts no host supervisor"
    );
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

/// The display elisions this transcript elided history with, by id, so a
/// test can ask both whether history is elided and whether the same
/// elisions survived a rebuild.
fn history_elisions(
    editor: &Entity<editor::Editor>,
    cx: &mut TestAppContext,
) -> rustc_hash::FxHashSet<editor::DisplayElisionId> {
    cx.update(|cx| {
        editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            snapshot
                .blocks_in_range(DisplayRow(0)..snapshot.max_point().row() + 1)
                .filter_map(|(_, block)| match block {
                    editor::display_map::Block::DisplayElision(elision) => Some(elision.id),
                    _ => None,
                })
                .collect()
        })
    })
}

fn has_display_elision(workspace: &WindowHandle<Workspace>, cx: &mut TestAppContext) -> bool {
    let editor = active_editor(workspace, cx);
    !history_elisions(&editor, cx).is_empty()
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
/// What a bench measures is the pipeline, not the validation the test
/// build wraps it in: `rows_within_their_document` walks the document on
/// every wrap sync, and with it on one flushed replacement read 5.64s at
/// five thousand items where the same edit costs 33.4ms. A correctness
/// suite keeps it.
pub(crate) fn measure_the_pipeline_and_not_the_validation() {
    editor::display_map::set_wrap_rows_check_enabled(false);
}

#[gpui::test]
#[ignore = "benchmark"]
fn bench_markdown_transcript(cx: &mut TestAppContext) {
    measure_the_pipeline_and_not_the_validation();
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
    measure_the_pipeline_and_not_the_validation();
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
                    timing: Default::default(),
                    id: format!("t1.{}", blocks_count - 1),
                    name: "shell_command".to_owned(),
                    arguments: format!("echo {tick}"),
                    format: rho_agents_client::protocol::transcript::ArgumentsFormat::Text,
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
    // The reader's own words are not markdown: their asterisks are text
    // and stay on screen exactly as they typed them.
    assert!(
        text.contains("**user markup renders**"),
        "the reader's own words were read as markup: {text:?}"
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
    let folded_elisions = |cx: &mut TestAppContext| history_elisions(&preview, cx);
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
    let folded_elisions = |cx: &mut TestAppContext| history_elisions(&preview, cx);
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
    measure_the_pipeline_and_not_the_validation();
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
                timing: Default::default(),
                id: "tool-1".to_owned(),
                name: "shell_command".to_owned(),
                arguments: "echo".to_owned(),
                format: rho_agents_client::protocol::transcript::ArgumentsFormat::Text,
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
                timing: Default::default(),
                id: format!("tool-{ix}"),
                name: format!("tool_{ix}"),
                arguments: format!("arg-{ix}"),
                format: rho_agents_client::protocol::transcript::ArgumentsFormat::Text,
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
    assert!(buffer_text(&workspace, cx).ends_with("[rho-agent-host error: third]\n"));
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
                .chain((1..rho_agents_view::messages::LOG_CAP).map(|index| {
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
            for index in 0..=rho_agents_view::messages::LOG_CAP {
                workspace.append_test_log_entry(format!("message-{index}"), cx);
            }
            let messages = workspace.message_log_texts(cx);
            assert_eq!(messages.len(), rho_agents_view::messages::LOG_CAP);
            assert_eq!(messages.first().map(String::as_str), Some("message-1"));
            let expected_last = format!("message-{}", rho_agents_view::messages::LOG_CAP);
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
                (0..rho_agents_view::messages::LOG_CAP).map(|index| {
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
            for index in 0..rho_agents_view::messages::REBASE_EVICTIONS {
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
                rho_agents_view::messages::LOG_CAP
            );
        })
        .expect("inspect rebased messages");
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
        !history_elisions(&editor, cx).is_empty(),
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
                .any(
                    |message| message.contains("not connected to an agent host")
                ))
            .expect("read messages"),
        "submit should reach the workspace and report the failed send"
    );
    // Draft submissions keep the buffer until the agent host confirms creation,
    // so a failed send never loses the message.
    assert!(
        text.contains("hello rho"),
        "a failed draft submit should keep the message: {text:?}"
    );
}

#[gpui::test]
fn upload_gui_telemetry_action_reports_when_no_host_is_connected(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    cx.dispatch_action(*workspace, crate::UploadGuiTelemetry);
    assert!(
        workspace
            .update(cx, |workspace, _, cx| workspace
                .message_log_texts(cx)
                .iter()
                .any(|message| message.contains(
                    "performance snapshot: no agent host is connected"
                )))
            .expect("read messages"),
        "telemetry action should reach the workspace and fail nonfatally"
    );
}

/// Restore flow: the agent's first frame is a snapshot that already carries
/// `context_used` (agent host loaded it from the event log / transcript). The
/// status chips must show it without any live turn happening.
#[gpui::test]
fn restored_context_usage_shows_in_status_chips(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        UiAgentState {
            exec_timings: Default::default(),
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
            exec_timings: Default::default(),
            blocks: vec![Arc::new(user("go"))],
            status: UiAgentStatus::Idle,
            context_used: Some(62_300),
            usage: Default::default(),
        },
    );
    feed_edit(&workspace, cx, agent(1), |state| {
        state.usage = rho_agents_client::state::UiAgentUsage {
            provider: "fable".to_owned(),
            total: rho_agents_client::protocol::AgentUsageBucket {
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
    use rho_agent_types::Place;

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
                        place: Place {
                            workset: "0123456789ab".to_owned(),
                            cwd: "/src/rho".into(),
                            mode: Default::default(),
                            origin: Some("/tmp/rho".into()),
                        },
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
            exec_timings: Default::default(),
            blocks: vec![Arc::new(user("go"))],
            status: UiAgentStatus::Idle,
            context_used: Some(62_300),
            usage: rho_agents_client::state::UiAgentUsage {
                provider: "fable".to_owned(),
                total: rho_agents_client::protocol::AgentUsageBucket {
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

/// Two hundred turns whose answers are all markdown tables, so every page
/// of history composed carries virtual-tab inlays of its own.
fn long_history_of_tables() -> UiAgentState {
    let mut blocks = Vec::new();
    for turn in 0..200 {
        blocks.push(user(&format!("ask {turn}")));
        blocks.push(assistant(
            &format!("| Name | Outcome |\n| --- | --- |\n| turn {turn} | passed |\n"),
            Some(UiMessagePhase::FinalAnswer),
        ));
    }
    state(blocks, Vec::new())
}

/// Two hundred turns whose answers are all visualization refs, each its own
/// ref, so every page of history composed carries blocks of its own.
fn long_history_of_visualizations() -> UiAgentState {
    let mut blocks = Vec::new();
    for turn in 0..200 {
        blocks.push(user(&format!("ask {turn}")));
        blocks.push(assistant(
            &format!("```visualization\nref={turn:032x} rows=2\n```"),
            Some(UiMessagePhase::FinalAnswer),
        ));
    }
    state(blocks, Vec::new())
}

fn inlay_ids(workspace: &WindowHandle<Workspace>, cx: &mut TestAppContext) -> HashSet<InlayId> {
    let editor = active_editor(workspace, cx);
    workspace
        .update(cx, |_, _, cx| {
            editor
                .read(cx)
                .all_inlays(cx)
                .into_iter()
                .map(|inlay| inlay.id)
                .collect()
        })
        .expect("read the inlays the editor shows")
}

fn visualization_blocks(
    workspace: &WindowHandle<Workspace>,
    cx: &mut TestAppContext,
    agent_id: AgentId,
) -> HashSet<CustomBlockId> {
    workspace
        .update(cx, |workspace, _, cx| {
            workspace
                .agent_model_for_test(agent_id)
                .read(cx)
                .visualization_blocks()
                .into_iter()
                .collect()
        })
        .expect("read the visualization blocks the editor shows")
}

/// A page composes the records it renders and nothing else, so the inlays
/// it brings are added and the ones already on screen keep their ids: they
/// are never removed and placed again for a page they had no part in.
#[gpui::test]
fn a_page_adds_its_inlays_and_removes_none_elsewhere(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(&workspace, cx, agent(1), long_history_of_tables());
    cx.run_until_parked();
    let opened = inlay_ids(&workspace, cx);
    assert!(
        !opened.is_empty(),
        "the tail's tables are aligned with virtual tabs"
    );

    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_scroll_position(gpui::point(0., 0.), window, cx);
            });
        })
        .expect("scroll to the top of what is composed");
    cx.run_until_parked();

    let after = inlay_ids(&workspace, cx);
    assert!(
        opened.is_subset(&after),
        "a page must not disturb the inlays already placed: {opened:?} then {after:?}"
    );
    assert!(
        after.len() > opened.len(),
        "the page brings its own tables' inlays: {} then {}",
        opened.len(),
        after.len()
    );
}

/// The same for visualizations: a page adds the blocks its own refs need
/// and leaves every block already placed where it is.
#[gpui::test]
fn a_page_adds_its_visualizations_and_removes_none_elsewhere(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(&workspace, cx, agent(1), long_history_of_visualizations());
    cx.run_until_parked();
    let opened = visualization_blocks(&workspace, cx, agent(1));
    assert!(!opened.is_empty(), "the tail's refs are blocks");

    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, window, cx| {
            editor.update(cx, |editor, cx| {
                editor.set_scroll_position(gpui::point(0., 0.), window, cx);
            });
        })
        .expect("scroll to the top of what is composed");
    cx.run_until_parked();

    let after = visualization_blocks(&workspace, cx, agent(1));
    assert!(
        opened.is_subset(&after),
        "a page must not disturb the blocks already placed: {opened:?} then {after:?}"
    );
    assert!(
        after.len() > opened.len(),
        "the page brings its own refs' blocks: {} then {}",
        opened.len(),
        after.len()
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
/// the transcript, and the next unread in a room — by their own
/// contexts, not by which binding was loaded
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
            let name = "RhoTranscript";
            {
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

/// What a call ran and what the reader typed are shown as they are: no
/// markdown grammar over either, so neither gets a delimiter on screen nor
/// a highlight from a parser. Only the model's own words are markdown.
#[gpui::test]
fn a_call_and_the_users_words_are_plain_text(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let ran = UiBlock::Tool(UiTool {
        timing: Default::default(),
        id: "tool-1".to_owned(),
        name: "shell".to_owned(),
        arguments: r#"{"command":"echo **bold** and _under_"}"#.to_owned(),
        format: rho_agents_client::protocol::transcript::ArgumentsFormat::Json,
        preview: None,
        status: UiToolStatus::Success,
        output: None,
        error: None,
        started_at: Some(rho_agent_types::UnixMs(10)),
        finished_at: Some(rho_agent_types::UnixMs(20)),
        metadata: None,
    });
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(
            Vec::new(),
            vec![
                user("**my** request"),
                assistant(
                    "**first** assistant segment",
                    Some(UiMessagePhase::Commentary),
                ),
                ran,
                assistant(
                    "**second** assistant segment",
                    Some(UiMessagePhase::FinalAnswer),
                ),
            ],
        ),
    );
    cx.run_until_parked();

    let text = display_text(&workspace, cx);
    assert!(
        text.contains("$ echo **bold** and _under_"),
        "the call's own punctuation was read as markup: {text:?}"
    );
    assert!(
        text.contains("**my** request"),
        "the reader's own punctuation was read as markup: {text:?}"
    );
    assert!(
        !text.contains("`"),
        "a delimiter reached the screen: {text:?}"
    );
    assert!(
        text.contains("first assistant segment") && !text.contains("**first**"),
        "the model's markdown stopped rendering: {text:?}"
    );

    // Nothing parses those two rows, so no chunk in them carries a
    // highlight, while the model's prose keeps the ones it had.
    let call = syntax_highlights_for_text(&workspace, "echo **bold** and _under_", cx);
    assert!(
        call.iter().all(Option::is_none),
        "a call's row is highlighted: {call:?}"
    );
    let typed = syntax_highlights_for_text(&workspace, "**my** request", cx);
    assert!(
        typed.iter().all(Option::is_none),
        "the reader's own words are highlighted: {typed:?}"
    );
    let prose = syntax_highlights_for_text(&workspace, "**first**", cx);
    assert!(
        prose.iter().any(Option::is_some),
        "the model's words lost their highlighting: {prose:?}"
    );

    let editor = active_editor(&workspace, cx);
    workspace
        .update(cx, |_, _, cx| {
            let buffers = editor.read(cx).buffer().read(cx).all_buffers();
            for buffer in buffers {
                let buffer = buffer.read(cx);
                let plain = buffer.text().contains("echo **bold**")
                    || buffer.text().contains("**my** request");
                if plain {
                    assert!(
                        buffer.language().is_none(),
                        "a call or the reader's words are in a parsed buffer: {:?}",
                        buffer.text()
                    );
                }
            }
        })
        .expect("inspect transcript buffers");
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
fn unnamed_gpt_quota_is_visible_to_the_status_line(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let summary = rho_agents_client::protocol::QuotaSummary {
        model: "gpt".to_owned(),
        auth_namespace: None,
        remaining_percent: 40,
        burn_10m: 0,
        burn_2h: 0,
        burn_1d: 0,
        burn_3d: 0,
        reset_at_unix: Some(1),
    };
    workspace
        .update(cx, |workspace, window, cx| {
            workspace.handle_model_event(
                HostId::default(),
                rho_agents_client::model::ModelMsg::QuotaUsage {
                    summaries: vec![summary.clone()],
                },
                window,
                cx,
            );
            assert_eq!(workspace.merged_quota_summaries_for_test(), vec![summary]);
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

/// The Slack key table, one assertion per row.
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

fn ui_head(agent_id: AgentId) -> story::UiAgentHead {
    story::UiAgentHead {
        agent_id,
        story_pos: story::UiStoryPos(0),
        role: rho_agent_types::AgentRole::default(),
        runtime_kind: story::UiRuntimeKind::Rho,
        place: rho_agent_types::Place {
            workset: "0123456789ab".into(),
            cwd: "/src/tmp".into(),
            mode: Default::default(),
            origin: None,
        },
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
fn story_wanting(agent_id: AgentId, at: UnixMs) -> rho_agents_client::stream::AgentFrame {
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
                .any(
                    |message| message.contains("not connected to an agent host")
                ))
            .expect("read messages"),
        "enter in a new-agent draft should submit the draft"
    );
}

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
    let mut host = workspace
        .update(cx, |workspace, _, _| {
            workspace.host_in_process_for_test(HostId::default())
        })
        .unwrap();

    cx.dispatch_action(*workspace, crate::SubmitPrompt);
    cx.run_until_parked();
    let mut calls = story::calls(&mut host);
    assert_eq!(calls.len(), 1, "the draft makes one call");
    let (call, mut stream) = calls.pop().unwrap();
    assert!(
        matches!(call, rho_agents_client::protocol::Request::New(_)),
        "the draft asked for a new agent: {call:?}"
    );
    story::answer(
        &mut stream,
        rho_rpc::protocol::Answer::<AgentId>::Failed {
            reason: "create workspace: no such repository".to_owned(),
        },
    );
    cx.run_until_parked();

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
        "the draft keeps the agent host's whole cause: {refusal:?}"
    );
}

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

    cx.dispatch_action(*workspace, rho_agents_view::RoleCycle);
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
                .any(
                    |message| message.contains("not connected to an agent host")
                ))
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

    // From the body, backwards is the filesystem row, then the start row,
    // then the role row.
    cx.dispatch_action(*workspace, rho_agents_view::RoleCycleGroup);
    workspace
        .update(cx, |workspace, _, cx| {
            assert!(workspace.cursor_in_draft_filesystem_field_for_test(cx));
        })
        .expect("filesystem row");
    cx.dispatch_action(*workspace, rho_agents_view::RoleCycleGroup);
    workspace
        .update(cx, |workspace, _, cx| {
            assert!(workspace.cursor_in_draft_start_field_for_test(cx));
        })
        .expect("start row");
    cx.dispatch_action(*workspace, rho_agents_view::RoleCycleGroup);
    workspace
        .update(cx, |workspace, _, cx| {
            assert!(workspace.cursor_in_draft_role_field_for_test(cx));
        })
        .expect("role row");
}

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
    cx.dispatch_action(*workspace, rho_agents_view::RoleCycle);
    cx.dispatch_action(*workspace, rho_agents_view::DraftFieldClear);
    cx.simulate_keystrokes(*workspace, "o k");
    cx.run_until_parked();

    workspace
        .update(cx, |workspace, _, cx| {
            let draft = workspace.draft_model_for_test().read(cx);
            assert_eq!(draft.workdir_text(cx), "ok", "the row holds what was typed");
            assert_eq!(draft.role_text(cx), "med-eng", "the row below is untouched");
        })
        .expect("read the rows");
}

enum FoldedInThisTest {}

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
                    rho_agents_view::agent_view::AgentModelEvent::HistoryComposed(_)
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

#[gpui::test]
fn the_buffer_picker_offers_home_before_the_context_has_shown_it(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            // Slack's context, before anything of Slack's has been opened
            // in it: the list is empty and Home has never been shown here.
            workspace.active_context = crate::workspace::ContextId::Slack;
            let home = workspace
                .buffer_table()
                .into_iter()
                .filter(|(name, _)| name == "home")
                .collect::<Vec<_>>();
            assert_eq!(
                home,
                vec![("home".to_owned(), "home".to_owned())],
                "one home row, in a context that has never shown it"
            );

            workspace.switch_buffer("home", window, cx);
            assert!(workspace.home_in_view(), "and choosing it opens Home");

            let home = workspace
                .buffer_table()
                .into_iter()
                .filter(|(name, _)| name == "home")
                .count();
            assert_eq!(home, 1, "and it is still one row, not two");
        })
        .unwrap();
}

fn home_text(workspace: &gpui::WindowHandle<Workspace>, cx: &mut TestAppContext) -> String {
    workspace
        .update(cx, |workspace, _, cx| {
            let home = workspace.home_view().expect("home is in view");
            home.update(cx, |home, cx| {
                let editor = home.editor().clone();
                editor.read(cx).buffer().read(cx).snapshot(cx).text()
            })
        })
        .unwrap()
}

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

/// A frame, so deferred redraws have run before the test reads the surface.
fn next_frame(cx: &mut TestAppContext, workspace: WindowHandle<Workspace>) {
    cx.update_window(*workspace, |_, window, cx| window.simulate_next_frame(cx))
        .expect("draw a frame");
    cx.run_until_parked();
}

fn agent(id: u64) -> AgentId {
    AgentId::from_counter(id, &rho_agent_types::AgentIdDomain(0)).unwrap()
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

/// Feeds the transcript back with one change, as an agent host that saw more
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
        timing: Default::default(),
        id: id.to_owned(),
        name: "shell_command".to_owned(),
        arguments: "echo ok".to_owned(),
        format: rho_agents_client::protocol::transcript::ArgumentsFormat::Text,
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
        exec_timings: Default::default(),
        blocks,
        status: UiAgentStatus::Streaming,
        context_used: None,
        usage: Default::default(),
    }
}

fn long_working_text() -> String {
    "alpha\nbravo\ncharlie\ndelta\necho\nfoxtrot\ngolf\nhotel\nindia\njuliet\nkilo\nlima\nmike\nnovember\noscar\npapa\n".to_owned()
}

/// A done verdict records the story position, not only a local hidden card.
/// Undo takes that exact ledger mark back and the same story asks again.
#[gpui::test]
fn agent_done_writes_its_story_cursor_and_undo_restores_the_hand(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let agent_id = agent(701);
    let node = rho_dealer::NodeId::Agent(agent_id);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ready_with(vec![ui_head(agent_id)], 702),
                window, cx,
            );
            story::feed(workspace, HostId::default(), story_wanting(agent_id, UnixMs(100)), window, cx);
            assert!(workspace.hand().iter().any(|card| card.node == node));
            workspace.pull_card(window, cx);
            assert_eq!(workspace.current_deal_card_for_test(cx).map(|card| card.0), Some(node.clone()));
            workspace.verdict_done(window, cx);
            assert!(matches!(workspace.attention.marks.get(&node).handled, Some(rho_dealer::marks::Cursor::Story(pos)) if pos > 0));
            assert!(!workspace.hand().iter().any(|card| card.node == node));
            workspace.undo_verdict(window, cx);
            assert_eq!(workspace.attention.marks.get(&node).handled, None);
            assert!(workspace.hand().iter().any(|card| card.node == node));
        })
        .unwrap();
}

#[gpui::test]
fn a_future_snooze_hides_an_agent_until_the_mark_ripens(cx: &mut TestAppContext) {
    use rho_dealer::{DateMark, NodeId, marks};
    let workspace = test_workspace(cx);
    let agent_id = agent(703);
    let node = NodeId::Agent(agent_id);
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ready_with(vec![ui_head(agent_id)], 704),
                window,
                cx,
            );
            story::feed(
                workspace,
                HostId::default(),
                story_wanting(agent_id, UnixMs(100)),
                window,
                cx,
            );
            assert!(workspace.hand().iter().any(|card| card.node == node));
            workspace.write_marks(
                vec![marks::snooze(&node, Some(DateMark::at(4_000_000_000_000)))],
                cx,
            );
            assert!(!workspace.hand().iter().any(|card| card.node == node));
            workspace.write_marks(vec![marks::snooze(&node, Some(DateMark::at(1_000)))], cx);
            assert!(workspace.hand().iter().any(|card| card.node == node));
        })
        .unwrap();
}

#[gpui::test]
fn toggling_a_label_twice_restores_the_note_and_find_uses_its_path(cx: &mut TestAppContext) {
    use rho_dealer::marks;
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            let note = workspace.create_note(None, cx);
            workspace.write_marks(vec![marks::body(&note, "Frost report\nDetails")], cx);
            let (on, _) = workspace.toggle_label(&note, "ops/weather", cx).unwrap();
            assert!(on);
            let label = workspace.attention.marks.label_at("ops/weather").unwrap();
            assert!(workspace.attention.marks.get(&note).labels.contains(&label));
            let candidate = workspace
                .find_candidates(cx)
                .into_iter()
                .find(|candidate| candidate.target == crate::find::FindTarget::Node(note.clone()))
                .unwrap();
            assert!(
                candidate
                    .names_for_test()
                    .iter()
                    .any(|name| name == "ops/weather › Frost report")
            );
            assert!(
                workspace
                    .find_rows_for_test("ops/weather", cx)
                    .iter()
                    .any(|row| row.value.contains("Frost report"))
            );
            workspace.open_node(&note, window, cx);
            assert_eq!(
                workspace.current_surface_key_for_test(),
                crate::pane::SurfaceKey::Note(note.clone())
            );
            let (on, _) = workspace.toggle_label(&note, "ops/weather", cx).unwrap();
            assert!(!on);
            assert!(!workspace.attention.marks.get(&note).labels.contains(&label));
        })
        .unwrap();
}

#[gpui::test]
fn deleting_a_label_takes_its_sublabels_and_undo_brings_them_back(cx: &mut TestAppContext) {
    use rho_dealer::NodeId;
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            let ops = workspace.mint_label("ops/weather", cx).unwrap();
            let ops = workspace
                .attention
                .marks
                .get(&NodeId::Label(ops))
                .parent
                .unwrap();
            workspace.open_node(&NodeId::Label(ops), window, cx);
            workspace.delete_made(window, cx);
            assert!(workspace.attention.marks.label_at("ops").is_none());
            assert!(workspace.attention.marks.label_at("ops/weather").is_none());
            assert_ne!(
                workspace.current_surface_key_for_test(),
                crate::pane::SurfaceKey::Note(NodeId::Label(ops))
            );
            workspace.undo_verdict(window, cx);
            assert_eq!(workspace.attention.marks.label_at("ops"), Some(ops));
            assert!(workspace.attention.marks.label_at("ops/weather").is_some());
        })
        .unwrap();
}

#[gpui::test]
fn deleting_a_note_leaves_it_out_of_the_notes(cx: &mut TestAppContext) {
    use rho_dealer::marks;
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            let note = workspace.create_note(None, cx);
            workspace.write_marks(vec![marks::body(&note, "Frost report")], cx);
            workspace.open_node(&note, window, cx);
            workspace.delete_made(window, cx);
            assert!(workspace.attention.marks.get(&note).deleted);
            assert!(
                workspace
                    .attention
                    .marks
                    .notes()
                    .all(|(held, _)| *held != note)
            );
        })
        .unwrap();
}

#[gpui::test]
fn a_moved_label_takes_its_sublabels_along(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, _window, cx| {
            let weather = workspace.mint_label("ops/weather/frost", cx).unwrap();
            let weather = workspace
                .attention
                .marks
                .get(&rho_dealer::NodeId::Label(weather))
                .parent
                .unwrap();
            workspace.move_label(weather, "areas/climate", cx);
            assert_eq!(
                workspace.attention.marks.label_path(weather),
                "areas/climate"
            );
            assert!(
                workspace
                    .attention
                    .marks
                    .label_at("areas/climate/frost")
                    .is_some()
            );
            assert!(workspace.attention.marks.label_at("ops").is_some());
            // Not under itself.
            workspace.move_label(weather, "areas/climate/frost/weather", cx);
            assert_eq!(
                workspace.attention.marks.label_path(weather),
                "areas/climate"
            );
        })
        .unwrap();
}

#[gpui::test]
fn creating_a_note_from_a_label_files_it_and_home_reads_the_dated_card(cx: &mut TestAppContext) {
    use rho_dealer::{DateMark, NodeId, marks};
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            let label = workspace.mint_label("writing/reviews", cx).unwrap();
            workspace.open_note(&NodeId::Label(label), window, cx);
            let areas = workspace.areas_for_test(cx);
            assert!(areas.iter().any(|(path, _)| path == "writing/reviews"));
            let note = workspace.create_note(Some(&NodeId::Label(label)), cx);
            workspace.write_marks(
                vec![
                    marks::body(&note, "Review the patch"),
                    marks::todo(
                        &note,
                        Some(marks::Todo {
                            wakes: Some(DateMark::at(1_000)),
                            deadline: None,
                            pace_days: 3,
                        }),
                    ),
                ],
                cx,
            );
            assert!(workspace.attention.marks.get(&note).labels.contains(&label));
            assert!(workspace.hand().iter().any(|card| card.node == note));
            workspace.open_home(window, cx);
        })
        .unwrap();
    next_frame(cx, workspace);
    assert!(home_text(&workspace, cx).contains("Review the patch"));
}

#[gpui::test]
fn a_phone_flick_moves_from_one_dated_card_to_the_next(cx: &mut TestAppContext) {
    use rho_dealer::{DateMark, marks};
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, _, cx| {
            for title in ["First phone card", "Second phone card"] {
                let note = workspace.create_note(None, cx);
                workspace.write_marks(
                    vec![
                        marks::body(&note, title),
                        marks::todo(
                            &note,
                            Some(marks::Todo {
                                wakes: Some(DateMark::at(1_000)),
                                deadline: None,
                                pace_days: 3,
                            }),
                        ),
                    ],
                    cx,
                );
            }
        })
        .unwrap();
    cx.simulate_window_resize(*workspace, size(px(400.), px(800.)));
    next_frame(cx, workspace);
    let first = workspace
        .update(cx, |workspace, _, cx| {
            assert!(workspace.phone_feed_for_test(cx));
            workspace.current_deal_card_for_test(cx).unwrap().0
        })
        .unwrap();
    cx.update_window(*workspace, |_, window, cx| {
        for (phase, y, millis) in [
            (TouchPhase::Started, 600., 0),
            (TouchPhase::Moved, 300., 80),
            (TouchPhase::Ended, 300., 100),
        ] {
            window.dispatch_event(
                TouchEvent {
                    id: TouchId(1),
                    phase,
                    position: point(px(200.), px(y)),
                    timestamp: std::time::Duration::from_millis(millis),
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
        }
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
fn the_new_note_and_agent_area_picker_files_at_the_label_in_view(cx: &mut TestAppContext) {
    use rho_dealer::NodeId;
    let workspace = test_workspace(cx);
    let label = workspace
        .update(cx, |workspace, window, cx| {
            let label = workspace.mint_label("software/rho", cx).unwrap();
            workspace.open_note(&NodeId::Label(label), window, cx);
            let areas = workspace.areas_for_test(cx);
            assert!(
                areas
                    .iter()
                    .any(|(path, kind)| path == "software/rho" && kind == "label")
            );
            assert!(
                areas
                    .iter()
                    .any(|(path, kind)| path == "here" && kind == "what is on screen")
            );
            workspace.begin_new(crate::create::NewKind::Agent, window, cx);
            assert!(workspace.minibuffer.is_some(), "agent asks for an area");
            label
        })
        .unwrap();
    cx.dispatch_action(*workspace, crate::MinibufferConfirm);
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, window, cx| {
            assert_eq!(workspace.draft_area_for_test(), Some(NodeId::Label(label)));
            workspace.open_note(&NodeId::Label(label), window, cx);
            workspace.begin_new(crate::create::NewKind::Note, window, cx);
            assert!(workspace.minibuffer.is_some(), "note asks for an area");
        })
        .unwrap();
    cx.dispatch_action(*workspace, crate::MinibufferConfirm);
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, _| {
            let crate::pane::SurfaceKey::Note(NodeId::Note(id)) =
                workspace.current_surface_key_for_test()
            else {
                panic!("new note opens its own surface");
            };
            assert!(
                workspace
                    .attention
                    .marks
                    .get(&NodeId::Note(id))
                    .labels
                    .contains(&label)
            );
        })
        .unwrap();
}

#[gpui::test]
fn a_note_todo_writes_its_pace_and_undo_restores_the_old_date(cx: &mut TestAppContext) {
    use rho_dealer::{DateMark, marks};
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            let note = workspace.create_note(None, cx);
            let old = marks::Todo {
                wakes: Some(DateMark::at(1_000)),
                deadline: None,
                pace_days: 2,
            };
            workspace.write_marks(
                vec![
                    marks::body(&note, "Pay invoice"),
                    marks::todo(&note, Some(old)),
                ],
                cx,
            );
            workspace.pull_card(window, cx);
            assert_eq!(
                workspace.current_deal_card_for_test(cx).map(|card| card.0),
                Some(note.clone())
            );
            workspace.verdict_todo(Some(9), window, cx);
            assert_eq!(
                workspace.attention.marks.get(&note).todo.unwrap().pace_days,
                9
            );
            workspace.undo_verdict(window, cx);
            assert_eq!(workspace.attention.marks.get(&note).todo, Some(old));
        })
        .unwrap();
}

#[gpui::test]
fn muting_a_dated_note_takes_it_out_of_the_hand_until_undo(cx: &mut TestAppContext) {
    use rho_dealer::{DateMark, marks};
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            let note = workspace.create_note(None, cx);
            workspace.write_marks(
                vec![
                    marks::body(&note, "Read contract"),
                    marks::todo(
                        &note,
                        Some(marks::Todo {
                            wakes: Some(DateMark::at(1_000)),
                            deadline: None,
                            pace_days: 2,
                        }),
                    ),
                ],
                cx,
            );
            workspace.pull_card(window, cx);
            assert_eq!(
                workspace.current_deal_card_for_test(cx).map(|card| card.0),
                Some(note.clone())
            );
            workspace.verdict_mute(window, cx);
            assert!(workspace.attention.marks.get(&note).muted);
            assert!(!workspace.hand().iter().any(|card| card.node == note));
            workspace.undo_verdict(window, cx);
            assert!(!workspace.attention.marks.get(&note).muted);
            assert!(workspace.hand().iter().any(|card| card.node == note));
        })
        .unwrap();
}

#[gpui::test]
fn flicking_down_from_an_empty_phone_feed_undoes_its_last_verdict(cx: &mut TestAppContext) {
    use rho_dealer::{DateMark, marks};
    let workspace = test_workspace(cx);
    let note = workspace
        .update(cx, |workspace, _, cx| {
            let note = workspace.create_note(None, cx);
            workspace.write_marks(
                vec![
                    marks::body(&note, "Last phone card"),
                    marks::todo(
                        &note,
                        Some(marks::Todo {
                            wakes: Some(DateMark::at(1_000)),
                            deadline: None,
                            pace_days: 3,
                        }),
                    ),
                ],
                cx,
            );
            note
        })
        .unwrap();
    cx.simulate_window_resize(*workspace, size(px(400.), px(800.)));
    next_frame(cx, workspace);
    cx.dispatch_action(*workspace, crate::DealDone);
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(workspace.current_deal_card_for_test(cx), None);
            workspace.phone_remember_last_verdict_for_test();
        })
        .unwrap();
    cx.update_window(*workspace, |_, window, cx| {
        for (phase, y, millis) in [
            (TouchPhase::Started, 250., 0),
            (TouchPhase::Moved, 550., 80),
            (TouchPhase::Ended, 550., 100),
        ] {
            window.dispatch_event(
                TouchEvent {
                    id: TouchId(1),
                    phase,
                    position: point(px(200.), px(y)),
                    timestamp: std::time::Duration::from_millis(millis),
                    ..Default::default()
                }
                .to_platform_input(),
                cx,
            );
        }
    })
    .unwrap();
    cx.executor()
        .advance_clock(std::time::Duration::from_millis(200));
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, _, cx| {
            assert_eq!(
                workspace.current_deal_card_for_test(cx).map(|card| card.0),
                Some(note.clone())
            );
            assert!(workspace.phone_feed_is_active_for_test());
            assert!(workspace.attention.marks.get(&note).todo.is_some());
        })
        .unwrap();
}

#[gpui::test]
fn desktop_advertisements_are_agent_scoped_and_disappear(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
            let sessions = [
                (agent(1), "preview"),
                (agent(2), "other"),
                (agent(1), "browser"),
            ]
            .into_iter()
            .map(
                |(agent, name)| rho_desktop_client::protocol::DesktopSession {
                    agent: agent.encoded(),
                    name: name.into(),
                },
            )
            .collect();
            workspace.desktops_arrived(HostId::default(), sessions, cx);
            assert_eq!(
                workspace.available_desktops(agent(1)),
                ["browser", "preview"]
            );
            assert_eq!(workspace.available_desktops(agent(2)), ["other"]);
            workspace.desktops_arrived(
                HostId::default(),
                vec![rho_desktop_client::protocol::DesktopSession {
                    agent: agent(1).encoded(),
                    name: "preview".into(),
                }],
                cx,
            );
            assert_eq!(workspace.available_desktops(agent(1)), ["preview"]);
            assert!(workspace.available_desktops(agent(2)).is_empty());
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::Disconnected("test".into()),
                window,
                cx,
            );
            assert!(workspace.available_desktops(agent(1)).is_empty());
        })
        .unwrap();
}

#[gpui::test]
fn q_discards_a_draft_from_surface_history(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
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

#[gpui::test]
fn discarding_a_draft_preserves_non_draft_history_cursor(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = overview_workspace(cx);
    workspace
        .update(cx, |workspace, window, cx| {
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
