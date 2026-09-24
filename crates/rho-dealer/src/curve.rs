//! How a want's pressure moves with time.
//!
//! These are deliberately all in one place: rho has one user, so policy
//! changes are edits, not a configuration system. A want carries its
//! curve rather than a score, and the dealer reads the curve at the moment
//! it ranks, so waiting moves a card without anything being made again.

use chrono::{DateTime, FixedOffset, NaiveDateTime};
use senax_encoder::{Decode, Encode};

/// A card at or under this is not dealt at all.
pub const DEAL_QUEUE_FLOOR: f64 = -1.0;
/// How long a skipped card stays out of the next pull. Nothing else times
/// out: the card is still open the whole time and Home still shows it.
pub const SKIP_COOLDOWN: chrono::TimeDelta = chrono::TimeDelta::minutes(15);
pub const BLOCKED_REPLY_HEAD_START: f64 = 1.0;
pub const BLOCKED_REPLY_SLOPE_PER_DAY: f64 = 12.0;
pub const FYI_REPLY_PACE_DAYS: f64 = 3.0;
/// A person waiting on a reply is a blocked agent with a name, so a thread
/// rises on the same slope. The head start is a tenth above an agent's so a
/// ping of the same wait comes first; an agent the user just spoke to still
/// outranks it through the recency bonus, which is far larger.
pub const THREAD_REPLY_HEAD_START: f64 = 1.1;
/// Unread traffic in a channel nobody addressed the user in. It starts far
/// below a direct message or a thread they are in and fades instead of
/// rising, so it can never overtake one however long it sits: a room is
/// worth a look today and worth nothing by the weekend.
pub const CHANNEL_TRAFFIC_HEAD_START: f64 = 0.3;
/// What being answered by somebody else takes off a channel's card. The
/// room is already being dealt with, so it falls under the floor in a
/// little over two days instead of four.
pub const CHANNEL_ANSWERED_DROP: f64 = 0.6;
/// Half a curve unit is enough to mark the hand visibly dirty without
/// turning every newly-ripe reminder into persistent chrome.
pub const LAMP_THRESHOLD: f64 = 0.5;
/// At 1.2 curve units a blocked agent chimes after about 24 minutes
/// unnoticed, an agent completed within about 12 minutes of interaction
/// chimes immediately through the recency bonus, and a ping takes about
/// 14 hours to cross. Sound therefore marks pressure, not every new card.
pub const CHIME_THRESHOLD: f64 = 1.2;
/// The agent the user just spoke to (a send, or opening its surface)
/// contributes 1.5 curve units. This must remain above the 1.2 chime
/// threshold or recently-driven agents lose their instant completion chime;
/// the quadratic fall below gives about 6 minutes of instant chime and about
/// 25 minutes above the 0.5 lamp threshold.
pub const AGENT_RECENCY_BONUS: f64 = 1.5;
/// The nudge is gone within the hour and falls steeply from the start
/// (quadratic: 0.375 left at 30 minutes), so "just spoke to" means minutes,
/// not a hidden hour-long preference.
pub const AGENT_RECENCY_WINDOW_MS: i64 = 60 * 60 * 1_000;

/// A moment the user named: to the day, or to the millisecond.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
pub struct DateMark {
    pub unix_ms: i64,
    /// Only the date counts: the mark ripens at the start of that day
    /// wherever the user is.
    pub day: bool,
}

impl DateMark {
    pub fn at(unix_ms: i64) -> Self {
        Self {
            unix_ms,
            day: false,
        }
    }

    pub fn day(date: chrono::NaiveDate) -> Self {
        Self {
            unix_ms: date
                .and_hms_opt(0, 0, 0)
                .map_or(0, |at| at.and_utc().timestamp_millis()),
            day: true,
        }
    }

    fn time(self) -> Option<NaiveDateTime> {
        DateTime::from_timestamp_millis(self.unix_ms).map(|time| time.naive_utc())
    }

    /// Days from the mark to `now`, whole days when only the date counts.
    pub fn elapsed_days(self, now: NaiveDateTime) -> Option<f64> {
        let time = self.time()?;
        Some(if self.day {
            now.date().signed_duration_since(time.date()).num_days() as f64
        } else {
            now.signed_duration_since(time).num_seconds() as f64 / 86_400.0
        })
    }

    /// Whether the mark is still ahead of `now`.
    pub fn is_ahead(self, now: DateTime<FixedOffset>) -> bool {
        self.elapsed_days(now.naive_local())
            .is_some_and(|elapsed| elapsed < 0.0)
    }

