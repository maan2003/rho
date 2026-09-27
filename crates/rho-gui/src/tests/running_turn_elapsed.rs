//! The duration a running row prints comes from the turn's own start.
//!
//! Home read the time since the user last spoke instead, which is a
//! different quantity even when it has one: an agent on its fifth turn
//! showed how long ago the user talked, and an agent the user never
//! messaged showed a turn running since the epoch, "20704.2d".

use rho_agent_types::UnixMs;
use rho_agents_client::AgentFacts;

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

#[test]
fn notebook_status_does_not_treat_a_message_as_a_finished_turn() {
    let now_ms = 1_757_000_000_000;
    let now = chrono::DateTime::from_timestamp_millis(now_ms)
        .unwrap()
        .fixed_offset();
    let mut facts = AgentFacts {
        notebook: Some(rho_agents_client::state::UiNotebookActivity {
            responding: false,
            running_tasks: 2,
            checkin_at: Some(UnixMs((now_ms + 60_000) as u64)),
            archived: false,
        }),
        turn_running: true,
        last_message_sent: Some(UnixMs((now_ms - 120_000) as u64)),
        ..AgentFacts::default()
    };
    assert_eq!(
        crate::attention::agent_state_label(&facts, now).as_deref(),
        Some("2 running tasks")
    );
    facts.awaiting_human = Some(UnixMs((now_ms - 60_000) as u64));
    assert_eq!(
        crate::attention::agent_state_label(&facts, now).as_deref(),
        Some("waiting on you · 1m · 2 running tasks")
    );
    facts.awaiting_human = None;
    facts.notebook.as_mut().unwrap().running_tasks = 0;
    facts.notebook.as_mut().unwrap().archived = true;
    assert_eq!(
        crate::attention::agent_state_label(&facts, now).as_deref(),
        Some("archived")
    );
    facts.notebook.as_mut().unwrap().archived = false;
    assert_eq!(
        crate::attention::agent_state_label(&facts, now).as_deref(),
        Some("next check-in in 1m")
    );
}
