//! What an inlay splice costs as the document grows.
//!
//! The shape is the point, not the milliseconds. `InlayMap::splice` asks the
//! inlay map where each affected offset lands, twice — once in the old
//! snapshot and once in the new — and that lookup used to walk every
//! transform in the map. So a splice cost the offsets times the transforms,
//! and both grow with the document: on a long transcript it was most of the
//! main thread, which is what three of the user's telemetry reports said
//! before this was fixed (45%, 48% and 56% of all main-thread samples inside
//! that one function, every sample reached through `splice`).
//!
//! A test that asserted a number of milliseconds would be a test about the
//! machine it ran on. This asserts the shape instead: quadrupling the
//! transforms must not quadruple the cost of a splice. A walk does; a seek
//! does not.

use std::time::Instant;

use gpui::TestAppContext;

use super::test_workspace;

/// How many inlays the small and large cases carry. The large case is near
/// the transform count the user's reports show on a live transcript.
const SMALL: usize = 500;
const LARGE: usize = 4_000;

/// Splicing one inlay into a map that already holds `held` of them, timed
/// over enough repeats that the number is not one scheduler hiccup.
fn cost_of_a_splice(cx: &mut TestAppContext, held: usize) -> f64 {
    use editor::Editor;
    use gpui::AppContext as _;
    use language::InlayId;

    let workspace = test_workspace(cx);
    let text = "a line of a transcript that is long enough to be wrapped\n".repeat(held + 32);
    let editor = workspace
        .update(cx, |_, window, cx| {
            cx.new(|cx| {
                let buffer = cx.new(|cx| language::Buffer::local(text.clone(), cx));
                let buffer = cx.new(|cx| multi_buffer::MultiBuffer::singleton(buffer, cx));
                Editor::new(editor::EditorMode::full(), buffer, None, window, cx)
            })
        })
        .expect("an editor");

    // Fill the map. Each inlay is a transform, and the isomorphic runs
    // between them are transforms too, so `held` inlays make a tree of
    // roughly twice that.
    workspace
        .update(cx, |_, _window, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                let inlays: Vec<_> = (0..held)
                    .map(|at| {
                        let offset = multi_buffer::MultiBufferOffset(at * 57 + 8);
                        editor::Inlay::custom(1_000_000 + at, snapshot.anchor_before(offset), "·")
                    })
                    .collect();
                editor.splice_inlays(&[], inlays, cx);
            });
        })
        .expect("the map filled");

    // The measurement is a batch, not a single inlay, because that is the
    // shape the cost has in the product: the dashboard re-splices the whole
    // prefix set of its agent tree on a model event, so one splice carries
    // as many affected offsets as there are rows. `splice` asks the inlay
    // map where every affected offset lands, twice, so N offsets against N
    // transforms is the quadratic the user is paying for. A splice of one
    // inlay never shows it — the rest of `splice` swamps two lookups.
    let batch = (held / 4).max(1);
    let repeats = 8;
    let started = Instant::now();
    workspace
        .update(cx, |_, _window, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.buffer().read(cx).snapshot(cx);
                for _ in 0..repeats {
                    let inlays: Vec<_> = (0..batch)
                        .map(|at| {
                            let offset = multi_buffer::MultiBufferOffset(at * 57 + 24);
                            editor::Inlay::custom(
                                9_000_000 + at,
                                snapshot.anchor_before(offset),
                                "x",
                            )
                        })
                        .collect();
                    let ids: Vec<_> = (0..batch)
                        .map(|at| InlayId::Custom(9_000_000 + at))
                        .collect();
                    editor.splice_inlays(&[], inlays, cx);
                    editor.splice_inlays(&ids, Vec::new(), cx);
                }
            });
        })
        .expect("the splices ran");
    started.elapsed().as_secs_f64() / repeats as f64
}

/// A splice must not cost what the whole document costs.
///
/// Eight times the transforms and eight times the offsets in the batch. The
/// lookup used to be a walk, so it paid both — sixty-four times over, and
/// measured at **41.4×** here. Seeking the tree instead leaves only the batch
/// itself, measured at **10.0×**. Twenty is the line between them: far above
/// the honest linear cost of a bigger batch, far below what a walk can reach
/// on any machine.
///
/// The numbers behind those two, on the desk host, per batch splice:
///
/// | transforms | walk    | seek    |
/// |------------|---------|---------|
/// | 500        | 0.089 s | 0.041 s |
/// | 4 000      | 3.683 s | 0.413 s |
#[gpui::test]
fn a_splice_does_not_cost_the_whole_document(cx: &mut TestAppContext) {
    let small = cost_of_a_splice(cx, SMALL);
    let large = cost_of_a_splice(cx, LARGE);
    let ratio = large / small.max(f64::EPSILON);
    assert!(
        ratio < 20.0,
        "a splice at {LARGE} inlays cost {large:.6}s against {small:.6}s at {SMALL} — \
         {ratio:.1}× for {}× the transforms, which is a walk and not a seek",
        LARGE / SMALL,
    );
}
