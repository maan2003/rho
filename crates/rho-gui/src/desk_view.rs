use std::collections::{BTreeMap, BTreeSet};

use gpui::{AppContext as _, Context, Entity};
use language::{Buffer, BufferEvent, Capability};
use rho_ui_proto::ClientMessage;
use text::{BufferId, ReplicaId};

use crate::registry::HostId;
use crate::workspace::Workspace;

struct HostDeskCells {
    /// What the daemon has told us. A rejected mutation falls back to this.
    confirmed: rho_desk::cells::Store,
    /// What the reader sees: `confirmed` plus every mutation still in
    /// flight, so a keypress shows before the round trip finishes.
    view: rho_desk::cells::Store,
    /// Mutations sent and not yet visible in `confirmed`, oldest first.
    pending: Vec<rho_desk::cells::CellMutation>,
    buffers: BTreeMap<rho_desk::NodeId, Entity<Buffer>>,
    _subscriptions: Vec<gpui::Subscription>,
    /// The node and text replica namespace the daemon assigned this
    /// connection. Every node this GUI creates lives in it.
    namespace: u16,
    next_node_counter: u64,
    /// A `DeskSync` is in flight; the daemon answers exactly one.
    syncing: bool,
    /// The newest frontier poked while a sync was in flight. A poke that
    /// races its response must not be dropped, so it is answered after.
    poked: Option<rho_desk::cells::Version>,
}

impl HostDeskCells {
    /// The stamp a new mutation carries: past everything this GUI has
    /// observed, so it beats its own earlier writes and cannot jump more
    /// than one past the daemon's global maximum.
    fn next_stamp(&self, device: rho_desk::cells::DeviceId) -> rho_desk::cells::Stamp {
        let version = self
            .view
            .version()
            .values()
            .copied()
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        rho_desk::cells::Stamp { device, version }
    }

    /// Replays the unconfirmed writes over the confirmed cells. Used when a
    /// rejection drops one from the middle of the queue.
    fn rebuild_view(&mut self, device: rho_desk::cells::DeviceId) {
        let mut view = rho_desk::cells::Store::new(device);
        // The confirmed store is this GUI's own merge; it cannot fail to
        // merge into an empty store of the same device.
        let _ = view.merge(self.confirmed.snapshot());
        for mutation in &self.pending {
            if let Err(error) = view.apply_mutation(mutation) {
                tracing::warn!(%error, "dropping an unreplayable Desk mutation");
            }
        }
        self.view = view;
    }

    /// Forgets the mutations the daemon has now told us about, which is what
    /// keeps the replay queue from growing for the life of the process.
    fn prune_pending(&mut self) {
        let confirmed = self.confirmed.version().clone();
        self.pending.retain(|mutation| {
            confirmed.get(&mutation.stamp.device).copied().unwrap_or(0) < mutation.stamp.version
        });
    }
}

/// Whether `frontier` covers every device version in `poke`.
fn covers(frontier: &rho_desk::cells::Version, poke: &rho_desk::cells::Version) -> bool {
    poke.iter()
        .all(|(device, version)| frontier.get(device).copied().unwrap_or(0) >= *version)
}

/// Workspace-owned Desk state for every attached host, on the cell protocol.
pub struct DeskCells {
    device: rho_desk::cells::DeviceId,
    next_buffer_id: u64,
    hosts: BTreeMap<HostId, HostDeskCells>,
}

impl DeskCells {
    pub fn new(device: rho_desk::cells::DeviceId) -> Self {
        Self {
            device,
            next_buffer_id: 1,
            hosts: BTreeMap::new(),
        }
    }

    pub fn device(&self) -> rho_desk::cells::DeviceId {
        self.device
    }

    /// The handshake, sent on connect and after every poke. `known` is what
    /// this GUI already holds, so the daemon answers with the difference.
    pub fn sync(&mut self, host: HostId) -> ClientMessage {
        let known = match self.hosts.get_mut(&host) {
            Some(desk) => {
                desk.syncing = true;
                desk.confirmed.version().clone()
            }
            None => rho_desk::cells::Version::new(),
        };
        ClientMessage::DeskSync {
            device: self.device,
            known,
        }
    }

