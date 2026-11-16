//! What each node comes to: the user's entries ([`crate::facts`]) folded
//! in time order.
//!
//! Attention facts are kept per node for [`crate::facts::Facts`] to read.
//! What a node is called and what it is about settle on the last one
//! said. A note is its newest revision. A retracted entry counts for
//! nothing, whenever the retract arrives. The label facts older builds
//! wrote are about no node ([`Fact::node`]), so nothing here reads them.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use jiff::{SignedDuration, Timestamp, Zoned};

use crate::address::Address;
use crate::facts::{Entry, EntryId, Fact, Facts};

/// Everything the user said about one node. Which fields mean anything
/// depends on the node: only a note has a body.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeMarks {
    /// Its attention facts, oldest first, none retracted.
    pub facts: Vec<Entry>,
    /// What the user calls it, over what its source calls it.
    pub name: Option<String>,
    /// A note's text; its first line is its title.
    pub body: Option<String>,
    /// When the note's newest revision was written.
    pub written_ms: Option<i64>,
    pub created_ms: Option<i64>,
    /// A note the user deleted.
    pub deleted: bool,
    /// What a note is about.
    pub about: Option<Address>,
}

static EMPTY: NodeMarks = NodeMarks {
    facts: Vec::new(),
    name: None,
    body: None,
    written_ms: None,
    created_ms: None,
    deleted: false,
    about: None,
};

impl NodeMarks {
    pub fn facts(&self) -> Facts<'_> {
        Facts(&self.facts)
    }

    /// What was said here, said again of `node`: the attention facts in
    /// order, then the name and subject. A note's body is not about a node
    /// and stays.
    pub fn restated_as(&self, node: &Address) -> Vec<Fact> {
        let mut facts: Vec<Fact> = self
            .facts
            .iter()
            .filter_map(|entry| entry.fact.retarget(node.clone()))
            .collect();
        if self.name.is_some() {
            facts.push(Fact::Named {
                node: node.clone(),
                name: self.name.clone(),
            });
        }
        if let Some(about) = &self.about {
            facts.push(Fact::About {
                node: node.clone(),
                about: Some(about.clone()),
            });
        }
        facts
    }

    /// A note's title: the first line of its body.
    pub fn title(&self) -> &str {
        self.body
            .as_deref()
            .and_then(|body| body.lines().next())
            .unwrap_or("")
            .trim()
    }
}

/// Every entry this device holds, folded by node.
#[derive(Default)]
pub struct Marks {
    entries: BTreeMap<EntryId, Entry>,
    retracted: HashSet<EntryId>,
    by_node: HashMap<Address, BTreeSet<EntryId>>,
    nodes: HashMap<Address, NodeMarks>,
    /// The newest instant any entry or revision here was said at.
    newest: Option<Timestamp>,
}

impl Marks {
    /// Takes in entries from any device, in any order and any number of
    /// times, and says which nodes moved.
    pub fn apply(&mut self, entries: impl IntoIterator<Item = Entry>) -> BTreeSet<Address> {
        let mut touched = BTreeSet::new();
        for entry in entries {
            let id = entry.id();
            if self.entries.contains_key(&id) {
                continue;
            }
            self.saw(id.at);
            match &entry.fact {
                Fact::Retract { of } => {
                    self.retracted.insert(*of);
                    if let Some(node) = self.entries.get(of).and_then(|entry| entry.fact.node()) {
                        touched.insert(node);
                    }
                }
                fact => {
                    if let Some(node) = fact.node() {
                        self.by_node.entry(node.clone()).or_default().insert(id);
                        touched.insert(node);
                    }
                }
            }
            self.entries.insert(id, entry);
        }
        for node in &touched {
            self.refresh(node);
        }
        touched
    }

    fn saw(&mut self, at: Timestamp) {
        self.newest = Some(self.newest.map_or(at, |newest| newest.max(at)));
    }

