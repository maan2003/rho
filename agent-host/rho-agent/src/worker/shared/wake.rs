//! The scheduling decision is pure: all deadlines are anchored to response
//! completion.
use std::time::Duration;

use rho_agent_types::UnixMs;

use crate::entry::Wake;

pub(crate) const DEFAULT_CHECKIN: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    pub human: Option<UnixMs>,
    pub agent: Option<UnixMs>,
    pub finished: Option<UnixMs>,
    pub notified: Option<UnixMs>,
    pub failure: Option<UnixMs>,
    pub checkin: Option<UnixMs>,
    pub response_finished: Option<UnixMs>,
    pub prose: bool,
    pub rewound: bool,
    pub compaction: bool,
    pub compaction_reply: bool,
    pub archived: bool,
    pub prose_silenced: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Now(Wake),
    Later(Option<UnixMs>),
}

pub(crate) fn decide(facts: &Facts, now: UnixMs) -> Decision {
    if facts.archived {
        return Decision::Later(None);
    }
    if facts.compaction_reply {
        return Decision::Now(Wake::CompactionReply);
    }
    if facts.compaction {
        return Decision::Now(Wake::Compaction);
    }
    // Each event waits only for the time after the last response: that
    // time lets the cell the response started finish and go with it. An
    // event long after the response goes at once.
    let after_response = |at: UnixMs, wait: Duration| {
        facts
            .response_finished
            .map_or(at, |finished| at.max(finished + wait))
    };
    let human = facts
        .human
        .map(|at| after_response(at, Duration::from_secs(2)));
    let mut due = Vec::new();
    if let Some(at) = human {
        due.push((at, Wake::Message));
    }
    if facts.prose_silenced {
        return match human {
            Some(at) if at <= now => Decision::Now(Wake::Message),
            at => Decision::Later(at),
        };
    }
    if let Some(at) = facts.agent {
        due.push((
            after_response(at, Duration::from_secs(15)),
            Wake::AgentMessage,
        ));
    }
    if facts.rewound {
        due.push((now, Wake::Rewound));
    }
    if facts.prose {
        due.push((now, Wake::Prose));
    }
    if let Some(at) = facts.finished {
        due.push((after_response(at, Duration::ZERO), Wake::Returned));
    }
    if let Some(at) = facts.notified {
        due.push((after_response(at, Duration::from_secs(2)), Wake::Notify));
    }
    if let Some(at) = facts.failure {
        due.push((after_response(at, Duration::from_secs(20)), Wake::Failure));
    }
    if let Some(at) = facts.checkin {
        due.push((at, Wake::Checkin));
    }
    match due.into_iter().min_by_key(|(at, _)| *at) {
        Some((at, why)) if at <= now => Decision::Now(why),
        Some((at, _)) => Decision::Later(Some(at)),
        None => Decision::Later(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn successful_older_tasks_and_output_never_wake_on_their_own() {
        assert_eq!(
            decide(
                &Facts {
                    response_finished: Some(UnixMs(10)),
                    ..Facts::default()
                },
                UnixMs(100_000)
            ),
            Decision::Later(None)
        );
    }

    #[test]
    fn prose_silence_waits_only_for_human_and_archive_waits_for_revival() {
        let facts = Facts {
            prose_silenced: true,
            agent: Some(UnixMs(0)),
            checkin: Some(UnixMs(1)),
            failure: Some(UnixMs(0)),
            ..Facts::default()
        };
        assert_eq!(decide(&facts, UnixMs(50_000)), Decision::Later(None));
        assert_eq!(
            decide(
                &Facts {
                    human: Some(UnixMs(2)),
                    ..facts.clone()
                },
                UnixMs(2)
            ),
            Decision::Now(Wake::Message)
        );
        assert_eq!(
            decide(
                &Facts {
                    archived: true,
                    human: Some(UnixMs(0)),
                    ..facts
                },
                UnixMs(50_000)
            ),
            Decision::Later(None)
        );
    }

    #[test]
    fn every_deadline_and_mid_response_anchor() {
        let base = Facts {
            response_finished: Some(UnixMs(10_000)),
            ..Facts::default()
        };
        for (facts, deadline, why) in [
            (
                Facts {
                    human: Some(UnixMs(1)),
                    ..base.clone()
                },
                12_000,
                Wake::Message,
            ),
            (
                Facts {
                    agent: Some(UnixMs(1)),
                    ..base.clone()
                },
                25_000,
                Wake::AgentMessage,
            ),
            (
                Facts {
                    notified: Some(UnixMs(1)),
                    ..base.clone()
                },
                12_000,
                Wake::Notify,
            ),
            (
                Facts {
                    failure: Some(UnixMs(1)),
                    ..base.clone()
                },
                30_000,
                Wake::Failure,
            ),
            (
                Facts {
                    finished: Some(UnixMs(1)),
                    ..base.clone()
                },
                10_000,
                Wake::Returned,
            ),
            (
                Facts {
                    checkin: Some(UnixMs(11_000)),
                    ..base.clone()
                },
                11_000,
                Wake::Checkin,
            ),
        ] {
            assert_eq!(
                decide(&facts, UnixMs(deadline - 1)),
                Decision::Later(Some(UnixMs(deadline))),
                "{why:?}"
            );
            assert_eq!(decide(&facts, UnixMs(deadline)), Decision::Now(why));
        }
        assert_eq!(
            decide(
                &Facts {
                    prose: true,
                    ..base.clone()
                },
                UnixMs(1)
            ),
            Decision::Now(Wake::Prose)
        );
        assert_eq!(
            decide(
                &Facts {
                    archived: true,
                    human: Some(UnixMs(0)),
                    ..base
                },
                UnixMs(40_000)
            ),
            Decision::Later(None)
        );
    }

    #[test]
    fn events_wait_only_for_the_time_after_a_response() {
        let idle = Facts {
            response_finished: Some(UnixMs(10_000)),
            ..Facts::default()
        };
        // Long after the response: at once, not 2 seconds after it came.
        assert_eq!(
            decide(
                &Facts {
                    human: Some(UnixMs(50_000)),
                    ..idle.clone()
                },
                UnixMs(50_000)
            ),
            Decision::Now(Wake::Message)
        );
        // Just after the response: when its 2 seconds end, not 2 seconds
        // after the message.
        let soon = Facts {
            human: Some(UnixMs(11_500)),
            ..idle.clone()
        };
        assert_eq!(
            decide(&soon, UnixMs(11_999)),
            Decision::Later(Some(UnixMs(12_000)))
        );
        assert_eq!(decide(&soon, UnixMs(12_000)), Decision::Now(Wake::Message));
        // Agent mail long after the response: at once, not 15 seconds on.
        assert_eq!(
            decide(
                &Facts {
                    agent: Some(UnixMs(50_000)),
                    ..idle.clone()
                },
                UnixMs(50_000)
            ),
            Decision::Now(Wake::AgentMessage)
        );
        // The same holds with prose silenced.
        assert_eq!(
            decide(
                &Facts {
                    prose_silenced: true,
                    ..soon
                },
                UnixMs(11_999)
            ),
            Decision::Later(Some(UnixMs(12_000)))
        );
    }
}
