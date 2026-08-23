//! The dashboard: the Desk document as the home surface — rho's
//! magit-status. The real per-host CRDT document is spliced into the
//! editor as writable excerpts, so headings and prose are edited
//! directly with plain vim, while generated read-only agent rows are
//! interleaved under the headings whose visible tags name them. Headings
//! normally summarize bindings in an end-of-line hint; `g t` temporarily
//! projects the named agents' shared runtime rows and complete spawn trees.
//! Acting keys address the row under the cursor: `enter` opens, `r`
//! splices an inline reply draft under the row. Generated rows
//! and drafts sit between document slices — a refresh rearranges excerpts
//! but can never eat what the user typed.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;

use editor::scroll::Autoscroll;
use editor::{
    Editor, EditorMode, HighlightKey, Inlay, RowHighlightOptions, SelectionEffects, SizingBehavior,
};
use gpui::prelude::*;
use gpui::{App, Context, Entity, Focusable as _, HighlightStyle, WeakEntity, Window};
use language::{Buffer, Capability, InlayId, Point};
use multi_buffer::composition::{Composition, CompositionSpec, CutSpec, RowSpec, SectionSpec};
use multi_buffer::{MultiBuffer, ToOffset as _};
use rho_ui_proto::desk::temporal::{is_overdue_deadline, priority, property_line};
use rho_ui_proto::desk::{DeskHeading, DeskHeadingState, TemporalMark, TemporalMarkKind, parse};
use rho_ui_proto::{AgentId, UiAttention};
use text::{BufferId, ToOffset as _};
use theme::ActiveTheme as _;

use crate::registry::{AgentRegistry, HostId};
use crate::workspace::Workspace;

/// Highlight-key space for dashboard classes, clear of the transcript's
/// semantic and syntax key ranges.
const DASHBOARD_KEY_BASE: usize = usize::MAX - 200;

/// Inlay id space for reply-draft placeholders, clear of the lamp ids.
const PLACEHOLDER_ID_BASE: usize = 1_000_000;

/// Title a quick-spawned heading carries until the agent's generated
/// summary replaces it.
const PLACEHOLDER_TITLE: &str = "…";

/// The reserved heading tag that makes a subtree an archive zone: agents
/// whose binding tag lives under it are muted, and the zone folds by
/// default. Archiving and unarchiving are ordinary text moves.
const ARCHIVE_TAG: &str = "archive";

/// Highlight key for draft text (the user-message accent), past the
/// class and lamp key ranges.
const DRAFT_TEXT_KEY: HighlightKey =
    HighlightKey::SyntaxTreeView(DASHBOARD_KEY_BASE + 2 * DashClass::ALL.len());
const WEB_HEADING_DECORATION: &str = "\0web";

type DraftTopic = Option<(HostId, usize)>;
type DraftState = (DraftTopic, Entity<Buffer>, gpui::Subscription);
type SyncSnapshot = (
    Vec<(HostId, String)>,
    Vec<Segment>,
    Vec<(LineKey, String)>,
    Vec<(HostId, usize, String)>,
    Vec<(HostId, Range<usize>)>,
    Vec<(HostId, Range<usize>, String, editor::display_map::CaretRest)>,
);
type ArchiveEdits = (Vec<(Range<usize>, String)>, usize);

const DEAL_QUEUE_FLOOR: f64 = -1.0;
const DEAL_PRIORITY_CUTOFF: usize = 8;
const BLOCKED_REPLY_HEAD_START: f64 = 1.0;
const BLOCKED_REPLY_SLOPE_PER_DAY: f64 = 12.0;
const FYI_REPLY_PACE_DAYS: f64 = 3.0;
const DEAL_HINT: &str = "r reply · d done · x discard · z snooze · t todo · n next · q quit";

struct DealCardHighlight;

