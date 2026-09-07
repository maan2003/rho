//! What a tool call said, drawn under the call, and only for the calls on
//! the screen.
//!
//! The story carries every call and its arguments but never its output:
//! the results are the bulk of a long transcript and most of them are
//! never read. So a chunk, as it composes, asks the daemon for the bodies
//! at the positions its calls name, and draws each answer under the call
//! that asked. Nothing is held for a call that is not composed, and an
//! answer for one that has gone is dropped.

use gpui::TestAppContext;
use rho_agents::HostId;
use rho_hosts::connection::ConnEvent;
use rho_ui_proto::mirror::{AgentPos, DetailBody, DetailResult, ToolStatus};

use super::{
    UiBlock, UiToolStatus, agent, assistant, display_text, feed_frame, state, story,
    test_workspace, tool, user,
};

/// A call that has finished, with its output left in the log at `pos`.
fn finished_call(id: &str, pos: u64) -> UiBlock {
    let mut called = tool(id, UiToolStatus::Success, Some(1_000), Some(1_200));
    called.result_at = Some(AgentPos(pos));
    UiBlock::Tool(called)
}

/// One answer, as the daemon sends it.
fn answer(
    workspace: &gpui::WindowHandle<crate::workspace::Workspace>,
    cx: &mut TestAppContext,
    agent_id: rho_ui_proto::AgentId,
    pos: u64,
    results: Vec<DetailResult>,
) {
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::Detail {
                    agent_id,
                    pos: AgentPos(pos),
                    body: DetailBody::Results(results),
                },
                window,
                cx,
            );
        })
        .expect("deliver the bodies the chunk asked for");
    cx.run_until_parked();
}

fn result(id: &str, output: &str, error: Option<&str>) -> DetailResult {
    DetailResult {
        id: id.to_owned(),
        status: if error.is_some() {
            ToolStatus::Error
        } else {
            ToolStatus::Success
        },
        output: output.to_owned(),
        error: error.map(str::to_owned),
    }
}

/// The answer is drawn under the call that asked for it, and under that
/// call only.
#[gpui::test]
fn a_composed_chunks_calls_show_their_results_when_the_answer_arrives(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let agent_id = agent(1);
    feed_frame(
        &workspace,
        cx,
        agent_id,
        state(
            Vec::new(),
            vec![
                user("what is in the directory"),
                finished_call("call-one", 7),
                finished_call("call-two", 7),
                assistant("two files", None),
            ],
        ),
    );

    let before = display_text(&workspace, cx);
    assert!(
        before.contains("echo ok"),
        "the call and its arguments come with the story: {before:?}"
    );
    assert!(
        !before.contains("one.txt"),
        "and nothing of what it said, until the body is asked for and answered: {before:?}"
    );

    answer(
        &workspace,
        cx,
        agent_id,
        7,
        vec![
            result("call-one", "one.txt\ntwo.txt", None),
            result("call-two", "", Some("no such file")),
        ],
    );

    let after = display_text(&workspace, cx);
    assert!(
        after.contains("    one.txt\n    two.txt"),
        "the output is drawn under its call, indented so the call still reads as the call: {after:?}"
    );
    assert!(
        after.contains("    no such file"),
        "and what a failing call said is drawn the same way: {after:?}"
    );
    assert!(
        after.contains("two files"),
        "the rest of the chunk is where it was: {after:?}"
    );
}

/// An answer for a chunk that is no longer composed changes nothing. The
/// chunk asks again when it is composed again, so there is nothing to
/// hold and nothing to reconcile.
#[gpui::test]
fn an_answer_for_a_chunk_that_went_away_splices_nothing(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let agent_id = agent(1);
    feed_frame(
        &workspace,
        cx,
        agent_id,
        state(Vec::new(), vec![user("look"), finished_call("call-one", 7)]),
    );

    // The turn is rebuilt without the call, as a daemon that saw the turn
    // differently would send it, while the answer is still in flight.
    feed_frame(
        &workspace,
        cx,
        agent_id,
        state(
            Vec::new(),
            vec![user("look"), assistant("never mind", None)],
        ),
    );
    let before = display_text(&workspace, cx);

    answer(
        &workspace,
        cx,
        agent_id,
        7,
        vec![result("call-one", "one.txt", None)],
    );

    let after = display_text(&workspace, cx);
    assert_eq!(
        after, before,
        "an answer nobody is waiting for is dropped, not spliced"
    );
    assert!(
        !after.contains("one.txt"),
        "and nothing of it reaches the screen: {after:?}"
    );
}

/// A call whose body never comes still shows what was called and with
/// what. This is what a daemon too old to answer leaves on the screen, and
/// what a call still running looks like: the one line and no more.
#[gpui::test]
fn a_call_whose_body_never_comes_still_shows_what_was_called(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let agent_id = agent(1);
    let mut running = tool("call-one", UiToolStatus::Running, Some(1_000), None);
    running.result_at = None;
    feed_frame(
        &workspace,
        cx,
        agent_id,
        state(
            Vec::new(),
            vec![
                user("look"),
                UiBlock::Tool(running),
                finished_call("call-two", 7),
            ],
        ),
    );

    let text = display_text(&workspace, cx);
    assert_eq!(
        text.matches("$ echo ok").count(),
        2,
        "both calls show what was run, answered or not: {text:?}"
    );
    assert!(
        !text.contains("\n    "),
        "and neither draws a body under it: {text:?}"
    );
}
