#[path = "../../gpui_linux/src/linux/wayland/text_input.rs"]
mod wayland_text_input;

use editor::{Editor, EditorMode, SelectionEffects};
use gpui::{
    App, AppContext as _, EntityInputHandler, Focusable as _, InputEvent as _, MouseButton,
    MouseDownEvent, TestAppContext, TouchEvent, TouchId, TouchPhase, WindowHandle, point, px, size,
};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, MultiBufferOffset, PathKey};
use std::time::Duration;

fn init(cx: &mut App) {
    assets::Assets.load_test_fonts(cx);
    settings::init(cx);
    theme_settings::init(theme::LoadThemes::JustBase, cx);
    editor::init(cx);
}

fn draw(editor: &WindowHandle<Editor>, cx: &mut TestAppContext) {
    cx.run_until_parked();
    cx.update_window(**editor, |_, window, cx| {
        window.refresh();
        window.simulate_next_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
}

fn text_point(
    editor: &WindowHandle<Editor>,
    cx: &mut TestAppContext,
    index: usize,
) -> gpui::Point<gpui::Pixels> {
    editor
        .update(cx, |editor, window, cx| {
            let bounds = *editor.last_bounds().unwrap();
            let character = editor
                .bounds_for_range(index..index, bounds, window, cx)
                .unwrap();
            character.origin + point(px(0.), character.size.height / 2.)
        })
        .unwrap()
}

fn touch(
    editor: &WindowHandle<Editor>,
    cx: &mut TestAppContext,
    phase: TouchPhase,
    position: gpui::Point<gpui::Pixels>,
    ms: u64,
) {
    cx.update_window(**editor, |_, window, cx| {
        window.dispatch_event(
            TouchEvent {
                id: TouchId(1),
                phase,
                position,
                timestamp: Duration::from_millis(ms),
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
    })
    .unwrap();
}

fn selection(editor: &WindowHandle<Editor>, cx: &mut TestAppContext) -> std::ops::Range<usize> {
    editor
        .update(cx, |editor, window, cx| {
            editor.selected_text_range(true, window, cx).unwrap().range
        })
        .unwrap()
}

#[gpui::test]
fn touch_input_word_handles_and_desktop_secondary_click(cx: &mut TestAppContext) {
    cx.update(init);
    let editor = cx.add_window(|window, cx| {
        let mut editor = Editor::multi_line(window, cx);
        editor.set_text("oak λambda birch\ncedar", window, cx);
        window.focus(&editor.focus_handle(cx), cx);
        editor
    });
    cx.simulate_window_resize(*editor, size(px(360.), px(720.)));
    draw(&editor, cx);
    let word = text_point(&editor, cx, 7);

    cx.update_window(*editor, |_, window, cx| {
        window.dispatch_event(
            MouseDownEvent {
                button: MouseButton::Right,
                position: word,
                ..Default::default()
            }
            .to_platform_input(),
            cx,
        );
    })
    .unwrap();
    assert_eq!(
        selection(&editor, cx),
        22..22,
        "desktop right click must not select a word"
    );
    // Dismiss the desktop menu before touch.
    editor
        .update(cx, |editor, window, cx| {
            editor.cancel(&editor::Cancel, window, cx);
            editor.set_mouse_click_selection_enabled(false, cx);
        })
        .unwrap();
    draw(&editor, cx);

    touch(&editor, cx, TouchPhase::Started, word, 0);
    cx.run_until_parked();
    cx.background_executor
        .advance_clock(Duration::from_millis(550));
    cx.run_until_parked();
    assert_eq!(
        selection(&editor, cx),
        4..10,
        "long press selects the complete Unicode word, not the adjacent space"
    );
    touch(
        &editor,
        cx,
        TouchPhase::Moved,
        word + point(px(1.), px(0.)),
        560,
    );
    assert_eq!(
        selection(&editor, cx),
        4..10,
        "small long-press motion inside the word must not shrink it"
    );
    touch(&editor, cx, TouchPhase::Ended, word, 600);
    draw(&editor, cx);

    let line_height = editor
        .update(cx, |editor, window, cx| {
            editor
                .bounds_for_range(10..10, *editor.last_bounds().unwrap(), window, cx)
                .unwrap()
                .size
                .height
        })
        .unwrap();
    let handle_offset = point(px(0.), line_height / 2. + px(8.));
    let end_handle = text_point(&editor, cx, 10) + handle_offset;
    let birch_end = text_point(&editor, cx, 16) + handle_offset;
    touch(&editor, cx, TouchPhase::Started, end_handle, 700);
    touch(&editor, cx, TouchPhase::Moved, birch_end, 750);
    touch(&editor, cx, TouchPhase::Ended, birch_end, 800);
    assert_eq!(
        selection(&editor, cx),
        4..16,
        "end handle extends without moving the fixed start"
    );
    draw(&editor, cx);

    let start_handle = text_point(&editor, cx, 4) + handle_offset;
    let beyond_fixed_end = text_point(&editor, cx, 20) + handle_offset;
    touch(&editor, cx, TouchPhase::Started, start_handle, 900);
    touch(&editor, cx, TouchPhase::Moved, beyond_fixed_end, 950);
    touch(&editor, cx, TouchPhase::Cancelled, beyond_fixed_end, 1000);
    assert_eq!(
        selection(&editor, cx),
        16..20,
        "start handle crosses the fixed end and keeps selection ordered"
    );
    draw(&editor, cx);

    let oak = text_point(&editor, cx, 1);
    touch(&editor, cx, TouchPhase::Started, oak, 1100);
    touch(&editor, cx, TouchPhase::Ended, oak, 1150);
    assert_eq!(
        selection(&editor, cx),
        1..1,
        "cancel releases drag capture, so a new tap places the caret"
    );
}

#[gpui::test]
fn touch_input_ime_requires_editable_selection_in_mixed_buffer(cx: &mut TestAppContext) {
    cx.update(init);
    let history = cx.new(|cx| {
        let mut buffer = Buffer::local("history", cx);
        buffer.set_capability(Capability::ReadOnly, cx);
        buffer
    });
    let draft = cx.new(|cx| Buffer::local("draft", cx));
    let multibuffer = cx.new(|cx| {
        let mut buffer = MultiBuffer::new(Capability::ReadWrite);
        buffer.set_excerpts_for_path(
            PathKey::sorted(0),
            history,
            [Point::new(0, 0)..Point::new(0, 7)],
            0,
            cx,
        );
        buffer.set_excerpts_for_path(
            PathKey::sorted(1),
            draft,
            [Point::new(0, 0)..Point::new(0, 5)],
            0,
            cx,
        );
        buffer
    });
    let editor =
        cx.add_window(|window, cx| Editor::new(EditorMode::full(), multibuffer, None, window, cx));
    editor
        .update(cx, |editor, window, cx| {
            editor.set_mouse_click_selection_enabled(false, cx);
            window.focus(&editor.focus_handle(cx), cx);
            let cases = [
                (0..0, false),
                (7..7, false),
                (8..8, true),
                (13..13, true),
                (6..9, false),
                (8..13, true),
            ];
            for (range, expected) in cases {
                editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                    selections.select_ranges([
                        MultiBufferOffset(range.start)..MultiBufferOffset(range.end)
                    ]);
                });
                assert_eq!(
                    editor.accepts_text_input(window, cx),
                    expected,
                    "range {range:?}"
                );
            }
            editor.set_expects_character_input(false);
            assert!(
                !editor.accepts_text_input(window, cx),
                "modal command mode does not request IME"
            );
            editor.set_expects_character_input(true);
            editor.set_read_only(true);
            assert!(
                !editor.accepts_text_input(window, cx),
                "editor override disables even a writable tail"
            );
            editor.set_read_only(false);
            assert!(
                editor.accepts_text_input(window, cx),
                "editable tail can reactivate IME"
            );
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([
                    MultiBufferOffset(8)..MultiBufferOffset(8),
                    MultiBufferOffset(0)..MultiBufferOffset(0),
                ]);
            });
            assert!(
                !editor.accepts_text_input(window, cx),
                "a writable primary cursor must not hide a read-only secondary cursor"
            );
        })
        .unwrap();
    cx.simulate_window_resize(*editor, size(px(360.), px(720.)));
    draw(&editor, cx);
    let history = text_point(&editor, cx, 2);
    touch(&editor, cx, TouchPhase::Started, history, 0);
    touch(&editor, cx, TouchPhase::Ended, history, 60);
    assert_eq!(
        selection(&editor, cx),
        2..2,
        "touch must place a caret even when desktop click selection is disabled"
    );
    editor
        .update(cx, |editor, window, cx| {
            assert!(
                !editor.accepts_text_input(window, cx),
                "reading history disables IME"
            );
        })
        .unwrap();
    draw(&editor, cx);
    let draft = text_point(&editor, cx, 10);
    touch(&editor, cx, TouchPhase::Started, draft, 100);
    touch(&editor, cx, TouchPhase::Ended, draft, 160);
    assert_eq!(selection(&editor, cx), 10..10);
    editor
        .update(cx, |editor, window, cx| {
            assert!(
                editor.accepts_text_input(window, cx),
                "touching draft re-enables IME"
            );
        })
        .unwrap();
}

