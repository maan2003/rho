//! The Desk: one editable Zed buffer per stable tree node, projected into a
//! single multibuffer in depth-first order.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use editor::display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId};
use editor::{Editor, EditorMode, HighlightKey, Inlay, InlayHighlight, SizingBehavior};
use gpui::prelude::*;
use gpui::{App, Context, Entity, Focusable as _, Window};
use language::{Buffer, BufferEvent, Capability, InlayId, Point};
use multi_buffer::{MultiBuffer, PathKey};
use rho_ui_proto::desk::{
    DeskBinding, DeskClock, DeskNode, DeskNodeId, DeskNodeText, DeskOperation, DeskSnapshot,
    DeskStructureOpRecord, DeskTextOpRecord, DeskTransaction,
};
use rho_ui_proto::{AgentId, ClientMessage};
use text::{BufferId, ReplicaId};
use theme::ActiveTheme as _;
use ui::div;

use crate::registry::{AgentRegistry, HostId};
use crate::workspace::Workspace;

const DEPTH_RAIL_KEY: HighlightKey = HighlightKey::SyntaxTreeView(usize::MAX - 1);

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
    None,
    Agent(AgentId),
}

struct HostDesk {
    snapshot: DeskSnapshot,
    replica_id: u16,
}

#[derive(Clone)]
struct NowItem {
    host: HostId,
    node_id: DeskNodeId,
    agent_id: AgentId,
    attention: rho_ui_proto::UiAttention,
    last_active: rho_core::UnixMs,
    title: String,
}

struct DeskCaret {
    anchor: text::Anchor,
    collapsed: HashSet<(HostId, DeskNodeId)>,
}

struct HeadingEntry {
    label: String,
    description: String,
    host: HostId,
    node_id: DeskNodeId,
}

pub struct Dashboard {
    multi_buffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    hosts: BTreeMap<HostId, HostDesk>,
    buffers: HashMap<(HostId, DeskNodeId), Entity<Buffer>>,
    subscriptions: HashMap<(HostId, DeskNodeId), gpui::Subscription>,
    known_text_ops: HashSet<(HostId, DeskNodeId, DeskClock)>,
    collapsed: HashSet<(HostId, DeskNodeId)>,
    collapse_blocks: Vec<CustomBlockId>,
    host_header_blocks: Vec<CustomBlockId>,
    depth_inlays: Vec<InlayId>,
    buffer_nodes: HashMap<BufferId, (HostId, DeskNodeId)>,
    headers_disabled: HashSet<BufferId>,
    displayed_len: usize,
    next_buffer_id: u64,
    now_items: Vec<NowItem>,
    now_block: Option<CustomBlockId>,
    now_cursor: Option<AgentId>,
    caret_stack: Vec<DeskCaret>,
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
            collapse_blocks: Vec::new(),
            host_header_blocks: Vec::new(),
            depth_inlays: Vec::new(),
            buffer_nodes: HashMap::new(),
            headers_disabled: HashSet::new(),
            displayed_len: 0,
            next_buffer_id: 1,
            now_items: Vec::new(),
            now_block: None,
            now_cursor: None,
            caret_stack: Vec::new(),
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

