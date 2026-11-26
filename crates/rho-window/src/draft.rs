//! One editor with a writable draft above read-only history, each in an
//! excerpt of its own (`window.Edge-stuck excerpt`): the draft stays at the
//! top edge while the reader is in the history below it.

use editor::{Anchor, Bias, Editor, HighlightKey, Inlay, StuckEdge};
use gpui::{App, AppContext as _, Context, Entity, Focusable, Pixels, Window};
use language::{Buffer, Capability, InlayId, Point};
use multi_buffer::{MultiBuffer, MultiBufferSnapshot, PathKey, ToPoint as _};

/// The draft leads the document and the history follows it.
fn draft_path() -> PathKey {
    PathKey::sorted(0)
}

fn history_path() -> PathKey {
    PathKey::sorted(1)
}

pub fn document(
    history: &Entity<Buffer>,
    draft: &Entity<Buffer>,
    cx: &mut App,
) -> Entity<MultiBuffer> {
    cx.new(|cx| {
        let mut doc = MultiBuffer::without_headers(Capability::ReadWrite);
        set_excerpt(&mut doc, history_path(), history, cx);
        set_excerpt(&mut doc, draft_path(), draft, cx);
        doc
    })
}

fn set_excerpt(
    doc: &mut MultiBuffer,
    path: PathKey,
    buffer: &Entity<Buffer>,
    cx: &mut Context<MultiBuffer>,
) {
    doc.set_excerpts_for_path(
        path,
        buffer.clone(),
        [Point::zero()..buffer.read(cx).max_point()],
        0,
        cx,
    );
}

/// While the reader is in the history below, the draft stays at the top
/// edge with `notice` at its end.
pub fn stick(editor: &mut Editor, notice: gpui::SharedString, cx: &mut Context<Editor>) {
    editor.set_stuck_excerpt(
        draft_path(),
        StuckEdge::Top,
        notice,
        crate::style::command_strip_background,
        cx,
    );
}

/// A sent draft isn't the next draft's undo history.
pub fn replace(editor: &Entity<Editor>, draft: &Entity<Buffer>, cx: &mut App) {
    let doc = editor.read(cx).buffer().clone();
    doc.update(cx, |doc, cx| replace_excerpt(doc, draft, cx));
    editor.update(cx, |editor, cx| {
        editor.disable_header_for_buffer(draft.read(cx).remote_id(), cx);
    });
}

fn replace_excerpt(doc: &mut MultiBuffer, draft: &Entity<Buffer>, cx: &mut Context<MultiBuffer>) {
    doc.remove_excerpts(draft_path(), cx);
    set_excerpt(doc, draft_path(), draft, cx);
}

pub fn range(snapshot: &MultiBufferSnapshot) -> std::ops::Range<Anchor> {
    snapshot
        .single_excerpt_range(&draft_path())
        .expect("document has one draft excerpt")
}

pub fn history_start(snapshot: &MultiBufferSnapshot) -> Point {
    snapshot
        .single_excerpt_range(&history_path())
        .expect("document has one history excerpt")
        .start
        .to_point(snapshot)
}

struct DraftGutter;
const PLACEHOLDER: usize = usize::MAX;

pub fn style(
    editor: &mut Editor,
    range: std::ops::Range<Anchor>,
    placeholder: &'static str,
    cx: &mut Context<Editor>,
) {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    let empty = range.start.to_point(&snapshot) == range.end.to_point(&snapshot);
    let inlay = empty.then(|| Inlay::custom(PLACEHOLDER, range.end, placeholder));
    editor.splice_inlays(
        &[InlayId::Custom(PLACEHOLDER)],
        inlay.into_iter().collect(),
        cx,
    );
    editor.highlight_text(
        HighlightKey::SyntaxTreeView(crate::style::PROMPT_DRAFT_HIGHLIGHT_KEY),
        if empty {
            Vec::new()
        } else {
            vec![range.clone()]
        },
        crate::style::StyleClass::UserMessage.resolve(cx),
        cx,
    );
    editor.highlight_gutter::<DraftGutter>(vec![range], crate::style::user_prompt_gutter_color, cx);
}

/// Where a pointer landed: in the draft, or on the blank line that
/// separates it from the history.
pub enum Hit {
    Separator,
    Text(Anchor),
}