#[derive(Clone, Debug, PartialEq)]
pub struct DealCard {
    pub label: String,
    pub priority: f64,
    pub host: HostId,
    pub heading_offset: Option<usize>,
    pub agent_id: Option<AgentId>,
    pub agent_tag: Option<String>,
    pub breadcrumb: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DealQueue {
    pub cards: Vec<DealCard>,
    /// Number of live headings whose winning mark is above the queue floor.
    pub total_alive: usize,
    /// Number selected by global priority, excluding the Random tail.
    pub dealt_count: usize,
    priority_topics: HashSet<(HostId, usize)>,
    random_topics: HashSet<(HostId, usize)>,
}

impl DealQueue {
    /// Revalidates a persisted card without applying the current top-N cutoff
    /// or changing its established order.
    pub fn is_live(&self, card: &DealCard) -> bool {
        let Some(offset) = card.heading_offset else {
            return false;
        };
        let topic = (card.host, offset);
        if card.label == "random" {
            self.random_topics.contains(&topic)
        } else {
            self.priority_topics.contains(&topic)
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DealQueueDepth {
    pub dealt_count: usize,
    pub total_alive: usize,
}

#[derive(Clone, Debug)]
struct DealSession {
    cards: Vec<DealCard>,
    anchors: Vec<Option<(HostId, text::Anchor)>>,
    boundary_anchors: Vec<Option<text::Anchor>>,
    index: usize,
    verdicts: usize,
    verdict_recorded: bool,
}

struct ListingVisibility<'a> {
    collapsed_unfiled: &'a HashSet<HostId>,
    expanded_portals: &'a HashSet<AgentOccurrence>,
    raw: bool,
}

#[derive(Clone, Copy)]
pub enum StructureDirection {
    Demote,
    Promote,
}

/// Identity of one generated line; each key owns one buffer in the
/// multibuffer. Reply drafts survive re-sorts by following their key,
/// not their line number. Document text is not keyed — it lives in the
/// shared Desk buffers directly.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum LineKey {
    Host(HostId),
    Agent {
        agent_id: AgentId,
        occurrence: AgentOccurrence,
    },
    Unfiled(HostId),
    Reply(AgentId),
    NewDraft(Option<(HostId, usize)>),
    /// An empty line separating listing regions.
    Spacer(HostId),
}

/// One place an agent's shared runtime row is projected. The occurrence is
/// row identity only; every occurrence points at the same per-agent buffer.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum AgentOccurrence {
    Filed {
        host: HostId,
        heading: u64,
        portal: AgentId,
    },
    Unfiled {
        host: HostId,
        portal: AgentId,
    },
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
        /// Whether the cursor sits on the heading line itself, or merely
        /// somewhere in its subtree. Verbs that share keys with vim text
        /// editing (`r`, folding on bare enter) require the line.
        on_heading_line: bool,
    },
    Agent {
        agent_id: AgentId,
        topic: Option<(HostId, usize)>,
    },
    /// An inline reply draft addressed to this agent.
    Reply(AgentId),
    /// The inline new-agent draft.
    NewDraft(Option<(HostId, usize)>),
    #[cfg(feature = "native")]
    Page(rho_browser::PageId),
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
    /// One live summary buffer per agent, projected through any number of
    /// occurrence-specific composition rows.
    agent_buffers: HashMap<AgentId, Entity<Buffer>>,
    /// Non-owning references to the workspace-owned Desk source buffers.
    hosts: BTreeMap<HostId, WeakEntity<Buffer>>,
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
    #[cfg(feature = "native")]
    heading_pages: HashMap<(HostId, usize), rho_browser::PageId>,
    /// Every page tag, including additional tags on a heading whose preview
    /// can display only one page.
    #[cfg(feature = "native")]
    referenced_pages: HashSet<rho_browser::PageId>,
    /// Roots whose binding tag lives inside an `:archive:` zone, as of the
    /// last sync. Archived agents are muted: no chime, quiet decorations.
    archived_agents: HashSet<AgentId>,
    /// Open reply drafts in creation order (position comes from `order`).
    replies: Vec<AgentId>,
    /// Keeps the workspace re-rendering on draft edits, so placeholder
    /// and gutter chrome track the text.
    reply_subscriptions: HashMap<AgentId, gpui::Subscription>,
    /// The inline new-agent draft, when open: its buffer plus the edit
    /// subscription that keeps chrome fresh.
    new_draft: Option<DraftState>,
    /// Collapsed subtrees as anchored fold ranges, org-style: the fold
    /// is persistent state that rides edits, not something re-derived
    /// from the parse. The start anchor is right-biased (org's
    /// front-sticky through our newline-shifted boundary: typing at the
    /// end of the title stays visible) and the end anchor left-biased
    /// (rear-nonsticky: a line opened below a folded heading stays
    /// outside and visible). Ranges are recomputed only by explicit
    /// operations — cycling, archiving — like org recomputes on cycle;
    /// a range whose start no longer sits on a heading line is dropped.
    collapsed: HashMap<HostId, Vec<(text::Anchor, text::Anchor)>>,
    /// Next S-TAB target in org's OVERVIEW → CONTENTS → SHOW ALL cycle.
    global_cycle: u8,
    /// Hosts whose Unfiled tail is folded behind its header.
    collapsed_unfiled: HashSet<HostId>,
    /// Shows only literal editable Desk source, with no generated UI.
    raw_mode: bool,
    deal: Option<DealSession>,
    queue_depth: DealQueueDepth,
    queue_depth_revision: Option<u64>,
    queue_depth_minute: Option<i64>,
    /// Portal occurrences whose complete runtime subtree is visible.
    /// This is transient display state and is never written to Desk.
    expanded_portals: HashSet<AgentOccurrence>,
    /// Move the cursor into this key's buffer on the next sync — how a
    /// freshly opened reply draft receives the cursor.
    pending_cursor: Option<LineKey>,
    /// Move the cursor to this document offset on the next sync.
    pending_doc_cursor: Option<(HostId, usize)>,
    /// Reply placeholder inlays currently spliced in.
    placeholder_ids: Vec<InlayId>,
    /// The previous pass's inputs and output, so a sync whose world is
    /// unchanged returns without touching the editor.
    last_synced: Option<SyncSnapshot>,
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
            #[cfg(feature = "native")]
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
            #[cfg(not(feature = "native"))]
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: SizingBehavior::ExcludeOverscrollMargin,
                },
                multi_buffer.clone(),
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
            agent_buffers: HashMap::new(),
            hosts: BTreeMap::new(),
            composition: Composition::default(),
            element_keys: HashMap::new(),
            next_element_key: 0,
            order: Vec::new(),
            targets: HashMap::new(),
            heading_agents: HashMap::new(),
            #[cfg(feature = "native")]
            heading_pages: HashMap::new(),
            #[cfg(feature = "native")]
            referenced_pages: HashSet::new(),
            archived_agents: HashSet::new(),
            replies: Vec::new(),
            reply_subscriptions: HashMap::new(),
            new_draft: None,
            collapsed: HashMap::new(),
            global_cycle: 0,
            collapsed_unfiled: HashSet::new(),
            raw_mode: false,
            deal: None,
            queue_depth: DealQueueDepth::default(),
            queue_depth_revision: None,
            queue_depth_minute: None,
            expanded_portals: HashSet::new(),
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
            .chain(self.agent_buffers.values())
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

    #[cfg(feature = "native")]
    pub fn page_ids(&self) -> HashSet<rho_browser::PageId> {
        self.referenced_pages.clone()
    }

    fn buffer_for_key(&self, key: &LineKey) -> Option<&Entity<Buffer>> {
        match key {
            LineKey::Agent { agent_id, .. } => self.agent_buffers.get(agent_id),
            _ => self.buffers.get(key),
        }
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

    pub fn deal_mode(&self) -> bool {
        self.deal.is_some()
    }

    pub fn deal_waiting(&self) -> usize {
        self.deal
            .as_ref()
            .map_or(0, |deal| deal.cards.len().saturating_sub(deal.index))
    }

    #[cfg(test)]
    pub fn deal_highlight_active_for_test(&self, cx: &App) -> bool {
        self.editor
            .read(cx)
            .highlighted_rows::<DealCardHighlight>(cx)
            .next()
            .is_some()
    }

    #[cfg(test)]
    pub fn current_deal_topic_for_test(&self) -> Option<(HostId, usize, &str)> {
        let card = self.current_deal_card()?;
        Some((card.host, card.heading_offset?, card.breadcrumb.as_str()))
    }

    pub fn enter_deal_mode(
        &mut self,
        registry: &AgentRegistry,
        now: chrono::DateTime<chrono::FixedOffset>,
        seed: u64,
        cx: &mut Context<Workspace>,
    ) {
        let documents = self
            .hosts
            .keys()
            .filter_map(|host| self.source_text(*host, cx).map(|text| (*host, text)))
            .collect::<Vec<_>>();
        let queue = assemble_deal_queue(&documents, &deal_agent_facts(registry), now, seed);
        let cards = queue.cards;
        let anchors = cards
            .iter()
            .map(|card| {
                let offset = card.heading_offset?;
                let source = self.hosts.get(&card.host)?.upgrade()?;
                Some((card.host, source.read(cx).anchor_before(offset)))
            })
            .collect::<Vec<_>>();
        // The before-biased anchor remains the card's durable identity.
        // Its after-biased mate only disambiguates an insertion made exactly
        // at the heading boundary, where both sides otherwise resolve to
        // different content.
        let boundary_anchors = cards
            .iter()
            .map(|card| {
                let offset = card.heading_offset?;
                let source = self.hosts.get(&card.host)?.upgrade()?;
                Some(source.read(cx).anchor_after(offset))
            })
            .collect();
        self.raw_mode = false;
        for source in self.hosts.values().filter_map(WeakEntity::upgrade) {
            source.update(cx, |buffer, cx| buffer.set_capability(Capability::Read, cx));
        }
        self.deal = Some(DealSession {
            cards,
            anchors,
            boundary_anchors,
            index: 0,
            verdicts: 0,
            verdict_recorded: false,
        });
        if let Some(card) = self.current_deal_card() {
            if let Some(offset) = card.heading_offset {
                self.pending_doc_cursor = Some((card.host, offset));
            } else if let Some(agent_id) = card.agent_id {
                self.pending_cursor = Some(LineKey::Agent {
                    agent_id,
                    occurrence: AgentOccurrence::Unfiled {
                        host: card.host,
                        portal: agent_id,
                    },
                });
            }
        }
        self.last_synced = None;
    }

    pub fn exit_deal_mode(&mut self, cx: &mut Context<Workspace>) -> bool {
        let exited = self.deal.take().is_some();
        if exited {
            for source in self.hosts.values().filter_map(WeakEntity::upgrade) {
                source.update(cx, |buffer, cx| {
                    buffer.set_capability(Capability::ReadWrite, cx)
                });
            }
        }
        self.last_synced = None;
        exited
    }

    pub fn advance_deal(&mut self, cx: &mut Context<Workspace>) -> bool {
        let Some(deal) = &mut self.deal else {
            return false;
        };
        if deal.index < deal.cards.len() {
            deal.index += 1;
            deal.verdict_recorded = false;
        }
        self.refresh_current_deal_offset(cx);
        if let Some(card) = self.current_deal_card() {
            if let Some(offset) = card.heading_offset {
                self.pending_doc_cursor = Some((card.host, offset));
            } else if let Some(agent_id) = card.agent_id {
                self.pending_cursor = Some(LineKey::Agent {
                    agent_id,
                    occurrence: AgentOccurrence::Unfiled {
                        host: card.host,
                        portal: agent_id,
                    },
                });
            }
        }
        self.last_synced = None;
        true
    }

    fn refresh_current_deal_offset(&mut self, cx: &App) -> bool {
        let Some(deal) = &self.deal else {
            return false;
        };
        if deal.index >= deal.cards.len() {
            return false;
        }
        let Some((host, anchor)) = deal.anchors.get(deal.index).and_then(Clone::clone) else {
            return true;
        };
        let Some(source) = self.hosts.get(&host).and_then(|source| source.upgrade()) else {
            return false;
        };
        let snapshot = source.read(cx).snapshot();
        let before_offset = anchor.to_offset(&snapshot);
        let after_offset = deal
            .boundary_anchors
            .get(deal.index)
            .and_then(Clone::clone)
            .map(|anchor| anchor.to_offset(&snapshot))
            .unwrap_or(before_offset);
        let text = snapshot.text();
        let headings = parse(&text);
        let previous_breadcrumb = &deal.cards[deal.index].breadcrumb;
        let previous_leaf = previous_breadcrumb
            .rsplit(" › ")
            .next()
            .unwrap_or(previous_breadcrumb);
        let heading_at = |offset| {
            headings
                .iter()
                .find(|heading| heading.heading_range.start == offset)
        };
        let offset = [before_offset, after_offset]
            .into_iter()
            .find(|offset| {
                headings.iter().enumerate().any(|(index, heading)| {
                    heading.heading_range.start == *offset
                        && heading_breadcrumb(&headings, index) == *previous_breadcrumb
                })
            })
            .or_else(|| {
                (before_offset == after_offset && heading_at(before_offset).is_some())
                    .then_some(before_offset)
            })
            .or_else(|| {
                [before_offset, after_offset].into_iter().find(|offset| {
                    heading_at(*offset).is_some_and(|heading| heading.title == previous_leaf)
                })
            })
            .or_else(|| {
                let mut matches = headings.iter().enumerate().filter_map(|(index, heading)| {
                    (heading_breadcrumb(&headings, index) == *previous_breadcrumb)
                        .then_some(heading.heading_range.start)
                });
                let matched = matches.next()?;
                matches.next().is_none().then_some(matched)
            })
            .or_else(|| heading_at(before_offset).map(|_| before_offset))
            .or_else(|| heading_at(after_offset).map(|_| after_offset))
            .unwrap_or(after_offset.max(before_offset));
        let found = headings
            .iter()
            .enumerate()
            .find(|(_, heading)| heading.heading_range.start == offset)
            .map(|(index, heading)| {
                (
                    heading.heading_range.start,
                    heading_breadcrumb(&headings, index),
                )
            });
        if let Some((offset, breadcrumb)) = found
            && let Some(deal) = &mut self.deal
            && let Some(card) = deal.cards.get_mut(deal.index)
        {
            card.heading_offset = Some(offset);
            card.breadcrumb = breadcrumb;
            deal.anchors[deal.index] = Some((host, snapshot.anchor_before(offset)));
            deal.boundary_anchors[deal.index] = Some(snapshot.anchor_after(offset));
            return true;
        }
        if let Some(deal) = &mut self.deal
            && let Some(card) = deal.cards.get_mut(deal.index)
        {
            card.heading_offset = None;
        }
        false
    }

    pub fn deal_accepts_verdict(&self) -> bool {
        self.deal
            .as_ref()
            .is_some_and(|deal| !deal.verdict_recorded && deal.index < deal.cards.len())
    }

    pub fn record_deal_verdict(&mut self) {
        if let Some(deal) = &mut self.deal
            && !deal.verdict_recorded
        {
            deal.verdict_recorded = true;
            deal.verdicts += 1;
        }
    }

    pub fn current_deal_card(&self) -> Option<&DealCard> {
        let deal = self.deal.as_ref()?;
        deal.cards.get(deal.index)
    }

    pub fn open_plain_deal_heading(&mut self, cx: &mut Context<Workspace>) -> bool {
        let card = self.current_deal_card().cloned();
        let Some(DealCard {
            host,
            heading_offset: Some(offset),
            agent_id: None,
            ..
        }) = card
        else {
            return false;
        };
        self.exit_deal_mode(cx);
        self.pending_doc_cursor = Some((host, offset));
        true
    }

    pub fn write_deal_snooze(
        &mut self,
        count: u32,
        today: chrono::NaiveDate,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let days = count.max(1);
        self.write_deal_property(
            TemporalMarkKind::Defer,
            today.and_time(chrono::NaiveTime::MIN) + chrono::Duration::days(days.into()),
            Some(days),
            cx,
        )
    }

    pub fn write_deal_todo(
        &mut self,
        today: chrono::NaiveDate,
        cx: &mut Context<Workspace>,
    ) -> bool {
        self.write_deal_property(
            TemporalMarkKind::Todo,
            today.and_time(chrono::NaiveTime::MIN),
            Some(7),
            cx,
        )
    }

    pub fn write_deal_done(
        &mut self,
        today: chrono::NaiveDate,
        cx: &mut Context<Workspace>,
    ) -> bool {
        self.write_deal_property(
            TemporalMarkKind::Done,
            today.and_time(chrono::NaiveTime::MIN),
            None,
            cx,
        )
    }

    pub fn write_deal_discarded(
        &mut self,
        today: chrono::NaiveDate,
        cx: &mut Context<Workspace>,
    ) -> bool {
        self.write_deal_property(
            TemporalMarkKind::Discarded,
            today.and_time(chrono::NaiveTime::MIN),
            None,
            cx,
        )
    }

    pub fn write_deal_property(
        &mut self,
        kind: rho_ui_proto::desk::TemporalMarkKind,
        at: chrono::NaiveDateTime,
        pace_days: Option<u32>,
        cx: &mut Context<Workspace>,
    ) -> bool {
        if !self.deal_accepts_verdict() {
            return false;
        }
        if !self.refresh_current_deal_offset(cx) {
            return false;
        }
        let Some(card) = self.current_deal_card().cloned() else {
            return false;
        };
        let Some(offset) = card.heading_offset else {
            let Some(text) = self.source_text(card.host, cx) else {
                return false;
            };
            let line = rho_ui_proto::desk::temporal::property_line(kind, at, pace_days);
            let separator = if text.is_empty() || text.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            let Some(agent_tag) = card.agent_tag.as_deref() else {
                return false;
            };
            let addition = format!("{separator}* Agent {agent_tag} :{agent_tag}:\n{line}");
            let offset = text.len() + separator.len();
            self.hosts[&card.host]
                .upgrade()
                .unwrap()
                .update(cx, |buffer, cx| {
                    buffer.edit([(text.len()..text.len(), addition)], None, cx)
                });
            if let Some(deal) = &mut self.deal
                && let Some(current) = deal.cards.get_mut(deal.index)
            {
                current.heading_offset = Some(offset);
            }
            self.record_deal_verdict();
            return true;
        };
        self.set_heading_property(card.host, offset, kind, at, pace_days, cx)
    }

    fn set_heading_property(
        &mut self,
        host: HostId,
        offset: usize,
        kind: rho_ui_proto::desk::TemporalMarkKind,
        at: chrono::NaiveDateTime,
        pace_days: Option<u32>,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let Some(text) = self.source_text(host, cx) else {
            return false;
        };
        let Some(heading) = parse(&text)
            .into_iter()
            .find(|heading| heading.heading_range.start == offset)
        else {
            return false;
        };
        let line = rho_ui_proto::desk::temporal::property_line(kind, at, pace_days);
        let mut edits = heading_property_edits(&text, &heading, kind, line);
        edits.sort_by_key(|(range, _)| range.start);
        let delta = edits
            .iter()
            .map(|edit| edit.1.len() as isize - (edit.0.end - edit.0.start) as isize)
            .sum::<isize>();
        let edit_start = edits[0].0.start;
        self.hosts[&host]
            .upgrade()
            .unwrap()
            .update(cx, |buffer, cx| buffer.edit(edits, None, cx));
        if delta != 0
            && let Some(deal) = &mut self.deal
        {
            for card in deal.cards.iter_mut().skip(deal.index + 1) {
                if card.host == host
                    && let Some(offset) = &mut card.heading_offset
                    && *offset > edit_start
                {
                    *offset = offset.saturating_add_signed(delta);
                }
            }
        }
        self.record_deal_verdict();
        true
    }

    /// Opens (or returns to) an inline reply draft under the agent's row.
    /// The draft is a writable buffer of its own: it parks where it is
    /// when the user wanders off and survives every refresh.
    pub fn open_reply(
        &mut self,
        agent_id: AgentId,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
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
                cx.subscribe_in(&buffer, window, |this, _, event, window, cx| {
                    if matches!(event, language::BufferEvent::Edited { .. }) {
                        this.refresh_dashboard(window, cx);
                    }
                }),
            );
        }
        self.pending_cursor = Some(key);
        cx.notify();
    }

    #[cfg(test)]
    pub fn reply_text_for_test(&self, agent_id: AgentId, cx: &App) -> Option<String> {
        self.buffers
            .get(&LineKey::Reply(agent_id))
            .map(|buffer| buffer.read(cx).text())
    }

    /// Opens (or returns to) the inline new-agent draft. Like a reply
    /// draft it parks when left and survives refreshes.
    pub fn open_new_draft(
        &mut self,
        topic: Option<(HostId, usize)>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if self.new_draft.is_none() {
            let buffer = cx.new(|cx| Buffer::local("", cx));
            let subscription = cx.subscribe_in(&buffer, window, |this, _, event, window, cx| {
                if matches!(event, language::BufferEvent::Edited { .. }) {
                    this.refresh_dashboard(window, cx);
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

    /// Parks the cursor at a document offset on the next sync.
    pub fn cursor_to_doc(&mut self, host: HostId, offset: usize, cx: &mut Context<Workspace>) {
        self.pending_doc_cursor = Some((host, offset));
        cx.notify();
    }

    pub fn cursor_to_agent(&mut self, agent_id: AgentId, cx: &mut Context<Workspace>) {
        if let Some(key) = self.targets.iter().find_map(|(key, target)| {
            matches!(target, RowTarget::Agent { agent_id: id, .. } if *id == agent_id)
                .then(|| key.clone())
        }) {
            self.pending_cursor = Some(key);
        } else if let Some((host, offset)) = self
            .heading_agents
            .iter()
            .find(|(_, agents)| agents.contains(&agent_id))
            .map(|(topic, _)| *topic)
        {
            self.pending_doc_cursor = Some((host, offset));
        }
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

    /// Resolves each heading-line tag to the exact agent it names. Repeated
    /// tags are independent portals onto the same runtime tree.
    fn resolve_bindings(
        &self,
        registry: &AgentRegistry,
        documents: &[(HostId, String)],
    ) -> HashMap<(HostId, usize), Vec<AgentId>> {
        let mut by_heading: HashMap<(HostId, usize), Vec<AgentId>> = HashMap::new();
        for (host, text) in documents {
            for heading in parse(text) {
                for tag in &heading.tags {
                    let Some(agent_id) = registry.agent_by_tag(*host, tag) else {
                        continue;
                    };
                    by_heading
                        .entry((*host, heading.heading_range.start))
                        .or_default()
                        .push(agent_id);
                }
            }
        }
        for agents in by_heading.values_mut() {
            *agents = sorted_agents(registry, agents.iter().copied());
        }
        by_heading
    }

    #[cfg(feature = "native")]
    fn resolve_page_bindings(
        documents: &[(HostId, String)],
    ) -> (
        HashMap<(HostId, usize), rho_browser::PageId>,
        HashSet<rho_browser::PageId>,
    ) {
        let mut by_heading = HashMap::new();
        let mut referenced = HashSet::new();
        for (host, text) in documents {
            for heading in parse(text) {
                for tag in &heading.tags {
                    if let Ok(page_id) = tag.parse::<rho_browser::PageId>() {
                        by_heading.insert((*host, heading.heading_range.start), page_id);
                        referenced.insert(page_id);
                    }
                }
            }
        }
        (by_heading, referenced)
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
        if self.deal.is_some() {
            self.refresh_current_deal_offset(cx);
        }
        let documents = self
            .hosts
            .keys()
            .filter_map(|host| self.source_text(*host, cx).map(|text| (*host, text)))
            .collect::<Vec<_>>();
        let filed = self.resolve_bindings(registry, &documents);
        let documents_changed = self
            .last_synced
            .as_ref()
            .is_none_or(|(previous, ..)| previous != &documents);
        let deal_revision = registry.deal_count_revision();
        let now = chrono::Local::now();
        let minute = now.timestamp() / 60;
        if documents_changed
            || self.queue_depth_revision != Some(deal_revision)
            || self.queue_depth_minute != Some(minute)
        {
            let queue = assemble_deal_queue(
                &documents,
                &deal_agent_facts(registry),
                now.fixed_offset(),
                0,
            );
            self.queue_depth = DealQueueDepth {
                dealt_count: queue.dealt_count,
                total_alive: queue.total_alive,
            };
            self.queue_depth_revision = Some(deal_revision);
            self.queue_depth_minute = Some(minute);
        }
        #[cfg(feature = "native")]
        let (filed_pages, referenced_pages) = Self::resolve_page_bindings(&documents);
        self.archived_agents = archived_roots(&documents, &filed);

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
            && let Some((topic, _, _)) = self.new_draft.take()
        {
            self.buffers.remove(&LineKey::NewDraft(topic));
        }

        let draft_topic = self.new_draft.as_ref().map(|(topic, _, _)| *topic);
        let collapsed = self.collapsed_ranges(&documents, cx);
        let fold_ranges = if self.raw_mode || self.deal.is_some() {
            Vec::new()
        } else {
            self.effective_fold_ranges(&documents, &collapsed, cx)
        };
        let segments = generate(
            registry,
            &documents,
            &filed,
            &fold_ranges,
            ListingVisibility {
                collapsed_unfiled: &self.collapsed_unfiled,
                expanded_portals: &self.expanded_portals,
                raw: self.raw_mode,
            },
            &self.replies,
            draft_topic,
        );
        // Staffed headings wear their agents as end-of-line inlays, not
        // rows, so the decoration strings join the fingerprint: attention
        // changes must not vanish into the early-out.
        let mut decorations = if self.raw_mode {
            Vec::new()
        } else {
            heading_decorations(
                registry,
                &documents,
                &filed,
                &self.expanded_portals,
                &fold_ranges,
            )
        };
        #[cfg(feature = "native")]
        if !self.raw_mode && self.deal.is_none() {
            decorations.extend(page_heading_decorations(&documents, &filed_pages));
        }
        let cursor_doc = if self.deal.is_some() {
            None
        } else {
            match &cursor_place {
                Some(CursorPlace::Doc(host, offset)) => Some((*host, *offset)),
                _ => None,
            }
        };
        let conceals = if self.raw_mode {
            Vec::new()
        } else {
            heading_conceals(&documents, cursor_doc, self.deal.is_none())
        };

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
            && {
                #[cfg(feature = "native")]
                {
                    self.heading_pages == filed_pages && self.referenced_pages == referenced_pages
                }
                #[cfg(not(feature = "native"))]
                {
                    true
                }
            }
            && self
                .last_synced
                .as_ref()
                .is_some_and(|(docs, segs, drafts, decs, folds, hidden)| {
                    *docs == documents
                        && *segs == segments
                        && *drafts == draft_texts
                        && *decs == decorations
                        && *folds == fold_ranges
                        && *hidden == conceals
                })
        {
            // The world is unchanged, but the caret may have moved onto
            // a conceal through a path that skips the editor's motion
            // constraint (anchor resolution over a daemon edit).
            self.nudge_caret_off_folds(window, cx);
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
            let new_buffer = || {
                cx.new(|cx| {
                    let mut buffer = Buffer::local("", cx);
                    buffer.set_capability(Capability::Read, cx);
                    buffer
                })
            };
            let buffer = match &line.key {
                LineKey::Agent { agent_id, .. } => self
                    .agent_buffers
                    .entry(*agent_id)
                    .or_insert_with(new_buffer),
                _ => self
                    .buffers
                    .entry(line.key.clone())
                    .or_insert_with(new_buffer),
            };
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
        // Last doc-slice end and host length per section, so trailing
        // blank lines can be hidden behind a rowless cut below.
        let mut section_ends: Vec<(usize, usize)> = Vec::new();
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
                            let Some(buffer) = self.hosts.get(host).and_then(|weak| weak.upgrade())
                            else {
                                current = None;
                                continue;
                            };
                            let host_len = buffer.read(cx).len();
                            spec.sections.push(SectionSpec {
                                host: buffer,
                                start: range.start,
                                end: None,
                                lead: std::mem::take(&mut pending_rows),
                                cuts: Vec::new(),
                            });
                            section_ends.push((0, host_len));
                        }
                    }
                    current = Some((*host, range.end));
                    if let Some(end) = section_ends.last_mut() {
                        end.0 = range.end;
                    }
                }
                Segment::Line(line) => {
                    let Some(buffer) = self.buffer_for_key(&line.key).cloned() else {
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
        // A section's trailing blank lines hide behind a final rowless
        // cut, so the listing's spacers control the spacing after it.
        for (section, (last_end, host_len)) in spec.sections.iter_mut().zip(&section_ends) {
            if last_end < host_len {
                section.cuts.push(CutSpec {
                    id: u64::MAX,
                    position: *last_end,
                    resume: *host_len,
                    rows: Vec::new(),
                });
            }
        }

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
        // The raw cursor offset, captured while its anchors still
        // resolve — the fallback target if the reconcile orphans them.
        let cursor_offset_before = self.editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            editor
                .selections
                .newest::<editor::MultiBufferOffset>(&snapshot)
                .head()
        });

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
        #[cfg(feature = "native")]
        {
            self.heading_pages = filed_pages;
            self.referenced_pages = referenced_pages;
        }

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
                    if order.contains(key) && (edited.contains(key) || rebuilt(self, key)) =>
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
        } else if let Some((host, offset)) = pending_doc.or(if structure_changed {
            doc_cursor_before
        } else {
            None
        }) {
            self.move_cursor_to_doc(host, offset, window, cx);
        }

        // No dead anchors may survive the reconcile: a selection whose
        // excerpt vanished (a sent draft's row, say) panics inside vim's
        // next mode switch. Clamp any orphan to its old offset.
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let orphaned = editor
                .selections
                .disjoint_anchors()
                .iter()
                .any(|selection| {
                    !snapshot.can_resolve(&selection.start) || !snapshot.can_resolve(&selection.end)
                });
            if orphaned {
                let offset = cursor_offset_before.min(snapshot.len());
                editor.change_selections(Default::default(), window, cx, |selections| {
                    selections.select_ranges([offset..offset]);
                });
            }
        });

        self.apply_highlights(&segments, &documents, cx);
        self.apply_reply_chrome(registry, cx);
        self.apply_heading_chrome(&decorations, &fold_ranges, cx);
        self.apply_tag_conceals(&conceals, cx);
        self.apply_subtree_folds(&fold_ranges, cx);
        self.apply_sticky_headings(&documents, &segments, cx);
        self.apply_deal_highlight(&documents, &segments, cx);
        self.nudge_caret_off_folds(window, cx);
        self.last_synced = Some((
            documents,
            segments,
            draft_texts,
            decorations,
            fold_ranges,
            conceals,
        ));
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
        if !matches!(key, LineKey::Agent { .. }) {
            let Some(buffer) = self.buffers.get(key) else {
                return;
            };
            let anchor = buffer.read(cx).anchor_after(0);
            self.select_buffer_anchor(anchor, None, window, cx);
            return;
        }
        let Some(id) = self.element_keys.get(key) else {
            return;
        };
        let Some(path) = self.composition.path_for_row(*id) else {
            return;
        };
        let Some(anchor) = self.multi_buffer.read(cx).location_for_path(&path, cx) else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            editor.change_selections(Default::default(), window, cx, |selections| {
                selections.select_anchor_ranges([anchor..anchor]);
            });
        });
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
        let autoscroll = self.deal.as_ref().map(|_| {
            let headings = parse(&buffer.text());
            let ancestors = headings
                .iter()
                .find(|heading| heading.heading_range.start == offset)
                .map_or(0, |heading| {
                    let mut count = 0;
                    let mut parent = heading.parent;
                    while let Some(index) = parent {
                        count += 1;
                        parent = headings[index].parent;
                    }
                    count
                });
            Autoscroll::top_relative((ancestors + 2) as f64)
        });
        self.select_buffer_anchor(anchor, autoscroll, window, cx);
    }

    fn select_buffer_anchor(
        &self,
        anchor: text::Anchor,
        autoscroll: Option<Autoscroll>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let Some(anchor) = snapshot.anchor_in_excerpt(anchor) else {
            return;
        };
        self.editor.update(cx, |editor, cx| {
            let effects = autoscroll.map_or_else(Default::default, SelectionEffects::scroll);
            editor.change_selections(effects, window, cx, |selections| {
                selections.select_anchor_ranges([anchor..anchor]);
            });
        });
    }

    /// Where the cursor is: a generated row, or an offset in a document.
    fn cursor_place(&self, cx: &mut Context<Workspace>) -> Option<CursorPlace> {
        let (anchor, buffer_id, offset) = self.editor.update(cx, |editor, cx| {
            let anchor = editor.selections.newest_anchor().head();
            let head = editor
                .selections
                .newest::<Point>(&editor.display_snapshot(cx))
                .head();
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            snapshot
                .point_to_buffer_offset(head)
                .map(|(buffer, offset)| (anchor, buffer.remote_id(), offset.0))
        })?;
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        if let multi_buffer::Anchor::Excerpt(anchor) = anchor {
            let path = snapshot.path_for_anchor(anchor);
            if let Some(key) = self.order.iter().find(|key| {
                self.element_keys
                    .get(*key)
                    .and_then(|id| self.composition.path_for_row(*id))
                    .as_ref()
                    == Some(path)
            }) {
                return Some(CursorPlace::Row(key.clone()));
            }
        }
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
                let text = self.source_text(host, cx)?;
                let Some(heading) = parse(&text)
                    .into_iter()
                    .rev()
                    .find(|heading| heading.heading_range.start <= offset)
                else {
                    return Some(RowTarget::None);
                };
                let start = heading.heading_range.start;
                #[cfg(feature = "native")]
                if let Some(page) = self.heading_pages.get(&(host, start)) {
                    return Some(RowTarget::Page(*page));
                }
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
                    on_heading_line: offset <= heading.heading_range.end,
                })
            }
        }
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
            CursorPlace::Row(key) => match &key {
                LineKey::NewDraft(topic) => *topic,
                LineKey::Agent { .. } => match self.targets.get(&key) {
                    Some(RowTarget::Agent { topic, .. }) => *topic,
                    _ => None,
                },
                LineKey::Reply(agent_id) => self.targets.values().find_map(|target| match target {
                    RowTarget::Agent {
                        agent_id: candidate,
                        topic,
                    } if candidate == agent_id => *topic,
                    _ => None,
                }),
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

    #[cfg(feature = "native")]
    pub fn tag_cursor_heading_with_page(&mut self, tag: &str, cx: &mut Context<Workspace>) -> bool {
        let Some((host, offset)) = self.cursor_topic(cx) else {
            return false;
        };
        let Some(buffer) = self.hosts.get(&host).and_then(WeakEntity::upgrade) else {
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
        if heading.tags.iter().any(|candidate| candidate == tag) {
            return true;
        }
        buffer.update(cx, |buffer, cx| {
            buffer.edit(
                [(
                    heading.heading_range.end..heading.heading_range.end,
                    format!(" :{tag}:"),
                )],
                None,
                cx,
            )
        });
        self.pending_doc_cursor = Some((host, offset));
        true
    }

    /// Appends a `* …` heading for a quick spawn and returns its offset.
    /// The `…` title is a stateless promise: autofill_titles rewrites it
    /// with the bound agent's generated summary, and only while the title
    /// is still literally `…`, so a manual rename is never clobbered.
    pub fn append_placeholder_heading(
        &mut self,
        host: HostId,
        cx: &mut Context<Workspace>,
    ) -> Option<usize> {
        let buffer = self.hosts.get(&host)?.upgrade()?;
        let text = self.source_text(host, cx)?;
        let prefix = if text.is_empty() || text.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        let len = buffer.read(cx).len();
        let offset = len + prefix.len();
        buffer.update(cx, |buffer, cx| {
            buffer.edit(
                [(len..len, format!("{prefix}* {PLACEHOLDER_TITLE}\n"))],
                None,
                cx,
            )
        });
        Some(offset)
    }

    /// Gives every `…` heading its bound agent's generated title, once
    /// one exists. Idempotent and cheap: hosts without a pending
    /// placeholder are skipped on a substring check.
    pub fn autofill_titles(&mut self, registry: &AgentRegistry, cx: &mut Context<Workspace>) {
        let hosts = self.hosts.keys().copied().collect::<Vec<_>>();
        for host in hosts {
            let Some(text) = self.source_text(host, cx) else {
                continue;
            };
            if !text.contains(PLACEHOLDER_TITLE) {
                continue;
            }
            let documents = [(host, text.clone())];
            let filed = self.resolve_bindings(registry, &documents);
            let edits = parse(&text)
                .into_iter()
                .filter(|heading| heading.title == PLACEHOLDER_TITLE)
                .filter_map(|heading| {
                    let agents = filed.get(&(host, heading.heading_range.start))?;
                    let title = agents
                        .iter()
                        .find_map(|agent_id| registry.agent_display_name(*agent_id))
                        .map(str::trim)
                        .filter(|title| !title.is_empty())?;
                    Some((heading.title_range.clone(), title.to_owned()))
                })
                .collect::<Vec<_>>();
            if edits.is_empty() {
                continue;
            }
            if let Some(buffer) = self.hosts.get(&host).and_then(|weak| weak.upgrade()) {
                buffer.update(cx, |buffer, cx| buffer.edit(edits, None, cx));
            }
        }
    }

    /// Whether the cursor is somewhere dashboard verbs apply: a heading
    /// line of the document or a generated agent row.
    pub fn cursor_on_heading_line(&self, cx: &mut Context<Workspace>) -> bool {
        if matches!(
            self.cursor_place(cx),
            Some(CursorPlace::Row(LineKey::Agent { .. }))
        ) {
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

    /// Write or replace a dated property on the heading line under the
    /// cursor. The visible Desk text remains the only state.
    pub fn set_cursor_heading_property(
        &mut self,
        kind: TemporalMarkKind,
        at: chrono::NaiveDateTime,
        pace_days: Option<u32>,
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
        let line = property_line(kind, at, pace_days);
        let first_other_terminal = heading.properties.iter().position(|property| {
            TemporalMarkKind::from_property_key(&property.key).is_some_and(|property_kind| {
                property_kind != kind
                    && matches!(
                        property_kind,
                        TemporalMarkKind::Done | TemporalMarkKind::Discarded
                    )
                    && matches!(kind, TemporalMarkKind::Done | TemporalMarkKind::Discarded)
            })
        });
        let same = heading
            .properties
            .iter()
            .position(|property| property.key.eq_ignore_ascii_case(kind.property_key()));
        let replace = same.filter(|same| first_other_terminal.is_none_or(|other| same < &other));
        let (range, replacement) = if let Some(index) = replace {
            let property = &heading.properties[index];
            let end = property.line_range.end
                + usize::from(text.as_bytes().get(property.line_range.end) == Some(&b'\n'));
            (property.line_range.start..end, line)
        } else {
            let followed_by_newline =
                text.as_bytes().get(heading.heading_range.end) == Some(&b'\n');
            let insertion = heading.heading_range.end + usize::from(followed_by_newline);
            (
                insertion..insertion,
                if followed_by_newline {
                    line
                } else {
                    format!("\n{line}")
                },
            )
        };
        self.hosts[&host]
            .upgrade()
            .unwrap()
            .update(cx, |buffer, cx| {
                buffer.edit([(range, replacement)], None, cx)
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

    /// `>>`/`<<`: one star in or out on the heading under the cursor and
    /// every heading in its subtree, so children keep their relative
    /// depth. Reordering is plain vim editing — cut and paste the lines.
    pub fn structure_move(&mut self, direction: StructureDirection, cx: &mut Context<Workspace>) {
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
        let edits = structure_edits(&headings, index, direction);
        if edits.is_empty() {
            return;
        }
        self.hosts[&host]
            .upgrade()
            .unwrap()
            .update(cx, |buffer, cx| buffer.edit(edits, None, cx));
    }

    /// Archives the heading under the cursor: its subtree moves beneath a
    /// sibling `:archive:` heading (created when missing), which folds.
    /// Unarchiving is plain vim editing — cut the lines back out.
    pub fn archive_cursor_heading(&mut self, cx: &mut Context<Workspace>) -> bool {
        let Some((host, offset)) = self.cursor_topic(cx) else {
            return false;
        };
        self.archive_heading(host, offset, cx)
    }

    pub fn archive_deal_heading(&mut self, cx: &mut Context<Workspace>) -> bool {
        if !self.deal_accepts_verdict() || !self.refresh_current_deal_offset(cx) {
            return false;
        }
        let Some(card) = self.current_deal_card() else {
            return false;
        };
        let Some(offset) = card.heading_offset else {
            return false;
        };
        let archived = self.archive_heading(card.host, offset, cx);
        if archived && let Some(deal) = &mut self.deal {
            if let Some(card) = deal.cards.get_mut(deal.index) {
                card.heading_offset = None;
            }
            if let Some(anchor) = deal.anchors.get_mut(deal.index) {
                *anchor = None;
            }
            if let Some(anchor) = deal.boundary_anchors.get_mut(deal.index) {
                *anchor = None;
            }
            // `archive_heading` aimed the ordinary Desk cursor at the archive
            // zone, which is outside the still-current narrowed card.
            self.pending_doc_cursor = None;
        }
        archived
    }

    fn archive_heading(
        &mut self,
        host: HostId,
        offset: usize,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let Some(text) = self.source_text(host, cx) else {
            return false;
        };
        let archived_at = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
        let Some((edits, archive_offset)) = archive_edits(&text, offset, &archived_at) else {
            return false;
        };
        let Some(buffer) = self.hosts.get(&host).and_then(|weak| weak.upgrade()) else {
            return false;
        };
        buffer.update(cx, |buffer, cx| buffer.edit(edits, None, cx));
        if let Some(text) = self.source_text(host, cx) {
            // The archive's fold recomputes so the subtree that just
            // moved in folds with it — an existing anchored range would
            // leave the new content visible (rear-nonsticky).
            let mut ranges: Vec<Range<usize>> = self
                .collapsed_ranges(&[(host, text.clone())], cx)
                .into_iter()
                .filter(|(_, owner, _)| *owner != archive_offset)
                .map(|(_, _, range)| range)
                .collect();
            if let Some(archive) = parse(&text)
                .iter()
                .find(|heading| heading.heading_range.start == archive_offset)
            {
                ranges.extend(subtree_fold_range(&text, archive));
            }
            self.store_fold_ranges(host, &ranges, cx);
        }
        self.cursor_to_doc(host, archive_offset, cx);
        true
    }

    /// Whether the agent's root is filed inside an archive zone (muted:
    /// no chime, quiet decorations).
    pub fn agent_archived(&self, registry: &AgentRegistry, agent_id: AgentId) -> bool {
        let root = root_agent(registry, agent_id);
        self.archived_agents
            .iter()
            .any(|archived| root_agent(registry, *archived) == root)
    }

    pub fn rename_cursor_topic(&mut self, title: &str, cx: &mut Context<Workspace>) -> bool {
        let Some((host, offset)) = self.cursor_topic(cx) else {
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

    pub fn delete_empty(&mut self, _cx: &mut Context<Workspace>) -> bool {
        false
    }

    /// The stored fold ranges resolved against today's text: each pair
    /// of anchors becomes (heading start, fold range). Org's `:fragile`
    /// rule, translated: a range whose start anchor no longer sits on a
    /// heading line — the heading was deleted or broken — drops out, as
    /// does one that collapsed to nothing.
    fn collapsed_ranges(
        &self,
        documents: &[(HostId, String)],
        cx: &App,
    ) -> Vec<(HostId, usize, Range<usize>)> {
        let mut resolved = Vec::new();
        for (host, text) in documents {
            let Some(pairs) = self.collapsed.get(host) else {
                continue;
            };
            let Some(buffer) = self.hosts.get(host).and_then(|weak| weak.upgrade()) else {
                continue;
            };
            let snapshot = buffer.read(cx).text_snapshot();
            let headings = parse(text);
            for (start, end) in pairs {
                let range = start.to_offset(&snapshot)..end.to_offset(&snapshot);
                let Some(heading) = heading_line_at(&headings, range.start) else {
                    continue;
                };
                if range.end > range.start {
                    resolved.push((*host, heading, range));
                }
            }
        }
        resolved
    }

    /// The fold ranges to apply this pass: the stored ranges, clamped
    /// so no fold captures the cursor (org's catch-invisible-edits and
    /// isearch-open, in one rule).
    fn effective_fold_ranges(
        &self,
        documents: &[(HostId, String)],
        collapsed: &[(HostId, usize, Range<usize>)],
        cx: &mut Context<Workspace>,
    ) -> Vec<(HostId, Range<usize>)> {
        let cursor = match self.cursor_place(cx) {
            Some(CursorPlace::Doc(host, offset)) => Some((host, offset)),
            _ => None,
        };
        let mut ranges = Vec::new();
        for (host, _, range) in collapsed {
            let mut range = range.clone();
            if let Some((cursor_host, offset)) = cursor
                && cursor_host == *host
            {
                let Some(text) = documents
                    .iter()
                    .find(|(document_host, _)| document_host == host)
                    .map(|(_, text)| text.as_str())
                else {
                    continue;
                };
                match cursor_clamped_fold(text, range, offset) {
                    Some(clamped) => range = clamped,
                    None => continue,
                }
            }
            ranges.push((*host, range));
        }
        ranges
    }

    /// Replaces a host's fold set. A range that resolves identically
    /// today keeps its anchored pair — the fold's drift from the parsed
    /// structure is deliberate, org-style — and every other range is
    /// anchored fresh from the current text.
    fn store_fold_ranges(&mut self, host: HostId, ranges: &[Range<usize>], cx: &App) {
        let Some(buffer) = self.hosts.get(&host).and_then(|weak| weak.upgrade()) else {
            return;
        };
        let snapshot = buffer.read(cx).text_snapshot();
        let existing: Vec<(Range<usize>, (text::Anchor, text::Anchor))> = self
            .collapsed
            .get(&host)
            .into_iter()
            .flatten()
            .map(|(start, end)| {
                (
                    start.to_offset(&snapshot)..end.to_offset(&snapshot),
                    (*start, *end),
                )
            })
            .collect();
        let pairs = ranges
            .iter()
            .map(|range| {
                existing
                    .iter()
                    .find(|(resolved, _)| resolved == range)
                    .map(|(_, pair)| *pair)
                    .unwrap_or_else(|| {
                        (
                            snapshot.anchor_after(range.start),
                            snapshot.anchor_before(range.end),
                        )
                    })
            })
            .collect();
        self.collapsed.insert(host, pairs);
    }

    /// Org-style visibility cycling on the heading under the cursor.
    pub fn toggle_subagents(&mut self, cx: &mut Context<Workspace>) -> bool {
        let cursor_place = self.cursor_place(cx);
        if let Some(CursorPlace::Row(LineKey::Unfiled(host))) = cursor_place.clone() {
            if !self.collapsed_unfiled.remove(&host) {
                self.collapsed_unfiled.insert(host);
            }
            cx.notify();
            return true;
        }
        let Some((host, offset)) = self.cursor_topic(cx) else {
            return false;
        };
        let Some(text) = self.source_text(host, cx) else {
            return false;
        };
        let headings = parse(&text);
        if matches!(
            cursor_place,
            Some(CursorPlace::Doc(cursor_host, cursor_offset))
                if cursor_host == host
                    && headings.iter().any(|heading| {
                        heading.heading_range.start == offset
                            && cursor_offset > heading.heading_range.end
                    })
        ) {
            // A fold cannot contain its caret: display synchronization
            // deliberately lifts such folds. Move a body caret to its
            // owning heading before cycling so `Tab fold` works from the
            // whole topic rather than immediately reopening itself.
            self.cursor_to_doc(host, offset, cx);
        }
        let current: Vec<(usize, Range<usize>)> = self
            .collapsed_ranges(&[(host, text.clone())], cx)
            .into_iter()
            .map(|(_, owner, range)| (owner, range))
            .collect();
        let next = cycle_folds(&text, &headings, offset, &current);
        self.store_fold_ranges(host, &next, cx);
        cx.notify();
        true
    }

    /// `g t`: toggles the transient runtime tree for the staffed heading
    /// under the cursor, or closes the occurrence containing a portal row.
    pub fn toggle_agent_tree(&mut self, cx: &mut Context<Workspace>) -> bool {
        let occurrences = match self.cursor_place(cx) {
            Some(CursorPlace::Row(LineKey::Agent { occurrence, .. })) => vec![occurrence],
            Some(CursorPlace::Doc(host, offset)) => {
                let Some(text) = self.source_text(host, cx) else {
                    return false;
                };
                let headings = parse(&text);
                let Some((index, heading)) = headings.iter().enumerate().find(|(_, heading)| {
                    heading.heading_range.start <= offset && offset <= heading.heading_range.end
                }) else {
                    return false;
                };
                let Some(agents) = self
                    .heading_agents
                    .get(&(host, heading.heading_range.start))
                else {
                    return false;
                };
                let heading = heading_portal_ids(&headings)[index];
                agents
                    .iter()
                    .map(|portal| AgentOccurrence::Filed {
                        host,
                        heading,
                        portal: *portal,
                    })
                    .collect()
            }
            _ => return false,
        };
        if occurrences
            .iter()
            .all(|occurrence| self.expanded_portals.contains(occurrence))
        {
            for occurrence in &occurrences {
                self.expanded_portals.remove(occurrence);
            }
        } else {
            self.expanded_portals.extend(occurrences);
        }
        cx.notify();
        true
    }

    /// Switches between the composed Desk and its literal editable
    /// source. The mode is display-only; no source or fold state is changed.
    pub fn toggle_raw_mode(&mut self, cx: &mut Context<Workspace>) {
        self.raw_mode = !self.raw_mode;
        cx.notify();
    }

    /// Org's S-TAB: cycle the whole document through OVERVIEW (only
    /// top-level headings), CONTENTS (every heading line, no bodies),
    /// and SHOW ALL.
    pub fn cycle_global_folds(&mut self, cx: &mut Context<Workspace>) -> bool {
        let state = self.global_cycle;
        self.global_cycle = (state + 1) % 3;
        let hosts: Vec<HostId> = self.hosts.keys().copied().collect();
        for host in hosts {
            let Some(text) = self.source_text(host, cx) else {
                continue;
            };
            let headings = parse(&text);
            let top_depth = headings.iter().map(|heading| heading.depth).min();
            let ranges: Vec<Range<usize>> = match state {
                // OVERVIEW: fold every top-level subtree.
                0 => headings
                    .iter()
                    .filter(|heading| Some(heading.depth) == top_depth)
                    .filter_map(|heading| subtree_fold_range(&text, heading))
                    .collect(),
                // CONTENTS: every heading visible, every body hidden.
                1 => (0..headings.len())
                    .filter_map(|index| body_fold_range(&text, &headings, index))
                    .collect(),
                // SHOW ALL.
                _ => Vec::new(),
            };
            self.store_fold_ranges(host, &ranges, cx);
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
            .filter_map(|key| match self.targets.get(key) {
                Some(RowTarget::Agent { agent_id, .. })
                    if registry.attention(*agent_id) >= UiAttention::Pending =>
                {
                    Some((key.clone(), *agent_id))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        let next = current
            .and_then(|key| agents.iter().position(|(candidate, _)| *candidate == key))
            .map_or(0, |index| (index + 1) % agents.len().max(1));
        let (key, agent_id) = agents.get(next)?.clone();
        self.move_cursor_to(&key, window, cx);
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

    pub fn hint(&self, _cx: &mut Context<Workspace>) -> String {
        if let Some(deal) = &self.deal {
            return deal_hint(deal);
        }
        format!(
            "{} dealt · {} waiting · enter open · r reply · o staff · d/x verdict · Tab fold · gn attention",
            self.queue_depth.dealt_count, self.queue_depth.total_alive
        )
    }

    /// Supplies Desk's org hierarchy to Zed's existing sticky-scroll renderer.
    /// The source anchors survive ordinary edits; sync rebuilds the hierarchy
    /// when headings themselves are added, removed, or reparented.
    fn apply_sticky_headings(
        &self,
        documents: &[(HostId, String)],
        segments: &[Segment],
        cx: &mut Context<Workspace>,
    ) {
        if self.raw_mode {
            self.editor.update(cx, |editor, cx| {
                editor.set_custom_sticky_header_ranges(Some(Vec::new()), cx)
            });
            return;
        }
        let mut ranges = Vec::new();
        for (host, text) in documents {
            for heading in parse(text) {
                if let Some(range) = self.projected_source_range(
                    *host,
                    heading.heading_range.start..heading.subtree_range.end,
                    segments,
                    cx,
                ) {
                    ranges.push(range);
                }
            }
        }
        self.editor.update(cx, |editor, cx| {
            editor.set_custom_sticky_header_ranges(Some(ranges), cx)
        });
    }

    fn apply_deal_highlight(
        &self,
        documents: &[(HostId, String)],
        segments: &[Segment],
        cx: &mut Context<Workspace>,
    ) {
        let range = self.current_deal_card().and_then(|card| {
            let offset = card.heading_offset?;
            let (_, text) = documents.iter().find(|(host, _)| *host == card.host)?;
            let heading = parse(text)
                .into_iter()
                .find(|heading| heading.heading_range.start == offset)?;
            self.projected_source_range(
                card.host,
                heading.heading_range.start..heading.subtree_range.end,
                segments,
                cx,
            )
        });
        self.editor.update(cx, |editor, cx| {
            editor.clear_row_highlights::<DealCardHighlight>();
            if let Some(range) = range {
                editor.highlight_rows::<DealCardHighlight>(
                    range,
                    |cx| {
                        cx.theme()
                            .colors()
                            .editor_highlighted_line_background
                            .into()
                    },
                    RowHighlightOptions {
                        autoscroll: false,
                        include_gutter: true,
                    },
                    cx,
                );
            }
        });
    }

    /// Maps a source range into the composed Desk, clipping its end to the
    /// last projected slice when generated rows or another host trim the
    /// source tail from this section.
    fn projected_source_range(
        &self,
        host: HostId,
        source_range: Range<usize>,
        segments: &[Segment],
        cx: &mut Context<Workspace>,
    ) -> Option<Range<multi_buffer::Anchor>> {
        let projected_end = segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Doc {
                    host: segment_host,
                    range,
                    ..
                } if *segment_host == host => Some(range.end),
                _ => None,
            })
            .max()?;
        let buffer = self.hosts.get(&host)?.upgrade()?;
        let buffer = buffer.read(cx);
        let start = buffer.anchor_before(source_range.start);
        let end = buffer.anchor_after(source_range.end.min(projected_end));
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        Some(snapshot.anchor_in_excerpt(start)?..snapshot.anchor_in_excerpt(end)?)
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
            let Some(buffer) = self.buffer_for_key(&line.key) else {
                continue;
            };
            let buffer_snapshot = buffer.read(cx).snapshot();
            let Some(row_id) = self.element_keys.get(&line.key) else {
                continue;
            };
            let Some(path) = self.composition.path_for_row(*row_id) else {
                continue;
            };
            let Some(row_start) = self.multi_buffer.read(cx).location_for_path(&path, cx) else {
                continue;
            };
            let row_start = row_start.to_offset(&snapshot);
            for (class, range) in &line.spans {
                let clamp = |offset: usize| offset.min(buffer_snapshot.len());
                let start = snapshot.anchor_before(row_start + clamp(range.start));
                let end = snapshot.anchor_before(row_start + clamp(range.end));
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
        // The new-draft placeholder says where the spawn will land, so
        // the project is visible (and fixable, via `:project:` on the
        // heading) before anything is sent.
        let new_draft_hint = self.new_draft.as_ref().map(|(topic, _, _)| {
            let mut label = "first message for the new agent".to_owned();
            if let Some((host, offset)) = topic
                && let Some(text) = self.source_text(*host, cx)
                && let Some(project) = parse(&text)
                    .into_iter()
                    .find(|heading| heading.heading_range.start == *offset)
                    .and_then(|heading| heading.resolved_project)
            {
                label.push_str(&format!(" · {project}"));
            }
            label.push('…');
            (LineKey::NewDraft(*topic), label)
        });
        let drafts = self
            .replies
            .iter()
            .map(|agent_id| {
                (
                    LineKey::Reply(*agent_id),
                    format!("reply to {}…", registry.agent_id_label(*agent_id)),
                )
            })
            .chain(new_draft_hint);
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

    /// Splices the staffed headings' end-of-line decorations in as
    /// inlays: display-only, so the document text never carries agent
    /// markers and typing on the heading line slides them along.
    /// Replaces the star prefix with an org-modern fold indicator and
    /// hides `:eng-x7y2:` heading tags. Both stay in the buffer — copy
    /// and move carry structure and binding — but the display swaps
    /// them for placeholders. The placeholders are never zero-width:
    /// a widthless fold makes its two buffer sides display-identical,
    /// and selection round-trips through display coordinates would
    /// silently canonicalize one side to the other.
    /// Motions constrain the caret to fold rest positions; applied
    /// folds and anchor resolution do not. A conceal materializing
    /// under a caret that was legally resting there — the daemon retag
    /// inserts the tag at the very spot the caret occupies after typing
    /// a title — strands it on a forbidden edge, where it renders past
    /// the heading's decoration inlay. Nudge it through the editor's
    /// own constraint.
    fn nudge_caret_off_folds(&self, window: &mut Window, cx: &mut Context<Workspace>) {
        self.editor.update(cx, |editor, cx| {
            let snapshot = editor.display_snapshot(cx);
            let selection = editor
                .selections
                .newest::<multi_buffer::MultiBufferPoint>(&snapshot);
            if selection.start == selection.end
                && snapshot.caret_rest_adjustment(selection.head()).is_some()
            {
                editor.change_selections(Default::default(), window, cx, |_| {});
            }
        });
    }

    fn apply_tag_conceals(
        &self,
        conceals: &[(HostId, Range<usize>, String, editor::display_map::CaretRest)],
        cx: &mut Context<Workspace>,
    ) {
        struct DeskTagConceal;
        let type_id = std::any::TypeId::of::<DeskTagConceal>();
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let mut creases = Vec::new();
        for (host, range, placeholder_text, caret_rest) in conceals {
            let Some(buffer) = self.hosts.get(host).and_then(|weak| weak.upgrade()) else {
                continue;
            };
            let buffer_snapshot = buffer.read(cx).snapshot();
            let (Some(start), Some(end)) = (
                snapshot.anchor_in_excerpt(buffer_snapshot.anchor_after(range.start)),
                snapshot.anchor_in_excerpt(buffer_snapshot.anchor_before(range.end)),
            ) else {
                continue;
            };
            // The placeholder is layout text (collapsed_text) but pixels
            // come from the render element, so the glyphs must be drawn
            // here too. Depth is recoverable from the indicator's indent.
            // An all-space placeholder (the tag conceal) keeps its layout
            // column — zero-width folds corrupt selection roundtrips —
            // but draws nothing, so no visible gap invites the eye in.
            let glyph: Option<gpui::SharedString> =
                (!placeholder_text.trim().is_empty()).then(|| placeholder_text.clone().into());
            let class = DashClass::for_depth(
                placeholder_text
                    .chars()
                    .take_while(|character| *character == ' ')
                    .count()
                    + 1,
            );
            creases.push(editor::display_map::Crease::simple(
                start..end,
                editor::FoldPlaceholder {
                    render: std::sync::Arc::new(move |_, _, cx| {
                        use gpui::Styled as _;
                        use settings::Settings as _;
                        let Some(glyph) = glyph.clone() else {
                            return gpui::Empty.into_any_element();
                        };
                        let buffer_font = theme_settings::ThemeSettings::get_global(cx)
                            .buffer_font
                            .clone();
                        let color = class.style(cx).color.unwrap_or_default();
                        gpui::div()
                            .font(buffer_font)
                            .text_color(color)
                            .child(glyph)
                            .into_any_element()
                    }),
                    constrain_width: false,
                    merge_adjacent: false,
                    type_tag: Some(type_id),
                    collapsed_text: Some(placeholder_text.clone().into()),
                    caret_rest: *caret_rest,
                },
            ));
        }
        self.editor.update(cx, |editor, cx| {
            editor.display_map.update(cx, |display_map, cx| {
                display_map.replace_folds_with_type(type_id, creases, cx);
            });
        });
    }

    /// Collapsed subtrees hide behind display-level folds: the buffer
    /// keeps the text (copy, search, and the daemon all still see it)
    /// while the display shows the heading line plus a `…` placeholder.
    fn apply_subtree_folds(
        &self,
        fold_ranges: &[(HostId, Range<usize>)],
        cx: &mut Context<Workspace>,
    ) {
        struct DeskSubtreeFold;
        let type_id = std::any::TypeId::of::<DeskSubtreeFold>();
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let mut creases = Vec::new();
        for (host, range) in fold_ranges {
            let Some(buffer) = self.hosts.get(host).and_then(|weak| weak.upgrade()) else {
                continue;
            };
            let buffer_snapshot = buffer.read(cx).snapshot();
            let (Some(start), Some(end)) = (
                snapshot.anchor_in_excerpt(buffer_snapshot.anchor_after(range.start)),
                snapshot.anchor_in_excerpt(buffer_snapshot.anchor_before(range.end)),
            ) else {
                continue;
            };
            // The folded indicator is an end-of-line hint, not the
            // placeholder: the placeholder keeps its layout columns
            // (zero-width folds corrupt selection roundtrips) but
            // draws nothing the cursor could appear to sit on.
            creases.push(editor::display_map::Crease::simple(
                start..end,
                editor::FoldPlaceholder {
                    render: std::sync::Arc::new(|_, _, _| gpui::Empty.into_any_element()),
                    constrain_width: false,
                    merge_adjacent: false,
                    type_tag: Some(type_id),
                    collapsed_text: Some(" …".into()),
                    // Buffer-space motions (`e`, `w`, searches) can land
                    // inside the hidden subtree; vim keeps the fold
                    // closed and shows the cursor on the fold line.
                    caret_rest: editor::display_map::CaretRest::Boundary,
                },
            ));
        }
        self.editor.update(cx, |editor, cx| {
            editor.display_map.update(cx, |display_map, cx| {
                display_map.replace_folds_with_type(type_id, creases, cx);
            });
        });
    }

    /// Heading chrome — the agent chip and the folded chevron — paints
    /// as end-of-line hints: annotations outside text flow, with no
    /// display columns for carets, motions, or goal columns to land on.
    fn apply_heading_chrome(
        &mut self,
        decorations: &[(HostId, usize, String)],
        fold_ranges: &[(HostId, Range<usize>)],
        cx: &mut Context<Workspace>,
    ) {
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        // One hint per line: the chip text (if any agents are bound)
        // and the chevron (if a fold hangs off the line).
        struct HeadingChrome {
            label: String,
            web: bool,
            folded: bool,
        }
        let mut lines: Vec<((HostId, usize), HeadingChrome)> = Vec::new();
        for (host, offset, text) in decorations {
            let key = (*host, *offset);
            let web = text == WEB_HEADING_DECORATION;
            match lines.iter_mut().find(|(candidate, _)| *candidate == key) {
                Some((_, chrome)) => {
                    chrome.web |= web;
                    if !web {
                        chrome.label.push_str(text);
                    }
                }
                None => lines.push((
                    key,
                    HeadingChrome {
                        label: (if web { "" } else { text.trim_start() }).to_owned(),
                        web,
                        folded: false,
                    },
                )),
            }
        }
        for (host, range) in fold_ranges {
            match lines
                .iter_mut()
                .find(|(key, _)| *key == (*host, range.start))
            {
                Some((_, chrome)) => chrome.folded = true,
                None => lines.push((
                    (*host, range.start),
                    HeadingChrome {
                        label: String::new(),
                        web: false,
                        folded: true,
                    },
                )),
            }
        }
        let mut hints: Vec<(editor::Anchor, editor::EolHintRenderer)> = Vec::new();
        for ((host, offset), chrome) in lines {
            let Some(buffer) = self.hosts.get(&host).and_then(|weak| weak.upgrade()) else {
                continue;
            };
            let buffer_snapshot = buffer.read(cx).snapshot();
            let Some(position) = snapshot.anchor_in_excerpt(buffer_snapshot.anchor_before(offset))
            else {
                continue;
            };
            let text: gpui::SharedString = chrome.label.into();
            let web = chrome.web;
            let folded = chrome.folded;
            let renderer: editor::EolHintRenderer = std::sync::Arc::new(move |_, cx| {
                use gpui::Styled as _;
                use settings::Settings as _;
                use theme::ActiveTheme as _;
                let settings = theme_settings::ThemeSettings::get_global(cx);
                let font = settings.buffer_font.clone();
                let size = settings.buffer_font_size(cx);
                let color = cx.theme().colors().text_muted;
                let mut row = gpui::div()
                    .flex()
                    .items_center()
                    .gap(gpui::px(6.))
                    .font(font)
                    .text_size(size)
                    // Match the editor's line height so the hint's
                    // baseline lands on the line's baseline.
                    .line_height(gpui::relative(settings.line_height()))
                    .text_color(color);
                if !text.is_empty() {
                    row = row.child(text.clone());
                }
                if web {
                    row = row.child(
                        gpui::svg()
                            .path("icons/public.svg")
                            .w(size)
                            .h(size)
                            .text_color(color),
                    );
                }
                if folded {
                    row = row.child(
                        gpui::svg()
                            .path("icons/chevron_right.svg")
                            .w(size)
                            .h(size)
                            .text_color(color),
                    );
                }
                row.into_any_element()
            });
            hints.push((position, renderer));
        }
        self.editor.update(cx, |editor, cx| {
            editor.set_eol_hints(hints, cx);
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
    Heading2,
    Heading3,
    Heading4,
    TodoHeading,
    StaffedHeading,
    Working,
    Pending,
    NeedsInput,
}

impl DashClass {
    const ALL: [DashClass; 10] = [
        DashClass::Muted,
        DashClass::Heading,
        DashClass::Heading2,
        DashClass::Heading3,
        DashClass::Heading4,
        DashClass::TodoHeading,
        DashClass::StaffedHeading,
        DashClass::Working,
        DashClass::Pending,
        DashClass::NeedsInput,
    ];

    /// Org-style per-level heading colors, cycling every four levels.
    fn for_depth(depth: usize) -> DashClass {
        match depth.saturating_sub(1) % 4 {
            0 => DashClass::Heading,
            1 => DashClass::Heading2,
            2 => DashClass::Heading3,
            _ => DashClass::Heading4,
        }
    }

    fn key(self) -> HighlightKey {
        let slot = match self {
            DashClass::Muted => 0,
            DashClass::Heading => 1,
            DashClass::Heading2 => 2,
            DashClass::Heading3 => 3,
            DashClass::Heading4 => 4,
            DashClass::TodoHeading => 5,
            DashClass::StaffedHeading => 6,
            DashClass::Working => 7,
            DashClass::Pending => 8,
            DashClass::NeedsInput => 9,
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
            // Bright at the top: prominence tracks how shallow the
            // heading sits, so top-level topics pop and deep ones recede.
            DashClass::Heading => colors.terminal_ansi_bright_magenta,
            DashClass::Heading2 => colors.terminal_ansi_bright_green,
            DashClass::Heading3 => colors.terminal_ansi_magenta,
            DashClass::Heading4 => colors.terminal_ansi_green,
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
            None => DashClass::for_depth(heading.depth),
        };
        let title_class = if matches!(
            heading.state,
            Some(DeskHeadingState::Done | DeskHeadingState::Discarded)
        ) {
            DashClass::Muted
        } else {
            DashClass::for_depth(heading.depth)
        };
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
    agents.sort_by_cached_key(|agent_id| {
        (
            Reverse(registry.attention(*agent_id)),
            Reverse(registry.agent_last_active(*agent_id).unwrap_or_default()),
            *agent_id,
        )
    });
    agents.dedup();
    agents
}

fn attention_glyph(attention: UiAttention) -> &'static str {
    match attention {
        UiAttention::NeedsInput => "?",
        UiAttention::Pending => "✓",
        UiAttention::Working => "~",
        UiAttention::Quiet => "·",
    }
}

fn agent_line(
    agent_id: AgentId,
    occurrence: AgentOccurrence,
    topic: Option<(HostId, usize)>,
    registry: &AgentRegistry,
) -> Line {
    let attention = registry.attention(agent_id);
    let mut line = Line::new(
        LineKey::Agent {
            agent_id,
            occurrence,
        },
        RowTarget::Agent { agent_id, topic },
    );

    // A fixed one-character status column, indented by the agent's durable
    // spawn depth. The text is occurrence-independent, so every Desk portal
    // can project this same live buffer.
    // `?` needs you, `✓` finished and waiting, `~` working, `·` quiet.
    // The glyph alone carries the state; no color.
    let mut depth = 0usize;
    let mut cursor = agent_id;
    let mut seen = HashSet::new();
    while seen.insert(cursor) {
        let Some(parent) = registry.agent_parent(cursor) else {
            break;
        };
        depth += 1;
        cursor = parent;
    }
    line.span(None, |text| {
        text.push_str("  ");
        text.push_str(&"  ".repeat(depth));
    });
    line.span(Some(DashClass::Muted), |text| {
        text.push_str(attention_glyph(attention))
    });
    line.span(None, |text| text.push(' '));
    let name_class = registry.agent_hidden(agent_id).then_some(DashClass::Muted);
    line.span(name_class, |text| {
        text.push_str(&registry.agent_human_name(agent_id))
    });
    let label = registry.agent_id_label(agent_id);
    if !line.text.contains(&label) {
        line.span(None, |text| text.push_str("  "));
        line.span(Some(DashClass::Muted), |text| text.push_str(&label));
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

fn agent_tree_lines(
    registry: &AgentRegistry,
    roots: &[AgentId],
    occurrence: AgentOccurrence,
    topic: Option<(HostId, usize)>,
) -> Vec<Line> {
    let mut lines = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = roots.iter().rev().copied().collect::<Vec<_>>();
    while let Some(agent_id) = stack.pop() {
        if !seen.insert(agent_id) {
            continue;
        }
        lines.push(agent_line(agent_id, occurrence.clone(), topic, registry));
        stack.extend(registry.agent_children(agent_id).iter().rev().copied());
    }
    lines
}

/// The classic org-bullets stars, one per heading level, cycling like
/// the level colors do. Fold state lives in the chevron the collapsed
/// body's placeholder draws at the end of the heading line.
const HEADING_STARS: [&str; 4] = ["◉", "○", "✸", "✿"];

/// The heading-line conceals: the star token and its separating space
/// render as an org-modern bullet (indented one column per level, same
/// width as the text it replaces), and the tag with its separating
/// whitespace hides behind a single space (the inlay decoration shows
/// the pretty form). Caret rest keeps the caret on the title side of
/// both, so typing can never split the star token or fall behind the
/// binding.
///
/// A conceal never captures the caret: typing with the caret inside a
/// fold replaces the fold's contents, so a rebuilt conceal swallowing
/// the position mid-edit would make the next keystroke eat the tag.
/// The tag conceal starts no earlier than the caret, and a caret inside
/// either token reveals it outright, org-appear style.
fn heading_conceals(
    documents: &[(HostId, String)],
    cursor: Option<(HostId, usize)>,
    conceal_property_newline: bool,
) -> Vec<(HostId, Range<usize>, String, editor::display_map::CaretRest)> {
    use editor::display_map::CaretRest;
    let mut conceals = Vec::new();
    for (host, text) in documents {
        let caret = match cursor {
            Some((cursor_host, offset)) if cursor_host == *host => Some(offset),
            _ => None,
        };
        for heading in parse(text) {
            let mut stars_end = heading.stars_range.end;
            if text[stars_end..].starts_with(' ') {
                stars_end += 1;
            }
            let stars = heading.stars_range.start..stars_end;
            if !caret.is_some_and(|caret| stars.start < caret && caret < stars.end) {
                let star = HEADING_STARS[(heading.depth.saturating_sub(1)) % HEADING_STARS.len()];
                let mut bullet = " ".repeat(heading.depth.saturating_sub(1));
                bullet.push_str(star);
                bullet.push(' ');
                conceals.push((*host, stars, bullet, CaretRest::End));
            }
            let mut seen_temporal = std::collections::BTreeSet::new();
            for property in &heading.properties {
                let Some(kind) = TemporalMarkKind::from_property_key(&property.key) else {
                    continue;
                };
                if !seen_temporal.insert(kind)
                    || TemporalMark::parse(kind, &property.value).is_none()
                {
                    continue;
                }
                let start = property.line_range.start.saturating_sub(1);
                let range = if conceal_property_newline
                    && text.as_bytes().get(start) == Some(&b'\n')
                {
                    start..property.line_range.end
                } else {
                    let end = property.line_range.end
                        + usize::from(text.as_bytes().get(property.line_range.end) == Some(&b'\n'));
                    property.line_range.start..end
                };
                if !caret.is_some_and(|caret| range.start < caret && caret < range.end) {
                    conceals.push((*host, range, String::new(), CaretRest::Start));
                }
            }
            let Some(tags_range) = heading.tags_range else {
                continue;
            };
            let mut start = text[..tags_range.start]
                .trim_end_matches([' ', '\t'])
                .len()
                .max(heading.stars_range.end);
            if let Some(caret) = caret {
                if start < caret && caret <= tags_range.start {
                    start = caret;
                } else if tags_range.start < caret && caret < tags_range.end {
                    continue;
                }
            }
            conceals.push((
                *host,
                start..tags_range.end,
                " ".to_owned(),
                CaretRest::Start,
            ));
        }
    }
    conceals
}

/// What a heading wears at the end of its line: a chevron when
/// collapsed over a hidden body, then each bound agent as `glyph
/// eng-id` (plus `+n` when it has subagents), and the attention reason
/// inline when the agent is waiting on the human.
fn heading_decorations(
    registry: &AgentRegistry,
    documents: &[(HostId, String)],
    filed: &HashMap<(HostId, usize), Vec<AgentId>>,
    expanded_portals: &HashSet<AgentOccurrence>,
    fold_ranges: &[(HostId, Range<usize>)],
) -> Vec<(HostId, usize, String)> {
    let empty = Vec::new();
    let mut decorations = Vec::new();
    for (host, text) in documents {
        let headings = parse(text);
        let portal_ids = heading_portal_ids(&headings);
        let zones = archive_zones(&headings);
        let folds = fold_ranges
            .iter()
            .filter(|(fold_host, _)| fold_host == host)
            .map(|(_, range)| range)
            .collect::<Vec<_>>();
        for (index, heading) in headings.iter().enumerate() {
            let agents = filed
                .get(&(*host, heading.heading_range.start))
                .unwrap_or(&empty);
            let folded = folds.iter().any(|range| {
                range.start <= heading.heading_range.end && heading.heading_range.end <= range.end
            });
            if !folded
                && agents.iter().any(|portal| {
                    expanded_portals.contains(&AgentOccurrence::Filed {
                        host: *host,
                        heading: portal_ids[index],
                        portal: *portal,
                    })
                })
            {
                continue;
            }
            let archived = zones
                .iter()
                .any(|zone| zone.contains(&heading.heading_range.start));
            let mut label = String::new();
            if let Some(mark) = display_mark(heading, chrono::Local::now().naive_local()) {
                label.push_str("  ");
                label.push_str(&mark);
            }
            for agent_id in agents {
                // Archived agents read as quiet no matter what they want.
                let attention = if archived {
                    UiAttention::Quiet
                } else {
                    registry.attention(*agent_id)
                };
                label.push_str("  ");
                label.push_str(attention_glyph(attention));
                label.push(' ');
                label.push_str(&registry.agent_id_label(*agent_id));
                let members = registry.agent_subtree(*agent_id).len().saturating_sub(1);
                if members > 0 {
                    label.push_str(&format!(" +{members}"));
                }
                if attention >= UiAttention::Pending
                    && let Some(reason) = registry.agent_attention_reason(*agent_id)
                {
                    let reason = reason.lines().next().unwrap_or_default().trim();
                    if !reason.is_empty() {
                        label.push_str(" — ");
                        label.push_str(&truncate_chars(reason, 48));
                    }
                }
            }
            if !label.is_empty() {
                // Decorations anchor at the line end — the same offset
                // where body and subtree folds start, so the chip and
                // the folded chevron merge into a single hint. Hints
                // paint outside text flow, so no fold can swallow them.
                decorations.push((*host, heading.heading_range.end, label));
            }
        }
    }
    decorations
}

fn display_mark(heading: &DeskHeading, now: chrono::NaiveDateTime) -> Option<String> {
    let mark = match heading.state {
        Some(DeskHeadingState::Done) => heading
            .temporal_marks
            .iter()
            .find(|mark| mark.kind == TemporalMarkKind::Done),
        Some(DeskHeadingState::Discarded) => heading
            .temporal_marks
            .iter()
            .find(|mark| mark.kind == TemporalMarkKind::Discarded),
        _ => heading
            .temporal_marks
            .iter()
            .max_by(|left, right| priority(left, now).total_cmp(&priority(right, now))),
    }?;
    let elapsed = if mark.date_only {
        now.date().signed_duration_since(mark.at.date()).num_days() as f64
    } else {
        now.signed_duration_since(mark.at).num_seconds() as f64 / 86_400.0
    };
    let days = elapsed.abs().ceil() as u64;
    let date = mark.at.format("%b %-d").to_string().to_lowercase();
    Some(match mark.kind {
        TemporalMarkKind::Deadline if priority(mark, now) == f64::NEG_INFINITY => return None,
        TemporalMarkKind::Deadline if is_overdue_deadline(mark, now) => {
            format!("deadline {date} · late {days}d")
        }
        TemporalMarkKind::Deadline => format!("deadline {date} · {}d", mark.pace_days),
        TemporalMarkKind::Todo => format!("todo {date} · {}d", mark.pace_days),
        TemporalMarkKind::Defer => format!("defer {date} · {}d", mark.pace_days),
        TemporalMarkKind::Reminder => format!("reminder {date} · {}d", mark.pace_days),
        TemporalMarkKind::Skip => format!("skipped until {date}"),
        TemporalMarkKind::Done => format!("done {date}"),
        TemporalMarkKind::Discarded => format!("discarded {date}"),
    })
}

#[cfg(feature = "native")]
fn page_heading_decorations(
    documents: &[(HostId, String)],
    filed: &HashMap<(HostId, usize), rho_browser::PageId>,
) -> Vec<(HostId, usize, String)> {
    let mut decorations = Vec::new();
    for (host, text) in documents {
        for heading in parse(text) {
            if filed.contains_key(&(*host, heading.heading_range.start)) {
                decorations.push((
                    *host,
                    heading.heading_range.end,
                    WEB_HEADING_DECORATION.to_owned(),
                ));
            }
        }
    }
    decorations
}

/// The filed roots whose heading sits inside an `:archive:` zone.
fn archived_roots(
    documents: &[(HostId, String)],
    filed: &HashMap<(HostId, usize), Vec<AgentId>>,
) -> HashSet<AgentId> {
    let mut archived = HashSet::new();
    for (host, text) in documents {
        let zones = archive_zones(&parse(text));
        if zones.is_empty() {
            continue;
        }
        for ((filed_host, offset), agents) in filed {
            if filed_host == host && zones.iter().any(|zone| zone.contains(offset)) {
                archived.extend(agents.iter().copied());
            }
        }
    }
    archived
}

/// The `:archive:` subtree ranges of a document.
fn archive_zones(headings: &[DeskHeading]) -> Vec<Range<usize>> {
    headings
        .iter()
        .filter(|heading| heading.tags.iter().any(|tag| tag == ARCHIVE_TAG))
        .map(|heading| heading.subtree_range.clone())
        .collect()
}

/// The edits demoting or promoting the heading at `index` together with
/// every heading in its subtree, one star each. Empty when promoting a
/// top-level heading.
fn structure_edits(
    headings: &[DeskHeading],
    index: usize,
    direction: StructureDirection,
) -> Vec<(Range<usize>, String)> {
    if matches!(direction, StructureDirection::Promote) && headings[index].depth <= 1 {
        return Vec::new();
    }
    let subtree_end = headings[index].subtree_range.end;
    headings[index..]
        .iter()
        .take_while(|heading| heading.heading_range.start < subtree_end)
        .map(|heading| {
            let stars = heading.stars_range.start;
            match direction {
                StructureDirection::Demote => (stars..stars, "*".to_owned()),
                StructureDirection::Promote => (stars..stars + 1, String::new()),
            }
        })
        .collect()
}

/// The heading whose heading line contains `offset`, as its start
/// offset. Anywhere else — body, blank line, out of range — is `None`.
fn heading_line_at(headings: &[DeskHeading], offset: usize) -> Option<usize> {
    headings
        .iter()
        .find(|heading| {
            heading.heading_range.start <= offset && offset <= heading.heading_range.end
        })
        .map(|heading| heading.heading_range.start)
}

/// What a collapsed heading folds away: everything after its heading
/// line to the end of its subtree, keeping the final newline so the
/// next visible line stays a line of its own. `None` when there is
/// nothing to hide. Trailing blank lines the boundary rule assigned to
/// the enclosing context sit outside `subtree_range`, so they stay
/// visible as the gap between a folded subtree and a shallower heading.
fn subtree_fold_range(text: &str, heading: &DeskHeading) -> Option<Range<usize>> {
    let mut end = heading.subtree_range.end;
    if text[..end].ends_with('\n') {
        end -= 1;
    }
    (end > heading.heading_range.end).then_some(heading.heading_range.end..end)
}

/// A fold may never capture the cursor: whatever the parse says, the
/// line the user is on stays visible. Outside the range the fold is
/// untouched. On the fold's last line — where `o` below a folded
/// heading puts you, and where the recomputed subtree would otherwise
/// swallow what you type — the fold shortens to end above that line.
/// Deeper inside, the fold lifts entirely (vim opens folds on jumps
/// into them); it reapplies once the cursor leaves.
fn cursor_clamped_fold(text: &str, range: Range<usize>, cursor: usize) -> Option<Range<usize>> {
    // Both boundaries stay foldable: the end anchor is left-biased, so
    // typing at the end boundary lands outside the fold, and motions
    // resting there (word motions stop on the fold's last character)
    // must not open it.
    if cursor <= range.start || cursor >= range.end {
        return Some(range);
    }
    let line_start = text[..cursor].rfind('\n').map_or(0, |at| at + 1);
    let line_end = text[cursor..]
        .find('\n')
        .map_or(text.len(), |at| cursor + at);
    if line_end >= range.end && line_start > range.start + 1 {
        Some(range.start..line_start - 1)
    } else {
        None
    }
}

/// Indices of the heading's direct children: the shallowest descendants.
/// With sloppy nesting (`*` straight to `***`) that can be deeper than
/// depth + 1.
fn direct_children(headings: &[DeskHeading], index: usize) -> Vec<usize> {
    let descendants: Vec<usize> = headings[index + 1..]
        .iter()
        .enumerate()
        .take_while(|(_, heading)| heading.depth > headings[index].depth)
        .map(|(at, _)| index + 1 + at)
        .collect();
    let child_depth = descendants.iter().map(|&child| headings[child].depth).min();
    descendants
        .into_iter()
        .filter(|&child| Some(headings[child].depth) == child_depth)
        .collect()
}

/// What org's CHILDREN state hides of the heading's own text: the body
/// between the heading line and its first child heading. For a heading
/// without children this is the whole subtree fold. `None` when there
/// is no body to hide.
fn body_fold_range(text: &str, headings: &[DeskHeading], index: usize) -> Option<Range<usize>> {
    let heading = &headings[index];
    let children = direct_children(headings, index);
    let Some(&first_child) = children.first() else {
        return subtree_fold_range(text, heading);
    };
    let mut end = headings[first_child].heading_range.start;
    if text[..end].ends_with('\n') {
        end -= 1;
    }
    (end > heading.heading_range.end).then_some(heading.heading_range.end..end)
}

/// One step of org's TAB cycle for the heading at `offset`: FOLDED →
/// CHILDREN (the body and every child's subtree hidden, so only the
/// child heading lines show) → SUBTREE (everything visible) → FOLDED.
/// Headings without children toggle. `current` is the host's resolved
/// fold set as (owning heading start, range); the returned ranges
/// replace it, leaving folds outside this subtree untouched.
fn cycle_folds(
    text: &str,
    headings: &[DeskHeading],
    offset: usize,
    current: &[(usize, Range<usize>)],
) -> Vec<Range<usize>> {
    let Some(index) = headings
        .iter()
        .position(|heading| heading.heading_range.start == offset)
    else {
        return current.iter().map(|(_, range)| range.clone()).collect();
    };
    let heading = &headings[index];
    let subtree = heading.subtree_range.clone();
    let children = direct_children(headings, index);
    let first_child_start = children
        .first()
        .map(|&child| headings[child].heading_range.start);

    let mut next: Vec<Range<usize>> = current
        .iter()
        .filter(|(owner, _)| !subtree.contains(owner))
        .map(|(_, range)| range.clone())
        .collect();
    let owned: Vec<&Range<usize>> = current
        .iter()
        .filter(|(owner, _)| *owner == offset)
        .map(|(_, range)| range)
        .collect();

    // Folded means the heading's own fold reaches past its first child
    // (or exists at all, for a childless heading); a shorter fold is the
    // CHILDREN state's body fold.
    let folded = owned.iter().any(|range| match first_child_start {
        Some(child_start) => range.end > child_start,
        None => true,
    });
    let children_folded = !owned.is_empty()
        || children.iter().any(|&child| {
            current
                .iter()
                .any(|(owner, _)| *owner == headings[child].heading_range.start)
        });

    if folded {
        // → CHILDREN; childless headings skip straight to expanded.
        if first_child_start.is_some() {
            next.extend(body_fold_range(text, headings, index));
            for &child in &children {
                next.extend(subtree_fold_range(text, &headings[child]));
            }
        }
    } else if children_folded {
        // → SUBTREE: every fold inside is already dropped.
    } else {
        next.extend(subtree_fold_range(text, heading));
    }
    next
}

/// The edits that archive the heading at `target_start`: its whole subtree
/// moves under a sibling `:archive:` heading (created at the end of the
/// parent's subtree when missing) and demotes one level so it nests
/// inside. The moved heading gains an `:archived: <when>` property line
/// recording the archive time. Returns the edits and the archive
/// heading's offset once they apply. `None` when the target is already
/// archived, is itself an archive, or does not exist.
fn archive_edits(text: &str, target_start: usize, archived_at: &str) -> Option<ArchiveEdits> {
    let headings = parse(text);
    let target_index = headings
        .iter()
        .position(|heading| heading.heading_range.start == target_start)?;
    let target = &headings[target_index];
    if archive_zones(&headings)
        .iter()
        .any(|zone| zone.contains(&target_start))
    {
        return None;
    }
    let removal = target.subtree_range.clone();
    let removal_len = removal.len();
    let mut moved = String::new();
    for line in text[removal.clone()].split_inclusive('\n') {
        let stars = line.bytes().take_while(|byte| *byte == b'*').count();
        if stars > 0 && line.as_bytes().get(stars) == Some(&b' ') {
            moved.push('*');
        }
        moved.push_str(line);
    }
    if !moved.ends_with('\n') {
        moved.push('\n');
    }
    if let Some(line_end) = moved.find('\n') {
        moved.insert_str(line_end + 1, &format!(":archived: {archived_at}\n"));
    }
    let sibling_archive = headings.iter().find(|heading| {
        heading.parent == target.parent
            && heading.heading_range.start != target_start
            && heading.tags.iter().any(|tag| tag == ARCHIVE_TAG)
    });
    let mut edits = vec![(removal.clone(), String::new())];
    let archive_offset = match sibling_archive {
        Some(archive) => {
            let mut insertion = moved;
            let at = archive.subtree_range.end;
            if at == text.len() && !text.ends_with('\n') {
                insertion.insert(0, '\n');
            }
            edits.push((at..at, insertion));
            let start = archive.heading_range.start;
            if removal.start < start {
                start - removal_len
            } else {
                start
            }
        }
        None => {
            let at = target
                .parent
                .map_or(text.len(), |parent| headings[parent].subtree_range.end);
            let fresh_line = usize::from(at == text.len() && !text.ends_with('\n'));
            let mut insertion = String::new();
            if fresh_line == 1 {
                insertion.push('\n');
            }
            insertion.push_str(&format!(
                "{} Archive :{ARCHIVE_TAG}:\n",
                "*".repeat(target.depth)
            ));
            insertion.push_str(&moved);
            edits.push((at..at, insertion));
            // The insertion point always trails the removed subtree, which
            // lives inside the same parent subtree (or the document).
            at - removal_len + fresh_line
        }
    };
    edits.sort_by_key(|(range, _)| range.start);
    Some((edits, archive_offset))
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

fn heading_property_edits(
    text: &str,
    heading: &DeskHeading,
    kind: TemporalMarkKind,
    line: String,
) -> Vec<(Range<usize>, String)> {
    let key = kind.property_key();
    let existing = heading
        .properties
        .iter()
        .find(|property| property.key.eq_ignore_ascii_case(key));
    let mut edits = vec![if let Some(property) = existing {
        (
            property.line_range.clone(),
            line.trim_end_matches('\n').to_owned(),
        )
    } else {
        let followed_by_newline = text.as_bytes().get(heading.heading_range.end) == Some(&b'\n');
        let insertion = heading.heading_range.end + usize::from(followed_by_newline);
        (
            insertion..insertion,
            if followed_by_newline {
                line
            } else {
                format!("\n{line}")
            },
        )
    }];
    if kind == TemporalMarkKind::Defer {
        edits.extend(heading.properties.iter().filter_map(|property| {
            let verdict_family = ["defer", "reminder", "skip"]
                .iter()
                .any(|key| property.key.eq_ignore_ascii_case(key));
            if !verdict_family
                || existing.is_some_and(|target| target.line_range == property.line_range)
            {
                return None;
            }
            let end = property.line_range.end
                + usize::from(text.as_bytes().get(property.line_range.end) == Some(&b'\n'));
            Some((property.line_range.start..end, String::new()))
        }));
    }
    edits
}

fn heading_portal_ids(headings: &[DeskHeading]) -> Vec<u64> {
    use std::hash::{Hash as _, Hasher as _};

    let mut title_counts: HashMap<&str, u32> = HashMap::new();
    headings
        .iter()
        .map(|heading| {
            let count = title_counts.entry(&heading.title).or_insert(0);
            let occurrence = *count;
            *count += 1;
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            heading.title.hash(&mut hasher);
            occurrence.hash(&mut hasher);
            hasher.finish()
        })
        .collect()
}

fn heading_breadcrumb(headings: &[DeskHeading], index: usize) -> String {
    let mut path = vec![headings[index].title.as_str()];
    let mut parent = headings[index].parent;
    while let Some(parent_index) = parent {
        path.push(headings[parent_index].title.as_str());
        parent = headings[parent_index].parent;
    }
    path.reverse();
    path.join(" › ")
}

#[derive(Clone, Debug)]
pub struct DealAgentFacts {
    pub agent_id: AgentId,
    pub parent: Option<AgentId>,
    pub host: HostId,
    pub role_prefix: &'static str,
    pub encoded_id: String,
    pub tag: String,
    pub heading: String,
    pub facts: rho_ui_proto::UiAgentFacts,
}

fn deal_agent_facts(registry: &AgentRegistry) -> Vec<DealAgentFacts> {
    registry
        .known_agents()
        .filter_map(|agent_id| {
            let host = registry.host_of_agent(*agent_id)?;
            let tag = registry
                .agent_id_label(*agent_id)
                .rsplit('/')
                .next()?
                .to_owned();
            Some(DealAgentFacts {
                agent_id: *agent_id,
                parent: registry.agent_parent(*agent_id),
                host,
                role_prefix: registry.agent_role(*agent_id)?.handle_prefix(),
                encoded_id: agent_id.encoded(),
                tag,
                heading: registry
                    .agent_human_name(*agent_id)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
                facts: registry.agent_facts(*agent_id),
            })
        })
        .collect()
}

fn reply_wait_days(ended: rho_core::UnixMs, now: chrono::DateTime<chrono::FixedOffset>) -> f64 {
    (now.timestamp_millis() - ended.0 as i64) as f64 / 86_400_000.0
}

fn blocked_reply_priority(wait_days: f64) -> f64 {
    BLOCKED_REPLY_HEAD_START + BLOCKED_REPLY_SLOPE_PER_DAY * wait_days
}

fn fyi_reply_priority(wait_days: f64) -> f64 {
    -wait_days / FYI_REPLY_PACE_DAYS
}

fn mark_elapsed_days(mark: &TemporalMark, now: chrono::NaiveDateTime) -> f64 {
    if mark.date_only {
        now.date().signed_duration_since(mark.at.date()).num_days() as f64
    } else {
        now.signed_duration_since(mark.at).num_seconds() as f64 / 86_400.0
    }
}

fn age_label(days: f64) -> String {
    if days < 1.0 {
        format!("{}h", (days.max(0.0) * 24.0).floor() as u64)
    } else {
        format!("{}d", days.floor() as u64)
    }
}

fn temporal_label(mark: &TemporalMark, now: chrono::NaiveDateTime, mark_priority: f64) -> String {
    let elapsed = mark_elapsed_days(mark, now);
    match mark.kind {
        TemporalMarkKind::Deadline if elapsed > 0.0 => {
            format!("deadline · {}d late", elapsed.floor() as u64)
        }
        TemporalMarkKind::Deadline => {
            format!("deadline · {}d", (-elapsed).ceil() as u64)
        }
        TemporalMarkKind::Todo if mark_priority >= 0.0 => {
            format!("todo · ripe {}d", mark_priority.floor() as u64)
        }
        TemporalMarkKind::Todo => "todo".to_owned(),
        TemporalMarkKind::Reminder => "reminder".to_owned(),
        TemporalMarkKind::Defer => "defer".to_owned(),
        TemporalMarkKind::Skip | TemporalMarkKind::Done | TemporalMarkKind::Discarded => {
            unreachable!("non-live marks cannot win")
        }
    }
}

#[derive(Clone)]
struct DealHeading {
    host: HostId,
    order: usize,
    heading: DeskHeading,
    breadcrumb: String,
    agents: Vec<AgentId>,
    gated: bool,
}

#[derive(Clone)]
struct RankedDealCard {
    priority: f64,
    virtual_reply: bool,
    order: usize,
    card: DealCard,
}

/// Builds one deterministic deal from Desk text and per-agent facts. Every
/// live textual and virtual mark competes on one days-scale priority.
pub fn assemble_deal_queue(
    documents: &[(HostId, String)],
    agent_facts: &[DealAgentFacts],
    now: chrono::DateTime<chrono::FixedOffset>,
    seed: u64,
) -> DealQueue {
    use rho_ui_proto::desk::temporal::priority;

    let desk_now = now.naive_local();
    let by_id = agent_facts
        .iter()
        .map(|agent| (agent.agent_id, agent))
        .collect::<HashMap<_, _>>();
    let resolve_tag = |host: HostId, tag: &str| {
        let (role, prefix) = tag.split_once('-')?;
        let mut matches = agent_facts.iter().filter(|agent| {
            agent.host == host && agent.role_prefix == role && agent.encoded_id.starts_with(prefix)
        });
        let first = matches.next()?;
        matches.next().is_none().then_some(first.agent_id)
    };
    let descendants = |root: AgentId| {
        let mut result = vec![root];
        let mut cursor = 0;
        while cursor < result.len() {
            let parent = result[cursor];
            result.extend(
                agent_facts
                    .iter()
                    .filter(|agent| agent.parent == Some(parent))
                    .map(|agent| agent.agent_id),
            );
            cursor += 1;
        }
        result
    };

    let mut heading_rows = Vec::new();
    let mut agent_topics: HashMap<AgentId, (HostId, usize)> = HashMap::new();
    let mut order = 0;
    for (host, text) in documents {
        let headings = parse(text);
        let archives = archive_zones(&headings);
        for (index, heading) in headings.iter().enumerate() {
            let archived = archives.iter().any(|zone| {
                zone.start <= heading.heading_range.start && heading.heading_range.start < zone.end
            });
            let terminal = matches!(
                heading.state,
                Some(DeskHeadingState::Done | DeskHeadingState::Discarded)
            ) || heading.temporal_marks.iter().any(|mark| {
                matches!(
                    mark.kind,
                    TemporalMarkKind::Done | TemporalMarkKind::Discarded
                )
            });
            let deferred = heading.temporal_marks.iter().any(|mark| {
                matches!(
                    mark.kind,
                    TemporalMarkKind::Defer | TemporalMarkKind::Reminder | TemporalMarkKind::Skip
                ) && mark.at > desk_now
            });
            let gated = archived || terminal || deferred;
            let agents = heading
                .tags
                .iter()
                .filter_map(|tag| resolve_tag(*host, tag))
                .collect::<Vec<_>>();
            for agent in &agents {
                for member in descendants(*agent) {
                    let candidate = (*host, heading.heading_range.start);
                    agent_topics
                        .entry(member)
                        .and_modify(|current| {
                            let current_gated = heading_rows.iter().any(|row: &DealHeading| {
                                row.host == current.0
                                    && row.heading.heading_range.start == current.1
                                    && row.gated
                            });
                            if current_gated && !gated {
                                *current = candidate;
                            }
                        })
                        .or_insert(candidate);
                }
            }
            heading_rows.push(DealHeading {
                host: *host,
                order,
                heading: heading.clone(),
                breadcrumb: heading_breadcrumb(&headings, index),
                agents,
                gated,
            });
            order += 1;
        }
    }

    let rows_by_topic = heading_rows
        .iter()
        .map(|row| ((row.host, row.heading.heading_range.start), row))
        .collect::<HashMap<_, _>>();
    let mut ranked: HashMap<(HostId, usize), RankedDealCard> = HashMap::new();
    let mut offer = |topic: (HostId, usize), candidate: RankedDealCard| {
        ranked
            .entry(topic)
            .and_modify(|current| {
                if candidate.priority > current.priority
                    || (candidate.priority == current.priority
                        && candidate.virtual_reply
                        && !current.virtual_reply)
                {
                    *current = candidate.clone();
                }
            })
            .or_insert(candidate);
    };

    for row in heading_rows.iter().filter(|row| !row.gated) {
        let topic = (row.host, row.heading.heading_range.start);
        for mark in &row.heading.temporal_marks {
            let mark_priority = priority(mark, desk_now);
            if mark_priority <= DEAL_QUEUE_FLOOR {
                continue;
            }
            let agent_id = row.agents.first().copied();
            offer(
                topic,
                RankedDealCard {
                    priority: mark_priority,
                    virtual_reply: false,
                    order: row.order,
                    card: DealCard {
                        label: temporal_label(mark, desk_now, mark_priority),
                        priority: mark_priority,
                        host: row.host,
                        heading_offset: Some(row.heading.heading_range.start),
                        agent_id,
                        agent_tag: agent_id
                            .and_then(|agent| by_id.get(&agent))
                            .map(|agent| agent.tag.clone()),
                        breadcrumb: row.breadcrumb.clone(),
                    },
                },
            );
        }
    }

    for agent in agent_facts {
        let Some(ended) = agent.facts.last_turn_ended else {
            continue;
        };
        if agent.facts.turn_running || ended <= agent.facts.last_user_message_at {
            continue;
        }
        let Some(topic) = agent_topics.get(&agent.agent_id).copied() else {
            continue;
        };
        let Some(row) = rows_by_topic.get(&topic).copied().filter(|row| !row.gated) else {
            continue;
        };
        let wait_days = reply_wait_days(ended, now);
        let (reply_priority, label) = if agent.facts.needs_you_hint {
            (
                blocked_reply_priority(wait_days),
                format!("blocked · {}", age_label(wait_days)),
            )
        } else {
            (fyi_reply_priority(wait_days), "fyi".to_owned())
        };
        if reply_priority <= DEAL_QUEUE_FLOOR {
            continue;
        }
        offer(
            topic,
            RankedDealCard {
                priority: reply_priority,
                virtual_reply: true,
                order: row.order,
                card: DealCard {
                    label,
                    priority: reply_priority,
                    host: topic.0,
                    heading_offset: Some(topic.1),
                    agent_id: Some(agent.agent_id),
                    agent_tag: Some(agent.tag.clone()),
                    breadcrumb: row.breadcrumb.clone(),
                },
            },
        );
    }

    let priority_topics = ranked.keys().copied().collect::<HashSet<_>>();
    let total_alive = ranked.len();
    let mut ranked = ranked.into_values().collect::<Vec<_>>();
    ranked.sort_by(|a, b| {
        b.priority
            .total_cmp(&a.priority)
            .then_with(|| b.virtual_reply.cmp(&a.virtual_reply))
            .then_with(|| a.order.cmp(&b.order))
    });
    let mut cards = ranked
        .into_iter()
        .take(DEAL_PRIORITY_CUTOFF)
        .map(|ranked| ranked.card)
        .collect::<Vec<_>>();
    let dealt_count = cards.len();
    let dealt_topics = cards
        .iter()
        .filter_map(|card| card.heading_offset.map(|offset| (card.host, offset)))
        .collect::<HashSet<_>>();

    let random_topics = heading_rows
        .iter()
        .filter(|row| !row.gated)
        .map(|row| (row.host, row.heading.heading_range.start))
        .collect::<HashSet<_>>();
    let mut random_pool = heading_rows
        .iter()
        .filter(|row| {
            !row.gated && !dealt_topics.contains(&(row.host, row.heading.heading_range.start))
        })
        .collect::<Vec<_>>();
    if !random_pool.is_empty() {
        let mut mixed = seed.wrapping_add(0x9e3779b97f4a7c15);
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d049bb133111eb);
        mixed ^= mixed >> 31;
        let row = random_pool.swap_remove(mixed as usize % random_pool.len());
        let agent_id = row.agents.first().copied();
        cards.push(DealCard {
            label: "random".to_owned(),
            priority: f64::NEG_INFINITY,
            host: row.host,
            heading_offset: Some(row.heading.heading_range.start),
            agent_id,
            agent_tag: agent_id
                .and_then(|agent| by_id.get(&agent))
                .map(|agent| agent.tag.clone()),
            breadcrumb: row.breadcrumb.clone(),
        });
    }

    DealQueue {
        cards,
        total_alive,
        dealt_count,
        priority_topics,
        random_topics,
    }
}

fn deal_hint(deal: &DealSession) -> String {
    let Some(card) = deal.cards.get(deal.index) else {
        return format!(
            "✓ Desk dealt — {} verdict{} · q quit",
            deal.verdicts,
            if deal.verdicts == 1 { "" } else { "s" }
        );
    };
    format!(
        "{} · {}/{} · {DEAL_HINT}",
        card.label,
        deal.index + 1,
        deal.cards.len()
    )
}

/// Ends a document slice before a cut point's newline, so the synthetic
/// newline between excerpts doesn't double it.
/// Generate the listing without mutating Desk text: the documents are
/// emitted as writable slices, cut where a bound heading's rows (reply
/// drafts, and runtime portals splice in after its heading line. Every tag
/// projects the exact named agent; expanded portal occurrences also project
/// its complete spawn tree.
fn generate(
    registry: &AgentRegistry,
    documents: &[(HostId, String)],
    filed: &HashMap<(HostId, usize), Vec<AgentId>>,
    fold_ranges: &[(HostId, Range<usize>)],
    visibility: ListingVisibility<'_>,
    replies: &[AgentId],
    draft_topic: Option<Option<(HostId, usize)>>,
) -> Vec<Segment> {
    let mut segments = Vec::new();
    let multiple_hosts = documents.len() > 1;
    let mut emitted_replies = HashSet::new();
    let empty = Vec::new();
    let ListingVisibility {
        collapsed_unfiled,
        expanded_portals,
        raw,
    } = visibility;
    if raw {
        return documents
            .iter()
            .map(|(host, text)| Segment::Doc {
                host: *host,
                range: 0..text.len(),
                id: 0,
            })
            .collect();
    }

    let push_agent_portals = |segments: &mut Vec<Segment>,
                              emitted_replies: &mut HashSet<AgentId>,
                              roots: &[AgentId],
                              occurrence_for: &dyn Fn(AgentId) -> AgentOccurrence,
                              topic: Option<(HostId, usize)>,
                              show_collapsed_roots: bool,
                              allow_expanded: bool| {
        for root in roots {
            let occurrence = occurrence_for(*root);
            let expanded = allow_expanded && expanded_portals.contains(&occurrence);
            for (index, line) in agent_tree_lines(registry, &[*root], occurrence, topic)
                .into_iter()
                .enumerate()
            {
                let RowTarget::Agent { agent_id, .. } = line.target else {
                    unreachable!()
                };
                if expanded || (show_collapsed_roots && index == 0) {
                    segments.push(Segment::Line(line));
                }
                if replies.contains(&agent_id) && emitted_replies.insert(agent_id) {
                    segments.push(Segment::Line(Line::new(
                        LineKey::Reply(agent_id),
                        RowTarget::Reply(agent_id),
                    )));
                }
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
        let portal_ids = heading_portal_ids(&headings);
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
        // Collapsed subtrees hide behind display-level folds, not cuts —
        // the document slice stays contiguous. Rows cannot splice inside
        // a fold, so drafts and replies there stay hidden with their
        // heading instead of parking at the document tail.
        let fold_zones: Vec<Range<usize>> = fold_ranges
            .iter()
            .filter(|(zone_host, _)| zone_host == host)
            .map(|(_, range)| range.clone())
            .collect();
        for (heading_index, heading) in headings.iter().enumerate() {
            let start = heading.heading_range.start;
            let agents = filed.get(&(*host, start)).unwrap_or(&empty);
            let draft_here = draft_topic == Some(Some((*host, start)));
            let folded_here = fold_zones.iter().any(|zone| {
                zone.start <= heading.heading_range.end && heading.heading_range.end <= zone.end
            });
            let portal_here = !folded_here
                && agents.iter().any(|portal| {
                    expanded_portals.contains(&AgentOccurrence::Filed {
                        host: *host,
                        heading: portal_ids[heading_index],
                        portal: *portal,
                    })
                });
            let reply_here = agents.iter().any(|root| {
                registry
                    .agent_subtree(*root)
                    .iter()
                    .any(|agent_id| replies.contains(agent_id))
            });
            if !portal_here && !reply_here && !draft_here {
                continue;
            }
            // Rows splice right after the heading line, before its body
            // (the cut swallows the newline, so the excerpt boundary's
            // synthetic one doesn't double it). When that point is
            // folded away, they land after the outermost enclosing fold
            // instead — visible below the collapsed heading without
            // popping it open.
            let mut position = heading.heading_range.end;
            let mut resume = heading.body_range.start;
            if let Some(zone_end) = fold_zones
                .iter()
                .filter(|zone| zone.start <= position && position <= zone.end)
                .map(|zone| zone.end)
                .max()
            {
                position = zone_end;
                resume = if text[zone_end..].starts_with('\n') {
                    zone_end + 1
                } else {
                    zone_end
                };
            }
            if position > slice_start {
                segments.push(Segment::Doc {
                    host: *host,
                    range: slice_start..position,
                    id: slice_id,
                });
                slice_id = next_slice_id(&heading.title, &mut title_counts);
                slice_start = resume;
            }
            push_agent_portals(
                &mut segments,
                &mut emitted_replies,
                agents,
                &|portal| AgentOccurrence::Filed {
                    host: *host,
                    heading: portal_ids[heading_index],
                    portal,
                },
                Some((*host, start)),
                false,
                portal_here,
            );
            if draft_here {
                segments.push(Segment::Line(Line::new(
                    LineKey::NewDraft(Some((*host, start))),
                    RowTarget::NewDraft(Some((*host, start))),
                )));
            }
        }
        // The tail slice drops trailing blank lines: the listing's own
        // spacers separate it from what follows.
        let tail_end = text.trim_end().len().max(slice_start);
        if tail_end > slice_start || slice_start == 0 {
            segments.push(Segment::Doc {
                host: *host,
                range: slice_start..tail_end,
                id: slice_id,
            });
        }
    }

    let filed_roots = filed
        .values()
        .flatten()
        .map(|agent_id| root_agent(registry, *agent_id))
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
        segments.push(Segment::Line(Line::new(
            LineKey::Spacer(*host),
            RowTarget::None,
        )));
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
            header.span(Some(DashClass::Muted), |line| {
                if loudest > UiAttention::Quiet {
                    line.push(' ');
                    line.push_str(attention_glyph(loudest));
                } else {
                    line.push_str(" …");
                }
            });
        }
        segments.push(Segment::Line(header));
        if !folded {
            push_agent_portals(
                &mut segments,
                &mut emitted_replies,
                &unfiled,
                &|portal| AgentOccurrence::Unfiled {
                    host: *host,
                    portal,
                },
                None,
                true,
                true,
            );
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

    // The tail trim above exists so the listing's spacers control the
    // gap before generated rows. When nothing follows the document, show
    // it whole — otherwise typing at its very end (a new heading, say)
    // lands in the concealed trailing newline and appears to do nothing.
    if let Some(Segment::Doc { host, range, .. }) = segments.last_mut()
        && let Some((_, text)) = documents.iter().find(|(doc_host, _)| doc_host == host)
    {
        range.end = text.len();
    }

    segments
}

/// Bench-only access to the pure per-frame pass: `generate` plus the
/// fingerprint comparison, the work `refresh_dashboard` does on every
/// render before its early-out.
#[cfg(feature = "bench-support")]
pub mod bench_support {
    use super::*;

    pub struct Pass(Vec<Segment>);

    pub fn generate_pass(
        registry: &AgentRegistry,
        documents: &[(HostId, String)],
        filed: &HashMap<(HostId, usize), Vec<AgentId>>,
    ) -> Pass {
        Pass(generate(
            registry,
            documents,
            filed,
            &[],
            ListingVisibility {
                collapsed_unfiled: &HashSet::new(),
                expanded_portals: &HashSet::new(),
                raw: false,
            },
            &[],
            None,
        ))
    }

    impl Pass {
        pub fn matches(&self, other: &Pass) -> bool {
            self.0 == other.0
        }

        pub fn len(&self) -> usize {
            self.0.len()
        }
    }
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
            facts: rho_ui_proto::UiAgentFacts {
                turn_running: attention == UiAttention::Working,
                last_turn_ended: (attention >= UiAttention::Pending).then_some(UnixMs(active)),
                last_user_message_at: UnixMs(0),
                needs_you_hint: attention == UiAttention::NeedsInput,
            },
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

    #[cfg(feature = "native")]
    #[test]
    fn every_page_tag_is_retained_when_one_heading_has_multiple_pages() {
        let first = "web-00000000-0000-4000-8000-000000000001"
            .parse::<rho_browser::PageId>()
            .unwrap();
        let second = "web-00000000-0000-4000-8000-000000000002"
            .parse::<rho_browser::PageId>()
            .unwrap();
        let documents = vec![(HostId(1), format!("* Topic :{}:{}:\n", first, second))];

        let (headings, referenced) = Dashboard::resolve_page_bindings(&documents);
        assert_eq!(headings.len(), 1);
        assert_eq!(referenced, HashSet::from([first, second]));
    }

    #[test]
    fn snippets_are_server_bounded_again_for_the_row() {
        assert_eq!(truncate_chars("short", 8), "short");
        assert_eq!(truncate_chars("123456789", 8), "1234567…");
    }

    #[test]
    fn global_reply_and_text_priorities_share_one_scale() {
        let now = chrono::NaiveDate::from_ymd_opt(2026, 8, 23)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
            .fixed_offset();
        let mut blocked = agent(1, None, UiAttention::Quiet, 0);
        blocked.facts = rho_ui_proto::UiAgentFacts {
            last_turn_ended: Some(UnixMs(now.timestamp_millis() as u64)),
            last_user_message_at: UnixMs(0),
            needs_you_hint: true,
            turn_running: false,
        };
        let (reg, host) = registry(vec![blocked.clone()]);
        let text = format!(
            "* One-day ripe todo\n:todo: 2026-08-15 7d\n* Blocked :eng-{}:\n",
            &blocked.agent_id.encoded()[..4]
        );
        let queue = assemble_deal_queue(&[(host, text)], &deal_agent_facts(&reg), now, 0);
        assert_eq!(queue.cards[0].label, "blocked · 0h");
        assert_eq!(queue.cards[0].priority, 1.0);
        assert_eq!(queue.cards[1].label, "todo · ripe 1d");
        assert_eq!(queue.cards[1].priority, 1.0);

        blocked.facts.last_turn_ended = Some(UnixMs(
            (now.timestamp_millis() - chrono::Duration::hours(2).num_milliseconds()) as u64,
        ));
        let (reg, host) = registry(vec![blocked.clone()]);
        let text = format!(
            "* Three-day ripe todo\n:todo: 2026-08-13 7d\n* Blocked :eng-{}:\n",
            &blocked.agent_id.encoded()[..4]
        );
        let queue = assemble_deal_queue(&[(host, text)], &deal_agent_facts(&reg), now, 0);
        assert_eq!(queue.cards[0].label, "todo · ripe 3d");
        assert_eq!(queue.cards[0].priority, 3.0);
        assert_eq!(queue.cards[1].label, "blocked · 2h");
        assert_eq!(queue.cards[1].priority, 2.0);
    }

    #[test]
    fn fyi_matches_reminder_fresh_and_dies_at_the_floor_after_three_days() {
        let now = chrono::NaiveDate::from_ymd_opt(2026, 8, 23)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
            .fixed_offset();
        let mut fyi = agent(1, None, UiAttention::Quiet, 0);
        fyi.facts = rho_ui_proto::UiAgentFacts {
            last_turn_ended: Some(UnixMs(now.timestamp_millis() as u64)),
            last_user_message_at: UnixMs(0),
            needs_you_hint: false,
            turn_running: false,
        };
        let (reg, host) = registry(vec![fyi.clone()]);
        let text = format!(
            "* Reminder\n:reminder: 2026-08-23\n* FYI :eng-{}:\n",
            &fyi.agent_id.encoded()[..4]
        );
        let queue = assemble_deal_queue(&[(host, text.clone())], &deal_agent_facts(&reg), now, 0);
        let fyi_card = queue.cards.iter().find(|card| card.label == "fyi").unwrap();
        let reminder = queue
            .cards
            .iter()
            .find(|card| card.label == "reminder")
            .unwrap();
        assert_eq!(fyi_card.priority, 0.0);
        assert_eq!(reminder.priority, 0.0);

        fyi.facts.last_turn_ended = Some(UnixMs(
            (now.timestamp_millis() - chrono::Duration::days(3).num_milliseconds()) as u64,
        ));
        let (reg, _) = registry(vec![fyi]);
        let queue = assemble_deal_queue(&[(host, text)], &deal_agent_facts(&reg), now, 0);
        assert!(queue.cards.iter().all(|card| card.label != "fyi"));
    }

    #[test]
    fn floor_cutoff_random_tail_and_liveness_are_distinct() {
        let now = chrono::NaiveDate::from_ymd_opt(2026, 8, 23)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
            .fixed_offset();
        let mut text = String::new();
        for index in 0..10 {
            text.push_str(&format!("* Todo {index}\n:todo: 2026-08-13 7d\n"));
        }
        text.push_str("* At floor\n:todo: 2026-08-15 9d\n");
        let host = HostId(1);
        let queue = assemble_deal_queue(&[(host, text)], &[], now, 7);
        assert_eq!(queue.total_alive, 10);
        assert_eq!(queue.dealt_count, DEAL_PRIORITY_CUTOFF);
        assert_eq!(queue.cards.len(), DEAL_PRIORITY_CUTOFF + 1);
        assert_eq!(queue.cards.last().unwrap().label, "random");
        let ninth_ranked = DealCard {
            label: "todo".into(),
            priority: 0.0,
            host,
            heading_offset: Some("* Todo 0\n:todo: 2026-08-13 7d\n".len() * 8),
            agent_id: None,
            agent_tag: None,
            breadcrumb: String::new(),
        };
        assert!(queue.is_live(&ninth_ranked));
    }

    #[test]
    fn snooze_supersedes_only_verdict_family_marks() {
        let text = "* Topic\n:deadline: 2026-08-30 7d\n:todo: 2026-08-20 7d\n:defer: 2026-08-24 1d\n:reminder: 2026-08-25 1d\n:skip: 2026-08-26\n:done: 2026-08-01\n";
        let heading = parse(text).remove(0);
        let line = property_line(
            TemporalMarkKind::Defer,
            chrono::NaiveDate::from_ymd_opt(2026, 8, 27)
                .unwrap()
                .and_time(chrono::NaiveTime::MIN),
            Some(4),
        );
        let mut edits = heading_property_edits(text, &heading, TemporalMarkKind::Defer, line);
        edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
        let mut result = text.to_owned();
        for (range, replacement) in edits {
            result.replace_range(range, &replacement);
        }
        assert!(result.contains(":deadline: 2026-08-30 7d"));
        assert!(result.contains(":todo: 2026-08-20 7d"));
        assert!(result.contains(":done: 2026-08-01"));
        assert!(result.contains(":defer: 2026-08-27 4d"));
        assert!(!result.contains(":reminder:"));
        assert!(!result.contains(":skip:"));
        assert_eq!(result.matches(":defer:").count(), 1);
    }

    #[test]
    fn deal_chrome_names_the_current_section_and_position() {
        let host = HostId(1);
        let cards = vec![
            DealCard {
                label: "reminder".into(),
                priority: 0.0,
                host,
                heading_offset: Some(0),
                agent_id: None,
                agent_tag: None,
                breadcrumb: "One".into(),
            },
            DealCard {
                label: "todo · ripe 2d".into(),
                priority: 2.0,
                host,
                heading_offset: Some(6),
                agent_id: None,
                agent_tag: None,
                breadcrumb: "Two".into(),
            },
        ];
        let deal = DealSession {
            anchors: vec![None, None],
            boundary_anchors: vec![None, None],
            cards,
            index: 1,
            verdicts: 0,
            verdict_recorded: false,
        };
        assert!(deal_hint(&deal).starts_with("todo · ripe 2d · 2/2 · r reply"));
    }

    #[test]
    fn valid_active_marks_have_terse_display_labels() {
        let text = "* Ship\n:deadline: 2026-08-30 7d\n* Broken\n:deadline: nonsense 7d\n";
        let heading = parse(text).remove(0);
        let now = chrono::NaiveDate::from_ymd_opt(2026, 8, 23)
            .unwrap()
            .and_time(chrono::NaiveTime::MIN);
        assert_eq!(
            display_mark(&heading, now).as_deref(),
            Some("deadline aug 30 · 7d")
        );
        let malformed = parse("* Ship\n:deadline: nonsense 7d\n").remove(0);
        assert_eq!(display_mark(&malformed, now), None);
        let concealed_source = heading_conceals(&[(HostId(1), text.to_owned())], None, true)
            .into_iter()
            .map(|(_, range, _, _)| &text[range])
            .collect::<Vec<_>>();
        assert!(
            concealed_source
                .iter()
                .any(|source| source.contains(":deadline: 2026-08-30 7d"))
        );
        assert!(
            concealed_source
                .iter()
                .all(|source| !source.contains(":deadline: nonsense 7d"))
        );
    }

    #[test]
    fn bound_agents_decorate_their_heading_and_replies_still_cut() {
        let a = agent(1, None, UiAttention::Quiet, 30);
        let b = agent(2, None, UiAttention::NeedsInput, 10);
        let (registry, host) = registry(vec![a.clone(), b.clone()]);
        let text = "* One\nbody\n* Two\n".to_string();
        let mut filed = HashMap::new();
        filed.insert(
            (host, 0),
            sorted_agents(&registry, [a.agent_id, b.agent_id]),
        );
        // The binding stays in the heading's end-of-line hint until `g t`.
        let segments = generate(
            &registry,
            &[(host, text.clone())],
            &filed,
            &[],
            ListingVisibility {
                collapsed_unfiled: &HashSet::new(),
                expanded_portals: &HashSet::new(),
                raw: false,
            },
            &[],
            None,
        );
        assert_eq!(segments.len(), 1);
        assert!(matches!(segments[0], Segment::Doc { ref range, .. } if *range == (0..text.len())));

        let heading = heading_portal_ids(&parse(&text))[0];
        let expanded = [a.agent_id, b.agent_id]
            .map(|portal| AgentOccurrence::Filed {
                host,
                heading,
                portal,
            })
            .into_iter()
            .collect();
        let segments = generate(
            &registry,
            &[(host, text.clone())],
            &filed,
            &[],
            ListingVisibility {
                collapsed_unfiled: &HashSet::new(),
                expanded_portals: &expanded,
                raw: false,
            },
            &[],
            None,
        );
        assert_eq!(segments.len(), 4);
        assert!(
            matches!(segments[1], Segment::Line(ref line) if matches!(line.target, RowTarget::Agent { agent_id, .. } if agent_id == b.agent_id))
        );
        assert!(
            matches!(segments[2], Segment::Line(ref line) if matches!(line.target, RowTarget::Agent { agent_id, .. } if agent_id == a.agent_id))
        );

        // An open reply splices in right under the heading line, before
        // the body.
        let segments = generate(
            &registry,
            &[(host, text.clone())],
            &filed,
            &[],
            ListingVisibility {
                collapsed_unfiled: &HashSet::new(),
                expanded_portals: &HashSet::new(),
                raw: false,
            },
            &[b.agent_id],
            None,
        );
        assert_eq!(segments.len(), 3);
        assert!(matches!(
            segments[1],
            Segment::Line(ref line) if line.key == LineKey::Reply(b.agent_id)
        ));

        // The heading's decoration triages the needs-input agent first
        // and names both agents by id.
        let decorations = heading_decorations(
            &registry,
            &[(host, text.clone())],
            &filed,
            &HashSet::new(),
            &[],
        );
        assert_eq!(decorations.len(), 1);
        let (_, offset, label) = &decorations[0];
        assert_eq!(*offset, "* One".len());
        assert!(label.starts_with("  ? "), "label: {label:?}");
        assert_eq!(label.matches(" eng-").count(), 2, "label: {label:?}");
    }

    #[test]
    fn repeated_portals_expand_the_same_complete_runtime_tree_independently() {
        let root = agent(1, None, UiAttention::Quiet, 30);
        let child = agent(2, Some(root.agent_id), UiAttention::Working, 20);
        let grandchild = agent(3, Some(child.agent_id), UiAttention::Pending, 10);
        let (registry, host) = registry(vec![root.clone(), child.clone(), grandchild.clone()]);
        let text = "* One\n* Two\n".to_owned();
        let mut filed = HashMap::new();
        filed.insert((host, 0), vec![root.agent_id]);
        filed.insert((host, "* One\n".len()), vec![root.agent_id]);

        let collapsed = generate(
            &registry,
            &[(host, text.clone())],
            &filed,
            &[],
            ListingVisibility {
                collapsed_unfiled: &HashSet::new(),
                expanded_portals: &HashSet::new(),
                raw: false,
            },
            &[],
            None,
        );
        let collapsed_rows = collapsed
            .iter()
            .filter_map(|segment| match segment {
                Segment::Line(Line {
                    key:
                        LineKey::Agent {
                            agent_id,
                            occurrence,
                        },
                    ..
                }) => Some((*agent_id, occurrence.clone())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(collapsed_rows.is_empty());

        let portal_ids = heading_portal_ids(&parse(&text));
        let first = AgentOccurrence::Filed {
            host,
            heading: portal_ids[0],
            portal: root.agent_id,
        };
        let second = AgentOccurrence::Filed {
            host,
            heading: portal_ids[1],
            portal: root.agent_id,
        };
        let expanded = HashSet::from([first.clone()]);
        let first_only = generate(
            &registry,
            &[(host, text.clone())],
            &filed,
            &[],
            ListingVisibility {
                collapsed_unfiled: &HashSet::new(),
                expanded_portals: &expanded,
                raw: false,
            },
            &[],
            None,
        );
        assert_eq!(
            first_only
                .iter()
                .filter_map(|segment| match segment {
                    Segment::Line(Line {
                        target: RowTarget::Agent { agent_id, .. },
                        ..
                    }) => {
                        Some(*agent_id)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![root.agent_id, child.agent_id, grandchild.agent_id]
        );

        let expanded = HashSet::from([first, second]);
        let segments = generate(
            &registry,
            &[(host, text)],
            &filed,
            &[],
            ListingVisibility {
                collapsed_unfiled: &HashSet::new(),
                expanded_portals: &expanded,
                raw: false,
            },
            &[],
            None,
        );
        let rows = segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Line(line) => match &line.target {
                    RowTarget::Agent { agent_id, .. } => Some((&line.key, *agent_id)),
                    _ => None,
                },
                Segment::Doc { .. } => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            rows.iter().map(|(_, id)| *id).collect::<Vec<_>>(),
            vec![
                root.agent_id,
                child.agent_id,
                grandchild.agent_id,
                root.agent_id,
                child.agent_id,
                grandchild.agent_id,
            ]
        );
        assert_ne!(
            rows[0].0, rows[3].0,
            "each portal has distinct row identity"
        );
        assert!(
            matches!(rows[1].0, LineKey::Agent { agent_id, .. } if *agent_id == child.agent_id)
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
            &[],
            ListingVisibility {
                collapsed_unfiled: &HashSet::new(),
                expanded_portals: &HashSet::new(),
                raw: false,
            },
            &[],
            None,
        );
        assert_eq!(keys(&segments), vec![format!("doc 0..{}", text.len()),]);
    }

    #[test]
    fn collapsed_heading_folds_its_subtree_and_keeps_the_document_whole() {
        let a = agent(1, None, UiAttention::Quiet, 30);
        let (registry, host) = registry(vec![a.clone()]);
        let text = "* One\nbody\n* Two\n".to_string();
        let mut filed = HashMap::new();
        filed.insert((host, 0), vec![a.agent_id]);
        let collapsed = vec![(host, 5..10)];
        let heading = heading_portal_ids(&parse(&text))[0];
        let expanded = HashSet::from([AgentOccurrence::Filed {
            host,
            heading,
            portal: a.agent_id,
        }]);
        // A folded heading hides even an explicitly opened runtime tree.
        let segments = generate(
            &registry,
            &[(host, text.clone())],
            &filed,
            &collapsed,
            ListingVisibility {
                collapsed_unfiled: &HashSet::new(),
                expanded_portals: &expanded,
                raw: false,
            },
            &[],
            None,
        );
        assert_eq!(segments.len(), 1);
        assert!(!segments.iter().any(|segment| matches!(
            segment,
            Segment::Line(Line {
                target: RowTarget::Agent { .. },
                ..
            })
        )));
        // The fold hides everything after the heading line except the
        // final newline, so the next heading keeps a line of its own.
        let headings = parse(&text);
        assert_eq!(subtree_fold_range(&text, &headings[0]), Some(5..10));
        // A reply under the fold splices right after the fold's end, so
        // it shows below the collapsed heading without popping it open
        // (and without parking at the document tail).
        let segments = generate(
            &registry,
            &[(host, text.clone())],
            &filed,
            &collapsed,
            ListingVisibility {
                collapsed_unfiled: &HashSet::new(),
                expanded_portals: &HashSet::new(),
                raw: false,
            },
            &[a.agent_id],
            None,
        );
        assert_eq!(segments.len(), 3);
        assert!(matches!(
            segments[1],
            Segment::Line(ref line) if line.key == LineKey::Reply(a.agent_id)
        ));
    }

    #[test]
    fn folds_never_capture_the_cursor() {
        let text = "* One\nbody\nx\n* Two\n";
        let fold = 5..12;
        // Outside the range — before, at either boundary, after — the
        // fold applies untouched. The end boundary is where word
        // motions rest, and its left-biased anchor keeps typed text
        // outside the fold.
        assert_eq!(cursor_clamped_fold(text, fold.clone(), 0), Some(5..12));
        assert_eq!(cursor_clamped_fold(text, fold.clone(), 5), Some(5..12));
        assert_eq!(cursor_clamped_fold(text, fold.clone(), 12), Some(5..12));
        assert_eq!(cursor_clamped_fold(text, fold.clone(), 14), Some(5..12));
        // On the fold's last line (`o` below a folded heading, then
        // typing) the fold shortens to end above that line.
        assert_eq!(cursor_clamped_fold(text, fold.clone(), 11), Some(5..10));
        // Deeper inside — or on the subtree's only line — it lifts.
        assert_eq!(cursor_clamped_fold(text, fold.clone(), 8), None);
        let short = "* One\nbody\n* Two\n";
        assert_eq!(cursor_clamped_fold(short, 5..10, 8), None);
    }

    #[test]
    fn collapsing_a_parent_folds_its_whole_subtree() {
        let text = "* Parent\nbody\n** Child\nchild body\n* Other\n";
        let headings = parse(text);
        // The parent's fold spans body and nested subheading alike; the
        // child's own fold nests inside it, waiting for the parent to
        // open.
        let parent = subtree_fold_range(text, &headings[0]).unwrap();
        let child = subtree_fold_range(text, &headings[1]).unwrap();
        assert_eq!(parent, 8..33);
        assert_eq!(child, 22..33);
        assert!(parent.start <= child.start && child.end <= parent.end);
        // A childless heading with no body has nothing to fold.
        assert_eq!(subtree_fold_range(text, &headings[2]), None);
    }

    fn apply_edits(text: &str, edits: &[(std::ops::Range<usize>, String)]) -> String {
        let mut patched = text.to_owned();
        for (range, replacement) in edits.iter().rev() {
            patched.replace_range(range.clone(), replacement);
        }
        patched
    }

    #[test]
    fn demote_and_promote_move_the_whole_subtree() {
        let text = "* Top\n** Child\nbody\n*** Grand\n* Next\n";
        let headings = parse(text);
        let edits = structure_edits(&headings, 0, StructureDirection::Demote);
        assert_eq!(
            apply_edits(text, &edits),
            "** Top\n*** Child\nbody\n**** Grand\n* Next\n"
        );
        let child = headings
            .iter()
            .position(|heading| heading.title == "Child")
            .unwrap();
        let edits = structure_edits(&headings, child, StructureDirection::Promote);
        assert_eq!(
            apply_edits(text, &edits),
            "* Top\n* Child\nbody\n** Grand\n* Next\n"
        );
        assert!(structure_edits(&headings, 0, StructureDirection::Promote).is_empty());
    }

    #[test]
    fn tab_cycles_folded_then_children_then_expanded() {
        let text = "* Top\nbody\n** A\n*** Deep\n** B\n* Next\ntail\n";
        let headings = parse(text);
        let top = 0;
        let a = text.find("** A").unwrap();
        let next = text.find("* Next").unwrap();

        // Expanded → folded: the whole subtree behind the heading line.
        assert_eq!(cycle_folds(text, &headings, top, &[]), vec![5..29]);
        // Folded → children (org's CHILDREN): the parent's own body and
        // each child's subtree hide, so only the child heading lines
        // show. B has nothing of its own to hide.
        assert_eq!(
            cycle_folds(text, &headings, top, &[(top, 5..29)]),
            vec![5..10, 15..24]
        );
        // Children → SUBTREE: everything inside opens, grandchildren
        // included.
        assert_eq!(
            cycle_folds(text, &headings, top, &[(top, 5..10), (a, 15..24)]),
            Vec::<Range<usize>>::new()
        );

        // A heading without children toggles.
        assert_eq!(cycle_folds(text, &headings, next, &[]), vec![36..41]);
        assert_eq!(
            cycle_folds(text, &headings, next, &[(next, 36..41)]),
            Vec::<Range<usize>>::new()
        );

        // Folds outside the cycled subtree survive the step.
        assert_eq!(
            cycle_folds(text, &headings, top, &[(next, 36..41)]),
            vec![36..41, 5..29]
        );
    }

    #[test]
    fn tab_cycle_survives_bodiless_children() {
        // With nothing to hide on any child, the CHILDREN state is
        // carried entirely by the parent's body fold — without it the
        // state would be indistinguishable from expanded and TAB would
        // degrade to a two-way toggle.
        let text = "* Top\nbody\n** A\n** B\n* Next\n";
        let headings = parse(text);

        let folded = cycle_folds(text, &headings, 0, &[]);
        assert_eq!(folded, vec![5..20]);
        let children = cycle_folds(text, &headings, 0, &[(0, 5..20)]);
        assert_eq!(children, vec![5..10]);
        let expanded = cycle_folds(text, &headings, 0, &[(0, 5..10)]);
        assert_eq!(expanded, Vec::<Range<usize>>::new());
    }

    #[test]
    fn archiving_moves_the_subtree_under_a_sibling_archive() {
        // No archive yet: one is created at the end of the document, the
        // subtree demotes into it, the tag rides along, and the archive
        // time lands as a property on the moved heading.
        let text = "* Done task :eng-aa:\nnotes\n** Sub\n* Alive\n";
        let (edits, archive_offset) =
            archive_edits(text, 0, "2026-08-08 12:00").expect("archivable");
        let patched = apply_edits(text, &edits);
        assert_eq!(
            patched,
            "* Alive\n* Archive :archive:\n** Done task :eng-aa:\n:archived: 2026-08-08 12:00\nnotes\n*** Sub\n"
        );
        assert_eq!(&patched[archive_offset..archive_offset + 9], "* Archive");

        // A nested heading archives under its own parent, next to its
        // siblings, into the archive that already exists there.
        let text = "* Project\n** Old :eng-bb:\n** Archive :archive:\n** Fresh\n* Other\n";
        let (edits, archive_offset) =
            archive_edits(text, text.find("** Old").unwrap(), "2026-08-08 12:00").unwrap();
        let patched = apply_edits(text, &edits);
        assert_eq!(
            patched,
            "* Project\n** Archive :archive:\n*** Old :eng-bb:\n:archived: 2026-08-08 12:00\n** Fresh\n* Other\n"
        );
        assert_eq!(&patched[archive_offset..archive_offset + 10], "** Archive");

        // Inside an archive there is nothing further to archive, and the
        // archive itself cannot be archived.
        assert!(archive_edits(&patched, patched.find("*** Old").unwrap(), "now").is_none());
        assert!(archive_edits(&patched, patched.find("** Archive").unwrap(), "now").is_none());
    }

    #[test]
    fn archived_agents_are_quiet_no_matter_what_they_want() {
        let loud = agent(1, None, UiAttention::NeedsInput, 10);
        let (registry, host) = registry(vec![loud.clone()]);
        let text = "* Archive :archive:\n** Old :eng-aa:\n".to_string();
        let mut filed = HashMap::new();
        filed.insert((host, text.find("** Old").unwrap()), vec![loud.agent_id]);
        let documents = [(host, text.clone())];
        assert!(archived_roots(&documents, &filed).contains(&loud.agent_id));
        let decorations = heading_decorations(&registry, &documents, &filed, &HashSet::new(), &[]);
        let (_, _, label) = &decorations[0];
        assert!(
            label.starts_with("  · ") && !label.contains('—'),
            "archived decoration should be quiet: {label:?}"
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
            &[],
            ListingVisibility {
                collapsed_unfiled: &HashSet::new(),
                expanded_portals: &HashSet::new(),
                raw: false,
            },
            &[],
            Some(Some((host, 0))),
        );
        assert_eq!(
            keys(&segments),
            vec![
                "doc 0..5".to_string(),
                format!("{:?}", LineKey::NewDraft(Some((host, 0)))),
                format!("doc 6..{}", text.len()),
            ]
        );
    }

    #[test]
    fn unfiled_portals_are_collapsed_by_default_and_expand_their_full_tree() {
        let root = agent(1, None, UiAttention::Quiet, 1);
        let child = agent(2, Some(root.agent_id), UiAttention::Pending, 2);
        let (registry, host) = registry(vec![root.clone(), child.clone()]);
        let segments = generate(
            &registry,
            &[(host, String::new())],
            &HashMap::new(),
            &[],
            ListingVisibility {
                collapsed_unfiled: &HashSet::new(),
                expanded_portals: &HashSet::new(),
                raw: false,
            },
            &[],
            None,
        );
        let agents = segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Line(line) if matches!(line.key, LineKey::Agent { .. }) => Some(line),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(agents.len(), 1);
        assert!(matches!(
            agents[0].key,
            LineKey::Agent { agent_id, .. } if agent_id == root.agent_id
        ));

        let mut expanded = HashSet::new();
        expanded.insert(AgentOccurrence::Unfiled {
            host,
            portal: root.agent_id,
        });
        let segments = generate(
            &registry,
            &[(host, String::new())],
            &HashMap::new(),
            &[],
            ListingVisibility {
                collapsed_unfiled: &HashSet::new(),
                expanded_portals: &expanded,
                raw: false,
            },
            &[],
            None,
        );
        let agents = segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Line(line) if matches!(line.key, LineKey::Agent { .. }) => Some(line),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(agents.len(), 2);
        assert!(matches!(
            agents[1].key,
            LineKey::Agent { agent_id, .. } if agent_id == child.agent_id
        ));
        assert!(!agents[1].text.contains("└─"));
        assert!(agents[1].text.starts_with("    "));
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
            &[],
            ListingVisibility {
                collapsed_unfiled: &collapsed_unfiled,
                expanded_portals: &HashSet::new(),
                raw: false,
            },
            &[],
            None,
        );
        assert!(
            !segments.iter().any(|segment| matches!(
                segment,
                Segment::Line(line) if matches!(line.key, LineKey::Agent { .. })
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
        let text = "* TODO Ship it\n:project: rho\n* STAFFED Crewed\n* DONE Old\n* Property terminal\n:done: 2026-08-24\n";
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
        // Terminal headings fade as a whole. The derived property state
        // drives the same styling even though its source line is concealed.
        assert!(
            spans
                .iter()
                .any(|(class, range)| matches!(class, DashClass::Muted)
                    && &text[range.clone()] == "DONE")
        );
        assert!(spans.iter().any(
            |(class, range)| matches!(class, DashClass::Muted) && &text[range.clone()] == "Old"
        ));
        assert!(
            spans
                .iter()
                .any(|(class, range)| matches!(class, DashClass::Muted)
                    && &text[range.clone()] == "Property terminal")
        );
        assert!(
            spans
                .iter()
                .any(|(class, range)| matches!(class, DashClass::Muted)
                    && &text[range.clone()] == ":project: rho")
        );
    }

    #[test]
    fn heading_titles_color_by_depth() {
        let text = "* One\n** Two\n*** Three\n**** Four\n***** Five\n";
        let spans = doc_spans(text);
        let class_of = |title: &str| {
            spans
                .iter()
                .find(|(_, range)| &text[range.clone()] == title)
                .map(|(class, _)| *class)
                .unwrap()
        };
        assert_eq!(class_of("One"), DashClass::Heading);
        assert_eq!(class_of("Two"), DashClass::Heading2);
        assert_eq!(class_of("Three"), DashClass::Heading3);
        assert_eq!(class_of("Four"), DashClass::Heading4);
        // Level five wraps back around to the level-one color.
        assert_eq!(class_of("Five"), DashClass::Heading);
    }
}
