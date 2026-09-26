//! When to wake the model: the one decision, as a plain function of facts.
//!
//! Every wake is a request, so the model is woken only for things it can act
//! on: somebody wrote, its latest cell returned, a cell called `notify()`, a
//! command or host call ended, or its check-in fell due. Output alone rides
//! along with whatever wakes it next. A short patience lets things that
//! happen together arrive together.

use std::time::Duration;

use rho_agent_types::UnixMs;

use crate::log::Wake;

pub const MESSAGE_PATIENCE: Duration = Duration::from_millis(500);
pub const RETURN_PATIENCE: Duration = Duration::from_millis(200);
pub const NOTIFY_PATIENCE: Duration = Duration::from_secs(1);
pub const ENDED_PATIENCE: Duration = Duration::from_secs(1);
/// The check-in when the model set none and is not waiting on the human.
pub const DEFAULT_CHECKIN: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// The oldest message the model has not seen.
    pub message: Option<UnixMs>,
    /// The latest cell's code returned, and the model has not been told.
    pub returned: Option<UnixMs>,
    /// The oldest unsent `notify()`.
    pub notified: Option<UnixMs>,
    /// The earliest unreported end of a command or host call.
    pub ended: Option<UnixMs>,
    /// When the check-in falls due, if there is one.
    pub checkin: Option<UnixMs>,
    /// `suppress_tool_wakeups()`: only messages and the check-in wake.
    pub wake_on_tools: bool,
    /// The last step made no call; it is told so at once.
    pub prose: bool,
    pub restarted: bool,
    /// After repeated failures, only a message wakes.
    pub stopped: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Now(Wake),
    /// Not yet; look again at this instant, or on the next event.
    Later(Option<UnixMs>),
}

pub fn decide(facts: &Facts, now: UnixMs) -> Decision {
    let mut due: Vec<(UnixMs, Wake)> = Vec::new();
    if let Some(at) = facts.message {
        due.push((at + MESSAGE_PATIENCE, Wake::Message));
    }
    if !facts.stopped {
        if facts.restarted {
            due.push((now, Wake::Restarted));
        }
        if facts.prose {
            due.push((now, Wake::Prose));
        }
        if facts.wake_on_tools {
            if let Some(at) = facts.returned {
                due.push((at + RETURN_PATIENCE, Wake::Returned));
            }
            if let Some(at) = facts.notified {
                due.push((at + NOTIFY_PATIENCE, Wake::Notify));
            }
            if let Some(at) = facts.ended {
                due.push((at + ENDED_PATIENCE, Wake::Ended));
            }
        }
        if let Some(at) = facts.checkin {
            due.push((at, Wake::Checkin));
        }
    }
    // Earliest first; on a tie, the order pushed above.
    match due.into_iter().min_by_key(|(at, _)| *at) {
        Some((at, why)) if at <= now => Decision::Now(why),
        Some((at, _)) => Decision::Later(Some(at)),
        None => Decision::Later(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            wake_on_tools: true,
            ..Facts::default()
        }
    }

    #[test]
    fn nothing_pending_waits_for_an_event() {
        assert_eq!(decide(&facts(), UnixMs(10)), Decision::Later(None));
    }

    #[test]
    fn a_message_waits_briefly_for_company() {
        let facts = Facts {
            message: Some(UnixMs(1000)),
            ..facts()
        };
        assert_eq!(
            decide(&facts, UnixMs(1000)),
            Decision::Later(Some(UnixMs(1500)))
        );
        assert_eq!(decide(&facts, UnixMs(1500)), Decision::Now(Wake::Message));
    }

    #[test]
    fn suppressed_tools_leave_messages_and_the_checkin() {
        let facts = Facts {
            wake_on_tools: false,
            notified: Some(UnixMs(0)),
            ended: Some(UnixMs(0)),
            checkin: Some(UnixMs(5000)),
            ..facts()
        };
        assert_eq!(
            decide(&facts, UnixMs(4000)),
            Decision::Later(Some(UnixMs(5000)))
        );
        assert_eq!(decide(&facts, UnixMs(5000)), Decision::Now(Wake::Checkin));
    }

    #[test]
    fn a_stopped_agent_wakes_only_for_a_message() {
        let facts = Facts {
            stopped: true,
            prose: true,
            checkin: Some(UnixMs(0)),
            ..facts()
        };
        assert_eq!(decide(&facts, UnixMs(10)), Decision::Later(None));
    }
}
