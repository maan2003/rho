//! Where the user's own clock comes in: what the user says ("in an hour",
//! "tomorrow", "tonight") as times named on their clock, and back into
//! words.

use jiff::civil::Date;
use jiff::{SignedDuration, Zoned};
use rho_dealer::Until;

use crate::workspace::SnoozeUnit;

/// A named hour of the day, for the phone's `tonight` and `tomorrow`.
/// `tonight` is this evening while it is still ahead and the next one after
/// that; `tomorrow` is always the next day, even when read before nine.
pub(crate) fn named_hour(hour: u32, tomorrow: bool, now: &Zoned) -> Zoned {
    let hour = i8::try_from(hour.min(23)).unwrap_or(0);
    let at = |day: Date| {
        day.at(hour, 0, 0, 0)
            .to_zoned(now.time_zone().clone())
            .unwrap_or_else(|_| now.clone())
    };
    let mut day = now.date();
    if tomorrow || at(day) <= *now {
        day = day.tomorrow().unwrap_or(day);
    }
    at(day)
}

/// Where a snooze lands and how the bar says it. Minutes and hours are a
/// length of time, so a card can come back this afternoon; days and weeks
/// land on the start of a day, which is what a defer has always been.
pub(crate) fn snooze_target(unit: SnoozeUnit, count: i64, now: &Zoned) -> (Until, String) {
    match unit {
        SnoozeUnit::Minutes | SnoozeUnit::Hours => {
            let ahead = match unit {
                SnoozeUnit::Minutes => SignedDuration::from_mins(count),
                _ => SignedDuration::from_hours(count),
            };
            let at = now.checked_add(ahead).unwrap_or_else(|_| now.clone());
            (Until::In(ahead), snooze_said(&at, now))
        }
        SnoozeUnit::Days | SnoozeUnit::Weeks => {
            let days = match unit {
                SnoozeUnit::Days => count,
                _ => count * 7,
            };
            let date = now
                .date()
                .checked_add(jiff::Span::new().days(days))
                .unwrap_or(now.date());
            (
                Until::Day(date),
                format!("snooze until {}", date.strftime("%a %-d %b")),
            )
        }
    }
}

/// The bar's words for a snooze with a clock time: the hour alone when it
/// is still today, the day in front of it when it is not.
pub(crate) fn snooze_said(at: &Zoned, now: &Zoned) -> String {
    match at.date() == now.date() {
        true => format!("snooze until {}", at.strftime("%H:%M")),
        false => format!("snooze until {}", at.strftime("%a %-d %b %H:%M")),
    }
}

#[cfg(test)]
mod tests {
    use jiff::tz::{Offset, TimeZone};

    use super::*;

    /// 10:00 on 23 Aug 2026 on a clock `hours` east of UTC.
    fn ten_am(hours: i8) -> Zoned {
        jiff::civil::date(2026, 8, 23)
            .at(10, 0, 0, 0)
            .to_zoned(TimeZone::fixed(Offset::constant(hours)))
            .unwrap()
    }

    #[test]
    fn an_hour_is_an_hour_on_any_clock() {
        for hours in [5, -8, 0, 14] {
            let now = ten_am(hours);
            let (until, said) = snooze_target(SnoozeUnit::Hours, 1, &now);
            assert_eq!(
                until.resolve(&now),
                now.timestamp() + SignedDuration::from_hours(1)
            );
            assert_eq!(said, "snooze until 11:00");
        }
    }

    #[test]
    fn a_day_starts_at_the_users_own_midnight() {
        for hours in [5, -8, 0, 14] {
            let now = ten_am(hours);
            let (until, said) = snooze_target(SnoozeUnit::Days, 1, &now);
            let midnight = jiff::civil::date(2026, 8, 24)
                .to_zoned(now.time_zone().clone())
                .unwrap();
            assert_eq!(until.resolve(&now), midnight.timestamp(), "UTC{hours:+}");
            assert_eq!(said, "snooze until Mon 24 Aug");
        }
    }

    #[test]
    fn tonight_is_this_evening_until_it_has_passed() {
        let now = ten_am(5);
        assert_eq!(named_hour(18, false, &now).date(), now.date());
        let late = now.checked_add(SignedDuration::from_hours(9)).unwrap();
        assert_eq!(
            named_hour(18, false, &late).date(),
            now.date().tomorrow().unwrap()
        );
        assert_eq!(
            named_hour(9, true, &now).date(),
            now.date().tomorrow().unwrap()
        );
    }
}
