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

/// The time a snooze names and the row as shown to the user. A calendar
/// choice lands at 09:00 local time, not a count of hours across a clock
/// change.
pub fn snooze_target(unit: SnoozeUnit, count: i64, now: &Zoned) -> (Until, String) {
    let count = count.max(1);
    let (until, name) = match unit {
        SnoozeUnit::Minutes => (
            Until::In(SignedDuration::from_mins(count)),
            format!("{count} min"),
        ),
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
                SnoozeUnit::Minutes | SnoozeUnit::Hours => unreachable!(),
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
        SnoozeUnit::Minutes | SnoozeUnit::Hours if at.date() == now.date() => "%H:%M",
        SnoozeUnit::Days if count == 1 => "%H:%M",
        _ => "%a %H:%M",
    };
    (until, format!("{name} · {}", at.strftime(clock)))
}

/// The times `snooze thread ` and `snooze note ` offer before anything is
/// typed after them, each shown with when it lands.
pub const SNOOZE_PRESETS: [&str; 5] = ["1h", "3h", "evening", "tomorrow", "next week"];

/// What `clear` after `snooze thread ` or `snooze note ` says: no snooze
/// at all, rather than a time.
pub const CLEAR_SNOOZE: &str = "clear";

/// The times offered on a thread or note already snoozed: back `now`, or
/// [`CLEAR_SNOOZE`], before the usual ones.
pub const SNOOZED_PRESETS: [&str; 7] = [
    "now",
    CLEAR_SNOOZE,
    "1h",
    "3h",
    "evening",
    "tomorrow",
    "next week",
];

/// The hour `evening` lands on.
const EVENING: i8 = 18;

const HOW_LONG: &str = "say how long, such as 45m, 3h, 2d, evening, tomorrow or mon 9am";

/// What a snooze typed after its name says (`:snooze thread 3h`): a count
/// and a unit (`45m`, `3h`, `2d`, `1w`), a preset (`now`, `evening`,
/// `tomorrow`, `next week`), a day with an optional time (`mon`,
/// `mon 9am`), or a time alone (`17:30`, `5pm`), which is today while it
/// is still ahead.
pub fn read_snooze(text: &str, now: &Zoned) -> Result<Until, &'static str> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.to_lowercase();
    let tomorrow = |date: jiff::civil::Date| date.tomorrow().unwrap_or(date);
    match text.as_str() {
        "now" => return Ok(Until::In(SignedDuration::ZERO)),
        "tomorrow" => return Ok(snooze_target(SnoozeUnit::Days, 1, now).0),
        "next week" => return Ok(snooze_target(SnoozeUnit::Weeks, 1, now).0),
        "evening" | "tonight" => {
            let date = match now.hour() < EVENING {
                true => now.date(),
                false => tomorrow(now.date()),
            };
            return Ok(Until::At(date.at(EVENING, 0, 0, 0)));
        }
        _ => {}
    }
    if let Some((day, rest)) = weekday(&text) {
        let time = match rest.trim() {
            "" => jiff::civil::time(9, 0, 0, 0),
            rest => clock(rest).ok_or(HOW_LONG)?,
        };
        let mut date = now.date();
        while date.weekday() != day || date.at(time.hour(), time.minute(), 0, 0) <= now.datetime() {
            date = tomorrow(date);
        }
        return Ok(Until::At(date.at(time.hour(), time.minute(), 0, 0)));
    }
    if let Some(time) = clock(&text) {
        let today = now.date().to_datetime(time);
        return Ok(Until::At(match today > now.datetime() {
            true => today,
            false => tomorrow(now.date()).to_datetime(time),
        }));
    }
    let (unit, count) = counted(&text)?;
    Ok(snooze_target(unit, i64::from(count), now).0)
}

