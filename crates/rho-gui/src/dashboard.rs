//! The dashboard: the Desk document as the home surface — rho's
//! magit-status. The real per-host CRDT document is spliced into the
//! editor as writable excerpts, so headings and prose are edited
//! directly with plain vim, while generated read-only agent rows are
//! interleaved under the headings their agents are bound to (via
//! daemon-owned anchors, never text markers). Acting keys address the
//! row under the cursor: `enter` opens, `r` splices an inline reply
//! draft under the row. Generated rows and drafts are tiny buffers of
//! their own between the document slices — a refresh rearranges
//! excerpts but can never eat what the user typed.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;

use editor::{Editor, EditorMode, HighlightKey, Inlay, SizingBehavior};
use gpui::prelude::*;
use gpui::{App, Context, Entity, Focusable as _, HighlightStyle, WeakEntity, Window};
use language::{Buffer, Capability, Point};
use multi_buffer::MultiBuffer;
use multi_buffer::composition::{Composition, CompositionSpec, CutSpec, RowSpec, SectionSpec};
use project::InlayId;
use rho_ui_proto::desk::{DeskBinding, DeskHeading, DeskHeadingState, parse};
use rho_ui_proto::{AgentId, UiAttention};
use text::BufferId;
use theme::ActiveTheme as _;

use crate::registry::{AgentRegistry, HostId};
use crate::workspace::Workspace;

/// How many member tags a workstream row shows before collapsing into `+n`.
const VISIBLE_TAGS: usize = 3;

/// Highlight-key space for dashboard classes, clear of the transcript's
/// semantic and syntax key ranges.
const DASHBOARD_KEY_BASE: usize = usize::MAX - 200;

/// Inlay id space for reply-draft placeholders, clear of the lamp ids.
const PLACEHOLDER_ID_BASE: usize = 1_000_000;

/// Highlight key for draft text (the user-message accent), past the
/// class and lamp key ranges.
const DRAFT_TEXT_KEY: HighlightKey =
    HighlightKey::SyntaxTreeView(DASHBOARD_KEY_BASE + 2 * DashClass::ALL.len());

pub type ParsedHeadingState = DeskHeadingState;

#[derive(Clone, Copy)]
pub enum StructureDirection {
    Demote,
    Promote,
    Up,
    Down,
}

/// Identity of one generated line; each key owns one buffer in the
/// multibuffer. Reply drafts survive re-sorts by following their key,
/// not their line number. Document text is not keyed — it lives in the
/// shared Desk buffers directly.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum LineKey {
    Host(HostId),
    Fold(HostId, usize),
    Agent(AgentId),
    Unfiled(HostId),
    NewAgent,
    Reply(AgentId),
    NewDraft(Option<(HostId, usize)>),
}

impl LineKey {
    /// Drafts are user-owned writable buffers; sync never rewrites them.
    fn is_draft(&self) -> bool {
        matches!(self, LineKey::Reply(_) | LineKey::NewDraft(_))
    }
}

/// What the line under the cursor refers to; the object of every
/// dashboard command.
#[derive(Clone, Debug, PartialEq)]
pub enum RowTarget {
    /// Group headers, document prose, and other inert positions.
    None,
    Topic {
        host: HostId,
        offset: usize,
        first_attention: Option<AgentId>,
    },
    Agent(AgentId),
    NewAgent,
    /// An inline reply draft addressed to this agent.
    Reply(AgentId),
    /// The inline new-agent draft.
    NewDraft(Option<(HostId, usize)>),
}

/// Where the cursor is: on a generated row, or at an offset inside a
/// host's document.
#[derive(Clone, Debug, PartialEq)]
enum CursorPlace {
    Row(LineKey),
    Doc(HostId, usize),
}

/// One generated segment: a slice of a host document, or a generated
/// line (row or draft slot). Equality against the previous pass lets a
/// sync bail out before touching the editor at all.
///
/// A document slice's `id` is its stable identity across passes: a hash
/// of the title of the heading whose cut opens the slice (0 for the
/// slice that starts the document). The composition keys the excerpt on
/// it, so typing that shifts every offset still reconciles to a no-op.
#[derive(Debug, PartialEq)]
enum Segment {
    Doc {
        host: HostId,
        range: Range<usize>,
        id: u64,
    },
    Line(Line),
}

pub struct Dashboard {
    multi_buffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    /// One buffer per generated line key: read-only listing lines and
    /// writable reply drafts alike.
    buffers: HashMap<LineKey, Entity<Buffer>>,
    /// Non-owning references to the workspace-owned Desk source buffers.
    hosts: BTreeMap<HostId, WeakEntity<Buffer>>,
    /// Daemon-owned agent bindings per host, replaced wholesale.
    bindings: HashMap<HostId, Vec<DeskBinding>>,
    /// Reconciles the multibuffer to the generated spec by element
    /// identity, so unchanged excerpts — and cursors in them — survive.
    composition: Composition,
    /// Stable composition keys per line, allocated once and never reused.
    element_keys: HashMap<LineKey, u64>,
    next_element_key: u64,
    /// Generated rows in display order, from the last sync.
    order: Vec<LineKey>,
    /// What each generated key means, for cursor lookup.
    targets: HashMap<LineKey, RowTarget>,
    /// Bound root agents per heading start, from the last sync — the
    /// source for `first_attention` and agent→heading lookups.
    heading_agents: HashMap<(HostId, usize), Vec<AgentId>>,
    /// Open reply drafts in creation order (position comes from `order`).
    replies: Vec<AgentId>,
    /// Keeps the workspace re-rendering on draft edits, so placeholder
    /// and gutter chrome track the text.
    reply_subscriptions: HashMap<AgentId, gpui::Subscription>,
    /// The inline new-agent draft, when open: its buffer plus the edit
    /// subscription that keeps chrome fresh.
    new_draft: Option<(Option<(HostId, usize)>, Entity<Buffer>, gpui::Subscription)>,
    collapsed: HashSet<(HostId, usize)>,
    /// Hosts whose Unfiled tail is folded behind its header.
    collapsed_unfiled: HashSet<HostId>,
    /// Move the cursor into this key's buffer on the next sync — how a
    /// freshly opened reply draft receives the cursor.
    pending_cursor: Option<LineKey>,
    /// Move the cursor to this document offset on the next sync.
    pending_doc_cursor: Option<(HostId, usize)>,
    /// Reply placeholder inlays currently spliced in.
    placeholder_ids: Vec<InlayId>,
    /// The previous pass's inputs and output, so a sync whose world is
    /// unchanged returns without touching the editor.
    last_synced: Option<(Vec<(HostId, String)>, Vec<Segment>, Vec<(LineKey, String)>)>,
    /// Buffers already registered as headerless with the editor. A
    /// boundary onto a headerless buffer draws nothing, so this is what
    /// keeps the interleaved excerpts seamless.
    headers_disabled: std::collections::HashSet<BufferId>,
}

