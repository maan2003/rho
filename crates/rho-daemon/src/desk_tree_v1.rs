#![allow(dead_code)]

//! Convergent movable-tree and per-node text CRDT used by the Desk.
//!
//! This crate owns document mechanics only. Rendering, daemon persistence,
//! and transport policy remain in their respective crates.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use bytes::BytesMut;
use camino::Utf8PathBuf;
use chrono::{Datelike as _, Timelike as _};
use clock::{Global, Lamport, ReplicaId};
use redb::{TableDefinition, TypeName, Value};
use rho_core::AgentId;
use rho_db::{SenValue, WriteTxn};
use senax_encoder::{Decode, Decoder, Encode, Encoder, Pack, Unpack};
use text::{Buffer, BufferId, EditOperation, FullOffset, Operation, UndoOperation};

const STATE: TableDefinition<(), StateValue> = TableDefinition::new("rho_desk_tree_state_v1");
const TREE_OPS: TableDefinition<u64, TreeRecordValue> =
    TableDefinition::new("rho_desk_tree_ops_v1");
const TEXT_OPS: TableDefinition<u64, TextRecordValue> =
    TableDefinition::new("rho_desk_node_text_ops_v1");
const BATCH_OPS: TableDefinition<u64, BatchRecordValue> =
    TableDefinition::new("rho_desk_batch_ops_v1");

#[derive(Clone, Debug, Encode, Decode)]
struct PersistentState {
    snapshot: Snapshot,
    next_sequence: u64,
    next_replica_id: u16,
    #[senax(default)]
    next_machine_counter: u64,
    #[senax(default)]
    next_machine_clock: u32,
}

#[derive(Debug)]
struct StateValue;
#[derive(Debug)]
struct TreeRecordValue;
#[derive(Debug)]
struct TextRecordValue;
#[derive(Debug)]
struct BatchRecordValue;

macro_rules! frozen_value {
    ($marker:ty, $record:ty, $name:literal) => {
        impl Value for $marker {
            type SelfType<'a> = SenValue<'a, $record>;
            type AsBytes<'a> = BytesMut;
            fn fixed_width() -> Option<usize> {
                None
            }
            fn from_bytes<'a>(data: &'a [u8]) -> Self::SelfType<'a>
            where
                Self: 'a,
            {
                let mut data = data;
                SenValue::owned(<$record>::decode(&mut data).expect("decode frozen Desk tree v1"))
            }
            fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
            where
                Self: 'b,
            {
                let mut bytes = BytesMut::new();
                value
                    .as_ref()
                    .encode(&mut bytes)
                    .expect("encode frozen Desk tree v1");
                bytes
            }
            fn type_name() -> TypeName {
                TypeName::new($name)
            }
        }
    };
}

frozen_value!(
    StateValue,
    PersistentState,
    "rho-db::Sen<rho_daemon::desk_tree::PersistentState>"
);
frozen_value!(
    TreeRecordValue,
    TreeOpRecord,
    "rho-db::Sen<rho_desk::TreeOpRecord>"
);
frozen_value!(
    TextRecordValue,
    TextOpRecord,
    "rho-db::Sen<rho_desk::TextOpRecord>"
);
frozen_value!(
    BatchRecordValue,
    BatchOpRecord,
    "rho-db::Sen<rho_desk::BatchOpRecord>"
);

/// Stable identity of a Desk node. Counters are allocated independently by
/// each replica; the replica component makes the pair globally unique.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct NodeId {
    pub replica_id: u16,
    pub counter: u64,
}

/// Lamport timestamp used by structural operations.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct TreeClock {
    pub value: u32,
    pub replica_id: u16,
}

impl From<Lamport> for TreeClock {
    fn from(value: Lamport) -> Self {
        Self {
            value: value.value,
            replica_id: value.replica_id.as_u16(),
        }
    }
}

