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

/// A row near the top of the opening tail: two hundred rows are composed
/// when a transcript opens and the reader is at the bottom of them, so
/// this one is a screen and a half above anything drawn.
const TOP_OF_THE_OPENING_TAIL: u32 = 5;

/// Opening parses the screen the reader opens on, and not the two hundred
/// rows composed behind it.
///
/// The open is the one place the editor cannot parse what it draws for
/// itself: it has not laid out yet, so it can see nothing and parses
/// nothing. The transcript warms a window's rows from the tail instead of
/// every buffer it composed.
#[gpui::test]
fn opening_parses_the_screen_it_opens_on(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(&workspace, cx, agent(1), long_history());
    let editor = active_editor(&workspace, cx);

    let (tail_has_language, tail_unparsed, top_has_language, top_unparsed) = workspace
        .update(cx, |_, _, cx| {
            let multi_buffer = editor.read(cx).buffer().clone();
            let last_row = multi_buffer.read(cx).snapshot(cx).max_point().row;
            let mut answer = Vec::new();
            for row in [last_row.saturating_sub(2), TOP_OF_THE_OPENING_TAIL] {
                let point = multi_buffer::MultiBufferPoint::new(row, 0);
                let (buffer, _) = multi_buffer
                    .read(cx)
                    .point_to_buffer_offset(point, cx)
                    .expect("a composed row");
                answer.push(buffer.read(cx).language().is_some());
                answer.push(buffer.update(cx, |buffer, cx| buffer.ensure_syntax_parsed(cx)));
            }
            (answer[0], answer[1], answer[2], answer[3])
        })
        .expect("read the two ends of the opening tail");

    assert!(
        tail_has_language && top_has_language,
        "both rows have to hold syntax, or the questions are vacuous"
    );
    assert!(
        !tail_unparsed,
        "the transcript opens on its tail and the tail is parsed for it"
    );
    assert!(
        top_unparsed,
        "the rows composed behind the screen are parsed when the reader \
         reaches them, not to open"
    );
}

/// Two hundred turns whose last answer is `tail`, so feeding two of these
/// in a row changes one turn's text and nothing else.
fn history_with_tail(tail: &str) -> super::UiAgentState {
    let mut blocks = Vec::new();
    for turn in 0..199 {
        blocks.push(super::user(&format!("ask {turn}")));
        blocks.push(super::assistant(
            &format!("turn {turn} line one\nturn {turn} line two\n"),
            Some(super::UiMessagePhase::FinalAnswer),
        ));
    }
    blocks.push(super::user("ask about the tail"));
    blocks.push(super::assistant(
        tail,
        Some(super::UiMessagePhase::FinalAnswer),
    ));
    super::state(blocks, Vec::new())
}

/// An answer that grows replaces the buffer it lands in, and the
/// replacement is parsed in the frame that made it.
///
/// A new buffer has no syntax until something asks. If the asking is left
/// to the editor it arrives 50 ms later, on the debounced scroll the
/// replacement itself caused: the markup a parse conceals is on screen
/// until then and the text shifts under the reader on a timer, which is
/// also a scene that changes with the clock outside any live cadence.
#[gpui::test]
fn a_replaced_tail_buffer_is_parsed_in_the_frame_that_made_it(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        history_with_tail("tail line one\n"),
    );
    let editor = active_editor(&workspace, cx);

    feed_frame(
        &workspace,
        cx,
        agent(1),
        history_with_tail("tail line one\ntail line two with `code`\n"),
    );

    let (has_language, unparsed) = workspace
        .update(cx, |_, _, cx| {
            let multi_buffer = editor.read(cx).buffer().clone();
            let last_row = multi_buffer.read(cx).snapshot(cx).max_point().row;
            let point = multi_buffer::MultiBufferPoint::new(last_row.saturating_sub(1), 0);
            let (buffer, _) = multi_buffer
                .read(cx)
                .point_to_buffer_offset(point, cx)
                .expect("the grown answer is composed");
            let has_language = buffer.read(cx).language().is_some();
            (
                has_language,
                buffer.update(cx, |buffer, cx| buffer.ensure_syntax_parsed(cx)),
            )
        })
        .expect("read the buffer the grown answer landed in");

    assert!(
        has_language,
        "the grown answer has to hold syntax, or the question is vacuous"
    );
    assert!(
        !unparsed,
        "the frame that replaced the buffer is the frame that draws it: \
         its syntax is parsed there, not on a timer afterwards"
    );
}
