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

use crate::address::Address;
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
        node: Address,
        until: Until,
    },
    /// Was a todo: on the user's plate until settled. No build writes it
    /// any more, as todos went with the rooms (docs/rho2.md); it stays so
    /// old entries decode. One still in a log reads as [`Fact::Read`]: it
    /// counts as seen and takes nothing back.
    Todo {
        node: Address,
        start: Option<Until>,
        seen: Seen,
    },
    /// Due by `by`, pressing from `lead_days` before.
    Deadline {
        node: Address,
        by: Until,
        lead_days: u32,
    },
    /// Done, having seen this far: takes back the node's deadline and
    /// snooze.
    Settled {
        node: Address,
        seen: Seen,
    },
    /// Nothing from the node reaches the user, until unmuted.
    Mute {
        node: Address,
    },
    Unmute {
        node: Address,
    },

    /// Made, renamed or moved a label. Labels went with the rooms
    /// (docs/rho2.md): no build writes this or the other label facts
    /// (`Unlabel`, `Repository`, `Host`, `Labeled`) any more, and nothing
    /// reads them. They stay so old entries decode.
    Label {
        label: uuid::Uuid,
        name: String,
        parent: Option<uuid::Uuid>,
    },
    /// Deleted a label. Written by no build, read by none.
    Unlabel {
        label: uuid::Uuid,
    },
    /// Where work under a label happened. Written by no build, read by
    /// none.
    Repository {
        label: uuid::Uuid,
        url: Option<String>,
    },
    /// Put a label on a node or took it off. Written by no build, read by
    /// none.
    Labeled {
        node: Address,
        label: uuid::Uuid,
        present: bool,
    },
    /// What the user calls a node, over what its source calls it.
    Named {
        node: Address,
        name: Option<String>,
    },
    /// What a node is about.
    About {
        node: Address,
        about: Option<Address>,
    },

    /// Takes back an entry: undo.
    Retract {
        of: EntryId,
    },
    /// Was put away on a pile the user named. No build writes it any
    /// more, as piles went with the rooms (docs/rho2.md); it stays so old
    /// entries decode. One still in a log reads as [`Fact::Settled`] said
    /// at the same instant: the user put the node away, so it takes back
    /// the deadline and snooze before it. It carries no [`Seen`], so it
    /// moves nobody's cursor. Later than `Retract`, as entries decode by
    /// variant order.
    Piled {
        node: Address,
        pile: String,
    },
    /// Which agent host work under a label ran on. Written by no build,
    /// read by none. Later still than `Piled`, for the same reason.
    Host {
        label: uuid::Uuid,
        host: Option<String>,
    },
    /// Seen this far, taking nothing back. Older builds wrote it for
    /// "marked read"; done is how the user says they read a topic now.
    /// Steering an agent writes it: the user has seen the stop the agent
    /// is in, and what else waits in its thread still waits. Later still
    /// than `Host`.
    Read {
        node: Address,
        seen: Seen,
    },
    /// The user's own text as a message, written or revised: the newest
    /// revision is the message. `created` is when it was first written,
    /// the same on every revision. Later still than `Read`.
    Wrote {
        note: uuid::Uuid,
        text: String,
        created: Timestamp,
    },
    /// The message removed. A revision said after it brings it back, and
    /// a retract of this entry too.
    Deleted {
        note: uuid::Uuid,
    },
    /// A page the user keeps by name: `node` is the system's own address
    /// for it, which the browser mints. The newest one said is the
    /// bookmark. Later still than `Deleted`.
    Bookmarked {
        node: Address,
        title: String,
        url: String,
    },
    /// The bookmark removed. A `Bookmarked` said after it brings it back,
    /// and a retract of this entry too.
    Unbookmarked {
        node: Address,
    },
}

impl Fact {
    /// The node the fact is about; `None` for a retract, and for the label
    /// facts, which nothing reads.
    pub fn node(&self) -> Option<Address> {
        match self {
            Self::Snooze { node, .. }
            | Self::Piled { node, .. }
            | Self::Todo { node, .. }
            | Self::Deadline { node, .. }
            | Self::Settled { node, .. }
            | Self::Mute { node }
            | Self::Unmute { node }
            | Self::Named { node, .. }
            | Self::About { node, .. }
            | Self::Read { node, .. }
            | Self::Bookmarked { node, .. }
            | Self::Unbookmarked { node } => Some(node.clone()),
            Self::Wrote { note, .. } | Self::Deleted { note } => Some(Address::note(*note)),
            Self::Label { .. }
            | Self::Unlabel { .. }
            | Self::Repository { .. }
            | Self::Host { .. }
            | Self::Labeled { .. }
            | Self::Retract { .. } => None,
        }
    }
}

/// How far the user had seen, in the source's own terms.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub enum Seen {
    /// An agent's story, through this position: what agent cards read
    /// before topics replaced them. Only the migration to topics reads
    /// it; it stays so `main`'s entries still decode.
    Agent(u64),
    /// A note or a user-assigned date: there is nothing more to see.
    #[default]
    Whole,
    /// A chat thread read through this position in its host's log: every
    /// message and edit at or before it. What done says on a thread now.
    Topic(u64),
    /// A chat thread, through this message, and through every stop its
    /// agents were in that began by `stopped`, in unix milliseconds on
    /// the host's clock: a stop is no message, so the instant its host
    /// gave it is where it stands.
    Thread { message: u64, stopped: u64 },
    /// One dealt mail and the version read, both positions in its host's
    /// log: what done said on a thread before it settled the whole thread.
    /// Older entries still read.
    Mail { message: u64, version: u64 },
}

