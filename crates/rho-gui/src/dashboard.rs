//! The dashboard: the rail reborn as a real editor buffer — rho's
//! magit-status. One line per workstream in triage order, generated
//! read-only text in a normal editor, so the cursor, motions, and search
//! all come from the editor rather than bespoke list chrome. Acting keys
//! address the row under the cursor: `enter` opens, `r` splices an inline
//! reply draft under the row. Every line is its own tiny buffer in the
//! multibuffer, so reply drafts are ordinary writable buffers between
//! read-only ones — a refresh rearranges excerpts but can never eat what
//! the user typed, and the cursor rides its line's buffer through
//! reorders instead of sticking to a line number.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;

use editor::hover_links::InlayHighlight;
use editor::{Editor, EditorMode, HighlightKey, Inlay, SizingBehavior};
use gpui::prelude::*;
use gpui::{App, Context, Entity, Focusable as _, FontWeight, HighlightStyle, WeakEntity, Window};
use language::{Buffer, BufferEvent, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey};
use project::InlayId;
use rho_ui_proto::desk::{DeskHeading, DeskHeadingState, parse};
use rho_ui_proto::{AgentId, UiAttention};
use text::BufferId;
use theme::ActiveTheme as _;

use crate::registry::{AgentRegistry, HostId};
use crate::workspace::Workspace;

/// How many member tags a workstream row shows before collapsing into `+n`.
const VISIBLE_TAGS: usize = 4;

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

/// Identity of one dashboard line; each key owns one buffer in the
/// multibuffer. Cursor position and reply drafts survive re-sorts by
/// following their key, not their line number.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum LineKey {
    Host(HostId),
    Topic(HostId, usize),
    Prose(HostId, usize),
    FoldTopic(HostId, usize),
    Agent(AgentId),
    Unfiled(HostId),
    NewAgent,
    Reply(AgentId),
    NewDraft(Option<(HostId, usize)>),
}

/// What the line under the cursor refers to; the object of every
/// dashboard command.
#[derive(Clone, Debug, PartialEq)]
pub enum RowTarget {
    /// Group headers and other inert lines.
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

pub struct Dashboard {
    multi_buffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    /// One buffer per line key: read-only listing lines and writable
    /// reply drafts alike.
    buffers: HashMap<LineKey, Entity<Buffer>>,
    /// Non-owning references to the workspace-owned Desk source buffers.
    hosts: BTreeMap<HostId, WeakEntity<Buffer>>,
    /// Current display order; index n is the multibuffer's path key n.
    order: Vec<LineKey>,
    /// What each present key means, for cursor lookup.
    targets: HashMap<LineKey, RowTarget>,
    /// Open reply drafts in creation order (position comes from `order`).
    replies: Vec<AgentId>,
    /// Keeps the workspace re-rendering on draft edits, so placeholder
    /// and gutter chrome track the text.
    reply_subscriptions: HashMap<AgentId, gpui::Subscription>,
    /// The inline new-agent draft, when open: its buffer plus the edit
    /// subscription that keeps chrome fresh.
    new_draft: Option<(Option<(HostId, usize)>, Entity<Buffer>, gpui::Subscription)>,
    prose_subscriptions: HashMap<(HostId, usize), gpui::Subscription>,
    collapsed: HashSet<(HostId, usize)>,
    /// Move the cursor into this key's buffer on the next sync — how a
    /// freshly opened reply draft receives the cursor.
    pending_cursor: Option<LineKey>,
    /// Attention lamps currently spliced in, for replacement on sync.
    lamp_ids: Vec<InlayId>,
    /// Reply placeholder inlays currently spliced in.
    placeholder_ids: Vec<InlayId>,
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
            order: Vec::new(),
            targets: HashMap::new(),
            replies: Vec::new(),
            reply_subscriptions: HashMap::new(),
            new_draft: None,
            prose_subscriptions: HashMap::new(),
            collapsed: HashSet::new(),
            pending_cursor: None,
            lamp_ids: Vec::new(),
            placeholder_ids: Vec::new(),
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