    /// When something said `now` is said: never at or before anything
    /// already here, so it sorts after everything this device has seen.
    pub fn next_at(&self, now: &Zoned) -> Zoned {
        match self.newest {
            Some(newest) if newest >= now.timestamp() => {
                (newest + SignedDuration::from_nanos(1)).to_zoned(now.time_zone().clone())
            }
            _ => now.clone(),
        }
    }

    fn refresh(&mut self, node: &Address) {
        let mut marks = NodeMarks::default();
        let ids = self.by_node.get(node);
        for id in ids.into_iter().flatten() {
            if self.retracted.contains(id) {
                continue;
            }
            let entry = &self.entries[id];
            match &entry.fact {
                Fact::Snooze { .. }
                | Fact::Piled { .. }
                | Fact::Todo { .. }
                | Fact::Deadline { .. }
                | Fact::Settled { .. }
                | Fact::Mute { .. }
                | Fact::Unmute { .. }
                | Fact::Read { .. } => marks.facts.push(entry.clone()),
                Fact::Named { name, .. } => marks.name = name.clone(),
                Fact::About { about, .. } => marks.about = about.clone(),
                Fact::Wrote { text, created, .. } => {
                    marks.body = Some(text.clone());
                    marks.written_ms = Some(entry.at.timestamp().as_millisecond());
                    marks.created_ms = Some(created.as_millisecond());
                    marks.deleted = false;
                }
                Fact::Deleted { .. } => marks.deleted = true,
                Fact::Label { .. }
                | Fact::Unlabel { .. }
                | Fact::Repository { .. }
                | Fact::Host { .. }
                | Fact::Labeled { .. }
                | Fact::Retract { .. } => {}
            }
        }
        if marks == NodeMarks::default() {
            self.nodes.remove(node);
        } else {
            self.nodes.insert(node.clone(), marks);
        }
    }

    /// What was said about a node; nothing, for a node never marked.
    pub fn get(&self, node: &Address) -> &NodeMarks {
        self.nodes.get(node).unwrap_or(&EMPTY)
    }

    pub fn nodes(&self) -> impl Iterator<Item = (&Address, &NodeMarks)> {
        self.nodes.iter()
    }

    /// Whether this device holds nothing at all yet.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every note that has text and is not deleted.
    pub fn notes(&self) -> impl Iterator<Item = (&Address, &NodeMarks)> {
        self.nodes.iter().filter(|(node, marks)| {
            node.as_note().is_some() && marks.body.is_some() && !marks.deleted
        })
    }

    /// The user's private replies under `node`: the notes about it, oldest
    /// first. A note made and not yet typed into has no text and is not
    /// a reply yet.
    pub fn replies_to(&self, node: &Address) -> Vec<(&Address, &NodeMarks)> {
        let mut replies: Vec<_> = self
            .notes()
            .filter(|(_, marks)| marks.about.as_ref() == Some(node))
            .collect();
        replies.sort_by_key(|(_, marks)| marks.created_ms);
        replies
    }

    /// The user's private rooms: the notes about nothing, newest written
    /// first.
    pub fn private_roots(&self) -> Vec<(&Address, &NodeMarks)> {
        let mut roots: Vec<_> = self
            .notes()
            .filter(|(_, marks)| marks.about.is_none())
            .collect();
        roots.sort_by_key(|(_, marks)| std::cmp::Reverse(marks.written_ms));
        roots
    }
}

#[cfg(test)]
mod tests {
    use jiff::tz::TimeZone;

    use super::*;
    use crate::facts::Device;
    use crate::until::Until;

    const LAPTOP: Device = Device([1; 16]);
    const PHONE: Device = Device([2; 16]);

    fn agent() -> Address {
        Address::new("agent", "00jvj4xuk96p")
    }

    fn at(minute: i64) -> Zoned {
        Timestamp::from_second(1_800_000_000 + minute * 60)
            .unwrap()
            .to_zoned(TimeZone::UTC)
    }

    fn said(device: Device, minute: i64, fact: Fact) -> Entry {
        Entry {
            device,
            at: at(minute),
            fact,
        }
    }

