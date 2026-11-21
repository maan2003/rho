//! One editor with a writable draft and read-only history in separate excerpts.

use editor::display_map::{DisplaySnapshot, ToDisplayPoint as _};
use editor::{Anchor, Bias, Editor, HighlightKey, Inlay, RowExt as _, StuckEdge};
use gpui::{App, AppContext as _, Context, Entity, Focusable, Pixels, Window};
use language::{Buffer, Capability, InlayId, Point};
use multi_buffer::{MultiBuffer, MultiBufferSnapshot, PathKey, ToPoint as _};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    Top,
    Bottom,
}

impl Placement {
    fn path(self) -> PathKey {
        PathKey::sorted(match self {
            Self::Top => 0,
            Self::Bottom => 1,
        })
    }

    fn history_path(self) -> PathKey {
        match self {
            Self::Top => Self::Bottom.path(),
            Self::Bottom => Self::Top.path(),
        }
    }

    pub fn document(
        self,
        history: &Entity<Buffer>,
        draft: &Entity<Buffer>,
        cx: &mut App,
    ) -> Entity<MultiBuffer> {
        cx.new(|cx| {
            let mut doc = MultiBuffer::without_headers(Capability::ReadWrite);
            Self::set_excerpt(&mut doc, self.history_path(), history, cx);
            Self::set_excerpt(&mut doc, self.path(), draft, cx);
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

    pub fn stick(self, editor: &mut Editor, notice: gpui::SharedString, cx: &mut Context<Editor>) {
        let edge = match self {
            Self::Top => StuckEdge::Top,
            Self::Bottom => StuckEdge::Bottom,
        };
        editor.set_stuck_excerpt(
            self.path(),
            edge,
            notice,
            rho_window::style::command_strip_background,
            cx,
        );
    }

    /// A sent draft isn't the next draft's undo history.
    pub fn replace(self, editor: &Entity<Editor>, draft: &Entity<Buffer>, cx: &mut App) {
        let doc = editor.read(cx).buffer().clone();
        doc.update(cx, |doc, cx| self.replace_excerpt(doc, draft, cx));
        editor.update(cx, |editor, cx| {
            editor.disable_header_for_buffer(draft.read(cx).remote_id(), cx);
        });
    }

    fn replace_excerpt(
        self,
        doc: &mut MultiBuffer,
        draft: &Entity<Buffer>,
        cx: &mut Context<MultiBuffer>,
    ) {
        doc.remove_excerpts(self.path(), cx);
        Self::set_excerpt(doc, self.path(), draft, cx);
    }

    pub fn range(self, snapshot: &MultiBufferSnapshot) -> std::ops::Range<Anchor> {
        snapshot
            .single_excerpt_range(&self.path())
            .expect("document has one draft excerpt")
    }

    pub fn history_start(self, snapshot: &MultiBufferSnapshot) -> Point {
        snapshot
            .single_excerpt_range(&self.history_path())
            .expect("document has one history excerpt")
            .start
            .to_point(snapshot)
    }
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
        HighlightKey::SyntaxTreeView(rho_window::style::PROMPT_DRAFT_HIGHLIGHT_KEY),
        if empty {
            Vec::new()
        } else {
            vec![range.clone()]
        },
        rho_window::style::StyleClass::UserMessage.resolve(cx),
        cx,
    );
    editor.highlight_gutter::<DraftGutter>(
        vec![range],
        rho_window::style::user_prompt_gutter_color,
        cx,
    );
}

/// A visible Insert caret before the history above it changes.
pub struct WritingCaret {
    anchor: Anchor,
    screen_y: f64,
}

impl WritingCaret {
    pub fn capture(editor: &Entity<Editor>, draft: &Entity<Buffer>, cx: &mut App) -> Option<Self> {
        if !vim::editor_in_insert_mode(editor, cx) {
            return None;
        }
        editor.update(cx, |editor, cx| {
            let anchor = editor.selections.newest_anchor().head();
            let scroll = editor.scroll_position(cx);
            let display = editor.display_snapshot(cx);
            let (_, buffer) = display.buffer_snapshot().anchor_to_buffer_anchor(anchor)?;
            if buffer.remote_id() != draft.read(cx).remote_id() {
                return None;
            }
            let screen_y = Self::row_y(anchor, &display) - display.row_y(scroll.y);
            (0. <= screen_y && screen_y < editor.visible_line_count()?)
                .then_some(Self { anchor, screen_y })
        })
    }

    /// Re-anchor the viewport after the mutation. Its normal top-of-screen
    /// anchor would keep the history in place instead of the draft.
    pub fn restore(self, editor: &Entity<Editor>, cx: &mut App) {
        let editor = editor.clone();
        // Reconciliation may run outside the window update to read the
        // frame. Only the scroll adjustment requires the window.
        cx.defer(move |cx| {
            cx.with_window(editor.entity_id(), |window, cx| {
                if !editor.read(cx).is_focused(window) || !vim::editor_in_insert_mode(&editor, cx) {
                    return;
                }
                editor.update(cx, |editor, cx| {
                    if editor.selections.newest_anchor().head() != self.anchor {
                        return;
                    }
                    let mut position = editor.scroll_position(cx);
                    let display = editor.display_snapshot(cx);
                    let top = (Self::row_y(self.anchor, &display) - self.screen_y).max(0.);
                    if (display.row_y(position.y) - top).abs() > 0.01 {
                        position.y = display.row_at_y(top);
                        editor.set_scroll_position(position, window, cx);
                    }
                });
            });
        });
    }

    fn row_y(anchor: Anchor, display: &DisplaySnapshot) -> f64 {
        display.row_y(anchor.to_display_point(display).row().as_f64())
    }
}

/// Where a pointer landed in the editable tail.
pub enum Hit {
    Separator,
    Text(Anchor),
}

pub fn hit(
    placement: Placement,
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
        let separator = match placement {
            Placement::Top => 0,
            Placement::Bottom => history.max_point().row,
        };
        (buffer_id == history.remote_id() && history.offset_to_point(offset).row == separator)
            .then_some(Hit::Separator)
    }
}

/// A touch goes straight into Insert at the tapped column. Place the target
/// inside the draft before invoking Vim so it doesn't jump in from history,
/// then place the exact caret without Normal's end-of-line clipping.
pub fn focus(
    placement: Placement,
    editor: &Entity<Editor>,
    position: Option<Anchor>,
    window: &mut Window,
    cx: &mut App,
) {
    let snapshot = editor.read(cx).buffer().read(cx).snapshot(cx);
    let start = placement.range(&snapshot).start;
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
    use super::*;

    fn start(history: &Entity<Buffer>, cx: &App) -> Point {
        Point::new(history.read(cx).max_point().row + 1, 0)
    }

    #[gpui::test]
    fn a_short_tail_clamps_and_the_stuck_draft_resolves_to_its_real_buffer(
        cx: &mut gpui::TestAppContext,
    ) {
        use editor::scroll::{Autoscroll, ScrollAmount};
        use editor::{EditorMode, SizingBehavior};
        use gpui::{point, px, size};

        let (history, draft, doc) = cx.update(|cx| {
            assets::Assets.load_test_fonts(cx);
            let settings = settings::SettingsStore::test(cx);
            cx.set_global(settings);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);

            let long_history = (0..84)
                .map(|i| format!("mail line {i}\n"))
                .collect::<String>();
            let history = cx.new(|cx| {
                let mut buffer = Buffer::local(&long_history, cx);
                buffer.set_capability(Capability::Read, cx);
                buffer
            });
            let draft = cx.new(|cx| Buffer::local("one line\nlonger second row", cx));
            let doc = Placement::Bottom.document(&history, &draft, cx);
            (history, draft, doc)
        });
        let window = cx.add_window(|window, cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                doc.clone(),
                None,
                window,
                cx,
            );
            Placement::Bottom.stick(&mut editor, "↓ new mail".into(), cx);
            editor
        });
        cx.simulate_window_resize(window.into(), size(px(600.), px(338.)));
        cx.draw_window(window.into());
        let draft_start = cx.update(|cx| start(&history, cx));
        let open_target = Point::new(draft_start.row - 4, 0);
        window
            .update(cx, |editor, window, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let caret = snapshot.anchor_after(draft_start);
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_anchor_ranges([caret..caret]);
                });
                editor.request_autoscroll(
                    Autoscroll::top().for_anchor(snapshot.anchor_after(open_target)),
                    cx,
                );
            })
            .unwrap();
        cx.draw_window(window.into());
        let original_top = window.update(cx, |editor, window, cx| {
            let viewport = editor.visible_line_count().unwrap();
            assert!(viewport > 8.0, "the viewport is taller than the short tail");
            let display = editor.snapshot(window, cx);
            let bottom = display.row_y(display.max_point().row().next_row().as_f64());
            let top = display.row_y(editor.scroll_position(cx).y);
            assert!((top + viewport - bottom).abs() < 0.1,
                "the viewport must end at the draft, not the requested top: {top} + {viewport} != {bottom}");
            assert!(!editor.stuck_excerpt_is_visible());
            top
        }).unwrap();

        window
            .update(cx, |editor, window, cx| {
                editor.scroll_screen_with_cursor_margin(&ScrollAmount::Page(-0.5), window, cx);
            })
            .unwrap();
        cx.draw_window(window.into());

        window
            .update(cx, |editor, window, cx| {
                let display = editor.snapshot(window, cx);
                assert!(
                    editor.stuck_excerpt_is_visible(),
                    "half-page up sticks the draft"
                );
                let top = display.row_y(editor.scroll_position(cx).y);
                assert!(
                    (original_top - top - (editor.visible_line_count().unwrap() / 2.0).trunc())
                        .abs()
                        < 0.2
                );
                assert_eq!(
                    editor
                        .selections
                        .newest_anchor()
                        .head()
                        .to_point(display.buffer_snapshot()),
                    draft_start
                );

                let draft_end = display.max_point();
                let xy = editor
                    .window_position_for_display_point(draft_end, &display, window, cx)
                    .unwrap();
                let (_, buffer, offset) = editor
                    .buffer_location_for_window_position(xy + point(px(2.), px(3.)), Bias::Left)
                    .expect("the final projected row is visible and hit-testable");
                assert_eq!(buffer, draft.read(cx).remote_id());
                assert_eq!(offset, "one line\nlonger second row".len());
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
                assert!(!editor.stuck_excerpt_is_visible());
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                assert_eq!(
                    editor.selections.newest_anchor().head().to_point(&snapshot),
                    draft_start
                );
            })
            .unwrap();

        // A draft longer than the entire viewport projects less than half
        // of it, tracks the caret, and has its own pointer-scroll offset.
        let long = (0..36)
            .map(|i| format!("draft line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tall = cx.update(|cx| cx.new(|cx| Buffer::local(&long, cx)));
        let editor = window.root(cx).unwrap();
        cx.update(|cx| Placement::Bottom.replace(&editor, &tall, cx));
        window
            .update(cx, |editor, window, cx| {
                let caret = Anchor::Max;
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_anchor_ranges([caret..caret]);
                });
                editor.set_scroll_position(point(0., 5.), window, cx);
            })
            .unwrap();
        cx.draw_window(window.into());
        let (over_draft, history_top) = window
            .update(cx, |editor, window, cx| {
                let display = editor.snapshot(window, cx);
                assert!(editor.stuck_excerpt_is_visible());
                let end = editor
                    .window_position_for_display_point(display.max_point(), &display, window, cx)
                    .unwrap();
                let position = point(end.x + px(1.), end.y + px(4.));
                let (_, buffer, offset) = editor
                    .buffer_location_for_window_position(position, Bias::Left)
                    .unwrap();
                assert_eq!(
                    (buffer, offset),
                    (tall.read(cx).remote_id(), long.len()),
                    "the capped projection follows the caret"
                );
                let half_height = window.viewport_size().height / 2.0;
                let (_, buffer_above_half, _) = editor
                    .buffer_location_for_window_position(
                        point(position.x, half_height - px(2.)),
                        Bias::Left,
                    )
                    .unwrap();
                assert_eq!(
                    buffer_above_half,
                    history.read(cx).remote_id(),
                    "the draft cannot cover more than half the viewport"
                );
                (position, editor.scroll_position(cx))
            })
            .unwrap();

        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        visual.simulate_event(gpui::ScrollEvent {
            position: over_draft,
            delta: gpui::ScrollDelta::Lines(point(0., 4.)),
            modifiers: Default::default(),
            touch_phase: gpui::TouchPhase::Moved,
            source: gpui::ScrollSource::Pointer,
        });
        cx.draw_window(window.into());
        window
            .update(cx, |editor, _, cx| {
                let (_, buffer, offset) = editor
                    .buffer_location_for_window_position(over_draft, Bias::Left)
                    .unwrap();
                assert_eq!(buffer, tall.read(cx).remote_id());
                assert!(
                    offset < long.len(),
                    "scrolling the panel reveals earlier draft lines"
                );
                assert_eq!(
                    editor.scroll_position(cx),
                    history_top,
                    "scrolling the panel leaves history fixed"
                );
            })
            .unwrap();

        // Even when there is no older history to fill the screen, the
        // last row ends at the viewport edge, not a few rows below its top.
        cx.update(|cx| {
            history.update(cx, |buffer, cx| {
                buffer.edit(
                    [(Point::zero()..buffer.max_point(), "just one mail\n")],
                    None,
                    cx,
                );
            });
            tall.update(cx, |buffer, cx| {
                buffer.edit(
                    [(Point::zero()..buffer.max_point(), "short answer")],
                    None,
                    cx,
                );
            });
        });
        cx.draw_window(window.into());
        window
            .update(cx, |editor, window, cx| {
                let display = editor.snapshot(window, cx);
                let xy = editor
                    .window_position_for_display_point(display.max_point(), &display, window, cx)
                    .unwrap();
                let height = f64::from(window.viewport_size().height);
                let line = height / editor.visible_line_count().unwrap();
                assert!(
                    (height - f64::from(xy.y) - line).abs() < 1.0,
                    "the short document is bottom-aligned"
                );
                assert!(!editor.stuck_excerpt_is_visible());
                assert_eq!(editor.scroll_position(cx).y, 0.0);
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
            let doc = Placement::Top.document(&history, &draft, cx);
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
            Placement::Top.stick(&mut editor, "↑ new mail".into(), cx);
            editor
        });
        cx.simulate_window_resize(window.into(), size(px(600.), px(338.)));
        cx.draw_window(window.into());
        let caret = cx.update(|cx| Placement::Top.range(&document.read(cx).snapshot(cx)).end);
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
                let mut history = Buffer::local("mail to keep\n── reply", cx);
                history.set_capability(Capability::Read, cx);
                history
            });
            let draft = cx.new(|cx| Buffer::local("some draft", cx));
            let document = Placement::Bottom.document(&history, &draft, cx);
            let count = blocked.clone();
            let subscription = cx.subscribe(&document, move |_, event: &multi_buffer::Event, _| {
                if matches!(event, multi_buffer::Event::EditBlockedByReadOnly) {
                    count.set(count.get() + 1);
                }
            });
            (document, history, draft, subscription)
        });
        let mail = Point::new(0, 3);
        let draft_point = Point::new(2, 5);
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
                doc.edit([(Point::new(1, 2)..Point::new(2, 5), "")], None, cx);
            });
        });
        assert_eq!(
            blocked.get(),
            1,
            "the cross-separator delete was partially blocked"
        );
        cx.update(|cx| {
            assert_eq!(history.read(cx).text(), "mail to keep\n── reply");
            assert_eq!(draft.read(cx).text(), "real draft");
        });
    }

    #[gpui::test]
    fn editing_changes_only_the_draft_and_history_updates_keep_its_position(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let history = cx.new(|cx| {
                let mut history = Buffer::local("old mail\n── reply", cx);
                history.set_capability(Capability::Read, cx);
                history
            });
            let draft = cx.new(|cx| Buffer::local("one\ntwo words\nthree", cx));
            let document = Placement::Bottom.document(&history, &draft, cx);
            assert_eq!(
                document.read(cx).snapshot(cx).text(),
                "old mail\n── reply\none\ntwo words\nthree"
            );
            let caret = document
                .read(cx)
                .snapshot(cx)
                .anchor_after(Point::new(3, 4));
            document.update(cx, |document, cx| {
                // Both edits go through the editor's edit boundary, not
                // directly to the component buffers.
                document.start_transaction(cx);
                document.edit(
                    [
                        (Point::new(0, 0)..Point::new(0, 3), "bad"),
                        (Point::new(3, 4)..Point::new(3, 9), "asymmetric"),
                    ],
                    None,
                    cx,
                );
                document.end_transaction(cx);
            });
            assert_eq!(history.read(cx).text(), "old mail\n── reply");
            assert_eq!(draft.read(cx).text(), "one\ntwo asymmetric\nthree");

            history.update(cx, |history, cx| {
                history.edit(
                    [(Point::zero()..Point::zero(), "new mail\nextra line\n")],
                    None,
                    cx,
                );
            });
            assert_eq!(caret.to_point(&document.read(cx).snapshot(cx)).row, 5);
            assert_eq!(
                document.read(cx).snapshot(cx).text(),
                "new mail\nextra line\nold mail\n── reply\none\ntwo asymmetric\nthree"
            );
            document.update(cx, |doc, cx| {
                assert!(doc.undo(cx).is_some());
            });
            assert_eq!(
                document.read(cx).snapshot(cx).text(),
                "new mail\nextra line\nold mail\n── reply\none\ntwo words\nthree"
            );

            let next_draft = cx.new(|cx| Buffer::local("", cx));
            document.update(cx, |doc, cx| {
                Placement::Bottom.replace_excerpt(doc, &next_draft, cx);
                assert!(doc.redo(cx).is_none(), "the sent draft is discarded");
            });
            let snapshot = document.read(cx).snapshot(cx);
            let tail = start(&history, cx);
            assert_eq!(
                snapshot.text(),
                "new mail\nextra line\nold mail\n── reply\n"
            );
            assert_eq!(tail, snapshot.max_point());
            assert!(!Editor::point_is_editable(&snapshot, Point::new(3, 0)));
            assert!(Editor::point_is_editable(&snapshot, tail));
        });
    }
    #[gpui::test]
    fn the_empty_tail_can_become_writable_without_moving_the_readonly_boundary(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            let history = cx.new(|cx| {
                let mut buffer = Buffer::local("0 wait\n── idea", cx);
                buffer.set_capability(Capability::Read, cx);
                buffer
            });
            let draft = cx.new(|cx| {
                let mut buffer = Buffer::local("", cx);
                buffer.set_capability(Capability::Read, cx);
                buffer
            });
            let doc = Placement::Bottom.document(&history, &draft, cx);
            let start = start(&history, cx);
            let separator = Point::new(start.row - 1, 3);
            let snapshot = doc.read(cx).snapshot(cx);
            assert_eq!(snapshot.text(), "0 wait\n── idea\n");
            assert!(!Editor::point_is_editable(&snapshot, start));
            assert!(!Editor::point_is_editable(&snapshot, separator));

            let addressed = cx.new(|cx| Buffer::local("", cx));
            doc.update(cx, |doc, cx| {
                Placement::Bottom.replace_excerpt(doc, &addressed, cx);
                let snapshot = doc.snapshot(cx);
                assert!(Editor::point_is_editable(&snapshot, start));
                assert!(!Editor::point_is_editable(&snapshot, separator));
                doc.edit([(start..start, "An idea")], None, cx);
            });
            assert_eq!(doc.read(cx).snapshot(cx).text(), "0 wait\n── idea\nAn idea");
            assert_eq!(draft.read(cx).text(), "");
            assert_eq!(addressed.read(cx).text(), "An idea");
        });
    }
}