impl Dashboard {
    pub fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let multi_buffer = cx.new(|_| {
            let mut multi_buffer = MultiBuffer::without_headers(Capability::ReadWrite);
            // Document slices interleave with generated rows: one Desk
            // buffer appears under many path keys at once.
            multi_buffer.set_multiple_paths_per_buffer(true);
            multi_buffer
        });
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                multi_buffer.clone(),
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
            hosts: BTreeMap::new(),
            bindings: HashMap::new(),
            composition: Composition::default(),
            element_keys: HashMap::new(),
            next_element_key: 0,
            order: Vec::new(),
            targets: HashMap::new(),
            heading_agents: HashMap::new(),
            replies: Vec::new(),
            reply_subscriptions: HashMap::new(),
            new_draft: None,
            collapsed: HashSet::new(),
            collapsed_unfiled: HashSet::new(),
            pending_cursor: None,
            pending_doc_cursor: None,
            placeholder_ids: Vec::new(),
            last_synced: None,
            headers_disabled: std::collections::HashSet::new(),
        }
    }

    /// Registers every current buffer (rows and Desk documents) as
    /// headerless with the editor, so excerpt boundaries draw no divider.
    fn ensure_headerless(&mut self, cx: &mut Context<Workspace>) {
        let new_ids = self
            .buffers
            .values()
            .map(|buffer| buffer.read(cx).remote_id())
            .chain(
                self.hosts
                    .values()
                    .filter_map(|weak| Some(weak.upgrade()?.read(cx).remote_id())),
            )
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

    pub fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }

    pub fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle(cx).is_focused(window)
    }

    pub fn set_source(&mut self, host: HostId, source: WeakEntity<Buffer>) {
        self.hosts.insert(host, source);
    }

    pub fn set_bindings(&mut self, host: HostId, bindings: Vec<DeskBinding>) {
        self.bindings.insert(host, bindings);
    }

    fn source_text(&self, host: HostId, cx: &App) -> Option<String> {
        let source = self.hosts.get(&host)?.upgrade()?;
        let buffer = source.read(cx);
        Some(buffer.text_for_range(0..buffer.len()).collect())
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

    /// Opens (or returns to) the inline new-agent draft. Like a reply
    /// draft it parks when left and survives refreshes.
    pub fn open_new_draft(&mut self, topic: Option<(HostId, usize)>, cx: &mut Context<Workspace>) {
        if self.new_draft.is_none() {
            let buffer = cx.new(|cx| Buffer::local("", cx));
            let subscription = cx.subscribe(&buffer, |_, _, event, cx| {
                if matches!(event, language::BufferEvent::Edited { .. }) {
                    cx.notify();
                }
            });
            self.buffers
                .insert(LineKey::NewDraft(topic), buffer.clone());
            self.new_draft = Some((topic, buffer, subscription));
        }
        let topic = self
            .new_draft
            .as_ref()
            .map(|draft| draft.0)
            .unwrap_or(topic);
        self.pending_cursor = Some(LineKey::NewDraft(topic));
        cx.notify();
    }

    /// Takes the new-agent draft's text and closes it. `None` when empty.
    pub fn take_new_draft(&mut self, cx: &mut Context<Workspace>) -> Option<String> {
        let (topic, buffer, _) = self.new_draft.take()?;
        let text = buffer.read(cx).text().trim().to_owned();
        self.buffers.remove(&LineKey::NewDraft(topic));
        if let Some((host, offset)) = topic {
            self.pending_doc_cursor = Some((host, offset));
        }
        cx.notify();
        (!text.is_empty()).then_some(text)
    }

    pub fn new_draft_topic(&self) -> Option<(HostId, usize)> {
        self.new_draft.as_ref().and_then(|draft| draft.0)
    }

    pub fn cursor_to_agent(&mut self, agent_id: AgentId, cx: &mut Context<Workspace>) {
        self.pending_cursor = Some(LineKey::Agent(agent_id));
        cx.notify();
    }

    /// Takes a reply draft's text and closes it. `None` when the draft is
    /// empty (nothing worth sending).
    pub fn take_reply(&mut self, agent_id: AgentId, cx: &mut Context<Workspace>) -> Option<String> {
        let key = LineKey::Reply(agent_id);
        let buffer = self.buffers.get(&key)?;
        let text = buffer.read(cx).text().trim().to_owned();
        self.replies.retain(|reply| *reply != agent_id);
        self.buffers.remove(&key);
        self.reply_subscriptions.remove(&agent_id);
        cx.notify();
        (!text.is_empty()).then_some(text)
    }

    /// Resolves each host's bindings to the headings that contain them.
    /// Unresolvable anchors and unknown agents fall through to Unfiled.
    fn resolve_bindings(
        &self,
        registry: &AgentRegistry,
        documents: &[(HostId, String)],
        cx: &App,
    ) -> HashMap<(HostId, usize), Vec<AgentId>> {
        let mut filed_roots = HashSet::new();
        let mut by_heading: HashMap<(HostId, usize), Vec<AgentId>> = HashMap::new();
        for (host, text) in documents {
            let Some(buffer) = self.hosts.get(host).and_then(|weak| weak.upgrade()) else {
                continue;
            };
            let snapshot = buffer.read(cx).snapshot();
            let buffer_id = snapshot.remote_id();
            let headings = parse(text);
            for binding in self.bindings.get(host).map_or(&[][..], Vec::as_slice) {
                if registry.host_of_agent(binding.agent_id) != Some(*host) {
                    continue;
                }
                let root = root_agent(registry, binding.agent_id);
                let anchor = binding.anchor.to_text(buffer_id);
                if !snapshot.can_resolve(&anchor) {
                    continue;
                }
                let offset = snapshot.offset_for_anchor(&anchor);
                let Some(heading) = headings
                    .iter()
                    .rev()
                    .find(|heading| heading.heading_range.start <= offset)
                else {
                    continue;
                };
                if filed_roots.insert(root) {
                    by_heading
                        .entry((*host, heading.heading_range.start))
                        .or_default()
                        .push(root);
                }
            }
        }
        for agents in by_heading.values_mut() {
            *agents = sorted_agents(registry, agents.iter().copied());
        }
        by_heading
    }

    /// Regenerates the listing: the host documents are sliced at bound
    /// headings, generated rows and drafts are interleaved between the
    /// slices, and highlights and lamps reapplied. The cursor follows
    /// its buffer through the rearrangement.
    pub fn sync(
        &mut self,
        registry: &AgentRegistry,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let documents = self
            .hosts
            .keys()
            .filter_map(|host| self.source_text(*host, cx).map(|text| (*host, text)))
            .collect::<Vec<_>>();
        let filed = self.resolve_bindings(registry, &documents, cx);

        // Empty reply drafts the cursor has left are dead weight; drop them.
        let cursor_place = self.cursor_place(cx);
        let cursor_key = match &cursor_place {
            Some(CursorPlace::Row(key)) => Some(key.clone()),
            _ => None,
        };
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
            })
            .collect::<Vec<_>>();
        for agent_id in empty_replies {
            self.replies.retain(|reply| *reply != agent_id);
            self.buffers.remove(&LineKey::Reply(agent_id));
            self.reply_subscriptions.remove(&agent_id);
        }
        if self
            .new_draft
            .as_ref()
            .is_some_and(|(_, buffer, _)| buffer.read(cx).is_empty())
            && !matches!(cursor_key, Some(LineKey::NewDraft(_)))
            && !matches!(pending, Some(LineKey::NewDraft(_)))
        {
            if let Some((topic, _, _)) = self.new_draft.take() {
                self.buffers.remove(&LineKey::NewDraft(topic));
            }
        }

        let draft_topic = self.new_draft.as_ref().map(|(topic, _, _)| *topic);
        let segments = generate(
            registry,
            &documents,
            &filed,
            &self.collapsed,
            &self.collapsed_unfiled,
            &self.replies,
            draft_topic,
        );

        // Render reconciles every frame, so most passes are registry
        // noise that changes nothing on screen. When the whole pass —
        // documents, segments, filing, and draft texts (which live in
        // their buffers, outside the segments) — matches the last one,
        // return before touching a single buffer or highlight.
        let draft_texts = self
            .replies
            .iter()
            .map(|agent_id| LineKey::Reply(*agent_id))
            .chain(
                self.new_draft
                    .as_ref()
                    .map(|(topic, _, _)| LineKey::NewDraft(*topic)),
            )
            .map(|key| {
                let text = self
                    .buffers
                    .get(&key)
                    .map_or_else(String::new, |buffer| buffer.read(cx).text());
                (key, text)
            })
            .collect::<Vec<_>>();
        if self.pending_cursor.is_none()
            && self.pending_doc_cursor.is_none()
            && self.heading_agents == filed
            && self
                .last_synced
                .as_ref()
                .is_some_and(|(docs, segs, drafts)| {
                    *docs == documents && *segs == segments && *drafts == draft_texts
                })
        {
            return;
        }

        // Create/refresh the generated line buffers (drafts keep their
        // user-typed text).
        let mut edited = std::collections::HashSet::new();
        for segment in &segments {
            let Segment::Line(line) = segment else {
                continue;
            };
            if line.key.is_draft() {
                continue;
            }
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

        // Build the composition spec: one section per host document,
        // cut wherever generated rows splice in (or a folded body hides).
        // Cut and slice identities are the heading-title hashes generate
        // stamped on the doc segments; row identities are the per-key
        // element ids. The composition reconciles by identity, so a pass
        // where only offsets shifted (typing) touches nothing.
        let mut spec = CompositionSpec::default();
        let mut order = Vec::new();
        let mut pending_rows: Vec<RowSpec> = Vec::new();
        let mut current: Option<(HostId, usize)> = None;
        for segment in &segments {
            match segment {
                Segment::Doc { host, range, id } => {
                    match current {
                        Some((section_host, position)) if section_host == *host => {
                            if let Some(section) = spec.sections.last_mut() {
                                section.cuts.push(CutSpec {
                                    id: *id,
                                    position,
                                    resume: range.start,
                                    rows: std::mem::take(&mut pending_rows),
                                });
                            }
                        }
                        _ => {
                            let Some(buffer) =
                                self.hosts.get(host).and_then(|weak| weak.upgrade())
                            else {
                                current = None;
                                continue;
                            };
                            spec.sections.push(SectionSpec {
                                host: buffer,
                                lead: std::mem::take(&mut pending_rows),
                                cuts: Vec::new(),
                            });
                        }
                    }
                    current = Some((*host, range.end));
                }
                Segment::Line(line) => {
                    let Some(buffer) = self.buffers.get(&line.key).cloned() else {
                        continue;
                    };
                    order.push(line.key.clone());
                    pending_rows.push(RowSpec {
                        id: self.element_key(&line.key),
                        buffer,
                    });
                }
            }
        }
        spec.tail = pending_rows;

        // Capture where the cursor is before reconciling: a document
        // cursor as a buffer offset, a row cursor as its current path
        // (if the path survives, so did the excerpt and the anchor).
        let pending_doc = self.pending_doc_cursor.take();
        let doc_cursor_before = match &cursor_place {
            Some(CursorPlace::Doc(host, offset)) => Some((*host, *offset)),
            _ => None,
        };
        let cursor_row_path = cursor_key
            .as_ref()
            .and_then(|key| self.element_keys.get(key))
            .and_then(|id| self.composition.path_for_row(*id));

        let structure_changed = self.composition.sync(&self.multi_buffer, &spec, cx);

        // Prune buffers for lines that fell out of the listing (their
        // excerpts are gone); open drafts always stay.
        self.buffers
            .retain(|key, _| order.contains(key) || key.is_draft());

        self.targets = segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Line(line) => Some((line.key.clone(), line.target.clone())),
                Segment::Doc { .. } => None,
            })
            .collect();
        self.heading_agents = filed;

        // The cursor follows its buffer: anchors survive any reconcile
        // that kept their excerpt, so a row cursor is re-placed only when
        // its excerpt was rebuilt (path changed) or its text rewritten,
        // and a document cursor is restored by offset only when the
        // structure moved at all.
        let rebuilt = |dashboard: &Self, key: &LineKey| {
            dashboard
                .element_keys
                .get(key)
                .and_then(|id| dashboard.composition.path_for_row(*id))
                != cursor_row_path
        };
        let restore = match &self.pending_cursor {
            Some(key) if order.contains(key) => Some(key.clone()),
            _ => match &cursor_key {
                Some(key)
                    if order.contains(key)
                        && (edited.contains(key) || rebuilt(self, key)) =>
                {
                    Some(key.clone())
                }
                _ => None,
            },
        };
        self.pending_cursor = None;
        self.order = order;
        if let Some(key) = restore {
            self.move_cursor_to(&key, window, cx);
        } else if let Some((host, offset)) =
            pending_doc.or(if structure_changed { doc_cursor_before } else { None })
        {
            self.move_cursor_to_doc(host, offset, window, cx);
        }

        self.apply_highlights(&segments, &documents, cx);
        self.apply_reply_chrome(registry, cx);
        self.last_synced = Some((documents, segments, draft_texts));
    }

    /// The stable composition id for a line key, allocated on first use.
    fn element_key(&mut self, key: &LineKey) -> u64 {
        if let Some(id) = self.element_keys.get(key) {
            return *id;
        }
        self.next_element_key += 1;
        self.element_keys.insert(key.clone(), self.next_element_key);
        self.next_element_key
    }

    /// Places the cursor at the start of a key's buffer.
    fn move_cursor_to(&self, key: &LineKey, window: &mut Window, cx: &mut Context<Workspace>) {
        let Some(buffer) = self.buffers.get(key) else {
            return;
        };
        // Right-biased, like the transcript's prompt anchor: the cursor
        // stays ahead of same-position inlays (the draft placeholder).
        let anchor = buffer.read(cx).anchor_after(0);
        self.select_buffer_anchor(anchor, window, cx);
    }

    /// Places the cursor at an offset in a host document, clipped into
    /// whichever slice contains it.
    fn move_cursor_to_doc(
        &self,
        host: HostId,
        offset: usize,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(buffer) = self.hosts.get(&host).and_then(|weak| weak.upgrade()) else {
            return;
        };
        let buffer = buffer.read(cx);
        let anchor = buffer.anchor_after(offset.min(buffer.len()));
        self.select_buffer_anchor(anchor, window, cx);
    }

    fn select_buffer_anchor(
        &self,
        anchor: text::Anchor,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
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

    /// Where the cursor is: a generated row, or an offset in a document.
    fn cursor_place(&self, cx: &mut Context<Workspace>) -> Option<CursorPlace> {
        let (buffer_id, offset) = self.editor.update(cx, |editor, cx| {
            let head = editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx))
                .head();
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            snapshot
                .point_to_buffer_offset(head)
                .map(|(buffer, offset)| (buffer.remote_id(), offset.0))
        })?;
        for (host, weak) in &self.hosts {
            if weak
                .upgrade()
                .is_some_and(|buffer| buffer.read(cx).remote_id() == buffer_id)
            {
                return Some(CursorPlace::Doc(*host, offset));
            }
        }
        self.buffers
            .iter()
            .find(|(_, buffer)| buffer.read(cx).remote_id() == buffer_id)
            .map(|(key, _)| CursorPlace::Row(key.clone()))
    }

    /// The heading whose *line* the cursor is on, if any.
    fn cursor_heading_line(&self, cx: &mut Context<Workspace>) -> Option<(HostId, usize)> {
        let Some(CursorPlace::Doc(host, offset)) = self.cursor_place(cx) else {
            return None;
        };
        let text = self.source_text(host, cx)?;
        parse(&text)
            .into_iter()
            .find(|heading| {
                heading.heading_range.start <= offset && offset <= heading.heading_range.end
            })
            .map(|heading| (host, heading.heading_range.start))
    }

    /// The row under the cursor.
    pub fn cursor_target(
        &self,
        registry: &AgentRegistry,
        cx: &mut Context<Workspace>,
    ) -> Option<RowTarget> {
        match self.cursor_place(cx)? {
            CursorPlace::Row(key) => self.targets.get(&key).cloned(),
            CursorPlace::Doc(host, offset) => {
                if let Some((host, start)) = self.cursor_heading_line_at(host, offset, cx) {
                    let first_attention = self
                        .heading_agents
                        .get(&(host, start))
                        .into_iter()
                        .flatten()
                        .copied()
                        .find(|agent_id| registry.attention(*agent_id) >= UiAttention::Pending);
                    Some(RowTarget::Topic {
                        host,
                        offset: start,
                        first_attention,
                    })
                } else {
                    Some(RowTarget::None)
                }
            }
        }
    }

    fn cursor_heading_line_at(
        &self,
        host: HostId,
        offset: usize,
        cx: &App,
    ) -> Option<(HostId, usize)> {
        let text = self.source_text(host, cx)?;
        parse(&text)
            .into_iter()
            .find(|heading| {
                heading.heading_range.start <= offset && offset <= heading.heading_range.end
            })
            .map(|heading| (host, heading.heading_range.start))
    }

    /// The heading that owns the cursor position: the containing heading
    /// for document positions, the bound heading for agent rows.
    pub fn cursor_topic(&self, cx: &mut Context<Workspace>) -> Option<(HostId, usize)> {
        match self.cursor_place(cx)? {
            CursorPlace::Doc(host, offset) => {
                let text = self.source_text(host, cx)?;
                parse(&text)
                    .into_iter()
                    .rev()
                    .find(|heading| heading.heading_range.start <= offset)
                    .map(|heading| (host, heading.heading_range.start))
            }
            CursorPlace::Row(key) => match key {
                LineKey::Fold(host, offset) => Some((host, offset)),
                LineKey::NewDraft(topic) => topic,
                LineKey::Agent(agent_id) | LineKey::Reply(agent_id) => self
                    .heading_agents
                    .iter()
                    .find(|(_, agents)| agents.contains(&agent_id))
                    .map(|(topic, _)| *topic),
                _ => None,
            },
        }
    }

    /// The heading's top agent (rows are sorted loudest-first), for verbs
    /// aimed at a heading line rather than a specific row.
    pub fn first_agent_for_topic(&self, topic: (HostId, usize)) -> Option<AgentId> {
        self.heading_agents
            .get(&topic)
            .and_then(|agents| agents.first().copied())
    }

    pub fn append_topic(&mut self, title: &str, cx: &mut Context<Workspace>) -> bool {
        let host = self
            .cursor_topic(cx)
            .map(|topic| topic.0)
            .or_else(|| self.hosts.keys().next().copied());
        let Some(host) = host else { return false };
        let Some(text) = self.source_text(host, cx) else {
            return false;
        };
        let insertion = if text.is_empty() || text.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        let topic = format!("{insertion}* {}\n", title.trim());
        let len = self.hosts[&host].upgrade().unwrap().read(cx).len();
        self.hosts[&host]
            .upgrade()
            .unwrap()
            .update(cx, |buffer, cx| buffer.edit([(len..len, topic)], None, cx));
        true
    }

    /// Whether the cursor is somewhere dashboard verbs apply: a heading
    /// line of the document or a generated agent row.
    pub fn cursor_on_heading_line(&self, cx: &mut Context<Workspace>) -> bool {
        if matches!(self.cursor_place(cx), Some(CursorPlace::Row(LineKey::Agent(_)))) {
            return true;
        }
        self.cursor_heading_line(cx).is_some()
    }

    pub fn staffing_target(
        &self,
        cx: &mut Context<Workspace>,
    ) -> Result<(HostId, usize, String, Option<String>), &'static str> {
        let (host, offset) = self.cursor_topic(cx).ok_or("staff: choose a topic")?;
        self.staffing_target_for((host, offset), cx)
    }

    pub fn staffing_target_for(
        &self,
        (host, offset): (HostId, usize),
        cx: &mut Context<Workspace>,
    ) -> Result<(HostId, usize, String, Option<String>), &'static str> {
        let text = self
            .source_text(host, cx)
            .ok_or("staff: Desk is unavailable")?;
        let heading = parse(&text)
            .into_iter()
            .find(|heading| heading.heading_range.start == offset)
            .ok_or("staff: topic moved")?;
        let prose = prose_for(&text, &heading);
        let brief = if prose.trim().is_empty() {
            heading.title.clone()
        } else {
            format!("{}\n\n{}", heading.title, prose)
        };
        Ok((host, offset, brief, heading.resolved_project))
    }

    pub fn set_heading_project(
        &mut self,
        host: HostId,
        offset: usize,
        project: &str,
        cx: &mut Context<Workspace>,
    ) {
        let Some(text) = self.source_text(host, cx) else {
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
        self.hosts[&host]
            .upgrade()
            .unwrap()
            .update(cx, |buffer, cx| {
                buffer.edit(
                    [(insertion..insertion, format!(":project: {project}\n"))],
                    None,
                    cx,
                )
            });
    }

    pub fn set_cursor_heading_state(
        &mut self,
        state: ParsedHeadingState,
        cx: &mut Context<Workspace>,
    ) {
        let Some((host, offset)) = self.cursor_heading_line(cx) else {
            return;
        };
        let Some(text) = self.source_text(host, cx) else {
            return;
        };
        let Some(heading) = parse(&text)
            .into_iter()
            .find(|heading| heading.heading_range.start == offset)
        else {
            return;
        };
        let replacement = format!(
            "{} {}{}",
            "*".repeat(heading.depth),
            state.keyword(),
            (!heading.title.is_empty())
                .then(|| format!(" {}", heading.title))
                .unwrap_or_default()
        );
        self.hosts[&host]
            .upgrade()
            .unwrap()
            .update(cx, |buffer, cx| {
                buffer.edit([(heading.heading_range, replacement)], None, cx)
            });
    }

    pub fn insert_sibling(
        &mut self,
        _above: bool,
        _window: &mut Window,
        _cx: &mut Context<Workspace>,
    ) -> bool {
        false
    }

    pub fn structure_move(&mut self, direction: StructureDirection, cx: &mut Context<Workspace>) {
        let (StructureDirection::Up | StructureDirection::Down) = direction else {
            return;
        };
        let Some((host, offset)) = self.cursor_heading_line(cx) else {
            return;
        };
        let Some(text) = self.source_text(host, cx) else {
            return;
        };
        let headings = parse(&text);
        let Some(index) = headings
            .iter()
            .position(|heading| heading.heading_range.start == offset)
        else {
            return;
        };
        let target = match direction {
            StructureDirection::Up => index.checked_sub(1),
            StructureDirection::Down => (index + 1 < headings.len()).then_some(index + 1),
            _ => None,
        };
        let Some(target) = target else { return };
        let block =
            |index: usize| headings[index].heading_range.start..headings[index].body_range.end;
        let a = block(index);
        let b = block(target);
        let range = a.start.min(b.start)..a.end.max(b.end);
        let replacement = if index < target {
            format!("{}{}", &text[b.clone()], &text[a.clone()])
        } else {
            format!("{}{}", &text[a.clone()], &text[b.clone()])
        };
        self.hosts[&host]
            .upgrade()
            .unwrap()
            .update(cx, |buffer, cx| {
                buffer.edit([(range, replacement)], None, cx)
            });
    }

    pub fn rename_cursor_topic(&mut self, title: &str, cx: &mut Context<Workspace>) -> bool {
        let Some((host, offset)) = self.cursor_heading_line(cx) else {
            return false;
        };
        let Some(text) = self.source_text(host, cx) else {
            return false;
        };
        let Some(heading) = parse(&text)
            .into_iter()
            .find(|heading| heading.heading_range.start == offset)
        else {
            return false;
        };
        self.hosts[&host]
            .upgrade()
            .unwrap()
            .update(cx, |buffer, cx| {
                buffer.edit([(heading.title_range, title.trim())], None, cx)
            });
        true
    }

    pub fn topic_titles_for_cursor_agent(
        &self,
        registry: &AgentRegistry,
        cx: &mut Context<Workspace>,
    ) -> Vec<String> {
        let Some(RowTarget::Agent(agent_id)) = self.cursor_target(registry, cx) else {
            return Vec::new();
        };
        let Some(host) = registry.host_of_agent(agent_id) else {
            return Vec::new();
        };
        let Some(text) = self.source_text(host, cx) else {
            return Vec::new();
        };
        parse(&text)
            .into_iter()
            .map(|heading| heading.title)
            .chain(["Unfiled".to_owned()])
            .collect()
    }

    /// Resolves the move-agent verb: which root agent to rebind, and the
    /// heading offset it should bind to (`None` unfiles it). The rebind
    /// itself is a daemon operation — the document text never changes.
    pub fn rebind_target_for_cursor_agent(
        &self,
        registry: &AgentRegistry,
        topic: &str,
        cx: &mut Context<Workspace>,
    ) -> Option<(HostId, AgentId, Option<usize>)> {
        let RowTarget::Agent(agent_id) = self.cursor_target(registry, cx)? else {
            return None;
        };
        let root = root_agent(registry, agent_id);
        let host = registry.host_of_agent(root)?;
        if topic == "Unfiled" {
            return Some((host, root, None));
        }
        let text = self.source_text(host, cx)?;
        let heading = parse(&text)
            .into_iter()
            .find(|heading| heading.title == topic)?;
        Some((host, root, Some(heading.heading_range.start)))
    }

    pub fn delete_empty(&mut self, _cx: &mut Context<Workspace>) -> bool {
        false
    }

    pub fn toggle_subagents(&mut self, cx: &mut Context<Workspace>) -> bool {
        if let Some(CursorPlace::Row(LineKey::Unfiled(host))) = self.cursor_place(cx) {
            if !self.collapsed_unfiled.remove(&host) {
                self.collapsed_unfiled.insert(host);
            }
            cx.notify();
            return true;
        }
        let Some(topic) = self.cursor_topic(cx) else {
            return false;
        };
        if !self.collapsed.remove(&topic) {
            self.collapsed.insert(topic);
        }
        cx.notify();
        true
    }

    /// Whether the cursor sits on a host's Unfiled header row.
    pub fn cursor_on_unfiled_header(&self, cx: &mut Context<Workspace>) -> bool {
        matches!(
            self.cursor_place(cx),
            Some(CursorPlace::Row(LineKey::Unfiled(_)))
        )
    }

    pub fn heading_candidates(
        &self,
        _registry: &AgentRegistry,
        needle: &str,
        cx: &App,
    ) -> Vec<(String, String)> {
        let needle = needle.to_lowercase();
        let mut entries = Vec::new();
        for host in self.hosts.keys() {
            let Some(text) = self.source_text(*host, cx) else {
                continue;
            };
            for heading in parse(&text) {
                if heading.title.to_lowercase().contains(&needle) {
                    entries.push((heading.title, format!("{} · Desk", host)));
                }
            }
        }
        entries
    }

    pub fn jump_to_heading(
        &mut self,
        title: &str,
        _registry: &AgentRegistry,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> bool {
        for host in self.hosts.keys().copied().collect::<Vec<_>>() {
            let Some(text) = self.source_text(host, cx) else {
                continue;
            };
            if let Some(heading) = parse(&text)
                .into_iter()
                .find(|heading| heading.title == title)
            {
                self.move_cursor_to_doc(host, heading.heading_range.start, window, cx);
                return true;
            }
        }
        false
    }

    pub fn next_now(
        &mut self,
        registry: &AgentRegistry,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Option<AgentId> {
        let current = self.cursor_place(cx).and_then(|place| match place {
            CursorPlace::Row(key) => Some(key),
            CursorPlace::Doc(..) => None,
        });
        let agents = self
            .order
            .iter()
            .filter_map(|key| match key {
                LineKey::Agent(agent_id)
                    if registry.attention(*agent_id) >= UiAttention::Pending =>
                {
                    Some(*agent_id)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let next = current
            .and_then(|key| agents.iter().position(|id| key == LineKey::Agent(*id)))
            .map_or(0, |index| (index + 1) % agents.len().max(1));
        let agent_id = *agents.get(next)?;
        self.move_cursor_to(&LineKey::Agent(agent_id), window, cx);
        Some(agent_id)
    }

    pub fn back(
        &mut self,
        _registry: &AgentRegistry,
        _window: &mut Window,
        _cx: &mut Context<Workspace>,
    ) -> bool {
        false
    }

    pub fn hint(&self, _cx: &mut Context<Workspace>) -> &'static str {
        "enter open · r reply · o staff · m move · d/x verdict · Tab fold · gn attention"
    }

    fn apply_highlights(
        &self,
        segments: &[Segment],
        documents: &[(HostId, String)],
        cx: &mut Context<Workspace>,
    ) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let mut by_class: Vec<(DashClass, Vec<Range<multi_buffer::Anchor>>)> = DashClass::ALL
            .into_iter()
            .map(|class| (class, Vec::new()))
            .collect();
        let mut push = |class: &DashClass, range: Range<multi_buffer::Anchor>| {
            if let Some((_, ranges)) = by_class.iter_mut().find(|(entry, _)| entry == class) {
                ranges.push(range);
            }
        };
        for segment in segments {
            let Segment::Line(line) = segment else {
                continue;
            };
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
                push(class, start..end);
            }
        }
        // Document chrome: heading lines and property lines, resolved
        // through whichever slice shows them (hidden ranges drop out).
        for (host, text) in documents {
            let Some(buffer) = self.hosts.get(host).and_then(|weak| weak.upgrade()) else {
                continue;
            };
            let buffer_snapshot = buffer.read(cx).snapshot();
            for (class, range) in doc_spans(text) {
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
                push(&class, start..end);
            }
        }
        self.editor.update(cx, |editor, cx| {
            for (class, ranges) in by_class {
                editor.highlight_text(class.key(), ranges, class.style(cx), cx);
            }
        });
    }

    /// Reply-draft chrome: an accent gutter stripe plus a placeholder
    /// inlay naming the addressee while the draft is empty.
    fn apply_reply_chrome(&mut self, registry: &AgentRegistry, cx: &mut Context<Workspace>) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let to_remove = std::mem::take(&mut self.placeholder_ids);
        let mut inlays = Vec::new();
        let mut gutter_ranges = Vec::new();
        let mut draft_text_ranges = Vec::new();
        let drafts = self
            .replies
            .iter()
            .map(|agent_id| {
                (
                    LineKey::Reply(*agent_id),
                    format!("reply to {}…", registry.agent_id_label(*agent_id)),
                )
            })
            .chain(
                self.new_draft
                    .as_ref()
                    .map(|(topic, _, _)| (LineKey::NewDraft(*topic), "new agent…".to_owned())),
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
            gutter_ranges.push(start..end);
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
            }
        }
        let draft_style = crate::style::StyleClass::UserMessage.resolve(cx);
        self.editor.update(cx, |editor, cx| {
            editor.splice_inlays(&to_remove, inlays, cx);
            editor.highlight_gutter::<ReplyGutter>(
                gutter_ranges,
                crate::style::user_prompt_gutter_color,
                cx,
            );
            editor.highlight_text(DRAFT_TEXT_KEY, draft_text_ranges, draft_style, cx);
        });
    }
}

