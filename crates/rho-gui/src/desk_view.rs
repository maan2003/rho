//! The window's half of the desk: an editor buffer behind every row the
//! map draws, and the note titles read off them. The store itself — the
//! cells, the map, the sync — is `rho_desk_client::Desk`, which knows
//! nothing of buffers; this holds only what needs a GPUI context.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

use gpui::{AppContext as _, Context, Entity};
use language::{Buffer, BufferEvent, Capability};
use rho_agent_host_proto::desk::cells::{BodySnapshot, Id};
use rho_agents_client::HostId;
use rho_desk_client::Desk;
use rho_desk_client::desk::{DeskCapture, DeskCaptureNode, DeskDelta, DeskNode};
use text::{BufferId, ReplicaId};

use crate::workspace::Workspace;

#[derive(Default)]
struct HostBuffers {
    buffers: BTreeMap<Id, Entity<Buffer>>,
    /// Every note's title, so that naming a row costs a read of this map
    /// rather than a read of the note's rope. A title only changes when
    /// its buffer does, and `refresh_titles` is what notices.
    titles: Rc<HashMap<Id, String>>,
    /// The buffer version each cached title was read at.
    title_versions: HashMap<Id, clock::Global>,
    _subscriptions: Vec<gpui::Subscription>,
}

/// The rows of one host's tree in the order they are drawn, the buffer
/// behind each row that has one, and the title of each. Three parts of
/// one answer: a screen needs all three to draw a row, and they are only
/// consistent together.
pub type TreeSource = (
    Vec<DeskNode>,
    BTreeMap<Id, Entity<Buffer>>,
    Rc<HashMap<Id, String>>,
);

pub struct DeskBuffers {
    next_buffer_id: u64,
    hosts: BTreeMap<HostId, HostBuffers>,
}

impl Default for DeskBuffers {
    fn default() -> Self {
        Self::new()
    }
}

impl DeskBuffers {
    pub fn new() -> Self {
        Self {
            next_buffer_id: 1,
            hosts: BTreeMap::new(),
        }
    }

    /// Drops everything held for a host whose store was replaced: its
    /// buffers carry operations counted in the old one.
    pub fn forget(&mut self, host: HostId) {
        self.hosts.remove(&host);
    }

    /// Applies the words a sync brought to the note buffers, making the
    /// buffers that do not exist yet.
    pub fn merge_bodies(
        &mut self,
        host: HostId,
        desk: &Desk,
        bodies: &[BodySnapshot],
        cx: &mut Context<Workspace>,
    ) {
        for body in bodies {
            let operations = body
                .operations
                .iter()
                .filter_map(|operation| operation.to_text().ok())
                .map(language::Operation::Buffer)
                .collect::<Vec<_>>();
            match self.buffer(host, &body.id) {
                Some(buffer) => {
                    let buffer = buffer.clone();
                    buffer.update(cx, |buffer, cx| buffer.apply_ops(operations, cx));
                }
                None => {
                    let buffer = self.new_note_buffer(host, desk, body.id.clone(), operations, cx);
                    self.host(host).buffers.insert(body.id.clone(), buffer);
                }
            }
        }
    }

    fn host(&mut self, host: HostId) -> &mut HostBuffers {
        self.hosts.entry(host).or_default()
    }

    /// A note's body, as an editor buffer whose local edits go back to the
    /// daemon as text operations.
    fn new_note_buffer(
        &mut self,
        host: HostId,
        desk: &Desk,
        id: Id,
        operations: Vec<language::Operation>,
        cx: &mut Context<Workspace>,
    ) -> Entity<Buffer> {
        let buffer_id = BufferId::new(self.next_buffer_id).expect("nonzero GUI buffer id");
        self.next_buffer_id += 1;
        let namespace = desk.namespace(host).unwrap_or(0);
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
        let subscription = watch_note_buffer(&buffer, host, id, cx);
        self.host(host)._subscriptions.push(subscription);
        buffer
    }