    /// The daemon's answer. Returns a further `DeskSync` when a poke
    /// arrived while this one was in flight and the answer does not already
    /// cover it.
    pub fn synced(
        &mut self,
        host: HostId,
        namespace: u16,
        delta: rho_desk::cells::Snapshot,
        texts: Vec<rho_desk::NodeTextSnapshot>,
        cx: &mut Context<Workspace>,
    ) -> Option<ClientMessage> {
        let frontier = delta.version.clone();
        let existing = self.hosts.contains_key(&host);
        if !existing {
            self.hosts.insert(
                host,
                HostDeskCells {
                    confirmed: rho_desk::cells::Store::new(self.device),
                    view: rho_desk::cells::Store::new(self.device),
                    pending: Vec::new(),
                    buffers: BTreeMap::new(),
                    _subscriptions: Vec::new(),
                    namespace,
                    next_node_counter: 0,
                    syncing: false,
                    poked: None,
                },
            );
        }
        {
            let desk = self.hosts.get_mut(&host)?;
            desk.namespace = namespace;
            desk.syncing = false;
            if let Err(error) = desk.confirmed.merge(delta.clone()) {
                tracing::error!(%error, "Desk cell delta did not merge");
                return None;
            }
            if let Err(error) = desk.view.merge(delta) {
                tracing::error!(%error, "Desk cell delta did not merge into the view");
            }
            desk.prune_pending();
            desk.rebuild_view(self.device);
            desk.next_node_counter = desk
                .view
                .materialize()
                .iter()
                .filter(|node| node.id.replica_id == namespace)
                .map(|node| node.id.counter)
                .max()
                .unwrap_or(0)
                .max(desk.next_node_counter);
        }
        self.merge_texts(host, texts, cx);
        self.reconcile_buffers(host, cx);
        let desk = self.hosts.get_mut(&host)?;
        match desk.poked.take() {
            // The answer already carries everything the poke announced.
            Some(poke) if covers(&frontier, &poke) => None,
            Some(_) => Some(self.sync(host)),
            None => None,
        }
    }

    /// `DeskCellsAvailable`: a poke, not a delta. One handshake is in flight
    /// at a time; a poke that arrives during one is answered after it.
    pub fn cells_available(
        &mut self,
        host: HostId,
        frontier: rho_desk::cells::Version,
    ) -> Option<ClientMessage> {
        let desk = self.hosts.get_mut(&host)?;
        if covers(desk.confirmed.version(), &frontier) {
            return None;
        }
        if desk.syncing {
            desk.poked = Some(frontier);
            return None;
        }
        Some(self.sync(host))
    }

    /// The daemon lost our place in its event stream: start over.
    pub fn resync_required(&mut self, host: HostId) -> ClientMessage {
        if let Some(desk) = self.hosts.get_mut(&host) {
            desk.syncing = false;
            desk.poked = None;
        }
        self.sync(host)
    }

    pub fn mutation_accepted(&mut self, host: HostId, stamp: rho_desk::cells::Stamp) {
        // Nothing to do but note it: the cells are already in the view and
        // the poke that follows brings them into `confirmed`.
        let _ = (host, stamp);
    }

    /// A rejected mutation never happened. The view falls back to the last
    /// merged cells, which is what the reader must see.
    pub fn mutation_rejected(
        &mut self,
        host: HostId,
        stamp: rho_desk::cells::Stamp,
        cx: &mut Context<Workspace>,
    ) {
        let device = self.device;
        let Some(desk) = self.hosts.get_mut(&host) else {
            return;
        };
        desk.pending.retain(|mutation| mutation.stamp != stamp);
        desk.rebuild_view(device);
        self.reconcile_buffers(host, cx);
    }
}

impl DeskCells {
    /// Merges the handshake's text histories. A snapshot never replaces a
    /// newer operation that arrived on its own: the two are queued
    /// independently, so the merge is by operation, not by replacement.
    fn merge_texts(
        &mut self,
        host: HostId,
        texts: Vec<rho_desk::NodeTextSnapshot>,
        cx: &mut Context<Workspace>,
    ) {
        for text in texts {
            let operations = text
                .operations
                .iter()
                .filter_map(|operation| operation.to_text().ok())
                .map(language::Operation::Buffer)
                .collect::<Vec<_>>();
            match self
                .hosts
                .get(&host)
                .and_then(|desk| desk.buffers.get(&text.node_id))
            {
                Some(buffer) => {
                    let buffer = buffer.clone();
                    buffer.update(cx, |buffer, cx| buffer.apply_ops(operations, cx));
                }
                None => {
                    let buffer = self.new_note_buffer(host, text.node_id, operations, cx);
                    if let Some(desk) = self.hosts.get_mut(&host) {
                        desk.buffers.insert(text.node_id, buffer);
                    }
                }
            }
        }
    }