pub fn hit(
    editor: &Editor,
    history: &Entity<Buffer>,
    draft: &Entity<Buffer>,
    position: gpui::Point<Pixels>,
    cx: &App,
) -> Option<Hit> {
    let (anchor, buffer_id, offset) =
        editor.buffer_location_for_window_position(position, Bias::Left)?;
    if buffer_id == draft.read(cx).remote_id() {
        Some(Hit::Text(anchor))
    } else {
        let history = history.read(cx);
        (buffer_id == history.remote_id() && history.offset_to_point(offset).row == 0)
            .then_some(Hit::Separator)
    }
}

/// A touch goes straight into Insert at the tapped column. Place the target
/// inside the draft before invoking Vim so it doesn't jump in from history,
/// then place the exact caret without Normal's end-of-line clipping.
pub fn focus(editor: &Entity<Editor>, position: Option<Anchor>, window: &mut Window, cx: &mut App) {
    let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
    let start = range(&snapshot).start;
    focus_writing_at(editor, start, position.unwrap_or(start), window, cx);
}

/// Starts Insert at an exact anchor, whether it is in a note or in a tail
/// below read-only text. `start` gives Vim an editable point for entering
/// Insert; the final anchor can be at the very end of a line.
pub fn focus_writing_at(
    editor: &Entity<Editor>,
    start: Anchor,
    at: Anchor,
    window: &mut Window,
    cx: &mut App,
) {
    window.focus(&editor.focus_handle(cx), cx);
    let place = |anchor, window: &mut Window, cx: &mut App| {
        editor.update(cx, |editor, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_anchor_ranges([anchor..anchor]);
            });
        });
    };
    place(start, window, cx);
    vim::enter_insert_mode(editor, window, cx);
    place(at, window, cx);
    editor.update(cx, |editor, cx| {
        editor.request_autoscroll(editor::scroll::Autoscroll::fit(), cx);
    });
}

#[cfg(test)]
mod tests {
    use editor::display_map::ToDisplayPoint as _;

    use super::*;

