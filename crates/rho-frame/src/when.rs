//! Where the user's own clock comes in: what the user says ("in an hour",
//! "tomorrow") as times named on their clock, and back into words.

use jiff::{SignedDuration, Zoned};
use rho_dealer::Until;

/// The units a snooze is counted in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnoozeUnit {
    Hours,
    Days,
    Weeks,
}

/// The time a snooze names and the row as shown to the user. A calendar
/// choice lands at 09:00 local time, not a count of hours across a clock
/// change.
pub fn snooze_target(unit: SnoozeUnit, count: i64, now: &Zoned) -> (Until, String) {
    let count = count.max(1);
    let (until, name) = match unit {
        SnoozeUnit::Hours => (
            Until::In(SignedDuration::from_hours(count)),
            if count == 1 {
                "1 hour".to_owned()
            } else {
                format!("{count} hours")
            },
        ),
        SnoozeUnit::Days | SnoozeUnit::Weeks => {
            let (days, name) = match unit {
                SnoozeUnit::Days if count == 1 => (1, "tomorrow".to_owned()),
                SnoozeUnit::Days => (count, format!("{count} days")),
                SnoozeUnit::Weeks => (
                    7 * count - i64::from(now.weekday().to_monday_zero_offset()),
                    if count == 1 {
                        "next week".to_owned()
                    } else {
                        format!("{count} weeks")
                    },
                ),
                SnoozeUnit::Hours => unreachable!(),
            };
            let date = now
                .date()
                .checked_add(jiff::Span::new().days(days.clamp(1, 365_000)))
                .unwrap_or(jiff::civil::Date::MAX);
            (Until::At(date.at(9, 0, 0, 0)), name)
        }
    };
    let at = until.resolve(now).to_zoned(now.time_zone().clone());
    let clock = match unit {
        SnoozeUnit::Hours if at.date() == now.date() => "%H:%M",
        SnoozeUnit::Days if count == 1 => "%H:%M",
        _ => "%a %H:%M",
    };
    (until, format!("{name} · {}", at.strftime(clock)))
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
            assert_eq!(said, "1 hour · 11:00");
        }
    }

    #[test]
    fn tomorrow_is_nine_in_the_users_zone() {
        for hours in [5, -8, 0, 14] {
            let now = ten_am(hours);
            let (until, said) = snooze_target(SnoozeUnit::Days, 1, &now);
            let nine = jiff::civil::date(2026, 8, 24)
                .at(9, 0, 0, 0)
                .to_zoned(now.time_zone().clone())
                .unwrap();
            assert_eq!(until.resolve(&now), nine.timestamp(), "UTC{hours:+}");
            assert_eq!(said, "tomorrow · 09:00");
        }
    }
    #[test]
    fn the_week_row_counts_mondays_and_matches_the_actual_instant() {
        let now = ten_am(-8);
        let (first, name) = snooze_target(SnoozeUnit::Weeks, 1, &now);
        let (third, third_name) = snooze_target(SnoozeUnit::Weeks, 3, &now);
        assert_eq!(name, "next week · Mon 09:00");
        assert_eq!(third_name, "3 weeks · Mon 09:00");
        assert_eq!(
            now.timestamp().duration_until(first.resolve(&now)),
            SignedDuration::from_hours(23)
        );
        assert_eq!(
            first.resolve(&now).duration_until(third.resolve(&now)),
            SignedDuration::from_hours(14 * 24)
        );
    }
}