    /// A note's body, as an editor buffer whose local edits go back to the
    /// daemon as text operations.
    fn new_note_buffer(
        &mut self,
        host: HostId,
        node_id: rho_desk::NodeId,
        operations: Vec<language::Operation>,
        cx: &mut Context<Workspace>,
    ) -> Entity<Buffer> {
        let buffer_id = BufferId::new(self.next_buffer_id).expect("nonzero GUI buffer id");
        self.next_buffer_id += 1;
        let namespace = self.hosts.get(&host).map_or(0, |desk| desk.namespace);
        let buffer = cx.new(|cx| {
            let mut buffer = Buffer::remote(
                buffer_id,
                ReplicaId::new(namespace),
                Capability::ReadWrite,
                "",
            );
            buffer.apply_ops(operations, cx);
            buffer
        });
        let subscription = watch_note_buffer(&buffer, host, node_id, cx);
        if let Some(desk) = self.hosts.get_mut(&host) {
            desk._subscriptions.push(subscription);
        }
        buffer
    }

    /// A machine row's title is derived from live metadata rather than from
    /// a text CRDT, so its buffer is local and read-only: nothing it holds
    /// is ever sent to the daemon.
    fn new_derived_buffer(&mut self, cx: &mut Context<Workspace>) -> Entity<Buffer> {
        let buffer_id = BufferId::new(self.next_buffer_id).expect("nonzero GUI buffer id");
        self.next_buffer_id += 1;
        cx.new(|_| Buffer::remote(buffer_id, ReplicaId::new(0), Capability::ReadOnly, ""))
    }

    /// Gives every live node a buffer and drops the buffers of nodes that
    /// are gone. Notes get theirs from the daemon's text history; machine
    /// rows get an empty local one the dashboard fills with a derived title.
    pub fn reconcile_buffers(&mut self, host: HostId, cx: &mut Context<Workspace>) {
        let Some(desk) = self.hosts.get(&host) else {
            return;
        };
        let live = desk
            .view
            .materialize()
            .into_iter()
            .map(|node| (node.id, node.kind))
            .collect::<BTreeMap<_, _>>();
        let stale = desk
            .buffers
            .keys()
            .copied()
            .filter(|node_id| !live.contains_key(node_id))
            .collect::<Vec<_>>();
        let missing = live
            .iter()
            .filter(|(node_id, _)| !desk.buffers.contains_key(node_id))
            .map(|(node_id, kind)| (*node_id, *kind))
            .collect::<Vec<_>>();
        if let Some(desk) = self.hosts.get_mut(&host) {
            for node_id in stale {
                desk.buffers.remove(&node_id);
            }
        }
        for (node_id, kind) in missing {
            let buffer = match kind {
                rho_desk::cells::NodeKind::Note => {
                    self.new_note_buffer(host, node_id, Vec::new(), cx)
                }
                _ => self.new_derived_buffer(cx),
            };
            if let Some(desk) = self.hosts.get_mut(&host) {
                desk.buffers.insert(node_id, buffer);
            }
        }
    }

    /// A text operation from the daemon (another device, or this one echoed
    /// back). Applying an operation the buffer already has is a no-op.
    pub fn text_applied(
        &mut self,
        host: HostId,
        node_id: rho_desk::NodeId,
        operation: rho_desk::TextOperation,
        cx: &mut Context<Workspace>,
    ) {
        let Ok(operation) = operation.to_text() else {
            return;
        };
        let Some(buffer) = self
            .hosts
            .get(&host)
            .and_then(|desk| desk.buffers.get(&node_id))
            .cloned()
        else {
            return;
        };
        buffer.update(cx, |buffer, cx| {
            buffer.apply_ops([language::Operation::Buffer(operation)], cx)
        });
    }

    /// The nodes and buffers the dashboard renders.
    pub fn tree_source(
        &self,
        host: HostId,
    ) -> Option<(
        Vec<rho_desk::cells::MaterializedNode>,
        BTreeMap<rho_desk::NodeId, Entity<Buffer>>,
    )> {
        let desk = self.hosts.get(&host)?;
        Some((desk.view.materialize(), desk.buffers.clone()))
    }

    pub fn node(
        &self,
        host: HostId,
        node_id: rho_desk::NodeId,
    ) -> Option<rho_desk::cells::MaterializedNode> {
        self.hosts
            .get(&host)?
            .view
            .materialize()
            .into_iter()
            .find(|node| node.id == node_id)
    }

