//! How a card's pressure moves with time.
//!
//! These are deliberately all in one place: rho has one user, so policy
//! changes are edits, not a configuration system. A card carries its
//! curve rather than a score, and the curve is read at the moment the hand
//! is ranked, so waiting moves a card without anything being made again.
//! The cases these serve are in `cases.md`.

use jiff::{SignedDuration, Timestamp};

/// A card at or under this is not dealt at all.
pub const DEAL_QUEUE_FLOOR: f64 = -1.0;
/// Someone waiting on the user rises this much a day.
pub const WAITING_SLOPE_PER_DAY: f64 = 12.0;
/// A project mail addressed to the user: an ask, result, or stop.
pub const AGENT_BLOCKED_HEAD_START: f64 = 1.0;
/// How many days before a deadline it starts to press, when the user did
/// not say.
pub const DEADLINE_LEAD_DAYS: u32 = 3;
/// An agent's reply this soon after the user's own message reaches them
/// through a snooze: they are in that conversation.
pub const REPLY_BREAKTHROUGH: SignedDuration = SignedDuration::from_hours(1);
/// How long a skipped card stays behind the rest, unless its source moves.
pub const SKIP_HOLD: SignedDuration = SignedDuration::from_mins(30);
/// Half a curve unit is enough to mark the hand visibly dirty without
/// turning every newly-ripe reminder into persistent chrome.
pub const LAMP_THRESHOLD: f64 = 0.5;
/// At 1.2 curve units a blocked agent chimes after about 24 minutes
/// unnoticed, an agent completed within about 12 minutes of interaction
/// chimes immediately through the recency bonus, and a ping takes about
/// 14 hours to cross. Sound therefore marks pressure, not every new card.
pub const CHIME_THRESHOLD: f64 = 1.2;
/// The agent the user just wrote to contributes 1.5 curve units. This must
/// remain above the 1.2 chime threshold or recently-driven agents lose
/// their instant completion chime; the quadratic fall below gives about 6
/// minutes of instant chime and about 25 minutes above the 0.5 lamp
/// threshold.
pub const AGENT_RECENCY_BONUS: f64 = 1.5;
/// The nudge is gone within the hour and falls steeply from the start
/// (quadratic: 0.375 left at 30 minutes), so "just spoke to" means minutes,
/// not a hidden hour-long preference.
pub const AGENT_RECENCY_WINDOW: SignedDuration = SignedDuration::from_hours(1);

fn days(from: Timestamp, to: Timestamp) -> f64 {
    to.duration_since(from).as_secs_f64() / 86_400.0
}

/// How a card's pressure moves with time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Curve {
    /// Someone waits on the user: rises from a head start by
    /// [`WAITING_SLOPE_PER_DAY`] for every day since `since`.
    Waiting { head_start: f64, since: Timestamp },
    /// For the user's information: falls from a head start to the floor
    /// in `gone_days`.
    Fading {
        head_start: f64,
        since: Timestamp,
        gone_days: f64,
    },
    /// Due at `by`: shows `lead_days` before, rises to it, and jumps over
    /// everything once it has passed.
    Deadline { by: Timestamp, lead_days: u32 },
}

impl Curve {
    pub fn priority(&self, now: Timestamp) -> f64 {
        match *self {
            Self::Waiting { head_start, since } => {
                head_start + WAITING_SLOPE_PER_DAY * days(since, now).max(0.0)
            }
            Self::Fading {
                head_start,
                since,
                gone_days,
            } => {
                head_start - (head_start - DEAL_QUEUE_FLOOR) * days(since, now).max(0.0) / gone_days
            }
            Self::Deadline { by, lead_days } => {
                let late = days(by, now);
                let lead = f64::from(lead_days.max(1));
                match late {
                    late if late < -lead => f64::NEG_INFINITY,
                    late if late <= 0.0 => late / lead,
                    late => 1_000_000.0 + late,
                }
            }
        }
    }