    pub fn apply_binding(&mut self, host: HostId, binding: DeskBinding) {
        let Some(desk) = self.hosts.get_mut(&host) else {
            return;
        };
        desk.snapshot.bindings.retain(|candidate| {
            candidate.node_id != binding.node_id && candidate.agent_id != binding.agent_id
        });
        desk.snapshot.bindings.push(binding);
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
            let buffer_id = BufferId::new(self.next_buffer_id).expect("nonzero GUI buffer id");
            self.next_buffer_id = self
                .next_buffer_id
                .checked_add(1)
                .expect("GUI buffer ids exhausted");
            let buffer = cx.new(|cx| {
                let mut buffer = Buffer::remote(
                    buffer_id,
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
        registry: &AgentRegistry,
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
                0,
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
        self.sync_now_strip(registry, &order, cx);
        self.sync_host_headers(registry, &order, cx);
        self.sync_node_chrome(registry, &order, &projection.depths, cx);
        self.sync_collapse_blocks(&order, projection.collapsed, cx);
    }

    fn sync_now_strip(
        &mut self,
        registry: &AgentRegistry,
        order: &[(HostId, DeskNodeId)],
        cx: &mut Context<Workspace>,
    ) {
        self.now_items = order
            .iter()
            .filter_map(|(host, node_id)| {
                let desk = self.hosts.get(host)?;
                let binding = desk
                    .snapshot
                    .bindings
                    .iter()
                    .find(|binding| binding.node_id == *node_id && !binding.orphaned)?;
                let attention = registry.attention(binding.agent_id);
                if attention < rho_ui_proto::UiAttention::Pending {
                    return None;
                }
                let last_active = registry.agent_last_active(binding.agent_id)?;
                let buffer = self.buffers.get(&(*host, *node_id))?.read(cx);
                let text = buffer.text_for_range(0..buffer.len()).collect::<String>();
                let (_, title) = parse_heading_line(&text);
                Some(NowItem {
                    host: *host,
                    node_id: *node_id,
                    agent_id: binding.agent_id,
                    attention,
                    last_active,
                    title: title.to_owned(),
                })
            })
            .collect();
        self.now_items
            .sort_by_key(|item| (Reverse(item.last_active), item.agent_id));
        if self
            .now_cursor
            .is_some_and(|agent| !self.now_items.iter().any(|item| item.agent_id == agent))
        {
            self.now_cursor = None;
        }

        if let Some(block) = self.now_block.take() {
            self.editor.update(cx, |editor, cx| {
                editor.remove_blocks([block].into_iter().collect(), None, cx)
            });
        }
        let Some((first_host, first_node)) = order.first().copied() else {
            return;
        };
        if self.now_items.is_empty() {
            return;
        }
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let Some(position) = self
            .buffers
            .get(&(first_host, first_node))
            .and_then(|buffer| snapshot.anchor_in_excerpt(buffer.read(cx).anchor_before(0)))
        else {
            return;
        };
        let lines = self
            .now_items
            .iter()
            .map(|item| {
                let reason = match item.attention {
                    rho_ui_proto::UiAttention::NeedsInput => "needs input",
                    _ => "pending response",
                };
                format!(
                    "{} · {} · {reason}",
                    item.title,
                    registry.agent_human_name(item.agent_id)
                )
            })
            .collect::<Vec<_>>();
        let height = u32::try_from(lines.len()).unwrap_or(u32::MAX);
        self.now_block = self
            .editor
            .update(cx, |editor, cx| {
                editor.insert_blocks(
                    [BlockProperties {
                        placement: BlockPlacement::Above(position),
                        height: Some(height),
                        style: BlockStyle::Sticky,
                        render: Arc::new(move |cx| {
                            div()
                                .w_full()
                                .flex()
                                .flex_col()
                                .border_b_1()
                                .border_color(cx.app.theme().colors().border_variant)
                                .children(
                                    lines
                                        .iter()
                                        .cloned()
                                        .map(|line| div().h_6().text_ellipsis().child(line)),
                                )
                                .into_any_element()
                        }),
                        priority: 2,
                    }],
                    None,
                    cx,
                )
            })
            .into_iter()
            .next();
    }

    fn sync_host_headers(
        &mut self,
        registry: &AgentRegistry,
        order: &[(HostId, DeskNodeId)],
        cx: &mut Context<Workspace>,
    ) {
        if !self.host_header_blocks.is_empty() {
            self.editor.update(cx, |editor, cx| {
                editor.remove_blocks(
                    std::mem::take(&mut self.host_header_blocks)
                        .into_iter()
                        .collect(),
                    None,
                    cx,
                );
            });
        }

        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let mut seen = HashSet::new();
        let properties = order
            .iter()
            .filter(|(host, _)| seen.insert(*host))
            .filter_map(|(host, node)| {
                let buffer = self.buffers.get(&(*host, *node))?;
                let position = snapshot.anchor_in_excerpt(buffer.read(cx).anchor_before(0))?;
                let name = match registry.host_name(*host) {
                    "" => format!("Host {}", host.0 + 1),
                    name => name.to_owned(),
                };
                Some(BlockProperties {
                    placement: BlockPlacement::Above(position),
                    height: Some(1),
                    style: BlockStyle::Flex,
                    render: Arc::new(move |cx| {
                        div()
                            .w_full()
                            .pt_2()
                            .pb_1()
                            .text_color(cx.app.theme().colors().text_muted)
                            .child(format!("{name} · Desk"))
                            .into_any_element()
                    }),
                    priority: 1,
                })
            })
            .collect::<Vec<_>>();
        if !properties.is_empty() {
            self.host_header_blocks = self
                .editor
                .update(cx, |editor, cx| editor.insert_blocks(properties, None, cx));
        }
    }

    fn sync_node_chrome(
        &mut self,
        registry: &AgentRegistry,
        order: &[(HostId, DeskNodeId)],
        depths: &[usize],
        cx: &mut Context<Workspace>,
    ) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let mut inlays = Vec::new();
        let mut highlights = Vec::new();
        for (key, depth) in order.iter().zip(depths).filter(|(_, depth)| **depth > 0) {
            let Some(buffer) = self.buffers.get(key) else {
                continue;
            };
            let buffer_snapshot = buffer.read(cx).snapshot();
            for row in 0..=buffer_snapshot.max_point().row {
                let Some(position) =
                    snapshot.anchor_in_excerpt(buffer_snapshot.anchor_before(Point::new(row, 0)))
                else {
                    continue;
                };
                let text = if row == 0 {
                    format!("{}├─ ", "│  ".repeat(depth - 1))
                } else {
                    "│  ".repeat(*depth)
                };
                let inlay = Inlay::custom(inlays.len(), position, text.clone());
                highlights.push(InlayHighlight {
                    inlay: inlay.id,
                    inlay_position: position,
                    range: 0..text.len(),
                });
                inlays.push(inlay);
            }
        }
        for (host, node_id) in order {
            let Some(binding) = self.hosts.get(host).and_then(|desk| {
                desk.snapshot
                    .bindings
                    .iter()
                    .find(|binding| binding.node_id == *node_id && !binding.orphaned)
            }) else {
                continue;
            };
            let Some(buffer) = self.buffers.get(&(*host, *node_id)) else {
                continue;
            };
            let buffer_snapshot = buffer.read(cx).snapshot();
            let heading_end = buffer_snapshot
                .text_for_range(0..buffer_snapshot.len())
                .collect::<String>()
                .find('\n')
                .unwrap_or(buffer_snapshot.len());
            let Some(position) =
                snapshot.anchor_in_excerpt(buffer_snapshot.anchor_before(heading_end))
            else {
                continue;
            };
            let attention = match registry.attention(binding.agent_id) {
                rho_ui_proto::UiAttention::Quiet => "idle",
                rho_ui_proto::UiAttention::Working => "working",
                rho_ui_proto::UiAttention::Pending => "pending",
                rho_ui_proto::UiAttention::NeedsInput => "needs you",
            };
            let text = format!(
                "  · {} · {attention}",
                registry.agent_human_name(binding.agent_id)
            );
            let inlay = Inlay::custom(inlays.len(), position, text.clone());
            highlights.push(InlayHighlight {
                inlay: inlay.id,
                inlay_position: position,
                range: 0..text.len(),
            });
            inlays.push(inlay);
        }

        let removed = std::mem::take(&mut self.depth_inlays);
        self.depth_inlays = inlays.iter().map(|inlay| inlay.id).collect();
        self.editor.update(cx, |editor, cx| {
            editor.splice_inlays(&removed, inlays, cx);
            editor.clear_highlights(DEPTH_RAIL_KEY, cx);
            editor.highlight_inlays(
                DEPTH_RAIL_KEY,
                highlights,
                gpui::HighlightStyle {
                    color: Some(cx.theme().colors().border_variant.into()),
                    ..Default::default()
                },
                cx,
            );
        });
    }

    fn sync_collapse_blocks(
        &mut self,
        order: &[(HostId, DeskNodeId)],
        collapsed: Vec<((HostId, DeskNodeId), std::ops::Range<usize>)>,
        cx: &mut Context<Workspace>,
    ) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let properties = collapsed
            .into_iter()
            .filter_map(|(_, range)| {
                let Some(first) = self.buffers.get(&order[range.start]) else {
                    return None;
                };
                let Some(last) = self.buffers.get(&order[range.end - 1]) else {
                    return None;
                };
                let Some(start) = snapshot.anchor_in_excerpt(first.read(cx).anchor_before(0))
                else {
                    return None;
                };
                let Some(end) =
                    snapshot.anchor_in_excerpt(last.read(cx).anchor_after(last.read(cx).len()))
                else {
                    return None;
                };
                let count = range.len();
                Some(BlockProperties {
                    placement: BlockPlacement::Replace(start..=end),
                    height: Some(1),
                    style: BlockStyle::Flex,
                    render: Arc::new(move |_| {
                        div()
                            .pl_2()
                            .child(format!("▸ {count} nested"))
                            .into_any_element()
                    }),
                    priority: 0,
                })
            })
            .collect::<Vec<_>>();

        if !self.collapse_blocks.is_empty() {
            self.editor.update(cx, |editor, cx| {
                editor.remove_blocks(
                    std::mem::take(&mut self.collapse_blocks)
                        .into_iter()
                        .collect(),
                    None,
                    cx,
                )
            });
        }
        if !properties.is_empty() {
            self.collapse_blocks = self
                .editor
                .update(cx, |editor, cx| editor.insert_blocks(properties, None, cx));
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

    fn caret(&self, cx: &mut Context<Workspace>) -> Option<text::Anchor> {
        self.editor.update(cx, |editor, cx| {
            let head = editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx))
                .head();
            editor
                .buffer()
                .read(cx)
                .snapshot(cx)
                .point_to_buffer_offset(head)
                .map(|(buffer, offset)| buffer.anchor_after(offset.0))
        })
    }

    fn move_to_anchor(
        &self,
        anchor: text::Anchor,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let Some(anchor) = self
            .multi_buffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(anchor)
        else {
            return false;
        };
        self.editor.update(cx, |editor, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_anchor_ranges([anchor..anchor]);
            });
        });
        true
    }

    pub fn next_now(
        &mut self,
        registry: &AgentRegistry,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Option<AgentId> {
        let index = self
            .now_cursor
            .and_then(|agent| {
                self.now_items
                    .iter()
                    .position(|item| item.agent_id == agent)
            })
            .map(|index| (index + 1) % self.now_items.len())
            .unwrap_or(0);
        let item = self.now_items.get(index)?.clone();
        if let Some(anchor) = self.caret(cx) {
            self.caret_stack.push(DeskCaret {
                anchor,
                collapsed: self.collapsed.clone(),
            });
        }
        let desk = self.hosts.get(&item.host)?;
        let mut current = desk
            .snapshot
            .node(item.node_id)
            .and_then(|node| node.parent);
        while let Some(parent) = current {
            self.collapsed.remove(&(item.host, parent));
            current = desk.snapshot.node(parent).and_then(|node| node.parent);
        }
        self.sync(registry, window, cx);
        let buffer = self.buffers.get(&(item.host, item.node_id))?.read(cx);
        if !self.move_to_anchor(buffer.anchor_before(0), window, cx) {
            return None;
        }
        self.now_cursor = Some(item.agent_id);
        Some(item.agent_id)
    }

    pub fn back(
        &mut self,
        registry: &AgentRegistry,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let Some(caret) = self.caret_stack.pop() else {
            return false;
        };
        self.collapsed = caret.collapsed;
        self.sync(registry, window, cx);
        self.move_to_anchor(caret.anchor, window, cx)
    }

    fn heading_entries(&self, registry: &AgentRegistry, cx: &App) -> Vec<HeadingEntry> {
        let mut entries = Vec::new();
        for (&host, desk) in &self.hosts {
            let mut order = Vec::new();
            let mut projection = Projection::default();
            project_depth_first(
                &desk.snapshot.nodes,
                None,
                &HashSet::new(),
                host,
                0,
                false,
                &mut projection,
            );
            order.extend(projection.order);
            for (_, node_id) in order {
                let Some(buffer) = self.buffers.get(&(host, node_id)) else {
                    continue;
                };
                let buffer = buffer.read(cx);
                let text = buffer.text_for_range(0..buffer.len()).collect::<String>();
                let (_, title) = parse_heading_line(&text);
                let title = if title.is_empty() {
                    "(untitled)"
                } else {
                    title
                };
                entries.push(HeadingEntry {
                    label: format!("{title} · {} · #{}", registry.host_name(host), node_id.0),
                    description: format!("{} · node {}", registry.host_name(host), node_id.0),
                    host,
                    node_id,
                });
            }
        }
        entries
    }

    pub fn heading_candidates(
        &self,
        registry: &AgentRegistry,
        input: &str,
        cx: &App,
    ) -> Vec<(String, String)> {
        self.heading_entries(registry, cx)
            .into_iter()
            .filter(|entry| fuzzy_contains(&entry.label, input))
            .map(|entry| (entry.label, entry.description))
            .collect()
    }

    pub fn jump_to_heading(
        &mut self,
        label: &str,
        registry: &AgentRegistry,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let Some(entry) = self
            .heading_entries(registry, cx)
            .into_iter()
            .find(|entry| entry.label == label)
        else {
            return false;
        };
        let Some(desk) = self.hosts.get(&entry.host) else {
            return false;
        };
        let mut current = desk
            .snapshot
            .node(entry.node_id)
            .and_then(|node| node.parent);
        while let Some(parent) = current {
            self.collapsed.remove(&(entry.host, parent));
            current = desk.snapshot.node(parent).and_then(|node| node.parent);
        }
        self.sync(registry, window, cx);
        let Some(buffer) = self.buffers.get(&(entry.host, entry.node_id)) else {
            return false;
        };
        let anchor = buffer.read(cx).anchor_before(0);
        self.move_to_anchor(anchor, window, cx)
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

    pub fn staffing_target(
        &self,
        cx: &mut Context<Workspace>,
    ) -> Option<(HostId, DeskNodeId, String)> {
        let (host, node_id) = self.cursor_node(cx)?;
        let desk = self.hosts.get(&host)?;
        if desk
            .snapshot
            .bindings
            .iter()
            .any(|binding| binding.node_id == node_id && !binding.orphaned)
        {
            return None;
        }
        let mut parts = Vec::new();
        for path_node in node_path(&desk.snapshot.nodes, node_id)? {
            let buffer = self.buffers.get(&(host, path_node))?.read(cx);
            parts.push(buffer.text_for_range(0..buffer.len()).collect::<String>());
        }
        let text = parts.join("\n\n");
        Some((host, node_id, text))
    }

    pub fn mark_staffed(&mut self, host: HostId, node_id: DeskNodeId, cx: &mut Context<Workspace>) {
        let Some(buffer) = self.buffers.get(&(host, node_id)).cloned() else {
            return;
        };
        buffer.update(cx, |buffer, cx| {
            let text = buffer.text_for_range(0..buffer.len()).collect::<String>();
            let heading_end = text.find('\n').unwrap_or(text.len());
            let (_, title) = parse_heading_line(&text);
            let heading = if title.is_empty() {
                "STAFFED".to_owned()
            } else {
                format!("STAFFED {title}")
            };
            if text[..heading_end] != heading {
                buffer.edit([(0..heading_end, heading)], None, cx);
            }
        });
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
}

#[derive(Default)]
struct Projection {
    order: Vec<(HostId, DeskNodeId)>,
    depths: Vec<usize>,
    collapsed: Vec<((HostId, DeskNodeId), std::ops::Range<usize>)>,
}

fn project_depth_first(
    nodes: &[DeskNode],
    parent: Option<DeskNodeId>,
    collapsed: &HashSet<(HostId, DeskNodeId)>,
    host: HostId,
    depth: usize,
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
        projection.depths.push(depth);
        let descendants_start = projection.order.len();
        let is_collapsed = collapsed.contains(&(host, node.id));
        project_depth_first(
            nodes,
            Some(node.id),
            collapsed,
            host,
            depth + 1,
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

fn node_path(nodes: &[DeskNode], node_id: DeskNodeId) -> Option<Vec<DeskNodeId>> {
    let mut path = Vec::new();
    let mut current = Some(node_id);
    while let Some(node_id) = current {
        let node = nodes.iter().find(|node| node.id == node_id)?;
        path.push(node.id);
        current = node.parent;
    }
    path.reverse();
    Some(path)
}

fn fuzzy_contains(candidate: &str, query: &str) -> bool {
    let mut candidate = candidate.chars().flat_map(char::to_lowercase);
    query
        .chars()
        .flat_map(char::to_lowercase)
        .all(|needle| candidate.by_ref().any(|character| character == needle))
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
        assert!(fuzzy_contains("Ship the Desk", "std"));
        assert!(!fuzzy_contains("Ship the Desk", "xyz"));
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
            0,
            false,
            &mut projection,
        );
        assert_eq!(
            projection.order,
            vec![(host, root), (host, child), (host, sibling)]
        );
        assert_eq!(projection.collapsed, vec![((host, root), 1..2)]);
        assert_eq!(projection.depths, vec![0, 1, 0]);
        assert_eq!(node_path(&nodes, child), Some(vec![root, child]));
    }
}