    pub fn buffer(&self, host: HostId, node_id: rho_desk::NodeId) -> Option<&Entity<Buffer>> {
        self.hosts.get(&host)?.buffers.get(&node_id)
    }

    /// True while the host has answered a handshake, so callers can tell an
    /// empty desk from one that has not arrived yet.
    pub fn is_synced(&self, host: HostId) -> bool {
        self.hosts.contains_key(&host)
    }

    /// Sends a mutation and shows it at once. The daemon's answer either
    /// confirms it or takes it back.
    pub fn apply(
        &mut self,
        host: HostId,
        writes: Vec<rho_desk::cells::CellWrite>,
        verdict: Option<(rho_desk::NodeId, rho_desk::cells::VerdictEvent)>,
    ) -> Option<ClientMessage> {
        let device = self.device;
        let desk = self.hosts.get_mut(&host)?;
        if writes.is_empty() {
            return None;
        }
        let stamp = desk.next_stamp(device);
        let verdict = verdict.map(|(node, event)| {
            let event = match event {
                rho_desk::cells::VerdictEvent::Applied {
                    verdict, changes, ..
                } => rho_desk::cells::VerdictEvent::Applied {
                    verdict,
                    at: stamp,
                    changes,
                },
                undone => undone,
            };
            (node, event)
        });
        let mutation = rho_desk::cells::CellMutation {
            stamp,
            writes,
            verdict,
        };
        if let Err(error) = desk.view.apply_mutation(&mutation) {
            tracing::error!(%error, "refusing to send an invalid Desk mutation");
            return None;
        }
        desk.pending.push(mutation.clone());
        Some(ClientMessage::DeskMutationApply { mutation })
    }

    /// The next node id in this connection's namespace. Ids from another
    /// namespace are rejected by the daemon, so creation goes through here.
    pub fn new_node_id(&mut self, host: HostId) -> Option<rho_desk::NodeId> {
        let desk = self.hosts.get_mut(&host)?;
        desk.next_node_counter = desk.next_node_counter.checked_add(1)?;
        Some(rho_desk::NodeId {
            replica_id: desk.namespace,
            counter: desk.next_node_counter,
        })
    }

    pub fn namespace(&self, host: HostId) -> Option<u16> {
        Some(self.hosts.get(&host)?.namespace)
    }

    pub fn value(
        &self,
        host: HostId,
        node_id: rho_desk::NodeId,
        field: &rho_desk::cells::Field,
    ) -> Option<rho_desk::cells::Value> {
        self.hosts.get(&host)?.view.value(node_id, field).cloned()
    }

    pub fn verdict_event(
        &self,
        host: HostId,
        node_id: rho_desk::NodeId,
        stamp: rho_desk::cells::Stamp,
    ) -> Option<rho_desk::cells::VerdictEvent> {
        self.hosts
            .get(&host)?
            .view
            .verdict_event(node_id, stamp)
            .cloned()
    }
}

/// A yanked subtree, ready to be pasted as fresh notes. Only notes are
/// captured: machine rows belong to the daemon and are never cloned.
#[derive(Clone, Debug, Default)]
pub struct DeskCapture {
    /// Parents come before their children, so paste can map old ids to new
    /// ones in one pass.
    pub nodes: Vec<DeskCaptureNode>,
}

#[derive(Clone, Debug)]
pub struct DeskCaptureNode {
    pub id: rho_desk::NodeId,
    pub parent: Option<rho_desk::NodeId>,
    pub text: String,
}

/// Structure verbs, as cell writes. Each returns the writes a verb needs;
/// the caller sends them through [`DeskCells::apply`] so one keypress is one
/// mutation.
impl DeskCells {
    /// A new note under `parent`. Every common field is written, because the
    /// daemon rejects a partial creation.
    pub fn create_note_writes(
        &mut self,
        host: HostId,
        parent: Option<rho_desk::NodeId>,
    ) -> Option<(rho_desk::NodeId, Vec<rho_desk::cells::CellWrite>)> {
        let node_id = self.new_node_id(host)?;
        Some((
            node_id,
            create_note_writes(node_id, parent, now_timestamp()),
        ))
    }

