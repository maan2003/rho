//! Whether a snapshot the wrap map hands out describes the document it
//! carries.
//!
//! The user's GUI panicked at 09:12 with `line_ix 38 is out of bounds -
//! row_infos.len(): 39, line_layouts.len(): 6`: the element took its row
//! count from the display snapshot and its text from a chunk iterator that
//! ran dry after six rows, then indexed the layouts it never got. Under it,
//! the rope had already said so twice - `point Point(0:49) extends beyond
//! row` for a row about thirty-three characters wide.
//!
//! `WrapSnapshot::line_len` resolves a row's width as `tab_line_len -
//! start.1.column()` on unsigned integers, so a wrap boundary sitting past
//! the end of a line is a row about four billion columns wide in any build
//! without debug assertions, which is every build a reader runs. The check
//! these tests use is the narrow one that catches it before it becomes a
//! width: every display row the snapshot offers must start at a tab row that
//! document has, at a column that row reaches.

use gpui::{TestAppContext, px, size};
use rho_agents::state::UiMessagePhase;

use crate::tests::{
    active_editor, agent, assistant, feed_frame, long_working_text, state, stream_text,
    test_workspace, user,
};

/// No sync hands its caller a row the document it is carrying does not have,
/// through a transcript that is composing while the window resizes.
///
/// This is the shape of the drive the fault appeared under: text arriving a
/// chunk at a time, the reader's width changing underneath it, and a frame
/// taken between every step rather than after the map has settled. Each of
/// those snapshots is one the element would have painted.
#[gpui::test]
fn no_sync_offers_a_row_the_document_does_not_have(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    cx.simulate_window_resize(*workspace, size(px(500.), px(800.)));

    // Lines long enough to soft-wrap several times at these widths, so the
    // transforms carry real wrap boundaries rather than a passthrough.
    let long_line = "wrap me ".repeat(120);
    let mut composing = state(
        vec![user(&long_line)],
        vec![assistant(
            &long_working_text(),
            Some(UiMessagePhase::Commentary),
        )],
    );
    feed_frame(&workspace, cx, agent(1), composing.clone());
    let editor = active_editor(&workspace, cx);

    // Widths a reader lands on, and text arriving between them. Nothing here
    // parks: a snapshot taken while the map still owes work is the one the
    // element would have painted, and the one the fault appeared on.
    let widths = [420., 360., 300., 640., 380.];
    let body = long_working_text();
    for step in 0..24 {
        let keep = body.len().saturating_sub(step * 37 + 11);
        stream_text(&mut composing, 1, keep, &long_line);
        workspace
            .update(cx, |workspace, window, cx| {
                workspace.seed_transcript_for_test(agent(1), composing.clone(), window, cx);
            })
            .expect("stream a chunk into the transcript");
        if step % 4 == 0 {
            cx.simulate_window_resize(
                *workspace,
                size(px(widths[(step / 4) % widths.len()]), px(800.)),
            );
        }
        // What a frame does: ask the display map for a snapshot.
        cx.update(|cx| {
            editor.update(cx, |editor, cx| {
                editor.display_snapshot(cx);
            })
        });
    }
    cx.run_until_parked();
    cx.update(|cx| {
        editor.update(cx, |editor, cx| {
            editor.display_snapshot(cx);
        })
    });

    let violations = cx.update(|cx| {
        editor.update(cx, |editor, cx| {
            editor
                .display_map
                .update(cx, |map, cx| map.take_wrap_sync_violations(cx))
        })
    });
    assert!(
        violations.is_empty(),
        "every snapshot a sync handed out described the document it was \
         carrying; these did not: {violations:#?}"
    );
}
/// The same shape with no transcript, no elisions and no blocks: a plain
/// soft-wrapped editor whose text shrinks between frames.
#[gpui::test]
fn a_plain_editor_whose_text_shrinks_between_frames(cx: &mut TestAppContext) {
    cx.update(crate::tests::init_test_app);
    let long_line = "wrap me ".repeat(120);
    let body = long_line.repeat(8);
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
    editor
        .update(cx, |_, window, cx| window.simulate_next_frame(cx))
        .expect("wrap the document this editor opened on");
    cx.run_until_parked();

    for step in 0..24 {
        let keep = body.len().saturating_sub(step * 37 + 11);
        editor
            .update(cx, |editor, _, cx| {
                let mut text = body.clone();
                text.truncate(keep);
                text.push_str(&long_line);
                editor.buffer().update(cx, |buffer, cx| {
                    let end = buffer.read(cx).len();
                    buffer.edit([(editor::MultiBufferOffset(0)..end, text)], None, cx);
                });
                editor.display_snapshot(cx);
            })
            .expect("shrink the document and take a frame");
    }
    cx.run_until_parked();
}
