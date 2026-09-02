use std::collections::{BTreeMap, BTreeSet};

use gpui::{AppContext as _, Context, Entity};
use language::{Buffer, BufferEvent, Capability};
use rho_ui_proto::ClientMessage;
use text::{BufferId, ReplicaId, ToOffset as _};

use crate::registry::HostId;
use crate::workspace::Workspace;

struct HostTreeDesk {
    document: rho_desk::Document,
    buffers: BTreeMap<rho_desk::NodeId, Entity<Buffer>>,
    _subscriptions: Vec<gpui::Subscription>,
    sequence: u64,
    replica_id: u16,
    next_tree_clock: u32,
    next_node_counter: u64,
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

fn snapshot_is_stale(current: u64, incoming: u64) -> bool {
    incoming < current
}

/// Workspace-owned source of truth for every attached host's Desk document.
pub struct DeskTreeSync {
    next_buffer_id: u64,
    tree_hosts: BTreeMap<HostId, HostTreeDesk>,
    pending_tree: BTreeMap<HostId, BTreeMap<u64, PendingTreeEvent>>,
    pending_replacements: BTreeMap<HostId, rho_desk::Snapshot>,
    pending_batches: BTreeMap<(HostId, rho_desk::TreeClock), rho_desk::OperationBatch>,
    pending_batch_text: BTreeMap<(HostId, rho_desk::TreeClock), Vec<rho_desk::BatchOperation>>,
}

#[derive(Clone)]
pub struct DeskSubtree {
    nodes: Vec<(rho_desk::MaterializedNode, String)>,
    relocated_machine_rows: Vec<RelocatedMachineRow>,
    relocation_destination: String,
}

#[derive(Clone)]
struct RelocatedMachineRow {
    node_id: rho_desk::NodeId,
    destination: Option<rho_desk::NodeId>,
    source_batch: Option<rho_desk::TreeClock>,
}

impl DeskSubtree {
    pub fn relocation_notice(&self) -> Option<String> {
        (!self.relocated_machine_rows.is_empty()).then(|| {
            format!(
                "moved {} agent rows to {}",
                self.relocated_machine_rows.len(),
                self.relocation_destination
            )
        })
    }
}

#[derive(Clone)]
pub struct DeskDeleteEmptyUndo {
    heading: rho_desk::MaterializedNode,
    deleted_right: Option<(rho_desk::MaterializedNode, String)>,
    previous_text: Option<(rho_desk::NodeId, String)>,
    moved_child: Option<rho_desk::NodeId>,
}

pub enum PreparedOpenProse {
    Existing {
        node_id: rho_desk::NodeId,
        offset: usize,
        open_above: bool,
    },
    Created {
        batch: rho_desk::OperationBatch,
        messages: Vec<ClientMessage>,
        node_id: rho_desk::NodeId,
    },
}

impl Default for DeskTreeSync {
    fn default() -> Self {
        Self {
            next_buffer_id: 1,
            tree_hosts: BTreeMap::new(),
            pending_tree: BTreeMap::new(),
            pending_replacements: BTreeMap::new(),
            pending_batches: BTreeMap::new(),
            pending_batch_text: BTreeMap::new(),
        }
    }
}

impl DeskTreeSync {
    #[cfg(test)]
    pub fn snapshot_for_test(&self, host: HostId) -> Option<rho_desk::Snapshot> {
        Some(self.tree_hosts.get(&host)?.document.snapshot())
    }

