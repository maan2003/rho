//! What the user said, as the ledger holds it.
//!
//! An [`Entry`] is one thing the user said: which device said it, when
//! and where, and the [`Fact`]. Entries are never edited or removed; an
//! undo is a [`Fact::Retract`] of its own. Every device holds every
//! device's entries, and what they come to is read off them in time
//! order, so merging two devices is only putting their entries together.
//!
//! A fact is kept as the user said it: a snooze "until tomorrow" is
//! `Until::Day`, read against the zone it was said in. How far the user
//! had seen when they were done is in the source's own terms ([`Seen`]),
//! never a time, so it means the same on every device.

use std::cmp::Ordering;

use jiff::{Timestamp, Zoned};
use senax_encoder::{Decode, Encode};

use crate::node::NodeId;
use crate::until::Until;

/// A device that writes entries. The ledger names it; here it only tells
/// entries said in the same instant apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
pub struct Device(pub [u8; 16]);

/// An entry, by who said it and when: what a retract names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
pub struct EntryId {
    pub at: Timestamp,
    pub device: Device,
}

/// One thing the user said.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Entry {
    pub device: Device,
    /// When and where it was said. A device never writes an instant at or
    /// before one it has already seen, so what it says after reading
    /// another device's entry sorts after it.
    pub at: Zoned,
    pub fact: Fact,
}

impl Entry {
    pub fn id(&self) -> EntryId {
        EntryId {
            at: self.at.timestamp(),
            device: self.device,
        }
    }

    /// The payload the ledger carries.
    pub fn encode(&self) -> Vec<u8> {
        senax_encoder::encode(self)
            .expect("encode an entry")
            .to_vec()
    }

    /// An entry read back; `None` for one this build cannot read, which a
    /// newer build may have written.
    pub fn decode(mut bytes: &[u8]) -> Option<Self> {
        senax_encoder::decode(&mut bytes).ok()
    }
}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> Ordering {
        self.id().cmp(&other.id())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Fact {
    /// Out of the way until then.
    Snooze {
        node: NodeId,
        until: Until,
    },
    /// On the user's plate until settled: from `start`, or from when it was
    /// said. Handles what the user had seen, and takes back a snooze.
    Todo {
        node: NodeId,
        start: Option<Until>,
        seen: Seen,
    },
    /// Due by `by`, pressing from `lead_days` before.
    Deadline {
        node: NodeId,
        by: Until,
        lead_days: u32,
    },
    /// Done, having seen this far: takes back the node's todo, deadline
    /// and snooze.
    Settled {
        node: NodeId,
        seen: Seen,
    },
    /// Nothing from the node reaches the user, until unmuted. For every
    /// node but a Slack one, which Slack mutes itself.
    Mute {
        node: NodeId,
    },
    Unmute {
        node: NodeId,
    },

    /// Makes a label, or renames or moves it.
    Label {
        label: uuid::Uuid,
        name: String,
        parent: Option<uuid::Uuid>,
    },
    /// Deletes a label.
    Unlabel {
        label: uuid::Uuid,
    },
    /// Where work under a label happens.
    Repository {
        label: uuid::Uuid,
        url: Option<String>,
    },
    Labeled {
        node: NodeId,
        label: uuid::Uuid,
        present: bool,
    },
    /// What the user calls a node, over what its source calls it.
    Named {
        node: NodeId,
        name: Option<String>,
    },
    /// What a node is about.
    About {
        node: NodeId,
        about: Option<NodeId>,
    },

    /// Takes back an entry: undo.
    Retract {
        of: EntryId,
    },
}

impl Fact {
    /// The node the fact is about; `None` for a retract.
    pub fn node(&self) -> Option<NodeId> {
        match self {
            Self::Snooze { node, .. }
            | Self::Todo { node, .. }
            | Self::Deadline { node, .. }
            | Self::Settled { node, .. }
            | Self::Mute { node }
            | Self::Unmute { node }
            | Self::Labeled { node, .. }
            | Self::Named { node, .. }
            | Self::About { node, .. } => Some(node.clone()),
            Self::Label { label, .. }
            | Self::Unlabel { label }
            | Self::Repository { label, .. } => Some(NodeId::Label(*label)),
            Self::Retract { .. } => None,
        }
    }
}

/// How far the user had seen, in the source's own terms.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Seen {
    /// An agent's story, through this position.
    Agent(u64),
    /// A Slack unit, through this message.
    Slack(String),
    /// A note or a label: there is nothing more to see.
    Whole,
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

/// A node's attention entries, oldest first and none of them retracted,
/// and what they come to.
#[derive(Clone, Copy, Debug, Default)]
pub struct Facts<'a>(pub &'a [Entry]);

