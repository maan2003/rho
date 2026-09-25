//! The user's marks on nodes, as the ledger holds them.
//!
//! Every mark is one ledger key, `n/<node>|<field>`, merged
//! last-writer-wins like everything in the ledger, so two devices marking
//! different things never collide and two marking the same thing settle
//! on the later. A source decides which marks it uses and what they mean
//! for its nodes; this keeps them, typed, and says which nodes moved.
//!
//! What the user did about a node — done, mute, snooze, todo — is not a
//! mark but a run of facts ([`crate::facts`]) under `f/<node>|`; they are
//! read here too, so a node's marks are everything the user said about it.
//!
//! A key this build cannot read (a newer build's field, or a node kind it
//! does not know) is kept as it is and never written over.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use jiff::Timestamp;

use crate::facts::{Cursor, Fact, Facts, Said};
use crate::node::NodeId;
use crate::until::Until;

/// A ledger write: a key, and its new value or `None` to take it away.
pub type Write = (Vec<u8>, Option<Vec<u8>>);

/// Everything the user said about one node. Which fields mean anything
/// depends on the node: a note has a body, a label a parent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeMarks {
    /// What the user did about it, oldest first ([`crate::facts`]).
    pub facts: Vec<Fact>,
    pub labels: BTreeSet<uuid::Uuid>,
    /// What the user calls it, over what its source calls it.
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

fn node_prefix(node: &NodeId) -> String {
    format!("n/{}|", node.key())
}

fn key(node: &NodeId, field: &str) -> Vec<u8> {
    format!("{}{field}", node_prefix(node)).into_bytes()
}

fn write<T: senax_encoder::Encoder>(node: &NodeId, field: &str, value: Option<&T>) -> Write {
    (
        key(node, field),
        value.map(|value| {
            senax_encoder::encode(value)
                .expect("encode a mark")
                .to_vec()
        }),
    )
}

pub fn label(node: &NodeId, label: uuid::Uuid, present: bool) -> Write {
    write(
        node,
        &format!("label:{}", label.simple()),
        present.then_some(&true),
    )
}

pub fn name(node: &NodeId, name: Option<String>) -> Write {
    write(node, "name", name.as_ref())
}

pub fn body(node: &NodeId, body: &str) -> Write {
    write(node, "body", Some(&body.to_owned()))
}

pub fn created(node: &NodeId, at_ms: i64) -> Write {
    write(node, "created", Some(&at_ms))
}

pub fn deleted(node: &NodeId, deleted: bool) -> Write {
    write(node, "deleted", deleted.then_some(&true))
}

pub fn about(node: &NodeId, about: Option<&NodeId>) -> Write {
    write(node, "about", about.map(NodeId::key).as_ref())
}

pub fn parent(label: &NodeId, parent: Option<uuid::Uuid>) -> Write {
    write(
        label,
        "parent",
        parent.map(|parent| parent.simple().to_string()).as_ref(),
    )
}

pub fn repository(label: &NodeId, url: Option<String>) -> Write {
    write(label, "repository", url.as_ref())
}

fn decode<T: senax_encoder::Decoder>(value: &[u8]) -> Option<T> {
    let mut value = value;
    senax_encoder::decode(&mut value).ok()
}

/// Every mark the ledger holds, typed by node.
#[derive(Default)]
pub struct Marks {
    /// Every mark key the ledger holds, as it holds it, including the
    /// ones this build cannot read.
    raw: BTreeMap<Vec<u8>, Vec<u8>>,
    nodes: HashMap<NodeId, NodeMarks>,
}

impl Marks {
    /// Takes in ledger entries, and says which nodes moved. Keys that are
    /// neither marks nor facts are ignored.
    pub fn apply(&mut self, entries: impl IntoIterator<Item = Write>) -> BTreeSet<NodeId> {
        let mut touched = BTreeSet::new();
        for (key, value) in entries {
            if !key.starts_with(b"n/") && !key.starts_with(b"f/") {
                continue;
            }
            let node = std::str::from_utf8(&key[2..])
                .ok()
                .and_then(|rest| rest.split_once('|'))
                .and_then(|(node, _)| NodeId::parse(node));
            match value {
                Some(value) => self.raw.insert(key, value),
                None => self.raw.remove(&key),
            };
            if let Some(node) = node {
                touched.insert(node);
            }
        }
        for node in &touched {
            self.refresh(node);
        }
        touched
    }