    pub fn prepare_merge_split(
        &mut self,
        host: HostId,
        heading: rho_desk::NodeId,
        prose: rho_desk::NodeId,
        cx: &mut Context<Workspace>,
    ) -> Option<(Vec<ClientMessage>, Vec<rho_desk::NodeId>)> {
        let desk = self.tree_hosts.get_mut(&host)?;
        let nodes = desk.document.materialize();
        nodes
            .iter()
            .find(|node| node.id == heading && node.kind == rho_desk::NodeKind::Heading)?;
        nodes.iter().find(|node| {
            node.id == prose
                && node.kind == rho_desk::NodeKind::Prose
                && node.parent == Some(heading)
        })?;
        let heading_buffer = desk.buffers.get(&heading)?.clone();
        let merged = format!(
            "{}{}",
            heading_buffer.read(cx).text(),
            desk.buffers.get(&prose)?.read(cx).text()
        );
        heading_buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, merged)], None, cx);
        });
        desk.next_tree_clock = desk.next_tree_clock.checked_add(1)?;
        let messages = vec![ClientMessage::DeskTreeApply {
            operation: rho_desk::TreeOperation::Delete {
                timestamp: rho_desk::TreeClock {
                    value: desk.next_tree_clock,
                    replica_id: desk.replica_id,
                },
                node_ids: vec![prose],
            },
        }];
        Some((messages, vec![heading, prose]))
    }

    pub fn prepare_move_to(
        &mut self,
        host: HostId,
        node_id: rho_desk::NodeId,
        parent: Option<rho_desk::NodeId>,
        order: rho_desk::OrderKey,
    ) -> Option<(rho_desk::OperationBatch, Vec<ClientMessage>)> {
        let desk = self.tree_hosts.get_mut(&host)?;
        desk.document
            .materialize()
            .iter()
            .find(|node| node.id == node_id)?;
        desk.next_tree_clock = desk.next_tree_clock.checked_add(1)?;
        let messages = vec![ClientMessage::DeskTreeApply {
            operation: rho_desk::TreeOperation::Move {
                timestamp: rho_desk::TreeClock {
                    value: desk.next_tree_clock,
                    replica_id: desk.replica_id,
                },
                node_id,
                parent,
                order,
            },
        }];
        let mut expected = vec![node_id];
        expected.extend(parent);
        let batch = self.operation_batch(host, expected, messages.clone(), None)?;
        Some((batch, messages))
    }

    pub fn prepare_open_prose(
        &mut self,
        host: HostId,
        relative: rho_desk::NodeId,
        above: bool,
        cx: &gpui::App,
    ) -> Option<PreparedOpenProse> {
        let desk = self.tree_hosts.get_mut(&host)?;
        let nodes = desk.document.materialize();
        let node = nodes.iter().find(|node| node.id == relative)?;
        if node.kind != rho_desk::NodeKind::Heading || node.owner != rho_desk::NodeOwner::User {
            return None;
        }
        let (parent, order, adjacent_id) = if above {
            let previous = nodes
                .iter()
                .filter(|candidate| candidate.parent == node.parent && candidate.order < node.order)
                .max_by_key(|candidate| &candidate.order);
            if let Some(previous) = previous.filter(|node| node.kind == rho_desk::NodeKind::Prose) {
                return Some(PreparedOpenProse::Existing {
                    node_id: previous.id,
                    offset: desk.buffers.get(&previous.id)?.read(cx).len(),
                    open_above: false,
                });
            }
            (
                node.parent,
                rho_desk::OrderKey::between(previous.map(|node| &node.order), Some(&node.order)),
                previous.map(|node| node.id),
            )
        } else {
            let first = nodes
                .iter()
                .filter(|candidate| candidate.parent == Some(relative))
                .min_by_key(|candidate| &candidate.order);
            if let Some(first) = first.filter(|node| node.kind == rho_desk::NodeKind::Prose) {
                return Some(PreparedOpenProse::Existing {
                    node_id: first.id,
                    offset: 0,
                    open_above: true,
                });
            }
            (
                Some(relative),
                rho_desk::OrderKey::between(None, first.map(|node| &node.order)),
                first.map(|node| node.id),
            )
        };
        desk.next_node_counter = desk.next_node_counter.checked_add(1)?;
        desk.next_tree_clock = desk.next_tree_clock.checked_add(1)?;
        let node_id = rho_desk::NodeId {
            replica_id: desk.replica_id,
            counter: desk.next_node_counter,
        };
        let messages = vec![ClientMessage::DeskTreeApply {
            operation: rho_desk::TreeOperation::Create {
                timestamp: rho_desk::TreeClock {
                    value: desk.next_tree_clock,
                    replica_id: desk.replica_id,
                },
                node_id,
                kind: rho_desk::NodeKind::Prose,
                owner: rho_desk::NodeOwner::User,
                parent,
                order,
            },
        }];
        let mut expected = vec![relative];
        expected.extend(adjacent_id);
        let batch = self.operation_batch(host, expected, messages.clone(), None)?;
        Some(PreparedOpenProse::Created {
            batch,
            messages,
            node_id,
        })
    }
    pub fn capture_subtree(
        &self,
        host: HostId,
        root: rho_desk::NodeId,
        cx: &gpui::App,
    ) -> Option<DeskSubtree> {
        let desk = self.tree_hosts.get(&host)?;
        let nodes = desk.document.materialize();
        let mut included = BTreeSet::from([root]);
        let mut captured = Vec::new();
        for node in nodes {
            if node.id != root && !node.parent.is_some_and(|parent| included.contains(&parent)) {
                continue;
            }
            included.insert(node.id);
            if node.owner == rho_desk::NodeOwner::User {
                captured.push((node.clone(), desk.buffers.get(&node.id)?.read(cx).text()));
            }
        }
        (!captured.is_empty()).then_some(DeskSubtree {
            nodes: captured,
            relocated_machine_rows: Vec::new(),
            relocation_destination: String::new(),
        })
    }

    pub fn prepare_delete_subtree(
        &mut self,
        host: HostId,
        root: rho_desk::NodeId,
        cx: &gpui::App,
    ) -> Option<(
        rho_desk::OperationBatch,
        Vec<ClientMessage>,
        DeskSubtree,
        Option<rho_desk::NodeId>,
    )> {
        let mut subtree = self.capture_subtree(host, root, cx)?;
        let ids = subtree
            .nodes
            .iter()
            .map(|(node, _)| node.id)
            .collect::<Vec<_>>();
        let desk = self.tree_hosts.get_mut(&host)?;
        let nodes = desk.document.materialize();
        let root_node = nodes.iter().find(|node| node.id == root)?;
        let deleted = ids.iter().copied().collect::<BTreeSet<_>>();
        for node in &nodes {
            if node.owner != rho_desk::NodeOwner::Machine
                || !node.parent.is_some_and(|parent| deleted.contains(&parent))
            {
                continue;
            }
            let original_parent = node.parent?;
            let mut destination = Some(original_parent);
            while destination.is_some_and(|parent| deleted.contains(&parent)) {
                destination = nodes
                    .iter()
                    .find(|candidate| Some(candidate.id) == destination)
                    .and_then(|candidate| candidate.parent);
            }
            subtree.relocated_machine_rows.push(RelocatedMachineRow {
                node_id: node.id,
                destination,
                source_batch: None,
            });
            if subtree.relocation_destination.is_empty() {
                subtree.relocation_destination = destination
                    .and_then(|parent| desk.buffers.get(&parent))
                    .map(|buffer| buffer.read(cx).text())
                    .filter(|title| !title.is_empty())
                    .unwrap_or_else(|| "root".into());
            }
        }
        let root_index = nodes.iter().position(|node| node.id == root)?;
        let previous = nodes[..root_index]
            .iter()
            .rev()
            .find(|node| node.parent == root_node.parent && !ids.contains(&node.id));
        let next = nodes[root_index + 1..]
            .iter()
            .find(|node| node.parent == root_node.parent && !ids.contains(&node.id));
        if previous.is_some_and(|node| node.kind == rho_desk::NodeKind::Prose)
            && next.is_some_and(|node| node.kind == rho_desk::NodeKind::Prose)
        {
            // A heading cannot be removed if doing so would create two
            // adjacent prose runs. Empty-row Backspace owns the lossless
            // prose join/undo path.
            return None;
        }
        let mut messages = Vec::new();
        desk.next_tree_clock = desk.next_tree_clock.checked_add(1)?;
        messages.push(ClientMessage::DeskTreeApply {
            operation: rho_desk::TreeOperation::Delete {
                timestamp: rho_desk::TreeClock {
                    value: desk.next_tree_clock,
                    replica_id: desk.replica_id,
                },
                node_ids: ids.clone(),
            },
        });
        let mut expected = ids;
        expected.extend(subtree.relocated_machine_rows.iter().map(|row| row.node_id));
        expected.extend(
            subtree
                .relocated_machine_rows
                .iter()
                .filter_map(|row| row.destination),
        );
        let mut batch = self.operation_batch(host, expected, messages.clone(), None)?;
        if !subtree.relocated_machine_rows.is_empty() {
            batch.machine_relocation =
                Some(rho_desk::MachineRelocationIntent::EvacuateDeletedChildren);
        }
        for relocation in &mut subtree.relocated_machine_rows {
            relocation.source_batch = Some(batch.id);
        }
        let focus = next
            .filter(|node| node.owner == rho_desk::NodeOwner::User)
            .or_else(|| previous.filter(|node| node.owner == rho_desk::NodeOwner::User))
            .or_else(|| {
                root_node
                    .parent
                    .and_then(|parent| nodes.iter().find(|node| node.id == parent))
            })
            .or(next)
            .or(previous)
            .map(|node| node.id);
        Some((batch, messages, subtree, focus))
    }

    pub fn prepare_paste_subtree(
        &mut self,
        host: HostId,
        relative: rho_desk::NodeId,
        before: bool,
        subtree: &DeskSubtree,
    ) -> Option<(
        rho_desk::OperationBatch,
        Vec<ClientMessage>,
        rho_desk::NodeId,
    )> {
        let desk = self.tree_hosts.get_mut(&host)?;
        let materialized = desk.document.materialize();
        let relative_node = materialized.iter().find(|node| node.id == relative)?;
        let siblings = || {
            materialized
                .iter()
                .filter(|node| node.parent == relative_node.parent)
        };
        let adjacent = if before {
            siblings()
                .filter(|node| node.order < relative_node.order)
                .max_by_key(|node| &node.order)
        } else {
            siblings()
                .filter(|node| node.order > relative_node.order)
                .min_by_key(|node| &node.order)
        };
        let adjacent_id = adjacent.map(|node| node.id);
        let root_order = if before {
            rho_desk::OrderKey::between(
                adjacent.map(|node| &node.order),
                Some(&relative_node.order),
            )
        } else {
            rho_desk::OrderKey::between(
                Some(&relative_node.order),
                adjacent.map(|node| &node.order),
            )
        };
        let mut ids = BTreeMap::new();
        for (node, _) in &subtree.nodes {
            desk.next_node_counter = desk.next_node_counter.checked_add(1)?;
            ids.insert(
                node.id,
                rho_desk::NodeId {
                    replica_id: desk.replica_id,
                    counter: desk.next_node_counter,
                },
            );
        }
        let root_old = subtree.nodes.first()?.0.id;
        let root_new = ids[&root_old];
        let mut messages = Vec::new();
        for (node, text) in &subtree.nodes {
            desk.next_tree_clock = desk.next_tree_clock.checked_add(1)?;
            let node_id = ids[&node.id];
            messages.push(ClientMessage::DeskTreeApply {
                operation: rho_desk::TreeOperation::Create {
                    timestamp: rho_desk::TreeClock {
                        value: desk.next_tree_clock,
                        replica_id: desk.replica_id,
                    },
                    node_id,
                    kind: node.kind,
                    owner: rho_desk::NodeOwner::User,
                    parent: if node.id == root_old {
                        relative_node.parent
                    } else {
                        node.parent.and_then(|parent| ids.get(&parent).copied())
                    },
                    order: if node.id == root_old {
                        root_order.clone()
                    } else {
                        node.order.clone()
                    },
                },
            });
            if !text.is_empty() {
                messages.push(text_apply_message(desk.replica_id, node_id, text));
            }
            append_node_metadata(desk, &mut messages, node_id, node)?;
        }
        let mut expected = vec![relative];
        expected.extend(adjacent_id);
        let batch = self.operation_batch(host, expected, messages.clone(), None)?;
        Some((batch, messages, root_new))
    }

    pub fn prepare_restore_subtree(
        &mut self,
        host: HostId,
        subtree: &DeskSubtree,
    ) -> Option<(
        rho_desk::OperationBatch,
        Vec<ClientMessage>,
        rho_desk::NodeId,
    )> {
        let desk = self.tree_hosts.get_mut(&host)?;
        let root_old = subtree.nodes.first()?.0.id;
        let mut ids = BTreeMap::new();
        for (node, _) in &subtree.nodes {
            desk.next_node_counter = desk.next_node_counter.checked_add(1)?;
            ids.insert(
                node.id,
                rho_desk::NodeId {
                    replica_id: desk.replica_id,
                    counter: desk.next_node_counter,
                },
            );
        }
        let root_new = ids[&root_old];
        let mut messages = Vec::new();
        for (node, text) in &subtree.nodes {
            desk.next_tree_clock = desk.next_tree_clock.checked_add(1)?;
            let node_id = ids[&node.id];
            messages.push(ClientMessage::DeskTreeApply {
                operation: rho_desk::TreeOperation::Create {
                    timestamp: rho_desk::TreeClock {
                        value: desk.next_tree_clock,
                        replica_id: desk.replica_id,
                    },
                    node_id,
                    kind: node.kind,
                    owner: rho_desk::NodeOwner::User,
                    parent: if node.id == root_old {
                        node.parent
                    } else {
                        node.parent.and_then(|parent| ids.get(&parent).copied())
                    },
                    order: node.order.clone(),
                },
            });
            if !text.is_empty() {
                messages.push(text_apply_message(desk.replica_id, node_id, text));
            }
            append_node_metadata(desk, &mut messages, node_id, node)?;
        }
        let mut expected = subtree
            .nodes
            .first()?
            .0
            .parent
            .into_iter()
            .collect::<Vec<_>>();
        expected.extend(subtree.relocated_machine_rows.iter().map(|row| row.node_id));
        let mut batch = self.operation_batch(host, expected, messages.clone(), None)?;
        if let Some(delete_batch_id) = subtree
            .relocated_machine_rows
            .first()
            .and_then(|row| row.source_batch)
        {
            batch.machine_relocation = Some(rho_desk::MachineRelocationIntent::Restore {
                delete_batch_id,
                replacements: ids
                    .iter()
                    .map(|(deleted, replacement)| rho_desk::NodeReplacement {
                        deleted: *deleted,
                        replacement: *replacement,
                    })
                    .collect(),
            });
        }
        Some((batch, messages, root_new))
    }
    pub fn tree_node(
        &self,
        host: HostId,
        node_id: rho_desk::NodeId,
    ) -> Option<rho_desk::MaterializedNode> {
        self.tree_hosts
            .get(&host)?
            .document
            .materialize()
            .into_iter()
            .find(|node| node.id == node_id)
    }

    pub fn prepare_temporal_batch(
        &mut self,
        host: HostId,
        node_id: rho_desk::NodeId,
        values: Vec<(rho_desk::TemporalKind, Option<rho_desk::TemporalMark>)>,
    ) -> Option<(rho_desk::OperationBatch, Vec<ClientMessage>)> {
        let desk = self.tree_hosts.get_mut(&host)?;
        let node = desk
            .document
            .materialize()
            .into_iter()
            .find(|node| node.id == node_id)?;
        if node.kind != rho_desk::NodeKind::Heading || node.owner != rho_desk::NodeOwner::User {
            return None;
        }
        let mut messages = Vec::with_capacity(values.len());
        for (kind, value) in values {
            desk.next_tree_clock = desk.next_tree_clock.checked_add(1)?;
            messages.push(ClientMessage::DeskTreeApply {
                operation: rho_desk::TreeOperation::SetTemporal {
                    timestamp: rho_desk::TreeClock {
                        value: desk.next_tree_clock,
                        replica_id: desk.replica_id,
                    },
                    node_id,
                    kind,
                    value,
                },
            });
        }
        let batch = self.operation_batch(host, vec![node_id], messages.clone(), None)?;
        Some((batch, messages))
    }
    pub fn tree_source(
        &self,
        host: HostId,
    ) -> Option<(
        Vec<rho_desk::MaterializedNode>,
        BTreeMap<rho_desk::NodeId, Entity<Buffer>>,
    )> {
        let desk = self.tree_hosts.get(&host)?;
        Some((desk.document.materialize(), desk.buffers.clone()))
    }

    pub fn tree_node_for_buffers(
        &self,
        buffer_ids: &[BufferId],
        require_newline: bool,
        cx: &gpui::App,
    ) -> Option<(HostId, rho_desk::NodeId)> {
        self.tree_hosts.iter().find_map(|(host, desk)| {
            desk.buffers.iter().find_map(|(node_id, buffer)| {
                let buffer = buffer.read(cx);
                (buffer_ids.contains(&buffer.remote_id())
                    && (!require_newline || buffer.text().contains('\n')))
                .then_some((*host, *node_id))
            })
        })
    }

    pub fn operation_batch(
        &mut self,
        host: HostId,
        expected_ids: Vec<rho_desk::NodeId>,
        messages: Vec<ClientMessage>,
        reuse_id: Option<rho_desk::TreeClock>,
    ) -> Option<rho_desk::OperationBatch> {
        let desk = self.tree_hosts.get_mut(&host)?;
        let nodes = desk
            .document
            .materialize()
            .into_iter()
            .map(|node| (node.id, node))
            .collect::<BTreeMap<_, _>>();
        let mut expected = Vec::new();
        for node_id in expected_ids.into_iter().collect::<BTreeSet<_>>() {
            let node = nodes.get(&node_id)?;
            expected.push(rho_desk::NodeExpectation {
                node_id,
                kind: node.kind,
                owner: node.owner,
                parent: node.parent,
                order: node.order.clone(),
                text_version: desk.document.text_version(node_id).ok()?,
            });
        }
        let operations = messages
            .into_iter()
            .filter_map(|message| match message {
                ClientMessage::DeskTreeApply { operation } => {
                    Some(rho_desk::BatchOperation::Tree(operation))
                }
                ClientMessage::DeskNodeTextApply {
                    node_id,
                    operation,
                    transaction,
                } => Some(rho_desk::BatchOperation::Text {
                    node_id,
                    operation,
                    transaction,
                }),
                _ => None,
            })
            .collect::<Vec<_>>();
        if reuse_id.is_none() {
            desk.next_tree_clock = desk.next_tree_clock.checked_add(1)?;
        }
        let batch = rho_desk::OperationBatch {
            id: reuse_id.unwrap_or(rho_desk::TreeClock {
                value: desk.next_tree_clock,
                replica_id: desk.replica_id,
            }),
            expected,
            operations,
            machine_relocation: None,
        };
        self.pending_batches.insert((host, batch.id), batch.clone());
        Some(batch)
    }

    pub fn reset_rejected_batch(
        &mut self,
        host: HostId,
        id: rho_desk::TreeClock,
        snapshot: rho_desk::Snapshot,
        cx: &mut Context<Workspace>,
    ) -> Vec<rho_desk::BatchOperation> {
        self.pending_batches.remove(&(host, id));
        let dependent = self
            .pending_batch_text
            .remove(&(host, id))
            .unwrap_or_default();
        self.replace_tree_snapshot(host, snapshot, cx);
        dependent
    }

    pub fn tree_anchor_at(
        &self,
        host: HostId,
        node_id: rho_desk::NodeId,
        offset: usize,
        bias: text::Bias,
        cx: &gpui::App,
    ) -> Option<text::Anchor> {
        let buffer = self.tree_hosts.get(&host)?.buffers.get(&node_id)?.read(cx);
        Some(buffer.anchor_at(offset.min(buffer.len()), bias))
    }

    pub fn resolve_tree_anchor(
        &self,
        host: HostId,
        node_id: rho_desk::NodeId,
        anchor: text::Anchor,
        cx: &gpui::App,
    ) -> Option<usize> {
        let buffer = self.tree_hosts.get(&host)?.buffers.get(&node_id)?.read(cx);
        let anchor = text::Anchor::new(
            anchor.timestamp(),
            anchor.offset,
            anchor.bias,
            buffer.remote_id(),
        );
        Some(anchor.to_offset(&buffer.snapshot()))
    }

    pub fn replay_text_edit(
        &mut self,
        host: HostId,
        node_id: rho_desk::NodeId,
        edit: &(std::ops::Range<usize>, String),
        cx: &mut Context<Workspace>,
    ) -> bool {
        let Some(buffer) = self
            .tree_hosts
            .get(&host)
            .and_then(|desk| desk.buffers.get(&node_id))
            .cloned()
        else {
            return false;
        };
        let len = buffer.read(cx).len();
        if edit.0.start > edit.0.end || edit.0.end > len {
            return false;
        }
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(edit.0.clone(), edit.1.as_str())], None, cx)
        });
        true
    }

    pub fn record_pending_batch_text(
        &mut self,
        host: HostId,
        node_id: rho_desk::NodeId,
        operation: rho_desk::TextOperation,
        transaction: Option<rho_desk::TextTransaction>,
    ) {
        let id = self
            .pending_batches
            .iter()
            .find_map(|((batch_host, id), batch)| {
                (*batch_host == host
                    && batch.operations.iter().any(|operation| {
                        matches!(operation, rho_desk::BatchOperation::Tree(
                        rho_desk::TreeOperation::Create { node_id: created, .. }
                    ) if *created == node_id)
                    }))
                .then_some(*id)
            });
        if let Some(id) = id {
            self.pending_batch_text.entry((host, id)).or_default().push(
                rho_desk::BatchOperation::Text {
                    node_id,
                    operation,
                    transaction,
                },
            );
        }
    }

    pub fn update_pending_batch(&mut self, host: HostId, batch: &rho_desk::OperationBatch) {
        self.pending_batches.insert((host, batch.id), batch.clone());
    }

    pub fn keep_pending_batch_text(
        &mut self,
        host: HostId,
        id: rho_desk::TreeClock,
        operations: Vec<rho_desk::BatchOperation>,
    ) {
        self.pending_batch_text.insert((host, id), operations);
    }

    pub fn apply_tree_snapshot(
        &mut self,
        host: HostId,
        mut snapshot: rho_desk::Snapshot,
        replica_id: u16,
        cx: &mut Context<Workspace>,
    ) -> bool {
        if self
            .tree_hosts
            .get(&host)
            .is_some_and(|desk| snapshot_is_stale(desk.sequence, snapshot.sequence))
        {
            return false;
        }
        if let Some(replacement) = self.pending_replacements.remove(&host)
            && replacement.sequence > snapshot.sequence
        {
            snapshot = replacement;
        }
        let sequence = snapshot.sequence;
        let next_tree_clock = snapshot
            .version
            .iter()
            .filter(|clock| clock.replica_id == replica_id)
            .map(|clock| clock.value)
            .max()
            .unwrap_or(0);
        let next_node_counter = snapshot
            .nodes
            .iter()
            .filter(|node| node.id.replica_id == replica_id)
            .map(|node| node.id.counter)
            .max()
            .unwrap_or(0);
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
                next_tree_clock,
                next_node_counter,
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
        let deleted = match &record.operation {
            rho_desk::TreeOperation::Delete { node_ids, .. } => node_ids.clone(),
            _ => Vec::new(),
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
        let timestamp = record.operation.timestamp();
        let applied = match desk.document.apply(record.operation) {
            Ok(applied) => applied,
            Err(_) => return true,
        };
        if applied && let (Some((node_id, owner)), Some(buffer_id)) = (created, new_buffer_id) {
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
            if node_id.replica_id == desk.replica_id {
                desk.next_node_counter = desk.next_node_counter.max(node_id.counter);
            }
        }
        if applied {
            for node_id in deleted {
                desk.buffers.remove(&node_id);
            }
        }
        desk.sequence = record.sequence;
        if timestamp.replica_id == desk.replica_id {
            desk.next_tree_clock = desk.next_tree_clock.max(timestamp.value);
        }
        false
    }

    pub fn prepare_structure_move(
        &mut self,
        host: HostId,
        node_id: rho_desk::NodeId,
        demote: bool,
    ) -> Option<rho_desk::TreeOperation> {
        let desk = self.tree_hosts.get_mut(&host)?;
        let nodes = desk.document.materialize();
        let index = nodes.iter().position(|node| node.id == node_id)?;
        let node = &nodes[index];
        if node.owner != rho_desk::NodeOwner::User || node.kind != rho_desk::NodeKind::Heading {
            return None;
        }
        let (parent, order) = if demote {
            let previous = nodes[..index].iter().rev().find(|candidate| {
                candidate.parent == node.parent && candidate.kind == rho_desk::NodeKind::Heading
            })?;
            let last = nodes
                .iter()
                .filter(|candidate| candidate.parent == Some(previous.id))
                .map(|candidate| &candidate.order)
                .max();
            (Some(previous.id), rho_desk::OrderKey::between(last, None))
        } else {
            let parent_id = node.parent?;
            let parent_node = nodes.iter().find(|candidate| candidate.id == parent_id)?;
            if parent_node.kind != rho_desk::NodeKind::Heading {
                return None;
            }
            let next = nodes
                .iter()
                .filter(|candidate| candidate.parent == parent_node.parent)
                .filter(|candidate| candidate.order > parent_node.order)
                .map(|candidate| &candidate.order)
                .min();
            (
                parent_node.parent,
                rho_desk::OrderKey::between(Some(&parent_node.order), next),
            )
        };
        desk.next_tree_clock = desk.next_tree_clock.checked_add(1)?;
        Some(rho_desk::TreeOperation::Move {
            timestamp: rho_desk::TreeClock {
                value: desk.next_tree_clock,
                replica_id: desk.replica_id,
            },
            node_id,
            parent,
            order,
        })
    }

    pub fn prepare_new_heading(
        &mut self,
        host: HostId,
        relative: rho_desk::NodeId,
        child: bool,
        above: bool,
    ) -> Option<rho_desk::TreeOperation> {
        let desk = self.tree_hosts.get_mut(&host)?;
        let nodes = desk.document.materialize();
        let node = nodes.iter().find(|node| node.id == relative)?;
        if node.kind != rho_desk::NodeKind::Heading || node.owner != rho_desk::NodeOwner::User {
            return None;
        }
        let (parent, order) = if child {
            let last = nodes
                .iter()
                .filter(|candidate| candidate.parent == Some(relative))
                .map(|candidate| &candidate.order)
                .max();
            (Some(relative), rho_desk::OrderKey::between(last, None))
        } else if above {
            let previous = nodes
                .iter()
                .filter(|candidate| candidate.parent == node.parent && candidate.order < node.order)
                .map(|candidate| &candidate.order)
                .max();
            (
                node.parent,
                rho_desk::OrderKey::between(previous, Some(&node.order)),
            )
        } else {
            let next = nodes
                .iter()
                .filter(|candidate| candidate.parent == node.parent && candidate.order > node.order)
                .map(|candidate| &candidate.order)
                .min();
            (
                node.parent,
                rho_desk::OrderKey::between(Some(&node.order), next),
            )
        };
        Some(new_heading_operation(desk, parent, order)?)
    }

    pub fn prepare_reorder(
        &mut self,
        host: HostId,
        node_id: rho_desk::NodeId,
        down: bool,
    ) -> Option<rho_desk::TreeOperation> {
        let desk = self.tree_hosts.get_mut(&host)?;
        let nodes = desk.document.materialize();
        let node = nodes.iter().find(|node| node.id == node_id)?;
        if node.kind != rho_desk::NodeKind::Heading || node.owner != rho_desk::NodeOwner::User {
            return None;
        }
        let siblings = nodes
            .iter()
            .filter(|candidate| candidate.parent == node.parent)
            .collect::<Vec<_>>();
        let index = siblings
            .iter()
            .position(|candidate| candidate.id == node_id)?;
        let order = if down {
            let next_index = (index + 1..siblings.len())
                .find(|&i| siblings[i].kind == rho_desk::NodeKind::Heading)?;
            let next = siblings[next_index];
            rho_desk::OrderKey::between(
                Some(&next.order),
                siblings.get(next_index + 1).map(|n| &n.order),
            )
        } else {
            let previous_index = (0..index)
                .rev()
                .find(|&i| siblings[i].kind == rho_desk::NodeKind::Heading)?;
            let previous = siblings[previous_index];
            rho_desk::OrderKey::between(
                previous_index
                    .checked_sub(1)
                    .and_then(|i| siblings.get(i))
                    .map(|n| &n.order),
                Some(&previous.order),
            )
        };
        desk.next_tree_clock = desk.next_tree_clock.checked_add(1)?;
        Some(rho_desk::TreeOperation::Move {
            timestamp: rho_desk::TreeClock {
                value: desk.next_tree_clock,
                replica_id: desk.replica_id,
            },
            node_id,
            parent: node.parent,
            order,
        })
    }

    /// Deletes an empty user heading. When it sits between two prose
    /// siblings, their text is joined first and the redundant right-hand
    /// prose node is deleted with the heading.
    pub fn prepare_delete_empty(
        &mut self,
        host: HostId,
        node_id: rho_desk::NodeId,
        cx: &mut Context<Workspace>,
    ) -> Option<(
        Vec<ClientMessage>,
        Option<(rho_desk::NodeId, usize)>,
        Vec<rho_desk::NodeId>,
        DeskDeleteEmptyUndo,
    )> {
        let desk = self.tree_hosts.get_mut(&host)?;
        let nodes = desk.document.materialize();
        let node = nodes.iter().find(|node| node.id == node_id)?;
        if node.kind != rho_desk::NodeKind::Heading
            || node.owner != rho_desk::NodeOwner::User
            || !desk.buffers.get(&node_id)?.read(cx).text().is_empty()
        {
            return None;
        }
        let siblings = nodes
            .iter()
            .filter(|candidate| candidate.parent == node.parent)
            .collect::<Vec<_>>();
        let index = siblings
            .iter()
            .position(|candidate| candidate.id == node_id)?;
        let previous = index
            .checked_sub(1)
            .and_then(|index| siblings.get(index))
            .filter(|node| {
                node.kind == rho_desk::NodeKind::Prose && node.owner == rho_desk::NodeOwner::User
            });
        let sibling_next = siblings.get(index + 1).filter(|node| {
            node.kind == rho_desk::NodeKind::Prose && node.owner == rho_desk::NodeOwner::User
        });
        let children = nodes
            .iter()
            .filter(|candidate| candidate.parent == Some(node_id))
            .collect::<Vec<_>>();
        if children.len() > 1
            || children.first().is_some_and(|node| {
                node.kind != rho_desk::NodeKind::Prose || node.owner != rho_desk::NodeOwner::User
            })
        {
            return None;
        }
        let child = children.first().copied();
        let right = child.or(sibling_next.copied());
        let undo = DeskDeleteEmptyUndo {
            heading: node.clone(),
            deleted_right: previous.zip(right).map(|(_, right)| {
                (
                    right.clone(),
                    desk.buffers.get(&right.id).unwrap().read(cx).text(),
                )
            }),
            previous_text: previous.map(|previous| {
                (
                    previous.id,
                    desk.buffers.get(&previous.id).unwrap().read(cx).text(),
                )
            }),
            moved_child: if previous.is_none() {
                child.map(|child| child.id)
            } else {
                None
            },
        };
        let mut expected = vec![node_id];
        expected.extend(previous.map(|node| node.id));
        expected.extend(right.map(|node| node.id));
        let mut delete_ids = vec![node_id];
        let mut messages = Vec::new();
        let focus = if let (Some(previous), Some(right)) = (previous, right) {
            let previous_len = desk.buffers.get(&previous.id)?.read(cx).len();
            let previous_text = desk.buffers.get(&previous.id)?.read(cx).text();
            let right_text = desk.buffers.get(&right.id)?.read(cx).text().to_owned();
            if !right_text.is_empty() {
                let joined = prose_join_suffix(&previous_text, &right_text);
                desk.buffers.get(&previous.id)?.update(cx, |buffer, cx| {
                    let len = buffer.len();
                    buffer.edit([(len..len, joined)], None, cx);
                });
            }
            delete_ids.push(right.id);
            Some((previous.id, previous_len))
        } else if let Some(previous) = previous {
            Some((previous.id, desk.buffers.get(&previous.id)?.read(cx).len()))
        } else {
            right.map(|node| (node.id, 0))
        };
        if previous.is_none()
            && let Some(child) = child
        {
            desk.next_tree_clock = desk.next_tree_clock.checked_add(1)?;
            messages.push(ClientMessage::DeskTreeApply {
                operation: rho_desk::TreeOperation::Move {
                    timestamp: rho_desk::TreeClock {
                        value: desk.next_tree_clock,
                        replica_id: desk.replica_id,
                    },
                    node_id: child.id,
                    parent: node.parent,
                    order: node.order.clone(),
                },
            });
            delete_ids.retain(|id| *id != child.id);
        }
        desk.next_tree_clock = desk.next_tree_clock.checked_add(1)?;
        messages.push(ClientMessage::DeskTreeApply {
            operation: rho_desk::TreeOperation::Delete {
                timestamp: rho_desk::TreeClock {
                    value: desk.next_tree_clock,
                    replica_id: desk.replica_id,
                },
                node_ids: delete_ids,
            },
        });
        Some((messages, focus, expected, undo))
    }

    /// Reverses `prepare_delete_empty`. Deleted CRDT ids cannot be revived,
    /// so the empty heading (and a joined-away prose node) receive fresh ids.
    pub fn prepare_restore_deleted_empty(
        &mut self,
        host: HostId,
        undo: &DeskDeleteEmptyUndo,
        cx: &mut Context<Workspace>,
    ) -> Option<(Vec<ClientMessage>, rho_desk::NodeId, Vec<rho_desk::NodeId>)> {
        let desk = self.tree_hosts.get_mut(&host)?;
        let materialized = desk.document.materialize();
        if let Some((previous, _)) = &undo.previous_text {
            materialized.iter().find(|node| node.id == *previous)?;
        }
        if let Some(child) = undo.moved_child {
            materialized.iter().find(|node| node.id == child)?;
        }

        desk.next_node_counter = desk.next_node_counter.checked_add(1)?;
        desk.next_tree_clock = desk.next_tree_clock.checked_add(1)?;
        let heading_id = rho_desk::NodeId {
            replica_id: desk.replica_id,
            counter: desk.next_node_counter,
        };
        let mut messages = vec![ClientMessage::DeskTreeApply {
            operation: rho_desk::TreeOperation::Create {
                timestamp: rho_desk::TreeClock {
                    value: desk.next_tree_clock,
                    replica_id: desk.replica_id,
                },
                node_id: heading_id,
                kind: undo.heading.kind,
                owner: undo.heading.owner,
                parent: undo.heading.parent,
                order: undo.heading.order.clone(),
            },
        }];
        append_node_metadata(desk, &mut messages, heading_id, &undo.heading)?;
        if let Some((right, text)) = &undo.deleted_right {
            desk.next_node_counter = desk.next_node_counter.checked_add(1)?;
            desk.next_tree_clock = desk.next_tree_clock.checked_add(1)?;
            let right_id = rho_desk::NodeId {
                replica_id: desk.replica_id,
                counter: desk.next_node_counter,
            };
            messages.push(ClientMessage::DeskTreeApply {
                operation: rho_desk::TreeOperation::Create {
                    timestamp: rho_desk::TreeClock {
                        value: desk.next_tree_clock,
                        replica_id: desk.replica_id,
                    },
                    node_id: right_id,
                    kind: right.kind,
                    owner: right.owner,
                    parent: (right.parent == Some(undo.heading.id))
                        .then_some(heading_id)
                        .or(right.parent),
                    order: right.order.clone(),
                },
            });
            if !text.is_empty() {
                messages.push(text_apply_message(desk.replica_id, right_id, text));
            }
            append_node_metadata(desk, &mut messages, right_id, right)?;
        }
        if let Some(child) = undo.moved_child {
            desk.next_tree_clock = desk.next_tree_clock.checked_add(1)?;
            messages.push(ClientMessage::DeskTreeApply {
                operation: rho_desk::TreeOperation::Move {
                    timestamp: rho_desk::TreeClock {
                        value: desk.next_tree_clock,
                        replica_id: desk.replica_id,
                    },
                    node_id: child,
                    parent: Some(heading_id),
                    order: materialized
                        .iter()
                        .find(|node| node.id == child)?
                        .order
                        .clone(),
                },
            });
        }
        if let Some((previous, text)) = &undo.previous_text {
            let buffer = desk.buffers.get(previous)?.clone();
            buffer.update(cx, |buffer, cx| {
                let len = buffer.len();
                buffer.edit([(0..len, text.as_str())], None, cx);
            });
        }
        let mut expected = undo.heading.parent.into_iter().collect::<Vec<_>>();
        expected.extend(undo.previous_text.iter().map(|(id, _)| *id));
        expected.extend(undo.moved_child);
        Some((messages, heading_id, expected))
    }

    pub fn recognize_heading(
        &mut self,
        host: HostId,
        node_id: rho_desk::NodeId,
        line_end: usize,
        focus_after: bool,
        cx: &mut Context<Workspace>,
    ) -> Option<(Vec<ClientMessage>, rho_desk::NodeId, Vec<rho_desk::NodeId>)> {
        let Some(desk) = self.tree_hosts.get_mut(&host) else {
            return None;
        };
        let nodes = desk.document.materialize();
        let Some(node) = nodes
            .iter()
            .find(|node| node.id == node_id && node.kind == rho_desk::NodeKind::Prose)
        else {
            return None;
        };
        let Some(buffer) = desk.buffers.get(&node_id).cloned() else {
            return None;
        };
        let text = buffer.read(cx).text();
        let line_end = line_end.min(text.len());
        let start = text[..line_end].rfind('\n').map_or(0, |offset| offset + 1);
        if !text[start..line_end].starts_with("* ") {
            return None;
        }
        let physical_line_end = text[line_end..]
            .find('\n')
            .map_or(text.len(), |offset| line_end + offset);
        let after_start = (physical_line_end < text.len())
            .then_some(physical_line_end + 1)
            .unwrap_or(physical_line_end);
        let before = text[..start].to_owned();
        let title = text[start + 2..physical_line_end].to_owned();
        let after = text[after_start..].to_owned();
        buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, before.as_str())], None, cx);
        });

        let next = nodes
            .iter()
            .filter(|candidate| candidate.parent == node.parent && candidate.order > node.order)
            .min_by_key(|candidate| &candidate.order);
        let next_order = next.map(|candidate| &candidate.order);
        let heading_order = if before.is_empty() {
            node.order.clone()
        } else {
            rho_desk::OrderKey::between(Some(&node.order), next_order)
        };
        let after_order = rho_desk::OrderKey::between(None, None);
        let mut next_id = || {
            desk.next_node_counter += 1;
            rho_desk::NodeId {
                replica_id: desk.replica_id,
                counter: desk.next_node_counter,
            }
        };
        let heading_id = next_id();
        let after_id = next_id();
        let mut next_clock = || {
            desk.next_tree_clock += 1;
            rho_desk::TreeClock {
                value: desk.next_tree_clock,
                replica_id: desk.replica_id,
            }
        };
        let mut messages = Vec::new();
        if before.is_empty() {
            messages.push(ClientMessage::DeskTreeApply {
                operation: rho_desk::TreeOperation::Delete {
                    timestamp: next_clock(),
                    node_ids: vec![node_id],
                },
            });
        }
        messages.push(ClientMessage::DeskTreeApply {
            operation: rho_desk::TreeOperation::Create {
                timestamp: next_clock(),
                node_id: heading_id,
                kind: rho_desk::NodeKind::Heading,
                owner: rho_desk::NodeOwner::User,
                parent: node.parent,
                order: heading_order,
            },
        });
        messages.push(ClientMessage::DeskTreeApply {
            operation: rho_desk::TreeOperation::Create {
                timestamp: next_clock(),
                node_id: after_id,
                kind: rho_desk::NodeKind::Prose,
                owner: rho_desk::NodeOwner::User,
                parent: Some(heading_id),
                order: after_order,
            },
        });
        if !title.is_empty() {
            messages.push(text_apply_message(desk.replica_id, heading_id, &title));
        }
        if !after.is_empty() {
            messages.push(text_apply_message(desk.replica_id, after_id, &after));
        }
        let mut expected = vec![node_id];
        expected.extend(next.map(|node| node.id));
        Some((
            messages,
            if focus_after { after_id } else { heading_id },
            expected,
        ))
    }

    /// Enter ends a heading's single-line title and moves the remainder into
    /// a prose child. This keeps the node kind structural even though the
    /// native editor initially delivers the newline to the title buffer.
    pub fn split_heading_on_newline(
        &mut self,
        host: HostId,
        node_id: rho_desk::NodeId,
        newline_offset: usize,
        cx: &mut Context<Workspace>,
    ) -> Option<(Vec<ClientMessage>, rho_desk::NodeId, Vec<rho_desk::NodeId>)> {
        let desk = self.tree_hosts.get_mut(&host)?;
        let nodes = desk.document.materialize();
        nodes.iter().find(|node| {
            node.id == node_id
                && node.kind == rho_desk::NodeKind::Heading
                && node.owner == rho_desk::NodeOwner::User
        })?;
        let buffer = desk.buffers.get(&node_id)?.clone();
        let text = buffer.read(cx).text();
        let newline_offset = (text.as_bytes().get(newline_offset) == Some(&b'\n'))
            .then_some(newline_offset)
            .or_else(|| text.find('\n'))?;
        let title = text[..newline_offset].to_owned();
        let prose = text[newline_offset + 1..].to_owned();
        buffer.update(cx, |buffer, cx| {
            let len = buffer.len();
            buffer.edit([(0..len, title)], None, cx);
        });
        let first_child = nodes
            .iter()
            .filter(|candidate| candidate.parent == Some(node_id))
            .min_by_key(|candidate| &candidate.order);
        desk.next_node_counter = desk.next_node_counter.checked_add(1)?;
        desk.next_tree_clock = desk.next_tree_clock.checked_add(1)?;
        let prose_id = rho_desk::NodeId {
            replica_id: desk.replica_id,
            counter: desk.next_node_counter,
        };
        let mut messages = vec![ClientMessage::DeskTreeApply {
            operation: rho_desk::TreeOperation::Create {
                timestamp: rho_desk::TreeClock {
                    value: desk.next_tree_clock,
                    replica_id: desk.replica_id,
                },
                node_id: prose_id,
                kind: rho_desk::NodeKind::Prose,
                owner: rho_desk::NodeOwner::User,
                parent: Some(node_id),
                order: rho_desk::OrderKey::between(None, first_child.map(|node| &node.order)),
            },
        }];
        if !prose.is_empty() {
            messages.push(text_apply_message(desk.replica_id, prose_id, &prose));
        }
        let mut expected = vec![node_id];
        expected.extend(first_child.map(|node| node.id));
        Some((messages, prose_id, expected))
    }

    pub fn apply_optimistic(
        &mut self,
        host: HostId,
        messages: &[ClientMessage],
        cx: &mut Context<Workspace>,
    ) {
        let mut creates = BTreeMap::new();
        for message in messages {
            if let ClientMessage::DeskTreeApply {
                operation: rho_desk::TreeOperation::Create { node_id, owner, .. },
            } = message
            {
                let buffer_id = BufferId::new(self.next_buffer_id).expect("nonzero GUI buffer id");
                self.next_buffer_id += 1;
                creates.insert(*node_id, (*owner, buffer_id));
            }
        }
        let Some(desk) = self.tree_hosts.get_mut(&host) else {
            return;
        };
        for message in messages {
            match message {
                ClientMessage::DeskTreeApply { operation } => {
                    if matches!(operation, rho_desk::TreeOperation::Delete { .. }) {
                        continue;
                    }
                    if desk.document.apply(operation.clone()) != Ok(true) {
                        continue;
                    }
                    if let rho_desk::TreeOperation::Create { node_id, .. } = operation {
                        let (owner, buffer_id) = creates[node_id];
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
                            *node_id,
                            cx,
                        );
                        desk.buffers.insert(*node_id, buffer);
                        desk._subscriptions.push(subscription);
                    }
                }
                ClientMessage::DeskNodeTextApply {
                    node_id, operation, ..
                } => {
                    if let (Some(buffer), Ok(operation)) =
                        (desk.buffers.get(node_id), operation.to_text())
                    {
                        buffer.update(cx, |buffer, cx| {
                            buffer.apply_ops([language::Operation::Buffer(operation)], cx)
                        });
                    }
                }
                _ => {}
            }
        }
    }

    pub fn apply_optimistic_delete(&mut self, host: HostId, operation: &rho_desk::TreeOperation) {
        let rho_desk::TreeOperation::Delete { node_ids, .. } = operation else {
            return;
        };
        let Some(desk) = self.tree_hosts.get_mut(&host) else {
            return;
        };
        if desk.document.apply(operation.clone()) == Ok(true) {
            for node_id in node_ids {
                desk.buffers.remove(node_id);
            }
        }
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

    /// Applies one accepted mixed batch and advances the shared stream once.
    pub fn apply_batch(
        &mut self,
        host: HostId,
        record: rho_desk::BatchOpRecord,
        cx: &mut Context<Workspace>,
    ) -> bool {
        self.pending_batches.remove(&(host, record.batch.id));
        self.pending_batch_text.remove(&(host, record.batch.id));
        let Some(desk) = self.tree_hosts.get_mut(&host) else {
            return true;
        };
        if record.sequence <= desk.sequence {
            return false;
        }
        if sequence_has_gap(desk.sequence, record.sequence) {
            return true;
        }
        for operation in record.batch.operations {
            match operation {
                rho_desk::BatchOperation::Tree(operation) => {
                    let created = match &operation {
                        rho_desk::TreeOperation::Create { node_id, owner, .. } => {
                            Some((*node_id, *owner))
                        }
                        _ => None,
                    };
                    let deleted = match &operation {
                        rho_desk::TreeOperation::Delete { node_ids, .. } => node_ids.clone(),
                        _ => Vec::new(),
                    };
                    let applied = match desk.document.apply(operation) {
                        Ok(applied) => applied,
                        Err(_) => return true,
                    };
                    if applied && let Some((node_id, owner)) = created {
                        let buffer_id =
                            BufferId::new(self.next_buffer_id).expect("nonzero GUI buffer id");
                        self.next_buffer_id += 1;
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
                    if applied {
                        for node_id in deleted {
                            desk.buffers.remove(&node_id);
                        }
                    }
                }
                rho_desk::BatchOperation::Text {
                    node_id,
                    operation,
                    transaction,
                } => {
                    if desk
                        .document
                        .apply_text(node_id, operation.clone(), transaction)
                        .is_err()
                    {
                        return true;
                    }
                    if let (Some(buffer), Ok(operation)) =
                        (desk.buffers.get(&node_id), operation.to_text())
                    {
                        buffer.update(cx, |buffer, cx| {
                            buffer.apply_ops([language::Operation::Buffer(operation)], cx)
                        });
                    }
                }
            }
        }
        for operation in record.daemon_tree_operations {
            if desk.document.apply(operation).is_err() {
                return true;
            }
        }
        desk.sequence = record.sequence;
        false
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
    let mut previous_text = buffer.read(cx).text();
    let mut pending_visible_edit = None;
    let subscription = cx.subscribe(&buffer, move |workspace, buffer, event, cx| {
        let current_text = buffer.read(cx).text();
        match event {
            BufferEvent::Edited { source } => {
                pending_visible_edit = source
                    .is_local()
                    .then(|| visible_text_edit(&previous_text, &current_text))
                    .flatten();
                previous_text = current_text;
                return;
            }
            BufferEvent::Operation {
                operation: language::Operation::Buffer(operation),
                is_local: true,
            } => {
                let operation = rho_desk::TextOperation::from_text(operation);
                let timestamp = operation.timestamp();
                let visible_edit = pending_visible_edit
                    .take()
                    .or_else(|| visible_text_edit(&previous_text, &current_text));
                previous_text = current_text;
                workspace.send_desk_node_text(
                    host,
                    node_id,
                    operation,
                    rho_desk::TextTransaction {
                        id: timestamp,
                        edit_ids: vec![timestamp],
                    },
                    visible_edit,
                    cx,
                );
                return;
            }
            BufferEvent::Operation { .. } => {
                pending_visible_edit = None;
                previous_text = current_text;
                return;
            }
            _ => return,
        }
    });
    (buffer, subscription)
}

fn visible_text_edit(before: &str, after: &str) -> Option<(std::ops::Range<usize>, String)> {
    if before == after {
        return None;
    }
    let start = before
        .chars()
        .zip(after.chars())
        .take_while(|(left, right)| left == right)
        .map(|(character, _)| character.len_utf8())
        .sum::<usize>();
    let suffix = before[start..]
        .chars()
        .rev()
        .zip(after[start..].chars().rev())
        .take_while(|(left, right)| left == right)
        .map(|(character, _)| character.len_utf8())
        .sum::<usize>();
    let before_end = before.len() - suffix;
    let after_end = after.len() - suffix;
    Some((start..before_end, after[start..after_end].to_owned()))
}

fn new_heading_operation(
    desk: &mut HostTreeDesk,
    parent: Option<rho_desk::NodeId>,
    order: rho_desk::OrderKey,
) -> Option<rho_desk::TreeOperation> {
    desk.next_node_counter = desk.next_node_counter.checked_add(1)?;
    desk.next_tree_clock = desk.next_tree_clock.checked_add(1)?;
    Some(rho_desk::TreeOperation::Create {
        timestamp: rho_desk::TreeClock {
            value: desk.next_tree_clock,
            replica_id: desk.replica_id,
        },
        node_id: rho_desk::NodeId {
            replica_id: desk.replica_id,
            counter: desk.next_node_counter,
        },
        kind: rho_desk::NodeKind::Heading,
        owner: rho_desk::NodeOwner::User,
        parent,
        order,
    })
}

fn prose_join_suffix(previous: &str, next: &str) -> String {
    if !previous.is_empty() && !next.is_empty() && !previous.ends_with('\n') {
        format!("\n{next}")
    } else {
        next.to_owned()
    }
}

fn text_apply_message(replica_id: u16, node_id: rho_desk::NodeId, text: &str) -> ClientMessage {
    let mut buffer = text::Buffer::new(
        ReplicaId::new(replica_id),
        BufferId::new(node_id.counter).expect("nonzero Desk node counter"),
        "",
    );
    let operation = rho_desk::TextOperation::from_text(&buffer.edit([(0..0, text)]));
    let timestamp = operation.timestamp();
    ClientMessage::DeskNodeTextApply {
        node_id,
        operation,
        transaction: Some(rho_desk::TextTransaction {
            id: timestamp,
            edit_ids: vec![timestamp],
        }),
    }
}

fn append_node_metadata(
    desk: &mut HostTreeDesk,
    messages: &mut Vec<ClientMessage>,
    node_id: rho_desk::NodeId,
    source: &rho_desk::MaterializedNode,
) -> Option<()> {
    for (&kind, &value) in &source.temporal {
        desk.next_tree_clock = desk.next_tree_clock.checked_add(1)?;
        messages.push(ClientMessage::DeskTreeApply {
            operation: rho_desk::TreeOperation::SetTemporal {
                timestamp: rho_desk::TreeClock {
                    value: desk.next_tree_clock,
                    replica_id: desk.replica_id,
                },
                node_id,
                kind,
                value: Some(value),
            },
        });
    }
    for (&kind, value) in &source.bindings {
        desk.next_tree_clock = desk.next_tree_clock.checked_add(1)?;
        messages.push(ClientMessage::DeskTreeApply {
            operation: rho_desk::TreeOperation::SetBinding {
                timestamp: rho_desk::TreeClock {
                    value: desk.next_tree_clock,
                    replica_id: desk.replica_id,
                },
                node_id,
                kind,
                value: Some(value.clone()),
            },
        });
    }
    for tag in &source.tags {
        desk.next_tree_clock = desk.next_tree_clock.checked_add(1)?;
        messages.push(ClientMessage::DeskTreeApply {
            operation: rho_desk::TreeOperation::SetTag {
                timestamp: rho_desk::TreeClock {
                    value: desk.next_tree_clock,
                    replica_id: desk.replica_id,
                },
                node_id,
                tag: tag.clone(),
                present: true,
            },
        });
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::{prose_join_suffix, sequence_has_gap, snapshot_is_stale};

    #[test]
    fn prose_merge_preserves_the_excerpt_line_boundary() {
        assert_eq!(prose_join_suffix("before", "after"), "\nafter");
        assert_eq!(prose_join_suffix("before\n", "after"), "after");
        assert_eq!(prose_join_suffix("", "after"), "after");
        assert_eq!(prose_join_suffix("before", ""), "");
    }

    #[test]
    fn tree_sequence_accepts_duplicates_and_next_but_detects_loss() {
        assert!(!sequence_has_gap(7, 7));
        assert!(!sequence_has_gap(7, 8));
        assert!(sequence_has_gap(7, 9));
        assert!(!sequence_has_gap(u64::MAX, u64::MAX));
    }

    #[test]
    fn tree_snapshot_never_regresses_live_state() {
        assert!(snapshot_is_stale(11, 10));
        assert!(!snapshot_is_stale(11, 11));
        assert!(!snapshot_is_stale(11, 12));
    }
}
