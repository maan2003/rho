//! What an agent is doing, in the word every screen says it in, and the
//! stops an agent did not choose: the ones that make a card.

use std::time::Duration;

use senax_encoder::{Decode, Encode, Pack, Unpack};

use crate::UnixMs;

/// What an agent is doing now, as its host last said.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Activity {
    Working,
    /// It asked the user and waits for the answer.
    Waiting,
    Idle,
    /// Its model kept failing and it stopped.
    Failed,
    /// Its model failed and it tries again.
    Retrying,
    /// Its host lost it: the process it ran in ended without a word.
    Lost,
    Archived,
}

impl Activity {
    /// The word the transcript's top line, `find`, and a thread's `with`
    /// line say.
    pub fn word(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Idle => "idle",
            Self::Failed => "failed",
            Self::Retrying => "retrying",
            Self::Lost => "lost",
            Self::Archived => "archived",
        }
    }
}

/// How long an agent retries its model before it counts as stuck. A
/// transient error can recover on its own, but work blocked for three
/// minutes needs to be visible to the user.
pub const STUCK_AFTER: Duration = Duration::from_secs(3 * 60);

/// A stop the agent did not choose: what draws the user to its thread
/// without a word from it.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub enum Stop {
    Failed,
    /// Retrying for [`STUCK_AFTER`] or longer.
    Stuck,
    Lost,
}

impl Stop {
    /// The word on its card, on its room and on the status line.
    pub fn word(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Stuck => "stuck",
            Self::Lost => "lost",
        }
    }
}

/// What an agent is doing and since when, as its host says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Doing {
    pub activity: Activity,
    /// When it started doing it, on its host's clock.
    pub since: UnixMs,
}

impl Doing {
    /// The stop it is in at `now`, if it is in one. Finishing, archiving
    /// and waiting on the user are its own choices and are none.
    pub fn stop(&self, now: UnixMs) -> Option<Stop> {
        match self.activity {
            Activity::Failed => Some(Stop::Failed),
            Activity::Lost => Some(Stop::Lost),
            Activity::Retrying if now >= self.stuck_at()? => Some(Stop::Stuck),
            _ => None,
        }
    }

    /// When retrying turns into being stuck; `None` while not retrying.
    pub fn stuck_at(&self) -> Option<UnixMs> {
        (self.activity == Activity::Retrying).then(|| self.since + STUCK_AFTER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doing(activity: Activity) -> Doing {
        Doing {
            activity,
            since: UnixMs(1_000_000),
        }
    }

    #[test]
    fn retrying_is_a_stop_only_from_three_minutes_on() {
        let retrying = doing(Activity::Retrying);
        let limit = 180_000;
        assert_eq!(retrying.stop(UnixMs(1_000_000 + limit - 1)), None);
        assert_eq!(retrying.stop(UnixMs(1_000_000 + limit)), Some(Stop::Stuck));
    }

    #[test]
    fn only_what_the_agent_did_not_choose_is_a_stop() {
        let now = UnixMs(u64::MAX / 2);
        assert_eq!(doing(Activity::Failed).stop(now), Some(Stop::Failed));
        assert_eq!(doing(Activity::Lost).stop(now), Some(Stop::Lost));
        for chosen in [
            Activity::Working,
            Activity::Waiting,
            Activity::Idle,
            Activity::Archived,
        ] {
            assert_eq!(doing(chosen).stop(now), None, "{chosen:?}");
        }
    }
}