impl<'a> Facts<'a> {
    /// The entries since the last one that takes everything back.
    fn since_cleared(self) -> &'a [Entry] {
        let from = self
            .0
            .iter()
            .rposition(|entry| matches!(entry.fact, Fact::Settled { .. } | Fact::Mute { .. }))
            .map_or(0, |at| at + 1);
        &self.0[from..]
    }

    fn seen(self) -> impl Iterator<Item = &'a Seen> {
        self.0.iter().filter_map(|entry| match &entry.fact {
            Fact::Settled { seen, .. } | Fact::Todo { seen, .. } => Some(seen),
            _ => None,
        })
    }

    /// How far into an agent's story the user has seen when done with it.
    pub fn seen_agent(self) -> Option<u64> {
        self.seen()
            .filter_map(|seen| match seen {
                Seen::Agent(pos) => Some(*pos),
                _ => None,
            })
            .max()
    }

    /// The newest Slack message the user had seen when done with a unit.
    pub fn seen_slack(self) -> Option<&'a str> {
        self.seen()
            .filter_map(|seen| match seen {
                Seen::Slack(ts) => Some(ts.as_str()),
                _ => None,
            })
            .max_by(|a, b| slack_ts_order(a, b))
    }

    pub fn muted(self) -> bool {
        self.0
            .iter()
            .rev()
            .find_map(|entry| match entry.fact {
                Fact::Mute { .. } => Some(true),
                Fact::Unmute { .. } => Some(false),
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
            .rposition(|entry| matches!(entry.fact, Fact::Todo { .. }))
            .map_or(0, |at| at + 1);
        live[from..]
            .iter()
            .rev()
            .find_map(|entry| match &entry.fact {
                Fact::Snooze { until, .. } => Some(Snooze {
                    set: entry.at.clone(),
                    until: until.resolve(&entry.at),
                }),
                _ => None,
            })
    }

    pub fn todo(self) -> Option<Todo> {
        self.since_cleared()
            .iter()
            .rev()
            .find_map(|entry| match &entry.fact {
                Fact::Todo { start, .. } => Some(Todo {
                    set: entry.at.clone(),
                    start: start.map_or(entry.at.timestamp(), |start| start.resolve(&entry.at)),
                }),
                _ => None,
            })
    }

    pub fn deadline(self) -> Option<Deadline> {
        self.since_cleared()
            .iter()
            .rev()
            .find_map(|entry| match &entry.fact {
                Fact::Deadline { by, lead_days, .. } => Some(Deadline {
                    set: entry.at.clone(),
                    by: by.resolve(&entry.at),
                    lead_days: *lead_days,
                }),
                _ => None,
            })
    }

    /// Muted, or snoozed past `now`: the user put it away.
    pub fn put_away(self, now: Timestamp) -> bool {
        self.muted() || self.snooze().is_some_and(|snooze| snooze.until > now)
    }

    /// How many times the user has snoozed the node, ever.
    pub fn snoozes(self) -> usize {
        self.0
            .iter()
            .filter(|entry| matches!(entry.fact, Fact::Snooze { .. }))
            .count()
    }
}

/// Slack timestamps in time order: seconds, then the sequence after the
/// dot.
pub fn slack_ts_order(a: &str, b: &str) -> Ordering {
    let parts = |ts: &str| {
        let (seconds, sequence) = ts.split_once('.').unwrap_or((ts, "0"));
        (
            seconds.parse::<u64>().unwrap_or(0),
            sequence.parse::<u64>().unwrap_or(0),
        )
    };
    parts(a).cmp(&parts(b))
}

#[cfg(test)]
mod tests {
    use jiff::tz::TimeZone;

    use super::*;

    fn at(minute: i64) -> Zoned {
        Timestamp::from_second(1_800_000_000 + minute * 60)
            .unwrap()
            .to_zoned(TimeZone::get("Europe/Berlin").unwrap())
    }

    fn entry(minute: i64, fact: Fact) -> Entry {
        Entry {
            device: Device([1; 16]),
            at: at(minute),
            fact,
        }
    }

    fn agent() -> NodeId {
        NodeId::Agent(rho_agent_types::AgentId::from_encoded("00jvj4xuk96p").unwrap())
    }

    #[test]
    fn an_entry_reads_back_as_it_was_said() {
        let said = entry(
            0,
            Fact::Snooze {
                node: agent(),
                until: Until::Day(jiff::civil::date(2027, 1, 16)),
            },
        );
        let back = Entry::decode(&said.encode()).unwrap();
        assert_eq!(back, said);
        assert_eq!(back.at.time_zone().iana_name(), Some("Europe/Berlin"));
    }

    #[test]
    fn seen_only_grows_whatever_order_it_was_said_in() {
        let settled = |minute, pos| {
            entry(
                minute,
                Fact::Settled {
                    node: agent(),
                    seen: Seen::Agent(pos),
                },
            )
        };
        let entries = [settled(0, 812), settled(1, 800)];
        assert_eq!(Facts(&entries).seen_agent(), Some(812));
    }

    #[test]
    fn settled_takes_back_the_todo_and_the_snooze_before_it() {
        let node = agent();
        let entries = [
            entry(
                0,
                Fact::Todo {
                    node: node.clone(),
                    start: None,
                    seen: Seen::Agent(3),
                },
            ),
            entry(
                1,
                Fact::Snooze {
                    node: node.clone(),
                    until: Until::In(jiff::SignedDuration::from_hours(1)),
                },
            ),
            entry(
                2,
                Fact::Settled {
                    node,
                    seen: Seen::Agent(4),
                },
            ),
        ];
        let facts = Facts(&entries);
        assert_eq!(facts.todo(), None);
        assert_eq!(facts.snooze(), None);
        assert_eq!(facts.snoozes(), 1, "every snooze is kept");
        assert_eq!(facts.seen_agent(), Some(4));
    }

    #[test]
    fn slack_messages_order_by_time_not_by_text() {
        assert_eq!(
            slack_ts_order("1800000100.000010", "1800000100.000009"),
            Ordering::Greater
        );
        assert_eq!(
            slack_ts_order("999999999.000000", "1800000000.000000"),
            Ordering::Less
        );
    }
}
