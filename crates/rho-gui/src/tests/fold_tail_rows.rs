//! A fold with a visible tail, and the width of the rows around it.
//!
//! `ElisionPolicy::Tail` is ours, not zed's: it splits one fold into a
//! placeholder over the head and a visible tail after it, so the reader
//! keeps the last few rows of what was elided. The split is by rows, and
//! the rows move as the text does.

use gpui::{TestAppContext, px, size};

/// No row of a tail-policy fold is asked for a width its own document
/// cannot answer.
///
/// `FoldSnapshot::line_len` resolves a row's width by subtracting the
/// row's start from the start of the row after it, on unsigned offsets.
/// If a tail-policy fold ever puts those two the wrong way round, the
/// width is not a small mistake: it wraps, and the row is reported about
/// four billion columns wide to everything above it.
///
/// The width is asked for directly rather than through the wrap map's
/// per-row check: that check asks whether a column falls within its row's
/// width, and a width of four billion answers yes to every column, so it
/// is blind to exactly this. A row cannot be wider than the document it
/// is a row of, and the document's length is the bound used here.
#[gpui::test]
fn a_tail_fold_never_reports_a_row_wider_than_its_document(cx: &mut TestAppContext) {
    struct TailFoldInThisTest;

    cx.update(crate::tests::init_test_app);
    let long_line = "wrap me ".repeat(40);
    let body = (0..12)
        .map(|row| format!("{row} {long_line}\n"))
        .collect::<String>();
    let editor = cx.add_window(|window, cx| {
        let mut editor = editor::Editor::multi_line(window, cx);
        editor.set_text(body.clone(), window, cx);
        editor
    });
    editor
        .update(cx, |editor, window, cx| {
            editor.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
            window.refresh();
        })
        .expect("soft wrap at the editor's width");
    cx.simulate_window_resize(*editor, size(px(500.), px(800.)));
    cx.run_until_parked();

    let fold_start = body.find("6 ").expect("row six");
    let fold_end = body.find("9 ").expect("row nine");
    editor
        .update(cx, |editor, _, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let range = snapshot.anchor_before(editor::MultiBufferOffset(fold_start))
                ..snapshot.anchor_after(editor::MultiBufferOffset(fold_end));
            editor.display_map.update(cx, |map, cx| {
                map.fold(
                    vec![
                        editor::display_map::Crease::simple(
                            range,
                            editor::FoldPlaceholder::concealed(std::any::TypeId::of::<
                                TailFoldInThisTest,
                            >()),
                        )
                        .with_elision_policy(editor::display_map::ElisionPolicy::Tail { rows: 3 }),
                    ],
                    cx,
                );
            });
            editor.display_snapshot(cx);
        })
        .expect("fold the middle with a visible tail");
    cx.run_until_parked();

    // The text under the fold shrinks, so the rows the tail is counted
    // from move while the fold stays where it is.
    for step in 0..8 {
        editor
            .update(cx, |editor, _, cx| {
                editor.buffer().update(cx, |buffer, cx| {
                    let len = buffer.read(cx).len().0;
                    let start = fold_start.saturating_sub(40 + step * 3).min(len);
                    let end = (fold_start + 20 + step * 5).min(len);
                    buffer.edit(
                        [(
                            editor::MultiBufferOffset(start)..editor::MultiBufferOffset(end),
                            "shorter ".repeat(step + 1),
                        )],
                        None,
                        cx,
                    );
                });
                editor.display_snapshot(cx);
            })
            .expect("shrink the text under the fold and take a frame");
        cx.run_until_parked();
    }

    // A row of this document cannot be wider than the document. Asking
    // every row for its width is what the element does on the way to
    // drawing them.
    cx.update(|cx| {
        editor
            .update(cx, |editor, _, cx| {
                let display_snapshot = editor.display_snapshot(cx);
                let document_len = editor.buffer().read(cx).read(cx).len().0 as u32;
                for row in 0..=display_snapshot.max_point().row().0 {
                    let width = display_snapshot.line_len(editor::display_map::DisplayRow(row));
                    assert!(
                        width <= document_len,
                        "row {row} was reported {width} columns wide in a document of \
                         {document_len} bytes",
                    );
                }
            })
            .expect("ask every row for its width")
    });

    // The same rows, seen from the wrap map, which records the ones whose
    // columns did not resolve.
    let violations = cx.update(|cx| {
        editor
            .update(cx, |editor, _, cx| {
                editor
                    .display_map
                    .update(cx, |map, cx| map.take_wrap_sync_violations(cx))
            })
            .expect("read the rows the snapshots offered")
    });
    assert!(
        violations.is_empty(),
        "every row a snapshot offered was one its document could answer for; \
         these were not: {violations:#?}"
    );
}