    #[gpui::test]
    fn the_short_top_draft_does_not_shift_when_stuck_or_notified(cx: &mut gpui::TestAppContext) {
        use editor::{EditorMode, SizingBehavior};
        use gpui::{point, px, size};

        let (document, caret) = cx.update(|cx| {
            assets::Assets.load_test_fonts(cx);
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);

            let draft = cx.new(|cx| Buffer::local("reference clock\nsecond line", cx));
            let history = cx.new(|cx| Buffer::local(&"history\n".repeat(70), cx));
            let document = document(&history, &draft, cx);
            let caret = document
                .read(cx)
                .snapshot(cx)
                .anchor_after(Point::new(1, 5));
            (document, caret)
        });
        let window = cx.add_window(|window, cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                document,
                None,
                window,
                cx,
            );
            stick(&mut editor, "↑ new mail".into(), cx);
            editor
        });
        cx.simulate_window_resize(window.into(), size(px(610.), px(342.)));
        cx.draw_window(window.into());
        let draft_xy =
            |editor: &mut Editor, window: &mut gpui::Window, cx: &mut Context<Editor>| {
                let display = editor.snapshot(window, cx);
                editor
                    .window_position_for_display_point(
                        caret.to_display_point(&display),
                        &display,
                        window,
                        cx,
                    )
                    .unwrap()
            };
        let inline = window.update(cx, draft_xy).unwrap();
        window
            .update(cx, |editor, window, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_anchor_ranges([caret..caret]);
                });
                editor.set_scroll_position(point(0., 12.25), window, cx);
            })
            .unwrap();
        cx.draw_window(window.into());
        let history_xy = |editor: &Editor| {
            editor
                .buffer_location_for_window_position(
                    point(inline.x, inline.y + px(65.)),
                    Bias::Left,
                )
                .unwrap()
        };
        let before_notice = window
            .update(cx, |editor, window, cx| {
                assert!(
                    editor.stuck_excerpt_is_visible(),
                    "scroll {:?}, visible lines {:?}, max point {:?}",
                    editor.scroll_position(cx),
                    editor.visible_line_count(),
                    editor.snapshot(window, cx).max_point()
                );
                assert_eq!(draft_xy(editor, window, cx), inline);
                let history = history_xy(editor);
                editor.mark_stuck_excerpt(cx);
                history
            })
            .unwrap();
        cx.draw_window(window.into());
        window
            .update(cx, |editor, window, cx| {
                assert_eq!(draft_xy(editor, window, cx), inline);
                assert_eq!(history_xy(editor), before_notice);
            })
            .unwrap();
    }

    #[gpui::test]
    fn the_top_projection_keeps_a_tall_drafts_real_caret_and_leaves_room_for_history(
        cx: &mut gpui::TestAppContext,
    ) {
        use editor::scroll::ScrollAmount;
        use editor::{EditorMode, SizingBehavior};
        use gpui::{point, px, size};

        let (draft, history, document, last_offset) = cx.update(|cx| {
            assets::Assets.load_test_fonts(cx);
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);

            let text = (0..27)
                .map(|i| format!("draft line {i}\n"))
                .collect::<String>();
            let draft = cx.new(|cx| Buffer::local(&text, cx));
            let history = cx.new(|cx| {
                let mut buffer = Buffer::local(&"history under the draft\n".repeat(48), cx);
                buffer.set_capability(Capability::Read, cx);
                buffer
            });
            let doc = document(&history, &draft, cx);
            (draft, history, doc, text.len())
        });
        let window = cx.add_window(|window, cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                document.clone(),
                None,
                window,
                cx,
            );
            stick(&mut editor, "↑ new mail".into(), cx);
            editor
        });
        cx.simulate_window_resize(window.into(), size(px(600.), px(338.)));
        cx.draw_window(window.into());
        let caret = cx.update(|cx| range(&document.read(cx).snapshot(cx)).end);
        window
            .update(cx, |editor, window, cx| {
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_anchor_ranges([caret..caret]);
                });
                editor.set_scroll_position(point(0., 35.), window, cx);
            })
            .unwrap();
        cx.draw_window(window.into());
        let before = window
            .update(cx, |editor, window, cx| {
                let display = editor.snapshot(window, cx);
                assert!(editor.stuck_excerpt_is_visible());
                let at = editor
                    .window_position_for_display_point(
                        caret.to_display_point(&display),
                        &display,
                        window,
                        cx,
                    )
                    .unwrap();
                let (_, buffer, offset) = editor
                    .buffer_location_for_window_position(
                        point(at.x + px(1.), at.y + px(4.)),
                        Bias::Left,
                    )
                    .unwrap();
                assert_eq!((buffer, offset), (draft.read(cx).remote_id(), last_offset));
                let below = point(at.x + px(3.), window.viewport_size().height / 2. + px(2.));
                let (_, buffer, _) = editor
                    .buffer_location_for_window_position(below, Bias::Left)
                    .unwrap();
                assert_eq!(buffer, history.read(cx).remote_id());
                editor.scroll_position(cx).y
            })
            .unwrap();
        window
            .update(cx, |editor, window, cx| {
                editor.scroll_screen_with_cursor_margin(&ScrollAmount::Page(0.5), window, cx);
            })
            .unwrap();
        cx.draw_window(window.into());
        window
            .update(cx, |editor, _, cx| {
                assert!(editor.scroll_position(cx).y > before);
                assert_eq!(
                    editor
                        .selections
                        .newest_anchor()
                        .head()
                        .to_point(&document.read(cx).snapshot(cx)),
                    caret.to_point(&document.read(cx).snapshot(cx))
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn only_an_attempt_to_change_mail_emits_the_read_only_event(cx: &mut gpui::TestAppContext) {
        let blocked = std::rc::Rc::new(std::cell::Cell::new(0));
        let (document, history, draft, _subscription) = cx.update(|cx| {
            let history = cx.new(|cx| {
                let mut history = Buffer::local("mail to keep\nolder mail", cx);
                history.set_capability(Capability::Read, cx);
                history
            });
            let draft = cx.new(|cx| Buffer::local("some draft", cx));
            let document = document(&history, &draft, cx);
            let count = blocked.clone();
            let subscription = cx.subscribe(&document, move |_, event: &multi_buffer::Event, _| {
                if matches!(event, multi_buffer::Event::EditBlockedByReadOnly) {
                    count.set(count.get() + 1);
                }
            });
            (document, history, draft, subscription)
        });
        let mail = Point::new(1, 3);
        let draft_point = Point::new(0, 5);
        cx.update(|cx| {
            document.update(cx, |doc, cx| {
                doc.edit(
                    [(mail..mail, ""), (draft_point..draft_point, "real ")],
                    None,
                    cx,
                );
            });
        });
        assert_eq!(blocked.get(), 0, "a no-op in the mail did not count");
        cx.update(|cx| {
            document.update(cx, |doc, cx| {
                // From inside the draft across into the newest mail.
                doc.edit([(Point::new(0, 10)..Point::new(1, 3), "")], None, cx);
            });
        });
        assert_eq!(
            blocked.get(),
            1,
            "the cross-separator delete was partially blocked"
        );
        cx.update(|cx| {
            assert_eq!(history.read(cx).text(), "mail to keep\nolder mail");
            assert_eq!(draft.read(cx).text(), "some real ");
        });
    }

    #[gpui::test]
    fn editing_changes_only_the_draft_and_history_updates_keep_its_position(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let history = cx.new(|cx| {
                let mut history = Buffer::local("old mail\nolder mail", cx);
                history.set_capability(Capability::Read, cx);
                history
            });
            let draft = cx.new(|cx| Buffer::local("one\ntwo words\nthree", cx));
            let document = document(&history, &draft, cx);
            assert_eq!(
                document.read(cx).snapshot(cx).text(),
                "one\ntwo words\nthree\nold mail\nolder mail"
            );
            let caret = document
                .read(cx)
                .snapshot(cx)
                .anchor_after(Point::new(2, 2));
            document.update(cx, |document, cx| {
                // Both edits go through the editor's edit boundary, not
                // directly to the component buffers.
                document.start_transaction(cx);
                document.edit(
                    [
                        (Point::new(1, 4)..Point::new(1, 9), "asymmetric"),
                        (Point::new(3, 0)..Point::new(3, 3), "bad"),
                    ],
                    None,
                    cx,
                );
                document.end_transaction(cx);
            });
            assert_eq!(history.read(cx).text(), "old mail\nolder mail");
            assert_eq!(draft.read(cx).text(), "one\ntwo asymmetric\nthree");

            // New mail lands at the top of the history, below the draft.
            history.update(cx, |history, cx| {
                history.edit(
                    [(Point::zero()..Point::zero(), "new mail\nextra line\n")],
                    None,
                    cx,
                );
            });
            assert_eq!(
                caret.to_point(&document.read(cx).snapshot(cx)),
                Point::new(2, 2),
                "mail arriving below the draft does not move its caret"
            );
            assert_eq!(
                document.read(cx).snapshot(cx).text(),
                "one\ntwo asymmetric\nthree\nnew mail\nextra line\nold mail\nolder mail"
            );
            document.update(cx, |doc, cx| {
                assert!(doc.undo(cx).is_some());
            });
            assert_eq!(
                document.read(cx).snapshot(cx).text(),
                "one\ntwo words\nthree\nnew mail\nextra line\nold mail\nolder mail"
            );

            let next_draft = cx.new(|cx| Buffer::local("", cx));
            document.update(cx, |doc, cx| {
                replace_excerpt(doc, &next_draft, cx);
                assert!(doc.redo(cx).is_none(), "the sent draft is discarded");
            });
            let snapshot = document.read(cx).snapshot(cx);
            assert_eq!(
                snapshot.text(),
                "\nnew mail\nextra line\nold mail\nolder mail"
            );
            assert_eq!(history_start(&snapshot), Point::new(1, 0));
            assert!(!Editor::point_is_editable(&snapshot, Point::new(1, 0)));
            assert!(Editor::point_is_editable(&snapshot, Point::zero()));
        });
    }

    #[gpui::test]
    fn an_empty_draft_can_become_writable_without_moving_the_readonly_boundary(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let history = cx.new(|cx| {
                let mut buffer = Buffer::local("\nthe newest mail", cx);
                buffer.set_capability(Capability::Read, cx);
                buffer
            });
            let draft = cx.new(|cx| {
                let mut buffer = Buffer::local("", cx);
                buffer.set_capability(Capability::Read, cx);
                buffer
            });
            let doc = document(&history, &draft, cx);
            let separator = Point::new(1, 0);
            let snapshot = doc.read(cx).snapshot(cx);
            assert_eq!(snapshot.text(), "\n\nthe newest mail");
            assert!(!Editor::point_is_editable(&snapshot, Point::zero()));
            assert!(!Editor::point_is_editable(&snapshot, separator));

            let writable = cx.new(|cx| Buffer::local("", cx));
            doc.update(cx, |doc, cx| {
                replace_excerpt(doc, &writable, cx);
                let snapshot = doc.snapshot(cx);
                assert!(Editor::point_is_editable(&snapshot, Point::zero()));
                assert!(!Editor::point_is_editable(&snapshot, separator));
                doc.edit([(Point::zero()..Point::zero(), "A reply")], None, cx);
            });
            assert_eq!(
                doc.read(cx).snapshot(cx).text(),
                "A reply\n\nthe newest mail"
            );
            assert_eq!(draft.read(cx).text(), "");
            assert_eq!(writable.read(cx).text(), "A reply");
        });
    }
}
