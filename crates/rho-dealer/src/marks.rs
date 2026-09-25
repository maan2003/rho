//! What each node comes to: the user's entries ([`crate::facts`]) folded
//! in time order, and its note ([`crate::notes`]).
//!
//! Attention facts are kept per node for [`crate::facts::Facts`] to read.
//! Filing facts settle on the last one said: a label's name and parent,
//! whether a node carries a label, what it is called and what it is about.
//! A retracted entry counts for nothing, whenever the retract arrives.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use jiff::{SignedDuration, Timestamp, Zoned};

use crate::facts::{Entry, EntryId, Fact, Facts};
use crate::node::NodeId;
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
    pub about: Option<NodeId>,
    /// The label a label sits under.
    pub parent: Option<uuid::Uuid>,
    /// The repository work under a label happens in.
    pub repository: Option<String>,
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

/// Every entry and note revision this device holds, folded by node.
#[derive(Default)]
pub struct Marks {
    entries: BTreeMap<EntryId, Entry>,
    retracted: HashSet<EntryId>,
    by_node: HashMap<NodeId, BTreeSet<EntryId>>,
    /// The newest revision of each note.
    notes: HashMap<uuid::Uuid, NoteRev>,
    nodes: HashMap<NodeId, NodeMarks>,
    /// The newest instant any entry or revision here was said at.
    newest: Option<Timestamp>,
}

