//! The dashboard: the rail reborn as a real editor buffer — rho's
//! magit-status. A single-root workstream is one compact row; the uncommon
//! multi-root workstream becomes a header followed by human-named root rows.
//! Generated read-only text lives in a normal editor, so cursor motions and
//! search come from the editor rather than bespoke list chrome. Acting keys
//! address the stable root under the cursor: `enter` opens, `r` splices an
//! inline reply draft under that root. Every line is its own tiny buffer in
//! the multibuffer, so refreshes can rearrange excerpts without eating typed
//! drafts or leaving the cursor attached to a stale line number.

use std::collections::HashMap;
use std::ops::Range;

use editor::hover_links::InlayHighlight;
use editor::{Editor, EditorMode, HighlightKey, Inlay, SizingBehavior};
use gpui::prelude::*;
use gpui::{App, Context, Entity, Focusable as _, FontWeight, HighlightStyle, Window};
use language::{Buffer, Capability, Point};
use multi_buffer::{MultiBuffer, PathKey};
use project::InlayId;
use rho_ui_proto::{AgentId, UiAttention, WorkstreamId};
use text::BufferId;
use theme::ActiveTheme as _;

use crate::registry::{AgentRegistry, Workstream};
use crate::workspace::Workspace;

/// Highlight-key space for dashboard classes, clear of the transcript's
/// semantic and syntax key ranges.
const DASHBOARD_KEY_BASE: usize = usize::MAX - 200;

/// Inlay id space for reply-draft placeholders, clear of the lamp ids.
const PLACEHOLDER_ID_BASE: usize = 1_000_000;

/// Highlight key for draft text (the user-message accent), past the
/// class and lamp key ranges.
const DRAFT_TEXT_KEY: HighlightKey =
    HighlightKey::SyntaxTreeView(DASHBOARD_KEY_BASE + 2 * DashClass::ALL.len());

/// Identity of one dashboard line; each key owns one buffer in the
/// multibuffer. Cursor position and reply drafts survive re-sorts by
/// following their key, not their line number.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum LineKey {
    Group(String),
    Stream(WorkstreamId),
    Agent(AgentId),
    FoldToggle,
    Reply(AgentId),
    /// The inline new-agent draft, at the top of the listing.
    NewDraft,
}

/// What the line under the cursor refers to; the object of every
/// dashboard command.
#[derive(Clone, Debug, PartialEq)]
pub enum RowTarget {
    /// Group headers and other inert lines.
    None,
    Stream {
        workstream_id: WorkstreamId,
        root: Option<AgentId>,
    },
    Agent(AgentId),
    FoldToggle,
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
    /// The inline new-agent draft, when open: its buffer plus the edit
    /// subscription that keeps chrome fresh.
    new_draft: Option<(Entity<Buffer>, gpui::Subscription, String)>,
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
        let multi_buffer =
            cx.new(|_| MultiBuffer::without_headers(Capability::ReadWrite));
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
            order: Vec::new(),
            targets: HashMap::new(),
            replies: Vec::new(),
            reply_subscriptions: HashMap::new(),
            new_draft: None,
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

    /// Takes the new-agent draft's text and closes it. `None` when empty.
    pub fn take_new_draft(&mut self, cx: &mut Context<Workspace>) -> Option<String> {
        let (buffer, _, _) = self.new_draft.take()?;
        let text = buffer.read(cx).text().trim().to_owned();
        self.buffers.remove(&LineKey::NewDraft);
        cx.notify();
        (!text.is_empty()).then_some(text)
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
        } else {
            LineKey::Stream(workstream_id)
        });
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
        let lines = generate(registry);

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
            .is_some_and(|(buffer, _, _)| buffer.read(cx).is_empty())
            && cursor_key != Some(LineKey::NewDraft)
            && pending != Some(LineKey::NewDraft)
        {
            self.new_draft = None;
            self.buffers.remove(&LineKey::NewDraft);
        }

        // Interleave: each reply draft directly under its agent's row;
        // drafts whose row is folded away trail the listing so they are
        // never lost off-screen.
        let mut order = Vec::new();
        if self.new_draft.is_some() {
            order.push(LineKey::NewDraft);
        }
        let mut orphans = self.replies.clone();
        for line in &lines {
            order.push(line.key.clone());
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
                order.push(LineKey::Reply(agent_id));
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
        let key = self.cursor_key(cx)?;
        self.targets.get(&key).cloned()
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
                if let Some((_, ranges)) =
                    by_class.iter_mut().find(|(entry, _)| entry == class)
                {
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
                // `highlight_inlays` only ever inserts per inlay id; without
                // the clear, a lamp that was once Working keeps its old entry
                // under the Working key and stays cyan after the attention
                // moves on.
                editor.clear_highlights(class.lamp_key(), cx);
                if !highlights.is_empty() {
                    editor.highlight_inlays(class.lamp_key(), highlights, class.style(cx), cx);
                }
            }
        });
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
                    .map(|(_, _, summary)| {
                        (LineKey::NewDraft, format!("new agent · {summary}…"))
                    }),
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
                let Some(position) =
                    snapshot.anchor_in_excerpt(buffer_snapshot.anchor_after(0))
                else {
                    continue;
                };
                let inlay = Inlay::custom(PLACEHOLDER_ID_BASE + index, position, placeholder);
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

