//! The Desk: one editable Zed buffer per stable tree node, projected into a
//! single multibuffer in depth-first order.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use editor::display_map::BlockStyle;
use editor::{DisplayElisionId, DisplayElisionProperties, Editor, EditorMode, SizingBehavior};
use gpui::prelude::*;
use gpui::{App, Context, Entity, Focusable as _, Window};
use language::{Buffer, BufferEvent, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey};
use rho_core::ContentPart;
use rho_ui_proto::desk::{
    DeskClock, DeskNode, DeskNodeId, DeskNodeText, DeskOperation, DeskSnapshot,
    DeskStructureOpRecord, DeskTextOpRecord, DeskTransaction,
};
use rho_ui_proto::{AgentId, ClientMessage, WorkstreamId};
use text::{BufferId, ReplicaId};
use ui::div;

use crate::registry::{AgentRegistry, HostId};
use crate::workspace::Workspace;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParsedHeadingState {
    Todo,
    Staffed,
    Done,
    Discarded,
}

pub fn parse_heading_line(text: &str) -> (Option<ParsedHeadingState>, &str) {
    let line = text.lines().next().unwrap_or_default().trim();
    for (keyword, state) in [
        ("TODO", ParsedHeadingState::Todo),
        ("STAFFED", ParsedHeadingState::Staffed),
        ("DONE", ParsedHeadingState::Done),
        ("DISCARDED", ParsedHeadingState::Discarded),
    ] {
        if let Some(title) = line.strip_prefix(keyword)
            && title.starts_with(char::is_whitespace)
        {
            return (Some(state), title.trim_start());
        }
    }
    (None, line)
}

#[derive(Clone, Debug, PartialEq)]
pub enum RowTarget {
    Iris,
    Zulip,
    None,
    Stream {
        workstream_id: WorkstreamId,
        root: Option<AgentId>,
    },
    Agent(AgentId),
    Reply(AgentId),
    NewDraft,
}

struct HostDesk {
    snapshot: DeskSnapshot,
    replica_id: u16,
}

pub struct Dashboard {
    multi_buffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    hosts: BTreeMap<HostId, HostDesk>,
    buffers: HashMap<(HostId, DeskNodeId), Entity<Buffer>>,
    subscriptions: HashMap<(HostId, DeskNodeId), gpui::Subscription>,
    known_text_ops: HashSet<(HostId, DeskNodeId, DeskClock)>,
    collapsed: HashSet<(HostId, DeskNodeId)>,
    collapse_elisions: HashMap<(HostId, DeskNodeId), DisplayElisionId>,
    buffer_nodes: HashMap<BufferId, (HostId, DeskNodeId)>,
    headers_disabled: HashSet<BufferId>,
    displayed_len: usize,
}

