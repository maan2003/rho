//! When the window was awake (`hosts.The window wakes`): the device's
//! sleeps, told apart from the wall clock by a clock that stops while
//! the device sleeps. The chime counts only waiting the user could have
//! noticed, so a night asleep does not ring the moment the window wakes.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use jiff::{SignedDuration, Timestamp};

/// Less than this between the two clocks is the clocks drifting or the
/// wall clock being set, not a sleep.
const SLEEP_AT_LEAST: SignedDuration = SignedDuration::from_mins(1);
/// A sleep this long ago has no card left to quiet: what waited since
/// before it crossed the chime's line long ago.
const KEPT: SignedDuration = SignedDuration::from_hours(7 * 24);

/// The sleeps the window has seen, as wall-clock spans.
#[derive(Default)]
pub(crate) struct Awake {
    /// Both clocks as last read.
    last: Option<(Timestamp, Duration)>,
    /// When each sleep fell and woke, oldest first.
    sleeps: Vec<(Timestamp, Timestamp)>,
}

impl Awake {
    /// Reads both clocks now.
    pub(crate) fn observe_now(&mut self) {
        self.observe(Timestamp::now(), awake_clock());
    }

    /// Both clocks as they read now: `wall`, and `awake`, which stops while
    /// the device sleeps. Wall time that passed beyond awake time since the
    /// last reading was a sleep, ending now.
    pub(crate) fn observe(&mut self, wall: Timestamp, awake: Duration) {
        if let Some((last_wall, last_awake)) = self.last {
            let awake_passed =
                SignedDuration::try_from(awake.saturating_sub(last_awake)).unwrap_or_default();
            let slept = last_wall.duration_until(wall) - awake_passed;
            if slept >= SLEEP_AT_LEAST {
                self.sleeps.push((wall - slept, wall));
            }
        }
        self.last = Some((wall, awake));
        self.sleeps
            .retain(|(_, woke)| woke.duration_until(wall) < KEPT);
    }

    /// Whether the window has slept at all lately.
    pub(crate) fn has_slept(&self) -> bool {
        !self.sleeps.is_empty()
    }

    /// How much of the time from `since` on the window slept through.
    pub(crate) fn slept_since(&self, since: Timestamp) -> SignedDuration {
        self.sleeps
            .iter()
            .filter(|(_, woke)| *woke > since)
            .map(|(fell, woke)| (*fell).max(since).duration_until(*woke))
            .fold(SignedDuration::ZERO, |sum, slept| sum + slept)
    }
}

/// A clock that stops while the device sleeps: `Instant` is the
/// monotonic clock on Linux and the uptime clock on macOS, neither of
/// which counts a suspend.
pub(crate) fn awake_clock() -> Duration {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hh_mm: &str) -> Timestamp {
        format!("2026-11-26T{hh_mm}:00Z").parse().unwrap()
    }

    fn mins(minutes: u64) -> Duration {
        Duration::from_secs(minutes * 60)
    }

    /// The lid shuts at 23:00 and opens at 08:00: nine hours of the wall
    /// clock, a few minutes of the awake one.
    #[test]
    fn a_night_with_the_lid_shut_is_a_sleep() {
        let mut awake = Awake::default();
        awake.observe(at("22:50"), mins(100));
        awake.observe(at("23:00"), mins(110));
        assert!(!awake.has_slept(), "awake time kept up with the wall");
        // The next day: the date moves on too.
        let morning: Timestamp = "2026-11-27T08:00:00Z".parse().unwrap();
        awake.observe(morning, mins(111));
        assert!(awake.has_slept());
        let hours = SignedDuration::from_hours;
        let fell = morning - (hours(9) - SignedDuration::from_mins(1));
        // A card waiting since before the night slept all of it; one that
        // began at 03:00, in the night, only what was left of it.
        assert_eq!(awake.slept_since(at("22:00")), fell.duration_until(morning));
        let three: Timestamp = "2026-11-27T03:00:00Z".parse().unwrap();
        assert_eq!(awake.slept_since(three), hours(5));
        // One that began after the window woke slept none.
        assert_eq!(awake.slept_since(morning), SignedDuration::ZERO);
    }

    /// A few seconds of drift, or a wall clock set back, is no sleep.
    #[test]
    fn drift_and_a_clock_set_back_are_not_sleeps() {
        let mut awake = Awake::default();
        awake.observe(at("10:00"), mins(0));
        awake.observe(at("10:10") + SignedDuration::from_secs(30), mins(10));
        awake.observe(at("09:00"), mins(20));
        assert!(!awake.has_slept());
        assert_eq!(awake.slept_since(at("08:00")), SignedDuration::ZERO);
    }

    /// Two naps both count from the time they began, and a week later
    /// neither is kept.
    #[test]
    fn naps_add_up_and_old_ones_are_let_go() {
        let mut awake = Awake::default();
        awake.observe(at("09:00"), mins(0));
        awake.observe(at("10:00"), mins(30));
        awake.observe(at("12:00"), mins(60));
        assert_eq!(
            awake.slept_since(at("08:00")),
            SignedDuration::from_mins(30 + 90)
        );
        assert_eq!(
            awake.slept_since(at("11:00")),
            SignedDuration::from_mins(60)
        );
        let later: Timestamp = "2026-12-03T12:00:00Z".parse().unwrap();
        awake.observe(later, mins(60) + Duration::from_secs(7 * 24 * 3600));
        assert!(!awake.has_slept());
    }
}