/// `3h`, `45 min`, `h`: a count, one when none is said, and a unit.
fn counted(text: &str) -> Result<(SnoozeUnit, u32), &'static str> {
    let digits = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let count = match &text[..digits] {
        "" => 1,
        digits => digits
            .parse::<u32>()
            .ok()
            .filter(|count| *count > 0)
            .ok_or(HOW_LONG)?,
    };
    let unit = match text[digits..].trim() {
        "m" | "min" | "mins" | "minute" | "minutes" => SnoozeUnit::Minutes,
        "h" | "hr" | "hrs" | "hour" | "hours" => SnoozeUnit::Hours,
        "d" | "day" | "days" => SnoozeUnit::Days,
        "w" | "wk" | "week" | "weeks" => SnoozeUnit::Weeks,
        _ => return Err(HOW_LONG),
    };
    Ok((unit, count))
}

/// A day named at the start of `text`, and what follows it.
fn weekday(text: &str) -> Option<(jiff::civil::Weekday, &str)> {
    use jiff::civil::Weekday::*;
    let (word, rest) = text.split_once(' ').unwrap_or((text, ""));
    let day = match word {
        "mon" | "monday" => Monday,
        "tue" | "tues" | "tuesday" => Tuesday,
        "wed" | "wednesday" => Wednesday,
        "thu" | "thur" | "thurs" | "thursday" => Thursday,
        "fri" | "friday" => Friday,
        "sat" | "saturday" => Saturday,
        "sun" | "sunday" => Sunday,
        _ => return None,
    };
    Some((day, rest))
}

/// A time on the clock: `9am`, `9:30`, `17:30`, `5 pm`. A bare number is
/// no time, so `3` is never three o'clock.
fn clock(text: &str) -> Option<jiff::civil::Time> {
    let text = text.replace(' ', "");
    let (digits, half) = match (text.strip_suffix("am"), text.strip_suffix("pm")) {
        (Some(digits), _) => (digits, Some(false)),
        (_, Some(digits)) => (digits, Some(true)),
        _ => (text.as_str(), None),
    };
    let (hour, minute) = match digits.split_once(':') {
        Some((hour, minute)) if minute.len() == 2 => (hour, minute.parse::<i8>().ok()?),
        Some(_) => return None,
        None if half.is_some() => (digits, 0),
        None => return None,
    };
    if hour.is_empty() || hour.len() > 2 {
        return None;
    }
    let hour = hour.parse::<i8>().ok()?;
    let hour = match half {
        Some(pm) if (1..=12).contains(&hour) => hour % 12 + if pm { 12 } else { 0 },
        Some(_) => return None,
        None => hour,
    };
    jiff::civil::Time::new(hour, minute, 0, 0).ok()
}

/// Where a snooze lands, as its row in `Commands` previews it: `→ now`,
/// `→ today 19:56`, `→ tomorrow 09:00`, `→ Sat 09:00` within the week, and
/// the date beyond it.
pub fn snooze_preview(until: &Until, now: &Zoned) -> String {
    let at = until.resolve(now).to_zoned(now.time_zone().clone());
    if at.timestamp() <= now.timestamp() {
        return "→ now".to_owned();
    }
    let days = (at.date() - now.date()).get_days();
    let day = match days {
        0 => "today".to_owned(),
        1 => "tomorrow".to_owned(),
        2..=6 => at.strftime("%a").to_string(),
        _ => at.strftime("%a %-d %b").to_string(),
    };
    format!("→ {day} {}", at.strftime("%H:%M"))
}

/// The bar's words for a snooze: `snooze until 05:17`, or `back now` for
/// one that ends as it is said.
pub fn snooze_said(at: &Zoned, now: &Zoned) -> String {
    match at.timestamp() <= now.timestamp() {
        true => "back now".to_owned(),
        false => format!("snooze {}", until_said(at, now)),
    }
}

/// When a snooze ends, on the user's clock: the hour alone when it is
/// still today, the day in front of it when it is not, and the day alone
/// when it ends at the start of one.
pub fn until_said(at: &Zoned, now: &Zoned) -> String {
    format!("until {}", clock_said(at, now))
}

