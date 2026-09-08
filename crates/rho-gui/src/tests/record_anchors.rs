//! Where a block ends, once nothing re-measures a rendered string to say so.
//!
//! A record's end anchor is where an elision stops hiding, so whatever
//! decides it decides which rows a settled turn keeps. These are the two
//! documents that put the decision under load: a block that grows with a
//! later block already standing after it, and a block that grows at exactly
//! the byte its own end anchor sits on.

use gpui::TestAppContext;

use super::{
    UiBlock, UiToolStatus, agent, assistant, buffer_text, feed_edit, feed_frame, state,
    stream_text, stream_tool_arguments, test_workspace, tool, user,
};

#[gpui::test]
fn a_tool_that_grows_under_a_later_tool_keeps_both_blocks_whole(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(
            vec![user("run both")],
            vec![
                UiBlock::Tool(tool("first", UiToolStatus::Running, None, None)),
                UiBlock::Tool(tool("second", UiToolStatus::Running, None, None)),
            ],
        ),
    );
    cx.run_until_parked();

    // The first tool's line grows. The second tool's block is unchanged and
    // sits directly after it, so this is an in-place edit of a block that is
    // not the last one.
    feed_edit(&workspace, cx, agent(1), |state| {
        stream_tool_arguments(state, 1, "echo ok".len(), " && sleep")
    });
    cx.run_until_parked();

    let text = buffer_text(&workspace, cx);
    assert!(
        text.contains("$ echo ok && sleep"),
        "the first tool's line should carry what it streamed: {text:?}"
    );
    assert_eq!(
        text.matches("$ echo ok").count(),
        2,
        "each tool keeps one line of its own, neither duplicated nor absorbed: {text:?}"
    );
}

/// The line that makes the end anchor's bias load-bearing. A block's last
/// byte is the newline the excerpt supplies, so its end anchor sits just
/// before it — and a second line arrives exactly there. A left-biased end
/// stays put and leaves the new line outside the block it belongs to; the
/// right bias carries it.
#[gpui::test]
fn a_second_line_arriving_at_the_blocks_end_lands_inside_it(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let first = "first line\n";
    feed_frame(
        &workspace,
        cx,
        agent(1),
        state(vec![user("go")], vec![assistant(first, None)]),
    );
    cx.run_until_parked();

    feed_edit(&workspace, cx, agent(1), |state| {
        stream_text(state, 1, first.len(), "second line\n")
    });
    cx.run_until_parked();

    let text = buffer_text(&workspace, cx);
    assert!(
        text.contains("first line\nsecond line\n"),
        "the streamed line should follow the first: {text:?}"
    );
}
