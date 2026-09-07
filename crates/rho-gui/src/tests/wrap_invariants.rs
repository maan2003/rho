//! What the wrap map promises the maps above it.
//!
//! `WrapMap::sync` hands its caller a snapshot and, beside it, the edits
//! since the caller's last one. The block map above resolves the rows those
//! edits name against that snapshot, so the two have to describe one
//! document. When they do not, a row range is asked of a document that does
//! not have it, a column of one row resolves on another, and the rope says
//! so from underneath four maps: "point Point(1:137) extends beyond row".

use editor::Editor;
use gpui::{Context, TestAppContext, px};
use multi_buffer::MultiBufferRow;

use crate::tests::init_test_app;

/// A width change that lays out the reader's rows first keeps the wrap
/// map's contract, including over the edits it has not applied yet.
///
/// `rewrap` opens by clearing `interpolated_edits` and `pending_edits`. That
/// was safe while every path behind it re-wrapped the whole document. The
/// reader-rows branch returns before any of that, so the clear now happens
/// with a chunk still owing the rest of the document — and this asserts
/// what the caller is told across the whole of it. The rows outside the
/// reader's range are left carrying an edit rather than wrapped for the new
/// width, which is wrong-looking until the backfill reaches them, but it is
/// structurally sound and it is reported: the point of this test is that
/// "reported" is the part that must not slip.
#[gpui::test]
fn a_width_change_for_a_reader_tells_its_caller_one_document(cx: &mut TestAppContext) {
    cx.update(init_test_app);
    let line = "alpha bravo charlie delta echo foxtrot golf hotel india juliett";
    let editor = cx.add_window(|window, cx| {
        let mut editor = Editor::multi_line(window, cx);
        editor.set_text(format!("{line}\n").repeat(2_000), window, cx);
        editor
    });
    editor
        .update(cx, |editor, window, cx| {
            editor.set_soft_wrap_mode(language::language_settings::SoftWrap::EditorWidth, cx);
            window.refresh();
        })
        .expect("soft wrap at the editor's width");
    cx.simulate_window_resize(*editor, gpui::size(px(300.), px(400.)));
    cx.run_until_parked();
    editor
        .update(cx, |_, window, cx| window.simulate_next_frame(cx))
        .expect("wrap the document this editor opened on");
    cx.run_until_parked();

    // Everything below happens without the executor ticking, so the chunk a
    // width change spawns is in flight for all of it. That is the only way
    // to hold this state here: nothing in this gpui can deprioritize a task.
    let (queued, resized, records) = editor
        .update(cx, |editor, _, cx| {
            let top = || MultiBufferRow(0)..MultiBufferRow(30);

            // A reader at the top asks for a new width. Their rows are laid
            // out before the call returns; the rest is owed to a chunk.
            set_width(editor, 120., top(), cx);

            // The transcript composes while that chunk runs. The edit is
            // queued and carried through by an interpolation, because a
            // flush does no real work while a chunk holds the map.
            edit_at_top(editor, "one\n", cx);
            let queued = editor.display_map.read(cx).wrap_queue_state(cx);

            // The reader resizes again, mid-backfill: this is where rewrap
            // drops the queue and the interpolation behind a partial layout.
            set_width(editor, 90., top(), cx);
            let resized = editor.display_map.read(cx).wrap_queue_state(cx);

            // The next edit — an elision applying a fold is one — syncs
            // against whatever the resize left behind.
            edit_at_top(editor, "two\n", cx);

            let records = editor.display_map.read(cx).wrap_sync_records(cx);
            (queued, resized, records)
        })
        .expect("the whole sequence with no executor tick in it");

    // The test is worth nothing unless it reached the state it is about.
    assert!(
        queued.chunk_in_flight && queued.backfilling,
        "a chunk still owes the rest of the document while the edit lands"
    );
    assert!(
        queued.queued_batches == 1 && queued.interpolated,
        "and the edit is queued behind it, carried by an interpolation"
    );
    assert!(
        resized.queued_batches == 0 && !resized.interpolated,
        "the second width change is what drops that queue: this is the \
         sequence under test, not a hypothetical"
    );

    let broken = records
        .windows(2)
        .filter(|pair| {
            i64::from(pair[0].rows) + pair[1].rows_named != i64::from(pair[1].rows)
                || pair[1].old_end > pair[0].rows
                || pair[1].new_end > pair[1].rows
        })
        .map(|pair| (pair[0].rows, pair[1]))
        .collect::<Vec<_>>();
    assert!(
        broken.is_empty(),
        "every row the snapshot gained or lost between two syncs is named \
         by the edits handed over with it, and no edit reaches past either \
         snapshot; {} syncs, these disagreed: {broken:?}",
        records.len()
    );
}

fn set_width(
    editor: &mut Editor,
    width: f32,
    rows: std::ops::Range<MultiBufferRow>,
    cx: &mut Context<Editor>,
) {
    editor.display_map.update(cx, |map, cx| {
        map.set_wrap_width(
            Some(px(width)),
            editor::display_map::WrapPriority::ReaderRows(rows),
            cx,
        )
    });
}

/// One edit at the top of the buffer, then the snapshot that drives the
/// display map's sync.
fn edit_at_top(editor: &mut Editor, text: &str, cx: &mut Context<Editor>) {
    editor.buffer().update(cx, |buffer, cx| {
        buffer.edit(
            [(
                editor::MultiBufferOffset(0)..editor::MultiBufferOffset(0),
                text,
            )],
            None,
            cx,
        );
    });
    editor.display_snapshot(cx);
}