/// Gutter highlight marker type for reply drafts.
pub struct ReplyGutter;

/// Dashboard text classes: lamps and muted chrome. The cursor itself is
/// the selection indicator — rows carry no selected styling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DashClass {
    Muted,
    Heading,
    TodoHeading,
    StaffedHeading,
    Working,
    Pending,
    NeedsInput,
}

impl DashClass {
    const ALL: [DashClass; 7] = [
        DashClass::Muted,
        DashClass::Heading,
        DashClass::TodoHeading,
        DashClass::StaffedHeading,
        DashClass::Working,
        DashClass::Pending,
        DashClass::NeedsInput,
    ];

    fn key(self) -> HighlightKey {
        let slot = match self {
            DashClass::Muted => 0,
            DashClass::Heading => 1,
            DashClass::TodoHeading => 2,
            DashClass::StaffedHeading => 3,
            DashClass::Working => 4,
            DashClass::Pending => 5,
            DashClass::NeedsInput => 6,
        };
        HighlightKey::SyntaxTreeView(DASHBOARD_KEY_BASE + slot)
    }

    /// Color does all the talking: nothing on the dashboard is bold.
    /// Headings deliberately avoid `text_accent`, which is the typed
    /// user-message color everywhere else in rho.
    fn style(self, cx: &App) -> HighlightStyle {
        let colors = cx.theme().colors();
        let color = match self {
            DashClass::Muted => colors.text_muted,
            DashClass::Heading => colors.terminal_ansi_blue,
            DashClass::TodoHeading => colors.terminal_ansi_red,
            DashClass::StaffedHeading => colors.terminal_ansi_cyan,
            DashClass::Working => colors.terminal_ansi_cyan,
            DashClass::Pending => colors.terminal_ansi_yellow,
            DashClass::NeedsInput => colors.terminal_ansi_red,
        };
        HighlightStyle {
            color: Some(color.into()),
            ..HighlightStyle::default()
        }
    }