    /// The note the cursor sits on gains a sibling or a child. Sibling order
    /// is `(CreatedAt, NodeId)` and `CreatedAt` cannot be rewritten, so a new
    /// row always lands after the existing siblings.
    pub fn new_note_writes(
        &mut self,
        host: HostId,
        relative: rho_desk::NodeId,
        child: bool,
    ) -> Option<(rho_desk::NodeId, Vec<rho_desk::cells::CellWrite>)> {
        let node = self.node(host, relative)?;
        let parent = if child { Some(relative) } else { node.parent };
        self.create_note_writes(host, parent)
    }

    /// Deletes exactly one cell. Live descendants are not tombstoned: the
    /// materializer roots any whose parent chain now crosses a deleted node,
    /// and undoing this one cell puts the hierarchy back.
    pub fn delete_writes(&self, node_id: rho_desk::NodeId) -> Vec<rho_desk::cells::CellWrite> {
        vec![rho_desk::cells::CellWrite {
            node: node_id,
            field: rho_desk::cells::Field::Deleted,
            value: rho_desk::cells::Value::Bool(true),
        }]
    }

    /// Promote or demote: the row keeps its identity and changes parent.
    /// Demoting adopts the previous sibling, promoting joins the grandparent.
    pub fn structure_move_writes(
        &self,
        host: HostId,
        node_id: rho_desk::NodeId,
        demote: bool,
    ) -> Option<Vec<rho_desk::cells::CellWrite>> {
        let nodes = self.hosts.get(&host)?.view.materialize();
        let node = nodes.iter().find(|node| node.id == node_id)?;
        let parent = if demote {
            let previous = nodes
                .iter()
                .filter(|other| other.parent == node.parent && other.id != node_id)
                .filter(|other| (other.created_at, other.id) < (node.created_at, node.id))
                .next_back()?;
            Some(previous.id)
        } else {
            let parent = nodes.iter().find(|other| Some(other.id) == node.parent)?;
            parent.parent
        };
        if parent == node.parent {
            return None;
        }
        Some(vec![parent_write(node_id, parent)])
    }

    /// The row a deleted or emptied one hands the cursor to: the previous
    /// visible row in materialized order.
    pub fn row_above(&self, host: HostId, node_id: rho_desk::NodeId) -> Option<rho_desk::NodeId> {
        let nodes = self.hosts.get(&host)?.view.materialize();
        let index = nodes.iter().position(|node| node.id == node_id)?;
        nodes.get(index.checked_sub(1)?).map(|node| node.id)
    }

    /// Where the cursor lands when a row is deleted: the row above, or the
    /// first row below that does not hang off it. Without this a delete at
    /// the very top leaves the cursor inside the removed excerpt, and the
    /// next structure verb has no row to work from.
    pub fn row_after_delete(
        &self,
        host: HostId,
        node_id: rho_desk::NodeId,
    ) -> Option<rho_desk::NodeId> {
        let nodes = self.hosts.get(&host)?.view.materialize();
        let index = nodes.iter().position(|node| node.id == node_id)?;
        if let Some(above) = index.checked_sub(1).and_then(|above| nodes.get(above)) {
            return Some(above.id);
        }
        let descends = |mut candidate: Option<rho_desk::NodeId>| {
            while let Some(current) = candidate {
                if current == node_id {
                    return true;
                }
                candidate = nodes
                    .iter()
                    .find(|node| node.id == current)
                    .and_then(|node| node.parent);
            }
            false
        };
        nodes
            .get(index + 1..)?
            .iter()
            .find(|node| !descends(Some(node.id)))
            .map(|node| node.id)
    }

    /// True while any live row still calls this node its parent.
    pub fn has_children(&self, host: HostId, node_id: rho_desk::NodeId) -> bool {
        self.hosts.get(&host).is_some_and(|desk| {
            desk.view
                .materialize()
                .iter()
                .any(|node| node.parent == Some(node_id))
        })
    }

    /// The workdir a note is staffed from: its machine-owned `File` child.
    pub fn file_path(
        &self,
        host: HostId,
        node_id: rho_desk::NodeId,
    ) -> Option<camino::Utf8PathBuf> {
        let nodes = self.hosts.get(&host)?.view.materialize();
        crate::dashboard::node_file_path(&nodes, node_id)
    }