impl From<TreeClock> for Lamport {
    fn from(value: TreeClock) -> Self {
        Lamport {
            value: value.value,
            replica_id: ReplicaId::new(value.replica_id),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum NodeKind {
    Heading,
    Prose,
    Agent,
    Page,
    File,
    Draft,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum NodeOwner {
    User,
    Machine,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ReplicaAuthor {
    User,
    Agent(AgentId),
    Machine,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct Replica {
    pub replica_id: u16,
    pub author: ReplicaAuthor,
}

/// Opaque browser-page identity without coupling the document model to the
/// browser crate.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct PageId(pub [u8; 16]);

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub enum TemporalKind {
    Todo,
    Deadline,
    Defer,
    Reminder,
    Done,
    Discarded,
}

/// A civil date/time. `minute_of_day == None` retains calendar-day semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct TemporalMark {
    pub year: i32,
    pub month: u8,
    pub day: u8,
    pub minute_of_day: Option<u16>,
    pub pace_days: u32,
}

impl TemporalMark {
    pub fn at(self) -> Option<chrono::NaiveDateTime> {
        chrono::NaiveDate::from_ymd_opt(self.year, self.month.into(), self.day.into()).and_then(
            |date| {
                chrono::NaiveTime::from_num_seconds_from_midnight_opt(
                    u32::from(self.minute_of_day.unwrap_or(0)) * 60,
                    0,
                )
                .map(|time| date.and_time(time))
            },
        )
    }
}

pub fn temporal_priority(
    kind: TemporalKind,
    mark: TemporalMark,
    now: chrono::NaiveDateTime,
) -> f64 {
    let Some(at) = mark.at() else {
        return f64::NEG_INFINITY;
    };
    let elapsed = if mark.minute_of_day.is_none() {
        now.date().signed_duration_since(at.date()).num_days() as f64
    } else {
        now.signed_duration_since(at).num_seconds() as f64 / 86_400.0
    };
    let pace = f64::from(mark.pace_days);
    match kind {
        TemporalKind::Deadline if elapsed < -pace => f64::NEG_INFINITY,
        TemporalKind::Deadline if elapsed <= 0.0 => elapsed / pace.max(1.0),
        TemporalKind::Deadline => 1_000_000.0 + elapsed,
        TemporalKind::Todo => elapsed - pace,
        TemporalKind::Defer if elapsed < 0.0 => f64::NEG_INFINITY,
        TemporalKind::Defer => elapsed,
        TemporalKind::Reminder if elapsed < 0.0 => f64::NEG_INFINITY,
        TemporalKind::Reminder => -elapsed / pace.max(1.0),
        TemporalKind::Done | TemporalKind::Discarded => f64::NEG_INFINITY,
    }
}

/// Renders a snapshot as a human-readable, org-looking tree. This is a
/// presentation format only: no runtime path parses it back into Desk state.
pub fn render_org(snapshot: Snapshot) -> Result<String, String> {
    let document = Document::from_snapshot(snapshot)?;
    let nodes = document.materialize();
    let by_id = nodes
        .iter()
        .map(|node| (node.id, node))
        .collect::<BTreeMap<_, _>>();
    let mut output = String::new();
    for node in &nodes {
        let text = document.text(
            node.id,
            ReplicaId::REMOTE_SERVER.as_u16(),
            BufferId::new(1).unwrap(),
        )?;
        match node.kind {
            NodeKind::Heading => {
                let depth = std::iter::successors(node.parent, |parent| {
                    by_id.get(parent).and_then(|node| node.parent)
                })
                .filter(|parent| {
                    by_id
                        .get(parent)
                        .is_some_and(|node| node.kind == NodeKind::Heading)
                })
                .count()
                    + 1;
                output.push_str(&"*".repeat(depth));
                output.push(' ');
                output.push_str(text.trim());
                if !node.tags.is_empty() {
                    output.push(' ');
                    output.push(':');
                    output.push_str(&node.tags.iter().cloned().collect::<Vec<_>>().join(":"));
                    output.push(':');
                }
                output.push('\n');
                for (kind, mark) in &node.temporal {
                    output.push_str(&format!(
                        ":{}: {:04}-{:02}-{:02}",
                        temporal_name(*kind),
                        mark.year,
                        mark.month,
                        mark.day
                    ));
                    if let Some(minute) = mark.minute_of_day {
                        output.push_str(&format!(" {:02}:{:02}", minute / 60, minute % 60));
                    }
                    if mark.pace_days != 0 {
                        output.push_str(&format!(" {}d", mark.pace_days));
                    }
                    output.push('\n');
                }
            }
            NodeKind::Prose => {
                output.push_str(&text);
                if !text.ends_with('\n') {
                    output.push('\n');
                }
            }
            NodeKind::Agent => output.push_str("- agent\n"),
            NodeKind::Page => output.push_str("- page\n"),
            NodeKind::File => {
                output.push_str("- file");
                if let Some(Binding::File(path)) = node.bindings.get(&BindingKind::File) {
                    output.push(' ');
                    output.push_str(path.as_str());
                }
                output.push('\n');
            }
            NodeKind::Draft => {
                output.push_str("- draft");
                if !text.trim().is_empty() {
                    output.push(' ');
                    output.push_str(text.trim());
                }
                output.push('\n');
            }
        }
    }
    Ok(output)
}

fn temporal_name(kind: TemporalKind) -> &'static str {
    match kind {
        TemporalKind::Todo => "todo",
        TemporalKind::Deadline => "deadline",
        TemporalKind::Defer => "defer",
        TemporalKind::Reminder => "reminder",
        TemporalKind::Done => "done",
        TemporalKind::Discarded => "discarded",
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub enum BindingKind {
    Agent,
    Page,
    File,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Binding {
    Agent(AgentId),
    Page(PageId),
    File(Utf8PathBuf),
}

impl Binding {
    pub fn kind(&self) -> BindingKind {
        match self {
            Self::Agent(_) => BindingKind::Agent,
            Self::Page(_) => BindingKind::Page,
            Self::File(_) => BindingKind::File,
        }
    }
}

/// Dense fractional position. Equal positions are valid: materialization uses
/// the placement timestamp and node id as deterministic tie breakers.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Encode, Decode, Pack, Unpack)]
pub struct OrderKey(pub Vec<u16>);

impl OrderKey {
    pub fn between(left: Option<&Self>, right: Option<&Self>) -> Self {
        let left = left.map(|key| key.0.as_slice()).unwrap_or(&[]);
        let right = right.map(|key| key.0.as_slice()).unwrap_or(&[]);
        let mut result = Vec::new();
        let mut depth = 0;
        loop {
            let lo = left.get(depth).copied().unwrap_or(0);
            let hi = right.get(depth).copied().unwrap_or(u16::MAX);
            if lo.saturating_add(1) < hi {
                result.push(lo + (hi - lo) / 2);
                return Self(result);
            }
            result.push(lo);
            depth += 1;
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum TreeOperation {
    Create {
        timestamp: TreeClock,
        node_id: NodeId,
        kind: NodeKind,
        owner: NodeOwner,
        parent: Option<NodeId>,
        order: OrderKey,
    },
    Move {
        timestamp: TreeClock,
        node_id: NodeId,
        parent: Option<NodeId>,
        order: OrderKey,
    },
    Delete {
        timestamp: TreeClock,
        node_ids: Vec<NodeId>,
    },
    SetTemporal {
        timestamp: TreeClock,
        node_id: NodeId,
        kind: TemporalKind,
        value: Option<TemporalMark>,
    },
    SetBinding {
        timestamp: TreeClock,
        node_id: NodeId,
        kind: BindingKind,
        value: Option<Binding>,
    },
    SetTag {
        timestamp: TreeClock,
        node_id: NodeId,
        tag: String,
        present: bool,
    },
}

impl TreeOperation {
    pub fn timestamp(&self) -> TreeClock {
        match self {
            Self::Create { timestamp, .. }
            | Self::Move { timestamp, .. }
            | Self::Delete { timestamp, .. }
            | Self::SetTemporal { timestamp, .. }
            | Self::SetBinding { timestamp, .. }
            | Self::SetTag { timestamp, .. } => *timestamp,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum TextOperation {
    Edit {
        timestamp: TreeClock,
        version: Vec<TreeClock>,
        ranges: Vec<(u64, u64)>,
        new_text: Vec<String>,
    },
    Undo {
        timestamp: TreeClock,
        version: Vec<TreeClock>,
        counts: Vec<(TreeClock, u32)>,
    },
}

impl TextOperation {
    pub fn timestamp(&self) -> TreeClock {
        match self {
            Self::Edit { timestamp, .. } | Self::Undo { timestamp, .. } => *timestamp,
        }
    }

    pub fn from_text(operation: &Operation) -> Self {
        match operation {
            Operation::Edit(edit) => Self::Edit {
                timestamp: edit.timestamp.into(),
                version: edit
                    .version
                    .iter()
                    .filter(|c| c.value != 0)
                    .map(Into::into)
                    .collect(),
                ranges: edit
                    .ranges
                    .iter()
                    .map(|r| (r.start.0 as u64, r.end.0 as u64))
                    .collect(),
                new_text: edit.new_text.iter().map(ToString::to_string).collect(),
            },
            Operation::Undo(undo) => Self::Undo {
                timestamp: undo.timestamp.into(),
                version: undo
                    .version
                    .iter()
                    .filter(|c| c.value != 0)
                    .map(Into::into)
                    .collect(),
                counts: undo
                    .counts
                    .iter()
                    .map(|(clock, count)| ((*clock).into(), *count))
                    .collect(),
            },
        }
    }

    pub fn to_text(&self) -> Result<Operation, String> {
        Ok(match self {
            Self::Edit {
                timestamp,
                version,
                ranges,
                new_text,
            } => {
                if ranges.len() != new_text.len() {
                    return Err("Desk node edit range/text count mismatch".into());
                }
                if ranges.len() > 65_536
                    || new_text.iter().map(String::len).sum::<usize>() > 4 * 1024 * 1024
                {
                    return Err("Desk node edit is too large".into());
                }
                Operation::Edit(EditOperation {
                    timestamp: (*timestamp).into(),
                    version: version.iter().copied().map(Into::into).collect::<Global>(),
                    ranges: ranges
                        .iter()
                        .map(|(start, end)| {
                            Ok(FullOffset(
                                usize::try_from(*start)
                                    .map_err(|_| "Desk node edit offset overflow")?,
                            )
                                ..FullOffset(
                                    usize::try_from(*end)
                                        .map_err(|_| "Desk node edit offset overflow")?,
                                ))
                        })
                        .collect::<Result<_, String>>()?,
                    new_text: new_text.iter().map(|text| text.as_str().into()).collect(),
                })
            }
            Self::Undo {
                timestamp,
                version,
                counts,
            } => Operation::Undo(UndoOperation {
                timestamp: (*timestamp).into(),
                version: version.iter().copied().map(Into::into).collect(),
                counts: counts
                    .iter()
                    .map(|(clock, count)| ((*clock).into(), *count))
                    .collect(),
            }),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct TextTransaction {
    pub id: TreeClock,
    pub edit_ids: Vec<TreeClock>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct TreeOpRecord {
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub operation: TreeOperation,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct TextOpRecord {
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub node_id: NodeId,
    pub operation: TextOperation,
    pub transaction: Option<TextTransaction>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum BatchOperation {
    Tree(TreeOperation),
    Text {
        node_id: NodeId,
        operation: TextOperation,
        transaction: Option<TextTransaction>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct NodeReplacement {
    pub deleted: NodeId,
    pub replacement: NodeId,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum MachineRelocationIntent {
    EvacuateDeletedChildren,
    Restore {
        delete_batch_id: TreeClock,
        replacements: Vec<NodeReplacement>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct NodeExpectation {
    pub node_id: NodeId,
    pub kind: NodeKind,
    pub owner: NodeOwner,
    pub parent: Option<NodeId>,
    pub order: OrderKey,
    pub text_version: Vec<TreeClock>,
}

/// One atomic structural/textual Desk mutation. Expected versions are exact
/// source-node preconditions; a mismatch rejects the entire batch.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct OperationBatch {
    pub id: TreeClock,
    pub expected: Vec<NodeExpectation>,
    pub operations: Vec<BatchOperation>,
    #[senax(default)]
    pub machine_relocation: Option<MachineRelocationIntent>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct BatchOpRecord {
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub batch: OperationBatch,
    #[senax(default)]
    pub daemon_tree_operations: Vec<TreeOperation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct NodeTextSnapshot {
    pub node_id: NodeId,
    pub operations: Vec<TextOperation>,
    pub transactions: Vec<TextTransaction>,
}

impl NodeTextSnapshot {
    pub fn buffer(&self, replica_id: u16, buffer_id: BufferId) -> Result<Buffer, String> {
        let mut buffer = Buffer::new(ReplicaId::new(replica_id), buffer_id, "");
        buffer.apply_ops(
            self.operations
                .iter()
                .map(TextOperation::to_text)
                .collect::<Result<Vec<_>, _>>()?,
        );
        if buffer.has_deferred_ops() {
            return Err("Desk node text has causally incomplete operation history".into());
        }
        Ok(buffer)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct NodeRecord {
    pub id: NodeId,
    pub kind: NodeKind,
    pub owner: NodeOwner,
    /// All placement candidates are retained so cycle resolution can fall back
    /// deterministically when a later concurrent move invalidates a winner.
    pub placements: Vec<Placement>,
    pub deleted_at: Option<TreeClock>,
    pub temporal: Vec<(TemporalKind, TreeClock, Option<TemporalMark>)>,
    pub bindings: Vec<(BindingKind, TreeClock, Option<Binding>)>,
    pub tags: Vec<(String, TreeClock, bool)>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct Placement {
    pub timestamp: TreeClock,
    pub parent: Option<NodeId>,
    pub order: OrderKey,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct Snapshot {
    pub nodes: Vec<NodeRecord>,
    pub texts: Vec<NodeTextSnapshot>,
    pub version: Vec<TreeClock>,
    pub replicas: Vec<Replica>,
    /// Last operation sequence included by the daemon. Plain in-memory
    /// documents leave this at zero.
    #[senax(default)]
    pub sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterializedNode {
    pub id: NodeId,
    pub kind: NodeKind,
    pub owner: NodeOwner,
    pub parent: Option<NodeId>,
    pub order: OrderKey,
    pub temporal: BTreeMap<TemporalKind, TemporalMark>,
    pub bindings: BTreeMap<BindingKind, Binding>,
    pub tags: BTreeSet<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Document {
    nodes: BTreeMap<NodeId, NodeRecord>,
    texts: BTreeMap<NodeId, NodeTextSnapshot>,
    seen: BTreeSet<TreeClock>,
    replicas: Vec<Replica>,
}

impl Document {
    pub fn replicas(&self) -> &[Replica] {
        &self.replicas
    }

    pub fn add_replica(&mut self, replica: Replica) {
        self.replicas.push(replica);
    }

    pub fn from_snapshot(snapshot: Snapshot) -> Result<Self, String> {
        let mut document = Self::default();
        for node in snapshot.nodes {
            if document.nodes.insert(node.id, node).is_some() {
                return Err("duplicate Desk node id".into());
            }
        }
        for text in snapshot.texts {
            if document.texts.insert(text.node_id, text).is_some() {
                return Err("duplicate Desk node text".into());
            }
        }
        document.seen.extend(snapshot.version);
        document.replicas = snapshot.replicas;
        Ok(document)
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            nodes: self.nodes.values().cloned().collect(),
            texts: self.texts.values().cloned().collect(),
            version: self.seen.iter().copied().collect(),
            replicas: self.replicas.clone(),
            sequence: 0,
        }
    }

    pub fn contains_operation(&self, timestamp: TreeClock) -> bool {
        self.seen.contains(&timestamp)
    }

    /// Returns ownership from the durable node record, including tombstones.
    pub fn owner(&self, node_id: NodeId) -> Option<NodeOwner> {
        self.nodes.get(&node_id).map(|node| node.owner)
    }

    pub fn apply(&mut self, operation: TreeOperation) -> Result<bool, String> {
        let timestamp = operation.timestamp();
        if self.seen.contains(&timestamp) {
            return Ok(false);
        }
        match &operation {
            TreeOperation::Create {
                node_id,
                parent,
                order,
                ..
            } => {
                if node_id.replica_id != timestamp.replica_id {
                    return Err("Desk node id was allocated by another replica".into());
                }
                if self.nodes.contains_key(node_id) {
                    return Err("duplicate Desk node id".into());
                }
                if *parent == Some(*node_id) {
                    return Err("Desk node cannot parent itself".into());
                }
                validate_order(order)?;
            }
            TreeOperation::Move {
                node_id,
                parent,
                order,
                ..
            } => {
                if !self.nodes.contains_key(node_id) {
                    return Err("move references an unknown Desk node".into());
                }
                if *parent == Some(*node_id) {
                    return Err("Desk node cannot parent itself".into());
                }
                validate_order(order)?;
            }
            TreeOperation::Delete { node_ids, .. } => {
                if node_ids.len() > 65_536 {
                    return Err("Desk delete is too large".into());
                }
                if node_ids
                    .iter()
                    .any(|node_id| !self.nodes.contains_key(node_id))
                {
                    return Err("delete references an unknown Desk node".into());
                }
            }
            TreeOperation::SetTemporal { node_id, value, .. } => {
                if !self.nodes.contains_key(node_id) {
                    return Err("metadata references an unknown Desk node".into());
                }
                if value.is_some_and(|mark| {
                    mark.at().is_none()
                        || mark.minute_of_day.is_some_and(|minute| minute >= 24 * 60)
                }) {
                    return Err("invalid Desk temporal mark".into());
                }
            }
            TreeOperation::SetBinding {
                node_id,
                kind,
                value,
                ..
            } => {
                if value.as_ref().is_some_and(|value| value.kind() != *kind) {
                    return Err("Desk binding kind/value mismatch".into());
                }
                if !self.nodes.contains_key(node_id) {
                    return Err("binding references an unknown Desk node".into());
                }
                let node = &self.nodes[node_id];
                let allowed = match kind {
                    BindingKind::Agent => {
                        node.kind == NodeKind::Agent && node.owner == NodeOwner::Machine
                    }
                    BindingKind::Page => {
                        node.kind == NodeKind::Page && node.owner == NodeOwner::Machine
                    }
                    BindingKind::File => {
                        node.kind == NodeKind::File
                            || (node.kind == NodeKind::Heading && node.owner == NodeOwner::User)
                    }
                };
                if !allowed {
                    return Err("Desk binding is not valid for this node kind and owner".into());
                }
            }
            TreeOperation::SetTag { node_id, tag, .. } => {
                if tag.is_empty() || tag.len() > 256 {
                    return Err("invalid Desk tag".into());
                }
                if !self.nodes.contains_key(node_id) {
                    return Err("tag references an unknown Desk node".into());
                }
            }
        }
        match operation {
            TreeOperation::Create {
                node_id,
                kind,
                owner,
                parent,
                order,
                ..
            } => {
                self.nodes.insert(
                    node_id,
                    NodeRecord {
                        id: node_id,
                        kind,
                        owner,
                        placements: vec![Placement {
                            timestamp,
                            parent,
                            order,
                        }],
                        deleted_at: None,
                        temporal: Vec::new(),
                        bindings: Vec::new(),
                        tags: Vec::new(),
                    },
                );
                self.texts.insert(
                    node_id,
                    NodeTextSnapshot {
                        node_id,
                        operations: Vec::new(),
                        transactions: Vec::new(),
                    },
                );
            }
            TreeOperation::Move {
                node_id,
                parent,
                order,
                ..
            } => {
                let node = self.nodes.get_mut(&node_id).expect("node validated");
                node.placements.push(Placement {
                    timestamp,
                    parent,
                    order,
                });
            }
            TreeOperation::Delete { node_ids, .. } => {
                for node_id in node_ids {
                    let node = self.nodes.get_mut(&node_id).expect("node validated");
                    if node.deleted_at.is_none_or(|old| old < timestamp) {
                        node.deleted_at = Some(timestamp);
                    }
                }
            }
            TreeOperation::SetTemporal {
                node_id,
                kind,
                value,
                ..
            } => {
                let node = self.nodes.get_mut(&node_id).expect("node validated");
                set_lww(&mut node.temporal, kind, timestamp, value);
            }
            TreeOperation::SetBinding {
                node_id,
                kind,
                value,
                ..
            } => {
                let node = self.nodes.get_mut(&node_id).expect("node validated");
                set_lww(&mut node.bindings, kind, timestamp, value);
            }
            TreeOperation::SetTag {
                node_id,
                tag,
                present,
                ..
            } => {
                let node = self.nodes.get_mut(&node_id).expect("node validated");
                set_lww(&mut node.tags, tag, timestamp, present);
            }
        }
        self.seen.insert(timestamp);
        Ok(true)
    }

    pub fn apply_text(
        &mut self,
        node_id: NodeId,
        operation: TextOperation,
        transaction: Option<TextTransaction>,
    ) -> Result<bool, String> {
        let node = self
            .nodes
            .get(&node_id)
            .ok_or("text operation references an unknown Desk node")?;
        if node.deleted_at.is_some() {
            return Ok(false);
        }
        let text = self
            .texts
            .get_mut(&node_id)
            .ok_or("Desk node has no text state")?;
        if text
            .operations
            .iter()
            .any(|old| old.timestamp() == operation.timestamp())
        {
            return Ok(false);
        }
        if let Some(transaction) = &transaction {
            if transaction.id.replica_id != operation.timestamp().replica_id
                || !transaction.edit_ids.contains(&operation.timestamp())
                || transaction.edit_ids.len() > 1024
            {
                return Err("invalid Desk node text transaction".into());
            }
        }
        text.operations.push(operation);
        if let Some(transaction) = transaction {
            text.transactions.push(transaction);
        }
        Ok(true)
    }

    pub fn text(
        &self,
        node_id: NodeId,
        replica_id: u16,
        buffer_id: BufferId,
    ) -> Result<String, String> {
        Ok(self
            .texts
            .get(&node_id)
            .ok_or("unknown Desk node")?
            .buffer(replica_id, buffer_id)?
            .text())
    }

    pub fn text_version(&self, node_id: NodeId) -> Result<Vec<TreeClock>, String> {
        let buffer_id = BufferId::new(1).unwrap();
        let buffer = self
            .texts
            .get(&node_id)
            .ok_or("unknown Desk node")?
            .buffer(ReplicaId::REMOTE_SERVER.as_u16(), buffer_id)?;
        Ok(buffer
            .version()
            .iter()
            .filter(|clock| clock.value != 0)
            .map(Into::into)
            .collect())
    }

    /// Returns live nodes in depth-first display order. Placement winners are
    /// chosen by LWW; if winners form a cycle, the lowest-priority edge in that
    /// cycle falls back to the node's next valid candidate.
    pub fn materialize(&self) -> Vec<MaterializedNode> {
        let live: BTreeSet<_> = self
            .nodes
            .values()
            .filter(|node| node.deleted_at.is_none())
            .map(|node| node.id)
            .collect();
        let mut candidates: BTreeMap<NodeId, Vec<&Placement>> = BTreeMap::new();
        for node in self.nodes.values().filter(|node| live.contains(&node.id)) {
            let mut placements = node
                .placements
                .iter()
                .filter(|placement| placement.parent.is_none_or(|parent| live.contains(&parent)))
                .collect::<Vec<_>>();
            placements.sort_by_key(|placement| std::cmp::Reverse(placement.timestamp));
            candidates.insert(node.id, placements);
        }
        let mut indexes: BTreeMap<NodeId, usize> = candidates.keys().map(|id| (*id, 0)).collect();
        loop {
            let parents: BTreeMap<_, _> = candidates
                .iter()
                .map(|(id, placements)| {
                    (
                        *id,
                        placements
                            .get(indexes[id])
                            .and_then(|placement| placement.parent),
                    )
                })
                .collect();
            let Some(cycle) = find_cycle(&parents) else {
                break;
            };
            let loser = cycle
                .into_iter()
                .min_by_key(|id| {
                    candidates[id]
                        .get(indexes[id])
                        .map(|placement| placement.timestamp)
                        .unwrap_or_default()
                })
                .unwrap();
            *indexes.get_mut(&loser).unwrap() += 1;
        }
        let mut materialized = BTreeMap::new();
        for (id, node) in &self.nodes {
            if !live.contains(id) {
                continue;
            }
            let placement = candidates[id].get(indexes[id]).copied();
            materialized.insert(
                *id,
                MaterializedNode {
                    id: *id,
                    kind: node.kind,
                    owner: node.owner,
                    parent: placement.and_then(|placement| placement.parent),
                    order: placement
                        .map(|placement| placement.order.clone())
                        .unwrap_or_default(),
                    temporal: node
                        .temporal
                        .iter()
                        .filter_map(|(kind, _, value)| value.map(|mark| (*kind, mark)))
                        .collect(),
                    bindings: node
                        .bindings
                        .iter()
                        .filter_map(|(kind, _, value)| {
                            value.clone().map(|binding| (*kind, binding))
                        })
                        .collect(),
                    tags: node
                        .tags
                        .iter()
                        .filter_map(|(tag, _, value)| value.then(|| tag.clone()))
                        .collect(),
                },
            );
        }
        let mut children: BTreeMap<Option<NodeId>, Vec<NodeId>> = BTreeMap::new();
        for node in materialized.values() {
            children.entry(node.parent).or_default().push(node.id);
        }
        for siblings in children.values_mut() {
            siblings.sort_by(|left, right| {
                let left_node = &materialized[left];
                let right_node = &materialized[right];
                let left_clock = candidates[left]
                    .get(indexes[left])
                    .map(|p| p.timestamp)
                    .unwrap_or_default();
                let right_clock = candidates[right]
                    .get(indexes[right])
                    .map(|p| p.timestamp)
                    .unwrap_or_default();
                (&left_node.order, left_clock, left).cmp(&(&right_node.order, right_clock, right))
            });
        }
        fn visit(
            parent: Option<NodeId>,
            children: &BTreeMap<Option<NodeId>, Vec<NodeId>>,
            nodes: &BTreeMap<NodeId, MaterializedNode>,
            out: &mut Vec<MaterializedNode>,
        ) {
            for id in children.get(&parent).into_iter().flatten() {
                out.push(nodes[id].clone());
                visit(Some(*id), children, nodes, out);
            }
        }
        let mut out = Vec::with_capacity(materialized.len());
        visit(None, &children, &materialized, &mut out);
        out
    }
}

fn validate_order(order: &OrderKey) -> Result<(), String> {
    if order.0.is_empty() || order.0.last() == Some(&0) || order.0.len() > 64 {
        Err("invalid Desk fractional order key".into())
    } else {
        Ok(())
    }
}

fn set_lww<K: Eq, V>(entries: &mut Vec<(K, TreeClock, V)>, key: K, timestamp: TreeClock, value: V) {
    if let Some((_, old_timestamp, old_value)) = entries
        .iter_mut()
        .find(|(candidate, _, _)| *candidate == key)
    {
        if *old_timestamp < timestamp {
            *old_timestamp = timestamp;
            *old_value = value;
        }
    } else {
        entries.push((key, timestamp, value));
    }
}

fn find_cycle(parents: &BTreeMap<NodeId, Option<NodeId>>) -> Option<Vec<NodeId>> {
    for start in parents.keys() {
        let mut positions = HashMap::new();
        let mut path = Vec::new();
        let mut current = Some(*start);
        while let Some(id) = current {
            if let Some(&position) = positions.get(&id) {
                return Some(path[position..].to_vec());
            }
            positions.insert(id, path.len());
            path.push(id);
            current = parents.get(&id).copied().flatten();
        }
    }
    None
}

pub fn todo_priority(at: chrono::NaiveDateTime, pace_days: u32, now: chrono::NaiveDateTime) -> f64 {
    temporal_priority(
        TemporalKind::Todo,
        TemporalMark {
            year: at.date().year(),
            month: at.date().month() as u8,
            day: at.date().day() as u8,
            minute_of_day: Some((at.time().num_seconds_from_midnight() / 60) as u16),
            pace_days,
        },
        now,
    )
}

/// Reads and replays the complete native-tree V1 epoch. This is the only
/// runtime entry point retained for the cells-V2 migration.
pub fn load_replayed(write: &mut WriteTxn) -> Result<Option<Snapshot>, String> {
    let Some(state) = write
        .open_table(STATE)
        .get(&())
        .map(|value| value.value().into_owned())
    else {
        return Ok(None);
    };
    let mut document = Document::from_snapshot(state.snapshot)?;
    enum Stored {
        Tree(TreeOpRecord),
        Text(TextOpRecord),
        Batch(BatchOpRecord),
    }
    let mut records = write
        .open_table(TREE_OPS)
        .iter()
        .map(|(sequence, record)| (sequence.value(), Stored::Tree(record.value().into_owned())))
        .collect::<Vec<_>>();
    records.extend(
        write.open_table(TEXT_OPS).iter().map(|(sequence, record)| {
            (sequence.value(), Stored::Text(record.value().into_owned()))
        }),
    );
    records.extend(
        write
            .open_table(BATCH_OPS)
            .iter()
            .map(|(sequence, record)| {
                (sequence.value(), Stored::Batch(record.value().into_owned()))
            }),
    );
    records.sort_by_key(|(sequence, _)| *sequence);
    for (_, record) in records {
        match record {
            Stored::Tree(record) => {
                document.apply(record.operation)?;
            }
            Stored::Text(record) => {
                document.apply_text(record.node_id, record.operation, record.transaction)?;
            }
            Stored::Batch(record) => {
                for operation in record.batch.operations {
                    match operation {
                        BatchOperation::Tree(operation) => {
                            document.apply(operation)?;
                        }
                        BatchOperation::Text {
                            node_id,
                            operation,
                            transaction,
                        } => {
                            document.apply_text(node_id, operation, transaction)?;
                        }
                    }
                }
                for operation in record.daemon_tree_operations {
                    document.apply(operation)?;
                }
            }
        }
    }
    Ok(Some(document.snapshot()))
}

pub fn text_into_current(text: NodeTextSnapshot) -> Result<rho_desk::NodeTextSnapshot, String> {
    let mut bytes = BytesMut::new();
    text.encode(&mut bytes).map_err(|error| error.to_string())?;
    let mut bytes = bytes.as_ref();
    rho_desk::NodeTextSnapshot::decode(&mut bytes).map_err(|error| error.to_string())
}
