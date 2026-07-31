//! The dashboard: the rail reborn as a real editor buffer — rho's
//! magit-status. A single-root workstream is one compact row; the uncommon
//! multi-root workstream becomes a header followed by human-named root rows.
//! Agent trees stay compact behind independently nested disclosure markers.
//! Generated read-only text lives in a normal editor, so cursor motions and
//! search come from the editor rather than bespoke list chrome. Acting keys
//! address the stable root under the cursor: `enter` opens, `r` splices an
//! inline reply draft under that root. Every line is its own tiny buffer in
//! the multibuffer, so refreshes can rearrange excerpts without eating typed
//! drafts or leaving the cursor attached to a stale line number.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::{Arc, Mutex};

use editor::display_map::{
    BlockContext, BlockStyle, CustomBlockId, DisplayRow, ToDisplayPoint as _,
};
use editor::{
    DisplayElisionId, DisplayElisionProperties, Editor, EditorMode, HighlightKey, Inlay,
    SizingBehavior,
};
use gpui::prelude::*;
use gpui::{App, Context, Entity, Focusable as _, HighlightStyle, Window};
use language::{Buffer, Capability, InlayId, Point};
use multi_buffer::{MultiBuffer, PathKey};
use rho_core::ContentPart;
use rho_ui_proto::{AgentId, UiAttention, WorkstreamId};
use text::BufferId;
use theme::ActiveTheme as _;
use ui::div;

use crate::registry::{AgentRegistry, HostId, Workstream};
use crate::workspace::Workspace;

/// Highlight-key space for dashboard classes, clear of the transcript's
/// semantic and syntax key ranges.
const DASHBOARD_KEY_BASE: usize = usize::MAX - 200;

/// Inlay id space for reply-draft placeholders.
const PLACEHOLDER_ID_BASE: usize = 1_000_000;

/// Highlight key for draft text (the user-message accent), past the
/// class key range.
const DRAFT_TEXT_KEY: HighlightKey =
    HighlightKey::SyntaxTreeView(DASHBOARD_KEY_BASE + 2 * DashClass::ALL.len());

/// Identity of one dashboard line; each key owns one buffer in the
/// multibuffer. Cursor position and reply drafts survive re-sorts by
/// following their key, not their line number.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum LineKey {
    Iris,
    /// A daemon's section header, present only while several are attached.
    Host {
        host: HostId,
        tail: bool,
    },
    Group {
        name: String,
        tail: bool,
    },
    Stream(WorkstreamId),
    Agent(AgentId),
    Reply(AgentId),
    /// The inline new-agent draft, at the top of the listing.
    NewDraft,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FoldSpec {
    parent_agent: AgentId,
    parent: LineKey,
    descendants: Vec<LineKey>,
    descendant_count: usize,
}

/// What the line under the cursor refers to; the object of every
/// dashboard command.
#[derive(Clone, Debug, PartialEq)]
pub enum RowTarget {
    /// The client-local Iris voice surface.
    Iris,
    /// Group headers and other inert lines.
    None,
    Stream {
        workstream_id: WorkstreamId,
        root: Option<AgentId>,
    },
    Agent(AgentId),
    /// An inline reply draft addressed to this agent.
    Reply(AgentId),
    /// The inline new-agent draft.
    NewDraft,
}

pub struct Dashboard {
    multi_buffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    /// One buffer per line key: read-only listing lines and writable
    /// reply drafts alike.
    buffers: HashMap<LineKey, Entity<Buffer>>,
    /// Current display order; index n is the multibuffer's path key n.
    order: Vec<LineKey>,
    /// What each present key means, for cursor lookup.
    targets: HashMap<LineKey, RowTarget>,
    /// Open reply drafts in creation order (position comes from `order`).
    replies: Vec<AgentId>,
    /// Keeps the workspace re-rendering on draft edits, so placeholder
    /// and gutter chrome track the text.
    reply_subscriptions: HashMap<AgentId, gpui::Subscription>,
    reply_attachments: HashMap<AgentId, Vec<ContentPart>>,
    /// The inline new-agent draft, when open: its buffer plus the edit
    /// subscription that keeps chrome fresh.
    new_draft: Option<(Entity<Buffer>, gpui::Subscription, String)>,
    new_draft_attachments: Vec<ContentPart>,
    attachment_blocks: Vec<CustomBlockId>,
    attachments_dirty: bool,
    /// Move the cursor into this key's buffer on the next sync — how a
    /// freshly opened reply draft receives the cursor.
    pending_cursor: Option<LineKey>,
    /// Reply placeholder inlays currently spliced in.
    placeholder_ids: Vec<InlayId>,
    /// Projected descendant rows keyed by their stable parent identity.
    folds: HashMap<AgentId, FoldSpec>,
    /// Expansion state keyed by stable parent identity.
    expanded_folds: Arc<Mutex<HashSet<AgentId>>>,
    /// One editor-native elision owns the contiguous quiet tail. Updating the
    /// same id preserves the editor's open/closed state across refreshes.
    rail_tail: Option<(DisplayElisionId, (LineKey, LineKey))>,
    /// Buffers already registered as headerless with the editor. A
    /// boundary onto a headerless buffer draws nothing, so this is what
    /// keeps the per-line excerpts seamless.
    headers_disabled: std::collections::HashSet<BufferId>,
}

