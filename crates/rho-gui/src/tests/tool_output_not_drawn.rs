//! A tool call's line, and never what the tool said.
//!
//! The output body is out of the transcript: not drawn, not composed, not
//! asked for. The results are the bulk of a long transcript — 1.73 GiB of
//! them against a client mirror of 1028 MiB with the calls alone — and a
//! body that is not in the document cannot be mis-elided, mis-anchored or
//! fetched. What stays is the call: what ran, with what arguments, its
//! status and how long it took.
//!
//! Nothing is drawn under a call. Nothing asks for a body either, and
//! nothing could: detail frames stop at `rho-hosts`, and the agents client
//! has no way to ask for one.

use gpui::TestAppContext;

use super::{
    UiBlock, UiToolStatus, agent, display_text, feed_frame, state, test_workspace, tool, user,
};

/// A call that has finished. Its output is in the daemon's log; nothing in
/// the transcript records where, because nothing goes looking.
fn finished_call(id: &str) -> UiBlock {
    UiBlock::Tool(tool(id, UiToolStatus::Success, Some(1_000), Some(1_200)))
}

/// A call shows what was run and nothing under it, whether its output is
/// sitting in the log or not.
#[gpui::test]
fn a_call_shows_what_was_run_and_never_what_it_said(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let agent_id = agent(1);
    let running = tool("call-one", UiToolStatus::Running, Some(1_000), None);
    feed_frame(
        &workspace,
        cx,
        agent_id,
        state(
            Vec::new(),
            vec![
                user("look"),
                UiBlock::Tool(running),
                finished_call("call-two"),
            ],
        ),
    );

    let text = display_text(&workspace, cx);
    assert_eq!(
        text.matches("$ echo ok").count(),
        2,
        "both calls show what was run: {text:?}"
    );
    assert!(
        !text.contains("\n    "),
        "a body was drawn under a call: {text:?}"
    );
}
