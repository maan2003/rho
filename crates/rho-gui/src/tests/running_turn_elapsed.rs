//! The duration a running row prints comes from the turn's own start.
//!
//! Home read the time since the user last spoke instead, which is a
//! different quantity even when it has one: an agent on its fifth turn
//! showed how long ago the user talked, and an agent the user never
//! messaged showed a turn running since the epoch, "20704.2d".

use rho_agents::AgentFacts;
use rho_core::UnixMs;

use crate::home::running_elapsed_label;

#[test]
fn a_running_turn_is_timed_from_when_it_started() {
    let now_ms = 1_757_000_000_000;
    let facts = AgentFacts {
        turn_running: true,
        turn_started_at: Some(UnixMs((now_ms - 12 * 60_000) as u64)),
        // Days older than the turn, and the row must not read from it.
        last_user_message_at: UnixMs((now_ms - 3 * 86_400_000) as u64),
        ..AgentFacts::default()
    };
    assert_eq!(running_elapsed_label(&facts, now_ms), "12m");
}

#[test]
fn a_turn_with_no_start_prints_no_duration() {
    let now_ms = 1_757_000_000_000;
    let facts = AgentFacts {
        turn_running: true,
        turn_started_at: None,
        // The default an agent nobody messaged keeps. Printed as a start
        // it read as fifty-six years.
        last_user_message_at: UnixMs::default(),
        ..AgentFacts::default()
    };
    assert_eq!(
        running_elapsed_label(&facts, now_ms),
        "",
        "a row with no start has nothing to say about how long"
    );
}