    /// Everything that is not a note has its title derived from its source,
    /// so its buffer is local and read-only: nothing it holds is ever sent
    /// to the daemon.
    fn new_derived_buffer(&mut self, cx: &mut Context<Workspace>) -> Entity<Buffer> {
        let buffer_id = BufferId::new(self.next_buffer_id).expect("nonzero GUI buffer id");
        self.next_buffer_id += 1;
        cx.new(|_| Buffer::remote(buffer_id, ReplicaId::new(0), Capability::ReadOnly, ""))
    }

    fn give_buffer(&mut self, host: HostId, desk: &Desk, id: Id, cx: &mut Context<Workspace>) {
        let buffer = match id {
            Id::Note(_) => self.new_note_buffer(host, desk, id.clone(), Vec::new(), cx),
            _ => self.new_derived_buffer(cx),
        };
        self.host(host).buffers.insert(id, buffer);
    }

    /// Gives the rows a delta named their buffers, and takes back the
    /// buffers of the rows it removed. This is the walk's replacement: a
    /// node gets its buffer when it appears, so nothing has to look at
    /// every node to find out that all of them already have one.
    pub fn give_buffers(
        &mut self,
        host: HostId,
        desk: &Desk,
        delta: &DeskDelta,
        cx: &mut Context<Workspace>,
    ) {
        if delta.is_quiet() || !desk.is_loaded(host) {
            return;
        }
        // A shape that moved can have taken rows away as well as brought
        // them, and which ones is not in the delta: that is the one case
        // the reconcile still answers.
        if delta.shape {
            self.reconcile_buffers(host, desk, cx);
            return;
        }
        let held = self.hosts.get(&host);
        let missing = delta
            .touched
            .iter()
            .filter(|id| {
                desk.has_node(host, id) && !held.is_some_and(|held| held.buffers.contains_key(*id))
            })
            .cloned()
            .collect::<Vec<_>>();
        for id in missing {
            self.give_buffer(host, desk, id, cx);
        }
    }

    /// Gives every shown thing a buffer and drops the buffers of things
    /// that are gone. Notes get theirs from the daemon's body history;
    /// everything else gets an empty local one the dashboard fills with a
    /// derived title.
    pub fn reconcile_buffers(&mut self, host: HostId, desk: &Desk, cx: &mut Context<Workspace>) {
        if !desk.is_loaded(host) {
            return;
        }
        let live = desk
            .nodes(host)
            .iter()
            .map(|node| node.id.clone())
            .collect::<BTreeSet<_>>();
        let held = self.host(host);
        held.buffers.retain(|id, _| live.contains(id));
        let missing = live
            .into_iter()
            .filter(|id| !held.buffers.contains_key(id))
            .collect::<Vec<_>>();
        for id in missing {
            self.give_buffer(host, desk, id, cx);
        }
    }

    /// A body operation from the daemon (another device, or this one echoed
    /// back). Applying an operation the buffer already has is a no-op.
    pub fn text_applied(
        &mut self,
        host: HostId,
        id: &Id,
        operation: rho_agent_host_proto::desk::TextOperation,
        cx: &mut Context<Workspace>,
    ) {
        let Ok(operation) = operation.to_text() else {
            return;
        };
        let Some(buffer) = self.buffer(host, id).cloned() else {
            return;
        };
        buffer.update(cx, |buffer, cx| {
            buffer.apply_ops([language::Operation::Buffer(operation)], cx)
        });
    }