impl Dashboard {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let multi_buffer = cx.new(|_| MultiBuffer::without_headers(Capability::ReadWrite));
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: true,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                multi_buffer.clone(),
                #[cfg(feature = "native")]
                None,
                window,
                cx,
            );
            crate::editor_config::configure(&mut editor, window, cx);
            editor.set_mouse_click_selection_enabled(true, cx);
            editor
        });
        Self {
            multi_buffer,
            editor,
            hosts: BTreeMap::new(),
            buffers: HashMap::new(),
            subscriptions: HashMap::new(),
            known_text_ops: HashSet::new(),
            collapsed: HashSet::new(),
            collapse_elisions: HashMap::new(),
            buffer_nodes: HashMap::new(),
            headers_disabled: HashSet::new(),
            displayed_len: 0,
        }
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }

    pub fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle(cx).is_focused(window)
    }

    pub fn apply_snapshot(
        &mut self,
        host: HostId,
        snapshot: DeskSnapshot,
        replica_id: u16,
        cx: &mut Context<Workspace>,
    ) {
        self.buffers.retain(|(candidate, _), _| *candidate != host);
        self.subscriptions
            .retain(|(candidate, _), _| *candidate != host);
        self.buffer_nodes
            .retain(|_, (candidate, _)| *candidate != host);
        self.known_text_ops
            .retain(|(candidate, _, _)| *candidate != host);
        for text in &snapshot.texts {
            for operation in &text.operations {
                self.known_text_ops
                    .insert((host, text.node_id, operation.timestamp()));
            }
        }
        self.hosts.insert(
            host,
            HostDesk {
                snapshot,
                replica_id,
            },
        );
        self.ensure_buffers(host, cx);
        cx.notify();
    }

    pub fn apply_structure(
        &mut self,
        host: HostId,
        record: DeskStructureOpRecord,
        cx: &mut Context<Workspace>,
    ) {
        let Some(desk) = self.hosts.get_mut(&host) else {
            return;
        };
        if desk.snapshot.apply_structure(&record.op).is_err() {
            return;
        }
        desk.snapshot.last_structure_op_id = record.id.0;
        if let Some(undone) = record.undo_of {
            desk.snapshot.undone_structure_ops.push(undone);
        }
        self.ensure_buffers(host, cx);
        cx.notify();
    }

    pub fn apply_text(
        &mut self,
        host: HostId,
        record: DeskTextOpRecord,
        cx: &mut Context<Workspace>,
    ) {
        let key = (host, record.node_id, record.operation.timestamp());
        let already_known = !self.known_text_ops.insert(key);
        let Some(desk) = self.hosts.get_mut(&host) else {
            return;
        };
        let text_index = desk
            .snapshot
            .texts
            .iter()
            .position(|text| text.node_id == record.node_id)
            .unwrap_or_else(|| {
                desk.snapshot.texts.push(DeskNodeText {
                    node_id: record.node_id,
                    operations: Vec::new(),
                    transactions: Vec::new(),
                });
                desk.snapshot.texts.len() - 1
            });
        let text = &mut desk.snapshot.texts[text_index];
        if !already_known {
            text.operations.push(record.operation.clone());
        }
        if let Some(transaction) = record.transaction {
            text.transactions.push(transaction);
        }
        if already_known {
            return;
        }
        if let Some(buffer) = self.buffers.get(&(host, record.node_id)).cloned()
            && let Ok(operation) = record.operation.to_text()
        {
            buffer.update(cx, |buffer, cx| {
                buffer.apply_ops([language::Operation::Buffer(operation)], cx);
            });
        }
    }

    fn ensure_buffers(&mut self, host: HostId, cx: &mut Context<Workspace>) {
        let Some(desk) = self.hosts.get(&host) else {
            return;
        };
        let replica_id = desk.replica_id;
        let nodes = desk.snapshot.nodes.clone();
        let texts = desk
            .snapshot
            .texts
            .iter()
            .map(|text| (text.node_id, text.operations.clone()))
            .collect::<HashMap<_, _>>();
        for node in nodes {
            let key = (host, node.id);
            if self.buffers.contains_key(&key) {
                continue;
            }
            let operations = texts.get(&node.id).cloned().unwrap_or_default();
            let buffer = cx.new(|cx| {
                let mut buffer = Buffer::remote(
                    BufferId::new(node.id.0).expect("nonzero Desk node id"),
                    ReplicaId::new(replica_id),
                    Capability::ReadWrite,
                    "",
                );
                let operations = operations
                    .iter()
                    .filter_map(|operation| operation.to_text().ok())
                    .map(language::Operation::Buffer)
                    .collect::<Vec<_>>();
                buffer.apply_ops(operations, cx);
                buffer
            });
            let subscription = cx.subscribe(&buffer, move |workspace, _, event, _cx| {
                let BufferEvent::Operation {
                    operation: language::Operation::Buffer(operation),
                    is_local: true,
                } = event
                else {
                    return;
                };
                let operation = DeskOperation::from_text(operation);
                let timestamp = operation.timestamp();
                workspace.mark_desk_text_local(host, node.id, timestamp);
                workspace.send_to_host(
                    host,
                    ClientMessage::DeskTextApply {
                        node_id: node.id,
                        operation,
                        transaction: Some(DeskTransaction {
                            id: timestamp,
                            edit_ids: vec![timestamp],
                        }),
                    },
                );
            });
            let buffer_id = buffer.read(cx).remote_id();
            self.buffer_nodes.insert(buffer_id, key);
            self.buffers.insert(key, buffer);
            self.subscriptions.insert(key, subscription);
        }
        let live = self
            .hosts
            .get(&host)
            .into_iter()
            .flat_map(|desk| desk.snapshot.nodes.iter().map(|node| node.id))
            .collect::<HashSet<_>>();
        self.buffers
            .retain(|(candidate, node), _| *candidate != host || live.contains(node));
        self.subscriptions
            .retain(|(candidate, node), _| *candidate != host || live.contains(node));
        self.buffer_nodes
            .retain(|_, (candidate, node)| *candidate != host || live.contains(node));
    }

    pub fn mark_local_text_op(&mut self, host: HostId, node_id: DeskNodeId, clock: DeskClock) {
        self.known_text_ops.insert((host, node_id, clock));
    }

    pub fn sync(
        &mut self,
        _registry: &AgentRegistry,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let mut projection = Projection::default();
        for (&host, desk) in &self.hosts {
            project_depth_first(
                &desk.snapshot.nodes,
                None,
                &self.collapsed,
                host,
                false,
                &mut projection,
            );
        }
        let order = projection.order;
        let live_buffers = order
            .iter()
            .filter_map(|key| self.buffers.get(key))
            .map(|buffer| buffer.read(cx).remote_id())
            .collect::<Vec<_>>();
        let new_headers = live_buffers
            .iter()
            .copied()
            .filter(|id| !self.headers_disabled.contains(id))
            .collect::<Vec<_>>();
        self.editor.update(cx, |editor, cx| {
            for id in &new_headers {
                editor.disable_header_for_buffer(*id, cx);
            }
        });
        self.headers_disabled.extend(new_headers);
        let old_len = self.displayed_len;
        self.multi_buffer.update(cx, |multi_buffer, cx| {
            for (index, key) in order.iter().enumerate() {
                if let Some(buffer) = self.buffers.get(key) {
                    multi_buffer.set_excerpts_for_path(
                        PathKey::sorted(index as u64),
                        buffer.clone(),
                        [Point::zero()..buffer.read(cx).max_point()],
                        0,
                        cx,
                    );
                }
            }
            for stale in order.len()..old_len {
                multi_buffer.remove_excerpts(PathKey::sorted(stale as u64), cx);
            }
        });
        self.displayed_len = order.len();
        self.sync_collapse_elisions(&order, projection.collapsed, cx);
    }

    fn sync_collapse_elisions(
        &mut self,
        order: &[(HostId, DeskNodeId)],
        collapsed: Vec<((HostId, DeskNodeId), std::ops::Range<usize>)>,
        cx: &mut Context<Workspace>,
    ) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let mut properties = Vec::new();
        for (key, range) in collapsed {
            let Some(first) = self.buffers.get(&order[range.start]) else {
                continue;
            };
            let Some(last) = self.buffers.get(&order[range.end - 1]) else {
                continue;
            };
            let Some(start) = snapshot.anchor_in_excerpt(first.read(cx).anchor_before(0)) else {
                continue;
            };
            let Some(end) =
                snapshot.anchor_in_excerpt(last.read(cx).anchor_after(last.read(cx).len()))
            else {
                continue;
            };
            let count = range.len();
            properties.push((
                key,
                DisplayElisionProperties {
                    range: start..end,
                    tail_rows: 0,
                    height: Some(1),
                    style: BlockStyle::Flex,
                    render: Arc::new(move |_| {
                        div()
                            .pl_2()
                            .child(format!("▸ {count} nested"))
                            .into_any_element()
                    }),
                    priority: 0,
                    type_tag: None,
                },
            ));
        }

        let live = properties
            .iter()
            .map(|(key, _)| *key)
            .collect::<HashSet<_>>();
        let removed = self
            .collapse_elisions
            .extract_if(|key, _| !live.contains(key))
            .map(|(_, id)| id)
            .collect::<Vec<_>>();
        if !removed.is_empty() {
            self.editor.update(cx, |editor, cx| {
                editor.remove_display_elisions(removed.into_iter().collect(), None, cx)
            });
        }

        let mut updates = Vec::new();
        let mut inserts = Vec::new();
        let mut insert_keys = Vec::new();
        for (key, property) in properties {
            if let Some(id) = self.collapse_elisions.get(&key).copied() {
                updates.push((id, property));
            } else {
                insert_keys.push(key);
                inserts.push(property);
            }
        }
        if !updates.is_empty() {
            self.editor.update(cx, |editor, cx| {
                editor.update_display_elisions(updates, None, cx)
            });
        }
        if !inserts.is_empty() {
            let ids = self.editor.update(cx, |editor, cx| {
                editor.insert_display_elisions(inserts, None, cx)
            });
            self.collapse_elisions
                .extend(insert_keys.into_iter().zip(ids));
        }
    }

    fn cursor_node(&self, cx: &mut Context<Workspace>) -> Option<(HostId, DeskNodeId)> {
        let buffer_id = self.editor.update(cx, |editor, cx| {
            let head = editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx))
                .head();
            editor
                .buffer()
                .read(cx)
                .snapshot(cx)
                .point_to_buffer_offset(head)
                .map(|(buffer, _)| buffer.remote_id())
        })?;
        self.buffer_nodes.get(&buffer_id).copied()
    }

    pub fn hint(&self, cx: &mut Context<Workspace>) -> &'static str {
        let on_heading = self.editor.update(cx, |editor, cx| {
            let head = editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx))
                .head();
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            snapshot
                .point_to_buffer_offset(head)
                .map(|(buffer, offset)| buffer.offset_to_point(offset.0).row == 0)
                .unwrap_or(false)
        });
        if on_heading {
            "Tab fold · Enter open · s staff · d done · x discard · u undo · g jump · n NOW · b back"
        } else {
            "edit text · Esc heading verbs"
        }
    }

    pub fn cursor_target(&self, cx: &mut Context<Workspace>) -> Option<RowTarget> {
        let (host, node_id) = self.cursor_node(cx)?;
        self.hosts
            .get(&host)?
            .snapshot
            .bindings
            .iter()
            .find(|binding| binding.node_id == node_id && !binding.orphaned)
            .map(|binding| RowTarget::Agent(binding.agent_id))
            .or(Some(RowTarget::None))
    }

    pub fn toggle_subagents(&mut self, cx: &mut Context<Workspace>) -> bool {
        let Some(node) = self.cursor_node(cx) else {
            return false;
        };
        let has_children = self.hosts.get(&node.0).is_some_and(|desk| {
            desk.snapshot
                .nodes
                .iter()
                .any(|candidate| candidate.parent == Some(node.1))
        });
        if !has_children {
            return false;
        }
        if !self.collapsed.remove(&node) {
            self.collapsed.insert(node);
        }
        cx.notify();
        true
    }

    pub(crate) fn toggle_subagents_for(
        &mut self,
        _parent: AgentId,
        _cx: &mut Context<Workspace>,
    ) -> bool {
        false
    }

    pub fn cursor_to_agent(
        &mut self,
        _agent_id: AgentId,
        _workstream_id: WorkstreamId,
        _cx: &mut Context<Workspace>,
    ) {
    }

    pub fn open_reply(&mut self, _agent_id: AgentId, _cx: &mut Context<Workspace>) {}
    pub fn open_new_draft(&mut self, _summary: String, _cx: &mut Context<Workspace>) {}
    pub fn take_new_draft(&mut self, _cx: &mut Context<Workspace>) -> Option<Vec<ContentPart>> {
        None
    }
    pub fn take_reply(
        &mut self,
        _agent_id: AgentId,
        _cx: &mut Context<Workspace>,
    ) -> Option<Vec<ContentPart>> {
        None
    }
    pub fn accepts_attachments(&self, _cx: &mut Context<Workspace>) -> bool {
        false
    }
    pub fn clear_attachments(&mut self, _cx: &mut Context<Workspace>) -> bool {
        false
    }
    pub fn add_image(
        &mut self,
        _media_type: String,
        _data: Vec<u8>,
        _cx: &mut Context<Workspace>,
    ) -> bool {
        false
    }

    #[cfg(test)]
    pub(crate) fn fold_count(&self) -> usize {
        self.collapsed.len()
    }
    #[cfg(test)]
    pub(crate) fn rail_tail_id(&self) -> Option<editor::DisplayElisionId> {
        None
    }
    #[cfg(test)]
    pub(crate) fn rail_tail_ends_in_reply(&self, _agent_id: AgentId) -> bool {
        false
    }
}

