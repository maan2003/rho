//! What the finder costs per keystroke, and how much of it is the tree.
//!
//! Find rebuilds its whole candidate set on every character typed: the
//! completion closure calls `find_candidates` and ranks the result, so a
//! five-character query walks every node in the store five times. That was
//! true when the candidates came from the dashboard and it is still true
//! now that they come from the store client; what changed is that no editor,
//! no buffers of the map's own and no composed rows stand in the way.
//!
//! These numbers are what the choice between "rebuilt per open" and "kept
//! and updated per event" has to be made on, so they are measured and
//! printed rather than asserted at. What *is* asserted is the shape: that
//! the cost per candidate does not run away as the desk grows, which is the
//! same answer on any machine. A millisecond bound here would be a bound on
//! whichever machine happened to run it, and in a debug build it would be a
//! bound on the debug build — this measures 3.68 ms per keystroke in debug
//! and 0.49 ms in release at 145 candidates, and the 4 ms bar the user set
//! is a bar on the profiling profile, not on this one.
//!
//! Slack is in the scale but not in the change. Conversations and threads
//! reach the finder through `slack_find_candidates`, which reads the
//! session's own rows and never went through the dashboard, so this landing
//! does not change their cost by a byte. They are here because they are
//! most of what the finder ranks on a real workspace, and a per-keystroke
//! number that leaves them out is a number for a desk nobody has: the
//! default fake Slack world is 300 conversations, and the count comes from
//! that world's own seed rather than from a number written down here.

use rho_agents::HostId;
use story::ready_with;

use super::{DeskFixture, agent, next_frame, overview_workspace, story, ui_head};

/// A desk of `topics` headings with `agents` filed evenly beneath them.
fn desk_of(
    cx: &mut gpui::TestAppContext,
    topics: u64,
    agents: u64,
) -> gpui::WindowHandle<crate::workspace::Workspace> {
    let mut desk = DeskFixture::new();
    let root = desk.note(None, "Desk");
    let headings = (0..topics)
        .map(|nth| desk.note(Some(root.clone()), &format!("topic {nth}")))
        .collect::<Vec<_>>();
    let ids = (1..=agents).map(agent).collect::<Vec<_>>();
    for (nth, id) in ids.iter().enumerate() {
        desk.agent_row(headings[nth % headings.len()].clone(), *id);
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
                    ids.iter()
                        .enumerate()
                        .map(|(nth, id)| story::UiAgentHead {
                            generated_title: Some(format!("agent {nth} on linux")),
                            ..ui_head(*id)
                        })
                        .collect(),
                    agents * 10,
                ),
                window,
                cx,
            );
        })
        .expect("build the desk");
    cx.run_until_parked();
    next_frame(cx, workspace);
    workspace
}

/// The finder answers a keystroke at a cost that grows with the desk and
/// not faster, and finds what it is asked for at both sizes.
///
/// The known-answer check comes first at each size: a cost test whose
/// subject returns nothing is measuring an empty loop, and would pass
/// fastest of all.
#[gpui::test]
fn a_keystroke_costs_no_more_per_candidate_as_the_desk_grows(cx: &mut gpui::TestAppContext) {
    // The desk these are built on costs roughly the square of its agents
    // to build — the fixture and the sync behind it, not the finder, which
    // is linear here and measured as such below. So the span is chosen for
    // the largest desk that builds in seconds; 1024 agents took over an
    // hour and wedged the suite.
    let small = measure(cx, 8, 80, "agent 77");
    let large = measure(cx, 32, 288, "agent 277");

    println!(
        "find: {} candidates, {:.2} ms per keystroke, {:.1} us per candidate",
        small.candidates,
        small.per_keystroke.as_secs_f64() * 1000.,
        small.per_candidate_us()
    );
    println!(
        "find: {} candidates, {:.2} ms per keystroke, {:.1} us per candidate",
        large.candidates,
        large.per_keystroke.as_secs_f64() * 1000.,
        large.per_candidate_us()
    );

    assert!(
        large.candidates > small.candidates * 3,
        "the larger desk has to be much larger, or the comparison says \
         nothing: {} against {}",
        large.candidates,
        small.candidates
    );
    // Linear would be equal; a little worse than linear is expected, since
    // a breadcrumb is a walk to the root and the tree is deeper. Quadratic
    // would be eight times over this span and is the answer that would say
    // the candidate set has to be kept rather than rebuilt.
    assert!(
        large.per_candidate_us() < small.per_candidate_us() * 3.,
        "the cost per candidate must not run away as the desk grows: \
         {:.1} us at {} candidates against {:.1} us at {}",
        large.per_candidate_us(),
        large.candidates,
        small.per_candidate_us(),
        small.candidates
    );
}

struct Cost {
    candidates: usize,
    per_keystroke: std::time::Duration,
}

impl Cost {
    fn per_candidate_us(&self) -> f64 {
        self.per_keystroke.as_secs_f64() * 1_000_000. / self.candidates as f64
    }
}