    /// When the mark comes, in the user's wall time.
    pub fn local(self) -> Option<NaiveDateTime> {
        self.time()
    }
}

/// How a want's pressure moves with time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Curve {
    /// Someone waits on the user: rises from a head start by
    /// [`BLOCKED_REPLY_SLOPE_PER_DAY`] for every day since `since_ms`.
    Waiting { head_start: f64, since_ms: i64 },
    /// For the user's information: fades from a head start by one unit
    /// every [`FYI_REPLY_PACE_DAYS`] since `since_ms`.
    Fading { head_start: f64, since_ms: i64 },
    /// A date the user set to come back to something: nothing until then,
    /// then it ages, starting `pace_days` under.
    Wakes { at: DateMark, pace_days: u32 },
    /// A deadline: shows `pace_days` before it, rises to it, and jumps
    /// over everything once it has passed.
    Deadline { at: DateMark, pace_days: u32 },
}

fn wait_days(since_ms: i64, now: DateTime<FixedOffset>) -> f64 {
    (now.timestamp_millis() - since_ms) as f64 / 86_400_000.0
}

impl Curve {
    pub fn priority(&self, now: DateTime<FixedOffset>) -> f64 {
        match *self {
            Self::Waiting {
                head_start,
                since_ms,
            } => head_start + BLOCKED_REPLY_SLOPE_PER_DAY * wait_days(since_ms, now),
            Self::Fading {
                head_start,
                since_ms,
            } => head_start - wait_days(since_ms, now) / FYI_REPLY_PACE_DAYS,
            Self::Wakes { at, pace_days } | Self::Deadline { at, pace_days } => {
                let Some(elapsed) = at.elapsed_days(now.naive_local()) else {
                    return f64::NEG_INFINITY;
                };
                let pace = f64::from(pace_days);
                match self {
                    // One curve serves a todo and a snooze alike: a todo
                    // carries its cadence, a snooze has none, and
                    // `elapsed - pace` is each of them.
                    Self::Wakes { .. } if elapsed < 0.0 => f64::NEG_INFINITY,
                    Self::Wakes { .. } => elapsed - pace,
                    _ if elapsed < -pace => f64::NEG_INFINITY,
                    _ if elapsed <= 0.0 => elapsed / pace.max(1.0),
                    _ => 1_000_000.0 + elapsed,
                }
            }
        }
    }

    /// The words for where the curve stands: `reason` with its `{age}`
    /// filled in, or the mark's own words for a date.
    pub fn label(&self, reason: &str, now: DateTime<FixedOffset>) -> String {
        match *self {
            Self::Waiting { since_ms, .. } | Self::Fading { since_ms, .. } => {
                reason.replace("{age}", &age_label(wait_days(since_ms, now)))
            }
            Self::Wakes { at, .. } => match at.elapsed_days(now.naive_local()) {
                // A woken node reads the same whether it was a todo or a
                // snooze: the cadence lives in the curve, not in two words
                // for one field.
                Some(elapsed) => format!("deferred · woke {}", age_label(elapsed)),
                None => String::new(),
            },
            Self::Deadline { at, .. } => match at.elapsed_days(now.naive_local()) {
                Some(elapsed) if elapsed > 0.0 => {
                    format!("deadline · {}d late", elapsed.floor() as u64)
                }
                Some(elapsed) => format!("deadline · {}d", (-elapsed).ceil() as u64),
                None => String::new(),
            },
        }
    }

    /// The moments after `now` at which this curve jumps rather than
    /// slides: a date arriving, or coming into view.
    pub(crate) fn steps(&self) -> Vec<NaiveDateTime> {
        match *self {
            Self::Waiting { .. } | Self::Fading { .. } => Vec::new(),
            Self::Wakes { at, .. } => at.local().into_iter().collect(),
            Self::Deadline { at, pace_days } => at
                .local()
                .into_iter()
                .flat_map(|at| [at - chrono::TimeDelta::days(i64::from(pace_days)), at])
                .collect(),
        }
    }
}

/// What the user having just spoken to an agent adds to its card.
pub fn recency_bonus(last_ms: i64, now: DateTime<FixedOffset>) -> f64 {
    let elapsed = (now.timestamp_millis() - last_ms).clamp(0, AGENT_RECENCY_WINDOW_MS);
    let remaining = 1.0 - elapsed as f64 / AGENT_RECENCY_WINDOW_MS as f64;
    AGENT_RECENCY_BONUS * remaining * remaining
}