/// A note's reminder, on its status line and its row in `Notes`: when it
/// comes due, as [`until_said`] says it, or that it is due.
pub fn reminder_said(at: &Zoned, now: &Zoned) -> String {
    match at.timestamp() <= now.timestamp() {
        true => "reminder due".to_owned(),
        false => format!("reminder {}", clock_said(at, now)),
    }
}

fn clock_said(at: &Zoned, now: &Zoned) -> String {
    match (
        at.date() == now.date(),
        at.time() == jiff::civil::Time::midnight(),
    ) {
        (true, _) => at.strftime("%H:%M").to_string(),
        (false, true) => at.strftime("%a %-d %b").to_string(),
        (false, false) => at.strftime("%a %-d %b %H:%M").to_string(),
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

    /// What `read_snooze` lands on, as its preview says it.
    fn lands(typed: &str, now: &Zoned) -> Result<String, &'static str> {
        read_snooze(typed, now).map(|until| snooze_preview(&until, now))
    }

    /// 10:00 on Sunday 23 Aug 2026, UTC−8.
    #[test]
    fn typed_snoozes_land_where_the_user_means() {
        let now = ten_am(-8);
        for (typed, preview) in [
            ("45m", "→ today 10:45"),
            ("45 min", "→ today 10:45"),
            ("3h", "→ today 13:00"),
            ("3 hours", "→ today 13:00"),
            ("h", "→ today 11:00"),
            ("1h", "→ today 11:00"),
            ("evening", "→ today 18:00"),
            (" Tomorrow ", "→ tomorrow 09:00"),
            ("2d", "→ Tue 09:00"),
            // A Sunday's next week is tomorrow, and the week after that.
            ("next week", "→ tomorrow 09:00"),
            ("1w", "→ tomorrow 09:00"),
            ("2 weeks", "→ Mon 31 Aug 09:00"),
            ("mon 9am", "→ tomorrow 09:00"),
            ("fri", "→ Fri 09:00"),
            ("sat 14:30", "→ Sat 14:30"),
            // Today's own day, still ahead today, or a week on.
            ("sun 5pm", "→ today 17:00"),
            ("sun 9am", "→ Sun 30 Aug 09:00"),
            ("17:30", "→ today 17:30"),
            ("9am", "→ tomorrow 09:00"),
            ("12pm", "→ today 12:00"),
            ("12am", "→ tomorrow 00:00"),
            ("now", "→ now"),
            (" NOW", "→ now"),
        ] {
            assert_eq!(lands(typed, &now).as_deref(), Ok(preview), "{typed:?}");
        }
        for wrong in [
            "",
            "3",
            "0h",
            "3x",
            "h3",
            "-2d",
            "soon",
            "99999999999h",
            "13pm",
            "9:3",
            "mon 3",
            "25:00",
        ] {
            assert!(read_snooze(wrong, &now).is_err(), "{wrong:?}");
        }
    }

    #[test]
    fn evening_after_six_is_tomorrows() {
        let late = ten_am(0).with().hour(19).build().unwrap();
        assert_eq!(lands("evening", &late).as_deref(), Ok("→ tomorrow 18:00"));
    }

    #[test]
    fn a_snooze_that_ends_now_says_so() {
        let now = ten_am(2);
        assert_eq!(snooze_said(&now, &now), "back now");
        let later = now.checked_add(SignedDuration::from_mins(90)).unwrap();
        assert_eq!(snooze_said(&later, &now), "snooze until 11:30");
    }

    #[test]
    fn every_preset_reads() {
        let now = ten_am(3);
        for preset in SNOOZE_PRESETS {
            assert!(read_snooze(preset, &now).is_ok(), "{preset}");
        }
        // `clear` is no time: the command reads it before the clock does.
        for preset in SNOOZED_PRESETS {
            assert_eq!(
                read_snooze(preset, &now).is_ok(),
                preset != CLEAR_SNOOZE,
                "{preset}"
            );
        }
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
