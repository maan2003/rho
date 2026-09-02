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
}

/// Workspace-owned source of truth for every attached host's Desk document.
pub struct DeskSync {
    hosts: BTreeMap<HostId, HostDesk>,
    known_ops: HashSet<(HostId, DeskClock)>,
    next_buffer_id: u64,
    tree_hosts: BTreeMap<HostId, HostTreeDesk>,
}

impl Default for DeskSync {
    fn default() -> Self {
        Self {
            hosts: BTreeMap::new(),
            known_ops: HashSet::new(),
            next_buffer_id: 1,
            tree_hosts: BTreeMap::new(),
        }
    }
}

impl DeskSync {
    pub fn apply_tree_snapshot(
        &mut self,
        host: HostId,
        snapshot: rho_desk::Snapshot,
        replica_id: u16,
        cx: &mut Context<Workspace>,
    ) {
        let Ok(document) = rho_desk::Document::from_snapshot(snapshot.clone()) else {
            return;
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
            let operations = text.operations.clone();
            let capability = if owners.get(&text.node_id) == Some(&rho_desk::NodeOwner::User) {
                Capability::ReadWrite
            } else {
                Capability::ReadOnly
            };
            let buffer = cx.new(|cx| {
                let mut buffer =
                    Buffer::remote(buffer_id, ReplicaId::new(replica_id), capability, "");
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
            let node_id = text.node_id;
            subscriptions.push(cx.subscribe(&buffer, move |workspace, _, event, _| {
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
            }));
            buffers.insert(node_id, buffer);
        }
        self.tree_hosts.insert(
            host,
            HostTreeDesk {
                document,
                buffers,
                _subscriptions: subscriptions,
            },
        );
    }

    pub fn apply_tree(&mut self, host: HostId, record: rho_desk::TreeOpRecord) {
        if let Some(desk) = self.tree_hosts.get_mut(&host) {
            let _ = desk.document.apply(record.operation);
        }
    }

    pub fn apply_node_text(
        &mut self,
        host: HostId,
        record: rho_desk::TextOpRecord,
        cx: &mut Context<Workspace>,
    ) {
        let Some(desk) = self.tree_hosts.get_mut(&host) else {
            return;
        };
        if !desk
            .document
            .apply_text(record.node_id, record.operation.clone(), record.transaction)
            .unwrap_or(false)
        {
            return;
        }
        if let (Some(buffer), Ok(operation)) = (
            desk.buffers.get(&record.node_id),
            record.operation.to_text(),
        ) {
            buffer.update(cx, |buffer, cx| {
                buffer.apply_ops([language::Operation::Buffer(operation)], cx)
            });
        }
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