impl Marks {
    /// Takes in entries from any device, in any order and any number of
    /// times, and says which nodes moved.
    pub fn apply(&mut self, entries: impl IntoIterator<Item = Entry>) -> BTreeSet<NodeId> {
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
    pub fn apply_notes(&mut self, revs: impl IntoIterator<Item = NoteRev>) -> BTreeSet<NodeId> {
        let mut touched = BTreeSet::new();
        for rev in revs {
            self.saw(rev.at.timestamp());
            let newer = self
                .notes
                .get(&rev.note)
                .is_none_or(|held| held.id() < rev.id());
            if newer {
                touched.insert(NodeId::Note(rev.note));
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

    fn refresh(&mut self, node: &NodeId) {
        let mut marks = NodeMarks::default();
        let ids = self.by_node.get(node);
        for id in ids.into_iter().flatten() {
            if self.retracted.contains(id) {
                continue;
            }
            let entry = &self.entries[id];
            match &entry.fact {
                Fact::Snooze { .. }
                | Fact::Todo { .. }
                | Fact::Deadline { .. }
                | Fact::Settled { .. }
                | Fact::Mute { .. }
                | Fact::Unmute { .. } => marks.facts.push(entry.clone()),
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
        if let NodeId::Note(id) = node
            && let Some(note) = self.notes.get(id)
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
    pub fn get(&self, node: &NodeId) -> &NodeMarks {
        self.nodes.get(node).unwrap_or(&EMPTY)
    }

    pub fn nodes(&self) -> impl Iterator<Item = (&NodeId, &NodeMarks)> {
        self.nodes.iter()
    }

    /// A note's newest revision.
    pub fn note(&self, note: uuid::Uuid) -> Option<&NoteRev> {
        self.notes.get(&note)
    }

    /// Whether this device holds nothing at all yet.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.notes.is_empty()
    }

    /// Every note that is not deleted.
    pub fn notes(&self) -> impl Iterator<Item = (&NodeId, &NodeMarks)> {
        self.nodes
            .iter()
            .filter(|(node, marks)| matches!(node, NodeId::Note(_)) && !marks.deleted)
    }

    /// Every label that is not deleted, with its full path.
    pub fn labels(&self) -> Vec<(uuid::Uuid, String)> {
        let mut labels: Vec<_> = self
            .nodes
            .iter()
            .filter_map(|(node, marks)| match node {
                NodeId::Label(id) if !marks.deleted && marks.name.is_some() => {
                    Some((*id, self.label_path(*id)))
                }
                _ => None,
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
            let marks = self.get(&NodeId::Label(id));
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
            .filter(|id| self.get(&NodeId::Label(*id)).parent == label)
            .collect()
    }

    /// Every node carrying `label`, except deleted notes.
    pub fn labeled(&self, label: uuid::Uuid) -> Vec<NodeId> {
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
    pub fn repository_of(&self, node: &NodeId) -> Option<String> {
        let mut labels: Vec<uuid::Uuid> = match node {
            NodeId::Label(id) => vec![*id],
            _ => self.get(node).labels.iter().copied().collect(),
        };
        let mut seen = BTreeSet::new();
        while !labels.is_empty() {
            let mut above = Vec::new();
            for id in labels {
                if !seen.insert(id) {
                    continue;
                }
                let marks = self.get(&NodeId::Label(id));
                if let Some(url) = &marks.repository {
                    return Some(url.clone());
                }
                above.extend(marks.parent);
            }
            labels = above;
        }
        None
    }
}

/// The ledger an older build kept: one last-writer-wins key per mark,
/// `n/<node>|<field>`. What the desk's carry-over still writes, and what
/// [`legacy::convert`] turns into entries and notes once.
pub mod legacy {
    use std::collections::BTreeMap;

    use jiff::tz::TimeZone;
    use jiff::{SignedDuration, Timestamp};
    use senax_encoder::{Decode, Encode};

    use crate::facts::{Device, Entry, Fact, Seen};
    use crate::node::NodeId;
    use crate::notes::NoteRev;
    use crate::until::Until;

    /// An old key and its value.
    pub type Mark = (Vec<u8>, Vec<u8>);

    /// How far a node was handled, as an older build kept it.
    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    pub enum Cursor {
        Done,
        Story(u64),
    }

    /// A day or a moment, as an instant.
    #[derive(Clone, Copy, Debug, Encode, Decode)]
    pub struct DateMark {
        pub unix_ms: i64,
        pub day: bool,
    }

    #[derive(Clone, Copy, Debug, Encode, Decode)]
    pub struct Todo {
        pub wakes: Option<DateMark>,
        pub deadline: Option<DateMark>,
        pub pace_days: u32,
    }

    fn mark<T: senax_encoder::Encoder>(node: &NodeId, field: &str, value: &T) -> Mark {
        (
            format!("n/{}|{field}", node.key()).into_bytes(),
            senax_encoder::encode(value)
                .expect("encode a mark")
                .to_vec(),
        )
    }

    pub fn label(node: &NodeId, label: uuid::Uuid) -> Mark {
        mark(node, &format!("label:{}", label.simple()), &true)
    }

    pub fn name(node: &NodeId, name: &str) -> Mark {
        mark(node, "name", &name.to_owned())
    }

    pub fn body(node: &NodeId, body: &str) -> Mark {
        mark(node, "body", &body.to_owned())
    }

    pub fn created(node: &NodeId, at_ms: i64) -> Mark {
        mark(node, "created", &at_ms)
    }

    pub fn deleted(node: &NodeId) -> Mark {
        mark(node, "deleted", &true)
    }

    pub fn about(node: &NodeId, about: &NodeId) -> Mark {
        mark(node, "about", &about.key())
    }

    pub fn parent(label: &NodeId, parent: uuid::Uuid) -> Mark {
        mark(label, "parent", &parent.simple().to_string())
    }

    pub fn repository(label: &NodeId, url: &str) -> Mark {
        mark(label, "repository", &url.to_owned())
    }

    pub fn handled(node: &NodeId, cursor: &Cursor) -> Mark {
        mark(node, "handled", cursor)
    }

    pub fn muted(node: &NodeId) -> Mark {
        mark(node, "muted", &true)
    }

    pub fn snooze(node: &NodeId, until: &DateMark) -> Mark {
        mark(node, "snooze", until)
    }

    pub fn todo(node: &NodeId, todo: &Todo) -> Mark {
        mark(node, "todo", todo)
    }

    fn decode<T: senax_encoder::Decoder>(value: &[u8]) -> Option<T> {
        let mut value = value;
        senax_encoder::decode(&mut value).ok()
    }

    /// A legacy date, as a time named on the user's clock.
    fn until(mark: DateMark, zone: &TimeZone) -> Until {
        let at = Timestamp::from_millisecond(mark.unix_ms)
            .unwrap_or(Timestamp::UNIX_EPOCH)
            .to_zoned(zone.clone());
        match mark.day {
            true => Until::Day(at.date()),
            false => Until::At(at.datetime()),
        }
    }

    /// One node's old marks: each field's value and when it was written.
    #[derive(Default)]
    struct Fields<'a>(BTreeMap<&'a str, (&'a [u8], Timestamp)>);

    impl<'a> Fields<'a> {
        fn get<T: senax_encoder::Decoder>(&self, field: &str) -> Option<(T, Timestamp)> {
            let (value, at) = self.0.get(field)?;
            Some((decode(value)?, *at))
        }

        fn at(&self, fields: &[&str]) -> Timestamp {
            fields
                .iter()
                .filter_map(|field| self.0.get(field).map(|(_, at)| *at))
                .max()
                .unwrap_or(Timestamp::UNIX_EPOCH)
        }
    }

    /// Turns an old ledger into entries and notes said by `device`, each
    /// dated when its mark was written, on `zone`'s clock. `marks` holds
    /// each key's value and the millis it was stamped.
    pub fn convert(
        marks: &[(Vec<u8>, Vec<u8>, u64)],
        device: Device,
        zone: &TimeZone,
    ) -> (Vec<Entry>, Vec<NoteRev>) {
        let mut by_node: BTreeMap<NodeId, Fields> = BTreeMap::new();
        for (key, value, stamp) in marks {
            let Some((node, field)) = key
                .strip_prefix(b"n/")
                .and_then(|rest| std::str::from_utf8(rest).ok())
                .and_then(|rest| rest.split_once('|'))
            else {
                continue;
            };
            let Some(node) = NodeId::parse(node) else {
                continue;
            };
            let at = Timestamp::from_millisecond(*stamp as i64).unwrap_or(Timestamp::UNIX_EPOCH);
            by_node
                .entry(node)
                .or_default()
                .0
                .insert(field, (value.as_slice(), at));
        }
        // Each fact at its mark's time; in this order within a node, so a
        // done never lands after the snooze written with it.
        let mut said: Vec<(Timestamp, usize, Fact)> = Vec::new();
        let mut notes = Vec::new();
        for (node, fields) in &by_node {
            let mut say = |at: Timestamp, fact: Fact| {
                let order = said.len();
                said.push((at, order, fact));
            };
            match node {
                NodeId::Label(label) => {
                    if let Some((name, _)) = fields.get::<String>("name") {
                        say(
                            fields.at(&["name", "parent"]),
                            Fact::Label {
                                label: *label,
                                name,
                                parent: fields
                                    .get::<String>("parent")
                                    .and_then(|(id, _)| uuid::Uuid::try_parse(&id).ok()),
                            },
                        );
                    }
                    if let Some((url, at)) = fields.get::<String>("repository") {
                        say(
                            at,
                            Fact::Repository {
                                label: *label,
                                url: Some(url),
                            },
                        );
                    }
                    if let Some((true, at)) = fields.get::<bool>("deleted") {
                        say(at, Fact::Unlabel { label: *label });
                    }
                }
                NodeId::Note(note) => {
                    let body = fields.get::<String>("body");
                    let created = fields.get::<i64>("created");
                    let deleted = fields
                        .get::<bool>("deleted")
                        .is_some_and(|(deleted, _)| deleted);
                    if body.is_some() || created.is_some() || deleted {
                        let at = fields.at(&["body", "created", "deleted"]);
                        notes.push(NoteRev {
                            note: *note,
                            device,
                            at: at.to_zoned(zone.clone()),
                            created: created
                                .and_then(|(ms, _)| Timestamp::from_millisecond(ms).ok())
                                .unwrap_or(at),
                            body: body.map(|(body, _)| body).unwrap_or_default(),
                            deleted,
                        });
                    }
                }
                _ => {
                    if let Some((name, at)) = fields.get::<String>("name") {
                        say(
                            at,
                            Fact::Named {
                                node: node.clone(),
                                name: Some(name),
                            },
                        );
                    }
                }
            }
            for (field, (value, at)) in &fields.0 {
                if let Some(label) = field
                    .strip_prefix("label:")
                    .and_then(|id| uuid::Uuid::try_parse(id).ok())
                    && decode::<bool>(value).unwrap_or(false)
                {
                    say(
                        *at,
                        Fact::Labeled {
                            node: node.clone(),
                            label,
                            present: true,
                        },
                    );
                }
            }
            if let Some((about, at)) = fields.get::<String>("about")
                && let Some(about) = NodeId::parse(&about)
            {
                say(
                    at,
                    Fact::About {
                        node: node.clone(),
                        about: Some(about),
                    },
                );
            }
            let seen = fields.get::<Cursor>("handled").map(|(cursor, at)| {
                let seen = match cursor {
                    Cursor::Story(pos) => Seen::Agent(pos),
                    Cursor::Done => Seen::Whole,
                };
                (seen, at)
            });
            let todo = fields.get::<Todo>("todo");
            match (
                &seen,
                todo.and_then(|(todo, at)| todo.wakes.map(|wakes| (todo, wakes, at))),
            ) {
                (_, Some((todo, wakes, at))) => {
                    let start = match until(wakes, zone) {
                        Until::Day(date) => Until::Day(
                            date.checked_add(
                                jiff::Span::new().days(i64::from(todo.pace_days.saturating_sub(1))),
                            )
                            .unwrap_or(date),
                        ),
                        start => start,
                    };
                    say(
                        at,
                        Fact::Todo {
                            node: node.clone(),
                            start: Some(start),
                            seen: seen.clone().map_or(Seen::Whole, |(seen, _)| seen),
                        },
                    );
                }
                (Some((seen, at)), None) => say(
                    *at,
                    Fact::Settled {
                        node: node.clone(),
                        seen: seen.clone(),
                    },
                ),
                (None, None) => {}
            }
            if let Some((todo, at)) = todo
                && let Some(by) = todo.deadline
            {
                say(
                    at,
                    Fact::Deadline {
                        node: node.clone(),
                        by: until(by, zone),
                        lead_days: match todo.pace_days {
                            0 => crate::curve::DEADLINE_LEAD_DAYS,
                            days => days,
                        },
                    },
                );
            }
            if let Some((mark, at)) = fields.get::<DateMark>("snooze") {
                say(
                    at,
                    Fact::Snooze {
                        node: node.clone(),
                        until: until(mark, zone),
                    },
                );
            }
            if let Some((true, at)) = fields.get::<bool>("muted") {
                say(at, Fact::Mute { node: node.clone() });
            }
        }
        // Every entry of one device needs an instant of its own.
        said.sort_by_key(|(at, order, _)| (*at, *order));
        let mut last: Option<Timestamp> = None;
        let entries = said
            .into_iter()
            .map(|(at, _, fact)| {
                let at = match last {
                    Some(last) if last >= at => last + SignedDuration::from_nanos(1),
                    _ => at,
                };
                last = Some(at);
                Entry {
                    device,
                    at: at.to_zoned(zone.clone()),
                    fact,
                }
            })
            .collect();
        (entries, notes)
    }
}

#[cfg(test)]
mod tests {
    use jiff::tz::TimeZone;

    use super::*;
    use crate::facts::{Device, Seen};
    use crate::until::Until;

    const LAPTOP: Device = Device([1; 16]);
    const PHONE: Device = Device([2; 16]);

    fn agent() -> NodeId {
        NodeId::Agent(rho_agent_types::AgentId::from_encoded("00jvj4xuk96p").unwrap())
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
        assert_eq!(marks.get(&NodeId::Note(id)).title(), "buy milk");
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

    #[test]
    fn an_older_ledger_converts_into_entries_and_notes_dated_as_written() {
        let rho = uuid::Uuid::new_v4();
        let label = NodeId::Label(rho);
        let note = NodeId::Note(uuid::Uuid::new_v4());
        let stamp = at(0).timestamp().as_millisecond() as u64;
        let mut old = vec![
            legacy::name(&label, "rho"),
            legacy::repository(&label, "https://example.com/rho"),
            legacy::label(&agent(), rho),
            legacy::name(&agent(), "fixer"),
            legacy::handled(&agent(), &legacy::Cursor::Story(9)),
            legacy::snooze(
                &agent(),
                &legacy::DateMark {
                    unix_ms: at(60 * 24).timestamp().as_millisecond(),
                    day: true,
                },
            ),
            legacy::body(&note, "buy milk"),
            legacy::created(&note, 5),
        ];
        old.push(legacy::muted(&NodeId::Label(uuid::Uuid::new_v4())));
        let old: Vec<_> = old
            .into_iter()
            .map(|(key, value)| (key, value, stamp))
            .collect();
        let (entries, notes) = legacy::convert(&old, LAPTOP, &TimeZone::UTC);
        let ids: BTreeSet<_> = entries.iter().map(Entry::id).collect();
        assert_eq!(ids.len(), entries.len(), "every entry has its own instant");
        assert!(
            entries
                .iter()
                .all(|entry| entry.at.timestamp() >= at(0).timestamp()
                    && entry.at.timestamp() < at(0).timestamp() + SignedDuration::from_micros(1)),
            "each is dated when its mark was written"
        );

        let mut marks = Marks::default();
        marks.apply(entries);
        marks.apply_notes(notes);
        assert_eq!(
            marks.repository_of(&agent()).as_deref(),
            Some("https://example.com/rho")
        );
        let held = marks.get(&agent());
        assert_eq!(held.name.as_deref(), Some("fixer"));
        assert_eq!(held.facts().seen_agent(), Some(9));
        assert_eq!(
            held.facts().snooze().map(|snooze| snooze.until),
            Some(Until::Day(jiff::civil::date(2027, 1, 16)).resolve(&at(0)))
        );
        let (_, held) = marks.notes().next().unwrap();
        assert_eq!(held.title(), "buy milk");
        assert_eq!(held.created_ms, Some(5));
        let _ = Seen::Whole;
    }
}
