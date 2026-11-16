//! What each node comes to: the user's entries ([`crate::facts`]) folded
//! in time order, and its note ([`crate::notes`]).
//!
//! Attention facts are kept per node for [`crate::facts::Facts`] to read.
//! Filing facts settle on the last one said: a label's name and parent,
//! whether a node carries a label, what it is called and what it is about.
//! A retracted entry counts for nothing, whenever the retract arrives.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use jiff::{SignedDuration, Timestamp, Zoned};

use crate::address::Address;
use crate::facts::{Entry, EntryId, Fact, Facts};
use crate::notes::NoteRev;

/// Everything the user said about one node. Which fields mean anything
/// depends on the node: a note has a body, a label a parent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeMarks {
    /// Its attention facts, oldest first, none retracted.
    pub facts: Vec<Entry>,
    pub labels: BTreeSet<uuid::Uuid>,
    /// What the user calls it, over what its source calls it; a label's
    /// own name.
    pub name: Option<String>,
    /// A note's text; its first line is its title.
    pub body: Option<String>,
    pub created_ms: Option<i64>,
    /// A note or a label the user deleted.
    pub deleted: bool,
    /// What a note is about.
    pub about: Option<Address>,
    /// The label a label sits under.
    pub parent: Option<uuid::Uuid>,
    /// The repository work under a label happens in.
    pub repository: Option<String>,
    /// The agent host work under a label runs on, by its identity.
    pub host: Option<String>,
}

static EMPTY: NodeMarks = NodeMarks {
    facts: Vec::new(),
    labels: BTreeSet::new(),
    name: None,
    body: None,
    created_ms: None,
    deleted: false,
    about: None,
    parent: None,
    repository: None,
    host: None,
};

impl NodeMarks {
    pub fn facts(&self) -> Facts<'_> {
        Facts(&self.facts)
    }

    /// What was said here, said again of `node`: the attention facts in
    /// order, then the labels, name and subject. A note's body and a
    /// label's own facts are not about a node and stay.
    pub fn restated_as(&self, node: &Address) -> Vec<Fact> {
        let mut facts: Vec<Fact> = self
            .facts
            .iter()
            .filter_map(|entry| entry.fact.retarget(node.clone()))
            .collect();
        facts.extend(self.labels.iter().map(|label| Fact::Labeled {
            node: node.clone(),
            label: *label,
            present: true,
        }));
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

/// Nodes the user put away together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pile {
    /// The name the user gave it; `None` for everything snoozed.
    pub name: Option<String>,
    pub nodes: Vec<Address>,
    /// When the first of an unnamed pile comes back by itself. A named pile
    /// comes back only when opened.
    pub back: Option<Timestamp>,
}

/// Every entry and note revision this device holds, folded by node.
#[derive(Default)]
pub struct Marks {
    entries: BTreeMap<EntryId, Entry>,
    retracted: HashSet<EntryId>,
    by_node: HashMap<Address, BTreeSet<EntryId>>,
    /// The newest revision of each note.
    notes: HashMap<uuid::Uuid, NoteRev>,
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

    /// Takes in note revisions, and says which notes moved.
    pub fn apply_notes(&mut self, revs: impl IntoIterator<Item = NoteRev>) -> BTreeSet<Address> {
        let mut touched = BTreeSet::new();
        for rev in revs {
            self.saw(rev.at.timestamp());
            let newer = self
                .notes
                .get(&rev.note)
                .is_none_or(|held| held.id() < rev.id());
            if newer {
                touched.insert(Address::note(rev.note));
                self.notes.insert(rev.note, rev);
            }
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
                Fact::Label { name, parent, .. } => {
                    marks.name = Some(name.clone());
                    marks.parent = *parent;
                    marks.deleted = false;
                    marks
                        .created_ms
                        .get_or_insert(entry.at.timestamp().as_millisecond());
                }
                Fact::Unlabel { .. } => marks.deleted = true,
                Fact::Repository { url, .. } => marks.repository = url.clone(),
                Fact::Host { host, .. } => marks.host = host.clone(),
                Fact::Labeled { label, present, .. } => {
                    match present {
                        true => marks.labels.insert(*label),
                        false => marks.labels.remove(label),
                    };
                }
                Fact::Named { name, .. } => marks.name = name.clone(),
                Fact::About { about, .. } => marks.about = about.clone(),
                Fact::Retract { .. } => {}
            }
        }
        if let Some(id) = node.as_note()
            && let Some(note) = self.notes.get(&id)
        {
            marks.body = Some(note.body.clone());
            marks.created_ms = Some(note.created.as_millisecond());
            marks.deleted = note.deleted;
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

    /// A note's newest revision.
    /// Every note's newest revision, deleted ones too.
    pub fn note_revs(&self) -> impl Iterator<Item = &NoteRev> {
        self.notes.values()
    }

    pub fn note(&self, note: uuid::Uuid) -> Option<&NoteRev> {
        self.notes.get(&note)
    }

    /// Whether this device holds nothing at all yet.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.notes.is_empty()
    }

