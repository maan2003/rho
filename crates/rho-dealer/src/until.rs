//! Times the user named, kept the way they named them.

use jiff::{SignedDuration, Timestamp, Zoned, civil};
use senax_encoder::{Decode, Encode};

/// A time the user named. It becomes an instant only against when and
/// where they said it: a day starts at midnight on the clock they were
/// on, and an hour is an hour on any clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Until {
    /// "in 1h": a length of time from when it was said.
    In(SignedDuration),
    /// "tomorrow": the start of that day.
    Day(civil::Date),
    /// "tomorrow at 9": that time on the clock.
    At(civil::DateTime),
}

impl Until {
    /// The instant this names, said at `said`.
    pub fn resolve(&self, said: &Zoned) -> Timestamp {
        let zone = said.time_zone().clone();
        let resolved = match *self {
            Self::In(ahead) => said.timestamp().checked_add(ahead).ok(),
            Self::Day(date) => date.to_zoned(zone).ok().map(|start| start.timestamp()),
            Self::At(at) => at.to_zoned(zone).ok().map(|at| at.timestamp()),
        };
        resolved.unwrap_or_else(|| said.timestamp())
    }
}

#[cfg(test)]
mod tests {
    use jiff::tz::{Offset, TimeZone};

    use super::*;

    fn ten_am(hours: i8) -> Zoned {
        civil::date(2026, 8, 23)
            .at(10, 0, 0, 0)
            .to_zoned(TimeZone::fixed(Offset::constant(hours)))
            .unwrap()
    }

    #[test]
    fn an_hour_is_an_hour_and_a_day_starts_at_the_clocks_own_midnight() {
        for hours in [-8, 0, 5, 14] {
            let said = ten_am(hours);
            assert_eq!(
                Until::In(SignedDuration::from_hours(1)).resolve(&said),
                said.timestamp() + SignedDuration::from_hours(1)
            );
            let tomorrow = Until::Day(civil::date(2026, 8, 24)).resolve(&said);
            assert_eq!(tomorrow, said.timestamp() + SignedDuration::from_hours(14));
        }
    }

    #[test]
    fn a_day_across_a_clock_change_is_still_its_midnight() {
        let zone = TimeZone::get("America/Los_Angeles").unwrap();
        let said = civil::date(2026, 11, 1)
            .at(0, 30, 0, 0)
            .to_zoned(zone.clone())
            .unwrap();
        let next = Until::Day(civil::date(2026, 11, 2)).resolve(&said);
        assert_eq!(
            next.to_zoned(zone).datetime(),
            civil::date(2026, 11, 2).at(0, 0, 0, 0)
        );
    }
}