    /// Everything said of a node comes out again about another: the
    /// attention facts as they were and the name. A label an older build
    /// put on it is not restated.
    #[test]
    fn a_nodes_marks_are_restated_as_another_node() {
        let mut marks = Marks::default();
        let moved = Address::new("thread", "yyyy/41");
        marks.apply([
            said(
                LAPTOP,
                0,
                Fact::Snooze {
                    node: agent(),
                    until: Until::In(SignedDuration::from_hours(1)),
                },
            ),
            said(
                LAPTOP,
                1,
                Fact::Labeled {
                    node: agent(),
                    label: uuid::Uuid::new_v4(),
                    present: true,
                },
            ),
            said(
                PHONE,
                2,
                Fact::Named {
                    node: agent(),
                    name: Some("eng".into()),
                },
            ),
        ]);
        assert_eq!(
            marks.get(&agent()).restated_as(&moved),
            [
                Fact::Snooze {
                    node: moved.clone(),
                    until: Until::In(SignedDuration::from_hours(1)),
                },
                Fact::Named {
                    node: moved.clone(),
                    name: Some("eng".into()),
                },
            ]
        );
        assert!(marks.get(&moved).restated_as(&agent()).is_empty());
    }

    #[test]
    fn a_name_settles_on_the_last_thing_said_from_any_device() {
        let mut marks = Marks::default();
        let named = |device, minute, name: &str| {
            said(
                device,
                minute,
                Fact::Named {
                    node: agent(),
                    name: Some(name.into()),
                },
            )
        };
        let entries = [named(LAPTOP, 0, "eng"), named(PHONE, 1, "fixer")];
        // Arriving in any order, and twice, comes to the same.
        let mut backwards = entries.to_vec();
        backwards.reverse();
        marks.apply(backwards);
        let touched = marks.apply(entries);
        assert!(touched.is_empty(), "entries already held move nothing");
        assert_eq!(marks.get(&agent()).name.as_deref(), Some("fixer"));
    }

    /// The label facts older builds wrote still decode, and come to
    /// nothing: no node, no move.
    #[test]
    fn old_label_facts_come_to_nothing() {
        let mut marks = Marks::default();
        let label = uuid::Uuid::new_v4();
        let entries = [
            said(
                LAPTOP,
                0,
                Fact::Label {
                    label,
                    name: "rho".into(),
                    parent: None,
                },
            ),
            said(
                LAPTOP,
                1,
                Fact::Repository {
                    label,
                    url: Some("https://example.com/rho".into()),
                },
            ),
            said(
                LAPTOP,
                2,
                Fact::Host {
                    label,
                    host: Some("local:/run/rho".into()),
                },
            ),
            said(
                PHONE,
                3,
                Fact::Labeled {
                    node: agent(),
                    label,
                    present: true,
                },
            ),
            said(PHONE, 4, Fact::Unlabel { label }),
        ];
        let read: Vec<Entry> = entries
            .iter()
            .map(|entry| Entry::decode(&entry.encode()).expect("an old label fact decodes"))
            .collect();
        assert!(marks.apply(read).is_empty());
        assert_eq!(marks.nodes().count(), 0);
        assert!(!marks.is_empty(), "the entries are still held");
    }

    #[test]
    fn a_retract_takes_an_entry_back_whenever_it_arrives() {
        let mut marks = Marks::default();
        let rename = said(
            PHONE,
            1,
            Fact::Named {
                node: agent(),
                name: Some("new".into()),
            },
        );
        let retract = said(PHONE, 2, Fact::Retract { of: rename.id() });
        marks.apply([
            said(
                LAPTOP,
                0,
                Fact::Named {
                    node: agent(),
                    name: Some("old".into()),
                },
            ),
            retract,
        ]);
        marks.apply([rename]);
        assert_eq!(marks.get(&agent()).name.as_deref(), Some("old"));
    }