#[derive(Default)]
struct Projection {
    order: Vec<(HostId, DeskNodeId)>,
    collapsed: Vec<((HostId, DeskNodeId), std::ops::Range<usize>)>,
}

fn project_depth_first(
    nodes: &[DeskNode],
    parent: Option<DeskNodeId>,
    collapsed: &HashSet<(HostId, DeskNodeId)>,
    host: HostId,
    ancestor_collapsed: bool,
    projection: &mut Projection,
) {
    let mut children = nodes
        .iter()
        .filter(|node| node.parent == parent)
        .collect::<Vec<_>>();
    children.sort_by(|left, right| left.order.cmp(&right.order));
    for node in children {
        projection.order.push((host, node.id));
        let descendants_start = projection.order.len();
        let is_collapsed = collapsed.contains(&(host, node.id));
        project_depth_first(
            nodes,
            Some(node.id),
            collapsed,
            host,
            ancestor_collapsed || is_collapsed,
            projection,
        );
        let descendants_end = projection.order.len();
        if !ancestor_collapsed && is_collapsed && descendants_start != descendants_end {
            projection
                .collapsed
                .push(((host, node.id), descendants_start..descendants_end));
        }
    }
}

#[cfg(test)]
mod tests {
    use rho_ui_proto::desk::DeskOrderKey;

