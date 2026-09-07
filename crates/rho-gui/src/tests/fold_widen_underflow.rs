//! An edit widened to a fold that begins at the start of the document.
//!
//! `FoldMap::sync` widens an edit that lands inside a fold out to the fold's
//! start, and widens *both* sides by the same number of inlay bytes, on the
//! premise that the bytes before an edit are the same bytes before and after
//! it. The premise fails at the start of a document: the two sides do not
//! have the same number of bytes in front of them when one of them has
//! almost none.
//!
//! `delta = old_delta.max(new_delta)` is then subtracted from both starts
//! without either being asked whether it has that many bytes to give. On the
//! rig this came back as `edit.old 18446744073709551402..2674` — two to the
//! sixty-fourth minus 214 — and the seek afterwards was asked to walk
//! forward from the end of the whole tree.
//!
//! The shape here is the one that produces it, deterministically and
//! without a transcript: a fold that begins at offset zero, and one splice
//! carrying a long inlay *inside* it and a second inlay a few bytes later.
//! The first inlay shifts the second edit's new side out by its whole length
//! while the old side stays where it was, so `new_delta` is large,
//! `old_delta` is a handful of bytes, and the larger of the two is taken
//! from both. The loop's own numbers on this document, before it wraps:
//!
//! ```text
//! old 5..5   new 219..227   old_delta 5   new_delta 219
//! ```
//!
//! The long inlay has to sit strictly inside the fold and not at its first
//! byte. Anchored at offset zero it lands in *front* of the fold's start
//! anchor, the fold's own start moves out by the same 214 bytes, and both
//! deltas come back 5 — symmetric, and no fault. That symmetry is the
//! premise the loop is written on, and it holds right up until something
//! shifts one side from inside the fold rather than before it.
//!
//! Reported by eng-b8os from the desk rig; the fault is theirs to fix and
//! this is only the case that names it.
//!
//! That rule is gone now: each side is widened to the boundary its own
//! fold starts at, named once in buffer coordinates. The document stays
//! because it is the shape that produced the fault, and what it asks for
//! is now the opposite - a widening with nothing to report.

use gpui::{TestAppContext, px, size};

