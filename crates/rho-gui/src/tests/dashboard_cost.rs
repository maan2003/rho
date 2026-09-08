//! What one agent's news costs the map.
//!
//! An agent that is streaming says so constantly: every turn, every tool,
//! every title the model generates arrives as a `Changed` naming that agent
//! and no other. The desk's own delta path already answers such an event by
//! drawing the rows it named — `redraw_tree_rows` — and leaving the rest of
//! the map standing. The model-event path did not: it went through
//! `sync_tree_rows` to `refresh_dashboard` to `sync_tree`, which removes
//! every inlay on the tree and splices them all back, re-anchors every
//! excerpt and re-highlights every row, for one agent's news.
//!
//! That is what the user's telemetry caught. `InlayMap::splice` was most of
//! the main thread, and every sample of it arrived through this path. The
//! seek fix in 93af55e4 made each splice cheaper; it did not make the map
//! stop splicing itself whole, which is this test's subject.
//!
//! The assertions are shapes, not milliseconds. A composition either
//! happens or it does not, and the rows drawn again are counted, so this
//! says the same thing on any machine.

use rho_agents::HostId;
use rho_hosts::connection::ConnEvent;
use story::ready_with;

use super::{DeskFixture, agent, next_frame, overview_workspace, story, ui_head};

/// Files `count` agents under one heading and lets the map settle.
///
/// Returns the workspace and the agent ids, in filing order.
fn desk_of_agents(
    cx: &mut gpui::TestAppContext,
    count: u64,
) -> (
    gpui::WindowHandle<crate::workspace::Workspace>,
    Vec<rho_ui_proto::AgentId>,
) {
    let mut desk = DeskFixture::new();
    let parent = desk.note(None, "Desk");
    let agents = (1..=count).map(agent).collect::<Vec<_>>();
    for id in &agents {
        desk.agent_row(parent.clone(), *id);
    }

    let workspace = overview_workspace(cx);
    cx.run_until_parked();
    workspace
        .update(cx, |workspace, window, cx| {
            story::feed(workspace, HostId::default(), desk.synced(), window, cx);
            story::feed(
                workspace,
                HostId::default(),
                ready_with(
                    agents
                        .iter()
                        .enumerate()
                        .map(|(nth, id)| story::UiAgentHead {
                            generated_title: Some(format!("agent {nth}")),
                            ..ui_head(*id)
                        })
                        .collect(),
                    count * 10,
                ),
                window,
                cx,
            );
        })
        .expect("build the desk");
    cx.run_until_parked();
    // The map is composed on the next frame, not when the event arrives:
    // `schedule_desk_sync` defers to `on_next_frame`. A test that only
    // parks the executor measures an event nothing acted on.
    next_frame(cx, workspace);
    (workspace, agents)
}

/// One agent's title moves, on a map of `count` filed agents.
///
/// Returns what the event cost: how many times the dealer's source was
/// taken whole and how many times it was patched, how many source entries
/// the walk looked at, and how long the whole event took.
fn cost_of_one_agent_s_news(
    cx: &mut gpui::TestAppContext,
    count: u64,
) -> (usize, usize, usize, std::time::Duration) {
    let (workspace, agents) = desk_of_agents(cx, count);
    let (taken, patched) = workspace
        .update(cx, |workspace, _, _| {
            workspace.dashboard.deal_work_for_test()
        })
        .expect("read the dealer's work");

    // One event is a noisy thing to time, and a single one could be paid
    // for out of work the build left pending. A run of them, each a real
    // change so none is discarded as news that did not move anything, is
    // both a steadier number and a stronger question: cost must not
    // accumulate across events either.
    const EVENTS: u32 = 32;
    crate::desk_view::take_source_scans();
    let started = std::time::Instant::now();
    for nth in 0..EVENTS {
        workspace
            .update(cx, |workspace, window, cx| {
                story::feed(
                    workspace,
                    HostId::default(),
                    ConnEvent::Log {
                        entries: story::head_entries(story::UiAgentHead {
                            generated_title: Some(format!("renamed {nth}")),
                            ..ui_head(agents[2])
                        }),
                    },
                    window,
                    cx,
                );
            })
            .expect("one agent's title changes");
        cx.run_until_parked();
        next_frame(cx, workspace);
    }
    let took = started.elapsed() / EVENTS;
    let scans = crate::desk_view::take_source_scans() / EVENTS as usize;

    let (taken_after, patched_after) = workspace
        .update(cx, |workspace, _, _| {
            workspace.dashboard.deal_work_for_test()
        })
        .expect("read the dealer's work");
    (taken_after - taken, patched_after - patched, scans, took)
}

/// What one agent's news must cost, whatever the map's size.
///
/// The known-answer check comes first and is the reason this test can be
/// believed: a row must be drawn again. An event that reached nothing would
/// satisfy every other assertion here trivially, which is the way a test
/// like this is usually wrong.
fn assert_one_agent_s_news_is_cheap(cx: &mut gpui::TestAppContext, count: u64) {
    let (taken, patched, scans, took) = cost_of_one_agent_s_news(cx, count);
    eprintln!(
        "{count} agents: taken {taken}, patched {patched}, {scans} source \
         entries, {took:?} per event"
    );
    assert!(
        patched >= 32,
        "one agent's news patched nothing on a desk of {count} agents: the \
         event reached the dealer's source not at all, so this measured \
         nothing"
    );
    assert_eq!(
        taken, 0,
        "one agent's news took the whole desk of {count} agents again; the \
         nodes and their order did not move, so there was nothing to read \
         again"
    );
    assert!(
        patched <= 64,
        "32 events patched {patched} times on a desk of {count} agents; each \
         names one agent and must cost that agent and no more"
    );
    // The walk asks the sources for one agent, unit or page per node it
    // builds. Asking by scanning made a walk cost the nodes times the
    // sources, which at 128 agents was 16,384 entries for one agent's
    // news. An index makes each ask one entry, so the count is the nodes
    // and not their square. Counted rather than timed on purpose: at these
    // sizes the difference is microseconds and a clock would not see it,
    // which is exactly why it could come back unnoticed.
    assert!(scans > 0, "the walk did not run, so this measured nothing");
    assert!(
        scans <= count as usize * 2,
        "one agent's news looked at {scans} source entries on a desk of \
         {count} agents; a lookup that scans is back"
    );
}

#[gpui::test]
async fn one_agent_s_news_costs_its_own_row(cx: &mut gpui::TestAppContext) {
    assert_one_agent_s_news_is_cheap(cx, 16);
}

/// The same event on eight times the map. If what an event costs grew with
/// the agents standing still, this is where it would show.
#[gpui::test]
async fn one_agent_s_news_costs_its_own_row_at_scale(cx: &mut gpui::TestAppContext) {
    assert_one_agent_s_news_is_cheap(cx, 128);
}