    /// The words for where the curve stands: `reason` with its `{age}`
    /// filled in, or the curve's own words for a deadline.
    pub fn label(&self, reason: &str, now: Timestamp) -> String {
        match *self {
            Self::Waiting { since, .. } | Self::Fading { since, .. } => {
                reason.replace("{age}", &age(since, now))
            }
            Self::Deadline { by, .. } => match days(by, now) {
                late if late > 0.0 => format!("deadline · {}d late", late.floor() as u64),
                late => format!("deadline · {}d", (-late).ceil() as u64),
            },
        }
    }

    /// The first moment after `now` when its words say another age.
    pub(crate) fn label_changes(&self, now: Timestamp) -> Timestamp {
        match *self {
            Self::Waiting { since, .. } | Self::Fading { since, .. } => age_changes(since, now),
            Self::Deadline { by, .. } => {
                let day = SignedDuration::from_hours(24);
                let late = now.duration_since(by);
                if late >= SignedDuration::ZERO {
                    let whole = late.as_secs() / day.as_secs();
                    by + day * (whole as i32 + 1)
                } else {
                    let ahead = (-late).as_secs();
                    let days = (ahead + day.as_secs() - 1) / day.as_secs();
                    by - day * (days as i32 - 1)
                }
            }
        }
    }

    /// The moments at which this curve jumps rather than slides: a
    /// deadline coming into view or passing.
    pub(crate) fn steps(&self) -> Vec<Timestamp> {
        match *self {
            Self::Waiting { .. } | Self::Fading { .. } => Vec::new(),
            Self::Deadline { by, lead_days } => vec![
                by - SignedDuration::from_hours(24 * i64::from(lead_days.max(1))),
                by,
            ],
        }
    }
}

/// What the user having just written to an agent adds to its card.
pub fn recency_bonus(last: Timestamp, now: Timestamp) -> f64 {
    fading(last, now, AGENT_RECENCY_WINDOW, AGENT_RECENCY_BONUS)
}

/// `size` at `from`, falling quadratically to nothing over `window`.
pub(crate) fn fading(from: Timestamp, now: Timestamp, window: SignedDuration, size: f64) -> f64 {
    let elapsed = now.duration_since(from).clamp(SignedDuration::ZERO, window);
    let remaining = 1.0 - elapsed.as_secs_f64() / window.as_secs_f64();
    size * remaining * remaining
}

/// How long since `since`, in the words every screen says it in: minutes
/// under an hour, then tenths of an hour, then tenths of a day, each the
/// nearest. A card's words and an agent's on its thread both say this.
pub fn age(since: Timestamp, now: Timestamp) -> String {
    let (count, unit) = age_step(since, now);
    match unit {
        AgeUnit::Minute if count == 0 => "now".to_owned(),
        AgeUnit::Minute => format!("{count}m"),
        AgeUnit::TenthHour => format!("{}.{}h", count / 10, count % 10),
        AgeUnit::TenthDay => format!("{}.{}d", count / 10, count % 10),
    }
}

/// The first moment after `now` when `age(since, ·)` says something else:
/// halfway to its next count, or where it moves to the next unit.
pub fn age_changes(since: Timestamp, now: Timestamp) -> Timestamp {
    let (count, unit) = age_step(since, now);
    let size = unit.millis();
    let next = if count == 0 && matches!(unit, AgeUnit::Minute) {
        size
    } else {
        ((count + 1) * size - size / 2).min(unit.until())
    };
    since + SignedDuration::from_millis(next)
}

#[derive(Clone, Copy)]
enum AgeUnit {
    Minute,
    TenthHour,
    TenthDay,
}

impl AgeUnit {
    fn millis(self) -> i64 {
        match self {
            Self::Minute => 60_000,
            Self::TenthHour => 360_000,
            Self::TenthDay => 8_640_000,
        }
    }

    /// Where the next unit takes over.
    fn until(self) -> i64 {
        match self {
            Self::Minute => 3_600_000,
            Self::TenthHour => 86_400_000,
            Self::TenthDay => i64::MAX,
        }
    }
}