/// A splice inside a fold that starts at offset zero does not ask an edit to
/// give up more bytes than it has.
///
/// The assertion is that the map answers at all: on the fault the widening
/// wraps and the seek that follows panics, so a snapshot taken after the
/// splice is the whole test. What it checks beyond that is that the document
/// the fold map describes still has the rows the inlay map beneath it says
/// it has — a wrapped offset that happens not to panic would leave those two
/// disagreeing, and a test that only caught the panic would pass on it.
#[gpui::test]
fn a_splice_inside_a_fold_that_starts_at_zero_does_not_underflow(cx: &mut TestAppContext) {
    struct FoldFromTheTop;

    cx.update(crate::tests::init_test_app);
    let long_line = "fold me ".repeat(20);
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

    // A fold whose first byte is the document's first byte. This is the
    // history fold's shape in a transcript, which is why the crash lands on
    // the first open rather than somewhere rare.
    let fold_end = body.find("9 ").expect("row nine");
    editor
        .update(cx, |editor, _, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let range = snapshot.anchor_before(editor::MultiBufferOffset(0))
                ..snapshot.anchor_after(editor::MultiBufferOffset(fold_end));
            editor.display_map.update(cx, |map, cx| {
                map.fold(
                    vec![editor::display_map::Crease::simple(
                        range,
                        editor::FoldPlaceholder {
                            type_tag: Some(std::any::TypeId::of::<FoldFromTheTop>()),
                            ..Default::default()
                        },
                    )],
                    cx,
                );
            });
            editor.display_snapshot(cx);
        })
        .expect("fold from the first byte");
    cx.run_until_parked();

    // Two inlays in one splice, both inside that fold. The first is long
    // and sits at offset zero; the second is a few bytes later, so its own
    // edit has a new side shifted out by the first inlay's whole length and
    // an old side still five bytes from the start of the document. That is
    // the pair the widening loop subtracts the larger of.
    editor
        .update(cx, |editor, _, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let inside = snapshot.anchor_before(editor::MultiBufferOffset(2));
            let just_after = snapshot.anchor_before(editor::MultiBufferOffset(5));
            editor.splice_inlays(
                &[],
                vec![
                    editor::Inlay::custom(1, inside, "x".repeat(214)),
                    editor::Inlay::custom(2, just_after, "y".repeat(8)),
                ],
                cx,
            );
            editor.display_snapshot(cx);
        })
        .expect("splice two inlays inside the fold");
    cx.run_until_parked();

    // Nothing to say. This assertion used to characterise a record of
    // three lines: two clamps and the inverted edit the clamps left
    // behind. It was the right assertion for a rule that no longer
    // exists. Each side of an edit is now widened to the boundary its own
    // fold starts at, named once in buffer coordinates and said back on
    // each side in that side's own, so the sides never have to give each
    // other bytes they do not have and never end up naming different
    // boundaries. On this document that leaves the widening with nothing
    // to report, which is the whole of what the rule change guarantees.
    //
    // Written as emptiness rather than as three matched lines on purpose:
    // an assertion that names the lines it expects is satisfied by a
    // fault standing next to the one it is checking, which is exactly how
    // the inverted edit went unnoticed here the first time.
    let said = editor
        .update(cx, |editor, _, cx| {
            editor
                .display_map
                .update(cx, |map, _| map.take_fold_widening_violations())
        })
        .expect("read what the widening recorded");
    assert!(
        said.is_empty(),
        "the widening had nothing to report on this document; it said {said:#?}"
    );

    // What the accounting record says here, in full and on purpose. This
    // is the fault that remains: the fold map tells the layers above the
    // document lost eight bytes, and its own output extent did not change.
    // Eight is the length of the `y` inlay spliced above, which is what
    // ties it to the end of an edit still being widened by one step common
    // to both sides. This assertion is meant to fail on the day that rule
    // is fixed rather than guarded.
    let books = editor
        .update(cx, |editor, _, cx| {
            editor
                .display_map
                .update(cx, |map, _| map.take_fold_accounting_violations())
        })
        .expect("read what the accounting recorded");
    assert_eq!(
        books.len(),
        1,
        "one sync on this document does not account for its own output; it said {books:#?}"
    );
    assert!(
        books[0].contains("net -8"),
        "the edits say the output lost eight bytes, the length of the inlay, \
         and the output did not shrink; it said {:?}",
        books[0]
    );

    // And what it does not say. The two ends of an edit meeting at one
    // buffer offset is legitimate when it is the end of one and the same
    // fold on both sides, and that is what happens here - once, at the end
    // of the fold. Counted rather than left to silence: without the count,
    // a document whose ends meet legitimately and one whose ends never meet
    // at all read the same, and an empty violation record would be a
    // statement about this document rather than about the rule.
    let met = editor
        .update(cx, |editor, _, cx| {
            editor
                .display_map
                .update(cx, |map, _| map.take_fold_end_convergences())
        })
        .expect("read how often the ends met legitimately");
    assert!(
        met > 0,
        "the ends of an edit meet at the end of the fold on this document, \
         so an empty convergence-fault record means the shape was allowed \
         rather than absent; they met {met} times"
    );

    // The map still describes one document: what the fold map says it has,
    // the inlay map beneath it agrees it has.
    editor
        .update(cx, |editor, _, cx| {
            let snapshot = editor.display_snapshot(cx);
            let fold_rows = snapshot.fold_snapshot().max_point().row() + 1;
            let inlay_rows = snapshot.inlay_snapshot().max_point().row() + 1;
            assert!(
                fold_rows <= inlay_rows,
                "a fold only ever removes rows, so the fold map cannot have \
                 more rows than the inlay map beneath it; it had {fold_rows} \
                 against {inlay_rows}"
            );
        })
        .expect("read the map after the splice");
}