    /// Every note that is not deleted.
    pub fn notes(&self) -> impl Iterator<Item = (&Address, &NodeMarks)> {
        self.nodes
            .iter()
            .filter(|(node, marks)| node.as_note().is_some() && !marks.deleted)
    }

    /// What the user put away and can open: each named pile with its nodes,
    /// oldest first, and last the unnamed one, whatever is snoozed past
    /// `now`, soonest back first.
    pub fn piles(&self, now: Timestamp) -> Vec<Pile> {
        let mut named: BTreeMap<&str, Vec<(Timestamp, &Address)>> = BTreeMap::new();
        let mut snoozed = Vec::new();
        for (node, marks) in &self.nodes {
            let facts = marks.facts();
            if marks.deleted || facts.muted() {
                continue;
            }
            if let Some((pile, at)) = facts.pile() {
                named.entry(pile).or_default().push((at.timestamp(), node));
            } else if let Some(snooze) = facts.snooze().filter(|snooze| snooze.until > now) {
                snoozed.push((snooze.until, node));
            }
        }
        let pile = |name: Option<&str>, mut nodes: Vec<(Timestamp, &Address)>| {
            nodes.sort();
            Pile {
                name: name.map(str::to_owned),
                back: name.is_none().then(|| nodes[0].0),
                nodes: nodes.into_iter().map(|(_, node)| node.clone()).collect(),
            }
        };
        let mut piles: Vec<Pile> = named
            .into_iter()
            .map(|(name, nodes)| pile(Some(name), nodes))
            .collect();
        if !snoozed.is_empty() {
            piles.push(pile(None, snoozed));
        }
        piles
    }

    /// Every label that is not deleted, with its full path.
    pub fn labels(&self) -> Vec<(uuid::Uuid, String)> {
        let mut labels: Vec<_> = self
            .nodes
            .iter()
            .filter_map(|(node, marks)| {
                let id = node.as_label()?;
                (!marks.deleted && marks.name.is_some()).then(|| (id, self.label_path(id)))
            })
            .collect();
        labels.sort_by(|a, b| a.1.cmp(&b.1));
        labels
    }

    /// A label's name with the names of the labels above it, `rho/agent`.
    pub fn label_path(&self, label: uuid::Uuid) -> String {
        let mut names = Vec::new();
        let mut seen = BTreeSet::new();
        let mut at = Some(label);
        while let Some(id) = at {
            if !seen.insert(id) {
                break;
            }
            let marks = self.get(&Address::label(id));
            names.push(marks.name.clone().unwrap_or_default());
            at = marks.parent;
        }
        names.reverse();
        names.join("/")
    }

    /// The live label at `path`, if there is one.
    pub fn label_at(&self, path: &str) -> Option<uuid::Uuid> {
        self.labels()
            .into_iter()
            .find(|(_, held)| held == path)
            .map(|(id, _)| id)
    }

    /// The live labels directly under `label`, or at the top for `None`.
    pub fn sublabels(&self, label: Option<uuid::Uuid>) -> Vec<uuid::Uuid> {
        self.labels()
            .into_iter()
            .map(|(id, _)| id)
            .filter(|id| self.get(&Address::label(*id)).parent == label)
            .collect()
    }

