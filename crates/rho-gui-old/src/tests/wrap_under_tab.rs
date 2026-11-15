//! Wrapping splits rows; it never joins them.
//!
//! eng-b8os's per-layer probe on the desk rig read wrap 246 rows against a
//! tab map of 247 beneath it, and the block map above was then built at 247
//! input rows over a wrap that says 246. That ordering cannot be right in
//! the lattice: the wrap map takes the tab map's rows and cuts some of them
//! in two, so it can only ever emit as many rows as it was given, or more.
//! On their healthy control the same reading is 728 against 727, the right
//! way round.
//!
//! This is that invariant as a test rather than as a rig session, written
//! around the shape they named: a fold is computed on inlay points, two
//! layers below wrap, so a fold boundary lands wherever it happens to fall
//! inside a wrapped row. Both ends of the fold here are mid-line for that
//! reason, and every row in this document is long enough to wrap at the
//! widths used.
//!
//! The invariant is asserted, not one run's numbers: this test does not know
//! what the right row count is and does not claim to. It knows only that one
//! layer cannot have fewer rows than the layer under it.

use gpui::{TestAppContext, px, size};

/// A fold whose ends fall inside wrapped rows does not leave the wrap map
/// shorter than the tab map it wrapped, at the width it was folded at or at
/// a new one.
///
/// The rewrap is half the test. A width change is what re-derives every
/// wrap row over a fold boundary that has not moved, which is the moment
/// the two layers have to agree again about a boundary neither of them
/// chose.
#[gpui::test]
fn a_mid_row_fold_never_leaves_wrap_shorter_than_tab(cx: &mut TestAppContext) {
    struct FoldInThisTest;

    cx.update(crate::tests::init_test_app);
    // Every row wraps several times at these widths, so a fold boundary
    // landing mid-wrapped-row is the ordinary case and not a contrivance.
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
    assert_wrap_is_not_shorter_than_tab(&editor, cx, "before any fold");

    // Inside the rows rather than at their starts: a boundary at a line
    // start is one wrap boundary as well, and the invariant is about the
    // boundaries neither layer chose.
    let fold_start = body.find("6 ").expect("row six") + 30;
    let fold_end = body.find("17 ").expect("row seventeen") + 30;
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
                            editor::FoldPlaceholder::concealed(
                                std::any::TypeId::of::<FoldInThisTest>(),
                            ),
                        ),
                    ],
                    cx,
                );
            });
            editor.display_snapshot(cx);
        })
        .expect("fold the middle");
    cx.run_until_parked();
    assert_wrap_is_not_shorter_than_tab(&editor, cx, "with a mid-row fold");

    // A rewrap at each width, with the fold left exactly where it is. The
    // boundary does not move; what a row is does.
    for width in [420., 360., 300., 640., 380., 500.] {
        cx.simulate_window_resize(*editor, size(px(width), px(800.)));
        cx.run_until_parked();
        editor
            .update(cx, |editor, _, cx| {
                editor.display_snapshot(cx);
            })
            .expect("take the frame this width would have painted");
        cx.run_until_parked();
        assert_wrap_is_not_shorter_than_tab(&editor, cx, &format!("rewrapped at {width}px"));
    }
}

/// The reading itself, from each layer's own snapshot. `DisplaySnapshot`
/// hands out the wrap and tab snapshots directly, so neither number can
/// arrive by deref from somewhere further down the chain — which is the
/// mistake the rig probe made before this invariant was trusted at all.
#[track_caller]
fn assert_wrap_is_not_shorter_than_tab(
    editor: &gpui::WindowHandle<editor::Editor>,
    cx: &mut TestAppContext,
    when: &str,
) {
    let (wrap_rows, tab_rows) = editor
        .update(cx, |editor, _, cx| {
            let snapshot = editor.display_snapshot(cx);
            (
                snapshot.wrap_snapshot().max_point().row().0 + 1,
                snapshot.tab_snapshot().max_point().row() + 1,
            )
        })
        .expect("read each layer's own row count");
    assert!(
        wrap_rows >= tab_rows,
        "wrapping splits rows and never joins them, so the wrap map cannot \
         have fewer rows than the tab map beneath it; {when} it had \
         {wrap_rows} against {tab_rows}"
    );
}
