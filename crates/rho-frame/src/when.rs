//! Where the user's own clock comes in: what the user says ("in an hour",
//! "tomorrow") as times named on their clock, and back into words.

use jiff::{SignedDuration, Zoned};
use rho_dealer::Until;

/// The units a snooze is counted in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnoozeUnit {
    Minutes,
    Hours,
    Days,
    Weeks,
}

/// Where a snooze lands and how the bar says it. Minutes and hours are a
/// length of time, so a card can come back this afternoon; days and weeks
/// land on the start of a day, which is what a defer has always been.
pub fn snooze_target(unit: SnoozeUnit, count: i64, now: &Zoned) -> (Until, String) {
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
            let at = date
                .to_zoned(now.time_zone().clone())
                .unwrap_or_else(|_| now.clone());
            (Until::Day(date), snooze_said(&at, now))
        }
    }
}

/// The bar's words for a snooze: `snooze until 05:17`.
pub fn snooze_said(at: &Zoned, now: &Zoned) -> String {
    format!("snooze {}", until_said(at, now))
}

/// When a snooze ends, on the user's clock: the hour alone when it is
/// still today, the day in front of it when it is not, and the day alone
/// when it ends at the start of one.
pub fn until_said(at: &Zoned, now: &Zoned) -> String {
    match (
        at.date() == now.date(),
        at.time() == jiff::civil::Time::midnight(),
    ) {
        (true, _) => format!("until {}", at.strftime("%H:%M")),
        (false, true) => format!("until {}", at.strftime("%a %-d %b")),
        (false, false) => format!("until {}", at.strftime("%a %-d %b %H:%M")),
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
}