/// Dashboard text classes: lamps and muted chrome. The cursor itself is
/// the selection indicator — rows carry no selected styling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DashClass {
    Muted,
    Working,
    Pending,
    NeedsInput,
    /// Attention at pending or above: the title asks for the eye.
    Urgent,
}

impl DashClass {
    const ALL: [DashClass; 5] = [
        DashClass::Muted,
        DashClass::Working,
        DashClass::Pending,
        DashClass::NeedsInput,
        DashClass::Urgent,
    ];

    fn key(self) -> HighlightKey {
        let slot = match self {
            DashClass::Muted => 0,
            DashClass::Working => 1,
            DashClass::Pending => 2,
            DashClass::NeedsInput => 3,
            DashClass::Urgent => 4,
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

/// One row of the assembled dashboard, in display order.
#[derive(Debug, PartialEq)]
pub enum RailRow<'a> {
    /// A workstream-group section starts; its member tasks follow.
    GroupHeader(&'a str),
    Task {
        topic: &'a Workstream,
        grouped: bool,
    },
    /// The quiet tail's "n more" / "fold" toggle.
    FoldToggle { folded_count: usize, expanded: bool },
}

/// Assembles the dashboard from the split rows: the whole structure as
/// plain data, decided here and only serialized by the caller.
///
/// Expansion merges the folded tail back before grouping, so a group split
/// across the fold reunites instead of repeating its header. A group
/// section anchors at its best-sorted member's position and gathers the
/// rest of the group up to it; ungrouped rows stay put. A non-empty tail
/// trails as the fold toggle.
fn rail_rows<'a>(
    listed: Vec<&'a Workstream>,
    folded: Vec<&'a Workstream>,
    expanded: bool,
) -> Vec<RailRow<'a>> {
    let folded_count = folded.len();
    let display = if expanded {
        listed.into_iter().chain(folded).collect()
    } else {
        listed
    };
    let mut rows = Vec::new();
    let mut seen_groups = std::collections::BTreeSet::new();
    for (index, topic) in display.iter().enumerate() {
        match &topic.group {
            None => rows.push(RailRow::Task {
                topic,
                grouped: false,
            }),
            Some(group) => {
                if !seen_groups.insert(group.clone()) {
                    continue;
                }
                rows.push(RailRow::GroupHeader(group));
                rows.extend(
                    display[index..]
                        .iter()
                        .filter(|member| member.group.as_ref() == Some(group))
                        .map(|member| RailRow::Task {
                            topic: member,
                            grouped: true,
                        }),
                );
            }
        }
    }
    if folded_count > 0 {
        rows.push(RailRow::FoldToggle {
            folded_count,
            expanded,
        });
    }
    rows
}

/// One generated dashboard line: its identity, text, spans (offsets
/// relative to the line), lamp, and what acting on it means.
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

/// Serializes the registry into the dashboard listing.
fn generate(registry: &AgentRegistry) -> Vec<Line> {
    let mut lines = Vec::new();
    let (listed, folded) = registry.split_rows();
    for row in rail_rows(listed, folded, registry.rail_tail_expanded()) {
        match row {
            RailRow::GroupHeader(name) => {
                let mut line = Line::new(LineKey::Group(name.to_owned()), RowTarget::None);
                line.span(Some(DashClass::Muted), |text| text.push_str(name));
                lines.push(line);
            }
            RailRow::Task { topic, grouped } => {
                lines.extend(task_lines(topic, grouped, registry));
            }
            RailRow::FoldToggle {
                folded_count,
                expanded,
            } => {
                let mut line = Line::new(LineKey::FoldToggle, RowTarget::FoldToggle);
                line.span(Some(DashClass::Muted), |text| {
                    if expanded {
                        text.push_str("fold");
                    } else {
                        text.push_str(&format!("{folded_count} more"));
                    }
                });
                lines.push(line);
            }
        }
    }
    lines
}

/// A workstream is flat in the common single-root case. Multiple roots make
/// the container meaningful, so it becomes a header followed by explicit,
/// human-named root rows. Descendants contribute attention to their root but
/// never replace the stable target under the cursor.
fn task_lines(topic: &Workstream, grouped: bool, registry: &AgentRegistry) -> Vec<Line> {
    let roots = registry.ordered_workstream_roots(topic);
    let attention = |root: AgentId| registry.root_attention(topic, root);
    let aggregate = roots
        .iter()
        .map(|root| attention(root.agent_id))
        .max()
        .unwrap_or(UiAttention::Quiet);

    match roots.as_slice() {
        [root] => vec![workstream_line(
            topic,
            grouped,
            Some(root.agent_id),
            attention(root.agent_id),
        )],
        [] => vec![workstream_line(topic, grouped, None, aggregate)],
        _ => {
            let mut lines = vec![workstream_line(topic, grouped, None, aggregate)];
            lines.extend(
                roots
                    .into_iter()
                    .map(|root| root_line(root, grouped, attention(root.agent_id), registry)),
            );
            lines
        }
    }
}

fn workstream_line(
    topic: &Workstream,
    grouped: bool,
    root: Option<AgentId>,
    attention: UiAttention,
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
    if grouped {
        line.span(None, |text| text.push_str("  "));
    }
    if topic.pinned {
        line.span(None, |text| text.push_str("◆ "));
    }
    let title_class = (attention >= UiAttention::Pending).then_some(DashClass::Urgent);
    line.span(title_class, |text| text.push_str(&title));

    // The attention lamp hangs off the row's right end as an inlay.
    if attention > UiAttention::Quiet {
        line.lamp = Some(attention);
    }
    line
}

fn root_line(
    root: &rho_ui_proto::UiAgentSummary,
    grouped: bool,
    attention: UiAttention,
    registry: &AgentRegistry,
) -> Line {
    let mut line = Line::new(LineKey::Agent(root.agent_id), RowTarget::Agent(root.agent_id));
    line.span(None, |text| text.push_str(if grouped { "    " } else { "  " }));
    let class = (attention >= UiAttention::Pending).then_some(DashClass::Urgent);
    line.span(class, |text| {
        text.push_str(&registry.agent_human_name(root.agent_id))
    });
    if attention > UiAttention::Quiet {
        line.lamp = Some(attention);
    }
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
            workstream: WorkstreamId(1),
            labels: match status {
                Status::Normal => Vec::new(),
                Status::Pinned => vec![crate::registry::PIN_LABEL.to_owned()],
            },
        }
    }

    fn topic(status: Status, agents: Vec<UiAgentSummary>) -> Workstream {
        Workstream {
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
        Workstream {
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
                RailRow::GroupHeader(group) => format!("[{group}]"),
                RailRow::Task { topic, grouped } => {
                    format!("{}{}", if *grouped { "  " } else { "" }, topic.name)
                }
                RailRow::FoldToggle {
                    folded_count,
                    expanded,
                } => format!("fold({folded_count},{expanded})"),
            })
            .collect()
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
        let assembled = rail_rows(rows.iter().collect(), Vec::new(), false);
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
    fn expansion_reunites_a_group_split_across_the_fold() {
        let listed = [stream(1, Some("infra")), stream(2, None)];
        let folded = [stream(3, Some("infra"))];

        let collapsed = rail_rows(listed.iter().collect(), folded.iter().collect(), false);
        assert_eq!(
            ids(&collapsed),
            ["[infra]", "  ws-1", "ws-2", "fold(1,false)"]
        );

        let expanded = rail_rows(listed.iter().collect(), folded.iter().collect(), true);
        assert_eq!(
            ids(&expanded),
            ["[infra]", "  ws-1", "  ws-3", "ws-2", "fold(1,true)"]
        );
    }

    #[test]
    fn empty_tail_gets_no_fold_toggle() {
        let listed = [stream(1, None)];
        let assembled = rail_rows(listed.iter().collect(), Vec::new(), false);
        assert_eq!(ids(&assembled), ["ws-1"]);
    }

    #[test]
    fn listing_lines_carry_targets_and_lamp_state() {
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
        assert_eq!(lines.len(), 1);
        assert!(lines[0].text.contains("topic"));
        assert_eq!(lines[0].key, LineKey::Stream(WorkstreamId(1)));
        assert_eq!(lines[0].lamp, Some(UiAttention::NeedsInput));
        assert!(matches!(
            lines[0].target,
            RowTarget::Stream {
                workstream_id: WorkstreamId(1),
                root: Some(agent_id),
            }
            if agent_id == root_id
        ));
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