impl Dashboard {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let multi_buffer = cx.new(|_| MultiBuffer::without_headers(Capability::ReadWrite));
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                multi_buffer.clone(),
                #[cfg(feature = "native")]
                None,
                window,
                cx,
            );
            crate::editor_config::configure(&mut editor, window, cx);
            // Unlike the chat editors, clicking a row to put the cursor on
            // it is the whole point.
            editor.set_mouse_click_selection_enabled(true, cx);
            editor
        });
        Self {
            multi_buffer,
            editor,
            buffers: HashMap::new(),
            order: Vec::new(),
            targets: HashMap::new(),
            replies: Vec::new(),
            reply_subscriptions: HashMap::new(),
            reply_attachments: HashMap::new(),
            new_draft: None,
            new_draft_attachments: Vec::new(),
            attachment_blocks: Vec::new(),
            attachments_dirty: false,
            pending_cursor: None,
            placeholder_ids: Vec::new(),
            folds: HashMap::new(),
            expanded_folds: Arc::new(Mutex::new(HashSet::new())),
            rail_tail: None,
            headers_disabled: std::collections::HashSet::new(),
        }
    }

    /// Registers every current buffer as headerless with the editor, so
    /// excerpt boundaries between the per-line buffers draw no divider.
    fn ensure_headerless(&mut self, cx: &mut Context<Workspace>) {
        let new_ids = self
            .buffers
            .values()
            .map(|buffer| buffer.read(cx).remote_id())
            .filter(|id| !self.headers_disabled.contains(id))
            .collect::<Vec<_>>();
        if new_ids.is_empty() {
            return;
        }
        self.editor.update(cx, |editor, cx| {
            for id in &new_ids {
                editor.disable_header_for_buffer(*id, cx);
            }
        });
        self.headers_disabled.extend(new_ids);
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    #[cfg(test)]
    pub(crate) fn fold_count(&self) -> usize {
        self.folds.len()
    }

    #[cfg(test)]
    pub(crate) fn rail_tail_id(&self) -> Option<DisplayElisionId> {
        self.rail_tail.as_ref().map(|(id, _)| *id)
    }

    #[cfg(test)]
    pub(crate) fn rail_tail_ends_in_reply(&self, agent_id: AgentId) -> bool {
        self.rail_tail
            .as_ref()
            .is_some_and(|(_, (_, last))| *last == LineKey::Reply(agent_id))
    }

    pub fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }

    /// Opens (or returns to) an inline reply draft under the agent's row.
    /// The draft is a writable buffer of its own: it parks where it is
    /// when the user wanders off and survives every refresh.
    pub fn open_reply(&mut self, agent_id: AgentId, cx: &mut Context<Workspace>) {
        let key = LineKey::Reply(agent_id);
        if !self.replies.contains(&agent_id) {
            self.replies.push(agent_id);
            let buffer = self
                .buffers
                .entry(key.clone())
                .or_insert_with(|| cx.new(|cx| Buffer::local("", cx)))
                .clone();
            self.reply_subscriptions.insert(
                agent_id,
                cx.subscribe(&buffer, |_, _, event, cx| {
                    if matches!(event, language::BufferEvent::Edited { .. }) {
                        cx.notify();
                    }
                }),
            );
        }
        self.pending_cursor = Some(key);
        cx.notify();
    }

    /// Opens (or returns to) the inline new-agent draft at the top of the
    /// listing. Like a reply draft it parks when left and survives
    /// refreshes.
    pub fn open_new_draft(&mut self, summary: String, cx: &mut Context<Workspace>) {
        if let Some((_, _, current)) = &mut self.new_draft {
            *current = summary;
        } else {
            let buffer = cx.new(|cx| Buffer::local("", cx));
            let subscription = cx.subscribe(&buffer, |_, _, event, cx| {
                if matches!(event, language::BufferEvent::Edited { .. }) {
                    cx.notify();
                }
            });
            self.buffers.insert(LineKey::NewDraft, buffer.clone());
            self.new_draft = Some((buffer, subscription, summary));
        }
        self.pending_cursor = Some(LineKey::NewDraft);
        cx.notify();
    }

    /// Takes the new-agent draft's content and closes it. `None` when empty.
    pub fn take_new_draft(&mut self, cx: &mut Context<Workspace>) -> Option<Vec<ContentPart>> {
        let (buffer, _, _) = self.new_draft.take()?;
        let text = buffer.read(cx).text().trim().to_owned();
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(ContentPart::Text { text });
        }
        content.append(&mut self.new_draft_attachments);
        self.buffers.remove(&LineKey::NewDraft);
        self.attachments_dirty = true;
        cx.notify();
        (!content.is_empty()).then_some(content)
    }

    /// Parks the cursor on an explicit agent row, or on its flattened
    /// singleton workstream row when the agent has no separate line.
    pub fn cursor_to_agent(
        &mut self,
        agent_id: AgentId,
        workstream_id: WorkstreamId,
        cx: &mut Context<Workspace>,
    ) {
        let key = LineKey::Agent(agent_id);
        self.pending_cursor = Some(if self.buffers.contains_key(&key) {
            key
        } else if let Some(parent) = self
            .folds
            .values()
            .filter(|fold| fold.descendants.contains(&key))
            .min_by_key(|fold| fold.descendant_count)
            .map(|fold| fold.parent.clone())
        {
            parent
        } else {
            LineKey::Stream(workstream_id)
        });
        cx.notify();
    }

    /// Takes a reply draft's content and closes it. `None` when empty.
    pub fn take_reply(
        &mut self,
        agent_id: AgentId,
        cx: &mut Context<Workspace>,
    ) -> Option<Vec<ContentPart>> {
        let key = LineKey::Reply(agent_id);
        let buffer = self.buffers.get(&key)?;
        let text = buffer.read(cx).text().trim().to_owned();
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(ContentPart::Text { text });
        }
        content.append(&mut self.reply_attachments.remove(&agent_id).unwrap_or_default());
        self.replies.retain(|reply| *reply != agent_id);
        self.buffers.remove(&key);
        self.reply_subscriptions.remove(&agent_id);
        self.attachments_dirty = true;
        cx.notify();
        (!content.is_empty()).then_some(content)
    }

    pub fn accepts_attachments(&self, cx: &mut Context<Workspace>) -> bool {
        matches!(
            self.cursor_target(cx),
            Some(RowTarget::Reply(_) | RowTarget::NewDraft)
        )
    }

    pub fn add_image(
        &mut self,
        media_type: String,
        data: Vec<u8>,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let part = ContentPart::Image { media_type, data };
        match self.cursor_target(cx) {
            Some(RowTarget::Reply(agent_id)) => {
                self.reply_attachments
                    .entry(agent_id)
                    .or_default()
                    .push(part);
            }
            Some(RowTarget::NewDraft) => self.new_draft_attachments.push(part),
            _ => return false,
        }
        self.attachments_dirty = true;
        cx.notify();
        true
    }

    pub fn clear_attachments(&mut self, cx: &mut Context<Workspace>) -> bool {
        let attachments = match self.cursor_target(cx) {
            Some(RowTarget::Reply(agent_id)) => self.reply_attachments.entry(agent_id).or_default(),
            Some(RowTarget::NewDraft) => &mut self.new_draft_attachments,
            _ => return false,
        };
        let had = !attachments.is_empty();
        attachments.clear();
        if had {
            self.attachments_dirty = true;
            cx.notify();
        }
        had
    }

    /// Regenerates the listing from the registry: per-line buffers are
    /// created or edited as needed, arranged (with reply drafts after
    /// their rows), and highlights reapplied. The cursor
    /// follows its line's buffer through the rearrangement.
    pub fn sync(
        &mut self,
        registry: &AgentRegistry,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let expanded = self
            .expanded_folds
            .lock()
            .map_or_else(|_| HashSet::new(), |expanded| expanded.clone());
        let mut lines = visible_lines(generate_dashboard(registry), &expanded);
        for line in &mut lines {
            if line
                .fold
                .as_ref()
                .is_some_and(|fold| !expanded.contains(&fold.parent_agent))
            {
                line.span(Some(DashClass::Muted), |text| text.push_str(" ›"));
            }
        }

        // Empty reply drafts the cursor has left are dead weight; drop them.
        let cursor_key = self.cursor_key(cx);
        let pending = self.pending_cursor.clone();
        let empty_replies = self
            .replies
            .iter()
            .copied()
            .filter(|agent_id| {
                let key = LineKey::Reply(*agent_id);
                Some(&key) != cursor_key.as_ref()
                    && Some(&key) != pending.as_ref()
                    && self
                        .buffers
                        .get(&key)
                        .is_some_and(|buffer| buffer.read(cx).is_empty())
                    && self
                        .reply_attachments
                        .get(agent_id)
                        .is_none_or(Vec::is_empty)
            })
            .collect::<Vec<_>>();
        for agent_id in empty_replies {
            self.replies.retain(|reply| *reply != agent_id);
            self.buffers.remove(&LineKey::Reply(agent_id));
            self.reply_subscriptions.remove(&agent_id);
            self.reply_attachments.remove(&agent_id);
            self.attachments_dirty = true;
        }
        if self
            .new_draft
            .as_ref()
            .is_some_and(|(buffer, _, _)| buffer.read(cx).is_empty())
            && self.new_draft_attachments.is_empty()
            && cursor_key != Some(LineKey::NewDraft)
            && pending != Some(LineKey::NewDraft)
        {
            self.new_draft = None;
            self.new_draft_attachments.clear();
            self.attachments_dirty = true;
            self.buffers.remove(&LineKey::NewDraft);
        }

        // Interleave: each reply draft directly under its agent's row;
        // drafts whose row is folded away trail the listing so they are
        // never lost off-screen.
        let mut order = Vec::new();
        if self.new_draft.is_some() {
            order.push(LineKey::NewDraft);
        }
        let mut rail_tail = None::<(LineKey, LineKey)>;
        let mut orphans = self.replies.clone();
        for line in &lines {
            order.push(line.key.clone());
            if line.tail {
                let first = rail_tail
                    .as_ref()
                    .map_or_else(|| line.key.clone(), |(first, _)| first.clone());
                rail_tail = Some((first, line.key.clone()));
            }
            let reply = match line.target {
                RowTarget::Stream {
                    root: Some(agent_id),
                    ..
                }
                | RowTarget::Agent(agent_id) => Some(agent_id),
                _ => None,
            };
            if let Some(agent_id) = reply.filter(|agent_id| self.replies.contains(agent_id)) {
                orphans.retain(|orphan| *orphan != agent_id);
                let reply = LineKey::Reply(agent_id);
                order.push(reply.clone());
                if line.tail
                    && let Some((_, last)) = &mut rail_tail
                {
                    *last = reply;
                }
            }
        }
        for agent_id in orphans {
            order.push(LineKey::Reply(agent_id));
        }

        // Create/refresh the listing buffers.
        let mut edited = std::collections::HashSet::new();
        for line in &lines {
            let buffer = self.buffers.entry(line.key.clone()).or_insert_with(|| {
                cx.new(|cx| {
                    let mut buffer = Buffer::local("", cx);
                    buffer.set_capability(Capability::Read, cx);
                    buffer
                })
            });
            if buffer.read(cx).text() != line.text {
                buffer.update(cx, |buffer, cx| {
                    let len = buffer.len();
                    buffer.edit([(0..len, line.text.as_str())], None, cx);
                });
                edited.insert(line.key.clone());
            }
        }

        self.ensure_headerless(cx);

        // Arrange excerpts when the order changed; path keys are display
        // indexes, and a buffer setting a new path leaves its old one.
        let order_changed = order != self.order;
        if order_changed || !edited.is_empty() {
            let old_len = self.order.len();
            self.multi_buffer.update(cx, |multi_buffer, cx| {
                for (index, key) in order.iter().enumerate() {
                    let Some(buffer) = self.buffers.get(key) else {
                        continue;
                    };
                    multi_buffer.set_excerpts_for_path(
                        PathKey::sorted(index as u64),
                        buffer.clone(),
                        [Point::zero()..buffer.read(cx).max_point()],
                        0,
                        cx,
                    );
                }
                for stale in order.len()..old_len {
                    multi_buffer.remove_excerpts(PathKey::sorted(stale as u64), cx);
                }
            });
        }
        // Prune buffers for lines that fell out of the listing (their
        // excerpts are gone); open drafts always stay.
        self.buffers.retain(|key, _| {
            order.contains(key) || matches!(key, LineKey::Reply(_) | LineKey::NewDraft)
        });

        self.targets = lines
            .iter()
            .map(|line| (line.key.clone(), line.target.clone()))
            .collect();
        for agent_id in &self.replies {
            self.targets
                .insert(LineKey::Reply(*agent_id), RowTarget::Reply(*agent_id));
        }
        if self.new_draft.is_some() {
            self.targets.insert(LineKey::NewDraft, RowTarget::NewDraft);
        }

        // The cursor follows its buffer: reposition only when the buffer
        // moved or its text was rewritten under the cursor (or a fresh
        // reply draft claims it).
        let moved = |key: &LineKey| {
            self.order.iter().position(|entry| entry == key)
                != order.iter().position(|entry| entry == key)
        };
        let first_population = self.order.is_empty() && !order.is_empty();
        let restore = match &self.pending_cursor {
            Some(key) if order.contains(key) => Some(key.clone()),
            _ => match &cursor_key {
                Some(key) if order.contains(key) && (moved(key) || edited.contains(key)) => {
                    Some(key.clone())
                }
                _ if first_population => order.first().cloned(),
                _ => None,
            },
        };
        self.pending_cursor = None;
        self.order = order;
        if let Some(key) = restore {
            self.move_cursor_to(&key, window, cx);
        }

        self.apply_folds(&lines);
        self.apply_rail_tail_elision(rail_tail, order_changed, cx);
        self.apply_highlights(&lines, cx);
        self.apply_reply_chrome(registry, cx);
        self.apply_attachment_blocks(cx);
    }

    fn apply_folds(&mut self, lines: &[Line]) {
        self.folds = lines
            .iter()
            .filter_map(|line| line.fold.clone())
            .map(|fold| (fold.parent_agent, fold))
            .collect::<HashMap<_, _>>();
    }

    fn apply_rail_tail_elision(
        &mut self,
        boundary: Option<(LineKey, LineKey)>,
        order_changed: bool,
        cx: &mut Context<Workspace>,
    ) {
        if !order_changed
            && self.rail_tail.as_ref().map(|(_, current)| current) == boundary.as_ref()
        {
            return;
        }
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let properties = boundary.as_ref().and_then(|(first, last)| {
            let first = self.buffers.get(first)?;
            let last = self.buffers.get(last)?;
            let start = snapshot.anchor_in_excerpt(first.read(cx).anchor_before(0))?;
            let end =
                snapshot.anchor_in_excerpt(last.read(cx).anchor_after(last.read(cx).len()))?;
            Some(DisplayElisionProperties {
                range: start..end,
                tail_rows: 0,
                height: Some(1),
                style: BlockStyle::Flex,
                render: Arc::new(|cx| render_rail_tail(cx).into_any_element()),
                priority: 0,
                type_tag: None,
            })
        });
        match (self.rail_tail.take(), properties, boundary) {
            (Some((id, _)), Some(properties), Some(boundary)) => {
                self.editor.update(cx, |editor, cx| {
                    editor.update_display_elisions([(id, properties)], None, cx)
                });
                self.rail_tail = Some((id, boundary));
            }
            (Some((id, _)), _, _) => {
                self.editor.update(cx, |editor, cx| {
                    editor.remove_display_elisions([id].into_iter().collect(), None, cx)
                });
            }
            (None, Some(properties), Some(boundary)) => {
                let ids = self.editor.update(cx, |editor, cx| {
                    editor.insert_display_elisions([properties], None, cx)
                });
                self.rail_tail = ids.into_iter().next().map(|id| (id, boundary));
            }
            (None, _, _) => {}
        }
    }

    /// Places the cursor at the start of a key's buffer.
    fn move_cursor_to(&self, key: &LineKey, window: &mut Window, cx: &mut Context<Workspace>) {
        let Some(buffer) = self.buffers.get(key) else {
            return;
        };
        // Right-biased, like the transcript's prompt anchor: the cursor
        // stays ahead of same-position inlays (the draft placeholder).
        let anchor = buffer.read(cx).anchor_after(0);
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let Some(anchor) = snapshot.anchor_in_excerpt(anchor) else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_anchor_ranges([anchor..anchor]);
            });
        });
    }

    /// The key of the buffer the cursor is in.
    fn cursor_key(&self, cx: &mut Context<Workspace>) -> Option<LineKey> {
        let buffer_id = self.cursor_buffer(cx)?;
        self.buffers
            .iter()
            .find(|(_, buffer)| buffer.read(cx).remote_id() == buffer_id)
            .map(|(key, _)| key.clone())
    }

    fn cursor_buffer(&self, cx: &mut Context<Workspace>) -> Option<BufferId> {
        self.editor.update(cx, |editor, cx| {
            let head = editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx))
                .head();
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            snapshot
                .point_to_buffer_offset(head)
                .map(|(buffer, _)| buffer.remote_id())
        })
    }

    /// The row under the cursor.
    pub fn cursor_target(&self, cx: &mut Context<Workspace>) -> Option<RowTarget> {
        if self.cursor_on_rail_tail(cx) {
            return None;
        }
        let key = self.cursor_key(cx)?;
        self.targets.get(&key).cloned()
    }

    fn cursor_on_rail_tail(&self, cx: &mut Context<Workspace>) -> bool {
        let Some((id, _)) = self.rail_tail else {
            return false;
        };
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let head = editor.selections.newest::<Point>(&snapshot).head();
            let row = head.to_display_point(&snapshot).row();
            snapshot
                .display_elisions_in_range(row..DisplayRow(row.0 + 1))
                .any(|candidate| candidate == id)
        })
    }

    pub fn toggle_subagents(&mut self, cx: &mut Context<Workspace>) -> bool {
        let parent = match self.cursor_target(cx) {
            Some(RowTarget::Stream {
                root: Some(agent_id),
                ..
            })
            | Some(RowTarget::Agent(agent_id)) => agent_id,
            _ => return false,
        };
        self.toggle_subagents_for(parent, cx)
    }

    pub(crate) fn toggle_subagents_for(
        &mut self,
        parent: AgentId,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let Some(parent_key) = self.folds.get(&parent).map(|fold| fold.parent.clone()) else {
            return false;
        };
        if let Ok(mut expanded) = self.expanded_folds.lock()
            && !expanded.remove(&parent)
        {
            expanded.insert(parent);
        } else {
            // The branch is collapsing. Keep the cursor and preview attached
            // to its parent instead of whichever excerpt takes the old slot.
            self.pending_cursor = Some(parent_key);
        }
        cx.notify();
        true
    }

    fn apply_highlights(&self, lines: &[Line], cx: &mut Context<Workspace>) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let mut by_class: Vec<(DashClass, Vec<Range<multi_buffer::Anchor>>)> = DashClass::ALL
            .into_iter()
            .map(|class| (class, Vec::new()))
            .collect();
        for line in lines {
            let Some(buffer) = self.buffers.get(&line.key) else {
                continue;
            };
            let buffer_snapshot = buffer.read(cx).snapshot();
            for (class, range) in &line.spans {
                let clamp = |offset: usize| offset.min(buffer_snapshot.len());
                let Some(start) =
                    snapshot.anchor_in_excerpt(buffer_snapshot.anchor_before(clamp(range.start)))
                else {
                    continue;
                };
                let Some(end) =
                    snapshot.anchor_in_excerpt(buffer_snapshot.anchor_before(clamp(range.end)))
                else {
                    continue;
                };
                if let Some((_, ranges)) = by_class.iter_mut().find(|(entry, _)| entry == class) {
                    ranges.push(start..end);
                }
            }
        }
        self.editor.update(cx, |editor, cx| {
            for (class, ranges) in by_class {
                editor.highlight_text(class.key(), ranges, class.style(cx), cx);
            }
        });
    }

    fn apply_attachment_blocks(&mut self, cx: &mut Context<Workspace>) {
        if !self.attachments_dirty {
            return;
        }
        self.attachments_dirty = false;
        if !self.attachment_blocks.is_empty() {
            self.editor.update(cx, |editor, cx| {
                editor.remove_blocks(
                    std::mem::take(&mut self.attachment_blocks)
                        .into_iter()
                        .collect::<collections::HashSet<_>>(),
                    None,
                    cx,
                );
            });
        }
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let drafts = self
            .reply_attachments
            .iter()
            .map(|(agent_id, attachments)| (LineKey::Reply(*agent_id), attachments))
            .chain(std::iter::once((
                LineKey::NewDraft,
                &self.new_draft_attachments,
            )));
        let blocks = drafts
            .filter_map(|(key, attachments)| {
                if attachments.is_empty() {
                    return None;
                }
                let buffer = self.buffers.get(&key)?.read(cx);
                let anchor = snapshot.anchor_in_excerpt(buffer.anchor_after(buffer.len()))?;
                Some(crate::style::attachment_block(anchor, attachments))
            })
            .collect::<Vec<_>>();
        self.attachment_blocks = self
            .editor
            .update(cx, |editor, cx| editor.insert_blocks(blocks, None, cx));
    }

    /// Reply-draft chrome: draft text in the user-message accent plus a
    /// placeholder inlay naming the addressee while the draft is empty.
    /// No gutter stripe here — that belongs to the transcript's prompt;
    /// in the listing the accent text is marker enough.
    fn apply_reply_chrome(&mut self, registry: &AgentRegistry, cx: &mut Context<Workspace>) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let to_remove = std::mem::take(&mut self.placeholder_ids);
        let mut inlays = Vec::new();
        let mut draft_text_ranges = Vec::new();
        let drafts = self
            .replies
            .iter()
            .map(|agent_id| {
                (
                    LineKey::Reply(*agent_id),
                    format!("reply to {}…", registry.agent_human_name(*agent_id)),
                )
            })
            .chain(
                self.new_draft
                    .as_ref()
                    .map(|(_, _, summary)| (LineKey::NewDraft, format!("new agent · {summary}…"))),
            );
        for (index, (key, placeholder)) in drafts.enumerate() {
            let Some(buffer) = self.buffers.get(&key) else {
                continue;
            };
            let buffer = buffer.read(cx);
            let buffer_snapshot = buffer.snapshot();
            let Some(start) = snapshot.anchor_in_excerpt(buffer_snapshot.anchor_before(0)) else {
                continue;
            };
            let Some(end) =
                snapshot.anchor_in_excerpt(buffer_snapshot.anchor_before(buffer_snapshot.len()))
            else {
                continue;
            };
            // Draft text wears the user-message accent, same as typed
            // prompts everywhere else in rho.
            draft_text_ranges.push(start..end);
            if buffer.is_empty() {
                // Right-biased like the transcript's prompt placeholder, so
                // the cursor renders before the hint, not after it.
                let Some(position) = snapshot.anchor_in_excerpt(buffer_snapshot.anchor_after(0))
                else {
                    continue;
                };
                let inlay = Inlay::custom(PLACEHOLDER_ID_BASE + index, position, placeholder);
                self.placeholder_ids.push(inlay.id);
                inlays.push(inlay);
            } else if let LineKey::Reply(agent_id) = key
                && !self.targets.iter().any(|(line_key, target)| {
                    !matches!(line_key, LineKey::Reply(_))
                        && matches!(
                            target,
                            RowTarget::Agent(target_id)
                                | RowTarget::Stream {
                                    root: Some(target_id),
                                    ..
                                } if *target_id == agent_id
                        )
                })
            {
                let Some(position) = snapshot.anchor_in_excerpt(buffer_snapshot.anchor_after(0))
                else {
                    continue;
                };
                let inlay = Inlay::custom(
                    PLACEHOLDER_ID_BASE + index,
                    position,
                    format!("reply to {} · ", registry.agent_human_name(agent_id)),
                );
                self.placeholder_ids.push(inlay.id);
                inlays.push(inlay);
            } else if key == LineKey::NewDraft
                && let Some((_, _, summary)) = &self.new_draft
            {
                let Some(position) =
                    snapshot.anchor_in_excerpt(buffer_snapshot.anchor_after(buffer_snapshot.len()))
                else {
                    continue;
                };
                let inlay = Inlay::custom(
                    PLACEHOLDER_ID_BASE + index,
                    position,
                    format!("  · {summary}"),
                );
                self.placeholder_ids.push(inlay.id);
                inlays.push(inlay);
            }
        }
        let draft_style = crate::style::StyleClass::UserMessage.resolve(cx);
        self.editor.update(cx, |editor, cx| {
            editor.splice_inlays(&to_remove, inlays, cx);
            editor.highlight_text(DRAFT_TEXT_KEY, draft_text_ranges, draft_style, cx);
        });
    }
}