#[gpui::test]
fn touch_input_ime_context_delete_preserves_selection_and_preedit_cursor(cx: &mut TestAppContext) {
    cx.update(init);
    let editor = cx.add_window(|window, cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_text("a😀é漢Z", window, cx);
        editor
    });
    editor
        .update(cx, |editor, window, cx| {
            window.focus(&editor.focus_handle(cx), cx);
            editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
                selections.select_ranges([MultiBufferOffset(7)..MultiBufferOffset(5)]);
            });
            editor.delete_surrounding_text(2, 1, window, cx);
            assert_eq!(editor.text(cx), "aéZ");
            let selection = editor.selected_text_range(false, window, cx).unwrap();
            assert_eq!(selection.range, 1..2);
            assert!(
                selection.reversed,
                "context deletion retains selection direction"
            );
            editor.replace_text_in_range(None, "中", window, cx);
            assert_eq!(
                editor.text(cx),
                "a中Z",
                "commit replaces selection, not its context"
            );

            editor.replace_and_mark_text_in_range(None, "α😀漢", Some(1..3), window, cx);
            assert_eq!(
                editor.selected_text_range(false, window, cx).unwrap().range,
                3..5
            );
            editor.set_ime_cursor_visible(false, window, cx);
            assert!(!editor.show_local_cursors(window, cx));
            editor.unmark_text(window, cx);
            assert!(editor.marked_text_range(window, cx).is_none());
            editor.set_ime_cursor_visible(true, window, cx);
        })
        .unwrap();
}

