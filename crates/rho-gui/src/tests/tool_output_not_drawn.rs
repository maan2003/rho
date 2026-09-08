//! A tool call's line, and never what the tool said.
//!
//! The output body is out of the transcript: not drawn, not composed, not
//! asked for. The results are the bulk of a long transcript — 1.73 GiB of
//! them against a client mirror of 1028 MiB with the calls alone — and a
//! body that is not in the document cannot be mis-elided, mis-anchored or
//! fetched. What stays is the call: what ran, with what arguments, its
//! status and how long it took.
//!
//! These guard the two halves of that. Nothing is drawn under a call, and
//! composing a chunk full of finished calls sends no request.

use gpui::TestAppContext;
use rho_agents::HostId;
use rho_hosts::connection::ConnEvent;
use rho_ui_proto::ClientMessage;
use rho_ui_proto::mirror::{AgentPos, DetailBody, DetailResult, ToolStatus};

use super::{
    UiBlock, UiToolStatus, agent, display_text, feed_frame, state, story, test_workspace, tool,
    user,
};

/// A call that has finished, with its output left in the log at `pos`.
fn finished_call(id: &str, pos: u64) -> UiBlock {
    let mut called = tool(id, UiToolStatus::Success, Some(1_000), Some(1_200));
    called.result_at = Some(AgentPos(pos));
    UiBlock::Tool(called)
}

/// A call shows what was run and nothing under it, whether its output is
/// sitting in the log or not.
#[gpui::test]
fn a_call_shows_what_was_run_and_never_what_it_said(cx: &mut TestAppContext) {
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
        "both calls show what was run: {text:?}"
    );
    assert!(
        !text.contains("\n    "),
        "a body was drawn under a call: {text:?}"
    );
}

/// Composing calls whose results are in the log asks for nothing. This is
/// the half a reader cannot see: the transcript used to send one request
/// per composed chunk naming every position its calls came from, and a
/// reader scrolling history paid a round trip a chunk.
#[gpui::test]
fn composing_calls_with_results_in_the_log_asks_for_nothing(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let agent_id = agent(1);
    let calls = (0..8)
        .map(|index| finished_call(&format!("call-{index}"), 10 + index))
        .collect::<Vec<_>>();
    let mut blocks = vec![user("look")];
    blocks.extend(calls);
    feed_frame(&workspace, cx, agent_id, state(Vec::new(), blocks));

    let sent = workspace
        .update(cx, |workspace, _, _| {
            workspace.take_host_messages_for_test(HostId::default())
        })
        .expect("read what the client sent");
    let details = sent
        .iter()
        .filter(|message| matches!(message, ClientMessage::Detail { .. }))
        .count();
    assert_eq!(
        details, 0,
        "composing asked the daemon for bodies: {sent:?}"
    );
}

/// An answer nobody asked for changes nothing. A daemon may still hold the
/// bodies and a Detail frame may still arrive — from a client that asked
/// before this landed, or a daemon answering late — and it must not put
/// output back into the document.
#[gpui::test]
fn an_answer_nobody_asked_for_draws_nothing(cx: &mut TestAppContext) {
    let workspace = test_workspace(cx);
    let agent_id = agent(1);
    feed_frame(
        &workspace,
        cx,
        agent_id,
        state(Vec::new(), vec![user("look"), finished_call("call-one", 7)]),
    );

    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(
                workspace,
                HostId::default(),
                ConnEvent::Detail {
                    agent_id,
                    pos: AgentPos(7),
                    body: DetailBody::Results(vec![DetailResult {
                        id: "call-one".to_owned(),
                        status: ToolStatus::Success,
                        output: "the body nobody asked for".to_owned(),
                        error: None,
                    }]),
                },
                window,
                cx,
            );
        })
        .expect("deliver an unasked-for answer");
    cx.run_until_parked();

    let text = display_text(&workspace, cx);
    assert!(
        !text.contains("the body nobody asked for"),
        "an unasked-for answer was drawn: {text:?}"
    );
}