    /// Every live note in the subtree, parents first, with its text.
    pub fn capture(
        &self,
        host: HostId,
        root: rho_desk::NodeId,
        cx: &gpui::App,
    ) -> Option<DeskCapture> {
        let desk = self.hosts.get(&host)?;
        let nodes = desk.view.materialize();
        let mut kept: BTreeSet<rho_desk::NodeId> = BTreeSet::new();
        let mut captured = Vec::new();
        for node in &nodes {
            let inside =
                node.id == root || node.parent.is_some_and(|parent| kept.contains(&parent));
            if !inside || node.kind != rho_desk::cells::NodeKind::Note {
                continue;
            }
            kept.insert(node.id);
            captured.push(DeskCaptureNode {
                id: node.id,
                parent: node.parent,
                text: desk
                    .buffers
                    .get(&node.id)
                    .map(|buffer| buffer.read(cx).text())
                    .unwrap_or_default(),
            });
        }
        (!captured.is_empty()).then_some(DeskCapture { nodes: captured })
    }

    /// Re-creates a captured subtree under the cursor's parent. Returns the
    /// new root, the creation writes, and the text each new note wants once
    /// the daemon has accepted them.
    #[allow(clippy::type_complexity)]
    pub fn paste_writes(
        &mut self,
        host: HostId,
        relative: rho_desk::NodeId,
        capture: &DeskCapture,
    ) -> Option<(
        rho_desk::NodeId,
        Vec<rho_desk::cells::CellWrite>,
        Vec<(rho_desk::NodeId, String)>,
    )> {
        let target = self.node(host, relative)?;
        let root_source = capture.nodes.first()?.id;
        let created_at = now_timestamp();
        let mut mapping: BTreeMap<rho_desk::NodeId, rho_desk::NodeId> = BTreeMap::new();
        let mut writes = Vec::new();
        let mut texts = Vec::new();
        for source in &capture.nodes {
            let node_id = self.new_node_id(host)?;
            let parent = if source.id == root_source {
                target.parent
            } else {
                source
                    .parent
                    .and_then(|parent| mapping.get(&parent).copied())
            };
            mapping.insert(source.id, node_id);
            writes.extend(create_note_writes(node_id, parent, created_at));
            if !source.text.is_empty() {
                texts.push((node_id, source.text.clone()));
            }
        }
        let root = mapping.get(&root_source).copied()?;
        Some((root, writes, texts))
    }

    /// The writes that put back whatever these writes are about to replace.
    /// A field with no current value cannot be unset, so undoing a creation
    /// is a delete rather than an inverse (see `dashboard_new_heading`).
    pub fn inverse_writes(
        &self,
        host: HostId,
        writes: &[rho_desk::cells::CellWrite],
    ) -> Vec<rho_desk::cells::CellWrite> {
        let Some(desk) = self.hosts.get(&host) else {
            return Vec::new();
        };
        writes
            .iter()
            .filter_map(|write| {
                let value = desk.view.value(write.node, &write.field)?.clone();
                (value != write.value).then_some(rho_desk::cells::CellWrite {
                    node: write.node,
                    field: write.field.clone(),
                    value,
                })
            })
            .collect()
    }

