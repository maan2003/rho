use std::collections::{BTreeMap, HashSet};

use gpui::{AppContext as _, Context, Entity};
use language::{Buffer, BufferEvent, Capability};
use rho_ui_proto::ClientMessage;
use rho_ui_proto::desk::{
    DeskAnchor, DeskClock, DeskOperation, DeskSnapshot, DeskTextOpRecord, DeskTransaction,
};
use text::{BufferId, ReplicaId};

use crate::registry::HostId;
use crate::workspace::Workspace;

struct HostDesk {
    snapshot: DeskSnapshot,
    buffer: Entity<Buffer>,
    _subscription: gpui::Subscription,
}

struct HostTreeDesk {
    document: rho_desk::Document,
    buffers: BTreeMap<rho_desk::NodeId, Entity<Buffer>>,
    _subscriptions: Vec<gpui::Subscription>,
    sequence: u64,
    replica_id: u16,
}

enum PendingTreeEvent {
    Tree(rho_desk::TreeOpRecord),
    Text(rho_desk::TextOpRecord),
}

impl PendingTreeEvent {
    fn sequence(&self) -> u64 {
        match self {
            Self::Tree(record) => record.sequence,
            Self::Text(record) => record.sequence,
        }
    }
}

fn sequence_has_gap(current: u64, incoming: u64) -> bool {
    incoming > current.saturating_add(1)
}

/// Workspace-owned source of truth for every attached host's Desk document.
pub struct DeskSync {
    hosts: BTreeMap<HostId, HostDesk>,
    known_ops: HashSet<(HostId, DeskClock)>,
    next_buffer_id: u64,
    tree_hosts: BTreeMap<HostId, HostTreeDesk>,
    pending_tree: BTreeMap<HostId, BTreeMap<u64, PendingTreeEvent>>,
    pending_replacements: BTreeMap<HostId, rho_desk::Snapshot>,
}

impl Default for DeskSync {
    fn default() -> Self {
        Self {
            hosts: BTreeMap::new(),
            known_ops: HashSet::new(),
            next_buffer_id: 1,
            tree_hosts: BTreeMap::new(),
            pending_tree: BTreeMap::new(),
            pending_replacements: BTreeMap::new(),
        }
    }
}

impl DeskSync {
    pub fn apply_tree_snapshot(
        &mut self,
        host: HostId,
        mut snapshot: rho_desk::Snapshot,
        replica_id: u16,
        cx: &mut Context<Workspace>,
    ) -> bool {
        if let Some(replacement) = self.pending_replacements.remove(&host)
            && replacement.sequence > snapshot.sequence
        {
            snapshot = replacement;
        }
        let sequence = snapshot.sequence;
        let Ok(document) = rho_desk::Document::from_snapshot(snapshot.clone()) else {
            return true;
        };
        let owners = document
            .materialize()
            .into_iter()
            .map(|node| (node.id, node.owner))
            .collect::<BTreeMap<_, _>>();
        let mut buffers = BTreeMap::new();
        let mut subscriptions = Vec::new();
        for text in snapshot.texts {
            let buffer_id = BufferId::new(self.next_buffer_id).expect("nonzero GUI buffer id");
            self.next_buffer_id += 1;
            let capability = if owners.get(&text.node_id) == Some(&rho_desk::NodeOwner::User) {
                Capability::ReadWrite
            } else {
                Capability::ReadOnly
            };
            let node_id = text.node_id;
            let (buffer, subscription) = make_tree_buffer(
                buffer_id,
                replica_id,
                capability,
                text.operations,
                host,
                node_id,
                cx,
            );
            subscriptions.push(subscription);
            buffers.insert(node_id, buffer);
        }
        self.tree_hosts.insert(
            host,
            HostTreeDesk {
                document,
                buffers,
                _subscriptions: subscriptions,
                sequence,
                replica_id,
            },
        );
        let pending = self.pending_tree.remove(&host).unwrap_or_default();
        for (_, event) in pending {
            if event.sequence() <= sequence {
                continue;
            }
            let gap = match event {
                PendingTreeEvent::Tree(record) => self.apply_tree(host, record, cx),
                PendingTreeEvent::Text(record) => self.apply_node_text(host, record, cx),
            };
            if gap {
                return true;
            }
        }
        false
    }

