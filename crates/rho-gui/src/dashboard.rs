//! The Desk: one org-like editable buffer per attached host, projected with
//! exactly one multibuffer excerpt at each daemon ownership boundary.

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
    DeskClock, DeskHeading, DeskHeadingState, DeskOperation, DeskSnapshot, DeskTextOpRecord,
    DeskTransaction, parse,
};
use rho_ui_proto::{AgentId, ClientMessage};
use text::{BufferId, ReplicaId, ToOffset as _};
use theme::ActiveTheme as _;
use ui::div;

use crate::registry::{AgentRegistry, HostId};
use crate::workspace::Workspace;

const DECORATION_KEY: HighlightKey = HighlightKey::SyntaxTreeView(usize::MAX - 1);
const PROPERTY_KEY: HighlightKey = HighlightKey::SyntaxTreeView(usize::MAX - 2);
const HEADING_KEY: HighlightKey = HighlightKey::SyntaxTreeView(usize::MAX - 3);
const TODO_KEY: HighlightKey = HighlightKey::SyntaxTreeView(usize::MAX - 4);
const MUTED_STATE_KEY: HighlightKey = HighlightKey::SyntaxTreeView(usize::MAX - 5);
const WELCOME_DESK: &str = "* TODO Run work from this Desk\n\
o/O add a sibling · >>/<< change depth · Tab folds a subtree\n\
s staffs; edit the brief and press s again to reply\n\
d marks done · x discards · gn cycles NOW · gh jumps headings\n\
:agent: shows who owns a heading · :project: chooses its workdir\n\
Edit freely in normal Vim modes; Desk text syncs across clients\n";

pub type ParsedHeadingState = DeskHeadingState;

#[derive(Clone, Debug, PartialEq)]
pub enum RowTarget {
    None,
    Agent(AgentId),
}

struct HostDesk {
    snapshot: DeskSnapshot,
}

#[derive(Clone)]
struct NowItem {
    host: HostId,
    offset: usize,
    agent_id: AgentId,
    attention: rho_ui_proto::UiAttention,
    last_active: rho_core::UnixMs,
    title: String,
}

struct DeskCaret {
    anchor: text::Anchor,
    collapsed: HashSet<(HostId, text::Anchor)>,
}

struct HeadingEntry {
    label: String,
    description: String,
    host: HostId,
    offset: usize,
}