    fn refresh(&mut self, node: &NodeId) {
        let mut marks = NodeMarks::default();
        let mut any = false;
        let prefix = node_prefix(node).into_bytes();
        for (key, value) in self.raw.range(prefix.clone()..) {
            let Some(field) = key.strip_prefix(prefix.as_slice()) else {
                break;
            };
            any = true;
            let Ok(field) = std::str::from_utf8(field) else {
                continue;
            };
            match field {
                "name" => marks.name = decode(value),
                "body" => marks.body = decode(value),
                "created" => marks.created_ms = decode(value),
                "deleted" => marks.deleted = decode(value).unwrap_or(false),
                "about" => {
                    marks.about = decode::<String>(value).and_then(|key| NodeId::parse(&key))
                }
                "parent" => {
                    marks.parent =
                        decode::<String>(value).and_then(|id| uuid::Uuid::try_parse(&id).ok())
                }
                "repository" => marks.repository = decode(value),
                _ => {
                    if let Some(label) = field
                        .strip_prefix("label:")
                        .and_then(|id| uuid::Uuid::try_parse(id).ok())
                        && decode(value).unwrap_or(false)
                    {
                        marks.labels.insert(label);
                    }
                }
            }
        }
        let prefix = format!("f/{}|", node.key()).into_bytes();
        let mut facts: Vec<(&[u8], Fact)> = Vec::new();
        for (key, value) in self.raw.range(prefix.clone()..) {
            if !key.starts_with(&prefix) {
                break;
            }
            any = true;
            if let Some(fact) = decode::<Fact>(value) {
                facts.push((key, fact));
            }
        }
        facts.sort_by(|a, b| (a.1.at.timestamp(), a.0).cmp(&(b.1.at.timestamp(), b.0)));
        marks.facts = facts.into_iter().map(|(_, fact)| fact).collect();
        if any {
            self.nodes.insert(node.clone(), marks);
        } else {
            self.nodes.remove(node);
        }
    }

    /// What was said about a node; nothing, for a node never marked.
    pub fn get(&self, node: &NodeId) -> &NodeMarks {
        self.nodes.get(node).unwrap_or(&EMPTY)
    }

    pub fn nodes(&self) -> impl Iterator<Item = (&NodeId, &NodeMarks)> {
        self.nodes.iter()
    }