    /// Rereads the titles of the notes whose bodies have moved since the
    /// last pass, and leaves the rest alone. A title is the note's first
    /// line, so the only way it changes is an edit, and comparing versions
    /// costs nothing next to walking a rope.
    fn refresh_titles(&mut self, host: HostId, cx: &gpui::App) {
        let Some(held) = self.hosts.get_mut(&host) else {
            return;
        };
        let mut changed = held.title_versions.len() != held.buffers.len();
        let mut titles = HashMap::with_capacity(held.buffers.len());
        let mut versions = HashMap::with_capacity(held.buffers.len());
        for (id, buffer) in &held.buffers {
            if !matches!(id, Id::Note(_)) {
                continue;
            }
            let version = buffer.read(cx).version();
            let title = match held.title_versions.get(id) {
                Some(seen) if *seen == version => held.titles.get(id).cloned(),
                _ => None,
            };
            let title = match title {
                Some(title) => title,
                None => {
                    changed = true;
                    crate::dashboard::note_title(&buffer.read(cx).text()).to_owned()
                }
            };
            titles.insert(id.clone(), title);
            versions.insert(id.clone(), version);
        }
        if changed || titles.len() != held.titles.len() {
            held.titles = Rc::new(titles);
            held.title_versions = versions;
        }
    }

    /// Every note's title, refreshed. Comparing buffer versions is what
    /// makes asking cheap, so the dealer can ask on every sync: only the
    /// notes whose bodies moved are reread.
    ///
    /// Machine rows are not in here. Their titles are derived from live
    /// metadata, and nothing that ranks or files a card needs them: a
    /// breadcrumb is made of notes.
    pub fn note_titles(
        &mut self,
        host: HostId,
        desk: &Desk,
        cx: &gpui::App,
    ) -> Option<Rc<HashMap<Id, String>>> {
        if !desk.is_loaded(host) {
            return None;
        }
        self.refresh_titles(host, cx);
        Some(
            self.hosts
                .get(&host)
                .map(|held| held.titles.clone())
                .unwrap_or_default(),
        )
    }

    /// What a screen is handed to draw one host's tree.
    pub fn tree_source(&mut self, host: HostId, desk: &Desk, cx: &gpui::App) -> Option<TreeSource> {
        let titles = self.note_titles(host, desk, cx)?;
        let buffers = self
            .hosts
            .get(&host)
            .map(|held| held.buffers.clone())
            .unwrap_or_default();
        Some((desk.nodes(host).to_vec(), buffers, titles))
    }

    pub fn buffer(&self, host: HostId, id: &Id) -> Option<&Entity<Buffer>> {
        self.hosts.get(&host)?.buffers.get(id)
    }

    /// Every note in the subtree, parents first, with its text.
    pub fn capture(
        &self,
        host: HostId,
        desk: &Desk,
        root: &Id,
        cx: &gpui::App,
    ) -> Option<DeskCapture> {
        let mut kept: BTreeSet<Id> = BTreeSet::new();
        let mut captured = Vec::new();
        for node in desk.nodes(host) {
            let inside = &node.id == root
                || node
                    .parent
                    .as_ref()
                    .is_some_and(|parent| kept.contains(parent));
            if !inside || !node.is_note() {
                continue;
            }
            kept.insert(node.id.clone());
            captured.push(DeskCaptureNode {
                text: self
                    .buffer(host, &node.id)
                    .map(|buffer| buffer.read(cx).text())
                    .unwrap_or_default(),
                id: node.id.clone(),
                parent: node.parent.clone(),
            });
        }
        (!captured.is_empty()).then_some(DeskCapture { nodes: captured })
    }
}

/// Rewrites a derived title. A derived row is not a CRDT: it is replaced
/// wholesale, and its capability keeps the reader from typing into it.
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
    id: Id,
    cx: &mut Context<Workspace>,
) -> gpui::Subscription {
    cx.subscribe(buffer, move |workspace, _, event, cx| {
        if let BufferEvent::Operation {
            operation: language::Operation::Buffer(operation),
            is_local: true,
        } = event
        {
            let operation = rho_agent_host_proto::desk::TextOperation::from_text(operation);
            let timestamp = operation.timestamp();
            let transaction = rho_agent_host_proto::desk::TextTransaction {
                id: timestamp,
                edit_ids: vec![timestamp],
            };
            workspace
                .desk
                .keep_text(host, id.clone(), &operation, &transaction);
            workspace.send_desk_text(host, id.clone(), operation, transaction, cx);
        }
    })
}