pub fn age_label(days: f64) -> String {
    if days < 1.0 / 24.0 {
        format!("{}m", (days * 1_440.0).max(0.0).round() as i64)
    } else if days < 1.0 {
        format!("{:.1}h", days * 24.0)
    } else {
        format!("{days:.1}d")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noon() -> DateTime<FixedOffset> {
        chrono::NaiveDate::from_ymd_opt(2026, 8, 23)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
            .fixed_offset()
    }

    fn days_ago(days: f64) -> i64 {
        noon().timestamp_millis() - (days * 86_400_000.0) as i64
    }

    fn thread(days: f64) -> Curve {
        Curve::Waiting {
            head_start: THREAD_REPLY_HEAD_START,
            since_ms: days_ago(days),
        }
    }

    fn room(days: f64, answered: bool) -> Curve {
        Curve::Fading {
            head_start: CHANNEL_TRAFFIC_HEAD_START
                - if answered { CHANNEL_ANSWERED_DROP } else { 0.0 },
            since_ms: days_ago(days),
        }
    }

    #[test]
    fn a_slack_thread_rises_like_an_agent_waiting_on_a_reply() {
        let priority = thread(2.0).priority(noon());
        assert_eq!(
            priority,
            THREAD_REPLY_HEAD_START + 2.0 * BLOCKED_REPLY_SLOPE_PER_DAY
        );
        assert_eq!(
            thread(2.0).label("mentioned in #design · needs reply · {age}", noon()),
            "mentioned in #design · needs reply · 2.0d"
        );
    }

    #[test]
    fn a_channel_fades_and_never_overtakes_a_thread() {
        let fresh = room(0.0, false).priority(noon());
        assert_eq!(fresh, CHANNEL_TRAFFIC_HEAD_START);
        // Under the floor in four days, and in a little over two when
        // somebody else is already answering in there.
        assert!(room(3.0, false).priority(noon()) > DEAL_QUEUE_FLOOR);
        assert!(room(4.0, false).priority(noon()) <= DEAL_QUEUE_FLOOR);
        assert!(room(2.0, true).priority(noon()) > DEAL_QUEUE_FLOOR);
        assert!(room(2.5, true).priority(noon()) <= DEAL_QUEUE_FLOOR);
        // The freshest room is still below the freshest thread.
        assert!(fresh < thread(0.0).priority(noon()));
    }

    #[test]
    fn a_ping_outranks_a_blocked_agent_of_the_same_wait_but_not_a_fresh_one() {
        let wait = 2.0 / 24.0;
        let ping = thread(wait).priority(noon());
        let blocked = Curve::Waiting {
            head_start: BLOCKED_REPLY_HEAD_START,
            since_ms: days_ago(wait),
        }
        .priority(noon());
        assert!(
            ping > blocked,
            "a ping ({ping}) outranks an agent ({blocked})"
        );
        let spoken_to =
            blocked + recency_bonus(noon().timestamp_millis() - 10 * 60 * 1_000, noon());
        assert!(
            spoken_to > ping,
            "an agent spoken to 10 minutes ago ({spoken_to}) still comes first"
        );
    }

    #[test]
    fn a_finished_agent_fades_under_the_floor_in_three_days() {
        let finished = |days| Curve::Fading {
            head_start: 0.0,
            since_ms: days_ago(days),
        };
        assert!(finished(2.9).priority(noon()) > DEAL_QUEUE_FLOOR);
        assert!(finished(3.0).priority(noon()) <= DEAL_QUEUE_FLOOR);
    }

    #[test]
    fn dates_wake_ripen_and_jump_once_overdue() {
        let today = noon().date_naive();
        let day = |offset: i64| DateMark::day(today + chrono::TimeDelta::days(offset));
        let wakes = |offset, pace_days| Curve::Wakes {
            at: day(offset),
            pace_days,
        };
        assert_eq!(wakes(1, 0).priority(noon()), f64::NEG_INFINITY);
        assert_eq!(wakes(0, 0).priority(noon()), 0.0);
        assert_eq!(wakes(-2, 1).priority(noon()), 1.0);
        assert_eq!(wakes(-2, 0).label("", noon()), "deferred · woke 2.0d");

        let deadline = |offset, pace_days| Curve::Deadline {
            at: day(offset),
            pace_days,
        };
        assert_eq!(deadline(5, 3).priority(noon()), f64::NEG_INFINITY);
        assert_eq!(deadline(2, 4).priority(noon()), -0.5);
        assert_eq!(deadline(-1, 4).priority(noon()), 1_000_001.0);
        assert_eq!(deadline(2, 4).label("", noon()), "deadline · 2d");
        assert_eq!(deadline(-1, 4).label("", noon()), "deadline · 1d late");
    }
}