    /// The writes that put back what `writes` would change.
    pub fn inverse(&self, writes: &[Write]) -> Vec<Write> {
        writes
            .iter()
            .map(|(key, _)| (key.clone(), self.raw.get(key).cloned()))
            .collect()
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
                NodeId::Label(id) if !marks.deleted => Some((*id, self.label_path(*id))),
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

/// The marks an older build kept where facts now go, as it kept them: what
/// the desk's carry-over still writes, and [`Marks::carry_legacy`] reads.
pub mod legacy {
    use senax_encoder::{Decode, Encode};

    use super::{Write, write};
    use crate::facts::Cursor;
    use crate::node::NodeId;

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

    pub fn handled(node: &NodeId, cursor: &Cursor) -> Write {
        write(node, "handled", Some(cursor))
    }

    pub fn muted(node: &NodeId) -> Write {
        write(node, "muted", Some(&true))
    }

    pub fn snooze(node: &NodeId, until: &DateMark) -> Write {
        write(node, "snooze", Some(until))
    }

    pub fn todo(node: &NodeId, todo: &Todo) -> Write {
        write(node, "todo", Some(todo))
    }
}

/// A node's legacy marks, by field: the key and the value.
type LegacyFields<'a> = BTreeMap<&'a str, (&'a [u8], &'a [u8])>;

/// A legacy mark, as a time named on the user's clock.
fn legacy_until(mark: legacy::DateMark, zone: &jiff::tz::TimeZone) -> Until {
    let at = Timestamp::from_millisecond(mark.unix_ms)
        .unwrap_or(Timestamp::UNIX_EPOCH)
        .to_zoned(zone.clone());
    match mark.day {
        true => Until::Day(at.date()),
        false => Until::At(at.datetime()),
    }
}

/// The same 128 bits for the same legacy key and value on every device,
/// so two devices carrying the same marks over write the same facts.
fn legacy_id(key: &[u8], value: &[u8]) -> uuid::Uuid {
    let mut hash: u128 = 0x6c62272e07bb014262b821756295c58d;
    for byte in key.iter().chain([0u8].iter()).chain(value) {
        hash ^= u128::from(*byte);
        hash = hash.wrapping_mul(0x0000000001000000000000000000013B);
    }
    uuid::Uuid::from_u128(hash)
}

impl Marks {
    /// The writes that carry an older build's done, mute, snooze and todo
    /// marks over into facts, and take the old keys away. `stamps` says
    /// when the ledger took each key; a fact is said then, on `zone`'s
    /// clock. Nothing, once nothing is left to carry.
    pub fn carry_legacy(
        &self,
        stamps: &HashMap<Vec<u8>, Timestamp>,
        zone: &jiff::tz::TimeZone,
    ) -> Vec<Write> {
        let mut writes = Vec::new();
        let mut by_node: BTreeMap<NodeId, LegacyFields<'_>> = BTreeMap::new();
        for (key, value) in self.raw.range(b"n/".to_vec()..) {
            if !key.starts_with(b"n/") {
                break;
            }
            let Some((node, field)) = std::str::from_utf8(&key[2..])
                .ok()
                .and_then(|rest| rest.split_once('|'))
            else {
                continue;
            };
            if !matches!(field, "handled" | "muted" | "snooze" | "todo") {
                continue;
            }
            writes.push((key.clone(), None));
            if let Some(node) = NodeId::parse(node) {
                by_node
                    .entry(node)
                    .or_default()
                    .insert(field, (key.as_slice(), value.as_slice()));
            }
        }
        for (node, fields) in by_node {
            let at = |field: &str| {
                let stamp = fields
                    .get(field)
                    .and_then(|(key, _)| stamps.get(*key))
                    .copied()
                    .unwrap_or(Timestamp::UNIX_EPOCH);
                stamp.to_zoned(zone.clone())
            };
            // Marks written together carry the same stamp. A nanosecond
            // apart, in this order, a done never lands after the snooze it
            // came with and takes it back.
            let mut order = 0;
            let mut fact = |field: &str, said: Said| {
                let (key, value) = fields[field];
                let at = at(field);
                let at = at
                    .checked_add(jiff::SignedDuration::from_nanos(order))
                    .unwrap_or(at);
                order += 1;
                writes.push(crate::facts::record_as(
                    &node,
                    legacy_id(key, value),
                    &Fact { at, said },
                ));
            };
            let handled = fields
                .get("handled")
                .and_then(|(_, value)| decode::<Cursor>(value));
            let todo = fields
                .get("todo")
                .and_then(|(_, value)| decode::<legacy::Todo>(value));
            match (
                &handled,
                todo.and_then(|todo| todo.wakes.map(|wakes| (todo, wakes))),
            ) {
                (_, Some((todo, wakes))) => {
                    let start = match legacy_until(wakes, zone) {
                        Until::Day(date) => Until::Day(
                            date.checked_add(
                                jiff::Span::new().days(i64::from(todo.pace_days.saturating_sub(1))),
                            )
                            .unwrap_or(date),
                        ),
                        until => until,
                    };
                    fact(
                        "todo",
                        Said::Todo {
                            through: handled.clone().unwrap_or(Cursor::Done),
                            start: Some(start),
                        },
                    );
                }
                (Some(through), None) => fact(
                    "handled",
                    Said::Done {
                        through: through.clone(),
                    },
                ),
                (None, None) => {}
            }
            if let Some(deadline) = todo.and_then(|todo| todo.deadline.map(|by| (todo, by))) {
                fact(
                    "todo",
                    Said::Deadline {
                        by: legacy_until(deadline.1, zone),
                        lead_days: match deadline.0.pace_days {
                            0 => crate::curve::DEADLINE_LEAD_DAYS,
                            days => days,
                        },
                    },
                );
            }
            if let Some(mark) = fields
                .get("snooze")
                .and_then(|(_, value)| decode::<legacy::DateMark>(value))
            {
                fact(
                    "snooze",
                    Said::Snooze {
                        until: legacy_until(mark, zone),
                    },
                );
            }
            if fields
                .get("muted")
                .is_some_and(|(_, value)| decode::<bool>(value).unwrap_or(false))
            {
                fact("muted", Said::Mute);
            }
        }
        writes
    }
}

#[cfg(test)]
mod tests {
    use jiff::tz::TimeZone;

    use super::*;

    fn agent() -> NodeId {
        NodeId::Agent(rho_agent_types::AgentId::from_encoded("00jvj4xuk96p").unwrap())
    }

    fn noon() -> jiff::Zoned {
        "2026-08-23T12:00:00Z"
            .parse::<Timestamp>()
            .unwrap()
            .to_zoned(TimeZone::UTC)
    }

    fn said(said: Said) -> Fact {
        Fact { at: noon(), said }
    }

    #[test]
    fn marks_and_facts_read_back_typed_and_say_which_node_moved() {
        let mut marks = Marks::default();
        let rho = uuid::Uuid::new_v4();
        let touched = marks.apply([
            crate::facts::record(
                &agent(),
                &said(Said::Done {
                    through: Cursor::Story(7),
                }),
            ),
            crate::facts::record(&agent(), &said(Said::Mute)),
            label(&agent(), rho, true),
            name(&NodeId::Label(rho), Some("rho".into())),
        ]);
        assert_eq!(touched, BTreeSet::from([agent(), NodeId::Label(rho)]));
        let held = marks.get(&agent());
        assert_eq!(held.facts().handled(), Some(&Cursor::Story(7)));
        assert!(held.facts().muted());
        assert_eq!(held.labels, BTreeSet::from([rho]));
        assert_eq!(marks.labeled(rho), [agent()]);
    }

    #[test]
    fn taking_a_fact_back_leaves_the_node_as_if_it_was_never_said() {
        let mut marks = Marks::default();
        let mute = crate::facts::record(&agent(), &said(Said::Mute));
        let undo = marks.inverse(std::slice::from_ref(&mute));
        marks.apply([mute]);
        assert!(marks.get(&agent()).facts().muted());
        marks.apply(undo);
        assert_eq!(marks.get(&agent()), &NodeMarks::default());
        assert_eq!(marks.nodes().count(), 0);
    }

    #[test]
    fn a_field_this_build_does_not_know_is_kept() {
        let mut marks = Marks::default();
        let unknown = (
            format!("n/{}|sparkle", agent().key()).into_bytes(),
            Some(vec![1, 2, 3]),
        );
        marks.apply([unknown.clone(), name(&agent(), Some("a".into()))]);
        assert_eq!(marks.get(&agent()).name.as_deref(), Some("a"));
        assert_eq!(marks.inverse(std::slice::from_ref(&unknown)), [unknown]);
    }

    #[test]
    fn an_older_builds_marks_carry_over_into_facts_once() {
        let zone = TimeZone::fixed(jiff::tz::Offset::constant(5));
        let note = NodeId::Note(uuid::Uuid::from_u128(1));
        let midnight = jiff::civil::date(2026, 8, 24)
            .to_zoned(zone.clone())
            .unwrap()
            .timestamp();
        let mut marks = Marks::default();
        marks.apply([
            legacy::handled(&agent(), &Cursor::Story(4)),
            legacy::snooze(
                &agent(),
                &legacy::DateMark {
                    unix_ms: midnight.as_millisecond() + 3_600_000,
                    day: false,
                },
            ),
            legacy::todo(
                &note,
                &legacy::Todo {
                    wakes: Some(legacy::DateMark {
                        unix_ms: midnight.as_millisecond(),
                        day: true,
                    }),
                    deadline: None,
                    pace_days: 3,
                },
            ),
            legacy::muted(&note),
        ]);
        let stamps = HashMap::new();
        let writes = marks.carry_legacy(&stamps, &zone);
        assert_eq!(
            writes,
            marks.carry_legacy(&stamps, &zone),
            "the same facts on every device"
        );
        marks.apply(writes);
        assert!(
            marks.carry_legacy(&stamps, &zone).is_empty(),
            "nothing left to carry"
        );

        let facts = marks.get(&agent()).facts();
        assert_eq!(facts.handled(), Some(&Cursor::Story(4)));
        assert_eq!(
            facts.snooze().unwrap().until,
            midnight + jiff::SignedDuration::from_hours(1)
        );
        let note = marks.get(&note).facts();
        assert!(note.muted());
        let todo = Facts(&marks.get(&NodeId::Note(uuid::Uuid::from_u128(1))).facts[..1]).todo();
        assert_eq!(
            todo.unwrap().start,
            midnight + jiff::SignedDuration::from_hours(48),
            "a pace of three came back on the third day"
        );
    }

    #[test]
    fn labels_nest_into_paths_and_lend_their_repository() {
        let mut marks = Marks::default();
        let (rho, agents) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let note = NodeId::Note(uuid::Uuid::new_v4());
        marks.apply([
            name(&NodeId::Label(rho), Some("rho".into())),
            repository(
                &NodeId::Label(rho),
                Some("git@github.com:maan2003/rho".into()),
            ),
            name(&NodeId::Label(agents), Some("agents".into())),
            parent(&NodeId::Label(agents), Some(rho)),
            body(&note, "fix the dealer\nsoon"),
            label(&note, agents, true),
        ]);
        assert_eq!(marks.label_path(agents), "rho/agents");
        assert_eq!(marks.label_at("rho/agents"), Some(agents));
        assert_eq!(marks.sublabels(Some(rho)), [agents]);
        assert_eq!(marks.sublabels(None), [rho]);
        assert_eq!(
            marks.repository_of(&note).as_deref(),
            Some("git@github.com:maan2003/rho")
        );
        assert_eq!(marks.get(&note).title(), "fix the dealer");
        marks.apply([deleted(&note, true)]);
        assert!(marks.labeled(agents).is_empty());
        assert_eq!(marks.notes().count(), 0);
    }
}
