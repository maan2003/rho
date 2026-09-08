//! What composing history parses, and what it leaves for later.
//!
//! Composing a screenful of history builds one buffer per chunk, and the
//! transcript used to parse the syntax of every one of them inside the
//! frame that composed them. That is unbounded by construction: the count
//! is chunks, not rows, and a hundred-and-sixty two-line chunks cost the
//! frame a hundred and sixty parses to draw the eight of them a window
//! holds.
//!
//! The editor already parses the buffers it can see — on an excerpt
//! change, on a display-map change and on a scroll — so the composing
//! frame pays for what it draws whether or not the transcript warms the
//! rest. Composing stops warming; the rows drawn are parsed by the
//! element that draws them, and everything above them is parsed when the
//! reader arrives at it.

use gpui::TestAppContext;

/// A row well below the window and well inside what `gg` composed. The
/// test window draws about sixty rows of the two hundred composed, and
/// this row holds a markdown chunk rather than a user message, so the
/// question asked of it is not vacuous.
const FAR_ROW: u32 = 160;

use super::{active_editor, agent, bind_test_keymaps, feed_frame, long_history, test_workspace};

/// Composing the top of a long transcript parses the buffers on screen and
/// leaves the rest alone.
///
/// The count is markdown buffers only: a buffer with no language has no
/// syntax to parse and would read as parsed either way. `ensure_syntax_parsed`
/// answers whether it turned parsing on, so asking is also the count —
/// which is why it is asked once, at the end, after the assertion about
/// the point's own buffer.
#[gpui::test]
fn composing_history_parses_what_it_draws(cx: &mut TestAppContext) {
    cx.update(bind_test_keymaps);
    let workspace = test_workspace(cx);
    feed_frame(&workspace, cx, agent(1), long_history());
    let editor = active_editor(&workspace, cx);

    cx.simulate_keystrokes(*workspace, "escape g g");
    cx.run_until_parked();

    let point_is_parsed = workspace
        .update(cx, |_, _, cx| {
            let point = editor.read(cx).selections.newest_anchor().head();
            let (buffer, _) = editor
                .read(cx)
                .buffer()
                .read(cx)
                .point_to_buffer_offset(point, cx)
                .expect("the point is in a buffer");
            !buffer.update(cx, |buffer, cx| buffer.ensure_syntax_parsed(cx))
        })
        .expect("read the buffer the point landed in");
    assert!(
        point_is_parsed,
        "the reader asked for the top and is looking at it: its syntax is parsed"
    );

    let (has_language, unparsed) = workspace
        .update(cx, |_, _, cx| {
            let far = multi_buffer::MultiBufferPoint::new(FAR_ROW, 0);
            let (buffer, _) = editor
                .read(cx)
                .buffer()
                .read(cx)
                .point_to_buffer_offset(far, cx)
                .expect("a row well below the window is composed");
            let has_language = buffer.read(cx).language().is_some();
            (
                has_language,
                buffer.update(cx, |buffer, cx| buffer.ensure_syntax_parsed(cx)),
            )
        })
        .expect("read a buffer the window does not show");
    assert!(
        has_language,
        "the far row has to be one with syntax to parse, or the check is vacuous"
    );
    assert!(
        unparsed,
        "a chunk composed for the page after this one is not parsed for this one"
    );
}