    fn lamp(attention: UiAttention) -> Option<DashClass> {
        match attention {
            UiAttention::Quiet => None,
            UiAttention::Working => Some(DashClass::Working),
            UiAttention::Pending => Some(DashClass::Pending),
            UiAttention::NeedsInput => Some(DashClass::NeedsInput),
        }
    }
}

/// One generated dashboard line: identity, text, semantic spans, and
/// the object addressed by dashboard verbs.
#[derive(Debug, PartialEq)]
struct Line {
    key: LineKey,
    text: String,
    spans: Vec<(DashClass, Range<usize>)>,
    target: RowTarget,
}

impl Line {
    fn new(key: LineKey, target: RowTarget) -> Self {
        Self {
            key,
            text: String::new(),
            spans: Vec::new(),
            target,
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

/// Highlight spans for a host document: heading lines styled by state,
/// stars and property lines muted.
fn doc_spans(text: &str) -> Vec<(DashClass, Range<usize>)> {
    let mut spans = Vec::new();
    for heading in parse(text) {
        spans.push((DashClass::Muted, heading.stars_range.clone()));
        // The title keeps the heading color in every state — a DONE
        // heading is still a heading, not a comment; only the keyword
        // fades.
        let state_class = match heading.state {
            Some(DeskHeadingState::Todo) => DashClass::TodoHeading,
            Some(DeskHeadingState::Staffed) => DashClass::StaffedHeading,
            Some(DeskHeadingState::Done | DeskHeadingState::Discarded) => DashClass::Muted,
            None => DashClass::Heading,
        };
        let title_class = DashClass::Heading;
        if let Some(state_range) = &heading.state_range {
            spans.push((state_class, state_range.clone()));
        }
        spans.push((title_class, heading.title_range.clone()));
        for property in &heading.properties {
            spans.push((DashClass::Muted, property.line_range.clone()));
        }
    }
    spans
}

fn prose_for(text: &str, heading: &DeskHeading) -> String {
    let property_ranges = heading
        .properties
        .iter()
        .map(|property| property.line_range.clone())
        .collect::<Vec<_>>();
    text[heading.body_range.clone()]
        .split_inclusive('\n')
        .scan(heading.body_range.start, |offset, line| {
            let start = *offset;
            *offset += line.len();
            Some((start, line))
        })
        .filter(|(start, _)| {
            !property_ranges
                .iter()
                .any(|property| property.start == *start)
        })
        .map(|(_, line)| line)
        .collect::<String>()
        .trim_end_matches('\n')
        .to_owned()
}

fn root_agent(registry: &AgentRegistry, mut agent_id: AgentId) -> AgentId {
    let mut seen = HashSet::new();
    while seen.insert(agent_id) {
        let Some(parent) = registry.agent_parent(agent_id) else {
            break;
        };
        agent_id = parent;
    }
    agent_id
}

fn sorted_agents(
    registry: &AgentRegistry,
    agents: impl IntoIterator<Item = AgentId>,
) -> Vec<AgentId> {
    let mut agents = agents.into_iter().collect::<Vec<_>>();
    agents.sort_by_key(|agent_id| {
        (
            Reverse(registry.attention(*agent_id)),
            Reverse(registry.agent_last_active(*agent_id).unwrap_or_default()),
            *agent_id,
        )
    });
    agents.dedup();
    agents
}

fn agent_line(agent_id: AgentId, registry: &AgentRegistry) -> Line {
    let attention = registry.attention(agent_id);
    let mut line = Line::new(LineKey::Agent(agent_id), RowTarget::Agent(agent_id));

    // A fixed one-glyph lamp column: attention is the color, the cursor
    // still lands on real text, and nothing floats at line ends.
    let glyph = if registry.agent_pinned(agent_id) {
        "◆"
    } else if attention > UiAttention::Quiet {
        "●"
    } else {
        "·"
    };
    // The glyph is the row's only splash of color; the rest stays plain
    // so the document's headings carry the page.
    line.span(
        Some(DashClass::lamp(attention).unwrap_or(DashClass::Muted)),
        |text| text.push_str(glyph),
    );
    line.span(None, |text| text.push(' '));
    line.span(None, |text| {
        text.push_str(&registry.agent_human_name(agent_id))
    });

    let members = registry.agent_subtree(agent_id);
    let members = members.into_iter().skip(1).collect::<Vec<_>>();
    let overflow = members.len().saturating_sub(VISIBLE_TAGS);
    for member in members.into_iter().take(VISIBLE_TAGS) {
        line.span(None, |text| text.push_str("  "));
        line.span(Some(DashClass::Muted), |text| {
            text.push_str(&registry.agent_id_label(member))
        });
    }
    if overflow > 0 {
        line.span(None, |text| text.push_str("  "));
        line.span(Some(DashClass::Muted), |text| {
            text.push_str(&format!("+{overflow}"))
        });
    }
    if attention >= UiAttention::Pending
        && let Some(reason) = registry.agent_attention_reason(agent_id)
    {
        let reason = reason.lines().next().unwrap_or_default().trim();
        if !reason.is_empty() {
            let snippet = truncate_chars(reason, 48);
            line.span(None, |text| text.push_str("  "));
            line.span(Some(DashClass::Muted), |text| {
                text.push_str(&format!("— {snippet}"))
            });
        }
    }
    line
}

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let mut result = value
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    result.push('…');
    result
}

/// Ends a document slice before a cut point's newline, so the synthetic
/// newline between excerpts doesn't double it.
fn trim_newline(text: &str, end: usize) -> usize {
    if end > 0 && text.as_bytes().get(end - 1) == Some(&b'\n') {
        end - 1
    } else {
        end
    }
}

/// Generate the listing without mutating Desk text: the documents are
/// emitted as writable slices, cut where a bound heading's rows (agent
/// rows, replies, the staffing draft) splice in after its body. Root
/// agents bound to no heading form the generated Unfiled tail.
fn generate(
    registry: &AgentRegistry,
    documents: &[(HostId, String)],
    filed: &HashMap<(HostId, usize), Vec<AgentId>>,
    collapsed: &HashSet<(HostId, usize)>,
    collapsed_unfiled: &HashSet<HostId>,
    replies: &[AgentId],
    draft_topic: Option<Option<(HostId, usize)>>,
) -> Vec<Segment> {
    let mut segments = Vec::new();
    let multiple_hosts = documents.len() > 1;
    let mut emitted_replies = HashSet::new();
    let empty = Vec::new();

    let push_agent_rows =
        |segments: &mut Vec<Segment>, emitted_replies: &mut HashSet<AgentId>, agents: &[AgentId]| {
            for agent_id in agents {
                segments.push(Segment::Line(agent_line(*agent_id, registry)));
                if replies.contains(agent_id) && emitted_replies.insert(*agent_id) {
                    segments.push(Segment::Line(Line::new(
                        LineKey::Reply(*agent_id),
                        RowTarget::Reply(*agent_id),
                    )));
                }
            }
        };

    for (host, text) in documents {
        if multiple_hosts {
            let mut header = Line::new(LineKey::Host(*host), RowTarget::None);
            header.span(Some(DashClass::Muted), |line| {
                line.push_str(registry.host_name(*host))
            });
            segments.push(Segment::Line(header));
        }
        let headings = parse(text);
        let mut slice_start = 0usize;
        let mut slice_id = 0u64;
        let mut title_counts: HashMap<String, u32> = HashMap::new();
        // The next slice's identity comes from the heading whose cut
        // opens it: a hash of the title (plus an occurrence index for
        // duplicates), so it survives every offset shift above it.
        let next_slice_id = |title: &str, title_counts: &mut HashMap<String, u32>| {
            use std::hash::{Hash as _, Hasher as _};
            let count = title_counts.entry(title.to_owned()).or_insert(0);
            let occurrence = *count;
            *count += 1;
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            title.hash(&mut hasher);
            occurrence.hash(&mut hasher);
            hasher.finish()
        };
        for heading in &headings {
            let start = heading.heading_range.start;
            let agents = filed.get(&(*host, start)).unwrap_or(&empty);
            let draft_here = draft_topic == Some(Some((*host, start)));
            if collapsed.contains(&(*host, start)) {
                // Collapsed: the heading line stays, its body and rows
                // fold behind an "n more" indicator.
                segments.push(Segment::Doc {
                    host: *host,
                    range: slice_start..heading.heading_range.end,
                    id: slice_id,
                });
                slice_id = next_slice_id(&heading.title, &mut title_counts);
                let prose = prose_for(text, heading);
                let folded_count = agents.len() + usize::from(!prose.is_empty());
                if folded_count > 0 {
                    let loudest = agents
                        .iter()
                        .map(|agent_id| registry.attention(*agent_id))
                        .max()
                        .unwrap_or(UiAttention::Quiet);
                    let mut fold = Line::new(
                        LineKey::Fold(*host, start),
                        RowTarget::Topic {
                            host: *host,
                            offset: start,
                            first_attention: agents.iter().copied().find(|agent_id| {
                                registry.attention(*agent_id) >= UiAttention::Pending
                            }),
                        },
                    );
                    fold.span(
                        Some(DashClass::lamp(loudest).unwrap_or(DashClass::Muted)),
                        |line| line.push_str(if loudest > UiAttention::Quiet { "●" } else { "…" }),
                    );
                    fold.span(Some(DashClass::Muted), |line| {
                        line.push_str(&format!(" {folded_count} more"))
                    });
                    segments.push(Segment::Line(fold));
                }
                if draft_here {
                    segments.push(Segment::Line(Line::new(
                        LineKey::NewDraft(Some((*host, start))),
                        RowTarget::NewDraft(Some((*host, start))),
                    )));
                }
                slice_start = heading.body_range.end;
            } else if !agents.is_empty() || draft_here {
                // Rows splice in after the heading's body, before the
                // next heading (trailing newline trimmed so the excerpt
                // boundary doesn't double it).
                segments.push(Segment::Doc {
                    host: *host,
                    range: slice_start..trim_newline(text, heading.body_range.end),
                    id: slice_id,
                });
                slice_id = next_slice_id(&heading.title, &mut title_counts);
                push_agent_rows(&mut segments, &mut emitted_replies, agents);
                if draft_here {
                    segments.push(Segment::Line(Line::new(
                        LineKey::NewDraft(Some((*host, start))),
                        RowTarget::NewDraft(Some((*host, start))),
                    )));
                }
                slice_start = heading.body_range.end;
            }
        }
        if slice_start < text.len() || slice_start == 0 {
            segments.push(Segment::Doc {
                host: *host,
                range: slice_start..text.len(),
                id: slice_id,
            });
        }
    }

    let filed_roots = filed
        .values()
        .flatten()
        .copied()
        .collect::<HashSet<AgentId>>();
    for (host, _) in documents {
        let unfiled = sorted_agents(
            registry,
            registry
                .known_agents()
                .copied()
                .filter(|agent_id| registry.host_of_agent(*agent_id) == Some(*host))
                .filter(|agent_id| registry.agent_parent(*agent_id).is_none())
                .filter(|agent_id| !registry.agent_hidden(*agent_id))
                .filter(|agent_id| !filed_roots.contains(agent_id)),
        );
        if unfiled.is_empty() {
            continue;
        }
        let folded = collapsed_unfiled.contains(host);
        let mut header = Line::new(LineKey::Unfiled(*host), RowTarget::None);
        header.span(Some(DashClass::Heading), |line| {
            line.push_str("Unfiled");
            if multiple_hosts {
                line.push_str(" · ");
                line.push_str(registry.host_name(*host));
            }
        });
        header.span(Some(DashClass::Muted), |line| {
            line.push_str(&format!(" · {}", unfiled.len()));
        });
        if folded {
            // The fold indicator turns into a lamp when something
            // folded away wants attention.
            let loudest = unfiled
                .iter()
                .map(|agent_id| registry.attention(*agent_id))
                .max()
                .unwrap_or(UiAttention::Quiet);
            header.span(
                Some(DashClass::lamp(loudest).unwrap_or(DashClass::Muted)),
                |line| line.push_str(if loudest > UiAttention::Quiet { " ●" } else { " …" }),
            );
        }
        segments.push(Segment::Line(header));
        if !folded {
            push_agent_rows(&mut segments, &mut emitted_replies, &unfiled);
        }
    }

    // Replies whose rows are folded away, and the unanchored new-agent
    // draft, park above the new-agent line so they are never lost.
    for agent_id in replies {
        if emitted_replies.insert(*agent_id) {
            segments.push(Segment::Line(Line::new(
                LineKey::Reply(*agent_id),
                RowTarget::Reply(*agent_id),
            )));
        }
    }
    if draft_topic == Some(None) {
        segments.push(Segment::Line(Line::new(
            LineKey::NewDraft(None),
            RowTarget::NewDraft(None),
        )));
    }

    let mut new_agent = Line::new(LineKey::NewAgent, RowTarget::NewAgent);
    new_agent.span(Some(DashClass::Muted), |line| line.push_str("+ new agent"));
    segments.push(Segment::Line(new_agent));
    segments
}

#[cfg(test)]
mod tests {
    use rho_core::UnixMs;
    use rho_ui_proto::{AgentDisposition, AgentIdDomain, AgentRole, UiAgentSummary, WorkspaceInfo};

    use super::*;

    fn agent(
        id: u64,
        parent_agent: Option<AgentId>,
        attention: UiAttention,
        active: u64,
    ) -> UiAgentSummary {
        UiAgentSummary {
            agent_id: AgentId::from_counter(id, &AgentIdDomain(0)).unwrap(),
            parent_agent,
            display_name: Some(format!("agent {id}")),
            created_at: UnixMs(id),
            updated_at: UnixMs(active),
            role: AgentRole::default(),
            workspace: WorkspaceInfo::UserCheckout {
                repo: "/tmp".into(),
            },
            attention,
            last_active: UnixMs(active),
            hidden: false,
            disposition: AgentDisposition::Pending,
            last_user_message_text: String::new(),
            activity: None,
            turn_report: None,
            labels: Vec::new(),
        }
    }

    fn registry(agents: Vec<UiAgentSummary>) -> (AgentRegistry, HostId) {
        let host = HostId(1);
        let mut registry = AgentRegistry::default();
        registry.attach_host(host, "local".to_owned());
        registry.set_host_data(host, 0, 100, agents);
        (registry, host)
    }

    fn keys(segments: &[Segment]) -> Vec<String> {
        segments
            .iter()
            .map(|segment| match segment {
                Segment::Doc { range, .. } => format!("doc {}..{}", range.start, range.end),
                Segment::Line(line) => format!("{:?}", line.key),
            })
            .collect()
    }

    #[test]
    fn prose_excludes_properties() {
        let text = "* Topic\n:agent: eng-abcd\njudgment\n:project: rho\nmore\n";
        let heading = parse(text).remove(0);
        assert_eq!(prose_for(text, &heading), "judgment\nmore");
    }

    #[test]
    fn snippets_are_server_bounded_again_for_the_row() {
        assert_eq!(truncate_chars("short", 8), "short");
        assert_eq!(truncate_chars("123456789", 8), "1234567…");
    }

    #[test]
    fn documents_slice_at_bound_headings_and_agents_triage_locally() {
        let a = agent(1, None, UiAttention::Quiet, 30);
        let b = agent(2, None, UiAttention::NeedsInput, 10);
        let (registry, host) = registry(vec![a.clone(), b.clone()]);
        let text = "* One\nbody\n* Two\n".to_string();
        let mut filed = HashMap::new();
        filed.insert(
            (host, 0),
            sorted_agents(&registry, [a.agent_id, b.agent_id]),
        );
        let segments = generate(
            &registry,
            &[(host, text.clone())],
            &filed,
            &HashSet::new(),
            &HashSet::new(),
            &[],
            None,
        );
        // The document splits after "One"'s body (trailing newline
        // trimmed); triage puts the needs-input agent first.
        assert_eq!(
            keys(&segments),
            vec![
                "doc 0..10".to_string(),
                format!("{:?}", LineKey::Agent(b.agent_id)),
                format!("{:?}", LineKey::Agent(a.agent_id)),
                format!("doc 11..{}", text.len()),
                format!("{:?}", LineKey::NewAgent),
            ]
        );
    }

    #[test]
    fn unsliced_document_is_a_single_writable_segment() {
        let (registry, host) = registry(vec![]);
        let text = "* Parent\n** Child\n* Other\n".to_string();
        let segments = generate(
            &registry,
            &[(host, text.clone())],
            &HashMap::new(),
            &HashSet::new(),
            &HashSet::new(),
            &[],
            None,
        );
        assert_eq!(
            keys(&segments),
            vec![
                format!("doc 0..{}", text.len()),
                format!("{:?}", LineKey::NewAgent),
            ]
        );
    }

    #[test]
    fn collapsed_heading_hides_body_behind_fold_row() {
        let a = agent(1, None, UiAttention::Quiet, 30);
        let (registry, host) = registry(vec![a.clone()]);
        let text = "* One\nbody\n* Two\n".to_string();
        let mut filed = HashMap::new();
        filed.insert((host, 0), vec![a.agent_id]);
        let mut collapsed = HashSet::new();
        collapsed.insert((host, 0));
        let segments = generate(
            &registry,
            &[(host, text.clone())],
            &filed,
            &collapsed,
            &HashSet::new(),
            &[],
            None,
        );
        // Slice ends at the heading line; body and rows are folded.
        assert_eq!(
            keys(&segments),
            vec![
                "doc 0..5".to_string(),
                format!("{:?}", LineKey::Fold(host, 0)),
                format!("doc 11..{}", text.len()),
                format!("{:?}", LineKey::NewAgent),
            ]
        );
    }

    #[test]
    fn staffing_draft_cuts_its_heading_even_without_agents() {
        let (registry, host) = registry(vec![]);
        let text = "* One\nbody\n* Two\n".to_string();
        let segments = generate(
            &registry,
            &[(host, text.clone())],
            &HashMap::new(),
            &HashSet::new(),
            &HashSet::new(),
            &[],
            Some(Some((host, 0))),
        );
        assert_eq!(
            keys(&segments),
            vec![
                "doc 0..10".to_string(),
                format!("{:?}", LineKey::NewDraft(Some((host, 0)))),
                format!("doc 11..{}", text.len()),
                format!("{:?}", LineKey::NewAgent),
            ]
        );
    }

    #[test]
    fn unbound_roots_form_the_unfiled_tail() {
        let root = agent(1, None, UiAttention::Quiet, 1);
        let child = agent(2, Some(root.agent_id), UiAttention::Pending, 2);
        let (registry, host) = registry(vec![root.clone(), child.clone()]);
        let segments = generate(
            &registry,
            &[(host, String::new())],
            &HashMap::new(),
            &HashSet::new(),
            &HashSet::new(),
            &[],
            None,
        );
        let agents = segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Line(line) if matches!(line.key, LineKey::Agent(_)) => Some(line),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].key, LineKey::Agent(root.agent_id));
        assert!(
            agents[0]
                .text
                .contains(&registry.agent_id_label(child.agent_id))
        );
    }