    pub fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }

    pub fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle(cx).is_focused(window)
    }

    pub fn set_source(&mut self, host: HostId, source: WeakEntity<Buffer>) {
        self.hosts.insert(host, source);
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

    /// Opens (or returns to) the inline new-agent draft at the top of the
    /// listing. Like a reply draft it parks when left and survives
    /// refreshes.
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
            self.pending_cursor = Some(LineKey::Topic(host, offset));
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

    /// Regenerates the listing from the registry: per-line buffers are
    /// created or edited as needed, arranged (with reply drafts after
    /// their rows), and highlights and lamps reapplied. The cursor
    /// follows its line's buffer through the rearrangement.
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
        let lines = generate(registry, &documents, &self.collapsed);

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

        // Interleave: each reply draft directly under its agent's row;
        // drafts whose row is folded away sit above the new-agent line so
        // they are never lost off-screen.
        let mut order = Vec::new();
        let mut orphans = self.replies.clone();
        let draft_key = self
            .new_draft
            .as_ref()
            .map(|(topic, _, _)| LineKey::NewDraft(*topic));
        for line in &lines {
            if line.key == LineKey::NewAgent {
                for agent_id in orphans.drain(..) {
                    order.push(LineKey::Reply(agent_id));
                }
                if draft_key.as_ref().is_some_and(|key| !order.contains(key)) {
                    order.push(draft_key.clone().unwrap());
                }
            }
            order.push(line.key.clone());
            if let LineKey::Topic(host, offset) = line.key
                && draft_key == Some(LineKey::NewDraft(Some((host, offset))))
            {
                order.push(draft_key.clone().unwrap());
            }
            if let LineKey::Agent(agent_id) = line.key {
                if self.replies.contains(&agent_id) {
                    orphans.retain(|orphan| *orphan != agent_id);
                    order.push(LineKey::Reply(agent_id));
                }
            }
        }

        // Create/refresh the listing buffers.
        let mut edited = std::collections::HashSet::new();
        for line in &lines {
            let writable = matches!(line.key, LineKey::Prose(_, _));
            let buffer = self.buffers.entry(line.key.clone()).or_insert_with(|| {
                cx.new(|cx| {
                    let mut buffer = Buffer::local("", cx);
                    if !writable {
                        buffer.set_capability(Capability::Read, cx);
                    }
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
            if let LineKey::Prose(host, offset) = line.key
                && !self.prose_subscriptions.contains_key(&(host, offset))
            {
                let key = (host, offset);
                self.prose_subscriptions.insert(
                    key,
                    cx.subscribe(buffer, move |workspace, _, event, cx| {
                        if matches!(event, BufferEvent::Edited { .. }) {
                            workspace.dashboard_prose_edited(key, cx);
                        }
                    }),
                );
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
            order.contains(key) || matches!(key, LineKey::Reply(_) | LineKey::NewDraft(_))
        });
        self.prose_subscriptions
            .retain(|key, _| self.buffers.contains_key(&LineKey::Prose(key.0, key.1)));

        self.targets = lines
            .iter()
            .map(|line| (line.key.clone(), line.target.clone()))
            .collect();
        for agent_id in &self.replies {
            self.targets
                .insert(LineKey::Reply(*agent_id), RowTarget::Reply(*agent_id));
        }
        if self.new_draft.is_some() {
            let topic = self.new_draft.as_ref().and_then(|draft| draft.0);
            self.targets
                .insert(LineKey::NewDraft(topic), RowTarget::NewDraft(topic));
        }

        // The cursor follows its buffer: reposition only when the buffer
        // moved or its text was rewritten under the cursor (or a fresh
        // reply draft claims it).
        let moved = |key: &LineKey| {
            self.order.iter().position(|entry| entry == key)
                != order.iter().position(|entry| entry == key)
        };
        let restore = match &self.pending_cursor {
            Some(key) if order.contains(key) => Some(key.clone()),
            _ => match &cursor_key {
                Some(key) if order.contains(key) && (moved(key) || edited.contains(key)) => {
                    Some(key.clone())
                }
                _ => None,
            },
        };
        self.pending_cursor = None;
        self.order = order;
        if let Some(key) = restore {
            self.move_cursor_to(&key, window, cx);
        }

        self.apply_highlights(&lines, cx);
        self.apply_lamps(&lines, cx);
        self.apply_reply_chrome(registry, cx);
    }

    /// Prose islands flush immediately after each local edit. The source
    /// heading body is rebuilt with its property lines first and the edited
    /// prose after them, so `:agent:`/`:project:` remain the shared contract.
    pub(crate) fn flush_prose(&mut self, key: (HostId, usize), cx: &mut Context<Workspace>) {
        let Some(prose) = self
            .buffers
            .get(&LineKey::Prose(key.0, key.1))
            .map(|buffer| buffer.read(cx).text())
        else {
            return;
        };
        let Some(text) = self.source_text(key.0, cx) else {
            return;
        };
        let Some(heading) = parse(&text)
            .into_iter()
            .find(|heading| heading.heading_range.start == key.1)
        else {
            return;
        };
        let mut replacement = String::new();
        for property in &heading.properties {
            replacement.push_str(&text[property.line_range.clone()]);
            replacement.push('\n');
        }
        if !prose.is_empty() {
            replacement.push_str(prose.trim_end_matches('\n'));
            replacement.push('\n');
        }
        if text[heading.body_range.clone()] == replacement {
            return;
        }
        self.hosts[&key.0]
            .upgrade()
            .unwrap()
            .update(cx, |buffer, cx| {
                buffer.edit([(heading.body_range, replacement)], None, cx)
            });
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
    pub fn cursor_target(
        &self,
        _registry: &AgentRegistry,
        cx: &mut Context<Workspace>,
    ) -> Option<RowTarget> {
        let key = self.cursor_key(cx)?;
        self.targets.get(&key).cloned()
    }

    pub fn cursor_topic(&self, cx: &mut Context<Workspace>) -> Option<(HostId, usize)> {
        let key = self.cursor_key(cx)?;
        match key {
            LineKey::Topic(host, offset) | LineKey::Prose(host, offset) => Some((host, offset)),
            LineKey::FoldTopic(host, offset) => Some((host, offset)),
            _ => {
                let index = self.order.iter().position(|candidate| *candidate == key)?;
                for candidate in self.order[..index].iter().rev() {
                    match candidate {
                        LineKey::Topic(host, offset) => return Some((*host, *offset)),
                        LineKey::Host(_) | LineKey::Unfiled(_) => break,
                        _ => {}
                    }
                }
                None
            }
        }
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

    pub fn cursor_on_heading_line(&self, cx: &mut Context<Workspace>) -> bool {
        matches!(
            self.cursor_key(cx),
            Some(LineKey::Topic(_, _) | LineKey::Agent(_))
        )
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
        let Some(LineKey::Topic(host, offset)) = self.cursor_key(cx) else {
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
        let Some(LineKey::Topic(host, offset)) = self.cursor_key(cx) else {
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
        let Some(LineKey::Topic(host, offset)) = self.cursor_key(cx) else {
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

    pub fn file_cursor_agent(
        &mut self,
        registry: &AgentRegistry,
        topic: &str,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let Some(RowTarget::Agent(agent_id)) = self.cursor_target(registry, cx) else {
            return false;
        };
        let root = root_agent(registry, agent_id);
        let Some(host) = registry.host_of_agent(root) else {
            return false;
        };
        let Some(text) = self.source_text(host, cx) else {
            return false;
        };
        let mut removals = Vec::new();
        for heading in parse(&text) {
            for property in heading
                .properties
                .iter()
                .filter(|property| property.key.eq_ignore_ascii_case("agent"))
            {
                if registry
                    .agent_by_label(&property.value)
                    .is_some_and(|bound| root_agent(registry, bound) == root)
                {
                    let end = property.line_range.end
                        + usize::from(text.as_bytes().get(property.line_range.end) == Some(&b'\n'));
                    removals.push(property.line_range.start..end);
                }
            }
        }
        if !removals.is_empty() {
            self.hosts[&host]
                .upgrade()
                .unwrap()
                .update(cx, |buffer, cx| {
                    buffer.edit(removals.into_iter().map(|range| (range, "")), None, cx)
                });
        }
        if topic == "Unfiled" {
            return true;
        }
        let Some(text) = self.source_text(host, cx) else {
            return false;
        };
        let Some(heading) = parse(&text)
            .into_iter()
            .find(|heading| heading.title == topic)
        else {
            return false;
        };
        let insertion = heading.heading_range.end
            + usize::from(text.as_bytes().get(heading.heading_range.end) == Some(&b'\n'));
        let property = format!(
            ":agent: {}\n",
            registry
                .agent_id_label(root)
                .rsplit('/')
                .next()
                .unwrap_or_default()
        );
        self.hosts[&host]
            .upgrade()
            .unwrap()
            .update(cx, |buffer, cx| {
                buffer.edit([(insertion..insertion, property)], None, cx)
            });
        true
    }

    pub fn delete_empty(&mut self, _cx: &mut Context<Workspace>) -> bool {
        false
    }

    pub fn toggle_subagents(&mut self, cx: &mut Context<Workspace>) -> bool {
        let Some(topic) = self.cursor_topic(cx) else {
            return false;
        };
        if !self.collapsed.remove(&topic) {
            self.collapsed.insert(topic);
        }
        cx.notify();
        true
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
        let key = self
            .order
            .iter()
            .find(|key| match key {
                LineKey::Topic(host, offset) => self.source_text(*host, cx).is_some_and(|text| {
                    parse(&text).into_iter().any(|heading| {
                        heading.heading_range.start == *offset && heading.title == title
                    })
                }),
                _ => false,
            })
            .cloned();
        key.is_some_and(|key| {
            self.move_cursor_to(&key, window, cx);
            true
        })
    }

    pub fn next_now(
        &mut self,
        registry: &AgentRegistry,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Option<AgentId> {
        let current = self.cursor_key(cx);
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
        "enter open · r reply · o staff · O topic · d/x verdict · Tab fold · gn attention"
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

    /// Splices the attention lamps in as ` ●` inlays at each row's end —
    /// state chrome the cursor never lands on — and colors them per level.
    fn apply_lamps(&mut self, lines: &[Line], cx: &mut Context<Workspace>) {
        const LAMP_TEXT: &str = " ●";
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let to_remove = std::mem::take(&mut self.lamp_ids);
        let mut inlays = Vec::new();
        let mut by_class: Vec<(DashClass, Vec<InlayHighlight>)> = [
            DashClass::Working,
            DashClass::Pending,
            DashClass::NeedsInput,
        ]
        .into_iter()
        .map(|class| (class, Vec::new()))
        .collect();
        for (index, line) in lines.iter().enumerate() {
            let Some(class) = line.lamp.and_then(DashClass::lamp) else {
                continue;
            };
            let Some(buffer) = self.buffers.get(&line.key) else {
                continue;
            };
            let buffer_snapshot = buffer.read(cx).snapshot();
            let Some(position) =
                snapshot.anchor_in_excerpt(buffer_snapshot.anchor_before(buffer_snapshot.len()))
            else {
                continue;
            };
            let inlay = Inlay::custom(index, position, LAMP_TEXT);
            if let Some((_, highlights)) = by_class.iter_mut().find(|(entry, _)| *entry == class) {
                highlights.push(InlayHighlight {
                    inlay: inlay.id,
                    inlay_position: position,
                    range: 0..LAMP_TEXT.len(),
                });
            }
            self.lamp_ids.push(inlay.id);
            inlays.push(inlay);
        }
        self.editor.update(cx, |editor, cx| {
            editor.splice_inlays(&to_remove, inlays, cx);
            for (class, highlights) in by_class {
                editor.highlight_inlays(class.lamp_key(), highlights, class.style(cx), cx);
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
    MutedHeading,
    Working,
    Pending,
    NeedsInput,
    /// Attention at pending or above: the title asks for the eye.
    Urgent,
}

impl DashClass {
    const ALL: [DashClass; 8] = [
        DashClass::Muted,
        DashClass::Heading,
        DashClass::TodoHeading,
        DashClass::MutedHeading,
        DashClass::Working,
        DashClass::Pending,
        DashClass::NeedsInput,
        DashClass::Urgent,
    ];

    fn key(self) -> HighlightKey {
        let slot = match self {
            DashClass::Muted => 0,
            DashClass::Heading => 1,
            DashClass::TodoHeading => 2,
            DashClass::MutedHeading => 3,
            DashClass::Working => 4,
            DashClass::Pending => 5,
            DashClass::NeedsInput => 6,
            DashClass::Urgent => 7,
        };
        HighlightKey::SyntaxTreeView(DASHBOARD_KEY_BASE + slot)
    }

    /// A parallel key space for lamp inlay highlights.
    fn lamp_key(self) -> HighlightKey {
        let HighlightKey::SyntaxTreeView(slot) = self.key() else {
            unreachable!("dashboard keys are syntax-tree-view keys");
        };
        HighlightKey::SyntaxTreeView(slot + DashClass::ALL.len())
    }

    fn style(self, cx: &App) -> HighlightStyle {
        let colors = cx.theme().colors();
        let color = match self {
            DashClass::Muted => colors.text_muted,
            DashClass::Heading => {
                return HighlightStyle {
                    font_weight: Some(FontWeight::BOLD),
                    ..Default::default()
                };
            }
            DashClass::TodoHeading => {
                return HighlightStyle {
                    color: Some(colors.text_accent.into()),
                    font_weight: Some(FontWeight::BOLD),
                    ..Default::default()
                };
            }
            DashClass::MutedHeading => {
                return HighlightStyle {
                    color: Some(colors.text_muted.into()),
                    font_weight: Some(FontWeight::BOLD),
                    ..Default::default()
                };
            }
            DashClass::Working => colors.terminal_ansi_cyan,
            DashClass::Pending => colors.terminal_ansi_yellow,
            DashClass::NeedsInput => colors.terminal_ansi_red,
            DashClass::Urgent => {
                return HighlightStyle {
                    font_weight: Some(FontWeight::BOLD),
                    ..HighlightStyle::default()
                };
            }
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

/// One generated dashboard line: identity, text, semantic spans, lamp, and
/// the object addressed by dashboard verbs.
#[derive(Debug)]
struct Line {
    key: LineKey,
    text: String,
    spans: Vec<(DashClass, Range<usize>)>,
    lamp: Option<UiAttention>,
    target: RowTarget,
}

impl Line {
    fn new(key: LineKey, target: RowTarget) -> Self {
        Self {
            key,
            text: String::new(),
            spans: Vec::new(),
            lamp: None,
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

fn agent_line(agent_id: AgentId, registry: &AgentRegistry, topic: Option<(HostId, usize)>) -> Line {
    let attention = registry.attention(agent_id);
    let mut line = Line::new(LineKey::Agent(agent_id), RowTarget::Agent(agent_id));
    if registry.agent_pinned(agent_id) {
        line.span(None, |text| text.push_str("◆ "));
    }
    line.span(
        (attention >= UiAttention::Pending).then_some(DashClass::Urgent),
        |text| text.push_str(&registry.agent_human_name(agent_id)),
    );

    let members = registry.agent_subtree(agent_id);
    let members = members.into_iter().skip(1).collect::<Vec<_>>();
    let overflow = members.len().saturating_sub(VISIBLE_TAGS);
    for member in members.into_iter().take(VISIBLE_TAGS) {
        line.span(None, |text| text.push_str("  "));
        let class = DashClass::lamp(registry.attention(member)).unwrap_or(DashClass::Muted);
        line.span(Some(class), |text| {
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
            let snippet = truncate_chars(reason, 80);
            line.span(None, |text| text.push_str("  "));
            line.span(Some(DashClass::Muted), |text| text.push_str(&snippet));
        }
    }
    if attention > UiAttention::Quiet {
        line.lamp = Some(attention);
    }
    let _ = topic;
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

/// Generate the view without mutating Desk text. Topic order comes directly
/// from the documents; each topic's agents are independently triaged by
/// attention then recency. Root agents absent from all headings form the
/// generated Unfiled tail.
fn generate(
    registry: &AgentRegistry,
    documents: &[(HostId, String)],
    collapsed: &HashSet<(HostId, usize)>,
) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut filed = HashSet::new();
    let multiple_hosts = documents.len() > 1;

    for (host, text) in documents {
        if multiple_hosts {
            let mut header = Line::new(LineKey::Host(*host), RowTarget::None);
            header.span(Some(DashClass::Muted), |line| {
                line.push_str(registry.host_name(*host))
            });
            lines.push(header);
        }
        let headings = parse(text);
        for heading in &headings {
            let mut topic_agents = Vec::new();
            for property in heading
                .properties
                .iter()
                .filter(|property| property.key.eq_ignore_ascii_case("agent"))
            {
                let Some(agent_id) = registry.agent_by_label(&property.value) else {
                    continue;
                };
                if registry.host_of_agent(agent_id) != Some(*host) {
                    continue;
                }
                let root = root_agent(registry, agent_id);
                if filed.insert(root) {
                    topic_agents.push(root);
                }
            }
            topic_agents = sorted_agents(registry, topic_agents);
            let first_attention = topic_agents
                .iter()
                .copied()
                .find(|agent_id| registry.attention(*agent_id) >= UiAttention::Pending);

            let mut topic = Line::new(
                LineKey::Topic(*host, heading.heading_range.start),
                RowTarget::Topic {
                    host: *host,
                    offset: heading.heading_range.start,
                    first_attention,
                },
            );
            let class = match heading.state {
                Some(DeskHeadingState::Todo) => Some(DashClass::TodoHeading),
                Some(DeskHeadingState::Done | DeskHeadingState::Discarded) => {
                    Some(DashClass::MutedHeading)
                }
                None => Some(DashClass::Heading),
            };
            // Rows triage flat (like org-agenda), so nested headings carry
            // their ancestry as a breadcrumb instead of indentation.
            let mut crumbs = Vec::new();
            let mut cursor = heading.parent;
            while let Some(parent) = cursor {
                crumbs.push(headings[parent].title.as_str());
                cursor = headings[parent].parent;
            }
            if !crumbs.is_empty() {
                crumbs.reverse();
                topic.span(Some(DashClass::Muted), |line| {
                    for crumb in &crumbs {
                        line.push_str(crumb);
                        line.push_str(" ▸ ");
                    }
                });
            }
            topic.span(class, |line| line.push_str(&heading.title));
            lines.push(topic);

            let prose = prose_for(text, heading);
            let folded_count = topic_agents.len() + usize::from(!prose.is_empty());
            if collapsed.contains(&(*host, heading.heading_range.start)) {
                if folded_count > 0 {
                    let mut fold = Line::new(
                        LineKey::FoldTopic(*host, heading.heading_range.start),
                        RowTarget::Topic {
                            host: *host,
                            offset: heading.heading_range.start,
                            first_attention,
                        },
                    );
                    fold.span(Some(DashClass::Muted), |line| {
                        line.push_str(&format!("{folded_count} more"))
                    });
                    lines.push(fold);
                }
                continue;
            }
            if !prose.is_empty() {
                let mut island = Line::new(
                    LineKey::Prose(*host, heading.heading_range.start),
                    RowTarget::None,
                );
                island.text = prose;
                lines.push(island);
            }
            lines.extend(topic_agents.into_iter().map(|agent_id| {
                agent_line(
                    agent_id,
                    registry,
                    Some((*host, heading.heading_range.start)),
                )
            }));
            if folded_count > 0 {
                let mut fold = Line::new(
                    LineKey::FoldTopic(*host, heading.heading_range.start),
                    RowTarget::Topic {
                        host: *host,
                        offset: heading.heading_range.start,
                        first_attention,
                    },
                );
                fold.span(Some(DashClass::Muted), |line| line.push_str("fold"));
                lines.push(fold);
            }
        }
    }

    for (host, _) in documents {
        let unfiled = sorted_agents(
            registry,
            registry
                .known_agents()
                .copied()
                .filter(|agent_id| registry.host_of_agent(*agent_id) == Some(*host))
                .filter(|agent_id| registry.agent_parent(*agent_id).is_none())
                .filter(|agent_id| !registry.agent_hidden(*agent_id))
                .filter(|agent_id| !filed.contains(agent_id)),
        );
        if unfiled.is_empty() {
            continue;
        }
        let mut header = Line::new(LineKey::Unfiled(*host), RowTarget::None);
        header.span(Some(DashClass::Muted), |line| {
            line.push_str("Unfiled");
            if multiple_hosts {
                line.push_str(" · ");
                line.push_str(registry.host_name(*host));
            }
        });
        lines.push(header);
        lines.extend(
            unfiled
                .into_iter()
                .map(|agent_id| agent_line(agent_id, registry, None)),
        );
    }

    let mut new_agent = Line::new(LineKey::NewAgent, RowTarget::NewAgent);
    new_agent.span(Some(DashClass::Muted), |line| line.push_str("+ new agent"));
    lines.push(new_agent);
    lines
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
    fn topics_keep_document_order_while_agents_triage_locally() {
        let a = agent(1, None, UiAttention::Quiet, 30);
        let b = agent(2, None, UiAttention::NeedsInput, 10);
        let c = agent(3, None, UiAttention::Pending, 20);
        let (registry, host) = registry(vec![a.clone(), b.clone(), c.clone()]);
        let text = format!(
            "* Later\n:agent: {}\n:agent: {}\n* Earlier\n:agent: {}\n",
            registry.agent_id_label(a.agent_id),
            registry.agent_id_label(b.agent_id),
            registry.agent_id_label(c.agent_id),
        );
        let rows = generate(&registry, &[(host, text)], &HashSet::new());
        let keys = rows.into_iter().map(|line| line.key).collect::<Vec<_>>();
        assert_eq!(keys[0], LineKey::Topic(host, 0));
        assert_eq!(keys[1], LineKey::Agent(b.agent_id));
        assert_eq!(keys[2], LineKey::Agent(a.agent_id));
        assert!(keys.iter().any(
            |key| matches!(key, LineKey::Topic(owner, offset) if *owner == host && *offset > 0)
        ));
    }

    #[test]
    fn nested_topics_carry_breadcrumbs_not_indentation() {
        let (registry, host) = registry(vec![]);
        let text = "* Parent\n** Child\n* Other\n".to_string();
        let rows = generate(&registry, &[(host, text)], &HashSet::new());
        let topics = rows
            .iter()
            .filter(|line| matches!(line.key, LineKey::Topic(..)))
            .collect::<Vec<_>>();
        assert_eq!(topics.len(), 3);
        assert_eq!(topics[0].text, "Parent");
        assert_eq!(topics[1].text, "Parent ▸ Child");
        assert_eq!(topics[2].text, "Other");
    }

    #[test]
    fn subagents_ride_the_root_row() {
        let root = agent(1, None, UiAttention::Quiet, 1);
        let child = agent(2, Some(root.agent_id), UiAttention::Pending, 2);
        let (registry, host) = registry(vec![root.clone(), child.clone()]);
        let rows = generate(&registry, &[(host, String::new())], &HashSet::new());
        let agents = rows
            .iter()
            .filter(|line| matches!(line.key, LineKey::Agent(_)))
            .collect::<Vec<_>>();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].key, LineKey::Agent(root.agent_id));
        assert!(
            agents[0]
                .text
                .contains(&registry.agent_id_label(child.agent_id))
        );
    }
}
