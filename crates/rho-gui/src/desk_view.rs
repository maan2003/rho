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

/// Workspace-owned source of truth for every attached host's Desk document.
pub struct DeskSync {
    hosts: BTreeMap<HostId, HostDesk>,
    known_ops: HashSet<(HostId, DeskClock)>,
    next_buffer_id: u64,
}

impl Default for DeskSync {
    fn default() -> Self {
        Self {
            hosts: BTreeMap::new(),
            known_ops: HashSet::new(),
            next_buffer_id: 1,
        }
    }
}

impl DeskSync {
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
