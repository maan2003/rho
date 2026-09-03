//! Cell-based convergent storage for the Desk.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use camino::Utf8PathBuf;
use rho_core::AgentId;
use senax_encoder::{Decode, Encode, Pack, Unpack};

use crate::{NodeId, PageId};

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct DeviceId(pub [u8; 16]);

/// A device-local Lamport version. Ordering is version first; device only
/// breaks ties, regardless of the field declaration order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Encode, Decode, Pack, Unpack)]
pub struct Stamp {
    pub device: DeviceId,
    pub version: u64,
}

impl Ord for Stamp {
    fn cmp(&self, other: &Self) -> Ordering {
        self.version
            .cmp(&other.version)
            .then_with(|| self.device.cmp(&other.device))
    }
}

impl PartialOrd for Stamp {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub enum NodeKind {
    Note,
    Agent,
    Page,
    Thread,
    PullRequest,
    File,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum State {
    #[default]
    Open,
    Done,
    Dismissed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode, Pack, Unpack)]
pub enum TimestampPrecision {
    Day,
    Minute,
    Millisecond,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode, Pack, Unpack)]
pub struct Timestamp {
    pub unix_ms: i64,
    pub precision: TimestampPrecision,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode, Pack, Unpack)]
pub enum Field {
    Kind,
    Parent,
    Deleted,
    CreatedAt,
    State,
    DeferUntil,
    Deadline,
    PaceDays,
    Tag(String),
    AgentId,
    Host,
    PageRef,
    Url,
    Workspace,
    Channel,
    ThreadTs,
    Repo,
    PullRequestNumber,
    Path,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Value {
    Kind(NodeKind),
    Parent(Option<NodeId>),
    Bool(bool),
    Timestamp(Timestamp),
    OptionalTimestamp(Option<Timestamp>),
    State(State),
    AgentId(AgentId),
    Host(u64),
    PageRef(PageId),
    Text(String),
    Number(u64),
    Days(u32),
    Path(Utf8PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct Cell {
    pub node: NodeId,
    pub field: Field,
    pub stamp: Stamp,
    pub value: Value,
}

impl Cell {
    pub fn new(node: NodeId, field: Field, stamp: Stamp, value: Value) -> Result<Self, String> {
        if !value_matches(&field, &value) {
            return Err("Desk cell value does not match its field".into());
        }
        Ok(Self {
            node,
            field,
            stamp,
            value,
        })
    }
}

fn value_matches(field: &Field, value: &Value) -> bool {
    matches!(
        (field, value),
        (Field::Kind, Value::Kind(_))
            | (Field::Parent, Value::Parent(_))
            | (Field::Deleted | Field::Tag(_), Value::Bool(_))
            | (Field::CreatedAt, Value::Timestamp(_))
            | (Field::State, Value::State(_))
            | (
                Field::DeferUntil | Field::Deadline,
                Value::OptionalTimestamp(_)
            )
            | (Field::AgentId, Value::AgentId(_))
            | (Field::Host, Value::Host(_))
            | (Field::PageRef, Value::PageRef(_))
            | (
                Field::Url | Field::Workspace | Field::Channel | Field::ThreadTs | Field::Repo,
                Value::Text(_)
            )
            | (Field::PullRequestNumber, Value::Number(_))
            | (Field::PaceDays, Value::Days(_))
            | (Field::Path, Value::Path(_))
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Verdict {
    Done,
    Dismiss,
    Defer { until: Timestamp },
    Todo { note: NodeId },
    File { parent: NodeId },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct FieldChange {
    pub node: NodeId,
    pub field: Field,
    pub before: Option<Value>,
    pub after: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum VerdictEvent {
    Applied {
        verdict: Verdict,
        at: Stamp,
        changes: Vec<FieldChange>,
    },
    Undone {
        of: Stamp,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct CellWrite {
    pub node: NodeId,
    pub field: Field,
    pub value: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct CellMutation {
    pub stamp: Stamp,
    pub writes: Vec<CellWrite>,
    pub verdict: Option<(NodeId, VerdictEvent)>,
}

pub type Version = BTreeMap<DeviceId, u64>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct Snapshot {
    pub cells: Vec<Cell>,
    pub verdicts: Vec<(NodeId, Stamp, VerdictEvent)>,
    pub version: Version,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterializedNode {
    pub id: NodeId,
    pub kind: NodeKind,
    pub parent: Option<NodeId>,
    pub created_at: Timestamp,
    pub state: State,
    pub defer_until: Option<Timestamp>,
    pub deadline: Option<Timestamp>,
    pub pace_days: u32,
    pub tags: BTreeSet<String>,
    pub fields: BTreeMap<Field, Value>,
}

#[derive(Clone, Debug)]
pub struct Store {
    device: DeviceId,
    clock: u64,
    cells: BTreeMap<(NodeId, Field), Cell>,
    verdicts: BTreeMap<(NodeId, Stamp), VerdictEvent>,
    version: Version,
}

impl Store {
    pub fn new(device: DeviceId) -> Self {
        Self {
            device,
            clock: 0,
            cells: BTreeMap::new(),
            verdicts: BTreeMap::new(),
            version: Version::new(),
        }
    }

    pub fn version(&self) -> &Version {
        &self.version
    }

    pub fn from_snapshot(device: DeviceId, snapshot: Snapshot) -> Result<Self, String> {
        let mut store = Self::new(device);
        store.merge(snapshot)?;
        Ok(store)
    }

    pub fn apply_mutation(&mut self, mutation: &CellMutation) -> Result<(), String> {
        if mutation.writes.is_empty() || mutation.writes.len() > 4096 {
            return Err("Desk cell mutation write count is invalid".into());
        }
        if mutation.verdict.as_ref().is_some_and(|(_, event)| {
            matches!(event, VerdictEvent::Applied { changes, .. } if changes.len() > 4096)
        }) {
            return Err("Desk verdict change count is invalid".into());
        }
        if let Some((_, VerdictEvent::Applied { at, .. })) = &mutation.verdict
            && *at != mutation.stamp
        {
            return Err("Desk verdict event stamp does not match its mutation".into());
        }
        let delta = Snapshot {
            cells: mutation
                .writes
                .iter()
                .map(|write| {
                    Cell::new(
                        write.node,
                        write.field.clone(),
                        mutation.stamp,
                        write.value.clone(),
                    )
                })
                .collect::<Result<Vec<_>, _>>()?,
            verdicts: mutation
                .verdict
                .iter()
                .map(|(node, event)| (*node, mutation.stamp, event.clone()))
                .collect(),
            version: Version::from([(mutation.stamp.device, mutation.stamp.version)]),
        };
        self.merge(delta)
    }

    pub fn write(&mut self, node: NodeId, field: Field, value: Value) -> Result<Cell, String> {
        if matches!(field, Field::Kind) && self.cells.contains_key(&(node, Field::Kind)) {
            return Err("Desk node kind is write-once".into());
        }
        self.clock = self
            .clock
            .checked_add(1)
            .ok_or("Desk device version exhausted")?;
        let cell = Cell::new(
            node,
            field,
            Stamp {
                device: self.device,
                version: self.clock,
            },
            value,
        )?;
        self.merge_cell(cell.clone())?;
        Ok(cell)
    }

    pub fn append_verdict(
        &mut self,
        node: NodeId,
        verdict: Verdict,
        changes: Vec<FieldChange>,
    ) -> Result<Stamp, String> {
        self.clock = self
            .clock
            .checked_add(1)
            .ok_or("Desk device version exhausted")?;
        let stamp = Stamp {
            device: self.device,
            version: self.clock,
        };
        self.verdicts.insert(
            (node, stamp),
            VerdictEvent::Applied {
                verdict,
                at: stamp,
                changes,
            },
        );
        self.observe(stamp);
        Ok(stamp)
    }

    pub fn append_undone(&mut self, node: NodeId, of: Stamp) -> Result<Stamp, String> {
        self.clock = self
            .clock
            .checked_add(1)
            .ok_or("Desk device version exhausted")?;
        let stamp = Stamp {
            device: self.device,
            version: self.clock,
        };
        self.verdicts
            .insert((node, stamp), VerdictEvent::Undone { of });
        self.observe(stamp);
        Ok(stamp)
    }

    pub fn merge(&mut self, snapshot: Snapshot) -> Result<(), String> {
        let mut staged = self.clone();
        staged.merge_inner(snapshot)?;
        *self = staged;
        Ok(())
    }

    fn merge_inner(&mut self, snapshot: Snapshot) -> Result<(), String> {
        for cell in snapshot.cells {
            self.merge_cell(cell)?;
        }
        for (node, stamp, event) in snapshot.verdicts {
            if matches!(&event, VerdictEvent::Applied { at, .. } if *at != stamp) {
                return Err("Desk verdict event stamp does not match its key".into());
            }
            if let Some(existing) = self.verdicts.get(&(node, stamp)) {
                if existing != &event {
                    return Err("conflicting Desk verdict event at the same stamp".into());
                }
            } else {
                self.verdicts.insert((node, stamp), event);
            }
            self.observe(stamp);
        }
        for (device, version) in snapshot.version {
            self.version
                .entry(device)
                .and_modify(|v| *v = (*v).max(version))
                .or_insert(version);
            self.clock = self.clock.max(version);
        }
        Ok(())
    }

    pub fn since(&self, version: &Version) -> Snapshot {
        Snapshot {
            cells: self
                .cells
                .values()
                .filter(|cell| {
                    cell.stamp.version > version.get(&cell.stamp.device).copied().unwrap_or(0)
                })
                .cloned()
                .collect(),
            verdicts: self
                .verdicts
                .iter()
                .filter(|((_, stamp), _)| {
                    stamp.version > version.get(&stamp.device).copied().unwrap_or(0)
                })
                .map(|((node, stamp), event)| (*node, *stamp, event.clone()))
                .collect(),
            version: self.version.clone(),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.since(&Version::new())
    }

    fn merge_cell(&mut self, cell: Cell) -> Result<(), String> {
        if !value_matches(&cell.field, &cell.value) {
            return Err("Desk cell value does not match its field".into());
        }
        let key = (cell.node, cell.field.clone());
        if let Some(old) = self.cells.get(&key) {
            if old.stamp == cell.stamp {
                return if old == &cell {
                    Ok(())
                } else {
                    Err("conflicting Desk cell values at the same stamp".into())
                };
            }
            if cell.field == Field::Kind && old.value != cell.value {
                // Kind is written once. Concurrent duplicate creates converge
                // on the earliest stamp rather than arrival order.
                let replace = cell.stamp < old.stamp;
                self.observe(cell.stamp);
                if replace {
                    self.cells.insert(key, cell);
                }
                return Ok(());
            }
        }
        let replace = self.cells.get(&key).is_none_or(|old| wins(&cell, old));
        self.observe(cell.stamp);
        if replace {
            self.cells.insert(key, cell);
        }
        Ok(())
    }

    fn observe(&mut self, stamp: Stamp) {
        self.clock = self.clock.max(stamp.version);
        self.version
            .entry(stamp.device)
            .and_modify(|v| *v = (*v).max(stamp.version))
            .or_insert(stamp.version);
    }

    pub fn materialize(&self) -> Vec<MaterializedNode> {
        let all_ids = self
            .cells
            .keys()
            .map(|(node, _)| *node)
            .collect::<BTreeSet<_>>();
        let deleted = |id: NodeId| self.value(id, &Field::Deleted) == Some(&Value::Bool(true));
        let ids = all_ids
            .iter()
            .copied()
            .filter(|id| {
                !deleted(*id) && matches!(self.value(*id, &Field::Kind), Some(Value::Kind(_)))
            })
            .collect::<BTreeSet<_>>();
        let mut nodes = Vec::new();
        for id in ids.iter().copied() {
            let Some(Value::Kind(kind)) = self.value(id, &Field::Kind).cloned() else {
                continue;
            };
            let requested = match self.value(id, &Field::Parent) {
                Some(Value::Parent(parent)) => *parent,
                _ => None,
            };
            let parent = requested.filter(|_| {
                let mut cursor = requested;
                let mut seen = BTreeSet::from([id]);
                while let Some(ancestor) = cursor {
                    if !seen.insert(ancestor) || !ids.contains(&ancestor) || deleted(ancestor) {
                        return false;
                    }
                    cursor = match self.value(ancestor, &Field::Parent) {
                        Some(Value::Parent(parent)) => *parent,
                        _ => None,
                    };
                }
                true
            });
            let fields = self
                .cells
                .iter()
                .filter(|((node, _), _)| *node == id)
                .map(|((_, field), cell)| (field.clone(), cell.value.clone()))
                .collect::<BTreeMap<_, _>>();
            let tags = fields
                .iter()
                .filter_map(|(field, value)| match (field, value) {
                    (Field::Tag(tag), Value::Bool(true)) => Some(tag.clone()),
                    _ => None,
                })
                .collect();
            nodes.push(MaterializedNode {
                id,
                kind,
                parent,
                created_at: match self.value(id, &Field::CreatedAt) {
                    Some(Value::Timestamp(at)) => *at,
                    _ => Timestamp {
                        unix_ms: 0,
                        precision: TimestampPrecision::Millisecond,
                    },
                },
                state: match self.value(id, &Field::State) {
                    Some(Value::State(state)) => *state,
                    _ => State::Open,
                },
                defer_until: match self.value(id, &Field::DeferUntil) {
                    Some(Value::OptionalTimestamp(at)) => *at,
                    _ => None,
                },
                deadline: match self.value(id, &Field::Deadline) {
                    Some(Value::OptionalTimestamp(at)) => *at,
                    _ => None,
                },
                pace_days: match self.value(id, &Field::PaceDays) {
                    Some(Value::Days(days)) => *days,
                    _ => 0,
                },
                tags,
                fields,
            });
        }
        let mut by_parent = BTreeMap::<Option<NodeId>, Vec<MaterializedNode>>::new();
        for node in nodes {
            by_parent.entry(node.parent).or_default().push(node);
        }
        for children in by_parent.values_mut() {
            children.sort_by_key(|node| (node.created_at, node.id));
        }
        fn visit(
            parent: Option<NodeId>,
            by_parent: &BTreeMap<Option<NodeId>, Vec<MaterializedNode>>,
            output: &mut Vec<MaterializedNode>,
        ) {
            let Some(children) = by_parent.get(&parent) else {
                return;
            };
            for node in children {
                output.push(node.clone());
                visit(Some(node.id), by_parent, output);
            }
        }
        let mut output = Vec::new();
        visit(None, &by_parent, &mut output);
        output
    }

    pub fn value(&self, node: NodeId, field: &Field) -> Option<&Value> {
        self.cells
            .get(&(node, field.clone()))
            .map(|cell| &cell.value)
    }

    pub fn verdict_event(&self, node: NodeId, stamp: Stamp) -> Option<&VerdictEvent> {
        self.verdicts.get(&(node, stamp))
    }
}

fn wins(new: &Cell, old: &Cell) -> bool {
    if matches!(new.field, Field::Tag(_)) && new.stamp.version == old.stamp.version {
        match (&new.value, &old.value) {
            (Value::Bool(true), Value::Bool(false)) => return true,
            (Value::Bool(false), Value::Bool(true)) => return false,
            _ => {}
        }
    }
    new.stamp > old.stamp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(byte: u8) -> DeviceId {
        DeviceId([byte; 16])
    }

    fn node(counter: u64) -> NodeId {
        NodeId {
            replica_id: 1,
            counter,
        }
    }

    fn timestamp(unix_ms: i64) -> Timestamp {
        Timestamp {
            unix_ms,
            precision: TimestampPrecision::Millisecond,
        }
    }

    fn variants() -> Vec<(Field, Value, Value)> {
        vec![
            (
                Field::Kind,
                Value::Kind(NodeKind::Note),
                Value::Kind(NodeKind::Note),
            ),
            (
                Field::Parent,
                Value::Parent(None),
                Value::Parent(Some(node(9))),
            ),
            (Field::Deleted, Value::Bool(false), Value::Bool(true)),
            (
                Field::CreatedAt,
                Value::Timestamp(timestamp(1)),
                Value::Timestamp(timestamp(2)),
            ),
            (
                Field::State,
                Value::State(State::Open),
                Value::State(State::Done),
            ),
            (
                Field::DeferUntil,
                Value::OptionalTimestamp(None),
                Value::OptionalTimestamp(Some(timestamp(2))),
            ),
            (
                Field::Deadline,
                Value::OptionalTimestamp(None),
                Value::OptionalTimestamp(Some(timestamp(3))),
            ),
            (
                Field::Tag("tag".into()),
                Value::Bool(false),
                Value::Bool(true),
            ),
            (
                Field::AgentId,
                Value::AgentId(AgentId::from_counter(1, &rho_core::AgentIdDomain(7)).unwrap()),
                Value::AgentId(AgentId::from_counter(2, &rho_core::AgentIdDomain(7)).unwrap()),
            ),
            (Field::Host, Value::Host(1), Value::Host(2)),
            (
                Field::PageRef,
                Value::PageRef(PageId([1; 16])),
                Value::PageRef(PageId([2; 16])),
            ),
            (Field::Url, Value::Text("a".into()), Value::Text("b".into())),
            (
                Field::Workspace,
                Value::Text("a".into()),
                Value::Text("b".into()),
            ),
            (
                Field::Channel,
                Value::Text("a".into()),
                Value::Text("b".into()),
            ),
            (
                Field::ThreadTs,
                Value::Text("a".into()),
                Value::Text("b".into()),
            ),
            (
                Field::Repo,
                Value::Text("a".into()),
                Value::Text("b".into()),
            ),
            (Field::PullRequestNumber, Value::Number(1), Value::Number(2)),
            (Field::PaceDays, Value::Days(1), Value::Days(2)),
            (
                Field::Path,
                Value::Path(Utf8PathBuf::from("/a")),
                Value::Path(Utf8PathBuf::from("/b")),
            ),
        ]
    }

    #[test]
    fn every_field_merge_is_commutative_and_idempotent() {
        for (field, left_value, right_value) in variants() {
            let left = Cell::new(
                node(1),
                field.clone(),
                Stamp {
                    device: device(1),
                    version: 3,
                },
                left_value,
            )
            .unwrap();
            let right = Cell::new(
                node(1),
                field,
                Stamp {
                    device: device(2),
                    version: 4,
                },
                right_value,
            )
            .unwrap();
            let left_snapshot = Snapshot {
                cells: vec![left],
                verdicts: vec![],
                version: Version::new(),
            };
            let right_snapshot = Snapshot {
                cells: vec![right],
                verdicts: vec![],
                version: Version::new(),
            };
            let mut ab = Store::new(device(8));
            ab.merge(left_snapshot.clone()).unwrap();
            ab.merge(right_snapshot.clone()).unwrap();
            let once = ab.snapshot();
            ab.merge(right_snapshot.clone()).unwrap();
            assert_eq!(ab.snapshot(), once);
            let mut ba = Store::new(device(8));
            ba.merge(right_snapshot).unwrap();
            ba.merge(left_snapshot).unwrap();
            assert_eq!(ba.snapshot(), once);
        }
    }

    #[test]
    fn concurrent_tag_add_wins_independent_of_device_order() {
        let remove = Cell::new(
            node(1),
            Field::Tag("x".into()),
            Stamp {
                device: device(9),
                version: 4,
            },
            Value::Bool(false),
        )
        .unwrap();
        let add = Cell::new(
            node(1),
            Field::Tag("x".into()),
            Stamp {
                device: device(1),
                version: 4,
            },
            Value::Bool(true),
        )
        .unwrap();
        for cells in [
            vec![remove.clone(), add.clone()],
            vec![add.clone(), remove.clone()],
        ] {
            let mut store = Store::new(device(3));
            for cell in cells {
                store
                    .merge(Snapshot {
                        cells: vec![cell],
                        verdicts: vec![],
                        version: Version::new(),
                    })
                    .unwrap();
            }
            assert_eq!(
                store.value(node(1), &Field::Tag("x".into())),
                Some(&Value::Bool(true))
            );
        }
        let mut store = Store::new(device(3));
        store
            .merge(Snapshot {
                cells: vec![add],
                verdicts: vec![],
                version: Version::new(),
            })
            .unwrap();
        store
            .merge(Snapshot {
                cells: vec![
                    Cell::new(
                        node(1),
                        Field::Tag("x".into()),
                        Stamp {
                            device: device(9),
                            version: 5,
                        },
                        Value::Bool(false),
                    )
                    .unwrap(),
                ],
                verdicts: vec![],
                version: Version::new(),
            })
            .unwrap();
        assert_eq!(
            store.value(node(1), &Field::Tag("x".into())),
            Some(&Value::Bool(false))
        );
    }

    #[test]
    fn kind_is_write_once_for_local_mutations() {
        let mut store = Store::new(device(1));
        store
            .write(node(1), Field::Kind, Value::Kind(NodeKind::Note))
            .unwrap();
        assert_eq!(
            store
                .write(node(1), Field::Kind, Value::Kind(NodeKind::Agent))
                .unwrap_err(),
            "Desk node kind is write-once"
        );
    }

    #[test]
    fn concurrent_kind_writes_converge_on_the_first_stamp() {
        let note = Cell::new(
            node(1),
            Field::Kind,
            Stamp {
                device: device(2),
                version: 1,
            },
            Value::Kind(NodeKind::Note),
        )
        .unwrap();
        let agent = Cell::new(
            node(1),
            Field::Kind,
            Stamp {
                device: device(3),
                version: 2,
            },
            Value::Kind(NodeKind::Agent),
        )
        .unwrap();
        let mut left = Store::new(device(1));
        let mut right = Store::new(device(1));
        for cell in [note.clone(), agent.clone()] {
            left.merge(Snapshot {
                cells: vec![cell],
                verdicts: vec![],
                version: Version::new(),
            })
            .unwrap();
        }
        for cell in [agent, note] {
            right
                .merge(Snapshot {
                    cells: vec![cell],
                    verdicts: vec![],
                    version: Version::new(),
                })
                .unwrap();
        }
        assert_eq!(left.snapshot(), right.snapshot());
        assert_eq!(
            left.value(node(1), &Field::Kind),
            Some(&Value::Kind(NodeKind::Note))
        );
    }

    #[test]
    fn losing_writes_still_advance_the_frontier() {
        let mut store = Store::new(device(1));
        let winner = Cell::new(
            node(1),
            Field::State,
            Stamp {
                device: device(2),
                version: 8,
            },
            Value::State(State::Done),
        )
        .unwrap();
        let loser = Cell::new(
            node(1),
            Field::State,
            Stamp {
                device: device(3),
                version: 7,
            },
            Value::State(State::Open),
        )
        .unwrap();
        store
            .merge(Snapshot {
                cells: vec![winner, loser],
                verdicts: vec![],
                version: Version::new(),
            })
            .unwrap();
        assert_eq!(store.version().get(&device(3)), Some(&7));
        let delta = store.since(&Version::from([(device(2), 8)]));
        assert!(delta.cells.is_empty());
        assert_eq!(delta.version.get(&device(3)), Some(&7));
    }

    #[test]
    fn verdict_and_undo_events_merge_as_a_grow_only_union() {
        let applied_stamp = Stamp {
            device: device(2),
            version: 4,
        };
        let undone_stamp = Stamp {
            device: device(3),
            version: 5,
        };
        let applied = VerdictEvent::Applied {
            verdict: Verdict::Done,
            at: applied_stamp,
            changes: vec![FieldChange {
                node: node(1),
                field: Field::State,
                before: Some(Value::State(State::Open)),
                after: Some(Value::State(State::Done)),
            }],
        };
        let undone = VerdictEvent::Undone { of: applied_stamp };
        let mut left = Store::new(device(1));
        left.merge(Snapshot {
            cells: vec![],
            verdicts: vec![(node(1), applied_stamp, applied.clone())],
            version: Version::new(),
        })
        .unwrap();
        left.merge(Snapshot {
            cells: vec![],
            verdicts: vec![(node(1), undone_stamp, undone.clone())],
            version: Version::new(),
        })
        .unwrap();
        let mut right = Store::new(device(1));
        right
            .merge(Snapshot {
                cells: vec![],
                verdicts: vec![(node(1), undone_stamp, undone)],
                version: Version::new(),
            })
            .unwrap();
        right
            .merge(Snapshot {
                cells: vec![],
                verdicts: vec![(node(1), applied_stamp, applied)],
                version: Version::new(),
            })
            .unwrap();
        assert_eq!(left.snapshot(), right.snapshot());
    }

    fn create(store: &mut Store, id: NodeId, parent: Option<NodeId>, created_at: i64) {
        store
            .write(id, Field::Kind, Value::Kind(NodeKind::Note))
            .unwrap();
        store
            .write(id, Field::Parent, Value::Parent(parent))
            .unwrap();
        store
            .write(
                id,
                Field::CreatedAt,
                Value::Timestamp(timestamp(created_at)),
            )
            .unwrap();
    }

    #[test]
    fn cycles_materialize_at_root_without_repairing_cells() {
        let mut store = Store::new(device(1));
        create(&mut store, node(1), Some(node(2)), 1);
        create(&mut store, node(2), Some(node(1)), 2);
        let before = store.snapshot();
        assert!(store.materialize().iter().all(|node| node.parent.is_none()));
        assert_eq!(store.snapshot(), before);
    }

    #[test]
    fn a_partial_parent_cell_cannot_hide_a_materializable_child() {
        let mut store = Store::new(device(1));
        create(&mut store, node(1), Some(node(2)), 1);
        store
            .write(node(2), Field::Parent, Value::Parent(None))
            .unwrap();
        let nodes = store.materialize();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].id, node(1));
        assert_eq!(nodes[0].parent, None);
    }

    #[test]
    fn a_parent_chain_crossing_a_deleted_node_materializes_at_root() {
        let mut store = Store::new(device(1));
        create(&mut store, node(1), None, 1);
        create(&mut store, node(2), Some(node(1)), 2);
        create(&mut store, node(3), Some(node(2)), 3);
        store
            .write(node(1), Field::Deleted, Value::Bool(true))
            .unwrap();
        let nodes = store.materialize();
        assert_eq!(
            nodes.iter().find(|item| item.id == node(2)).unwrap().parent,
            None
        );
        assert_eq!(
            nodes.iter().find(|item| item.id == node(3)).unwrap().parent,
            None
        );
    }

    #[test]
    fn daemon_and_gui_round_trip_only_cells_since_the_peer_version() {
        let mut daemon = Store::new(device(1));
        let mut gui = Store::new(device(2));
        create(&mut gui, node(1), None, 10);
        let daemon_version = daemon.version().clone();
        daemon.merge(gui.since(&daemon_version)).unwrap();
        let gui_version = gui.version().clone();
        daemon
            .write(node(1), Field::State, Value::State(State::Done))
            .unwrap();
        let delta = daemon.since(&gui_version);
        assert_eq!(delta.cells.len(), 1);
        gui.merge(delta).unwrap();
        assert_eq!(gui.snapshot(), daemon.snapshot());
    }

    #[test]
    fn two_guis_converge_through_one_daemon() {
        let mut daemon = Store::new(device(1));
        let mut first = Store::new(device(2));
        let mut second = Store::new(device(3));
        create(&mut first, node(1), None, 10);
        daemon.merge(first.since(daemon.version())).unwrap();
        second.merge(daemon.since(second.version())).unwrap();
        first
            .write(
                node(1),
                Field::Deadline,
                Value::OptionalTimestamp(Some(timestamp(20))),
            )
            .unwrap();
        second
            .write(
                node(1),
                Field::DeferUntil,
                Value::OptionalTimestamp(Some(timestamp(15))),
            )
            .unwrap();
        daemon.merge(first.since(daemon.version())).unwrap();
        daemon.merge(second.since(daemon.version())).unwrap();
        first.merge(daemon.since(first.version())).unwrap();
        second.merge(daemon.since(second.version())).unwrap();
        assert_eq!(first.snapshot(), daemon.snapshot());
        assert_eq!(second.snapshot(), daemon.snapshot());
    }
}
