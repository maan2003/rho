//! The state words an agent's status line says.

use rho_agent_types::UnixMs;
use rho_agents_client::AgentFacts;

#[test]
fn notebook_status_does_not_treat_a_message_as_a_finished_turn() {
    let now_ms = 1_757_000_000_000;
    let now = chrono::DateTime::from_timestamp_millis(now_ms)
        .unwrap()
        .fixed_offset();
    let mut facts = AgentFacts {
        runtime: Some(rho_agents_client::protocol::transcript::RuntimeState {
            inference: Default::default(),
            awaiting_human: false,
            running_tasks: 2,
            checkin_at: Some(UnixMs((now_ms + 60_000) as u64)),
            archived: false,
            stale: false,
        }),
        turn_running: true,
        last_message_sent: Some(UnixMs((now_ms - 120_000) as u64)),
        ..AgentFacts::default()
    };
    // Running tasks are the agent's business; the check-in is the reader's.
    assert_eq!(
        crate::attention::agent_state_label(&facts, now).as_deref(),
        Some("next check-in in 1m")
    );
    facts.runtime.as_mut().unwrap().awaiting_human = true;
    assert_eq!(
        crate::attention::agent_state_label(&facts, now).as_deref(),
        Some("waiting on you")
    );
    facts.runtime.as_mut().unwrap().awaiting_human = false;
    facts.awaiting_human = Some(UnixMs((now_ms - 60_000) as u64));
    facts.runtime.as_mut().unwrap().awaiting_human = true;
    assert_eq!(
        crate::attention::agent_state_label(&facts, now).as_deref(),
        Some("waiting on you · 1m")
    );
    facts.awaiting_human = None;
    facts.runtime.as_mut().unwrap().awaiting_human = false;
    facts.runtime.as_mut().unwrap().running_tasks = 0;
    facts.runtime.as_mut().unwrap().archived = true;
    assert_eq!(
        crate::attention::agent_state_label(&facts, now).as_deref(),
        Some("archived")
    );
    facts.runtime.as_mut().unwrap().archived = false;
    assert_eq!(
        crate::attention::agent_state_label(&facts, now).as_deref(),
        Some("next check-in in 1m")
    );
}

#[test]
fn retry_and_failure_status_override_archived_or_running_tasks() {
    use rho_agents_client::protocol::transcript::{InferenceState, RuntimeState};
    let now_ms = 1_757_000_000_000;
    let now = chrono::DateTime::from_timestamp_millis(now_ms)
        .unwrap()
        .fixed_offset();
    let mut facts = AgentFacts {
        runtime: Some(RuntimeState {
            inference: InferenceState::Retrying {
                at: UnixMs((now_ms + 60_000) as u64),
                error: "private provider details".into(),
            },
            running_tasks: 2,
            archived: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        crate::attention::agent_state_label(&facts, now).as_deref(),
        Some("retrying in 1m")
    );
    facts.runtime.as_mut().unwrap().inference = InferenceState::Failed {
        error: "private provider details".into(),
    };
    assert_eq!(
        crate::attention::agent_state_label(&facts, now).as_deref(),
        Some("errored")
    );
}

#[test]
fn a_stale_workset_is_told_whatever_the_state() {
    use rho_agents_client::protocol::transcript::RuntimeState;
    let now = chrono::DateTime::from_timestamp_millis(1_757_000_000_000)
        .unwrap()
        .fixed_offset();
    let mut facts = AgentFacts {
        runtime: Some(RuntimeState {
            awaiting_human: true,
            stale: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        crate::attention::agent_state_label(&facts, now).as_deref(),
        Some("waiting on you · old build")
    );
    facts.runtime.as_mut().unwrap().awaiting_human = false;
    assert_eq!(
        crate::attention::agent_state_label(&facts, now).as_deref(),
        Some("idle · old build")
    );
    facts.runtime.as_mut().unwrap().stale = false;
    assert_eq!(
        crate::attention::agent_state_label(&facts, now).as_deref(),
        Some("idle")
    );
}
