//! The user's marks on nodes, as the ledger holds them.
//!
//! Every mark is one ledger key, `n/<node>|<field>`, merged
//! last-writer-wins like everything in the ledger, so two devices marking
//! different things never collide and two marking the same thing settle
//! on the later. A source decides which marks it uses and what they mean
//! for its nodes; this keeps them, typed, and says which nodes moved.
//!
//! A key this build cannot read (a newer build's field, or a node kind it
//! does not know) is kept as it is and never written over.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use senax_encoder::{Decode, Encode};

use crate::curve::DateMark;
use crate::node::NodeId;

/// A ledger write: a key, and its new value or `None` to take it away.
pub type Write = (Vec<u8>, Option<Vec<u8>>);

/// How far the user has dealt with a node: its source's position when
/// they last said done.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Cursor {
    /// Done, for a node whose source has no position: a note.
    Done,
    /// An agent's story, through this position.
    Story(u64),
}

/// A node the user means to come back to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Todo {
    /// When it comes back.
    pub wakes: Option<DateMark>,
    pub deadline: Option<DateMark>,
    /// How many days the curve gives it: under water that long after it
    /// wakes, in view that long before a deadline.
    pub pace_days: u32,
}

/// Everything the user said about one node. Which fields mean anything
/// depends on the node: a note has a body, a label a parent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeMarks {
    pub handled: Option<Cursor>,
    /// Muted for good: nothing from this node reaches the user.
    pub muted: bool,
    /// Out of the way until then.
    pub snoozed: Option<DateMark>,
    pub todo: Option<Todo>,
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
    handled: None,
    muted: false,
    snoozed: None,
    todo: None,
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
    /// Whether the user has put the node away for now: muted, or snoozed
    /// to a time still ahead.
    pub fn put_away(&self, now: chrono::DateTime<chrono::FixedOffset>) -> bool {
        self.muted || self.snoozed.is_some_and(|until| until.is_ahead(now))
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

pub fn handled(node: &NodeId, cursor: Option<Cursor>) -> Write {
    write(node, "handled", cursor.as_ref())
}

pub fn muted(node: &NodeId, muted: bool) -> Write {
    write(node, "muted", muted.then_some(&true))
}

pub fn snooze(node: &NodeId, until: Option<DateMark>) -> Write {
    write(node, "snooze", until.as_ref())
}

pub fn todo(node: &NodeId, todo: Option<Todo>) -> Write {
    write(node, "todo", todo.as_ref())
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
    /// not marks are ignored.
    pub fn apply(&mut self, entries: impl IntoIterator<Item = Write>) -> BTreeSet<NodeId> {
        let mut touched = BTreeSet::new();
        for (key, value) in entries {
            if !key.starts_with(b"n/") {
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
        let prefix = node_prefix(node).into_bytes();
        let mut marks = NodeMarks::default();
        let mut any = false;
        for (key, value) in self.raw.range(prefix.clone()..) {
            let Some(field) = key.strip_prefix(prefix.as_slice()) else {
                break;
            };
            any = true;
            let Ok(field) = std::str::from_utf8(field) else {
                continue;
            };
            match field {
                "handled" => marks.handled = decode(value),
                "muted" => marks.muted = decode(value).unwrap_or(false),
                "snooze" => marks.snoozed = decode(value),
                "todo" => marks.todo = decode(value),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn agent() -> NodeId {
        NodeId::Agent(rho_agent_types::AgentId::from_encoded("00jvj4xuk96p").unwrap())
    }

    #[test]
    fn marks_read_back_typed_and_say_which_node_moved() {
        let mut marks = Marks::default();
        let rho = uuid::Uuid::new_v4();
        let touched = marks.apply([
            handled(&agent(), Some(Cursor::Story(7))),
            muted(&agent(), true),
            label(&agent(), rho, true),
            name(&NodeId::Label(rho), Some("rho".into())),
        ]);
        assert_eq!(touched, BTreeSet::from([agent(), NodeId::Label(rho)]));
        let held = marks.get(&agent());
        assert_eq!(held.handled, Some(Cursor::Story(7)));
        assert!(held.muted);
        assert_eq!(held.labels, BTreeSet::from([rho]));
        assert_eq!(marks.labeled(rho), [agent()]);
    }

    #[test]
    fn taking_a_mark_away_leaves_the_node_as_if_never_marked() {
        let mut marks = Marks::default();
        marks.apply([muted(&agent(), true)]);
        marks.apply([muted(&agent(), false)]);
        assert_eq!(marks.get(&agent()), &NodeMarks::default());
        assert_eq!(marks.nodes().count(), 0);
    }

    #[test]
    fn the_inverse_puts_back_what_was_there() {
        let mut marks = Marks::default();
        marks.apply([snooze(&agent(), Some(DateMark::at(5)))]);
        let change = vec![
            snooze(&agent(), Some(DateMark::at(9))),
            muted(&agent(), true),
        ];
        let undo = marks.inverse(&change);
        marks.apply(change);
        marks.apply(undo);
        assert_eq!(marks.get(&agent()).snoozed, Some(DateMark::at(5)));
        assert!(!marks.get(&agent()).muted);
    }

    #[test]
    fn a_field_this_build_does_not_know_is_kept() {
        let mut marks = Marks::default();
        let unknown = (
            format!("n/{}|sparkle", agent().key()).into_bytes(),
            Some(vec![1, 2, 3]),
        );
        marks.apply([unknown.clone(), muted(&agent(), true)]);
        assert!(marks.get(&agent()).muted);
        assert_eq!(marks.inverse(std::slice::from_ref(&unknown)), [unknown]);
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
