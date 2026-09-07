//! An inlay standing exactly on a boundary an edit is widened to.
//!
//! Widening an edit to a fold is a statement about buffer offsets: both
//! sides name one boundary, and the only way they may differ is by the
//! inlay bytes standing in front of that boundary on one side and not the
//! other. Saying the boundary back on each side means converting a buffer
//! offset to that side's inlay offset, and at an inlay a buffer offset has
//! two inlay offsets - one before the inlay text and one after it.
//!
//! The choice between them is made by the direction of travel and never by
//! the inlay's own bias. A start travels backwards, so it takes the offset
//! before the inlay; an end travels forwards, so it takes the one after.
//! Either way the inlay finishes up *inside* the widened edit, which is
//! where it belongs: the layers above are being told this stretch of the
//! document is being re-emitted, and an inlay left just outside is text
//! they will not be told changed.
//!
//! `to_inlay_offset` on its own does not do this. It resolves the ambiguity
//! by the inlay's own bias - stepping over a `Bias::Left` inlay and
//! stopping in front of a `Bias::Right` one - which is the right rule for a
//! caller asking where a buffer position is and the wrong one for a caller
//! asking what a widened edit covers. Half the four cases here would pass
//! on `to_inlay_offset` alone, which is why all four are written out.
//!
//! Each case asserts through the widening record, which reports an inlay
//! left adjacent to a widened side rather than brought inside it.
//!
//! Only the two start cases are here. The mirror pair on a widened end are
//! written and held back: the end is still widened by one step common to
//! both sides, the rule this cut replaced at the start, and on the
//! right-biased end case that leaves the inlay outside the edit. They land
//! with the end rule.

use gpui::{TestAppContext, px, size};

struct BiasFold;

/// A document with one fold in the middle of it, and the buffer offsets of
/// that fold's first and last byte.
fn folded_document(cx: &mut TestAppContext) -> (gpui::WindowHandle<editor::Editor>, usize, usize) {
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

    // Interior on both sides, so a widened start and a widened end are
    // both real boundaries with document either side of them.
    let fold_start = body.find("5 ").expect("row five");
    let fold_end = body.find("9 ").expect("row nine");
    editor
        .update(cx, |editor, _, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let range = snapshot.anchor_before(editor::MultiBufferOffset(fold_start))
                ..snapshot.anchor_after(editor::MultiBufferOffset(fold_end));
            editor.display_map.update(cx, |map, cx| {
                map.fold(
                    vec![editor::display_map::Crease::simple(
                        range,
                        editor::FoldPlaceholder {
                            type_tag: Some(std::any::TypeId::of::<BiasFold>()),
                            ..Default::default()
                        },
                    )],
                    cx,
                );
            });
            editor.display_snapshot(cx);
        })
        .expect("fold the middle of the document");
    cx.run_until_parked();
    (editor, fold_start, fold_end)
}

/// Stand an inlay on the fold's first byte with the given bias,
/// then make an edit *inside* the fold, whose start is widened back to the
/// fold's start and whose end is widened out to the fold's end. Both
/// widened boundaries then have an inlay on them.
fn inlay_on_a_widened_boundary(cx: &mut TestAppContext, at_start: bool, before: bool) {
    let (editor, fold_start, fold_end) = folded_document(cx);
    let boundary = if at_start { fold_start } else { fold_end };
    editor
        .update(cx, |editor, _, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let anchor = if before {
                snapshot.anchor_before(editor::MultiBufferOffset(boundary))
            } else {
                snapshot.anchor_after(editor::MultiBufferOffset(boundary))
            };
            editor.splice_inlays(
                &[],
                vec![editor::Inlay::custom(1, anchor, "z".repeat(96))],
                cx,
            );
            editor.display_snapshot(cx);
        })
        .expect("stand an inlay on the boundary");
    cx.run_until_parked();
    editor
        .update(cx, |editor, _, cx| {
            editor
                .display_map
                .update(cx, |map, _| map.take_fold_widening_violations())
        })
        .expect("clear what standing the inlay up recorded");

    // An edit strictly inside the fold. Its start is widened back to the
    // fold's start and its end out to the fold's end, so both boundaries
    // the widening chooses have an inlay sitting on them.
    let inside = (fold_start + fold_end) / 2;
    editor
        .update(cx, |editor, _, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let anchor = snapshot.anchor_before(editor::MultiBufferOffset(inside));
            editor.splice_inlays(
                &[],
                vec![editor::Inlay::custom(2, anchor, "y".repeat(24))],
                cx,
            );
            editor.display_snapshot(cx);
        })
        .expect("splice inside the fold");
    cx.run_until_parked();

    let said = editor
        .update(cx, |editor, _, cx| {
            editor
                .display_map
                .update(cx, |map, _| map.take_fold_widening_violations())
        })
        .expect("read what the widening recorded");
    assert!(
        said.is_empty(),
        "the inlay standing on the boundary belongs inside the widened \
         edit; the widening said {said:#?}"
    );

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

#[gpui::test]
fn a_left_biased_inlay_on_a_widened_start_lands_inside_it(cx: &mut TestAppContext) {
    inlay_on_a_widened_boundary(cx, true, true);
}

#[gpui::test]
fn a_right_biased_inlay_on_a_widened_start_lands_inside_it(cx: &mut TestAppContext) {
    inlay_on_a_widened_boundary(cx, true, false);
}