    #[test]
    fn unfiled_tail_folds_behind_its_header() {
        let root = agent(1, None, UiAttention::Quiet, 1);
        let (registry, host) = registry(vec![root.clone()]);
        let mut collapsed_unfiled = HashSet::new();
        collapsed_unfiled.insert(host);
        let segments = generate(
            &registry,
            &[(host, String::new())],
            &HashMap::new(),
            &HashSet::new(),
            &collapsed_unfiled,
            &[],
            None,
        );
        assert!(
            !segments.iter().any(|segment| matches!(
                segment,
                Segment::Line(line) if matches!(line.key, LineKey::Agent(_))
            )),
            "folded Unfiled hides its rows"
        );
        let header = segments
            .iter()
            .find_map(|segment| match segment {
                Segment::Line(line) if line.key == LineKey::Unfiled(host) => Some(line),
                _ => None,
            })
            .expect("header row remains");
        assert!(header.text.contains("· 1"));
    }

    #[test]
    fn heading_lines_style_by_state() {
        let text = "* TODO Ship it\n:project: rho\n* STAFFED Crewed\n* DONE Old\n";
        let spans = doc_spans(text);
        assert!(
            spans
                .iter()
                .any(|(class, range)| matches!(class, DashClass::TodoHeading)
                    && &text[range.clone()] == "TODO")
        );
        assert!(
            spans
                .iter()
                .any(|(class, range)| matches!(class, DashClass::StaffedHeading)
                    && &text[range.clone()] == "STAFFED")
        );
        // A DONE heading's keyword fades but the title keeps the heading
        // color — it should not read as a comment next to its body.
        assert!(
            spans
                .iter()
                .any(|(class, range)| matches!(class, DashClass::Muted)
                    && &text[range.clone()] == "DONE")
        );
        assert!(
            spans
                .iter()
                .any(|(class, range)| matches!(class, DashClass::Heading)
                    && &text[range.clone()] == "Old")
        );
        assert!(
            spans
                .iter()
                .any(|(class, range)| matches!(class, DashClass::Muted)
                    && &text[range.clone()] == ":project: rho")
        );
    }
}