/// A snooze in force or past: when it was said and when it ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snooze {
    pub set: Zoned,
    pub until: Timestamp,
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
    /// The entries since the last one that takes everything back: done, an
    /// old pile, which reads as done, or a mute.
    fn since_cleared(self) -> &'a [Entry] {
        let from = self
            .0
            .iter()
            .rposition(|entry| {
                matches!(
                    entry.fact,
                    Fact::Settled { .. } | Fact::Piled { .. } | Fact::Mute { .. }
                )
            })
            .map_or(0, |at| at + 1);
        &self.0[from..]
    }

    /// How far the user had seen, by every fact that says: done, and an
    /// old todo or read.
    fn seen(self) -> impl Iterator<Item = &'a Seen> {
        self.0.iter().filter_map(|entry| match &entry.fact {
            Fact::Settled { seen, .. } | Fact::Todo { seen, .. } | Fact::Read { seen, .. } => {
                Some(seen)
            }
            _ => None,
        })
    }

    /// How far the thread is read: the furthest done, or old read, said
    /// on it.
    pub fn seen_topic(self) -> Option<u64> {
        self.seen()
            .filter_map(|seen| match seen {
                Seen::Topic(id) | Seen::Thread { message: id, .. } => Some(*id),
                _ => None,
            })
            .max()
    }

    /// Individual mail and version pairs an older build's verdict took out
    /// of the deal.
    pub fn seen_mail(self) -> impl Iterator<Item = (u64, u64)> + 'a {
        self.seen().filter_map(|seen| match *seen {
            Seen::Mail { message, version } => Some((message, version)),
            _ => None,
        })
    }

    /// When the newest stop of a thread's agents the user had seen began:
    /// a stop that began later is news.
    pub fn seen_stopped(self) -> Option<u64> {
        self.seen()
            .filter_map(|seen| match seen {
                Seen::Thread { stopped, .. } => Some(*stopped),
                _ => None,
            })
            .max()
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

    /// The last snooze, unless done or a mute since took it back. It may
    /// be over already: a snooze that ended still says when the node came
    /// back.
    pub fn snooze(self) -> Option<Snooze> {
        self.since_cleared()
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

    /// How many times the user has snoozed the node, ever.
    pub fn snoozes(self) -> usize {
        self.0
            .iter()
            .filter(|entry| matches!(entry.fact, Fact::Snooze { .. }))
            .count()
    }
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

    fn agent() -> Address {
        Address::new("agent", "00jvj4xuk96p")
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
                    seen: Seen::Topic(pos),
                },
            )
        };
        let entries = [settled(0, 812), settled(1, 800)];
        assert_eq!(Facts(&entries).seen_topic(), Some(812));
    }

    #[test]
    fn exact_mail_verdicts_round_trip_without_moving_the_legacy_cutoff() {
        let said = entry(
            2,
            Fact::Settled {
                node: Address::new("thread", "clock"),
                seen: Seen::Mail {
                    message: 17,
                    version: 38,
                },
            },
        );
        let back = Entry::decode(&said.encode()).unwrap();
        assert_eq!(back, said);
        let entries = [
            entry(
                1,
                Fact::Settled {
                    node: Address::new("thread", "clock"),
                    seen: Seen::Topic(13),
                },
            ),
            back,
        ];
        assert_eq!(Facts(&entries).seen_topic(), Some(13));
        assert_eq!(Facts(&entries).seen_mail().collect::<Vec<_>>(), [(17, 38)]);
    }

    fn snooze(minute: i64) -> Entry {
        entry(
            minute,
            Fact::Snooze {
                node: agent(),
                until: Until::In(jiff::SignedDuration::from_hours(1)),
            },
        )
    }

    fn deadline(minute: i64) -> Entry {
        entry(
            minute,
            Fact::Deadline {
                node: agent(),
                by: Until::Day(jiff::civil::date(2027, 1, 16)),
                lead_days: 3,
            },
        )
    }

    #[test]
    fn settled_takes_back_the_deadline_and_the_snooze_before_it() {
        let entries = [
            deadline(0),
            snooze(1),
            entry(
                2,
                Fact::Settled {
                    node: agent(),
                    seen: Seen::Topic(4),
                },
            ),
        ];
        let facts = Facts(&entries);
        assert_eq!(facts.deadline(), None);
        assert_eq!(facts.snooze(), None);
        assert_eq!(facts.snoozes(), 1, "every snooze is kept");
        assert_eq!(facts.seen_topic(), Some(4));
    }

    /// An old pile is done said when it was piled: what came before it is
    /// taken back, what came after stands, and it says nothing seen.
    #[test]
    fn an_old_pile_reads_as_done_said_then() {
        let piled = entry(
            2,
            Fact::Piled {
                node: agent(),
                pile: "friday".into(),
            },
        );
        let before = [deadline(0), snooze(1), piled.clone()];
        let facts = Facts(&before);
        assert_eq!(facts.deadline(), None);
        assert_eq!(facts.snooze(), None);
        assert_eq!(facts.seen_topic(), None, "a pile saw nothing");
        assert!(!facts.muted());

        let after = [piled, snooze(3)];
        assert_eq!(
            Facts(&after).snooze().map(|snooze| snooze.set),
            Some(at(3)),
            "a snooze said after the pile holds"
        );
    }

    /// An old todo is a read: it counts as seen, and takes back neither the
    /// snooze before it, which a todo used to, nor anything else.
    #[test]
    fn an_old_todo_reads_as_read() {
        let entries = [
            deadline(0),
            snooze(1),
            entry(
                2,
                Fact::Todo {
                    node: agent(),
                    start: None,
                    seen: Seen::Topic(7),
                },
            ),
        ];
        let facts = Facts(&entries);
        assert_eq!(facts.seen_topic(), Some(7));
        assert_eq!(facts.snooze().map(|snooze| snooze.set), Some(at(1)));
        assert!(facts.deadline().is_some());
    }
}