    /// A dealt verdict: the field it changes, plus the log entry recording
    /// exactly what it changed so an undo can be validated against it.
    pub fn verdict_writes(
        &mut self,
        host: HostId,
        node_id: rho_desk::NodeId,
        verdict: DeskVerdict,
    ) -> Option<(
        Vec<rho_desk::cells::CellWrite>,
        (rho_desk::NodeId, rho_desk::cells::VerdictEvent),
    )> {
        use rho_desk::cells::{Field, Value, Verdict};

        let (verdict, mut writes, change): (Verdict, Vec<rho_desk::cells::CellWrite>, _) =
            match verdict {
                DeskVerdict::Done | DeskVerdict::Dismiss => {
                    let state = if matches!(verdict, DeskVerdict::Done) {
                        rho_desk::cells::State::Done
                    } else {
                        rho_desk::cells::State::Dismissed
                    };
                    let verdict = if matches!(verdict, DeskVerdict::Done) {
                        Verdict::Done
                    } else {
                        Verdict::Dismiss
                    };
                    (
                        verdict,
                        Vec::new(),
                        (node_id, Field::State, Value::State(state)),
                    )
                }
                DeskVerdict::Defer { until } => (
                    Verdict::Defer { until },
                    Vec::new(),
                    (
                        node_id,
                        Field::DeferUntil,
                        Value::OptionalTimestamp(Some(until)),
                    ),
                ),
                DeskVerdict::File { parent } => (
                    Verdict::File { parent },
                    Vec::new(),
                    (node_id, Field::Parent, Value::Parent(Some(parent))),
                ),
                DeskVerdict::Todo { defer_until, pace } => {
                    let (note, mut writes) = self.create_note_writes(host, Some(node_id))?;
                    // The creation already writes every common field, so the
                    // cadence replaces those values rather than repeating them.
                    for write in &mut writes {
                        match write.field {
                            Field::DeferUntil => {
                                write.value = Value::OptionalTimestamp(Some(defer_until));
                            }
                            Field::PaceDays => write.value = Value::Days(pace),
                            _ => {}
                        }
                    }
                    // A todo is the one verdict that writes a whole new note, so
                    // its log entry carries all three cells that make the note a
                    // live cadence, against the values a node that never existed
                    // is read as having. One of them is not enough: the daemon
                    // checks the entry against exactly this shape.
                    let changes = vec![
                        field_change(note, Field::Deleted, Value::Bool(true), Value::Bool(false)),
                        field_change(
                            note,
                            Field::DeferUntil,
                            Value::OptionalTimestamp(None),
                            Value::OptionalTimestamp(Some(defer_until)),
                        ),
                        field_change(note, Field::PaceDays, Value::Days(0), Value::Days(pace)),
                    ];
                    let event = rho_desk::cells::VerdictEvent::Applied {
                        verdict: Verdict::Todo { note },
                        at: rho_desk::cells::Stamp {
                            device: self.device,
                            version: 0,
                        },
                        changes,
                    };
                    return Some((writes, (node_id, event)));
                }
            };
        let (change_node, field, after) = change;
        let before = self
            .hosts
            .get(&host)?
            .view
            .value(change_node, &field)
            .cloned()
            // A note this mutation creates has no prior cell; the verdict log
            // records its arrival as the deletion it was born out of.
            .or_else(|| {
                (field == Field::Deleted && change_node != node_id).then_some(Value::Bool(true))
            });
        if before.as_ref() == Some(&after) {
            return None;
        }
        if !writes
            .iter()
            .any(|write| write.node == change_node && write.field == field)
        {
            writes.push(rho_desk::cells::CellWrite {
                node: change_node,
                field: field.clone(),
                value: after.clone(),
            });
        } else if let Some(write) = writes
            .iter_mut()
            .find(|write| write.node == change_node && write.field == field)
        {
            write.value = after.clone();
        }
        let event = rho_desk::cells::VerdictEvent::Applied {
            verdict,
            at: rho_desk::cells::Stamp {
                device: self.device,
                version: 0,
            },
            changes: vec![rho_desk::cells::FieldChange {
                node: change_node,
                field,
                before,
                after: Some(after),
            }],
        };
        Some((writes, (node_id, event)))
    }

    /// Undoes an applied verdict by reapplying its before-values and
    /// appending the `Undone` log entry the daemon validates against them.
    pub fn undo_verdict_writes(
        &self,
        host: HostId,
        node_id: rho_desk::NodeId,
        at: rho_desk::cells::Stamp,
    ) -> Option<(
        Vec<rho_desk::cells::CellWrite>,
        (rho_desk::NodeId, rho_desk::cells::VerdictEvent),
    )> {
        let rho_desk::cells::VerdictEvent::Applied { changes, .. } =
            self.verdict_event(host, node_id, at)?
        else {
            return None;
        };
        let writes = changes
            .iter()
            .filter_map(|change| {
                Some(rho_desk::cells::CellWrite {
                    node: change.node,
                    field: change.field.clone(),
                    value: change.before.clone()?,
                })
            })
            .collect::<Vec<_>>();
        (!writes.is_empty()).then_some((
            writes,
            (node_id, rho_desk::cells::VerdictEvent::Undone { of: at }),
        ))
    }
}

/// What the dealer asked for, before it becomes cells.
#[derive(Clone, Debug)]
pub enum DeskVerdict {
    Done,
    Dismiss,
    Defer {
        until: rho_desk::cells::Timestamp,
    },
    File {
        parent: rho_desk::NodeId,
    },
    Todo {
        defer_until: rho_desk::cells::Timestamp,
        pace: u32,
    },
}

pub fn now_timestamp() -> rho_desk::cells::Timestamp {
    rho_desk::cells::Timestamp {
        unix_ms: chrono::Utc::now().timestamp_millis(),
        precision: rho_desk::cells::TimestampPrecision::Millisecond,
    }
}