/// The quiet-tail placeholder row. The cursor itself is the selection
/// indicator — rows carry no selected styling.
fn render_rail_tail(cx: &mut BlockContext<'_, '_>) -> impl IntoElement {
    let text_style = cx.editor_style.text.clone();
    let color = if cx.selected {
        text_style.color
    } else {
        crate::style::hint_color(cx.app)
    };
    div()
        .block_mouse_except_scroll()
        .pl(cx.anchor_x)
        .h(cx.line_height)
        .flex()
        .items_center()
        .font_family(text_style.font_family)
        .text_size(text_style.font_size)
        .line_height(text_style.line_height)
        .text_color(color)
        .child("…")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DashClass {
    Muted,
    /// A finished turn awaiting the user; with the lamps gone, the row text
    /// itself carries the emphasis.
    Urgent,
    /// Blocked on the user (error or unfinished turn): error color and a
    /// trailing glyph instead of spelling it out.
    Blocked,
}

impl DashClass {
    const ALL: [DashClass; 3] = [DashClass::Muted, DashClass::Urgent, DashClass::Blocked];

    fn key(self) -> HighlightKey {
        let slot = match self {
            DashClass::Muted => 0,
            DashClass::Urgent => 1,
            DashClass::Blocked => 2,
        };
        HighlightKey::SyntaxTreeView(DASHBOARD_KEY_BASE + slot)
    }

    fn style(self, cx: &App) -> HighlightStyle {
        let color = match self {
            DashClass::Muted => cx.theme().colors().text_muted,
            DashClass::Urgent => cx.theme().colors().text_accent,
            DashClass::Blocked => cx.theme().status().error,
        };
        HighlightStyle {
            color: Some(color.into()),
            ..HighlightStyle::default()
        }
    }
}

/// One row of the assembled dashboard, in display order.
#[derive(Debug, PartialEq)]
pub enum RailRow<'a> {
    /// A daemon's section starts; everything of its is nested beneath.
    HostHeader(HostId),
    /// A workstream-group section starts; its member tasks follow.
    GroupHeader { name: &'a str, indent: usize },
    Task {
        topic: &'a Workstream,
        indent: usize,
    },
}

/// Assembles the dashboard from the split rows: the whole structure as
/// plain data, decided here and only serialized by the caller.
///
/// A section anchors a group at its best-sorted member's position and gathers
/// the rest of that section's group beneath it. Listed and quiet-tail rows
/// are assembled separately so the tail remains one contiguous elision.
fn rail_rows(display: Vec<&Workstream>, multihost: bool) -> Vec<RailRow<'_>> {
    if !multihost {
        return group_rows(&display, 0);
    }
    // Hosts section exactly as groups do — anchored at their best-sorted
    // workstream — so attaching a second daemon reorders nothing that was
    // already urgent; it only draws a line around where each row lives.
    let mut rows = Vec::new();
    let mut seen_hosts = std::collections::BTreeSet::new();
    for (index, topic) in display.iter().enumerate() {
        if !seen_hosts.insert(topic.host) {
            continue;
        }
        rows.push(RailRow::HostHeader(topic.host));
        let members = display[index..]
            .iter()
            .copied()
            .filter(|member| member.host == topic.host)
            .collect::<Vec<_>>();
        rows.extend(group_rows(&members, 1));
    }
    rows
}