/// A tail-policy fold that reaches the end of its document leaves the
/// tree level with the transforms under it.
///
/// This is the same fault as above met from the other side. When the folds
/// emitted for one edit reach past the end of that edit, the old tree has
/// to be walked forward by the same amount before its remainder is
/// appended. A fold running to the end of the document puts that walk
/// exactly on the last boundary there is, where there is nothing left to
/// append; a fold with text after it puts it inside the run of text that
/// follows. Both are here because they are the two answers the walk can
/// give.
#[gpui::test]
fn a_tail_fold_that_runs_to_the_end_of_its_document(cx: &mut TestAppContext) {
    struct FoldToTheEndInThisTest;

    cx.update(crate::tests::init_test_app);
    let long_line = "wrap me ".repeat(40);
    let body = (0..24)
        .map(|row| format!("{row} {long_line}\n"))
        .collect::<String>();
    let editor = cx.add_window(|window, cx| {
        let mut editor = editor::Editor::multi_line(window, cx);
        editor.set_text(body.clone(), window, cx);
        editor
    });
    editor
        .update(cx, |editor, window, cx| {
            editor.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
            window.refresh();
        })
        .expect("soft wrap at the editor's width");
    cx.simulate_window_resize(*editor, size(px(500.), px(800.)));
    cx.run_until_parked();

    let fold_start = body.find("18 ").expect("row eighteen");
    editor
        .update(cx, |editor, _, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let range = snapshot.anchor_before(editor::MultiBufferOffset(fold_start))
                ..snapshot.anchor_after(editor::MultiBufferOffset(body.len()));
            editor.display_map.update(cx, |map, cx| {
                map.fold(
                    vec![
                        editor::display_map::Crease::simple(
                            range,
                            editor::FoldPlaceholder::concealed(std::any::TypeId::of::<
                                FoldToTheEndInThisTest,
                            >()),
                        )
                        .with_elision_policy(editor::display_map::ElisionPolicy::Tail { rows: 2 }),
                    ],
                    cx,
                );
            });
            editor.display_snapshot(cx);
        })
        .expect("fold the last rows with a visible tail");
    cx.run_until_parked();

    // The edits land in the head of the fold, so what is emitted for them
    // reaches past their end and on to the end of the document.
    for step in 0..10 {
        editor
            .update(cx, |editor, _, cx| {
                editor.buffer().update(cx, |buffer, cx| {
                    let len = buffer.read(cx).len().0;
                    let start = (fold_start + step * 9).min(len);
                    let end = (start + 60).min(len);
                    buffer.edit(
                        [(
                            editor::MultiBufferOffset(start)..editor::MultiBufferOffset(end),
                            "shorter ".repeat(step),
                        )],
                        None,
                        cx,
                    );
                });
                editor.display_snapshot(cx);
            })
            .expect("shrink the head of the fold and take a frame");
        cx.run_until_parked();
    }

    cx.update(|cx| {
        editor
            .update(cx, |editor, _, cx| {
                let display_snapshot = editor.display_snapshot(cx);
                let document_len = editor.buffer().read(cx).read(cx).len().0 as u32;
                for row in 0..=display_snapshot.max_point().row().0 {
                    let width = display_snapshot.line_len(editor::display_map::DisplayRow(row));
                    assert!(
                        width <= document_len,
                        "row {row} was reported {width} columns wide in a document of \
                         {document_len} bytes",
                    );
                }
            })
            .expect("ask every row for its width")
    });
}
