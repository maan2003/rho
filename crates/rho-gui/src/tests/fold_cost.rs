//! What an edit costs as the folds above it multiply.
//!
//! The cost rule the window holds to says a keystroke pays for the rows it
//! touches and a logarithm, never for the document — and elision adds a
//! clause: never for the elisions the document carries. A settled turn is a
//! fold, so a reader typing at the bottom of a long transcript is typing
//! under hundreds of them.
//!
//! The shape is the point, not the milliseconds. Eight times the folds
//! above the edit must not be eight times the edit.

use gpui::TestAppContext;

use super::test_workspace;

/// How many folds stand above the edit in the small and large cases. The
/// large case is the order a four-hundred-turn transcript reaches with one
/// elision per settled turn.
const SMALL: usize = 32;
const LARGE: usize = 256;

/// The document, identical in both cases: the rows the folds are spread
/// over, and the rows below them the edit lands in.
const FOLDED_LINES: usize = 1_024;
const LINES: usize = FOLDED_LINES + 32;

/// What the fold map's cursors walk for one edit at the foot of a document
/// carrying `held` folds above it: leaf items crossed, not milliseconds.
/// A wall clock here would be measuring the rows the folds hide as much as
/// the folds themselves - folding two thousand ranges takes four thousand
/// rows out of the layout - and the count is the thing the cost rule is
/// written in.
///
/// The document is the same size in both cases — the same rows, the same
/// bytes, the same distance from the top of the buffer to the edit. Only
/// the number of folds standing between them changes, so what the ratio
/// measures is the folds and not the document.
fn fold_items_walked_by_an_edit(cx: &mut TestAppContext, held: usize) -> f64 {
    use editor::Editor;
    use gpui::AppContext as _;

    let workspace = test_workspace(cx);
    let line = "a line of a transcript that a reader has already read\n";
    let text = line.repeat(LINES);
    let editor = workspace
        .update(cx, |_, window, cx| {
            cx.new(|cx| {
                let buffer = cx.new(|cx| language::Buffer::local(text.clone(), cx));
                let buffer = cx.new(|cx| multi_buffer::MultiBuffer::singleton(buffer, cx));
                Editor::new(editor::EditorMode::full(), buffer, None, window, cx)
            })
        })
        .expect("an editor");
    cx.run_until_parked();

    workspace
        .update(cx, |_, _window, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                // Spread evenly over the same rows in both cases: two lines
                // folded, the rest of the stride left standing, so the map
                // holds a fold transform and an isomorphic one in turn.
                let stride = FOLDED_LINES / held;
                let creases = (0..held)
                    .map(|at| {
                        let start = at * stride * line.len();
                        let range = snapshot.anchor_before(editor::MultiBufferOffset(start))
                            ..snapshot
                                .anchor_after(editor::MultiBufferOffset(start + 2 * line.len()));
                        editor::display_map::Crease::simple(
                            range,
                            editor::FoldPlaceholder::default(),
                        )
                    })
                    .collect();
                editor
                    .display_map
                    .update(cx, |map, cx| map.fold(creases, cx));
                editor.display_snapshot(cx);
            });
        })
        .expect("the folds stand");
    cx.run_until_parked();

    // The edit is at the foot of the document, below every fold: the row a
    // reader is typing on, with the settled turns above it.
    let repeats = 8;
    gpui::profiler::set_editor_trace_enabled(true);
    let mut collector = gpui::profiler::EditorTimingCollector::new();
    workspace
        .update(cx, |_, _window, cx| {
            editor.update(cx, |editor, cx| {
                for _ in 0..repeats {
                    let end = editor.buffer().read(cx).len(cx);
                    editor.buffer().update(cx, |buffer, cx| {
                        buffer.edit([(end..end, "x")], None, cx);
                    });
                    editor.display_snapshot(cx);
                }
            });
        })
        .expect("the edits ran");
    let walked: u64 = collector
        .collect_unseen()
        .iter()
        .filter(|timing| matches!(timing.kind, gpui::profiler::EditorTimingKind::FoldMapSync))
        .map(|timing| timing.walked_items)
        .sum();
    walked as f64 / repeats as f64
}

/// A keystroke must not cost what the elisions above it cost.
#[gpui::test]
fn an_edit_does_not_cost_the_folds_above_it(cx: &mut TestAppContext) {
    let small = fold_items_walked_by_an_edit(cx, SMALL);
    let large = fold_items_walked_by_an_edit(cx, LARGE);
    let ratio = large / small.max(f64::EPSILON);
    eprintln!("FOLDCOST small={small:.1} large={large:.1} ratio={ratio:.2}");
    assert!(
        ratio < 2.0,
        "an edit under {LARGE} folds walked {large:.1} fold items against {small:.1} under \
         {SMALL} — {ratio:.1}× for {}× the folds",
        LARGE / SMALL,
    );
}