/// One host's section (or the whole rail when only one is attached), with
/// its group sections nested one level deeper than `indent`.
fn group_rows<'a>(display: &[&'a Workstream], indent: usize) -> Vec<RailRow<'a>> {
    let mut rows = Vec::new();
    let mut seen_groups = std::collections::BTreeSet::new();
    for (index, topic) in display.iter().enumerate() {
        match &topic.group {
            None => rows.push(RailRow::Task { topic, indent }),
            Some(group) => {
                if !seen_groups.insert(group.clone()) {
                    continue;
                }
                rows.push(RailRow::GroupHeader {
                    name: group,
                    indent,
                });
                rows.extend(
                    display[index..]
                        .iter()
                        .filter(|member| member.group.as_ref() == Some(group))
                        .map(|member| RailRow::Task {
                            topic: member,
                            indent: indent + 1,
                        }),
                );
            }
        }
    }
    rows
}

/// One generated dashboard line: its identity, text, spans (offsets
/// relative to the line), and what acting on it means.
struct Line {
    key: LineKey,
    text: String,
    spans: Vec<(DashClass, Range<usize>)>,
    target: RowTarget,
    fold: Option<FoldSpec>,
    tail: bool,
}

impl Line {
    fn new(key: LineKey, target: RowTarget) -> Self {
        Self {
            key,
            text: String::new(),
            spans: Vec::new(),
            target,
            fold: None,
            tail: false,
        }
    }

