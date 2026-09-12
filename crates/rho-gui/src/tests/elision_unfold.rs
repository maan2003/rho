//! What unfolds a turn the model has stopped eliding.
//!
//! The reconcile used to unfold by resolving the stale spec's anchors
//! again, and a spec's anchors can stop resolving while the fold it made is
//! still the editor's: a buffer that has left the multibuffer, or an
//! excerpt that covers only part of the buffer it names, answers nothing
//! for a range it does not hold. A range that resolves to nothing unfolds
//! nothing, and there is no second chance — the crease is gone in the same
//! breath, so no later reconcile can name that fold again.
//!
//! What the editor was folding is the editor's own record, so that is what
//! removing a crease now hands back and what the unfold is made of. Every
//! way I could make a spec stop resolving, the multibuffer's own edit had
//! already dropped the fold with the text under it, so this is the
//! invariant pinned rather than a fault reproduced.

use editor::Editor;
use gpui::{AppContext as _, TestAppContext};
use rho_agents::transcript::elisions::{ElisionSpec, ElisionState, ElisionSync};

use super::{history_elisions, init_test_app};

/// Nothing stays folded once the model elides nothing, even when the spec
/// that made the fold can no longer say where it was.
#[gpui::test]
fn a_spec_that_stops_resolving_still_unfolds_its_turn(cx: &mut TestAppContext) {
    cx.update(init_test_app);
    let text = (0..40)
        .map(|row| format!("line {row} of a settled turn"))
        .collect::<Vec<_>>()
        .join("\n");
    let buffer = cx.update(|cx| cx.new(|cx| language::Buffer::local(text, cx)));
    let multi_buffer = cx.update(|cx| {
        cx.new(|cx| {
            let mut multi_buffer =
                multi_buffer::MultiBuffer::without_headers(language::Capability::ReadWrite);
            multi_buffer.set_excerpts_for_path(
                multi_buffer::PathKey::sorted(0),
                buffer.clone(),
                [language::Point::zero()..buffer.read(cx).max_point()],
                0,
                cx,
            );
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

    let spec = cx.update(|cx| {
        let buffer = buffer.read(cx);
        ElisionSpec {
            start_block: 0,
            range: buffer.anchor_before(10)..buffer.anchor_after(90),
            tool_count: 2,
            tail_rows: 1,
        }
    });
    sync.set_specs(vec![spec]);
    cx.update(|cx| {
        host.update(cx, |_, cx| {
            sync.apply(&mut state, &multi_buffer, &editor, cx)
        })
    });
    assert_eq!(
        history_elisions(&editor, cx).len(),
        1,
        "the turn the model elides is folded"
    );

    // The buffer leaves the multibuffer, which is what a rebuild does to
    // the buffer it replaces: the spec's anchors now resolve to nothing.
    let replacement = cx.update(|cx| {
        cx.new(|cx| language::Buffer::local("a different turn entirely\nwith two rows\n", cx))
    });
    cx.update(|cx| {
        multi_buffer.update(cx, |multi_buffer, cx| {
            multi_buffer.set_excerpts_for_path(
                multi_buffer::PathKey::sorted(0),
                replacement.clone(),
                [language::Point::zero()..replacement.read(cx).max_point()],
                0,
                cx,
            );
        })
    });

    sync.set_specs(Vec::new());
    cx.update(|cx| {
        host.update(cx, |_, cx| {
            sync.apply(&mut state, &multi_buffer, &editor, cx)
        })
    });
    assert_eq!(
        history_elisions(&editor, cx).len(),
        0,
        "a fold outlived the spec that made it"
    );
    assert_eq!(
        state.active_specs().count(),
        0,
        "the editor is carrying a spec it was never told to fold"
    );
}
