//! The scheduling decision is pure: all deadlines are anchored to response
//! completion.
use std::time::Duration;

use rho_agent_types::UnixMs;

use crate::log::Wake;

pub const DEFAULT_CHECKIN: Duration = Duration::from_secs(120);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    pub human: Option<UnixMs>,
    pub agent: Option<UnixMs>,
    pub finished: Option<UnixMs>,
    pub notified: Option<UnixMs>,
    pub failure: Option<UnixMs>,
    pub checkin: Option<UnixMs>,
    pub response_finished: Option<UnixMs>,
    pub wake_on_tools: bool,
    pub prose: bool,
    pub restarted: bool,
    pub rewound: bool,
    pub archived: bool,
    pub prose_silenced: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Now(Wake),
    Later(Option<UnixMs>),
}

pub fn decide(facts: &Facts, now: UnixMs) -> Decision {
    if facts.archived {
        return Decision::Later(None);
    }
    let base = |at: UnixMs| at.max(facts.response_finished.unwrap_or(at));
    let mut due = Vec::new();
    if let Some(at) = facts.human {
        due.push((base(at) + Duration::from_secs(2), Wake::Message));
    }
    if facts.prose_silenced {
        return match facts.human.map(|at| base(at) + Duration::from_secs(2)) {
            Some(at) if at <= now => Decision::Now(Wake::Message),
            at => Decision::Later(at),
        };
    }
    if let Some(at) = facts.agent {
        due.push((base(at) + Duration::from_secs(15), Wake::AgentMessage));
    }
    if facts.restarted {
        due.push((now, Wake::Restarted));
    }
    if facts.rewound {
        due.push((now, Wake::Rewound));
    }
    if facts.prose {
        due.push((now, Wake::Prose));
    }
    if facts.wake_on_tools {
        if let Some(at) = facts.finished {
            due.push((base(at), Wake::Returned));
        }
        if let Some(at) = facts.notified {
            due.push((base(at) + Duration::from_secs(2), Wake::Notify));
        }
    }
    if let Some(at) = facts.failure {
        due.push((base(at) + Duration::from_secs(20), Wake::Failure));
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
                    wake_on_tools: true,
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
                UnixMs(2_002)
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
            wake_on_tools: true,
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
                    restarted: true,
                    ..base.clone()
                },
                UnixMs(1)
            ),
            Decision::Now(Wake::Restarted)
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
}
