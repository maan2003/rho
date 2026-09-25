//! What the user did about a node, as the ledger holds it.
//!
//! A fact is one thing the user said, with when and where they said it:
//! done, mute, snooze, todo, a deadline. Facts are never edited. Each is
//! its own ledger key, `f/<node>|<id>`, so two devices never write over
//! each other, and taking one back (undo) removes its key. What a node's
//! facts come to right now is read off them in time order; nothing
//! summarises them in storage.

use jiff::{Timestamp, Zoned};
use senax_encoder::{Decode, Encode};

use crate::node::NodeId;
use crate::until::Until;

/// How far the user has dealt with a node: its source's position when
/// they said so.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Cursor {
    /// For a node whose source has no position: a note, a Slack unit
    /// (Slack keeps its own cursor).
    Done,
    /// An agent's story, through this position.
    Story(u64),
}

/// One thing the user said about a node.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Fact {
    /// When and where they said it.
    pub at: Zoned,
    pub said: Said,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Said {
    /// Dealt with through `through`. Takes back any snooze, todo and
    /// deadline said before it.
    Done {
        through: Cursor,
    },
    /// Nothing from the node reaches the user, until [`Said::Unmute`].
    /// Takes back what [`Said::Done`] does.
    Mute,
    Unmute,
    /// Out of the way until then.
    Snooze {
        until: Until,
    },
    /// Handled through `through`, and kept on the user's plate until done:
    /// from `start` when they named one, from now otherwise. Takes back an
    /// earlier snooze.
    Todo {
        through: Cursor,
        start: Option<Until>,
    },
    /// Due by `by`, pressing from `lead_days` before.
    Deadline {
        by: Until,
        lead_days: u32,
    },
}

/// A snooze in force or past: when it was said and when it ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snooze {
    pub set: Zoned,
    pub until: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Todo {
    pub set: Zoned,
    /// When it comes onto the plate.
    pub start: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deadline {
    pub set: Zoned,
    pub by: Timestamp,
    pub lead_days: u32,
}

fn prefix(node: &NodeId) -> String {
    format!("f/{}|", node.key())
}

/// The ledger key a fact on `node` is kept under.
pub fn key(node: &NodeId, id: uuid::Uuid) -> Vec<u8> {
    format!("{}{}", prefix(node), id.simple()).into_bytes()
}

/// The ledger write that records `fact` on `node`, under a new key. Keys
/// sort in the order this process made them, which is what orders two
/// facts said in the same instant.
pub fn record(node: &NodeId, fact: &Fact) -> crate::marks::Write {
    record_as(node, uuid::Uuid::now_v7(), fact)
}

pub(crate) fn record_as(node: &NodeId, id: uuid::Uuid, fact: &Fact) -> crate::marks::Write {
    (
        key(node, id),
        Some(senax_encoder::encode(fact).expect("encode a fact").to_vec()),
    )
}

/// A node's facts, oldest first, and what they come to.
#[derive(Clone, Copy, Debug, Default)]
pub struct Facts<'a>(pub &'a [Fact]);

impl<'a> Facts<'a> {
    /// The facts since the last one that takes everything back.
    fn since_cleared(self) -> &'a [Fact] {
        let from = self
            .0
            .iter()
            .rposition(|fact| matches!(fact.said, Said::Done { .. } | Said::Mute))
            .map_or(0, |at| at + 1);
        &self.0[from..]
    }

    /// How far the user has dealt with the node: the last done or todo.
    pub fn handled(self) -> Option<&'a Cursor> {
        self.0.iter().rev().find_map(|fact| match &fact.said {
            Said::Done { through } | Said::Todo { through, .. } => Some(through),
            _ => None,
        })
    }

    pub fn muted(self) -> bool {
        self.0
            .iter()
            .rev()
            .find_map(|fact| match fact.said {
                Said::Mute => Some(true),
                Said::Unmute => Some(false),
                _ => None,
            })
            .unwrap_or(false)
    }

    /// The last snooze, unless something since took it back. It may be
    /// over already: a snooze that ended still says when the node came
    /// back.
    pub fn snooze(self) -> Option<Snooze> {
        let live = self.since_cleared();
        let from = live
            .iter()
            .rposition(|fact| matches!(fact.said, Said::Todo { .. }))
            .map_or(0, |at| at + 1);
        live[from..].iter().rev().find_map(|fact| match fact.said {
            Said::Snooze { until } => Some(Snooze {
                set: fact.at.clone(),
                until: until.resolve(&fact.at),
            }),
            _ => None,
        })
    }

    pub fn todo(self) -> Option<Todo> {
        self.since_cleared()
            .iter()
            .rev()
            .find_map(|fact| match &fact.said {
                Said::Todo { start, .. } => Some(Todo {
                    set: fact.at.clone(),
                    start: start.map_or(fact.at.timestamp(), |start| start.resolve(&fact.at)),
                }),
                _ => None,
            })
    }

    pub fn deadline(self) -> Option<Deadline> {
        self.since_cleared()
            .iter()
            .rev()
            .find_map(|fact| match fact.said {
                Said::Deadline { by, lead_days } => Some(Deadline {
                    set: fact.at.clone(),
                    by: by.resolve(&fact.at),
                    lead_days,
                }),
                _ => None,
            })
    }

    /// How many times the user has snoozed the node, ever.
    /// Muted, or snoozed past `now`: the user put it away.
    pub fn put_away(self, now: Timestamp) -> bool {
        self.muted() || self.snooze().is_some_and(|snooze| snooze.until > now)
    }

    pub fn snoozes(self) -> usize {
        self.0
            .iter()
            .filter(|fact| matches!(fact.said, Said::Snooze { .. }))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use jiff::SignedDuration;

    use super::*;

    fn at(minutes: i64, said: Said) -> Fact {
        let noon: Timestamp = "2026-08-23T12:00:00Z".parse().unwrap();
        Fact {
            at: (noon + SignedDuration::from_mins(minutes)).to_zoned(jiff::tz::TimeZone::UTC),
            said,
        }
    }

    fn snooze(minutes: i64, hours: i64) -> Fact {
        at(
            minutes,
            Said::Snooze {
                until: Until::In(SignedDuration::from_hours(hours)),
            },
        )
    }

    #[test]
    fn done_takes_back_what_came_before_it_and_not_after() {
        let facts = [
            snooze(0, 1),
            at(
                1,
                Said::Todo {
                    through: Cursor::Story(3),
                    start: None,
                },
            ),
            at(
                2,
                Said::Done {
                    through: Cursor::Story(5),
                },
            ),
            snooze(3, 2),
        ];
        let facts = Facts(&facts);
        assert_eq!(facts.handled(), Some(&Cursor::Story(5)));
        assert_eq!(facts.todo(), None);
        let snoozed = facts.snooze().unwrap();
        assert_eq!(
            snoozed.until,
            snoozed.set.timestamp() + SignedDuration::from_hours(2)
        );
        assert_eq!(facts.snoozes(), 2);
    }

    #[test]
    fn a_todo_takes_back_an_earlier_snooze_and_mute_is_the_latest_word() {
        let facts = [
            snooze(0, 1),
            at(
                1,
                Said::Todo {
                    through: Cursor::Done,
                    start: None,
                },
            ),
            at(2, Said::Mute),
            at(3, Said::Unmute),
        ];
        let facts = Facts(&facts);
        assert_eq!(facts.snooze(), None);
        assert!(!facts.muted());
        assert_eq!(facts.todo(), None, "the mute took the todo back");
    }
}
