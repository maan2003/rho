//! Wire vocabulary for the daemon-owned Desk text buffer.

use std::sync::Arc;

use clock::{Global, Lamport, ReplicaId};
use senax_encoder::{Decode, Encode, Pack, Unpack};
use text::{Buffer, BufferId, EditOperation, FullOffset, Operation, UndoOperation};

use crate::AgentId;

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct DeskNodeId(pub u64);

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct DeskStructureOpId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode, Pack, Unpack)]
pub struct DeskOrderKey(pub Vec<u8>);

impl DeskOrderKey {
    pub fn first() -> Self {
        Self(vec![128])
    }

    pub fn between(lower: Option<&Self>, upper: Option<&Self>) -> Option<Self> {
        let lower = lower.map_or(&[][..], |key| key.0.as_slice());
        let upper = upper.map(|key| key.0.as_slice());
        let mut result = Vec::new();
        for index in 0..64 {
            let low = lower.get(index).copied().unwrap_or(0);
            let high = upper
                .and_then(|bound| bound.get(index).copied())
                .unwrap_or(255);
            if low.saturating_add(1) < high {
                result.push(low + (high - low) / 2);
                return Some(Self(result));
            }
            result.push(low);
        }
        None
    }

    pub fn is_valid(&self) -> bool {
        !self.0.is_empty() && self.0.len() <= 64 && self.0.last() != Some(&0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct DeskNode {
    pub id: DeskNodeId,
    pub parent: Option<DeskNodeId>,
    pub order: DeskOrderKey,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum DeskStructureOp {
    Insert {
        nodes: Vec<DeskNode>,
    },
    Remove {
        node_id: DeskNodeId,
    },
    Move {
        node_id: DeskNodeId,
        parent: Option<DeskNodeId>,
        order: DeskOrderKey,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum DeskStructureAuthor {
    User,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct DeskStructureOpRecord {
    pub id: DeskStructureOpId,
    pub author: DeskStructureAuthor,
    pub timestamp_ms: u64,
    pub op: DeskStructureOp,
    pub inverse: DeskStructureOp,
    pub undo_of: Option<DeskStructureOpId>,
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct DeskClock {
    pub replica_id: u16,
    pub value: u32,
}

impl From<Lamport> for DeskClock {
    fn from(value: Lamport) -> Self {
        Self {
            replica_id: value.replica_id.as_u16(),
            value: value.value,
        }
    }
}

impl From<DeskClock> for Lamport {
    fn from(value: DeskClock) -> Self {
        Self {
            replica_id: ReplicaId::new(value.replica_id),
            value: value.value,
        }
    }
}

/// Senax representation of Zed's native text-buffer operations.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum DeskOperation {
    Edit {
        timestamp: DeskClock,
        version: Vec<DeskClock>,
        ranges: Vec<(u64, u64)>,
        new_text: Vec<String>,
    },
    Undo {
        timestamp: DeskClock,
        version: Vec<DeskClock>,
        counts: Vec<(DeskClock, u32)>,
    },
}

impl DeskOperation {
    pub fn timestamp(&self) -> DeskClock {
        match self {
            Self::Edit { timestamp, .. } | Self::Undo { timestamp, .. } => *timestamp,
        }
    }

    pub fn replica_id(&self) -> u16 {
        self.timestamp().replica_id
    }

    pub fn from_text(operation: &Operation) -> Self {
        match operation {
            Operation::Edit(edit) => Self::Edit {
                timestamp: edit.timestamp.into(),
                version: version_to_wire(&edit.version),
                ranges: edit
                    .ranges
                    .iter()
                    .map(|range| (range.start.0 as u64, range.end.0 as u64))
                    .collect(),
                new_text: edit.new_text.iter().map(ToString::to_string).collect(),
            },
            Operation::Undo(undo) => Self::Undo {
                timestamp: undo.timestamp.into(),
                version: version_to_wire(&undo.version),
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
                    return Err("Desk edit range/text count mismatch".to_owned());
                }
                if ranges.len() > 65_536
                    || new_text.iter().map(String::len).sum::<usize>() > 4 * 1024 * 1024
                {
                    return Err("Desk edit is too large".to_owned());
                }
                let ranges = ranges
                    .iter()
                    .map(|(start, end)| {
                        let start =
                            usize::try_from(*start).map_err(|_| "Desk edit offset overflow")?;
                        let end = usize::try_from(*end).map_err(|_| "Desk edit offset overflow")?;
                        Ok(FullOffset(start)..FullOffset(end))
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                Operation::Edit(EditOperation {
                    timestamp: (*timestamp).into(),
                    version: version_from_wire(version),
                    ranges,
                    new_text: new_text
                        .iter()
                        .map(|text| Arc::from(text.as_str()))
                        .collect(),
                })
            }
            Self::Undo {
                timestamp,
                version,
                counts,
            } => {
                if counts.len() > 65_536 {
                    return Err("Desk undo is too large".to_owned());
                }
                Operation::Undo(UndoOperation {
                    timestamp: (*timestamp).into(),
                    version: version_from_wire(version),
                    counts: counts
                        .iter()
                        .map(|(clock, count)| ((*clock).into(), *count))
                        .collect(),
                })
            }
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct DeskTransaction {
    pub id: DeskClock,
    pub edit_ids: Vec<DeskClock>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum DeskReplicaAuthor {
    User,
    Agent(AgentId),
    Gatekeeper,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct DeskReplica {
    pub replica_id: u16,
    pub author: DeskReplicaAuthor,
}

/// Principal bindings remain daemon state outside the undoable buffer.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct DeskBinding {
    pub node_id: DeskNodeId,
    pub agent_id: AgentId,
    pub orphaned: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct DeskTextOpRecord {
    pub sequence: u64,
    pub node_id: DeskNodeId,
    pub timestamp_ms: u64,
    pub operation: DeskOperation,
    pub transaction: Option<DeskTransaction>,
    pub undo_of: Option<DeskClock>,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct DeskNodeText {
    pub node_id: DeskNodeId,
    pub operations: Vec<DeskOperation>,
    pub transactions: Vec<DeskTransaction>,
}

impl DeskNodeText {
    pub fn buffer(&self, replica_id: u16) -> Result<Buffer, String> {
        let mut buffer = Buffer::new(
            ReplicaId::new(replica_id),
            BufferId::new(self.node_id.0).map_err(|error| error.to_string())?,
            "",
        );
        let operations = self
            .operations
            .iter()
            .map(DeskOperation::to_text)
            .collect::<Result<Vec<_>, _>>()?;
        buffer.apply_ops(operations);
        if buffer.has_deferred_ops() {
            return Err("Desk node text has causally incomplete operation history".to_owned());
        }
        Ok(buffer)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct DeskSnapshot {
    pub nodes: Vec<DeskNode>,
    pub texts: Vec<DeskNodeText>,
    pub replicas: Vec<DeskReplica>,
    pub bindings: Vec<DeskBinding>,
    pub next_node_id: u64,
    pub last_structure_op_id: u64,
    pub undone_structure_ops: Vec<DeskStructureOpId>,
}

impl Default for DeskSnapshot {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            texts: Vec::new(),
            replicas: Vec::new(),
            bindings: Vec::new(),
            next_node_id: 1,
            last_structure_op_id: 0,
            undone_structure_ops: Vec::new(),
        }
    }
}

impl DeskSnapshot {
    pub fn allocate_node_id(&mut self) -> DeskNodeId {
        let id = DeskNodeId(self.next_node_id);
        self.next_node_id = self
            .next_node_id
            .checked_add(1)
            .expect("Desk node ids exhausted");
        id
    }

    pub fn node(&self, id: DeskNodeId) -> Option<&DeskNode> {
        self.nodes.iter().find(|node| node.id == id)
    }

    pub fn apply_structure(&mut self, op: &DeskStructureOp) -> Result<DeskStructureOp, String> {
        let inverse = match op {
            DeskStructureOp::Insert { nodes } => self.insert_nodes(nodes),
            DeskStructureOp::Remove { node_id } => self.remove_node(*node_id),
            DeskStructureOp::Move {
                node_id,
                parent,
                order,
            } => self.move_node(*node_id, *parent, order.clone()),
        }?;
        let visible = self
            .nodes
            .iter()
            .map(|node| node.id)
            .collect::<std::collections::BTreeSet<_>>();
        for binding in &mut self.bindings {
            binding.orphaned = !visible.contains(&binding.node_id);
        }
        Ok(inverse)
    }

    pub fn bind(&mut self, node_id: DeskNodeId, agent_id: AgentId) -> Result<DeskBinding, String> {
        if self.node(node_id).is_none() {
            return Err(format!("unknown Desk node {}", node_id.0));
        }
        self.bindings
            .retain(|binding| binding.node_id != node_id && binding.agent_id != agent_id);
        let binding = DeskBinding {
            node_id,
            agent_id,
            orphaned: false,
        };
        self.bindings.push(binding.clone());
        Ok(binding)
    }

    fn insert_nodes(&mut self, inserted: &[DeskNode]) -> Result<DeskStructureOp, String> {
        if inserted.is_empty() {
            return Err("Desk insert is empty".to_owned());
        }
        let existing: std::collections::BTreeSet<_> =
            self.nodes.iter().map(|node| node.id).collect();
        let mut available = existing.clone();
        let mut inserted_ids = std::collections::BTreeSet::new();
        let mut occupied: std::collections::BTreeSet<_> = self
            .nodes
            .iter()
            .map(|node| (node.parent, node.order.clone()))
            .collect();
        for node in inserted {
            if node.id.0 == 0
                || node.id.0 >= self.next_node_id
                || existing.contains(&node.id)
                || !inserted_ids.insert(node.id)
            {
                return Err(format!("invalid or duplicate Desk node id {}", node.id.0));
            }
            if !node.order.is_valid() || !occupied.insert((node.parent, node.order.clone())) {
                return Err("invalid or duplicate Desk order key".to_owned());
            }
            if node
                .parent
                .is_some_and(|parent| !available.contains(&parent))
            {
                return Err("unknown or out-of-order Desk parent".to_owned());
            }
            available.insert(node.id);
        }
        let root = inserted[0].id;
        self.nodes.extend_from_slice(inserted);
        self.sort_nodes();
        Ok(DeskStructureOp::Remove { node_id: root })
    }

    fn remove_node(&mut self, id: DeskNodeId) -> Result<DeskStructureOp, String> {
        if self.node(id).is_none() {
            return Err(format!("unknown Desk node {}", id.0));
        }
        let mut removed_ids = std::collections::BTreeSet::from([id]);
        loop {
            let before = removed_ids.len();
            for node in &self.nodes {
                if node
                    .parent
                    .is_some_and(|parent| removed_ids.contains(&parent))
                {
                    removed_ids.insert(node.id);
                }
            }
            if before == removed_ids.len() {
                break;
            }
        }
        let mut removed = Vec::new();
        self.nodes.retain(|node| {
            if removed_ids.contains(&node.id) {
                removed.push(node.clone());
                false
            } else {
                true
            }
        });
        Ok(DeskStructureOp::Insert { nodes: removed })
    }

    fn move_node(
        &mut self,
        id: DeskNodeId,
        parent: Option<DeskNodeId>,
        order: DeskOrderKey,
    ) -> Result<DeskStructureOp, String> {
        if !order.is_valid()
            || parent == Some(id)
            || parent.is_some_and(|candidate| self.is_descendant(candidate, id))
        {
            return Err("invalid Desk move".to_owned());
        }
        if parent.is_some_and(|parent| self.node(parent).is_none()) {
            return Err("unknown Desk parent".to_owned());
        }
        if self
            .nodes
            .iter()
            .any(|node| node.id != id && node.parent == parent && node.order == order)
        {
            return Err("duplicate Desk order key".to_owned());
        }
        let node = self
            .nodes
            .iter_mut()
            .find(|node| node.id == id)
            .ok_or_else(|| format!("unknown Desk node {}", id.0))?;
        let inverse = DeskStructureOp::Move {
            node_id: id,
            parent: node.parent,
            order: node.order.clone(),
        };
        node.parent = parent;
        node.order = order;
        self.sort_nodes();
        Ok(inverse)
    }

    fn is_descendant(&self, candidate: DeskNodeId, ancestor: DeskNodeId) -> bool {
        let mut current = Some(candidate);
        while let Some(id) = current {
            if id == ancestor {
                return true;
            }
            current = self.node(id).and_then(|node| node.parent);
        }
        false
    }

    fn sort_nodes(&mut self) {
        self.nodes
            .sort_by(|a, b| (a.parent, &a.order, a.id).cmp(&(b.parent, &b.order, b.id)));
    }
}

fn version_to_wire(version: &Global) -> Vec<DeskClock> {
    version
        .iter()
        .filter(|clock| clock.value != 0)
        .map(Into::into)
        .collect()
}

fn version_from_wire(version: &[DeskClock]) -> Global {
    version.iter().copied().map(Into::into).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operations_round_trip_and_use_native_undo() {
        let mut buffer = Buffer::new(ReplicaId::new(8), BufferId::new(1).unwrap(), "");
        let edit = buffer.edit([(0..0, "* TODO plan\nnotes\n")]);
        let edit_id = edit.timestamp();
        let undo = buffer.undo_edit_ids([edit_id]);
        let node_text = DeskNodeText {
            node_id: DeskNodeId(1),
            operations: vec![
                DeskOperation::from_text(&edit),
                DeskOperation::from_text(&undo),
            ],
            transactions: vec![DeskTransaction {
                id: edit_id.into(),
                edit_ids: vec![edit_id.into()],
            }],
        };
        assert_eq!(node_text.buffer(9).unwrap().text(), "");
    }

    #[test]
    fn structural_inverse_restores_tree_without_reusing_id() {
        let mut snapshot = DeskSnapshot::default();
        let id = snapshot.allocate_node_id();
        let insert = DeskStructureOp::Insert {
            nodes: vec![DeskNode {
                id,
                parent: None,
                order: DeskOrderKey::first(),
            }],
        };
        let inverse = snapshot.apply_structure(&insert).unwrap();
        snapshot.apply_structure(&inverse).unwrap();
        assert!(snapshot.nodes.is_empty());
        assert!(snapshot.allocate_node_id().0 > id.0);
    }
}
