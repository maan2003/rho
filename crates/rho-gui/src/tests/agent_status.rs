//! The words the activity view's status line says of the runtime.

use rho_agent_types::UnixMs;
use rho_agents_client::protocol::transcript::{InferenceState, RuntimeState};

fn label(runtime: &RuntimeState, now_ms: i64) -> Option<String> {
    let now = chrono::DateTime::from_timestamp_millis(now_ms)
        .unwrap()
        .fixed_offset();
    crate::attention::activity_state_label(runtime, now)
}

/// Waiting on the user, running tasks, a check-in, archiving and idling
/// are no words of the activity's: what wants the user is a mention of
/// them in a topic.
#[test]
fn an_agent_has_no_attention_state() {
    let now_ms = 1_757_000_000_000;
    for runtime in [
        RuntimeState::default(),
        RuntimeState {
            awaiting_human: true,
            running_tasks: 2,
            checkin_at: Some(UnixMs((now_ms + 60_000) as u64)),
            archived: true,
            ..Default::default()
        },
    ] {
        assert_eq!(label(&runtime, now_ms), None, "{runtime:?}");
    }
}

/// What the model is doing explains the activity, whatever else holds.
#[test]
fn inference_is_said_and_a_stale_build_with_it() {
    let now_ms = 1_757_000_000_000;
    let mut runtime = RuntimeState {
        inference: InferenceState::Retrying {
            at: UnixMs((now_ms + 60_000) as u64),
            error: "private provider details".into(),
        },
        awaiting_human: true,
        running_tasks: 2,
        archived: true,
        ..Default::default()
    };
    assert_eq!(label(&runtime, now_ms).as_deref(), Some("retrying in 1m"));
    runtime.inference = InferenceState::Failed {
        error: "private provider details".into(),
    };
    assert_eq!(label(&runtime, now_ms).as_deref(), Some("errored"));
    runtime.inference = InferenceState::Responding;
    runtime.stale = true;
    assert_eq!(
        label(&runtime, now_ms).as_deref(),
        Some("responding · old build")
    );
    runtime.inference = InferenceState::Idle;
    assert_eq!(label(&runtime, now_ms).as_deref(), Some("old build"));
}
