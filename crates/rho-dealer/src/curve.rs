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
/// An agent that asked for the user, or died.
pub const AGENT_BLOCKED_HEAD_START: f64 = 1.0;
/// A direct message: the most personal ask, just above a mention.
pub const SLACK_DM_HEAD_START: f64 = 1.2;
pub const SLACK_MENTION_HEAD_START: f64 = 1.1;
/// A reply in a thread Slack follows for the user: a conversation they
/// are in, but not one addressed to them.
pub const SLACK_THREAD_HEAD_START: f64 = 0.6;
/// A thread this small is a conversation with the user, and asks like a
/// direct message.
pub const SLACK_SMALL_THREAD_PEOPLE: usize = 3;
/// An agent that finished: for the user's information, gone in three
/// days.
pub const AGENT_FINISHED_HEAD_START: f64 = 0.0;
pub const AGENT_FINISHED_GONE_DAYS: f64 = 3.0;
/// Unread traffic in a channel nobody addressed the user in: barely
/// above nothing, and gone within the day, or half of it once someone else
/// is answering.
pub const CHANNEL_TRAFFIC_HEAD_START: f64 = 0.1;
pub const CHANNEL_TRAFFIC_GONE_DAYS: f64 = 1.0;
pub const CHANNEL_ANSWERED_GONE_DAYS: f64 = 0.5;
/// A todo sits low and rises slowly, and never fades: it stays until done.
pub const TODO_HEAD_START: f64 = 0.0;
pub const TODO_SLOPE_PER_DAY: f64 = 0.1;
/// How many days before a deadline it starts to press, when the user did
/// not say.
pub const DEADLINE_LEAD_DAYS: u32 = 3;
/// What each new message from somebody else in a direct conversation adds
/// to its card when a snooze on it ends, up to the cap.
pub const SNOOZED_MESSAGE_BUMP: f64 = 0.1;
pub const SNOOZED_MESSAGE_BUMP_CAP: f64 = 0.5;
/// An agent's reply this soon after the user's own message reaches them
/// through a snooze: they are in that conversation.
pub const REPLY_BREAKTHROUGH: SignedDuration = SignedDuration::from_hours(1);
/// A skipped card drops this far, and climbs back over [`SKIP_FADE`]
/// (quadratically, so most of the way in the first few minutes).
pub const SKIP_PENALTY: f64 = 1.0;
pub const SKIP_FADE: SignedDuration = SignedDuration::from_mins(30);
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
    /// On the user's plate since `since`: low, rising slowly, never gone.
    Plate { since: Timestamp },
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
            Self::Plate { since } if now < since => f64::NEG_INFINITY,
            Self::Plate { since } => TODO_HEAD_START + TODO_SLOPE_PER_DAY * days(since, now),
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
    /// filled in, or the curve's own words for a todo or a deadline.
    pub fn label(&self, reason: &str, now: Timestamp) -> String {
        match *self {
            Self::Waiting { since, .. } | Self::Fading { since, .. } => {
                reason.replace("{age}", &age_label(days(since, now)))
            }
            Self::Plate { since } => format!("todo · {}", age_label(days(since, now))),
            Self::Deadline { by, .. } => match days(by, now) {
                late if late > 0.0 => format!("deadline · {}d late", late.floor() as u64),
                late => format!("deadline · {}d", (-late).ceil() as u64),
            },
        }
    }

    /// The moments at which this curve jumps rather than slides: a todo
    /// arriving, a deadline coming into view or passing.
    pub(crate) fn steps(&self) -> Vec<Timestamp> {
        match *self {
            Self::Waiting { .. } | Self::Fading { .. } => Vec::new(),
            Self::Plate { since } => vec![since],
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

    fn noon() -> Timestamp {
        "2026-08-23T12:00:00Z".parse().unwrap()
    }

    fn ago(days: f64) -> Timestamp {
        noon() - SignedDuration::from_secs_f64(days * 86_400.0)
    }

    #[test]
    fn someone_waiting_rises_and_information_fades_to_the_floor() {
        let waiting = Curve::Waiting {
            head_start: SLACK_DM_HEAD_START,
            since: ago(2.0),
        };
        assert_eq!(
            waiting.priority(noon()),
            SLACK_DM_HEAD_START + 2.0 * WAITING_SLOPE_PER_DAY
        );
        assert_eq!(
            waiting.label("needs reply · {age}", noon()),
            "needs reply · 2.0d"
        );
        let finished = |days| Curve::Fading {
            head_start: AGENT_FINISHED_HEAD_START,
            since: ago(days),
            gone_days: AGENT_FINISHED_GONE_DAYS,
        };
        assert!(finished(2.9).priority(noon()) > DEAL_QUEUE_FLOOR);
        assert!(finished(3.0).priority(noon()) <= DEAL_QUEUE_FLOOR);
    }

    #[test]
    fn a_channel_is_gone_within_the_day_and_never_overtakes_a_thread() {
        let room = |days, gone_days| Curve::Fading {
            head_start: CHANNEL_TRAFFIC_HEAD_START,
            since: ago(days),
            gone_days,
        };
        assert!(room(0.9, CHANNEL_TRAFFIC_GONE_DAYS).priority(noon()) > DEAL_QUEUE_FLOOR);
        assert!(room(1.0, CHANNEL_TRAFFIC_GONE_DAYS).priority(noon()) <= DEAL_QUEUE_FLOOR);
        assert!(room(0.5, CHANNEL_ANSWERED_GONE_DAYS).priority(noon()) <= DEAL_QUEUE_FLOOR);
        let thread = Curve::Waiting {
            head_start: SLACK_THREAD_HEAD_START,
            since: noon(),
        };
        assert!(room(0.0, CHANNEL_TRAFFIC_GONE_DAYS).priority(noon()) < thread.priority(noon()));
    }

    #[test]
    fn a_todo_waits_for_its_start_and_then_climbs_slowly_for_good() {
        let todo = Curve::Plate { since: noon() };
        assert_eq!(todo.priority(ago(0.1)), f64::NEG_INFINITY);
        assert_eq!(todo.priority(noon()), TODO_HEAD_START);
        let month = noon() + SignedDuration::from_hours(24 * 30);
        assert!((todo.priority(month) - (TODO_HEAD_START + 3.0)).abs() < 1e-9);
        assert_eq!(todo.label("", month), "todo · 30.0d");
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
}
