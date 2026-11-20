//! An editor over read-only history followed by a writable draft. The last
//! history row is the separator; the next row belongs to the draft buffer.

use editor::{Anchor, Bias, Editor};
use gpui::{App, AppContext as _, Context, Entity, Focusable, Pixels, Window};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey, ToPoint as _};

pub fn document(
    history: &Entity<Buffer>,
    draft: &Entity<Buffer>,
    cx: &mut App,
) -> Entity<MultiBuffer> {
    cx.new(|cx| {
        let mut doc = MultiBuffer::without_headers(Capability::ReadWrite);
        doc.set_excerpts_for_path(
            PathKey::sorted(0),
            history.clone(),
            [Point::zero()..history.read(cx).max_point()],
            0,
            cx,
        );
        set_excerpt(&mut doc, draft, cx);
        doc
    })
}

fn set_excerpt(doc: &mut MultiBuffer, draft: &Entity<Buffer>, cx: &mut Context<MultiBuffer>) {
    doc.set_excerpts_for_path(
        PathKey::sorted(1),
        draft.clone(),
        [Point::zero()..draft.read(cx).max_point()],
        0,
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
    doc.remove_excerpts(PathKey::sorted(1), cx);
    set_excerpt(doc, draft, cx);
}

pub fn start(history: &Entity<Buffer>, cx: &App) -> Point {
    Point::new(history.read(cx).max_point().row + 1, 0)
}

pub fn on_tail(editor: &Editor, history: &Entity<Buffer>, cx: &App) -> bool {
    let snapshot = editor.buffer().read(cx).snapshot(cx);
    editor
        .selections
        .newest_anchor()
        .head()
        .to_point(&snapshot)
        .row
        >= start(history, cx).row
}

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
        (buffer_id == history.remote_id()
            && history.offset_to_point(offset).row == history.max_point().row)
            .then_some(Hit::Separator)
    }
}

/// A touch goes straight into Insert at the tapped column. Place the target
/// inside the draft before invoking Vim so it doesn't jump in from history,
/// then place the exact caret without Normal's end-of-line clipping.
pub fn focus(
    editor: &Entity<Editor>,
    history: &Entity<Buffer>,
    position: Option<Anchor>,
    window: &mut Window,
    cx: &mut App,
) {
    let focus = editor.focus_handle(cx);
    window.focus(&focus, cx);
    let tail = start(history, cx);
    editor.update(cx, |editor, cx| {
        editor.change_selections(Default::default(), window, cx, |selections| {
            selections.select_ranges([tail..tail]);
        });
    });
    let insert = cx
        .build_action("vim::InsertBefore", None)
        .expect("vim is initialised before a draft opens");
    focus.dispatch_action(insert.as_ref(), window, cx);
    if let Some(anchor) = position {
        editor.update(cx, |editor, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_anchor_ranges([anchor..anchor]);
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let document = document(&history, &draft, cx);
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
                replace_excerpt(doc, &next_draft, cx);
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
            let doc = document(&history, &draft, cx);
            let start = start(&history, cx);
            let separator = Point::new(start.row - 1, 3);
            let snapshot = doc.read(cx).snapshot(cx);
            assert_eq!(snapshot.text(), "0 wait\n── idea\n");
            assert!(!Editor::point_is_editable(&snapshot, start));
            assert!(!Editor::point_is_editable(&snapshot, separator));

            let addressed = cx.new(|cx| Buffer::local("", cx));
            doc.update(cx, |doc, cx| {
                replace_excerpt(doc, &addressed, cx);
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