    pub fn replace_tree_snapshot(
        &mut self,
        host: HostId,
        snapshot: rho_desk::Snapshot,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let Some(replica_id) = self.tree_hosts.get(&host).map(|desk| desk.replica_id) else {
            self.pending_replacements.insert(host, snapshot);
            return false;
        };
        if self
            .tree_hosts
            .get(&host)
            .is_some_and(|desk| snapshot.sequence <= desk.sequence)
        {
            return false;
        }
        self.apply_tree_snapshot(host, snapshot, replica_id, cx)
    }

    /// Returns true when a sequence gap requires a fresh snapshot.
    pub fn apply_tree(
        &mut self,
        host: HostId,
        record: rho_desk::TreeOpRecord,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let created = match &record.operation {
            rho_desk::TreeOperation::Create { node_id, owner, .. } => Some((*node_id, *owner)),
            _ => None,
        };
        let new_buffer_id = created.map(|_| {
            let id = BufferId::new(self.next_buffer_id).expect("nonzero GUI buffer id");
            self.next_buffer_id += 1;
            id
        });
        let Some(desk) = self.tree_hosts.get_mut(&host) else {
            self.pending_tree
                .entry(host)
                .or_default()
                .insert(record.sequence, PendingTreeEvent::Tree(record));
            return false;
        };
        if record.sequence <= desk.sequence {
            return false;
        }
        if sequence_has_gap(desk.sequence, record.sequence) {
            return true;
        }
        if desk.document.apply(record.operation).is_err() {
            return true;
        }
        if let (Some((node_id, owner)), Some(buffer_id)) = (created, new_buffer_id) {
            let capability = if owner == rho_desk::NodeOwner::User {
                Capability::ReadWrite
            } else {
                Capability::ReadOnly
            };
            let (buffer, subscription) = make_tree_buffer(
                buffer_id,
                desk.replica_id,
                capability,
                Vec::new(),
                host,
                node_id,
                cx,
            );
            desk.buffers.insert(node_id, buffer);
            desk._subscriptions.push(subscription);
        }
        desk.sequence = record.sequence;
        false
    }

    pub fn apply_node_text(
        &mut self,
        host: HostId,
        record: rho_desk::TextOpRecord,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let Some(desk) = self.tree_hosts.get_mut(&host) else {
            self.pending_tree
                .entry(host)
                .or_default()
                .insert(record.sequence, PendingTreeEvent::Text(record));
            return false;
        };
        if record.sequence <= desk.sequence {
            return false;
        }
        if sequence_has_gap(desk.sequence, record.sequence) {
            return true;
        }
        if !desk
            .document
            .apply_text(record.node_id, record.operation.clone(), record.transaction)
            .unwrap_or(false)
        {
            return true;
        }
        if let (Some(buffer), Ok(operation)) = (
            desk.buffers.get(&record.node_id),
            record.operation.to_text(),
        ) {
            buffer.update(cx, |buffer, cx| {
                buffer.apply_ops([language::Operation::Buffer(operation)], cx)
            });
        }
        desk.sequence = record.sequence;
        false
    }