    /// Every node carrying `label`, except deleted notes.
    pub fn labeled(&self, label: uuid::Uuid) -> Vec<Address> {
        let mut nodes: Vec<_> = self
            .nodes
            .iter()
            .filter(|(_, marks)| marks.labels.contains(&label) && !marks.deleted)
            .map(|(node, _)| node.clone())
            .collect();
        nodes.sort();
        nodes
    }

    /// The repository work on a node happens in: the first its labels
    /// name, looking up through each label's parents.
    pub fn repository_of(&self, node: &Address) -> Option<String> {
        self.inherited(node, |marks| marks.repository.clone())
    }

    /// The host work under `node` runs on: its own label's, else the
    /// nearest one above, as with the repository.
    pub fn host_of(&self, node: &Address) -> Option<String> {
        self.inherited(node, |marks| marks.host.clone())
    }

    fn inherited(
        &self,
        node: &Address,
        read: impl Fn(&NodeMarks) -> Option<String>,
    ) -> Option<String> {
        let mut labels: Vec<uuid::Uuid> = match node.as_label() {
            Some(id) => vec![id],
            None => self.get(node).labels.iter().copied().collect(),
        };
        let mut seen = BTreeSet::new();
        while !labels.is_empty() {
            let mut above = Vec::new();
            for id in labels {
                if !seen.insert(id) {
                    continue;
                }
                let marks = self.get(&Address::label(id));
                if let Some(value) = read(marks) {
                    return Some(value);
                }
                above.extend(marks.parent);
            }
            labels = above;
        }
        None
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
    /// attention facts as they were, each label still on it, and the name.
    /// A label taken off again is not restated.
    #[test]
    fn a_nodes_marks_are_restated_as_another_node() {
        let mut marks = Marks::default();
        let gui = uuid::Uuid::new_v4();
        let old = uuid::Uuid::new_v4();
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
                    label: gui,
                    present: true,
                },
            ),
            said(
                LAPTOP,
                2,
                Fact::Labeled {
                    node: agent(),
                    label: old,
                    present: true,
                },
            ),
            said(
                PHONE,
                3,
                Fact::Labeled {
                    node: agent(),
                    label: old,
                    present: false,
                },
            ),
            said(
                PHONE,
                4,
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
                Fact::Labeled {
                    node: moved.clone(),
                    label: gui,
                    present: true,
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
    fn filing_settles_on_the_last_thing_said_from_any_device() {
        let mut marks = Marks::default();
        let rho = uuid::Uuid::new_v4();
        let gui = uuid::Uuid::new_v4();
        let entries = [
            said(
                LAPTOP,
                0,
                Fact::Label {
                    label: rho,
                    name: "rho".into(),
                    parent: None,
                },
            ),
            said(
                PHONE,
                1,
                Fact::Label {
                    label: gui,
                    name: "gui".into(),
                    parent: Some(rho),
                },
            ),
            said(
                LAPTOP,
                2,
                Fact::Labeled {
                    node: agent(),
                    label: gui,
                    present: true,
                },
            ),
            said(
                PHONE,
                3,
                Fact::Named {
                    node: agent(),
                    name: Some("fixer".into()),
                },
            ),
        ];
        // Arriving in any order, and twice, comes to the same.
        let mut backwards = entries.to_vec();
        backwards.reverse();
        marks.apply(backwards);
        let touched = marks.apply(entries);
        assert!(touched.is_empty(), "entries already held move nothing");
        assert_eq!(marks.label_path(gui), "rho/gui");
        assert_eq!(marks.labeled(gui), vec![agent()]);
        assert_eq!(marks.get(&agent()).name.as_deref(), Some("fixer"));
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
    fn a_note_is_its_newest_revision() {
        let mut marks = Marks::default();
        let id = uuid::Uuid::new_v4();
        let rev = |device, minute, body: &str| NoteRev {
            note: id,
            device,
            at: at(minute),
            created: at(0).timestamp(),
            body: body.into(),
            deleted: false,
        };
        marks.apply_notes([rev(PHONE, 2, "buy milk"), rev(LAPTOP, 1, "buy")]);
        assert_eq!(marks.get(&Address::note(id)).title(), "buy milk");
        assert_eq!(marks.notes().count(), 1);
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