/// The nearest count of the unit `now - since` is said in. In whole
/// milliseconds, so the moment the words change is exact.
fn age_step(since: Timestamp, now: Timestamp) -> (i64, AgeUnit) {
    let elapsed = now
        .duration_since(since)
        .as_millis()
        .clamp(0, i128::from(i64::MAX / 2)) as i64;
    let unit = [AgeUnit::Minute, AgeUnit::TenthHour, AgeUnit::TenthDay]
        .into_iter()
        .find(|unit| elapsed < unit.until())
        .expect("the last unit has no end");
    let size = unit.millis();
    let count = if matches!(unit, AgeUnit::Minute) && elapsed < size {
        0
    } else {
        (elapsed + size / 2) / size
    };
    (count, unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noon() -> Timestamp {
        "2026-08-23T12:00:00Z".parse().unwrap()
    }

    fn ago(days: f64) -> Timestamp {
        noon() - SignedDuration::from_secs_f64(days * 86_400.0)
    }

    #[test]
    fn someone_waiting_rises_and_information_fades_to_the_floor() {
        let waiting = Curve::Waiting {
            head_start: AGENT_BLOCKED_HEAD_START,
            since: ago(2.0),
        };
        assert_eq!(
            waiting.priority(noon()),
            AGENT_BLOCKED_HEAD_START + 2.0 * WAITING_SLOPE_PER_DAY
        );
        assert_eq!(
            waiting.label("needs reply · {age}", noon()),
            "needs reply · 2.0d"
        );
        let information = |days| Curve::Fading {
            head_start: 0.0,
            since: ago(days),
            gone_days: 3.0,
        };
        assert!(information(2.9).priority(noon()) > DEAL_QUEUE_FLOOR);
        assert!(information(3.0).priority(noon()) <= DEAL_QUEUE_FLOOR);
    }

    #[test]
    fn a_deadline_shows_its_lead_ahead_and_jumps_once_late() {
        let deadline = |days: f64, lead_days| Curve::Deadline {
            by: ago(-days),
            lead_days,
        };
        assert_eq!(deadline(5.0, 3).priority(noon()), f64::NEG_INFINITY);
        assert_eq!(deadline(2.0, 4).priority(noon()), -0.5);
        assert_eq!(deadline(-1.0, 4).priority(noon()), 1_000_001.0);
        assert_eq!(deadline(2.0, 4).label("", noon()), "deadline · 2d");
        assert_eq!(deadline(-1.0, 4).label("", noon()), "deadline · 1d late");
    }

    #[test]
    fn an_age_says_the_nearest_count_and_knows_when_it_changes() {
        let since = noon();
        let at = |millis: i64| since + SignedDuration::from_millis(millis);
        let minute = 60_000;
        let hour = 60 * minute;
        let day = 24 * hour;
        for (elapsed, words, changes) in [
            (0, "now", minute),
            (30_000, "now", minute),
            (59_999, "now", minute),
            (60_000, "1m", 90_000),
            (59 * minute + 29_999, "59m", 59 * minute + 30_000),
            (59 * minute + 30_000, "60m", hour),
            (hour, "1.0h", hour + 3 * minute),
            (hour + 3 * minute, "1.1h", hour + 9 * minute),
            (day - 1, "24.0h", day),
            (day, "1.0d", day + day / 20),
            (48 * day + 10 * hour, "48.4d", 48 * day + day * 45 / 100),
        ] {
            assert_eq!(age(since, at(elapsed)), words, "at {elapsed}ms");
            let next = age_changes(since, at(elapsed));
            assert_eq!(next, at(changes), "after {elapsed}ms");
            assert_ne!(age(since, next), words, "it changes at {next}");
            let before = next - SignedDuration::from_millis(1);
            assert_eq!(age(since, before), words, "not before {next}");
        }
        // A clock behind the host's says no age yet, and moves on with it.
        assert_eq!(age(since, at(-5_000)), "now");
        assert_eq!(age_changes(since, at(-5_000)), at(minute));
    }
}
