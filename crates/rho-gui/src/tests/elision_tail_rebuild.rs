//! Rebuilding a buffer under elisions that keep a tail.
//!
//! The panic this pins crashed the window on a frame batch: a batch
//! replaces the buffers from the changed block on, and disabling the
//! header on each new buffer syncs the display map with the elisions above
//! still folded. The fold map then walked a fold that starts behind the
//! input it had already consumed and asserted.
//!
//! Only `ElisionPolicy::Tail` reaches it, which is ours. A fold that hides
//! its whole range is one transform, so the copied prefix either takes it
//! whole or stops before it; a fold that keeps a tail is two — a
//! placeholder over the head and the visible tail after it — and the
//! prefix can stop between them, on a boundary inside the fold's own
//! range.
//!
//! This drives the operations a frame batch performs rather than a batch:
//! excerpts replaced for a run of paths in one update, then the header
//! disabled for each new buffer. I could not get a batch itself to make
//! the geometry — the turns it elides sit inside their buffers rather than
//! covering them — and the sequence that does is the one below.

use editor::Editor;
use gpui::{AppContext as _, Entity, TestAppContext};
use rho_agents::transcript::elisions::{ElisionSpec, ElisionState, ElisionSync};

use super::{history_elisions, init_test_app};

/// Three buffers, and the last one replaced: two elisions have to stand
/// above the rebuild for the copied prefix to stop inside one of them.
const TURNS: usize = 3;
const REBUILT_FROM: usize = 2;

fn turn_text(tag: usize) -> String {
    (0..8)
        .map(|row| format!("row {row} of turn {tag}, long enough that a reader would scroll it\n"))
        .collect()
}

#[gpui::test]
fn rebuilding_a_buffer_under_elided_turns_that_keep_a_tail(cx: &mut TestAppContext) {
    cx.update(init_test_app);
    let buffers: Vec<Entity<language::Buffer>> = (0..TURNS)
        .map(|tag| cx.update(|cx| cx.new(|cx| language::Buffer::local(turn_text(tag), cx))))
        .collect();
    let multi_buffer = cx.update(|cx| {
        cx.new(|cx| {
            let mut multi_buffer =
                multi_buffer::MultiBuffer::without_headers(language::Capability::ReadWrite);
            for (index, buffer) in buffers.iter().enumerate() {
                multi_buffer.set_excerpts_for_path(
                    multi_buffer::PathKey::sorted(index as u64),
                    buffer.clone(),
                    [language::Point::zero()..buffer.read(cx).max_point()],
                    0,
                    cx,
                );
            }
            multi_buffer
        })
    });
    let window = cx.add_window(|window, cx| {
        Editor::new(
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
    let host = cx.update(|cx| cx.new(|_| ()));
    let mut sync = ElisionSync::default();
    let mut state = ElisionState::default();

    // Every turn elided with a tail kept, which is what a turn of working
    // blocks and no answer gets.
    let specs = cx.update(|cx| {
        buffers
            .iter()
            .map(|buffer| {
                let buffer = buffer.read(cx);
                ElisionSpec {
                    range: buffer.anchor_before(4)..buffer.anchor_after(buffer.len() - 4),
                    tool_count: 2,
                    tail_rows: 1,
                }
            })
            .collect::<Vec<_>>()
    });
    sync.set_specs(specs);
    cx.update(|cx| {
        host.update(cx, |_, cx| {
            sync.apply(&mut state, &multi_buffer, &editor, cx)
        })
    });
    assert_eq!(
        history_elisions(&editor, cx).len(),
        TURNS,
        "every turn is elided before the rebuild"
    );

    // The rebuild: new buffers under the paths from the changed block on,
    // set in one update, then each new buffer's header disabled — which is
    // what syncs the display map through the wrap with the folds above
    // still in it.
    let rebuilt: Vec<Entity<language::Buffer>> = (REBUILT_FROM..TURNS)
        .map(|tag| cx.update(|cx| cx.new(|cx| language::Buffer::local(turn_text(tag + 100), cx))))
        .collect();
    cx.update(|cx| {
        multi_buffer.update(cx, |multi_buffer, cx| {
            for (offset, buffer) in rebuilt.iter().enumerate() {
                multi_buffer.set_excerpts_for_path(
                    multi_buffer::PathKey::sorted((REBUILT_FROM + offset) as u64),
                    buffer.clone(),
                    [language::Point::zero()..buffer.read(cx).max_point()],
                    0,
                    cx,
                );
            }
        })
    });
    cx.update(|cx| {
        editor.update(cx, |editor, cx| {
            for buffer in &rebuilt {
                let id = buffer.read(cx).remote_id();
                editor.disable_header_for_buffer(id, cx);
            }
        })
    });

    assert_eq!(
        history_elisions(&editor, cx).len(),
        REBUILT_FROM,
        "the turns above the rebuild are still elided and the rebuilt one is not"
    );
}