    fn span(&mut self, class: Option<DashClass>, write: impl FnOnce(&mut String)) {
        let start = self.text.len();
        write(&mut self.text);
        if let Some(class) = class {
            self.spans.push((class, start..self.text.len()));
        }
    }
}

/// Serializes the registry into the dashboard listing.
#[cfg(test)]
fn generate(registry: &AgentRegistry) -> Vec<Line> {
    generate_dashboard(registry).into_iter().skip(1).collect()
}

fn generate_dashboard(registry: &AgentRegistry) -> Vec<Line> {
    let mut iris = Line::new(LineKey::Iris, RowTarget::Iris);
    iris.text.push_str("iris · listening");
    let mut lines = vec![iris];
    let (listed, folded) = registry.split_rows();
    let multihost = registry.host_count() > 1;
    for (section, tail) in [(listed, false), (folded, true)] {
        for row in rail_rows(section, multihost) {
            match row {
                RailRow::HostHeader(host) => {
                    let mut line = Line::new(LineKey::Host { host, tail }, RowTarget::None);
                    let name = registry.host_name(host);
                    line.span(Some(DashClass::Muted), |text| text.push_str(&name));
                    line.tail = tail;
                    lines.push(line);
                }
                RailRow::GroupHeader { name, indent } => {
                    let mut line = Line::new(
                        LineKey::Group {
                            name: name.to_owned(),
                            tail,
                        },
                        RowTarget::None,
                    );
                    line.span(Some(DashClass::Muted), |text| {
                        text.push_str(&"  ".repeat(indent));
                        text.push_str(name);
                    });
                    line.tail = tail;
                    lines.push(line);
                }
                RailRow::Task { topic, indent } => {
                    let mut task = task_lines(topic, indent, registry);
                    task.iter_mut().for_each(|line| line.tail = tail);
                    lines.extend(task);
                }
            }
        }
    }
    lines
}

fn visible_lines(lines: Vec<Line>, expanded: &HashSet<AgentId>) -> Vec<Line> {
    let hidden = lines
        .iter()
        .filter_map(|line| line.fold.as_ref())
        .filter(|fold| !expanded.contains(&fold.parent_agent))
        .flat_map(|fold| fold.descendants.iter().cloned())
        .collect::<HashSet<_>>();
    lines
        .into_iter()
        .filter(|line| !hidden.contains(&line.key))
        .collect()
}

/// The status text and emphasis a row carries in place of the old attention
/// lamp: working rows show activity, finished rows show the turn report's
/// one-liner — lit when it needs the user, dimmed for a dismissable FYI.
struct RowStatus<'a> {
    attention: UiAttention,
    activity: Option<&'a str>,
    report: Option<&'a rho_ui_proto::UiTurnReport>,
}

impl RowStatus<'_> {
    fn for_agent(registry: &AgentRegistry, agent_id: AgentId) -> RowStatus<'_> {
        RowStatus {
            attention: registry.attention(agent_id),
            activity: registry.agent_activity(agent_id),
            report: registry.agent_turn_report(agent_id),
        }
    }

    fn bare(attention: UiAttention) -> RowStatus<'static> {
        RowStatus {
            attention,
            activity: None,
            report: None,
        }
    }

    fn title_class(&self) -> Option<DashClass> {
        match self.attention {
            UiAttention::NeedsInput => Some(DashClass::Blocked),
            UiAttention::Pending => match self.report {
                Some(report) if !report.needs_you => Some(DashClass::Muted),
                _ => Some(DashClass::Urgent),
            },
            UiAttention::Working | UiAttention::Quiet => None,
        }
    }

    fn suffix(&self) -> Option<(DashClass, String)> {
        let activity = || {
            self.activity
                .map(|activity| (DashClass::Muted, format!(" · {activity}")))
        };
        match self.attention {
            UiAttention::NeedsInput => Some((DashClass::Blocked, " ◆".to_owned())),
            UiAttention::Pending => match self.report {
                Some(report) if report.needs_you => {
                    Some((DashClass::Muted, format!(" · {}", report.one_liner)))
                }
                Some(report) => Some((DashClass::Muted, format!(" ✓ {}", report.one_liner))),
                None => activity(),
            },
            UiAttention::Working | UiAttention::Quiet => activity(),
        }
    }

    fn apply(&self, line: &mut Line, title: &str) {
        line.span(self.title_class(), |text| text.push_str(title));
        if let Some((class, suffix)) = self.suffix() {
            line.span(Some(class), |text| text.push_str(&suffix));
        }
    }
}

