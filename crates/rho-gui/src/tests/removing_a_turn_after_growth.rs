//! Removing a turn when the turn above it has grown.
//!
//! Two excerpts under paths of their own, text appended to the first
//! buffer, then the second path removed - which is what a frame batch does
//! when a turn is dropped and the one before it has streamed since. The
//! multi-buffer's diff-transform sync then emits an edit whose end is
//! behind its start and subtracts one from the other:
//!
//! ```text
//! attempt to subtract with overflow
//!   multi_buffer::MultiBuffer::sync_diff_transforms
//!   multi_buffer::MultiBuffer::set_excerpts_for_paths
//! ```
//!
//! In a build with overflow checks - a test build, or a debug one - that
//! subtraction panics where it happens. In the build the user runs it
//! wraps, and the edit that comes out of it names offsets near the end of
//! the address space; what crashes there is whatever converts them next,
//! which is why this arrives as `display point out of range` from the
//! display map rather than as anything about excerpts.
//!
//! Found by the walk in `elision_block_geometry`, minimised to this.

use gpui::{AppContext as _, TestAppContext};

fn turn_text(tag: usize, rows: usize) -> String {
    (0..rows)
        .map(|row| format!("row {row} of turn {tag}, long enough that a reader would scroll it\n"))
        .collect()
}

#[gpui::test]
fn removing_a_turn_after_the_one_above_it_grew(cx: &mut TestAppContext) {
    cx.update(crate::tests::init_test_app);
    let mut buffers = Vec::new();
    let multi_buffer = cx.update(|cx| {
        cx.new(|cx| {
            let mut multi_buffer =
                multi_buffer::MultiBuffer::without_headers(language::Capability::ReadWrite);
            for path in 0..2 {
                let buffer = cx.new(|cx| language::Buffer::local(turn_text(path, 3), cx));
                let end = buffer.read(cx).max_point();
                multi_buffer.set_excerpts_for_path(
                    multi_buffer::PathKey::sorted(path as u64),
                    buffer.clone(),
                    [language::Point::zero()..end],
                    0,
                    cx,
                );
                buffers.push(buffer);
            }
            multi_buffer
        })
    });
    let window = cx.add_window(|window, cx| {
        editor::Editor::new(
            editor::EditorMode::Full {
                scale_ui_elements_with_buffer_font_size: true,
                show_active_line_background: false,
                sizing_behavior: editor::SizingBehavior::ExcludeOverscrollMargin,
            },
            multi_buffer.clone(),
            None,
            window,
            cx,
        )
    });
    let editor = window.root(cx).expect("editor");
    cx.run_until_parked();

    // The turn above streams.
    cx.update(|cx| {
        buffers[0].update(cx, |buffer, cx| {
            let end = buffer.len();
            buffer.edit([(end..end, "one more row of the turn above\n")], None, cx);
        });
    });
    cx.run_until_parked();

    // The turn below is dropped.
    cx.update(|cx| {
        multi_buffer.update(cx, |multi_buffer, cx| {
            multi_buffer.set_excerpts_for_paths(
                [(
                    multi_buffer::PathKey::sorted(1),
                    buffers[1].clone(),
                    Vec::new(),
                )],
                0,
                cx,
            );
        });
    });

    // And the snapshot behind it, which is what the display map is asked
    // for on the frame the removal lands in.
    cx.update(|cx| {
        editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let _ = snapshot.text();
            let _ = snapshot.max_point();
        });
    });
    cx.run_until_parked();
}
