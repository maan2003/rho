//! What each node comes to: the user's entries ([`crate::facts`]) folded
//! in time order.
//!
//! Attention facts are kept per node for [`crate::facts::Facts`] to read.
//! What a note is about settles on the last one said. A note is its newest
//! revision, and so is a bookmark. A retracted entry counts for nothing,
//! whenever the retract arrives. The label, deadline and name facts older
//! builds wrote are about no node ([`Fact::node`]), so nothing here reads
//! them.

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
    /// A note's text; its first line is its title.
    pub body: Option<String>,
    /// When the note's newest revision was written.
    pub written_ms: Option<i64>,
    pub created_ms: Option<i64>,
    /// A note the user deleted.
    pub deleted: bool,
    /// What a note is about.
    pub about: Option<Address>,
    /// A bookmark's page, until it is removed.
    pub bookmark: Option<Bookmark>,
}

/// A page the user keeps by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bookmark {
    pub title: String,
    pub url: String,
}

static EMPTY: NodeMarks = NodeMarks {
    facts: Vec::new(),
    body: None,
    written_ms: None,
    created_ms: None,
    deleted: false,
    about: None,
    bookmark: None,
};

impl NodeMarks {
    pub fn facts(&self) -> Facts<'_> {
        Facts(&self.facts)
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
                | Fact::Settled { .. }
                | Fact::Mute { .. }
                | Fact::Unmute { .. }
                | Fact::Read { .. } => marks.facts.push(entry.clone()),
                Fact::About { about, .. } => marks.about = about.clone(),
                Fact::Wrote { text, created, .. } => {
                    marks.body = Some(text.clone());
                    marks.written_ms = Some(entry.at.timestamp().as_millisecond());
                    marks.created_ms = Some(created.as_millisecond());
                    marks.deleted = false;
                }
                Fact::Deleted { .. } => marks.deleted = true,
                Fact::Bookmarked { title, url, .. } => {
                    marks.bookmark = Some(Bookmark {
                        title: title.clone(),
                        url: url.clone(),
                    });
                }
                Fact::Unbookmarked { .. } => marks.bookmark = None,
                Fact::Deadline { .. }
                | Fact::Named { .. }
                | Fact::Label { .. }
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
            node.as_note().is_some()
                && marks.body.as_ref().is_some_and(|body| !body.is_empty())
                && !marks.deleted
        })
    }

    /// Every bookmark the user keeps.
    pub fn bookmarks(&self) -> impl Iterator<Item = (&Address, &Bookmark)> {
        self.nodes
            .iter()
            .filter_map(|(node, marks)| Some((node, marks.bookmark.as_ref()?)))
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
}

#[cfg(test)]
mod tests {
    use jiff::tz::TimeZone;

    use super::*;
    use crate::facts::Device;

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

    #[test]
    fn what_a_note_is_about_settles_on_the_last_thing_said_from_any_device() {
        let mut marks = Marks::default();
        let about = |device, minute, key: &str| {
            said(
                device,
                minute,
                Fact::About {
                    node: agent(),
                    about: Some(Address::new("thread", key)),
                },
            )
        };
        let entries = [about(LAPTOP, 0, "eng"), about(PHONE, 1, "fixer")];
        // Arriving in any order, and twice, comes to the same.
        let mut backwards = entries.to_vec();
        backwards.reverse();
        marks.apply(backwards);
        let touched = marks.apply(entries);
        assert!(touched.is_empty(), "entries already held move nothing");
        assert_eq!(
            marks.get(&agent()).about,
            Some(Address::new("thread", "fixer"))
        );
    }

    /// The label, deadline and name facts older builds wrote still decode,
    /// and come to nothing: no node, no move. A deadline left in a log
    /// can never deal a card that no verdict settles.
    #[test]
    fn old_label_deadline_and_name_facts_come_to_nothing() {
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
            said(
                PHONE,
                5,
                Fact::Deadline {
                    node: Address::note(uuid::Uuid::new_v4()),
                    by: crate::until::Until::Day(jiff::civil::date(2026, 1, 1)),
                    lead_days: 3,
                },
            ),
            said(
                LAPTOP,
                6,
                Fact::Named {
                    node: agent(),
                    name: Some("fixer".into()),
                },
            ),
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
        let about = |device, minute, key: &str| {
            said(
                device,
                minute,
                Fact::About {
                    node: agent(),
                    about: Some(Address::new("thread", key)),
                },
            )
        };
        let moved = about(PHONE, 1, "new");
        let retract = said(PHONE, 2, Fact::Retract { of: moved.id() });
        marks.apply([about(LAPTOP, 0, "old"), retract]);
        marks.apply([moved]);
        assert_eq!(
            marks.get(&agent()).about,
            Some(Address::new("thread", "old"))
        );
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

    /// A bookmark is what was said last about it from any device: a
    /// removal said after it takes it away whatever order they arrive in,
    /// and saying it again, or taking the removal back, brings it back.
    #[test]
    fn a_bookmark_is_the_last_thing_said_about_it() {
        let page = Address::new("bookmark", "slack");
        let kept = |device, minute, title: &str| {
            said(
                device,
                minute,
                Fact::Bookmarked {
                    node: page.clone(),
                    title: title.into(),
                    url: "https://example.slack.com/".into(),
                },
            )
        };
        let removed = said(PHONE, 2, Fact::Unbookmarked { node: page.clone() });
        let mut marks = Marks::default();
        marks.apply([removed.clone(), kept(LAPTOP, 1, "slack")]);
        assert_eq!(marks.bookmarks().count(), 0, "removed after it was kept");
        assert!(marks.nodes().all(|(node, _)| *node != page));

        marks.apply([kept(LAPTOP, 3, "work slack")]);
        let held: Vec<_> = marks.bookmarks().collect();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].1.title, "work slack");

        let mut undone = Marks::default();
        undone.apply([
            kept(LAPTOP, 1, "slack"),
            removed.clone(),
            said(PHONE, 4, Fact::Retract { of: removed.id() }),
        ]);
        assert_eq!(undone.bookmarks().count(), 1, "a retracted removal");
    }
}