/// A workstream is flat in the common single-root case. Multiple roots make
/// the container meaningful, so it becomes a header followed by explicit,
/// human-named root rows. Every descendant is a normal actionable agent row;
/// inline editor creases collapse each contiguous subtree onto its parent.
fn task_lines(topic: &Workstream, indent: usize, registry: &AgentRegistry) -> Vec<Line> {
    let tree = registry.ordered_workstream_tree(topic);
    let roots = tree
        .iter()
        .filter(|(_, depth)| *depth == 0)
        .map(|(agent, _)| *agent)
        .collect::<Vec<_>>();
    let attention = |root: AgentId| {
        let Some(index) = tree.iter().position(|(agent, _)| agent.agent_id == root) else {
            return UiAttention::Quiet;
        };
        let end = tree[index + 1..]
            .iter()
            .position(|(_, depth)| *depth == 0)
            .map_or(tree.len(), |offset| index + 1 + offset);
        tree[index..end]
            .iter()
            .map(|(agent, _)| registry.attention(agent.agent_id))
            .max()
            .unwrap_or_default()
    };
    let aggregate = roots
        .iter()
        .map(|root| attention(root.agent_id))
        .max()
        .unwrap_or(UiAttention::Quiet);

    if roots.is_empty() {
        return vec![workstream_line(
            topic,
            indent,
            None,
            RowStatus::bare(aggregate),
        )];
    }

    let singleton = roots.len() == 1;
    let mut lines = if singleton {
        vec![workstream_line(
            topic,
            indent,
            Some(roots[0].agent_id),
            RowStatus {
                attention: attention(roots[0].agent_id),
                ..RowStatus::for_agent(registry, roots[0].agent_id)
            },
        )]
    } else {
        vec![workstream_line(
            topic,
            indent,
            None,
            RowStatus::bare(aggregate),
        )]
    };
    let mut agent_line_indexes = Vec::with_capacity(tree.len());
    for (index, (agent, depth)) in tree.iter().enumerate() {
        if singleton && index == 0 {
            agent_line_indexes.push(0);
            continue;
        }
        let row_depth = if singleton { *depth } else { depth + 1 };
        agent_line_indexes.push(lines.len());
        let status = RowStatus {
            attention: if *depth == 0 {
                attention(agent.agent_id)
            } else {
                registry.attention(agent.agent_id)
            },
            ..RowStatus::for_agent(registry, agent.agent_id)
        };
        lines.push(agent_line(agent, indent, row_depth, status, registry));
    }

    for (index, (agent, depth)) in tree.iter().enumerate() {
        let subtree_end = tree[index + 1..]
            .iter()
            .position(|(_, candidate_depth)| candidate_depth <= depth)
            .map_or(tree.len(), |offset| index + 1 + offset);
        if subtree_end == index + 1 {
            continue;
        }
        let parent_line = agent_line_indexes[index];
        lines[parent_line].fold = Some(FoldSpec {
            parent_agent: agent.agent_id,
            parent: lines[parent_line].key.clone(),
            descendants: agent_line_indexes[index + 1..subtree_end]
                .iter()
                .map(|line| lines[*line].key.clone())
                .collect(),
            descendant_count: subtree_end - index - 1,
        });
    }
    lines
}

fn workstream_line(
    topic: &Workstream,
    indent: usize,
    root: Option<AgentId>,
    status: RowStatus<'_>,
) -> Line {
    let title = if topic.name.trim().is_empty() {
        "Untitled workstream".to_owned()
    } else {
        topic.name.clone()
    };

    let mut line = Line::new(
        LineKey::Stream(topic.workstream_id),
        RowTarget::Stream {
            workstream_id: topic.workstream_id,
            root,
        },
    );
    // Rows, headers, and reply drafts all sit flush at one level — the
    // container's margin does the breathing, not per-row indents. The
    // cursor is the selection indicator; rows carry no selected styling.
    if indent > 0 {
        line.span(None, |text| text.push_str(&"  ".repeat(indent)));
    }
    if topic.pinned {
        line.span(None, |text| text.push_str("◆ "));
    }
    status.apply(&mut line, &title);
    line
}

fn agent_line(
    agent: &rho_ui_proto::UiAgentSummary,
    indent: usize,
    depth: usize,
    status: RowStatus<'_>,
    registry: &AgentRegistry,
) -> Line {
    let mut line = Line::new(
        LineKey::Agent(agent.agent_id),
        RowTarget::Agent(agent.agent_id),
    );
    line.span(None, |text| text.push_str(&"  ".repeat(depth + indent)));
    status.apply(&mut line, &registry.agent_human_name(agent.agent_id));
    line
}

#[cfg(test)]
mod tests {
    use rho_core::UnixMs;
    use rho_ui_proto::{
        AgentIdDomain, AgentRole, UiAgentSummary, UiWorkstream, WorkspaceInfo, WorkstreamId,
    };

    use super::*;

    /// Pin state fixture shorthand, in the shape the old tag `Status` had.
    #[derive(Clone, Copy, PartialEq)]
    enum Status {
        Normal,
        Pinned,
    }

    /// Freshly-engaged fixture (`last_active` at now + `id`) for deterministic
    /// active-bucket ordering.
    fn agent(id: u64, status: Status, updated_at: u64) -> UiAgentSummary {
        UiAgentSummary {
            agent_id: AgentId::from_counter(id, &AgentIdDomain(0)).unwrap(),
            parent_agent: None,
            display_name: None,
            created_at: UnixMs(id),
            updated_at: UnixMs(updated_at),
            role: AgentRole::default(),
            workspace: WorkspaceInfo::UserCheckout {
                repo: "/tmp".into(),
            },
            attention: UiAttention::Quiet,
            last_active: UnixMs(crate::workspace::now_ms() + id),
            hidden: false,
            last_user_message_text: String::new(),
            activity: None,
            turn_report: None,
            workstream: WorkstreamId(1),
            labels: match status {
                Status::Normal => Vec::new(),
                Status::Pinned => vec![crate::registry::PIN_LABEL.to_owned()],
            },
        }
    }

    fn topic(status: Status, agents: Vec<UiAgentSummary>) -> Workstream {
        Workstream {
            host: HostId::default(),
            workstream_id: WorkstreamId(1),
            name: "topic".to_owned(),
            pinned: status == Status::Pinned,
            hidden: false,
            group: None,
            agents,
        }
    }

    fn install(registry: &mut AgentRegistry, topic: &Workstream) {
        let mut labels = Vec::new();
        if topic.pinned {
            labels.push(crate::registry::PIN_LABEL.to_owned());
        }
        registry.set_data(
            vec![UiWorkstream {
                workstream_id: topic.workstream_id,
                name: topic.name.clone(),
                labels,
            }],
            topic.agents.clone(),
        );
    }

    /// Bare workstream fixture for row-assembly tests: identity and group
    /// only, no members.
    fn stream(id: u64, group: Option<&str>) -> Workstream {
        stream_on(HostId::default(), id, group)
    }

    fn stream_on(host: HostId, id: u64, group: Option<&str>) -> Workstream {
        Workstream {
            host,
            workstream_id: WorkstreamId(id),
            name: format!("ws-{id}"),
            pinned: false,
            hidden: false,
            group: group.map(str::to_owned),
            agents: Vec::new(),
        }
    }