    #[test]
    fn a_note_is_its_newest_revision_from_any_device() {
        let mut marks = Marks::default();
        let id = uuid::Uuid::new_v4();
        let wrote = |device, minute, text: &str| {
            said(
                device,
                minute,
                Fact::Wrote {
                    note: id,
                    text: text.into(),
                    created: at(0).timestamp(),
                },
            )
        };
        marks.apply([wrote(PHONE, 2, "buy milk"), wrote(LAPTOP, 1, "buy")]);
        let note = marks.get(&Address::note(id));
        assert_eq!(note.title(), "buy milk");
        assert_eq!(note.created_ms, Some(at(0).timestamp().as_millisecond()));
        assert_eq!(note.written_ms, Some(at(2).timestamp().as_millisecond()));
        assert_eq!(marks.notes().count(), 1);
    }

    /// A deletion is a fact in order with the revisions: one said after the
    /// newest revision removes the note, and a revision said after it, or
    /// a retract of it, brings the note back.
    #[test]
    fn a_deleted_note_comes_back_with_a_later_revision_or_a_retract() {
        let mut marks = Marks::default();
        let id = uuid::Uuid::new_v4();
        let wrote = |minute, text: &str| {
            said(
                LAPTOP,
                minute,
                Fact::Wrote {
                    note: id,
                    text: text.into(),
                    created: at(0).timestamp(),
                },
            )
        };
        let deleted = said(PHONE, 2, Fact::Deleted { note: id });
        marks.apply([wrote(1, "milk"), deleted.clone()]);
        assert_eq!(marks.notes().count(), 0);
        assert!(marks.get(&Address::note(id)).deleted);
        marks.apply([wrote(3, "milk and eggs")]);
        assert_eq!(marks.get(&Address::note(id)).title(), "milk and eggs");
        assert!(!marks.get(&Address::note(id)).deleted);

        let mut marks = Marks::default();
        marks.apply([
            wrote(1, "milk"),
            deleted.clone(),
            said(PHONE, 3, Fact::Retract { of: deleted.id() }),
        ]);
        assert_eq!(marks.notes().count(), 1);
    }

    /// A note about a message is a private reply under it, in the order
    /// the notes were made, whatever order they were last written in. A
    /// deleted one, and one with no text yet, are not replies.
    #[test]
    fn notes_about_a_message_are_its_private_replies_oldest_first() {
        let mut marks = Marks::default();
        let (first, second, empty, gone) = (
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
            uuid::Uuid::new_v4(),
        );
        let about = |minute, note| {
            said(
                LAPTOP,
                minute,
                Fact::About {
                    node: Address::note(note),
                    about: Some(agent()),
                },
            )
        };
        let wrote = |minute, note, text: &str, created| {
            said(
                PHONE,
                minute,
                Fact::Wrote {
                    note,
                    text: text.into(),
                    created: at(created).timestamp(),
                },
            )
        };
        marks.apply([
            about(0, first),
            about(1, second),
            about(2, empty),
            about(3, gone),
            wrote(9, first, "first, edited last", 0),
            wrote(5, second, "second", 1),
            wrote(6, gone, "gone", 3),
            said(PHONE, 7, Fact::Deleted { note: gone }),
        ]);
        let replies: Vec<&str> = marks
            .replies_to(&agent())
            .into_iter()
            .map(|(_, marks)| marks.title())
            .collect();
        assert_eq!(replies, ["first, edited last", "second"]);
        assert!(marks.replies_to(&Address::note(first)).is_empty());
        let roots: Vec<&Address> = marks
            .private_roots()
            .into_iter()
            .map(|(node, _)| node)
            .collect();
        assert!(roots.is_empty(), "a reply is not a room");
    }

    #[test]
    fn what_is_said_next_sorts_after_everything_seen() {
        let mut marks = Marks::default();
        marks.apply([said(PHONE, 10, Fact::Mute { node: agent() })]);
        // This device's clock is behind the phone's.
        let next = marks.next_at(&at(5));
        assert!(next.timestamp() > at(10).timestamp());
        assert_eq!(marks.next_at(&at(20)), at(20));
    }
}