pub fn day_timestamp(date: chrono::NaiveDate) -> rho_desk::cells::Timestamp {
    rho_desk::cells::Timestamp {
        unix_ms: date
            .and_hms_opt(0, 0, 0)
            .map_or(0, |at| at.and_utc().timestamp_millis()),
        precision: rho_desk::cells::TimestampPrecision::Day,
    }
}

fn parent_write(
    node_id: rho_desk::NodeId,
    parent: Option<rho_desk::NodeId>,
) -> rho_desk::cells::CellWrite {
    rho_desk::cells::CellWrite {
        node: node_id,
        field: rho_desk::cells::Field::Parent,
        value: rho_desk::cells::Value::Parent(parent),
    }
}

/// The eight common fields a client must write to create a note.
fn field_change(
    node: rho_desk::NodeId,
    field: rho_desk::cells::Field,
    before: rho_desk::cells::Value,
    after: rho_desk::cells::Value,
) -> rho_desk::cells::FieldChange {
    rho_desk::cells::FieldChange {
        node,
        field,
        before: Some(before),
        after: Some(after),
    }
}

fn create_note_writes(
    node_id: rho_desk::NodeId,
    parent: Option<rho_desk::NodeId>,
    created_at: rho_desk::cells::Timestamp,
) -> Vec<rho_desk::cells::CellWrite> {
    use rho_desk::cells::{Field, Value};

    let write = |field, value| rho_desk::cells::CellWrite {
        node: node_id,
        field,
        value,
    };
    vec![
        write(Field::Kind, Value::Kind(rho_desk::cells::NodeKind::Note)),
        parent_write(node_id, parent),
        write(Field::Deleted, Value::Bool(false)),
        write(Field::CreatedAt, Value::Timestamp(created_at)),
        write(Field::State, Value::State(rho_desk::cells::State::Open)),
        write(Field::DeferUntil, Value::OptionalTimestamp(None)),
        write(Field::Deadline, Value::OptionalTimestamp(None)),
        write(Field::PaceDays, Value::Days(0)),
    ]
}

/// Rewrites a machine row's title. A derived row is not a CRDT: it is
/// replaced wholesale, and its capability keeps the reader from typing into
/// it.
pub(crate) fn write_derived_title(
    buffer: &Entity<Buffer>,
    title: &str,
    cx: &mut Context<Workspace>,
) {
    buffer.update(cx, |buffer, cx| {
        if buffer.text() == title {
            return;
        }
        let end = buffer.len();
        buffer.set_capability(Capability::ReadWrite, cx);
        buffer.edit([(0..end, title)], None, cx);
        buffer.set_capability(Capability::ReadOnly, cx);
    });
}

/// Watches a note body and sends every local edit to the daemon.
fn watch_note_buffer(
    buffer: &Entity<Buffer>,
    host: HostId,
    node_id: rho_desk::NodeId,
    cx: &mut Context<Workspace>,
) -> gpui::Subscription {
    cx.subscribe(buffer, move |workspace, _, event, cx| {
        if let BufferEvent::Operation {
            operation: language::Operation::Buffer(operation),
            is_local: true,
        } = event
        {
            let operation = rho_desk::TextOperation::from_text(operation);
            let timestamp = operation.timestamp();
            workspace.send_desk_text(
                host,
                node_id,
                operation,
                rho_desk::TextTransaction {
                    id: timestamp,
                    edit_ids: vec![timestamp],
                },
                cx,
            );
        }
    })
}

/// This GUI's device identity, persisted once in the client state directory.
///
/// The daemon binds one writer connection per device, and a device's stamps
/// must keep ascending across restarts, so a fresh id every launch would
/// both lock the GUI out of a second window and lose that ordering.
pub fn desk_device() -> rho_desk::cells::DeviceId {
    #[cfg(test)]
    {
        // Tests run several GUIs in one process; each is its own device.
        rho_desk::cells::DeviceId(uuid::Uuid::new_v4().into_bytes())
    }
    #[cfg(not(test))]
    {
        let path = dirs::state_dir().map(|base| base.join("rho").join("desk-device"));
        if let Some(path) = &path
            && let Ok(bytes) = std::fs::read(path)
            && let Ok(bytes) = <[u8; 16]>::try_from(bytes.as_slice())
        {
            return rho_desk::cells::DeviceId(bytes);
        }
        let device = rho_desk::cells::DeviceId(uuid::Uuid::new_v4().into_bytes());
        if let Some(path) = &path {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(error) = std::fs::write(path, device.0) {
                tracing::warn!(%error, "could not persist the Desk device id");
            }
        }
        device
    }
}