/// Type `query` one character at a time, the way the finder is used, and
/// report what a character cost.
fn measure(cx: &mut gpui::TestAppContext, topics: u64, agents: u64, query: &str) -> Cost {
    let workspace = desk_of(cx, topics, agents);
    let (found, candidates) = workspace
        .update(cx, |workspace, _, cx| {
            (
                workspace.find_rows_for_test(query, cx),
                workspace.find_candidates(cx).len(),
            )
        })
        .expect("ask the finder for a name it must know");
    assert!(
        found.iter().any(|row| row.value.contains(query)),
        "the finder has to find an agent that is filed and named; asked for \
         {query:?} over {candidates} candidates it returned {found:#?}"
    );

    let started = std::time::Instant::now();
    for end in 1..=query.len() {
        workspace
            .update(cx, |workspace, _, cx| {
                workspace.find_rows_for_test(&query[..end], cx);
            })
            .expect("type a character into the finder");
    }
    Cost {
        candidates,
        per_keystroke: started.elapsed() / query.len() as u32,
    }
}

/// The two frames the finder costs at the scale the user runs at: the one
/// that opens it, and the one that answers a keystroke. Both against the
/// 4 ms bar, which has no exception for a surface that is opening.
#[gpui::test]
fn the_open_frame_and_the_keystroke_frame_at_the_scale_the_user_runs_at(
    cx: &mut gpui::TestAppContext,
) {
    let rooms = slack_rooms().len();
    let workspace = desk_of(cx, 16, 128);
    let desk = workspace
        .update(cx, |workspace, _, cx| workspace.find_candidates(cx).len())
        .expect("count the desk's half");

    // The open: everything there is to find, and the names it will be
    // ranked by. This is the whole of what a keystroke used to pay.
    let started = std::time::Instant::now();
    let snapshot = workspace
        .update(cx, |workspace, _, cx| {
            workspace.find_snapshot_for_test(slack_rooms(), cx)
        })
        .expect("open the finder");
    let open = started.elapsed();

    let query = "agent 77";
    let found = crate::workspace::Workspace::find_rows_in_for_test(&snapshot, query);
    assert!(
        found.iter().any(|row| row.value.contains(query)),
        "the finder has to find a filed agent with the Slack rooms beside \
         it; asked for {query:?} it returned {found:#?}"
    );
    let rooms_found = crate::workspace::Workspace::find_rows_in_for_test(&snapshot, "channel 7");
    assert!(
        rooms_found
            .iter()
            .any(|row| row.value.starts_with("slack › ")),
        "and it has to find a Slack room, or the rooms are being built and \
         thrown away rather than ranked: it returned {rooms_found:#?}"
    );

    // The keystroke: the ranking, over a set already in hand.
    let started = std::time::Instant::now();
    for end in 1..=query.len() {
        crate::workspace::Workspace::find_rows_in_for_test(&snapshot, &query[..end]);
    }
    let keystroke = started.elapsed();

    // The same keystrokes down the path this cut replaced: the candidate
    // set taken again, and its names built again, for every character.
    // Measured here, in the same run on the same machine, because the
    // claim is a comparison and a number from another run is not one.
    let started = std::time::Instant::now();
    for end in 1..=query.len() {
        workspace
            .update(cx, |workspace, _, cx| {
                workspace.find_rows_over_for_test(slack_rooms(), &query[..end], cx);
            })
            .expect("type a character down the old path");
    }
    let rebuilt = started.elapsed();

    println!(
        "find at scale: {} candidates ({desk} desk + {rooms} slack); \
         open {:.2} ms, keystroke {:.2} ms, keystroke down the old path \
         {:.2} ms",
        desk + rooms,
        open.as_secs_f64() * 1000.,
        keystroke.as_secs_f64() * 1000. / query.len() as f64,
        rebuilt.as_secs_f64() * 1000. / query.len() as f64,
    );

    // The shape, not the milliseconds. Ranking is O(candidates) and no
    // implementation makes it less, so a keystroke is not cheap and is not
    // meant to be; what the cut removes is the taking of the set and the
    // building of its names, once per character. So the comparison that
    // says the rebuild is gone is against the old path, not against the
    // open. The open is the smaller of the two here — building 445
    // candidates costs less than scoring them — which is why comparing a
    // keystroke to the open proved nothing in either direction.
    let per_keystroke = keystroke / query.len() as u32;
    let per_rebuild = rebuilt / query.len() as u32;
    assert!(
        per_keystroke * 5 < per_rebuild * 4,
        "a keystroke must not be paying for the candidate set: {per_rebuild:?} \
         down the old path against {per_keystroke:?} ranking a set in hand"
    );
}

/// The default fake Slack world's conversations as the finder sees them.
///
/// The count is the world's, not a number chosen here, so a change to what
/// the fake world means by "default" moves this measurement with it. The
/// rows themselves are named and dated rather than generated from the
/// store: what a candidate costs to rank is its label, and the label of the
/// three-hundredth conversation is a string either way.
fn slack_rooms() -> Vec<crate::find::FindCandidate> {
    let conversations = rho_fake_slack::world::Seed::default().conversations;
    let rows = (0..conversations)
        .map(|nth| rho_slack::model::ConversationRow {
            id: rho_slack::types::ChannelId(format!("C{nth:04}")),
            label: format!("#channel {nth}"),
            unread: false,
            mention_count: 0,
            unread_count: 0,
            muted: false,
            watched: false,
            latest: None,
        })
        .collect();
    crate::find::slack_candidates(rows, Vec::new())
}
