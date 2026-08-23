//! Dated Desk marks and their howm-style priority curves.

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};

/// The dated property vocabulary understood by the Desk parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TemporalMarkKind {
    Deadline,
    Todo,
    Defer,
    Reminder,
    Skip,
    Done,
    Discarded,
}

impl TemporalMarkKind {
    pub fn property_key(self) -> &'static str {
        match self {
            Self::Deadline => "deadline",
            Self::Todo => "todo",
            Self::Defer => "defer",
            Self::Reminder => "reminder",
            Self::Skip => "skip",
            Self::Done => "done",
            Self::Discarded => "discarded",
        }
    }

    pub fn from_property_key(key: &str) -> Option<Self> {
        if key.eq_ignore_ascii_case("deadline") {
            Some(Self::Deadline)
        } else if key.eq_ignore_ascii_case("todo") {
            Some(Self::Todo)
        } else if key.eq_ignore_ascii_case("defer") {
            Some(Self::Defer)
        } else if key.eq_ignore_ascii_case("reminder") {
            Some(Self::Reminder)
        } else if key.eq_ignore_ascii_case("skip") {
            Some(Self::Skip)
        } else if key.eq_ignore_ascii_case("done") {
            Some(Self::Done)
        } else if key.eq_ignore_ascii_case("discarded") {
            Some(Self::Discarded)
        } else {
            None
        }
    }

    fn default_pace_days(self) -> u32 {
        match self {
            Self::Deadline | Self::Todo => 7,
            Self::Defer => 30,
            Self::Reminder => 1,
            Self::Skip | Self::Done | Self::Discarded => 0,
        }
    }

    fn accepts_pace(self) -> bool {
        !matches!(self, Self::Skip | Self::Done | Self::Discarded)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TemporalMark {
    pub kind: TemporalMarkKind,
    pub at: NaiveDateTime,
    /// Whether the source omitted an explicit time and follows calendar-day
    /// rather than instant semantics.
    pub date_only: bool,
    /// Lead, pace, or period, depending on `kind`, resolved to its default.
    pub pace_days: u32,
}

impl TemporalMark {
    pub fn parse(kind: TemporalMarkKind, value: &str) -> Option<Self> {
        let mut words = value.split_whitespace();
        let date = NaiveDate::parse_from_str(words.next()?, "%Y-%m-%d").ok()?;
        let mut at = date.and_time(NaiveTime::MIN);
        let mut date_only = true;
        let mut pace = None;
        if let Some(word) = words.next() {
            if let Ok(time) = NaiveTime::parse_from_str(word, "%H:%M") {
                at = date.and_time(time);
                date_only = false;
                if let Some(word) = words.next() {
                    pace = Some(parse_days(word)?);
                }
            } else {
                pace = Some(parse_days(word)?);
            }
        }
        if words.next().is_some() || (pace.is_some() && !kind.accepts_pace()) {
            return None;
        }
        Some(Self {
            kind,
            at,
            date_only,
            pace_days: pace.unwrap_or_else(|| kind.default_pace_days()),
        })
    }
}

fn parse_days(value: &str) -> Option<u32> {
    value
        .strip_suffix(['d', 'D'])?
        .parse()
        .ok()
        .filter(|days| *days > 0)
}

fn elapsed_days(mark: &TemporalMark, now: NaiveDateTime) -> f64 {
    if mark.date_only {
        now.date().signed_duration_since(mark.at.date()).num_days() as f64
    } else {
        now.signed_duration_since(mark.at).num_seconds() as f64 / 86_400.0
    }
}

/// A common days-scale priority. Larger values sort first.
pub fn priority(mark: &TemporalMark, now: NaiveDateTime) -> f64 {
    let elapsed = elapsed_days(mark, now);
    let pace = mark.pace_days as f64;
    match mark.kind {
        TemporalMarkKind::Deadline if elapsed < -pace => f64::NEG_INFINITY,
        TemporalMarkKind::Deadline if elapsed <= 0.0 => elapsed / pace,
        TemporalMarkKind::Deadline => 1_000_000.0 + elapsed,
        TemporalMarkKind::Todo => elapsed - pace,
        TemporalMarkKind::Defer => {
            let phase = elapsed.rem_euclid(pace);
            -phase.min(pace - phase)
        }
        TemporalMarkKind::Reminder if elapsed < 0.0 => f64::NEG_INFINITY,
        TemporalMarkKind::Reminder => -elapsed / pace,
        TemporalMarkKind::Skip | TemporalMarkKind::Done | TemporalMarkKind::Discarded => {
            f64::NEG_INFINITY
        }
    }
}

pub fn surfaced(mark: &TemporalMark, now: NaiveDateTime, threshold: f64) -> bool {
    priority(mark, now) > threshold
}

pub fn is_overdue_deadline(mark: &TemporalMark, now: NaiveDateTime) -> bool {
    mark.kind == TemporalMarkKind::Deadline
        && if mark.date_only {
            now.date() > mark.at.date()
        } else {
            now > mark.at
        }
}

/// Render one visible Desk property line. Midnight is kept date-only.
pub fn property_line(kind: TemporalMarkKind, at: NaiveDateTime, pace_days: Option<u32>) -> String {
    let at = if at.time() == NaiveTime::MIN {
        at.format("%Y-%m-%d").to_string()
    } else {
        at.format("%Y-%m-%d %H:%M").to_string()
    };
    let pace = pace_days
        .filter(|_| kind.accepts_pace())
        .map_or_else(String::new, |days| format!(" {days}d"));
    format!(":{}: {at}{pace}\n", kind.property_key())
}

#[cfg(test)]
mod tests {
    use chrono::Duration;

    use super::*;

    fn at(day: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2026, 8, day)
            .unwrap()
            .and_time(NaiveTime::MIN)
    }

    fn mark(kind: TemporalMarkKind, day: u32, pace_days: u32) -> TemporalMark {
        TemporalMark {
            kind,
            at: at(day),
            date_only: true,
            pace_days,
        }
    }

    #[test]
    fn deadline_hides_rises_then_pins_above_everything() {
        let mark = mark(TemporalMarkKind::Deadline, 20, 7);
        assert_eq!(priority(&mark, at(12)), f64::NEG_INFINITY);
        assert_eq!(priority(&mark, at(13)), -1.0);
        assert!(!surfaced(&mark, at(13), -1.0));
        assert_eq!(priority(&mark, at(14)), -6.0 / 7.0);
        assert!(surfaced(&mark, at(14), -1.0));
        assert_eq!(priority(&mark, at(19)), -1.0 / 7.0);
        assert_eq!(priority(&mark, at(20)), 0.0);
        assert_eq!(priority(&mark, at(21)), 1_000_001.0);
        assert!(priority(&mark, at(22)) > priority(&mark, at(21)));
    }

    #[test]
    fn todo_sinks_then_surfaces_and_keeps_rising() {
        let mark = mark(TemporalMarkKind::Todo, 10, 7);
        assert_eq!(priority(&mark, at(9)), -8.0);
        assert_eq!(priority(&mark, at(10)), -7.0);
        assert_eq!(priority(&mark, at(17)), 0.0);
        assert_eq!(priority(&mark, at(18)), 1.0);
    }

    #[test]
    fn defer_is_a_permanent_triangle_wave() {
        let mark = mark(TemporalMarkKind::Defer, 1, 10);
        assert_eq!(priority(&mark, at(1)), 0.0);
        assert_eq!(priority(&mark, at(6)), -5.0);
        assert_eq!(priority(&mark, at(11)), 0.0);
        assert_eq!(priority(&mark, at(16)), -5.0);
    }

    #[test]
    fn reminder_surfaces_once_then_forgivingly_sinks() {
        let mark = mark(TemporalMarkKind::Reminder, 10, 2);
        assert_eq!(priority(&mark, at(9)), f64::NEG_INFINITY);
        assert_eq!(priority(&mark, at(1)), f64::NEG_INFINITY);
        assert_eq!(priority(&mark, at(10)), 0.0);
        assert_eq!(priority(&mark, at(12)), -1.0);
        assert_eq!(priority(&mark, at(14)), -2.0);
        assert!(!surfaced(&mark, at(10), 0.0));
        assert!(surfaced(&mark, at(10), -0.01));
    }

    #[test]
    fn date_only_reminders_and_deadlines_use_whole_calendar_days() {
        let midday = |day| at(day) + Duration::hours(15);
        let reminder = mark(TemporalMarkKind::Reminder, 10, 1);
        assert_eq!(priority(&reminder, midday(9)), f64::NEG_INFINITY);
        assert_eq!(priority(&reminder, midday(10)), 0.0);
        assert!(surfaced(&reminder, midday(10), -0.01));
        assert_eq!(priority(&reminder, midday(11)), -1.0);

        let deadline = mark(TemporalMarkKind::Deadline, 10, 7);
        assert_eq!(priority(&deadline, midday(10)), 0.0);
        assert!(!is_overdue_deadline(&deadline, midday(10)));
        assert_eq!(priority(&deadline, midday(11)), 1_000_001.0);
        assert!(is_overdue_deadline(&deadline, midday(11)));

        let todo = mark(TemporalMarkKind::Todo, 10, 7);
        assert_eq!(priority(&todo, midday(17)), 0.0);
    }

    #[test]
    fn explicitly_timed_marks_keep_instant_semantics() {
        let reminder =
            TemporalMark::parse(TemporalMarkKind::Reminder, "2026-08-10 12:00 1d").unwrap();
        assert!(!reminder.date_only);
        assert_eq!(
            priority(&reminder, at(10) + Duration::hours(11)),
            f64::NEG_INFINITY
        );
        assert_eq!(priority(&reminder, at(10) + Duration::hours(12)), 0.0);
        assert!(priority(&reminder, at(10) + Duration::hours(15)) < 0.0);
    }

    #[test]
    fn skip_has_no_pace_and_never_surfaces_on_its_own() {
        assert!(TemporalMark::parse(TemporalMarkKind::Skip, "2026-08-10 2d").is_none());
        let mark = mark(TemporalMarkKind::Skip, 10, 0);
        assert_eq!(priority(&mark, at(9)), f64::NEG_INFINITY);
        assert_eq!(priority(&mark, at(11)), f64::NEG_INFINITY);
        assert_eq!(
            property_line(TemporalMarkKind::Skip, at(10), None),
            ":skip: 2026-08-10\n"
        );
    }

    #[test]
    fn parsing_defaults_and_date_only_writing_are_stable() {
        let parsed = TemporalMark::parse(TemporalMarkKind::Todo, "2026-08-10 12:30 9d").unwrap();
        assert_eq!(
            parsed.at,
            at(10) + Duration::hours(12) + Duration::minutes(30)
        );
        assert_eq!(parsed.pace_days, 9);
        assert!(!parsed.date_only);
        assert_eq!(
            property_line(TemporalMarkKind::Done, at(10), None),
            ":done: 2026-08-10\n"
        );
    }
}