    use super::*;

    #[test]
    fn heading_state_comes_only_from_first_line() {
        assert_eq!(
            parse_heading_line("TODO ship Desk\nDONE is body text"),
            (Some(ParsedHeadingState::Todo), "ship Desk")
        );
        assert_eq!(parse_heading_line("TODOish title"), (None, "TODOish title"));
    }

    #[test]
    fn tree_projects_in_depth_first_order_and_marks_collapsed_subtrees() {
        let root = DeskNodeId(1);
        let child = DeskNodeId(2);
        let sibling = DeskNodeId(3);
        let nodes = vec![
            DeskNode {
                id: sibling,
                parent: None,
                order: DeskOrderKey(vec![192]),
            },
            DeskNode {
                id: child,
                parent: Some(root),
                order: DeskOrderKey::first(),
            },
            DeskNode {
                id: root,
                parent: None,
                order: DeskOrderKey::first(),
            },
        ];
        let host = HostId::default();
        let mut projection = Projection::default();
        project_depth_first(
            &nodes,
            None,
            &HashSet::from([(host, root)]),
            host,
            false,
            &mut projection,
        );
        assert_eq!(
            projection.order,
            vec![(host, root), (host, child), (host, sibling)]
        );
        assert_eq!(projection.collapsed, vec![((host, root), 1..2)]);
    }
}