    pub fn apply_snapshot(
        &mut self,
        host: HostId,
        snapshot: DeskSnapshot,
        replica_id: u16,
        cx: &mut Context<Workspace>,
    ) -> Entity<Buffer> {
        self.known_ops.retain(|(owner, _)| *owner != host);
        self.known_ops
            .extend(snapshot.operations.iter().map(|op| (host, op.timestamp())));
        let operations = snapshot.operations.clone();
        let buffer_id = BufferId::new(self.next_buffer_id).expect("nonzero GUI buffer id");
        self.next_buffer_id += 1;
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::remote(
                buffer_id,
                ReplicaId::new(replica_id),
                Capability::ReadWrite,
                "",
            );
            buffer.apply_ops(
                operations
                    .iter()
                    .filter_map(|operation| operation.to_text().ok())
                    .map(language::Operation::Buffer)
                    .collect::<Vec<_>>(),
                cx,
            );
            buffer
        });
        let subscription = cx.subscribe(&buffer, move |workspace, _, event, _| {
            let BufferEvent::Operation {
                operation: language::Operation::Buffer(operation),
                is_local: true,
            } = event
            else {
                return;
            };
            let operation = DeskOperation::from_text(operation);
            let timestamp = operation.timestamp();
            workspace.mark_desk_text_local(host, timestamp);
            workspace.send_to_host(
                host,
                ClientMessage::DeskTextApply {
                    operation,
                    transaction: Some(DeskTransaction {
                        id: timestamp,
                        edit_ids: vec![timestamp],
                    }),
                },
            );
        });
        self.hosts.insert(
            host,
            HostDesk {
                snapshot,
                buffer: buffer.clone(),
                _subscription: subscription,
            },
        );
        buffer
    }

    pub fn apply_text(
        &mut self,
        host: HostId,
        record: DeskTextOpRecord,
        cx: &mut Context<Workspace>,
    ) {
        if !self.known_ops.insert((host, record.operation.timestamp())) {
            return;
        }
        let Some(desk) = self.hosts.get_mut(&host) else {
            return;
        };
        desk.snapshot.operations.push(record.operation.clone());
        if let Some(transaction) = record.transaction {
            desk.snapshot.transactions.push(transaction);
        }
        if let Ok(operation) = record.operation.to_text() {
            desk.buffer.update(cx, |buffer, cx| {
                buffer.apply_ops([language::Operation::Buffer(operation)], cx)
            });
        }
    }

    pub fn mark_local(&mut self, host: HostId, clock: DeskClock) {
        self.known_ops.insert((host, clock));
    }

    pub fn buffer(&self, host: HostId) -> Option<Entity<Buffer>> {
        self.hosts.get(&host).map(|desk| desk.buffer.clone())
    }

    /// The anchor a staffing request should carry for the heading at this
    /// offset, computed against our replica of the document.
    pub fn anchor_at(&self, host: HostId, offset: usize, cx: &gpui::App) -> Option<DeskAnchor> {
        let buffer = self.hosts.get(&host)?.buffer.read(cx);
        Some(DeskAnchor::from_text(buffer.anchor_after(offset)))
    }
}

fn make_tree_buffer(
    buffer_id: BufferId,
    replica_id: u16,
    capability: Capability,
    operations: Vec<rho_desk::TextOperation>,
    host: HostId,
    node_id: rho_desk::NodeId,
    cx: &mut Context<Workspace>,
) -> (Entity<Buffer>, gpui::Subscription) {
    let buffer = cx.new(|cx| {
        let mut buffer = Buffer::remote(buffer_id, ReplicaId::new(replica_id), capability, "");
        buffer.apply_ops(
            operations
                .iter()
                .filter_map(|operation| operation.to_text().ok())
                .map(language::Operation::Buffer)
                .collect::<Vec<_>>(),
            cx,
        );
        buffer
    });
    let subscription = cx.subscribe(&buffer, move |workspace, _, event, _| {
        let BufferEvent::Operation {
            operation: language::Operation::Buffer(operation),
            is_local: true,
        } = event
        else {
            return;
        };
        let operation = rho_desk::TextOperation::from_text(operation);
        let timestamp = operation.timestamp();
        workspace.send_to_host(
            host,
            ClientMessage::DeskNodeTextApply {
                node_id,
                operation,
                transaction: Some(rho_desk::TextTransaction {
                    id: timestamp,
                    edit_ids: vec![timestamp],
                }),
            },
        );
    });
    (buffer, subscription)
}

#[cfg(test)]
mod tests {
    use super::sequence_has_gap;

    #[test]
    fn tree_sequence_accepts_duplicates_and_next_but_detects_loss() {
        assert!(!sequence_has_gap(7, 7));
        assert!(!sequence_has_gap(7, 8));
        assert!(sequence_has_gap(7, 9));
        assert!(!sequence_has_gap(u64::MAX, u64::MAX));
    }
}