pub struct Dashboard {
    multi_buffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    hosts: BTreeMap<HostId, HostDesk>,
    buffers: HashMap<HostId, Entity<Buffer>>,
    subscriptions: HashMap<HostId, gpui::Subscription>,
    buffer_hosts: HashMap<BufferId, HostId>,
    known_ops: HashSet<(HostId, DeskClock)>,
    headers_disabled: HashSet<BufferId>,
    displayed_len: usize,
    next_buffer_id: u64,
    collapsed: HashSet<(HostId, text::Anchor)>,
    fold_blocks: Vec<CustomBlockId>,
    host_header_blocks: Vec<CustomBlockId>,
    decoration_inlays: Vec<InlayId>,
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
            buffer_hosts: HashMap::new(),
            known_ops: HashSet::new(),
            headers_disabled: HashSet::new(),
            displayed_len: 0,
            next_buffer_id: 1,
            collapsed: HashSet::new(),
            fold_blocks: Vec::new(),
            host_header_blocks: Vec::new(),
            decoration_inlays: Vec::new(),
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
        let should_seed = should_seed_snapshot(&snapshot);
        self.buffers.remove(&host);
        self.subscriptions.remove(&host);
        self.buffer_hosts.retain(|_, candidate| *candidate != host);
        self.known_ops.retain(|(candidate, _)| *candidate != host);
        self.known_ops
            .extend(snapshot.operations.iter().map(|op| (host, op.timestamp())));
        let operations = snapshot.operations.clone();
        self.hosts.insert(host, HostDesk { snapshot });
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
                    .filter_map(|op| op.to_text().ok())
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
        self.buffer_hosts.insert(buffer.read(cx).remote_id(), host);
        self.buffers.insert(host, buffer);
        self.subscriptions.insert(host, subscription);
        if should_seed {
            self.buffers[&host].update(cx, |buffer, cx| {
                buffer.edit([(0..0, WELCOME_DESK)], None, cx)
            });
        }
        cx.notify();
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
        if let Some(buffer) = self.buffers.get(&host)
            && let Ok(operation) = record.operation.to_text()
        {
            buffer.update(cx, |buffer, cx| {
                buffer.apply_ops([language::Operation::Buffer(operation)], cx)
            });
        }
    }

    pub fn mark_local_text_op(&mut self, host: HostId, clock: DeskClock) {
        self.known_ops.insert((host, clock));
    }

    fn text(&self, host: HostId, cx: &App) -> Option<String> {
        let buffer = self.buffers.get(&host)?.read(cx);
        Some(buffer.text_for_range(0..buffer.len()).collect())
    }

    pub fn sync(
        &mut self,
        registry: &AgentRegistry,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let order = self
            .hosts
            .keys()
            .copied()
            .filter(|host| self.buffers.contains_key(host))
            .collect::<Vec<_>>();
        let ids = order
            .iter()
            .map(|host| self.buffers[host].read(cx).remote_id())
            .collect::<Vec<_>>();
        let new_headers = ids
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
        self.multi_buffer.update(cx, |multi, cx| {
            for (index, host) in order.iter().enumerate() {
                let buffer = &self.buffers[host];
                multi.set_excerpts_for_path(
                    PathKey::sorted(index as u64),
                    buffer.clone(),
                    [Point::zero()..buffer.read(cx).max_point()],
                    0,
                    cx,
                );
            }
            for stale in order.len()..old_len {
                multi.remove_excerpts(PathKey::sorted(stale as u64), cx);
            }
        });
        self.displayed_len = order.len();
        self.sync_headers(registry, &order, cx);
        self.sync_decorations(registry, &order, cx);
        self.sync_folds(&order, cx);
        self.sync_now(registry, &order, cx);
    }

    fn sync_headers(
        &mut self,
        registry: &AgentRegistry,
        order: &[HostId],
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
                )
            });
        }
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let props = order
            .iter()
            .filter_map(|host| {
                let position =
                    snapshot.anchor_in_excerpt(self.buffers[host].read(cx).anchor_before(0))?;
                let name = registry.host_name(*host).to_owned();
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
        self.host_header_blocks = self
            .editor
            .update(cx, |editor, cx| editor.insert_blocks(props, None, cx));
    }

    fn binding_for(
        registry: &AgentRegistry,
        host: HostId,
        heading: &DeskHeading,
    ) -> Option<AgentId> {
        if heading.duplicate_agent {
            return None;
        }
        let agent_id = registry.agent_by_label(heading.agent_value.as_deref()?)?;
        (registry.host_of_agent(agent_id) == Some(host)).then_some(agent_id)
    }

    fn sync_decorations(
        &mut self,
        registry: &AgentRegistry,
        order: &[HostId],
        cx: &mut Context<Workspace>,
    ) {
        let multi = self.multi_buffer.read(cx).snapshot(cx);
        let mut inlays = Vec::new();
        let mut highlights = Vec::new();
        let mut property_ranges = Vec::new();
        let mut heading_ranges = Vec::new();
        let mut todo_ranges = Vec::new();
        let mut muted_state_ranges = Vec::new();
        for host in order {
            let Some(text) = self.text(*host, cx) else {
                continue;
            };
            let buffer = self.buffers[host].read(cx);
            for heading in parse(&text) {
                for property in &heading.properties {
                    let range = &property.line_range;
                    if let (Some(start), Some(end)) = (
                        multi.anchor_in_excerpt(buffer.anchor_before(range.start)),
                        multi.anchor_in_excerpt(buffer.anchor_after(range.end)),
                    ) {
                        property_ranges.push(start..end);
                    }
                }
                for range in [&heading.stars_range, &heading.title_range] {
                    if let (Some(start), Some(end)) = (
                        multi.anchor_in_excerpt(buffer.anchor_before(range.start)),
                        multi.anchor_in_excerpt(buffer.anchor_after(range.end)),
                    ) {
                        heading_ranges.push(start..end);
                    }
                }
                if let Some(range) = &heading.state_range
                    && let (Some(start), Some(end)) = (
                        multi.anchor_in_excerpt(buffer.anchor_before(range.start)),
                        multi.anchor_in_excerpt(buffer.anchor_after(range.end)),
                    )
                {
                    match heading.state {
                        Some(DeskHeadingState::Todo) => todo_ranges.push(start..end),
                        Some(DeskHeadingState::Done | DeskHeadingState::Discarded) => {
                            muted_state_ranges.push(start..end)
                        }
                        None => {}
                    }
                }
                let (position, label) = if heading.duplicate_agent {
                    (heading.heading_range.end, "  · duplicate agent".to_owned())
                } else if let Some(agent_id) = Self::binding_for(registry, *host, &heading) {
                    let attention = match registry.attention(agent_id) {
                        rho_ui_proto::UiAttention::Quiet => "idle",
                        rho_ui_proto::UiAttention::Working => "working",
                        rho_ui_proto::UiAttention::Pending => "pending",
                        rho_ui_proto::UiAttention::NeedsInput => "needs you",
                    };
                    (
                        heading.heading_range.end,
                        format!("  · {} · {attention}", registry.agent_human_name(agent_id)),
                    )
                } else if heading.agent_value.is_some() {
                    (heading.heading_range.end, "  · unknown agent".to_owned())
                } else {
                    continue;
                };
                let Some(position) = multi.anchor_in_excerpt(buffer.anchor_before(position)) else {
                    continue;
                };
                let inlay = Inlay::custom(inlays.len(), position, label.clone());
                highlights.push(InlayHighlight {
                    inlay: inlay.id,
                    inlay_position: position,
                    range: 0..label.len(),
                });
                inlays.push(inlay);
            }
        }
        let removed = std::mem::take(&mut self.decoration_inlays);
        self.decoration_inlays = inlays.iter().map(|inlay| inlay.id).collect();
        self.editor.update(cx, |editor, cx| {
            editor.splice_inlays(&removed, inlays, cx);
            editor.clear_highlights(DECORATION_KEY, cx);
            editor.highlight_inlays(
                DECORATION_KEY,
                highlights,
                gpui::HighlightStyle {
                    color: Some(cx.theme().colors().border_variant.into()),
                    ..Default::default()
                },
                cx,
            );
            editor.highlight_text_key(
                PROPERTY_KEY,
                property_ranges,
                gpui::HighlightStyle {
                    color: Some(cx.theme().colors().text_muted.into()),
                    ..Default::default()
                },
                false,
                cx,
            );
            editor.highlight_text_key(
                HEADING_KEY,
                heading_ranges,
                gpui::HighlightStyle {
                    font_weight: Some(gpui::FontWeight::BOLD),
                    ..Default::default()
                },
                false,
                cx,
            );
            editor.highlight_text_key(
                TODO_KEY,
                todo_ranges,
                gpui::HighlightStyle {
                    color: Some(cx.theme().colors().text_accent.into()),
                    font_weight: Some(gpui::FontWeight::BOLD),
                    ..Default::default()
                },
                false,
                cx,
            );
            editor.highlight_text_key(
                MUTED_STATE_KEY,
                muted_state_ranges,
                gpui::HighlightStyle {
                    color: Some(cx.theme().colors().text_muted.into()),
                    ..Default::default()
                },
                false,
                cx,
            );
        });
    }

    fn subtree_end(headings: &[DeskHeading], index: usize, text_len: usize) -> usize {
        headings
            .iter()
            .skip(index + 1)
            .find(|next| next.depth <= headings[index].depth)
            .map_or(text_len, |next| next.heading_range.start)
    }

    fn sync_folds(&mut self, order: &[HostId], cx: &mut Context<Workspace>) {
        if !self.fold_blocks.is_empty() {
            self.editor.update(cx, |editor, cx| {
                editor.remove_blocks(
                    std::mem::take(&mut self.fold_blocks).into_iter().collect(),
                    None,
                    cx,
                )
            });
        }
        let mut collapsed_offsets = HashSet::new();
        let mut valid_collapsed = HashSet::new();
        for host in order {
            let Some(buffer) = self.buffers.get(host) else {
                continue;
            };
            let buffer = buffer.read(cx);
            let snapshot = buffer.text_snapshot();
            let text = snapshot.text();
            let headings = parse(&text);
            for (collapsed_host, anchor) in &self.collapsed {
                if collapsed_host == host
                    && let Some(offset) = resolved_heading_offset(*anchor, &snapshot, &headings)
                {
                    collapsed_offsets.insert((*host, offset));
                    valid_collapsed.insert((*host, *anchor));
                }
            }
        }
        self.collapsed = valid_collapsed;
        let multi = self.multi_buffer.read(cx).snapshot(cx);
        let mut props = Vec::new();
        for host in order {
            let Some(text) = self.text(*host, cx) else {
                continue;
            };
            let headings = parse(&text);
            let buffer = self.buffers[host].read(cx);
            for (index, heading) in headings.iter().enumerate() {
                if !collapsed_offsets.contains(&(*host, heading.heading_range.start)) {
                    continue;
                }
                let end = Self::subtree_end(&headings, index, text.len());
                let start = heading.heading_range.end;
                if start >= end {
                    continue;
                }
                let (Some(start), Some(end)) = (
                    multi.anchor_in_excerpt(buffer.anchor_before(start)),
                    multi.anchor_in_excerpt(buffer.anchor_after(end)),
                ) else {
                    continue;
                };
                props.push(BlockProperties {
                    placement: BlockPlacement::Replace(start..=end),
                    height: Some(1),
                    style: BlockStyle::Flex,
                    render: Arc::new(move |_| {
                        div().pl_2().child("▸ folded subtree").into_any_element()
                    }),
                    priority: 0,
                });
            }
        }
        self.fold_blocks = self
            .editor
            .update(cx, |editor, cx| editor.insert_blocks(props, None, cx));
    }

    fn sync_now(
        &mut self,
        registry: &AgentRegistry,
        order: &[HostId],
        cx: &mut Context<Workspace>,
    ) {
        self.now_items.clear();
        for host in order {
            let Some(text) = self.text(*host, cx) else {
                continue;
            };
            for heading in parse(&text) {
                let Some(agent_id) = Self::binding_for(registry, *host, &heading) else {
                    continue;
                };
                let attention = registry.attention(agent_id);
                if attention < rho_ui_proto::UiAttention::Pending {
                    continue;
                }
                let Some(last_active) = registry.agent_last_active(agent_id) else {
                    continue;
                };
                self.now_items.push(NowItem {
                    host: *host,
                    offset: heading.heading_range.start,
                    agent_id,
                    attention,
                    last_active,
                    title: heading.title,
                });
            }
        }
        self.now_items
            .sort_by_key(|item| (Reverse(item.last_active), item.agent_id));
        if let Some(block) = self.now_block.take() {
            self.editor.update(cx, |editor, cx| {
                editor.remove_blocks([block].into_iter().collect(), None, cx)
            });
        }
        let Some(host) = order.first() else {
            return;
        };
        if self.now_items.is_empty() {
            return;
        }
        let multi = self.multi_buffer.read(cx).snapshot(cx);
        let Some(position) = multi.anchor_in_excerpt(self.buffers[host].read(cx).anchor_before(0))
        else {
            return;
        };
        let lines = self
            .now_items
            .iter()
            .map(|item| {
                format!(
                    "{} · {} · {}",
                    item.title,
                    registry.agent_human_name(item.agent_id),
                    if item.attention == rho_ui_proto::UiAttention::NeedsInput {
                        "needs input"
                    } else {
                        "pending response"
                    }
                )
            })
            .collect::<Vec<_>>();
        let height = lines.len() as u32;
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

    fn cursor(&self, cx: &mut Context<Workspace>) -> Option<(HostId, usize)> {
        let (id, offset) = self.editor.update(cx, |editor, cx| {
            let head = editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx))
                .head();
            editor
                .buffer()
                .read(cx)
                .snapshot(cx)
                .point_to_buffer_offset(head)
                .map(|(buffer, offset)| (buffer.remote_id(), offset.0))
        })?;
        Some((*self.buffer_hosts.get(&id)?, offset))
    }

    fn cursor_heading(
        &self,
        cx: &mut Context<Workspace>,
    ) -> Option<(HostId, String, Vec<DeskHeading>, usize)> {
        let (host, offset) = self.cursor(cx)?;
        let text = self.text(host, cx)?;
        let headings = parse(&text);
        let index = headings
            .iter()
            .enumerate()
            .rev()
            .find(|(_, h)| h.heading_range.start <= offset)?
            .0;
        (offset < Self::subtree_end(&headings, index, text.len()))
            .then_some((host, text, headings, index))
    }

    fn caret(&self, cx: &mut Context<Workspace>) -> Option<text::Anchor> {
        let (host, offset) = self.cursor(cx)?;
        Some(self.buffers.get(&host)?.read(cx).anchor_after(offset))
    }
    fn move_to(
        &self,
        host: HostId,
        offset: usize,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let anchor = self
            .buffers
            .get(&host)
            .map(|buffer| buffer.read(cx).anchor_before(offset));
        let Some(anchor) = anchor.and_then(|anchor| {
            self.multi_buffer
                .read(cx)
                .snapshot(cx)
                .anchor_in_excerpt(anchor)
        }) else {
            return false;
        };
        self.editor.update(cx, |editor, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_anchor_ranges([anchor..anchor])
            })
        });
        true
    }

    pub fn cursor_target(
        &self,
        registry: &AgentRegistry,
        cx: &mut Context<Workspace>,
    ) -> Option<RowTarget> {
        let (host, _, headings, index) = self.cursor_heading(cx)?;
        Self::binding_for(registry, host, &headings[index])
            .map(RowTarget::Agent)
            .or(Some(RowTarget::None))
    }

    pub fn staffing_target(
        &self,
        cx: &mut Context<Workspace>,
    ) -> Result<(HostId, usize, String, Option<String>), &'static str> {
        let (host, text, headings, index) = self
            .cursor_heading(cx)
            .ok_or("staff: put the cursor on a heading")?;
        if headings[index].title.trim().is_empty() {
            return Err("staff: give this heading a title first");
        }
        let mut path = Vec::new();
        let mut current = Some(index);
        while let Some(at) = current {
            path.push(at);
            current = headings[at].parent;
        }
        path.reverse();
        let brief = path
            .into_iter()
            .map(|at| {
                let heading = &headings[at];
                let body = text[heading.body_range.clone()].trim_end();
                if body.is_empty() {
                    heading.title.clone()
                } else {
                    format!("{}\n{}", heading.title, body)
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        Ok((
            host,
            headings[index].heading_range.start,
            brief,
            headings[index].resolved_project.clone(),
        ))
    }

    pub fn set_heading_project(
        &mut self,
        host: HostId,
        offset: usize,
        project: &str,
        cx: &mut Context<Workspace>,
    ) {
        let Some(text) = self.text(host, cx) else {
            return;
        };
        let Some(heading) = parse(&text)
            .into_iter()
            .find(|heading| heading.heading_range.start == offset)
        else {
            return;
        };
        let insertion = heading.heading_range.end
            + usize::from(text.as_bytes().get(heading.heading_range.end) == Some(&b'\n'));
        self.buffers[&host].update(cx, |buffer, cx| {
            buffer.edit(
                [(insertion..insertion, format!(":project: {project}\n"))],
                None,
                cx,
            )
        });
    }

    pub fn set_heading_state(
        &mut self,
        host: HostId,
        offset: usize,
        state: ParsedHeadingState,
        cx: &mut Context<Workspace>,
    ) {
        let Some(text) = self.text(host, cx) else {
            return;
        };
        let Some(heading) = parse(&text)
            .into_iter()
            .find(|h| h.heading_range.start == offset)
        else {
            return;
        };
        let replacement = format!(
            "{} {}{}",
            "*".repeat(heading.depth),
            state.keyword(),
            if heading.title.is_empty() {
                String::new()
            } else {
                format!(" {}", heading.title)
            }
        );
        self.buffers[&host].update(cx, |buffer, cx| {
            buffer.edit([(heading.heading_range, replacement)], None, cx)
        });
    }
    pub fn set_cursor_heading_state(
        &mut self,
        state: ParsedHeadingState,
        cx: &mut Context<Workspace>,
    ) {
        if let Some((host, _, headings, index)) = self.cursor_heading(cx) {
            self.set_heading_state(host, headings[index].heading_range.start, state, cx);
        }
    }
    pub fn cursor_on_heading_line(&self, cx: &mut Context<Workspace>) -> bool {
        let Some((_, offset)) = self.cursor(cx) else {
            return false;
        };
        self.cursor_heading(cx)
            .is_some_and(|(_, _, headings, index)| {
                headings[index].heading_range.contains(&offset)
                    || offset == headings[index].heading_range.end
            })
    }

    pub fn insert_sibling(
        &mut self,
        above: bool,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let Some((host, text, headings, index)) = self.cursor_heading(cx) else {
            return false;
        };
        let heading = &headings[index];
        let at = if above {
            heading.heading_range.start
        } else {
            Self::subtree_end(&headings, index, text.len())
        };
        let insertion = format!("{} \n", "*".repeat(heading.depth));
        self.buffers[&host].update(cx, |buffer, cx| {
            buffer.edit([(at..at, insertion.clone())], None, cx)
        });
        self.move_to(host, at + heading.depth + 1, window, cx)
    }

    pub fn structure_move(
        &mut self,
        direction: StructureDirection,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let Some((host, text, headings, index)) = self.cursor_heading(cx) else {
            return false;
        };
        let heading = &headings[index];
        match direction {
            StructureDirection::Demote => {
                self.buffers[&host].update(cx, |buffer, cx| {
                    buffer.edit(
                        [(
                            heading.heading_range.start..heading.heading_range.start,
                            "*",
                        )],
                        None,
                        cx,
                    )
                });
            }
            StructureDirection::Promote if heading.depth > 1 => {
                self.buffers[&host].update(cx, |buffer, cx| {
                    buffer.edit(
                        [(
                            heading.heading_range.start..heading.heading_range.start + 1,
                            "",
                        )],
                        None,
                        cx,
                    )
                });
            }
            StructureDirection::Promote => return false,
            StructureDirection::Up => {
                let Some(previous) = (0..index).rev().find(|at| {
                    headings[*at].depth == heading.depth && headings[*at].parent == heading.parent
                }) else {
                    return false;
                };
                let current_end = Self::subtree_end(&headings, index, text.len());
                let prev_start = headings[previous].heading_range.start;
                let prev_end = heading.heading_range.start;
                let replacement = format!(
                    "{}{}",
                    &text[heading.heading_range.start..current_end],
                    &text[prev_start..prev_end]
                );
                self.buffers[&host].update(cx, |buffer, cx| {
                    buffer.edit([(prev_start..current_end, replacement)], None, cx)
                });
            }
            StructureDirection::Down => {
                let current_end = Self::subtree_end(&headings, index, text.len());
                let Some(next) = headings
                    .iter()
                    .enumerate()
                    .skip(index + 1)
                    .find(|(_, h)| {
                        h.heading_range.start >= current_end
                            && h.depth == heading.depth
                            && h.parent == heading.parent
                    })
                    .map(|(at, _)| at)
                else {
                    return false;
                };
                let next_end = Self::subtree_end(&headings, next, text.len());
                let replacement = format!(
                    "{}{}",
                    &text[current_end..next_end],
                    &text[heading.heading_range.start..current_end]
                );
                self.buffers[&host].update(cx, |buffer, cx| {
                    buffer.edit(
                        [(heading.heading_range.start..next_end, replacement)],
                        None,
                        cx,
                    )
                });
            }
        }
        true
    }

    pub fn delete_empty(&mut self, cx: &mut Context<Workspace>) -> bool {
        let Some((host, text, headings, index)) = self.cursor_heading(cx) else {
            return false;
        };
        let heading = &headings[index];
        let end = Self::subtree_end(&headings, index, text.len());
        let has_child = headings
            .get(index + 1)
            .is_some_and(|next| next.heading_range.start < end && next.depth > heading.depth);
        if has_child
            || !heading.title.is_empty()
            || !text[heading.body_range.clone()].trim().is_empty()
        {
            return false;
        }
        self.buffers[&host].update(cx, |buffer, cx| {
            buffer.edit([(heading.heading_range.start..end, "")], None, cx)
        });
        true
    }

    pub fn toggle_subagents(&mut self, cx: &mut Context<Workspace>) -> bool {
        let Some((host, _, headings, index)) = self.cursor_heading(cx) else {
            return false;
        };
        if !headings
            .get(index + 1)
            .is_some_and(|next| next.depth > headings[index].depth)
        {
            return false;
        }
        let key = (
            host,
            self.buffers[&host]
                .read(cx)
                .anchor_before(headings[index].heading_range.start),
        );
        if !self.collapsed.remove(&key) {
            self.collapsed.insert(key);
        }
        cx.notify();
        true
    }

    fn heading_entries(&self, registry: &AgentRegistry, cx: &App) -> Vec<HeadingEntry> {
        self.hosts
            .keys()
            .flat_map(|host| {
                self.text(*host, cx)
                    .into_iter()
                    .flat_map(|text| parse(&text))
                    .map(|heading| HeadingEntry {
                        label: format!(
                            "{} · {} · @{}",
                            if heading.title.is_empty() {
                                "(untitled)"
                            } else {
                                &heading.title
                            },
                            registry.host_name(*host),
                            heading.heading_range.start
                        ),
                        description: format!(
                            "{} · level {}",
                            registry.host_name(*host),
                            heading.depth
                        ),
                        host: *host,
                        offset: heading.heading_range.start,
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
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
        self.move_to(entry.host, entry.offset, window, cx)
    }

    pub fn next_now(
        &mut self,
        _registry: &AgentRegistry,
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
            .map(|at| (at + 1) % self.now_items.len())
            .unwrap_or(0);
        let item = self.now_items.get(index)?.clone();
        if let Some(anchor) = self.caret(cx) {
            self.caret_stack.push(DeskCaret {
                anchor,
                collapsed: self.collapsed.clone(),
            });
        }
        self.move_to(item.host, item.offset, window, cx);
        self.now_cursor = Some(item.agent_id);
        Some(item.agent_id)
    }
    pub fn back(
        &mut self,
        _registry: &AgentRegistry,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let Some(caret) = self.caret_stack.pop() else {
            return false;
        };
        self.collapsed = caret.collapsed;
        let Some(anchor) = self
            .multi_buffer
            .read(cx)
            .snapshot(cx)
            .anchor_in_excerpt(caret.anchor)
        else {
            return false;
        };
        self.editor.update(cx, |editor, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_anchor_ranges([anchor..anchor])
            })
        });
        true
    }
    pub fn hint(&self, cx: &mut Context<Workspace>) -> &'static str {
        if self.cursor_on_heading_line(cx) {
            "o/O new heading · >>/<< demote/promote · Tab fold · s staff/reply · :project: workdir · d done · x discard · Alt-↑/↓ move · gn now · gh jump"
        } else {
            "vim editing · gn now · gb back · gh jump headings"
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StructureDirection {
    Demote,
    Promote,
    Up,
    Down,
}

fn resolved_heading_offset(
    anchor: text::Anchor,
    snapshot: &text::BufferSnapshot,
    headings: &[DeskHeading],
) -> Option<usize> {
    let offset = anchor.to_offset(snapshot);
    headings
        .iter()
        .any(|heading| heading.heading_range.start == offset)
        .then_some(offset)
}

fn should_seed_snapshot(snapshot: &DeskSnapshot) -> bool {
    snapshot.text.is_empty() && snapshot.operations.is_empty()
}

fn fuzzy_contains(haystack: &str, needle: &str) -> bool {
    let mut chars = needle.chars().flat_map(char::to_lowercase);
    let mut wanted = chars.next();
    if wanted.is_none() {
        return true;
    }
    for candidate in haystack.chars().flat_map(char::to_lowercase) {
        if Some(candidate) == wanted {
            wanted = chars.next();
            if wanted.is_none() {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fold_range_ends_at_next_peer() {
        let text = "* a\nbody\n** child\nchild body\n* b\n";
        let headings = parse(text);
        assert_eq!(
            &text
                [headings[0].heading_range.start..Dashboard::subtree_end(&headings, 0, text.len())],
            "* a\nbody\n** child\nchild body\n"
        );
    }

    #[test]
    fn collapsed_heading_anchor_tracks_edits_above_it() {
        let mut buffer = text::Buffer::new(
            ReplicaId::new(1),
            text::BufferId::new(1).unwrap(),
            "* parent\n** child\nbody\n",
        );
        let anchor = buffer.anchor_before("* parent\n".len());
        buffer.edit([(0..0, "intro\n")]);
        let snapshot = buffer.snapshot();
        let text = snapshot.text();
        let headings = parse(&text);

        assert_eq!(
            resolved_heading_offset(anchor, &snapshot, &headings),
            Some("intro\n* parent\n".len())
        );
    }

    #[test]
    fn welcome_seed_is_only_for_a_never_edited_document() {
        let empty = DeskSnapshot::default();
        assert!(should_seed_snapshot(&empty));

        let mut buffer = text::Buffer::new(ReplicaId::new(1), text::BufferId::new(1).unwrap(), "");
        let mut edited_empty = DeskSnapshot::default();
        edited_empty.operations.push(DeskOperation::from_text(
            &buffer.edit([(0..0, "edited then deleted")]),
        ));
        assert!(!should_seed_snapshot(&edited_empty));

        let headings = parse(WELCOME_DESK);
        assert_eq!(headings.len(), 1);
        assert_eq!(headings[0].state, Some(ParsedHeadingState::Todo));
        assert_eq!(
            WELCOME_DESK[headings[0].body_range.clone()].lines().count(),
            5
        );
    }
}