#[gpui::test]
fn touch_input_wayland_done_applies_context_commit_and_preedit_atomically(cx: &mut TestAppContext) {
    cx.update(init);
    let editor = cx.add_window(|window, cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_text("a😀漢Z", window, cx);
        editor.change_selections(SelectionEffects::no_scroll(), window, cx, |selections| {
            selections.select_ranges([MultiBufferOffset(5)..MultiBufferOffset(5)]);
        });
        editor.replace_and_mark_text_in_range(None, "old", Some(1..1), window, cx);
        editor
    });
    let mut handler = editor
        .update(cx, |_, window, cx| {
            gpui::PlatformInputHandler::new(
                window.to_async(cx),
                Box::new(gpui::ElementInputHandler::new(
                    gpui::Bounds::default(),
                    cx.entity(),
                )),
            )
        })
        .unwrap();
    wayland_text_input::ImeBatch {
        delete: Some((4, 3)),
        commit: Some("é".into()),
        preedit: Some(("α😀中".into(), 2, 6)),
    }
    .apply(&mut handler);
    editor
        .update(cx, |editor, window, cx| {
            assert_eq!(editor.text(cx), "aéα😀中Z");
            assert_eq!(editor.marked_text_range(window, cx), Some(2..6));
            assert_eq!(
                editor.selected_text_range(false, window, cx).unwrap().range,
                3..5
            );
        })
        .unwrap();
    wayland_text_input::ImeBatch {
        preedit: Some(("hidden".into(), -1, -1)),
        ..Default::default()
    }
    .apply(&mut handler);
    editor
        .update(cx, |editor, window, cx| {
            assert_eq!(editor.text(cx), "aéhiddenZ");
            assert!(!editor.show_local_cursors(window, cx));
        })
        .unwrap();
    wayland_text_input::ImeBatch {
        commit: Some("終".into()),
        ..Default::default()
    }
    .apply(&mut handler);
    editor
        .update(cx, |editor, window, cx| {
            assert_eq!(
                editor.text(cx),
                "aé終Z",
                "prior composition removed before commit exactly once"
            );
            assert_eq!(editor.marked_text_range(window, cx), None);
        })
        .unwrap();
}