    fn ids(rows: &[RailRow<'_>]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                RailRow::HostHeader(host) => format!("<{host}>"),
                RailRow::GroupHeader { name, indent } => {
                    format!("{}[{name}]", "  ".repeat(*indent))
                }
                RailRow::Task { topic, indent } => {
                    format!("{}{}", "  ".repeat(*indent), topic.name)
                }
            })
            .collect()
    }

    #[test]
    fn iris_is_the_first_dashboard_target() {
        let topic = topic(Status::Normal, vec![agent(1, Status::Normal, 10)]);
        let mut registry = AgentRegistry::default();
        install(&mut registry, &topic);

        let lines = generate_dashboard(&registry);
        assert_eq!(lines[0].key, LineKey::Iris);
        assert_eq!(lines[0].text, "iris · listening");
        assert_eq!(lines[0].target, RowTarget::Iris);
        assert!(matches!(lines[1].target, RowTarget::Stream { .. }));
    }

    fn split_agents<'a>(
        topic: &'a Workstream,
        registry: &AgentRegistry,
    ) -> (Vec<&'a UiAgentSummary>, Vec<&'a UiAgentSummary>) {
        registry.split_workstream_agents(topic)
    }

    #[test]
    fn groups_anchor_at_first_member_and_gather_the_rest() {
        let rows = [
            stream(1, None),
            stream(2, Some("infra")),
            stream(3, None),
            stream(4, Some("infra")),
        ];
        let assembled = rail_rows(rows.iter().collect(), false);
        assert_eq!(
            ids(&assembled),
            ["ws-1", "[infra]", "  ws-2", "  ws-4", "ws-3"]
        );
    }

    #[test]
    fn groups_anchor_in_stateful_order_instead_of_at_the_top() {
        let mut agents = (1..=4)
            .map(|id| agent(id, Status::Normal, 10))
            .collect::<Vec<_>>();
        for (index, agent) in agents.iter_mut().enumerate() {
            agent.workstream = WorkstreamId(index as u64 + 1);
        }
        let workstreams = (1..=4)
            .map(|id| UiWorkstream {
                workstream_id: WorkstreamId(id),
                name: format!("ws-{id}"),
                labels: if matches!(id, 1 | 3) {
                    vec!["group:infra".to_owned()]
                } else {
                    Vec::new()
                },
            })
            .collect();
        let mut registry = AgentRegistry::default();
        registry.set_data(workstreams, agents);

        let lines = generate(&registry)
            .into_iter()
            .map(|line| line.text)
            .collect::<Vec<_>>();

        // Stateful order is 4, 3, 2, 1. The infra section anchors at 3 and
        // gathers 1 beneath it; ungrouped 4 remains above the section.
        assert_eq!(lines, ["ws-4", "infra", "  ws-3", "  ws-1", "ws-2"]);
    }

    #[test]
    fn hosts_section_the_rail_and_nest_their_groups() {
        let rows = [
            stream_on(HostId(0), 1, None),
            stream_on(HostId(1), 2, Some("infra")),
            stream_on(HostId(0), 3, Some("infra")),
            stream_on(HostId(1), 4, None),
        ];

        let assembled = rail_rows(rows.iter().collect(), true);

        // Each host anchors where its best-sorted workstream sits, and its
        // groups indent one level inside the host's section.
        assert_eq!(
            ids(&assembled),
            [
                "<host0>",
                "  ws-1",
                "  [infra]",
                "    ws-3",
                "<host1>",
                "  [infra]",
                "    ws-2",
                "  ws-4",
            ]
        );
    }

    #[test]
    fn a_single_host_draws_no_header() {
        let rows = [stream(1, None), stream(2, Some("infra"))];

        assert_eq!(
            ids(&rail_rows(rows.iter().collect(), false)),
            ["ws-1", "[infra]", "  ws-2"]
        );
    }

    #[test]
    fn group_split_keeps_the_folded_section_contiguous() {
        let listed = [stream(1, Some("infra")), stream(2, None)];
        let folded = [stream(3, Some("infra"))];

        assert_eq!(
            ids(&rail_rows(listed.iter().collect(), false)),
            ["[infra]", "  ws-1", "ws-2"]
        );
        assert_eq!(
            ids(&rail_rows(folded.iter().collect(), false)),
            ["[infra]", "  ws-3"]
        );
    }

    #[test]
    fn empty_section_gets_no_rows() {
        let listed = [stream(1, None)];
        let assembled = rail_rows(listed.iter().collect(), false);
        assert_eq!(ids(&assembled), ["ws-1"]);
        assert!(rail_rows(Vec::new(), false).is_empty());
    }

    #[test]
    fn listing_lines_carry_targets_and_status() {
        let root = agent(1, Status::Normal, 10);
        let root_id = root.agent_id;
        let mut urgent_child = agent(2, Status::Normal, 10);
        urgent_child.parent_agent = Some(root_id);
        urgent_child.attention = UiAttention::NeedsInput;
        let child_id = urgent_child.agent_id;
        let members = vec![root, urgent_child];
        let topic = topic(Status::Normal, members);
        let mut registry = AgentRegistry::default();
        install(&mut registry, &topic);
        registry.set_attention(child_id, UiAttention::NeedsInput);

        let lines = generate(&registry);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].text.contains("topic"));
        assert_eq!(lines[0].key, LineKey::Stream(WorkstreamId(1)));
        assert!(lines[0].text.ends_with(" ◆"));
        assert!(matches!(
            lines[0].target,
            RowTarget::Stream {
                workstream_id: WorkstreamId(1),
                root: Some(agent_id),
            }
            if agent_id == root_id
        ));
        assert_eq!(lines[1].target, RowTarget::Agent(child_id));
        assert!(lines[1].text.ends_with(" ◆"));
        assert_eq!(
            lines[0].fold,
            Some(FoldSpec {
                parent_agent: root_id,
                parent: LineKey::Stream(WorkstreamId(1)),
                descendants: vec![LineKey::Agent(child_id)],
                descendant_count: 1,
            })
        );
    }

    #[test]
    fn pending_rows_split_on_turn_report() {
        let root = agent(1, Status::Normal, 10);
        let root_id = root.agent_id;
        let topic = topic(Status::Normal, vec![root]);
        let mut registry = AgentRegistry::default();
        install(&mut registry, &topic);
        registry.set_attention(root_id, UiAttention::Pending);

        // Pending without a report: emphasized title, nothing else.
        let lines = generate(&registry);
        assert!(lines[0].spans.iter().any(|(class, range)| {
            *class == DashClass::Urgent && lines[0].text[range.clone()].contains("topic")
        }));

        registry.set_turn_report(
            root_id,
            rho_ui_proto::UiTurnReport {
                needs_you: true,
                one_liner: "asking: delete old migration?".to_owned(),
            },
        );
        let lines = generate(&registry);
        assert!(lines[0].text.contains("· asking: delete old migration?"));
        assert!(lines[0].spans.iter().any(|(class, range)| {
            *class == DashClass::Urgent && lines[0].text[range.clone()].contains("topic")
        }));

        // An FYI dims the whole row and swaps the separator for a check.
        registry.set_turn_report(
            root_id,
            rho_ui_proto::UiTurnReport {
                needs_you: false,
                one_liner: "tests pass, pushed".to_owned(),
            },
        );
        let lines = generate(&registry);
        assert!(lines[0].text.contains("✓ tests pass, pushed"));
        assert!(lines[0].spans.iter().any(|(class, range)| {
            *class == DashClass::Muted && lines[0].text[range.clone()].contains("topic")
        }));

        // A new turn retires the report: the row is back to activity text.
        registry.set_attention(root_id, UiAttention::Working);
        let lines = generate(&registry);
        assert!(!lines[0].text.contains("tests pass"));
    }

    #[test]
    fn descendant_of_hidden_parent_does_not_become_a_root() {
        let mut parent = agent(1, Status::Normal, 10);
        parent.hidden = true;
        let mut child = agent(2, Status::Normal, 10);
        child.parent_agent = Some(parent.agent_id);
        child.attention = UiAttention::NeedsInput;
        let root = agent(3, Status::Normal, 10);
        let root_id = root.agent_id;
        let topic = topic(Status::Normal, vec![parent, child, root]);
        let mut registry = AgentRegistry::default();
        install(&mut registry, &topic);

        let lines = generate(&registry);
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].text.ends_with(" ◆"));
        assert!(matches!(
            lines[0].target,
            RowTarget::Stream {
                root: Some(agent_id),
                ..
            } if agent_id == root_id
        ));
    }

    #[test]
    fn multiple_roots_and_nested_subagents_get_independent_folds() {
        let mut first = agent(1, Status::Normal, 10);
        first.display_name = Some("First root".to_owned());
        let first_id = first.agent_id;
        let mut child = agent(2, Status::Normal, 10);
        child.parent_agent = Some(first_id);
        child.display_name = Some("Child".to_owned());
        let child_id = child.agent_id;
        let mut grandchild = agent(3, Status::Normal, 10);
        grandchild.parent_agent = Some(child_id);
        grandchild.display_name = Some("Grandchild".to_owned());
        let grandchild_id = grandchild.agent_id;
        let mut second = agent(4, Status::Normal, 10);
        second.display_name = Some("Second root".to_owned());
        let second_id = second.agent_id;
        let mut second_child = agent(5, Status::Normal, 10);
        second_child.parent_agent = Some(second_id);
        second_child.display_name = Some("Second child".to_owned());
        let second_child_id = second_child.agent_id;
        let topic = topic(
            Status::Normal,
            vec![first, grandchild, second_child, child, second],
        );
        let mut registry = AgentRegistry::default();
        install(&mut registry, &topic);

        let lines = generate(&registry);
        assert_eq!(
            lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            [
                "topic",
                "  Second root",
                "    Second child",
                "  First root",
                "    Child",
                "      Grandchild",
            ]
        );
        let fold = |agent_id| {
            lines
                .iter()
                .find_map(|line| {
                    line.fold
                        .as_ref()
                        .filter(|fold| fold.parent_agent == agent_id)
                })
                .unwrap()
        };
        assert_eq!(
            fold(second_id).descendants,
            [LineKey::Agent(second_child_id)]
        );
        assert_eq!(fold(second_id).descendant_count, 1);
        assert_eq!(
            fold(first_id).descendants,
            [LineKey::Agent(child_id), LineKey::Agent(grandchild_id)]
        );
        assert_eq!(fold(first_id).descendant_count, 2);
        assert_eq!(fold(child_id).descendants, [LineKey::Agent(grandchild_id)]);
        assert_eq!(fold(child_id).descendant_count, 1);

        let collapsed = visible_lines(generate(&registry), &HashSet::new());
        assert_eq!(
            collapsed
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["topic", "  Second root", "  First root"]
        );

        let expanded = visible_lines(generate(&registry), &HashSet::from([first_id]));
        assert_eq!(
            expanded
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["topic", "  Second root", "  First root", "    Child"]
        );

        let expanded = visible_lines(generate(&registry), &HashSet::from([first_id, child_id]));
        assert!(expanded.iter().any(|line| line.text == "      Grandchild"));
    }

    #[test]
    fn multiple_roots_follow_retained_engagement_order() {
        let mut release_notes = agent(1, Status::Normal, 10);
        release_notes.display_name = Some("Prepare release notes".to_owned());
        let release_id = release_notes.agent_id;
        let mut deployment = agent(2, Status::Normal, 10);
        deployment.last_user_message_text = "Verify staging deployment".to_owned();
        let deployment_id = deployment.agent_id;
        let topic = topic(Status::Normal, vec![release_notes, deployment]);
        let mut registry = AgentRegistry::default();
        install(&mut registry, &topic);

        let lines = generate(&registry);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].text, "topic");
        // Agent 2 was engaged more recently, so the registry's retained
        // order wins over the daemon snapshot order used to build `topic`.
        assert_eq!(lines[1].text, "  Verify staging deployment");
        assert_eq!(lines[2].text, "  Prepare release notes");
        assert!(matches!(
            lines[0].target,
            RowTarget::Stream { root: None, .. }
        ));
        assert_eq!(lines[1].target, RowTarget::Agent(deployment_id));
        assert_eq!(lines[2].target, RowTarget::Agent(release_id));
    }

    #[test]
    fn hidden_and_inactive_bucket_agents_move_to_the_folded_tail() {
        let inactive = agent(1, Status::Normal, 10);
        let fresh = agent(2, Status::Normal, 10);
        let mut filed = agent(3, Status::Normal, 10);
        filed.hidden = true;
        let mut summaries = vec![inactive, fresh, filed];
        summaries.extend((4..=13).map(|id| agent(id, Status::Normal, 10)));
        let topic = topic(Status::Normal, summaries);
        let mut registry = AgentRegistry::default();
        install(&mut registry, &topic);

        let (active, folded) = split_agents(&topic, &registry);
        let active = active
            .into_iter()
            .map(|summary| summary.agent_id)
            .collect::<Vec<_>>();
        let folded = folded
            .into_iter()
            .map(|summary| summary.agent_id)
            .collect::<Vec<_>>();

        assert_eq!(
            active,
            [13, 12, 11, 10, 9, 8, 7, 6, 5, 4].map(|id| AgentId::from_counter(
                id,
                &AgentIdDomain(0)
            )
            .unwrap())
        );
        assert_eq!(
            folded,
            [
                AgentId::from_counter(1, &AgentIdDomain(0)).unwrap(),
                AgentId::from_counter(2, &AgentIdDomain(0)).unwrap(),
                AgentId::from_counter(3, &AgentIdDomain(0)).unwrap(),
            ]
        );
    }

    #[test]
    fn folded_agents_sort_by_updated_at_newest_first() {
        let mut summaries = vec![
            agent(1, Status::Normal, 10),
            agent(2, Status::Normal, 30),
            agent(3, Status::Normal, 20),
        ];
        for summary in &mut summaries {
            summary.hidden = true;
        }
        let topic = topic(Status::Normal, summaries);

        let mut registry = AgentRegistry::default();
        install(&mut registry, &topic);
        let (_, folded) = split_agents(&topic, &registry);
        let folded = folded
            .into_iter()
            .map(|summary| summary.updated_at)
            .collect::<Vec<_>>();

        assert_eq!(folded, [UnixMs(30), UnixMs(20), UnixMs(10)]);
    }

    #[test]
    fn pinned_agents_stay_above_attention_bucket() {
        let quiet_pinned = agent(1, Status::Pinned, 10);
        let urgent = agent(2, Status::Normal, 10);
        let topic = topic(Status::Normal, vec![quiet_pinned, urgent.clone()]);

        let mut registry = AgentRegistry::default();
        install(&mut registry, &topic);
        registry.set_attention(urgent.agent_id, UiAttention::NeedsInput);

        let visible = split_agents(&topic, &registry)
            .0
            .into_iter()
            .map(|summary| summary.agent_id)
            .collect::<Vec<_>>();

        assert_eq!(
            visible,
            [
                AgentId::from_counter(1, &AgentIdDomain(0)).unwrap(),
                AgentId::from_counter(2, &AgentIdDomain(0)).unwrap(),
            ]
        );
    }

    #[test]
    fn active_agents_sort_by_engagement_after_pins() {
        let idle = agent(1, Status::Normal, 10);
        let pinned = agent(2, Status::Pinned, 10);
        let mut recent = agent(3, Status::Normal, 10);
        recent.last_active = UnixMs(crate::workspace::now_ms() + 100);
        let topic = topic(Status::Normal, vec![idle, pinned, recent]);

        let mut registry = AgentRegistry::default();
        install(&mut registry, &topic);
        let visible = split_agents(&topic, &registry)
            .0
            .into_iter()
            .map(|summary| summary.agent_id)
            .collect::<Vec<_>>();

        // Pins first, then by seeded engagement recency (last user message).
        assert_eq!(
            visible,
            [
                AgentId::from_counter(2, &AgentIdDomain(0)).unwrap(),
                AgentId::from_counter(3, &AgentIdDomain(0)).unwrap(),
                AgentId::from_counter(1, &AgentIdDomain(0)).unwrap(),
            ]
        );
    }

    #[test]
    fn same_topic_children_follow_their_parent() {
        let parent = agent(1, Status::Pinned, 10);
        let mut child = agent(2, Status::Normal, 10);
        child.parent_agent = Some(parent.agent_id);
        let mut grandchild = agent(3, Status::Normal, 10);
        grandchild.parent_agent = Some(child.agent_id);
        let root = agent(4, Status::Normal, 10);
        let topic = topic(Status::Normal, vec![parent, root, grandchild, child]);

        let mut registry = AgentRegistry::default();
        install(&mut registry, &topic);
        let collapsed = split_agents(&topic, &registry)
            .0
            .into_iter()
            .map(|summary| summary.agent_id)
            .collect::<Vec<_>>();
        assert_eq!(
            collapsed,
            [1, 4].map(|id| AgentId::from_counter(id, &AgentIdDomain(0)).unwrap())
        );

        registry.select_agent(AgentId::from_counter(1, &AgentIdDomain(0)).unwrap());
        let visible = split_agents(&topic, &registry)
            .0
            .into_iter()
            .map(|summary| summary.agent_id)
            .collect::<Vec<_>>();

        assert_eq!(
            visible,
            [1, 2, 3, 4].map(|id| AgentId::from_counter(id, &AgentIdDomain(0)).unwrap())
        );
    }
}
