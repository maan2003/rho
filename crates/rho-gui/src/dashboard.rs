//! The dashboard: the Desk document as the home surface — rho's
//! magit-status. The real per-host CRDT document is spliced into the
//! editor as writable excerpts, so headings and prose are edited
//! directly with plain vim, while generated read-only agent rows are
//! interleaved under the headings whose typed bindings attach them. Headings
//! normally summarize bindings in an end-of-line hint; `g t` temporarily
//! projects the named agents' shared runtime rows and complete spawn trees.
//! Acting keys address the row under the cursor: `enter` opens, `r`
//! splices an inline reply draft under the row. Generated rows
//! and drafts sit between document slices — a refresh rearranges excerpts
//! but can never eat what the user typed.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use editor::scroll::Autoscroll;
use editor::{Editor, EditorMode, HighlightKey, Inlay, SelectionEffects, SizingBehavior};
use gpui::prelude::*;
use gpui::{App, Context, Entity, Focusable as _, HighlightStyle, Window};
use language::{Buffer, Capability, InlayId, Point};
use multi_buffer::composition::{Composition, CompositionSpec, RowSpec};
use multi_buffer::{MultiBuffer, MultiBufferRow};
pub use rho_desk::cells::SlackUnit;
use rho_registry::Attention as UiAttention;
use rho_ui_proto::AgentId;
use text::{Bias, BufferId, ToOffset as _};
use theme::ActiveTheme as _;

use crate::registry::{AgentRegistry, HostId};
use crate::workspace::Workspace;

/// Highlight-key space for dashboard classes, clear of the transcript's
/// semantic and syntax key ranges.
const DASHBOARD_KEY_BASE: usize = usize::MAX - 200;

const TREE_INLAY_ID_BASE: usize = 2_000_000;
/// Indent inlays for the second and later lines of a note body. Far enough
/// past the row prefixes that a long note cannot collide with them.
const CONTINUATION_INLAY_ID_BASE: usize = 3_000_000;

type DraftTopic = Option<(HostId, rho_desk::cells::Id)>;
type DraftState = (DraftTopic, Entity<Buffer>, gpui::Subscription);

// Dealer curve tuning. These are deliberately all in one place: rho has one
// user, so policy changes are edits, not a configuration system.
const DEAL_QUEUE_FLOOR: f64 = -1.0;
/// How long a skipped card stays out of the next pull. Nothing else times
/// out: the card is still open the whole time and Home still shows it.
const SKIP_COOLDOWN: chrono::TimeDelta = chrono::TimeDelta::minutes(15);
const BLOCKED_REPLY_HEAD_START: f64 = 1.0;
const BLOCKED_REPLY_SLOPE_PER_DAY: f64 = 12.0;
const FYI_REPLY_PACE_DAYS: f64 = 3.0;
/// A person waiting on a reply is a blocked agent with a name, so a thread
/// rises on the same slope. The head start is a tenth above an agent's so a
/// ping of the same wait comes first; an agent the user just spoke to still
/// outranks it through the recency bonus, which is far larger.
const THREAD_REPLY_HEAD_START: f64 = 1.1;
/// Half a curve unit is enough to mark the hand visibly dirty without
/// turning every newly-ripe reminder into persistent chrome.
pub(crate) const LAMP_THRESHOLD: f64 = 0.5;
/// At 1.2 curve units a blocked agent chimes after about 24 minutes
/// unnoticed, an agent completed within about 12 minutes of interaction
/// chimes immediately through the recency bonus, and a ping takes about
/// 14 hours to cross. Sound therefore marks pressure, not every new card.
pub(crate) const CHIME_THRESHOLD: f64 = 1.2;
/// The agent the user just spoke to (a send, or opening its surface)
/// contributes 1.5 curve units. This must remain above the 1.2 chime
/// threshold or recently-driven agents lose their instant completion chime;
/// the quadratic fall below gives about 6 minutes of instant chime and about
/// 25 minutes above the 0.5 lamp threshold.
const AGENT_RECENCY_BONUS: f64 = 1.5;
/// The nudge is gone within the hour and falls steeply from the start
/// (quadratic: 0.375 left at 30 minutes), so "just spoke to" means minutes,
/// not a hidden hour-long preference.
const AGENT_RECENCY_WINDOW_MS: i64 = 60 * 60 * 1_000;

pub(crate) fn dealer_policy_snapshot() -> crate::journal::DealerPolicySnapshot {
    crate::journal::DealerPolicySnapshot {
        queue_floor: DEAL_QUEUE_FLOOR,
        skip_cooldown_minutes: SKIP_COOLDOWN.num_minutes(),
        blocked_reply_head_start: BLOCKED_REPLY_HEAD_START,
        blocked_reply_slope_per_day: BLOCKED_REPLY_SLOPE_PER_DAY,
        fyi_reply_pace_days: FYI_REPLY_PACE_DAYS,
        thread_reply_head_start: THREAD_REPLY_HEAD_START,
        lamp_threshold: LAMP_THRESHOLD,
        chime_threshold: CHIME_THRESHOLD,
        agent_recency_bonus: AGENT_RECENCY_BONUS,
        agent_recency_window_ms: AGENT_RECENCY_WINDOW_MS,
    }
}

#[cfg(test)]
struct DealCardHighlight;

#[derive(Clone, Debug, PartialEq)]
pub struct DealCard {
    pub label: String,
    pub priority: f64,
    pub host: HostId,
    /// The note the card hangs under, which is the anchor the desk cursor
    /// follows and the key the dealer deduplicates on. The verdict itself
    /// lands on the card's own node, `identity`.
    pub topic_node_id: rho_desk::cells::Id,
    pub agent_id: Option<AgentId>,
    pub agent_tag: Option<String>,
    pub breadcrumb: String,
    pub room: Option<String>,
    pub kind: DealCardKind,
    pub identity: DealCardId,
    /// Passed over by a pull and still inside the cooldown, with nothing new
    /// on its source since. It is still owed, so Home shows it and says so;
    /// only the next pull skips over it.
    pub skipped: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DeskRoom {
    pub host: HostId,
    pub node_id: rho_desk::cells::Id,
    pub name: String,
}

/// What a dealt node opens as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CardTarget {
    Note,
    Agent(AgentId),
    Page(rho_browser::PageId),
    Thread(SlackUnit),
    /// The node is gone, or lacks the fields its kind needs.
    Missing,
}

/// The marker and name in front of an agent's row, keeping the indent the
/// row was drawn with: only the agent's own part of the prefix moves.
fn agent_prefix(drawn: &str, agent_id: AgentId, registry: &AgentRegistry) -> String {
    let indent = drawn
        .find('•')
        .map(|at| drawn[..at].to_owned())
        .unwrap_or_default();
    format!(
        "{indent}• {} {} ",
        match registry.attention(agent_id) {
            UiAttention::Quiet => "○",
            UiAttention::Working => "·",
            UiAttention::Pending => "●",
            UiAttention::NeedsInput => "!",
        },
        registry.agent_human_name(agent_id)
    )
}

/// What is hinted after a row: what the user said about it and where it
/// stands. A verdict is exactly a change to this, which is why it is a
/// function of the row rather than something the drawing keeps.
fn row_hint(node: &crate::desk_view::DeskNode) -> Option<gpui::SharedString> {
    let mut hints = Vec::new();
    match node.state {
        rho_desk::cells::State::Done => hints.push("done".to_owned()),
        rho_desk::cells::State::Muted => hints.push("muted".to_owned()),
        rho_desk::cells::State::Open => {}
    }
    if let Some(at) = node.defer_until {
        hints.push(format!("defer {} · {}d", desk_date(at), node.pace_days));
    }
    if let Some(at) = node.deadline {
        hints.push(format!("due {} · {}d", desk_date(at), node.pace_days));
    }
    if node.page().is_some() {
        hints.push("page".to_owned());
    }
    if let Some(path) = node.path() {
        hints.push(path.to_string());
    }
    (!hints.is_empty()).then(|| hints.join(" · ").into())
}

/// The hint as the editor paints it.
fn eol_hint(text: gpui::SharedString) -> editor::EolHintRenderer {
    std::sync::Arc::new(move |_, cx| {
        use gpui::Styled as _;
        use settings::Settings as _;
        use theme::ActiveTheme as _;
        let settings = theme_settings::ThemeSettings::get_global(cx);
        gpui::div()
            .font(settings.buffer_font.clone())
            .text_size(settings.buffer_font_size(cx))
            .line_height(gpui::relative(settings.line_height()))
            .text_color(cx.theme().colors().text_muted)
            .child(text.clone())
            .into_any_element()
    })
}

/// One row of the map as it was drawn: where it sits in the composition,
/// what is written in front of it and what is hinted after it. Keeping
/// this is what lets a verdict cost its own row.
struct TreeRowDraw {
    host: HostId,
    node_id: rho_desk::cells::Id,
    /// Where the row starts and ends in the map, as anchors that survive
    /// an edit inside the row.
    start: editor::Anchor,
    end: editor::Anchor,
    /// The heading prefix as an inlay, and the padding inlays under it.
    prefix: String,
    inlays: Vec<InlayId>,
    /// The base the row's inlay ids were minted from, so a redraw can mint
    /// the same ones again.
    at: usize,
    /// The end-of-line hint, when the row has one.
    hint: Option<gpui::SharedString>,
    /// A closed ancestor hides the row, and a hidden row is drawn with
    /// neither prefix nor hint.
    hidden_by_fold: bool,
}

/// Every card is a thing on the desk, on the host that holds it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DealCardId {
    pub host: HostId,
    pub node_id: rho_desk::cells::Id,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DealCardKind {
    Desk,
    Agent,
    Thread,
}

/// What a Slack unit is currently about, read live from the mirror. The
/// store holds the unit's identity and its verdicts; the words, the wait,
/// and the newest message stay in Slack, and the title here is rendered
/// fresh every time this is built rather than kept from when it landed.
#[derive(Clone, Debug, PartialEq)]
pub struct SlackFacts {
    pub title: String,
    pub conversation: String,
    pub raised_at: chrono::DateTime<chrono::FixedOffset>,
    /// How long the ball has been where it is, counted from the newest
    /// message: the wait a `needs reply` card rises on, and the age a
    /// `replied` card decays from.
    pub wait_days: f64,
    /// Who the ball is with, when it is not the user.
    pub waiting_on: Option<String>,
    /// The newest message in the unit: a new one voids a skip.
    pub latest: String,
    /// The newest message from someone else, which is what a verdict cursor
    /// is compared against. `None` when only the user has written here.
    pub newest_from_other: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DealerVerdict {
    Skip,
    Done,
    Mute,
    Defer,
    Open,
    File,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DealerEvent {
    pub card: DealCardId,
    pub kind: DealCardKind,
    pub verdict: DealerVerdict,
    pub at: chrono::DateTime<chrono::FixedOffset>,
    pub skip_until: Option<chrono::DateTime<chrono::FixedOffset>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DealQueue {
    pub cards: Vec<DealCard>,
    /// Number of live headings whose winning mark is above the queue floor.
    pub total_alive: usize,
    /// Number selected by global priority.
    pub dealt_count: usize,
    /// Where each card's source stands, which is what a skip records.
    cursors: HashMap<DealCardId, CardCursor>,
}

impl DealQueue {
    /// The card a pull opens: the top of the ranking that is not skipped and
    /// is not the one already in view.
    pub fn top(&self, exclude: Option<&DealCardId>) -> Option<&DealCard> {
        self.cards
            .iter()
            .filter(|card| !card.skipped)
            .find(|card| exclude.is_none_or(|excluded| card.identity != *excluded))
    }

    pub fn card(&self, identity: &DealCardId) -> Option<&DealCard> {
        self.cards.iter().find(|card| &card.identity == identity)
    }

    /// Where the card's source stands, which is what a skip holds on to.
    pub fn cursor(&self, identity: &DealCardId) -> Option<&CardCursor> {
        self.cursors.get(identity)
    }
}

/// Everything a card is made of besides the node it is about.
struct DealerFacts<'a> {
    by_agent: HashMap<AgentId, &'a DealAgentFacts>,
    spawned: HashMap<AgentId, Vec<AgentId>>,
    threads: &'a HashMap<SlackUnit, SlackFacts>,
    now: chrono::DateTime<chrono::FixedOffset>,
    interactions: &'a HashMap<AgentId, i64>,
}

/// What a note lends the cards under it.
struct HeadingContext {
    /// The row that lends the place. An agent's card is remade when this
    /// row moves, so the set knows which cards a heading holds.
    heading: rho_desk::cells::Id,
    breadcrumb: String,
    room: Option<String>,
    bindings: Vec<AgentId>,
}

/// The cards, kept rather than made again. A card is made when the thing
/// it is about changes, and at no other time: a `Changed` remakes that
/// agent's card, a desk that arrives whole remakes that host's. Reading
/// the ranking never touches the desk, only what is already here.
#[derive(Default)]
struct DealerSet {
    /// Every candidate that stands, by the topic it is about. A note with
    /// two dated marks offers two, and the read picks one per topic the
    /// way the old pass did.
    cards: HashMap<DealCardId, Vec<RankedDealCard>>,
    /// The topics a host contributed, so a whole remake retires exactly
    /// those and leaves the other hosts alone.
    of_host: HashMap<HostId, HashSet<DealCardId>>,
    /// Where an agent's card sits, so a `Changed` can retire it without
    /// looking through the map for it.
    of_agent: HashMap<AgentId, DealCardId>,
    /// The agents each heading lends a place to, so that a verdict on a
    /// note costs the cards under it and no others.
    of_heading: HashMap<(HostId, rho_desk::cells::Id), HashSet<AgentId>>,
    /// How many cards have been made since this dashboard existed. The
    /// point of the set is that this rises by what a change names and not
    /// by the size of the desk, so a test can say exactly that.
    #[cfg(test)]
    made: usize,
}

impl DealerSet {
    fn retire(&mut self, id: &DealCardId) {
        for card in self.cards.remove(id).into_iter().flatten() {
            if let (Some(agent_id), Some(heading)) = (card.card.agent_id, card.heading)
                && let Some(held) = self.of_heading.get_mut(&(id.host, heading))
            {
                held.remove(&agent_id);
            }
        }
        if let Some(topics) = self.of_host.get_mut(&id.host) {
            topics.remove(id);
        }
    }

    fn insert(&mut self, card: RankedDealCard) {
        #[cfg(test)]
        {
            self.made += 1;
        }
        let id = card.card.identity.clone();
        if let Some(agent_id) = card.card.agent_id
            && card.card.kind == DealCardKind::Agent
        {
            self.of_agent.insert(agent_id, id.clone());
            if let Some(heading) = card.heading.clone() {
                self.of_heading
                    .entry((id.host, heading))
                    .or_default()
                    .insert(agent_id);
            }
        }
        self.of_host.entry(id.host).or_default().insert(id.clone());
        self.cards.entry(id).or_default().push(card);
    }
}

/// What a refresh is allowed to leave standing.
#[derive(Clone, Copy, Debug)]
pub enum DealScope<'a> {
    /// The desk itself is different: what is on it at all can have
    /// changed, so the host's cards are made again.
    Whole,
    /// Only these agents moved. Every other card on the host stands.
    Agents(&'a [AgentId]),
    /// Only these rows moved: the cells a desk delta named. Each row's own
    /// cards are made again, and so are the cards of the agents the row
    /// heads, since a heading that closes or defers takes its subtree out
    /// of the hand with it.
    Nodes(&'a [rho_desk::cells::Id]),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DealQueueDepth {
    pub dealt_count: usize,
    pub total_alive: usize,
}

/// Where a card's source stood. A skip holds one, and the source moving
/// past it is what voids the skip: it is the same position a verdict cursor
/// is compared against, per source, and never a rendering of one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CardCursor {
    /// The newest message in the unit, which is what a Slack verdict writes.
    Slack(rho_desk::cells::SlackTs),
    /// The agent's own chronology and what it is asking for.
    Agent(rho_registry::AgentFacts, rho_registry::Attention),
    /// The dated mark the card stands on.
    Desk(DeskMark, rho_desk::cells::Timestamp),
}

#[derive(Clone, Debug)]
struct SkippedCard {
    at: chrono::DateTime<chrono::FixedOffset>,
    cursor: CardCursor,
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
    NewDraft(Option<(HostId, rho_desk::cells::Id)>),
}

/// One place an agent's shared runtime row is projected. The occurrence is
/// row identity only; every occurrence points at the same per-agent buffer.
/// What the line under the cursor refers to; the object of every
/// dashboard command.
#[derive(Clone, Debug, PartialEq)]
pub enum RowTarget {
    None,
    TreeTopic {
        host: HostId,
        node_id: rho_desk::cells::Id,
        first_attention: Option<AgentId>,
        on_heading_line: bool,
    },
    TreeAgent {
        host: HostId,
        node_id: rho_desk::cells::Id,
        topic_node_id: rho_desk::cells::Id,
        agent_id: AgentId,
    },
    TreePage {
        host: HostId,
        node_id: rho_desk::cells::Id,
        topic_node_id: rho_desk::cells::Id,
        page_id: rho_browser::PageId,
    },
    NewDraft,
    NewTreeDraft((HostId, rho_desk::cells::Id)),
}

/// Where the cursor is: on a generated row, or at an offset inside a
/// host's document.
#[derive(Clone, Debug, PartialEq)]
enum CursorPlace {
    Row(LineKey),
    Tree(HostId, rho_desk::cells::Id, usize),
}

/// One generated segment: a slice of a host document, or a generated
/// line (row or draft slot). Equality against the previous pass lets a
/// sync bail out before touching the editor at all.
///
/// A document slice's `id` is its stable identity across passes: a hash
/// of the title of the heading whose cut opens the slice (0 for the
/// slice that starts the document). The composition keys the excerpt on

pub struct Dashboard {
    multi_buffer: Entity<MultiBuffer>,
    editor: Entity<Editor>,
    /// One buffer per generated line key: read-only listing lines and
    /// writable reply drafts alike.
    buffers: HashMap<LineKey, Entity<Buffer>>,
    /// Non-owning references to the workspace-owned Desk source buffers.
    tree_hosts: BTreeMap<HostId, TreeHostSource>,
    /// Reconciles the multibuffer to the generated spec by element
    /// identity, so unchanged excerpts — and cursors in them — survive.
    composition: Composition,
    /// Stable composition keys per line, allocated once and never reused.
    element_keys: HashMap<LineKey, u64>,
    /// One key per row, and a row is a thing in one of its places: a
    /// labelled thing has a row in its own place and one under each label.
    tree_element_keys: HashMap<(HostId, rho_desk::cells::Id, Option<rho_desk::cells::Id>), u64>,
    tree_heading_agents: HashMap<(HostId, rho_desk::cells::Id), Vec<AgentId>>,
    tree_heading_pages: HashMap<(HostId, rho_desk::cells::Id), Vec<rho_browser::PageId>>,
    next_element_key: u64,
    /// Generated rows in display order, from the last sync.
    /// What each generated key means, for cursor lookup.
    targets: HashMap<LineKey, RowTarget>,
    /// Every bound browser page, including additional bindings on a heading
    /// whose preview can display only one page.
    referenced_pages: HashSet<rho_browser::PageId>,
    /// Roots whose binding tag lives inside an `:archive:` zone, as of the
    /// last sync. Archived agents are muted: no chime, quiet decorations.
    /// Open reply drafts in creation order (position comes from `order`).
    /// Keeps the workspace re-rendering on draft edits, so placeholder
    /// and gutter chrome track the text.
    /// The inline new-agent draft, when open: its buffer plus the edit
    /// subscription that keeps chrome fresh.
    new_draft: Option<DraftState>,
    tree_new_draft_parent: Option<(HostId, rho_desk::cells::Id)>,
    /// Collapsed subtrees as anchored fold ranges, org-style: the fold
    /// is persistent state that rides edits, not something re-derived
    /// from the parse. The start anchor is right-biased (org's
    /// front-sticky through our newline-shifted boundary: typing at the
    /// end of the title stays visible) and the end anchor left-biased
    /// (rear-nonsticky: a line opened below a folded heading stays
    /// outside and visible). Ranges are recomputed only by explicit
    /// operations — cycling, archiving — like org recomputes on cycle;
    /// a range whose start no longer sits on a heading line is dropped.
    /// Hosts whose initial Desk visibility has already been seeded.
    /// User-opened folds must survive every later document sync.
    /// Next S-TAB target in org's OVERVIEW → CONTENTS → SHOW ALL cycle.
    /// Shows only literal editable Desk source, with no generated UI.
    raw_mode: bool,
    /// Phone-only composed Desk presentation: bound-agent chips collapse
    /// into colored heading bullets while desktop chrome stays unchanged.
    phone_browse_mode: bool,
    skipped: HashMap<DealCardId, SkippedCard>,
    queue_depth: DealQueueDepth,
    /// The ranking, kept between reads and maintained per change.
    dealer: DealerSet,
    /// Portal occurrences whose complete runtime subtree is visible.
    /// This is transient display state and is never written to Desk.
    /// Move the cursor into this key's buffer on the next sync — how a
    /// freshly opened reply draft receives the cursor.
    pending_cursor: Option<LineKey>,
    /// Move the cursor to this document offset on the next sync.
    /// Reply placeholder inlays currently spliced in.
    tree_inlay_ids: Vec<InlayId>,
    /// What each row of the map is drawn as, in the order the composition
    /// put them. A delta that keeps the shape redraws the rows it names
    /// out of this and leaves the rest of the map alone; without it the
    /// only way to move one row's hint was to draw every row again.
    tree_draw: Vec<TreeRowDraw>,
    tree_draw_at: HashMap<(HostId, rho_desk::cells::Id), Vec<usize>>,
    /// How many times the map has been composed, and how many rows have
    /// been drawn again in place. The rule this slice is about is a
    /// difference between these two, so a test can read it.
    #[cfg(test)]
    composed: usize,
    #[cfg(test)]
    redrawn: usize,
    /// The title last written into each machine row's buffer, against the
    /// buffer it went into. A sync used to read every one of those ropes
    /// back to find out it had not changed; on a desk of thousands of rows
    /// that read, and the editor events the rewrites raised, were most of
    /// what a sync cost.
    derived_titles: HashMap<(HostId, rho_desk::cells::Id), (gpui::EntityId, String)>,
    tree_collapsed: HashSet<(HostId, rho_desk::cells::Id)>,
    pending_tree_cursor: Option<(HostId, rho_desk::cells::Id, usize)>,
    /// The previous pass's inputs and output, so a sync whose world is
    /// unchanged returns without touching the editor.
    /// Buffers already registered as headerless with the editor. A
    /// boundary onto a headerless buffer draws nothing, so this is what
    /// keeps the interleaved excerpts seamless.
    headers_disabled: std::collections::HashSet<BufferId>,
}

struct TreeHostSource {
    nodes: Vec<crate::desk_view::DeskNode>,
    buffers: BTreeMap<rho_desk::cells::Id, Entity<Buffer>>,
    /// Every note's title, kept by the desk so that naming a row never
    /// reads a rope. Rows that are not notes have their title derived.
    titles: std::rc::Rc<HashMap<rho_desk::cells::Id, String>>,
    /// The map's edges, made once when the source is set. Walking every
    /// node to find one node's children was the whole cost of dealing a
    /// desk: quadratic in a map that only grows.
    index: TreeIndex,
}

/// The lookups the map needs, built in one pass over the nodes.
#[derive(Default)]
struct TreeIndex {
    by_id: HashMap<rho_desk::cells::Id, usize>,
    children: HashMap<rho_desk::cells::Id, Vec<usize>>,
    by_agent: HashMap<AgentId, usize>,
}

impl TreeIndex {
    fn build(nodes: &[crate::desk_view::DeskNode]) -> Self {
        let mut index = TreeIndex {
            by_id: HashMap::with_capacity(nodes.len()),
            children: HashMap::new(),
            by_agent: HashMap::new(),
        };
        for (at, node) in nodes.iter().enumerate() {
            index.by_id.insert(node.id.clone(), at);
            if let Some(parent) = &node.parent {
                index.children.entry(parent.clone()).or_default().push(at);
            }
            if let Some(agent) = node.agent() {
                index.by_agent.insert(agent, at);
            }
        }
        index
    }
}

impl TreeHostSource {
    fn node(&self, id: &rho_desk::cells::Id) -> Option<&crate::desk_view::DeskNode> {
        self.index.by_id.get(id).map(|at| &self.nodes[*at])
    }

    fn children(
        &self,
        id: &rho_desk::cells::Id,
    ) -> impl Iterator<Item = &crate::desk_view::DeskNode> {
        self.index
            .children
            .get(id)
            .into_iter()
            .flatten()
            .map(|at| &self.nodes[*at])
    }

    fn agent_node(&self, agent: AgentId) -> Option<&crate::desk_view::DeskNode> {
        self.index.by_agent.get(&agent).map(|at| &self.nodes[*at])
    }

    /// What a row is called: a note's own first line, or the title the map
    /// derives for everything else.
    fn title(&self, id: &rho_desk::cells::Id) -> Option<&str> {
        self.titles.get(id).map(String::as_str)
    }

    /// Every row's title, including the derived ones the map wrote into
    /// read-only buffers. This reads those buffers, so it belongs to the
    /// pickers a keystroke opens and not to anything a sync runs.
    fn all_titles(&self, cx: &App) -> HashMap<rho_desk::cells::Id, String> {
        let mut titles = (*self.titles).clone();
        for (id, buffer) in &self.buffers {
            if !titles.contains_key(id) {
                titles.insert(id.clone(), note_title(&buffer.read(cx).text()).to_owned());
            }
        }
        titles
    }
}

fn nearest_tree_heading(
    source: &TreeHostSource,
    mut node_id: Option<rho_desk::cells::Id>,
) -> Option<rho_desk::cells::Id> {
    while let Some(id) = node_id {
        let node = source.nodes.iter().find(|node| node.id == id)?;
        if node.is_note() {
            return Some(id);
        }
        node_id = node.parent.clone();
    }
    None
}

impl Dashboard {
    #[cfg(test)]
    pub(crate) fn has_new_draft_for_test(&self) -> bool {
        self.new_draft.is_some()
    }
    pub fn push_external_undo_transaction(&self, cx: &mut Context<Workspace>) -> clock::Lamport {
        self.editor
            .update(cx, |editor, cx| editor.push_external_undo_transaction(cx))
    }

    pub fn group_until_transaction(
        &self,
        transaction_id: clock::Lamport,
        cx: &mut Context<Workspace>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.group_until_transaction(transaction_id, cx)
        });
    }

    pub fn forget_external_undo_transaction(
        &self,
        transaction_id: clock::Lamport,
        cx: &mut Context<Workspace>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.forget_external_undo_transaction(transaction_id, cx)
        });
    }

    pub fn dispatch_semantic_row_action(
        &self,
        action: editor::SemanticRowAction,
        cx: &mut Context<Workspace>,
    ) -> bool {
        self.editor.update(cx, |editor, cx| {
            editor.dispatch_semantic_row_action(action, cx)
        })
    }

    pub fn tree_heading_named(&self, title: &str) -> Option<(HostId, rho_desk::cells::Id)> {
        self.tree_hosts.iter().find_map(|(host, source)| {
            source.nodes.iter().find_map(|node| {
                (node.is_note() && source.title(&node.id) == Some(title.trim()))
                    .then_some((*host, node.id.clone()))
            })
        })
    }

    /// Everything a card is made of that is not the node it is about,
    /// gathered once so that making one card and making all of them read
    /// the same facts.
    fn dealer_facts<'a>(
        &self,
        threads: &'a HashMap<SlackUnit, SlackFacts>,
        now: chrono::DateTime<chrono::FixedOffset>,
        agent_interactions: &'a HashMap<AgentId, i64>,
        agents: &'a [DealAgentFacts],
    ) -> DealerFacts<'a> {
        let by_agent = agents
            .iter()
            .map(|facts| (facts.agent_id, facts))
            .collect::<HashMap<_, _>>();
        // Who each agent spawned, so that the walk down from a filed agent
        // costs its own descendants rather than a pass over every agent
        // for every agent.
        let mut spawned: HashMap<AgentId, Vec<AgentId>> = HashMap::new();
        for agent in agents {
            if let Some(parent) = agent.parent {
                spawned.entry(parent).or_default().push(agent.agent_id);
            }
        }
        DealerFacts {
            by_agent,
            spawned,
            threads,
            now,
            interactions: agent_interactions,
        }
    }

    /// What a note heading lends the cards under it, or nothing when the
    /// heading is closed or waiting: a deferred note defers its whole
    /// subtree, which is the gate the old model spelled as a ripe todo.
    fn heading_context(
        &self,
        source: &TreeHostSource,
        heading: &crate::desk_view::DeskNode,
        now: chrono::DateTime<chrono::FixedOffset>,
    ) -> Option<HeadingContext> {
        if heading.state != rho_desk::cells::State::Open {
            return None;
        }
        if desk_deferred(heading, now.naive_local()) {
            return None;
        }
        let ancestor_deferred = std::iter::successors(heading.parent.clone(), |parent| {
            source.node(parent).and_then(|node| node.parent.clone())
        })
        .filter_map(|parent| source.node(&parent))
        .any(|node| desk_deferred(node, now.naive_local()));
        if ancestor_deferred {
            return None;
        }
        let breadcrumb = tree_breadcrumb(&heading.id, source);
        let room = breadcrumb.split(" › ").next().map(str::to_owned);
        let bindings = source
            .children(&heading.id)
            .filter_map(|node| node.agent())
            .collect::<Vec<_>>();
        Some(HeadingContext {
            heading: heading.id.clone(),
            breadcrumb,
            room,
            bindings,
        })
    }

    /// The cards a note's own dated marks make. The topic is the note.
    fn desk_cards(
        &self,
        host: HostId,
        heading: &crate::desk_view::DeskNode,
        context: &HeadingContext,
        order: usize,
        facts: &DealerFacts<'_>,
    ) -> Vec<RankedDealCard> {
        let now = facts.now;
        let mut cards = Vec::new();
        for (mark, at) in desk_marks(heading) {
            let priority = desk_mark_priority(mark, at, heading.pace_days, now.naive_local());
            if priority <= DEAL_QUEUE_FLOOR {
                continue;
            }
            let identity = DealCardId {
                host,
                node_id: heading.id.clone(),
            };
            cards.push(RankedDealCard {
                priority,
                heading: None,
                virtual_reply: false,
                order,
                cursor: CardCursor::Desk(mark, at),
                curve: PriorityCurve::DeskMark {
                    mark,
                    at,
                    pace_days: heading.pace_days,
                },
                card: DealCard {
                    label: desk_mark_label(mark, at, now.naive_local()),
                    priority,
                    host,
                    topic_node_id: heading.id.clone(),
                    agent_id: context.bindings.first().copied(),
                    agent_tag: None,
                    breadcrumb: context.breadcrumb.clone(),
                    room: context.room.clone(),
                    kind: DealCardKind::Desk,
                    identity,
                    skipped: false,
                },
            });
        }
        cards
    }

    /// One agent's card, wherever it is shown from. `carded` records that
    /// the agent has been accounted for, verdict or card, so the pass over
    /// unfiled agents does not deal it twice.
    fn agent_card(
        &self,
        host: HostId,
        source: &TreeHostSource,
        agent_id: AgentId,
        context: &HeadingContext,
        order: usize,
        facts: &DealerFacts<'_>,
        carded: &mut HashSet<AgentId>,
    ) -> Option<RankedDealCard> {
        let breadcrumb = context.breadcrumb.as_str();
        let room = context.room.as_ref();
        let agent = facts.by_agent.get(&agent_id).copied()?;
        // The user's verdict on the agent closes its card wherever the
        // card is shown; being reached through a note does not exempt it.
        if agent_node_closed(source, agent_id, facts.now) {
            carded.insert(agent_id);
            return None;
        }
        let (priority, label) =
            agent_card_facts(&agent.facts, agent_id, facts.now, facts.interactions)?;
        carded.insert(agent_id);
        // Every agent is its own topic. Taking the note as the topic made
        // two agents filed under one note compete for a single card, so all
        // but the loudest disappeared from Home; a spawned child with no
        // row of its own shared its parent's identity as well.
        let node_id = source
            .agent_node(agent_id)
            .map(|node| node.id.clone())
            .unwrap_or(rho_desk::cells::Id::Agent(agent_id));
        Some(RankedDealCard {
            priority,
            heading: Some(context.heading.clone()),
            virtual_reply: true,
            order,
            cursor: CardCursor::Agent(agent.facts, agent.attention),
            curve: PriorityCurve::AgentReply { agent_id },
            card: DealCard {
                label,
                priority,
                host,
                topic_node_id: node_id.clone(),
                agent_id: Some(agent_id),
                agent_tag: None,
                breadcrumb: breadcrumb.to_owned(),
                room: room.cloned(),
                kind: DealCardKind::Agent,
                skipped: false,
                identity: DealCardId { host, node_id },
            },
        })
    }

    /// Every agent a heading holds: the ones filed under it and, through
    /// them, the ones they spawned.
    fn agent_cards_under(
        &self,
        host: HostId,
        source: &TreeHostSource,
        context: &HeadingContext,
        order: usize,
        facts: &DealerFacts<'_>,
        carded: &mut HashSet<AgentId>,
    ) -> Vec<RankedDealCard> {
        let mut cards = Vec::new();
        for root_agent in &context.bindings {
            let mut agents = vec![*root_agent];
            let mut cursor = 0;
            while cursor < agents.len() {
                let parent = agents[cursor];
                agents.extend(facts.spawned.get(&parent).into_iter().flatten().copied());
                cursor += 1;
            }
            for agent_id in agents {
                if let Some(card) =
                    self.agent_card(host, source, agent_id, context, order, facts, carded)
                {
                    cards.push(card);
                }
            }
        }
        cards
    }

    /// A Slack unit that started to matter is a row like any other, so it
    /// ranks in the same queue. A conversation is a unit as much as a
    /// followed thread is: a direct message and a channel somebody named
    /// the user in each deal as one card. What the card says comes from the
    /// mirror; the store holds the unit's identity and its verdicts, never
    /// its words.
    fn thread_card(
        &self,
        host: HostId,
        node: &crate::desk_view::DeskNode,
        order: usize,
        facts: &DealerFacts<'_>,
    ) -> Option<RankedDealCard> {
        if node.state != rho_desk::cells::State::Open
            || desk_deferred(node, facts.now.naive_local())
        {
            return None;
        }
        let thread = node.slack().and_then(|unit| facts.threads.get(unit))?;
        let (label, priority) = thread_card_facts(thread, facts.now);
        if priority <= DEAL_QUEUE_FLOOR {
            return None;
        }
        Some(RankedDealCard {
            priority,
            heading: None,
            virtual_reply: false,
            order,
            cursor: CardCursor::Slack(rho_desk::cells::SlackTs(thread.latest.clone())),
            curve: PriorityCurve::Thread,
            card: DealCard {
                label,
                priority,
                host,
                topic_node_id: node.id.clone(),
                agent_id: None,
                agent_tag: None,
                breadcrumb: thread.title.clone(),
                room: Some(thread.conversation.clone()),
                kind: DealCardKind::Thread,
                skipped: false,
                identity: DealCardId {
                    host,
                    node_id: node.id.clone(),
                },
            },
        })
    }

    /// An agent nobody filed is a card all the same: what makes one is the
    /// agent asking for the user, and filing is only the user's own
    /// labelling and placement. This is also the whole of Home before a
    /// daemon answers, when the client has its mirror of the agents and no
    /// Desk yet: the card ranks at the root, with no breadcrumb.
    fn loose_agent_card(
        &self,
        agent: &DealAgentFacts,
        order: usize,
        facts: &DealerFacts<'_>,
    ) -> Option<RankedDealCard> {
        let node = self
            .tree_hosts
            .get(&agent.host)
            .and_then(|source| source.agent_node(agent.agent_id));
        // A handled, muted or deferred agent is the user's verdict on this
        // very card; without a Desk there is no verdict to read.
        if node.is_some_and(|node| node_closed(node, facts.now)) {
            return None;
        }
        let (priority, label) =
            agent_card_facts(&agent.facts, agent.agent_id, facts.now, facts.interactions)?;
        let node_id = node
            .map(|node| node.id.clone())
            .unwrap_or(rho_desk::cells::Id::Agent(agent.agent_id));
        let identity = DealCardId {
            host: agent.host,
            node_id: node_id.clone(),
        };
        Some(RankedDealCard {
            priority,
            heading: None,
            virtual_reply: true,
            order,
            cursor: CardCursor::Agent(agent.facts, agent.attention),
            curve: PriorityCurve::AgentReply {
                agent_id: agent.agent_id,
            },
            card: DealCard {
                label,
                priority,
                host: agent.host,
                topic_node_id: node_id,
                agent_id: Some(agent.agent_id),
                agent_tag: None,
                breadcrumb: String::new(),
                room: None,
                kind: DealCardKind::Agent,
                skipped: false,
                identity,
            },
        })
    }

    /// One winning card per topic, in priority order, with the skips
    /// marked. This is the shaping every reading of the ranking ends with,
    /// whether the candidates were all made again or only one of them was.
    fn deal_queue_from(
        &self,
        mut candidates: Vec<RankedDealCard>,
        now: chrono::DateTime<chrono::FixedOffset>,
        agent_interactions: &HashMap<AgentId, i64>,
    ) -> DealQueue {
        // Kept cards were scored when they were made. Waiting is what moves
        // them after that, and this is where the clock is applied: a card
        // the curve has taken under the floor leaves here, and none of it
        // needs the desk.
        candidates.retain_mut(|candidate| candidate.rescore(now, agent_interactions));
        // One winning card per topic; a virtual reply wins an exact tie.
        let mut by_topic = HashMap::new();
        for candidate in candidates {
            let topic = (candidate.card.host, candidate.card.topic_node_id.clone());
            by_topic
                .entry(topic)
                .and_modify(|old: &mut RankedDealCard| {
                    if candidate.priority > old.priority
                        || (candidate.priority == old.priority
                            && candidate.virtual_reply
                            && !old.virtual_reply)
                    {
                        *old = candidate.clone();
                    }
                })
                .or_insert(candidate);
        }
        // A skipped card is marked, never dropped: it is still open, so
        // Home shows it and says so, and only the next pull passes over it.
        let mut ranked = by_topic
            .into_values()
            .map(|mut ranked| {
                ranked.card.skipped = self.skipped.get(&ranked.card.identity).is_some_and(|skip| {
                    now < skip.at + SKIP_COOLDOWN && skip.cursor == ranked.cursor
                });
                ranked
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|a, b| {
            b.priority
                .total_cmp(&a.priority)
                .then_with(|| b.virtual_reply.cmp(&a.virtual_reply))
                .then_with(|| a.order.cmp(&b.order))
                // Last, the identity: two cards that tie on everything else
                // must still come out in the same order every read, and a
                // set has no insertion order to fall back on.
                .then_with(|| a.card.identity.cmp(&b.card.identity))
        });
        DealQueue {
            total_alive: ranked.len(),
            dealt_count: usize::from(!ranked.is_empty()),
            cursors: ranked
                .iter()
                .map(|ranked| (ranked.card.identity.clone(), ranked.cursor.clone()))
                .collect(),
            cards: ranked.into_iter().map(|ranked| ranked.card).collect(),
        }
    }

    /// A card for a node the ranking no longer holds. Reading a thing can
    /// quiet it, and the verdict on what is still on screen has to land
    /// anyway; it is the same node, so it is the same card.
    pub fn card_for_node(
        &self,
        host: HostId,
        node_id: rho_desk::cells::Id,
        _cx: &App,
    ) -> Option<DealCard> {
        let source = self.tree_hosts.get(&host)?;
        let node = source.nodes.iter().find(|node| node.id == node_id)?;
        let kind = match (node.slack().is_some(), node_agent(node)) {
            (true, _) => DealCardKind::Thread,
            (false, Some(_)) => DealCardKind::Agent,
            (false, None) => DealCardKind::Desk,
        };
        Some(DealCard {
            label: String::new(),
            priority: 0.,
            host,
            topic_node_id: node_id.clone(),
            agent_id: node_agent(node),
            agent_tag: None,
            breadcrumb: self.breadcrumb_for_node(host, node_id.clone())?,
            room: self
                .room_for_node(host, node_id.clone())
                .map(|room| room.name),
            kind,
            skipped: false,
            identity: DealCardId { host, node_id },
        })
    }

    fn breadcrumb_for_node(&self, host: HostId, node_id: rho_desk::cells::Id) -> Option<String> {
        let source = self.tree_hosts.get(&host)?;
        Some(tree_breadcrumb(&node_id, source))
    }

    fn room_for_node(&self, host: HostId, mut node_id: rho_desk::cells::Id) -> Option<DeskRoom> {
        let source = self.tree_hosts.get(&host)?;
        loop {
            let node = source.nodes.iter().find(|node| node.id == node_id)?;
            let Some(ref parent) = node.parent else { break };
            let parent_node = source.nodes.iter().find(|node| node.id == *parent)?;
            if !parent_node.is_note() {
                break;
            }
            node_id = parent.clone();
        }
        let name = source.title(&node_id)?.to_owned();
        Some(DeskRoom {
            host,
            node_id,
            name,
        })
    }

    pub fn cursor_room(&self, cx: &mut Context<Workspace>) -> Option<DeskRoom> {
        let (host, node_id) = self.cursor_topic(cx)?;
        self.room_for_node(host, node_id)
    }

    pub fn cursor_breadcrumb(&self, cx: &mut Context<Workspace>) -> Option<String> {
        let (host, node_id) = self.cursor_topic(cx)?;
        self.breadcrumb_for_node(host, node_id)
    }

    pub fn breadcrumb_for_agent(&self, agent_id: AgentId, _cx: &App) -> Option<String> {
        let (host, node_id) = self
            .tree_heading_agents
            .iter()
            .find_map(|(topic, agents)| agents.contains(&agent_id).then_some(topic.clone()))?;
        self.breadcrumb_for_node(host, node_id)
    }

    pub fn breadcrumb_for_page(&self, page_id: rho_browser::PageId, _cx: &App) -> Option<String> {
        let (host, node_id) = self
            .tree_heading_pages
            .iter()
            .find_map(|(topic, pages)| pages.contains(&page_id).then_some(topic.clone()))?;
        self.breadcrumb_for_node(host, node_id)
    }

    pub fn room_for_agent(&self, agent_id: AgentId, _cx: &App) -> Option<DeskRoom> {
        let (host, node_id) = self
            .tree_heading_agents
            .iter()
            .find_map(|(topic, agents)| agents.contains(&agent_id).then_some(topic.clone()))?;
        self.room_for_node(host, node_id)
    }

    pub fn room_for_page(&self, page_id: rho_browser::PageId, _cx: &App) -> Option<DeskRoom> {
        let (host, node_id) = self
            .tree_heading_pages
            .iter()
            .find_map(|(topic, pages)| pages.contains(&page_id).then_some(topic.clone()))?;
        self.room_for_node(host, node_id)
    }

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
            tree_hosts: BTreeMap::new(),
            composition: Composition::default(),
            element_keys: HashMap::new(),
            tree_element_keys: HashMap::new(),
            tree_heading_agents: HashMap::new(),
            tree_heading_pages: HashMap::new(),
            next_element_key: 0,
            targets: HashMap::new(),
            referenced_pages: HashSet::new(),
            new_draft: None,
            tree_new_draft_parent: None,
            raw_mode: false,
            phone_browse_mode: false,
            skipped: HashMap::new(),
            queue_depth: DealQueueDepth::default(),
            dealer: DealerSet::default(),
            pending_cursor: None,
            tree_inlay_ids: Vec::new(),
            tree_draw: Vec::new(),
            tree_draw_at: HashMap::new(),
            #[cfg(test)]
            composed: 0,
            #[cfg(test)]
            redrawn: 0,
            derived_titles: HashMap::new(),
            tree_collapsed: HashSet::new(),
            pending_tree_cursor: None,
            headers_disabled: std::collections::HashSet::new(),
        }
    }

    /// Registers every current buffer (rows and Desk documents) as
    /// headerless with the editor, so excerpt boundaries draw no divider.
    fn ensure_headerless(&mut self, cx: &mut Context<Workspace>) {
        let new_ids = self
            .buffers
            .values()
            .chain(
                self.tree_hosts
                    .values()
                    .flat_map(|host| host.buffers.values()),
            )
            .map(|buffer| buffer.read(cx).remote_id())
            .filter(|id| !self.headers_disabled.contains(id))
            .collect::<Vec<_>>();
        // One call, not one per buffer: each of these resyncs the display
        // map, so a build of n rows cost n block-map syncs of n rows.
        self.editor.update(cx, |editor, cx| {
            editor.disable_headers_for_buffers(new_ids.iter().copied(), cx);
        });
        self.headers_disabled.extend(new_ids);
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }

    pub fn raw_mode(&self) -> bool {
        self.raw_mode
    }

    pub fn set_phone_browse_mode(&mut self, enabled: bool) -> bool {
        if self.phone_browse_mode == enabled {
            return false;
        }
        self.phone_browse_mode = enabled;
        true
    }

    pub fn page_ids(&self) -> HashSet<rho_browser::PageId> {
        self.referenced_pages.clone()
    }

    pub fn is_focused(&self, window: &Window, cx: &App) -> bool {
        self.focus_handle(cx).is_focused(window)
    }

    pub fn set_tree_source(
        &mut self,
        host: HostId,
        nodes: Vec<crate::desk_view::DeskNode>,
        buffers: BTreeMap<rho_desk::cells::Id, Entity<Buffer>>,
        titles: std::rc::Rc<HashMap<rho_desk::cells::Id, String>>,
        cx: &mut Context<Workspace>,
    ) {
        if self.pending_tree_cursor.is_none()
            && let Some((cursor_host, node_id, offset)) = self.tree_node_cursor_offset(cx)
            && cursor_host == host
            && self
                .tree_hosts
                .get(&host)
                .and_then(|source| source.buffers.get(&node_id))
                != buffers.get(&node_id)
            && buffers.contains_key(&node_id)
        {
            self.pending_tree_cursor = Some((host, node_id, offset));
        }
        let index = TreeIndex::build(&nodes);
        self.tree_hosts.insert(
            host,
            TreeHostSource {
                nodes,
                buffers,
                titles,
                index,
            },
        );
    }

    /// What a card's node is, which decides the surface it opens.
    pub fn card_target(&self, card: DealCardId) -> CardTarget {
        let Some(node) = self
            .tree_hosts
            .get(&card.host)
            .and_then(|source| source.nodes.iter().find(|node| node.id == card.node_id))
        else {
            // An unfiled agent is a card with no node behind it, and it
            // still opens its transcript: the id says which agent, and
            // filing was never what made it openable.
            return match card.node_id {
                rho_desk::cells::Id::Agent(agent_id) => CardTarget::Agent(agent_id),
                _ => CardTarget::Missing,
            };
        };
        match &node.id {
            rho_desk::cells::Id::Agent(_) => {
                node.agent().map_or(CardTarget::Missing, CardTarget::Agent)
            }
            rho_desk::cells::Id::Page(_) => {
                node_page(node).map_or(CardTarget::Missing, CardTarget::Page)
            }
            rho_desk::cells::Id::Slack(_) => {
                node_unit(node).map_or(CardTarget::Missing, CardTarget::Thread)
            }
            _ => CardTarget::Note,
        }
    }

    /// The card a thing on screen would be dealt as, so the dealer can stay
    /// quiet about what the user is already looking at.
    pub fn agent_card_id(&self, agent_id: AgentId) -> Option<DealCardId> {
        self.node_card(|node| node.agent() == Some(agent_id))
    }

    pub fn page_card_id(&self, page: rho_browser::PageId) -> Option<DealCardId> {
        self.node_card(|node| node_page(node) == Some(page))
    }

    pub fn thread_card_id(&self, thread: &SlackUnit) -> Option<DealCardId> {
        self.node_card(|node| node_unit(node).as_ref() == Some(thread))
    }

    /// The thread a card stands for, if it is a thread card at all.
    pub fn card_thread(&self, card: DealCardId) -> Option<SlackUnit> {
        self.tree_hosts
            .get(&card.host)?
            .nodes
            .iter()
            .find(|node| node.id == card.node_id)
            .and_then(node_unit)
    }

    /// Every open Slack thread node, with the thread it stands for. The
    /// backlog command needs them all at once rather than the one the
    /// cursor is on.
    pub fn open_thread_cards(&self) -> Vec<(DealCardId, SlackUnit)> {
        self.tree_hosts
            .iter()
            .flat_map(|(host, source)| {
                source
                    .nodes
                    .iter()
                    .filter(|node| node.state == rho_desk::cells::State::Open)
                    .filter_map(move |node| {
                        Some((
                            DealCardId {
                                host: *host,
                                node_id: node.id.clone(),
                            },
                            node_unit(node)?,
                        ))
                    })
            })
            .collect()
    }

    /// Whether a card's node still wants attention. A node a verdict
    /// closed is not re-dealt until something reopens it.
    pub fn node_is_open(&self, card: DealCardId) -> bool {
        self.tree_hosts
            .get(&card.host)
            .and_then(|source| source.nodes.iter().find(|node| node.id == card.node_id))
            .is_some_and(|node| node.state == rho_desk::cells::State::Open)
    }

    /// When a card is put down until, as the view derives it: a snooze a
    /// newer message has voided reads as no snooze at all.
    pub fn node_defer_until(&self, card: DealCardId) -> Option<rho_desk::cells::Timestamp> {
        self.tree_hosts
            .get(&card.host)
            .and_then(|source| source.nodes.iter().find(|node| node.id == card.node_id))
            .and_then(|node| node.defer_until)
    }

    fn node_card(
        &self,
        matches: impl Fn(&crate::desk_view::DeskNode) -> bool,
    ) -> Option<DealCardId> {
        self.tree_hosts.iter().find_map(|(host, source)| {
            source
                .nodes
                .iter()
                .find(|node| matches(node))
                .map(|node| DealCardId {
                    host: *host,
                    node_id: node.id.clone(),
                })
        })
    }

    pub fn tree_node_at_cursor(
        &self,
        cx: &mut Context<Workspace>,
    ) -> Option<(HostId, rho_desk::cells::Id)> {
        self.tree_node_cursor_offset(cx)
            .map(|(host, node_id, _)| (host, node_id))
    }

    pub fn tree_node_for_buffer(
        &self,
        buffer_id: BufferId,
        cx: &App,
    ) -> Option<(HostId, rho_desk::cells::Id)> {
        self.tree_hosts.iter().find_map(|(host, source)| {
            source.buffers.iter().find_map(|(node_id, buffer)| {
                (buffer.read(cx).remote_id() == buffer_id).then_some((*host, node_id.clone()))
            })
        })
    }

    pub fn first_tree_agent_for_topic(
        &self,
        topic: (HostId, rho_desk::cells::Id),
    ) -> Option<AgentId> {
        self.tree_heading_agents
            .get(&topic)
            .and_then(|agents| agents.first())
            .copied()
    }

    pub fn tree_node_cursor_offset(
        &self,
        cx: &mut Context<Workspace>,
    ) -> Option<(HostId, rho_desk::cells::Id, usize)> {
        let (buffer_id, offset) = self.editor.update(cx, |editor, cx| {
            let head = editor.selections.newest_anchor().head();
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            snapshot
                .anchor_to_buffer_anchor(head)
                .map(|(anchor, buffer)| (buffer.remote_id(), anchor.to_offset(buffer)))
        })?;
        self.tree_hosts.iter().find_map(|(host, source)| {
            source.buffers.iter().find_map(|(node_id, buffer)| {
                (buffer.read(cx).remote_id() == buffer_id).then_some((
                    *host,
                    node_id.clone(),
                    offset,
                ))
            })
        })
    }

    pub fn move_to_tree_node_when_ready(&mut self, host: HostId, node_id: rho_desk::cells::Id) {
        self.pending_tree_cursor = Some((host, node_id, 0));
    }

    pub fn move_to_tree_position_when_ready(
        &mut self,
        host: HostId,
        node_id: rho_desk::cells::Id,
        offset: usize,
    ) {
        self.pending_tree_cursor = Some((host, node_id, offset));
    }

    /// How many cards have been made in this dashboard's life.
    #[cfg(test)]
    pub(crate) fn cards_made_for_test(&self) -> usize {
        self.dealer.made
    }

    /// How many times the whole map has been composed, and how many rows
    /// have been drawn again where they sit.
    #[cfg(test)]
    pub(crate) fn map_work_for_test(&self) -> (usize, usize) {
        (self.composed, self.redrawn)
    }

    #[cfg(test)]
    pub fn deal_highlight_active_for_test(&self, cx: &App) -> bool {
        self.editor
            .read(cx)
            .highlighted_rows::<DealCardHighlight>(cx)
            .next()
            .is_some()
    }

    /// The ranking as it stands. Nothing is made here: the cards are
    /// already in the set, and the clock is applied to them on the way
    /// out. This is why Home, the lamp and the map's depth counter can
    /// each ask, as often as they like, without any of them walking a
    /// desk.
    pub fn dealer_hand(
        &self,
        now: chrono::DateTime<chrono::FixedOffset>,
        agent_interactions: &HashMap<AgentId, i64>,
    ) -> DealQueue {
        let candidates = self
            .dealer
            .cards
            .values()
            .flatten()
            .cloned()
            .collect::<Vec<_>>();
        self.deal_queue_from(candidates, now, agent_interactions)
    }

    /// Makes again the cards `scope` says have moved, and leaves the rest
    /// standing. `Whole` is a desk that arrived or changed shape;
    /// `Agents` is a `Changed`, which is the one that arrives in
    /// thousands and must cost what it names.
    pub fn refresh_deal_cards(
        &mut self,
        host: HostId,
        scope: DealScope<'_>,
        registry: &AgentRegistry,
        threads: &HashMap<SlackUnit, SlackFacts>,
        now: chrono::DateTime<chrono::FixedOffset>,
        agent_interactions: &HashMap<AgentId, i64>,
    ) {
        match scope {
            DealScope::Whole => {
                self.rebuild_host_cards(host, registry, threads, now, agent_interactions)
            }
            DealScope::Agents(agents) => {
                for agent_id in agents {
                    self.remake_agent_card(host, *agent_id, registry, now, agent_interactions);
                }
            }
            DealScope::Nodes(nodes) => {
                for node_id in nodes {
                    self.remake_node_cards(
                        host,
                        node_id,
                        registry,
                        threads,
                        now,
                        agent_interactions,
                    );
                }
            }
        }
    }

    /// The rows a delta named, drawn again where they sit. Nothing is
    /// composed: the excerpts, the folds and the highlights are the same
    /// rows in the same order, so what one verdict moves is the hint at
    /// the end of its row, the marker in front of it, and the title of a
    /// machine row. Answers false when a row the delta names was never
    /// drawn, which is the caller's cue that the shape moved after all.
    pub fn redraw_tree_rows(
        &mut self,
        host: HostId,
        touched: &BTreeSet<rho_desk::cells::Id>,
        nodes: &[crate::desk_view::DeskNode],
        registry: &AgentRegistry,
        threads: &HashMap<SlackUnit, SlackFacts>,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let Some(source) = self.tree_hosts.get_mut(&host) else {
            return false;
        };
        // The map the dashboard holds is the desk's, one step behind: the
        // rows the delta named are copied across, and nothing else is read.
        for id in touched {
            let Some(at) = source.index.by_id.get(id).copied() else {
                return false;
            };
            let Some(fresh) = nodes.get(at) else {
                return false;
            };
            if fresh.id != *id {
                return false;
            }
            source.nodes[at] = fresh.clone();
        }
        let mut remove = Vec::new();
        let mut insert = Vec::new();
        let mut hints_moved = false;
        let mut titles = Vec::new();
        for id in touched {
            let Some(draws) = self.tree_draw_at.get(&(host, id.clone())) else {
                return false;
            };
            for at in draws.clone() {
                #[cfg(test)]
                {
                    self.redrawn += 1;
                }
                let Some(node) = self
                    .tree_hosts
                    .get(&host)
                    .and_then(|source| source.node(id))
                    .cloned()
                else {
                    return false;
                };
                let draw = &self.tree_draw[at];
                if draw.hidden_by_fold {
                    continue;
                }
                let hint = row_hint(&node);
                if hint != draw.hint {
                    hints_moved = true;
                    self.tree_draw[at].hint = hint;
                }
                // A machine row's words are derived, so a verdict that
                // moves them moves the row's own text.
                if !is_note(&node)
                    && let Some(buffer) = self
                        .tree_hosts
                        .get(&host)
                        .and_then(|source| source.buffers.get(id))
                        .cloned()
                {
                    let title = derived_title(&node, registry, threads);
                    let key = (host, id.clone());
                    if self.derived_titles.get(&key) != Some(&(buffer.entity_id(), title.clone())) {
                        self.derived_titles
                            .insert(key, (buffer.entity_id(), title.clone()));
                        titles.push((buffer, title));
                    }
                }
                // The marker in front of an agent row is its attention and
                // its name, and both can move without the map moving.
                let draw = &self.tree_draw[at];
                if let Some(agent_id) = node.agent() {
                    let prefix = agent_prefix(&draw.prefix, agent_id, registry);
                    if prefix != draw.prefix {
                        remove.extend(draw.inlays.iter().copied());
                        let padding = " ".repeat(prefix.chars().count());
                        let mut ids = Vec::with_capacity(draw.inlays.len());
                        let head = Inlay::custom(
                            TREE_INLAY_ID_BASE + draw.at * 2,
                            draw.start,
                            prefix.clone(),
                        );
                        ids.push(head.id);
                        insert.push(head);
                        for (line, _) in draw.inlays.iter().skip(1).enumerate() {
                            let inlay = Inlay::custom(
                                CONTINUATION_INLAY_ID_BASE + draw.at * 256 + line,
                                draw.start,
                                padding.clone(),
                            );
                            ids.push(inlay.id);
                            insert.push(inlay);
                        }
                        self.tree_draw[at].prefix = prefix;
                        self.tree_draw[at].inlays = ids;
                    }
                }
            }
        }
        for (buffer, title) in titles {
            crate::desk_view::write_derived_title(&buffer, &title, cx);
        }
        if !remove.is_empty() || !insert.is_empty() {
            self.tree_inlay_ids.retain(|id| !remove.contains(id));
            self.tree_inlay_ids
                .extend(insert.iter().map(|inlay| inlay.id));
            self.editor
                .update(cx, |editor, cx| editor.splice_inlays(&remove, insert, cx));
        }
        if hints_moved {
            // The editor takes its hints as a set, so the ones that did not
            // move are handed back as they were. Nothing is measured or
            // anchored again: this is the kept drawing, read out.
            let hints = self
                .tree_draw
                .iter()
                .filter_map(|draw| {
                    let text = draw.hint.clone()?;
                    Some((draw.end, eol_hint(text)))
                })
                .collect::<Vec<_>>();
            self.editor
                .update(cx, |editor, cx| editor.set_eol_hints(hints, cx));
        }
        true
    }

    /// A row and everything drawn under it. A note's words are the head of
    /// every breadcrumb below it, so an edit to them moves the cards of its
    /// subtree and of nothing outside it.
    pub fn subtree_ids(&self, host: HostId, id: &rho_desk::cells::Id) -> Vec<rho_desk::cells::Id> {
        let Some(source) = self.tree_hosts.get(&host) else {
            return Vec::new();
        };
        let mut ids = vec![id.clone()];
        let mut cursor = 0;
        while cursor < ids.len() {
            let at = ids[cursor].clone();
            cursor += 1;
            ids.extend(source.children(&at).map(|node| node.id.clone()));
        }
        ids
    }

    /// The cards of one row, made again. What it costs is the row and the
    /// agents it heads: a note that was deferred or closed stops lending
    /// its subtree a place in the hand, and that is the whole of what one
    /// verdict on a heading can move.
    fn remake_node_cards(
        &mut self,
        host: HostId,
        node_id: &rho_desk::cells::Id,
        registry: &AgentRegistry,
        threads: &HashMap<SlackUnit, SlackFacts>,
        now: chrono::DateTime<chrono::FixedOffset>,
        agent_interactions: &HashMap<AgentId, i64>,
    ) {
        let identity = DealCardId {
            host,
            node_id: node_id.clone(),
        };
        self.dealer.retire(&identity);
        let Some(source) = self.tree_hosts.get(&host) else {
            return;
        };
        let Some(order) = source.index.by_id.get(node_id).copied() else {
            return;
        };
        let node = source.nodes[order].clone();
        // The row's own cards need no agent facts: a dated mark is the
        // note's, and a thread's wait is the mirror's.
        let facts = DealerFacts {
            by_agent: HashMap::new(),
            spawned: HashMap::new(),
            threads,
            now,
            interactions: agent_interactions,
        };
        let made = if node.is_note() {
            match self.heading_context(source, &node, now) {
                Some(context) => self.desk_cards(host, &node, &context, order, &facts),
                None => Vec::new(),
            }
        } else if node.slack().is_some() {
            self.thread_card(host, &node, order, &facts)
                .into_iter()
                .collect()
        } else {
            Vec::new()
        };
        for card in made {
            self.dealer.insert(card);
        }
        // Whoever the row lends a place to: the agents filed under it, and
        // the ones already carded there, which is where a spawned agent
        // reached through its parent's filing shows up.
        let mut agents = self
            .tree_hosts
            .get(&host)
            .into_iter()
            .flat_map(|source| source.children(node_id))
            .filter_map(|child| child.agent())
            .collect::<HashSet<_>>();
        agents.extend(
            self.dealer
                .of_heading
                .get(&(host, node_id.clone()))
                .into_iter()
                .flatten()
                .copied(),
        );
        for agent_id in agents {
            self.remake_agent_card(host, agent_id, registry, now, agent_interactions);
        }
    }

    /// Every card a host holds, made again. This is the only pass over a
    /// host's nodes left, and only a desk that changed shape asks for it.
    fn rebuild_host_cards(
        &mut self,
        host: HostId,
        registry: &AgentRegistry,
        threads: &HashMap<SlackUnit, SlackFacts>,
        now: chrono::DateTime<chrono::FixedOffset>,
        agent_interactions: &HashMap<AgentId, i64>,
    ) {
        for id in self.dealer.of_host.remove(&host).unwrap_or_default() {
            self.dealer.cards.remove(&id);
        }
        self.dealer.of_agent.retain(|_, id| id.host != host);
        let Some(source) = self.tree_hosts.get(&host) else {
            return;
        };
        let agents = deal_agent_facts(registry);
        let facts = self.dealer_facts(threads, now, agent_interactions, &agents);
        let mut made = Vec::new();
        // Filing decides where a card is shown, never whether it exists, so
        // an agent reached through a note is only skipped by the pass below
        // to keep it from being carded twice.
        let mut carded = HashSet::new();
        for (order, node) in source.nodes.iter().enumerate() {
            if node.is_note() {
                let Some(context) = self.heading_context(source, node, now) else {
                    continue;
                };
                made.extend(self.desk_cards(host, node, &context, order, &facts));
                made.extend(self.agent_cards_under(
                    host,
                    source,
                    &context,
                    order,
                    &facts,
                    &mut carded,
                ));
            } else if node.slack().is_some() {
                made.extend(self.thread_card(host, node, order, &facts));
            }
        }
        for agent in &agents {
            if agent.host != host || carded.contains(&agent.agent_id) {
                continue;
            }
            made.extend(self.loose_agent_card(
                agent,
                self.agent_order(host, agent.agent_id),
                &facts,
            ));
        }
        for card in made {
            self.dealer.insert(card);
        }
    }

    /// One agent's card, made again because that agent moved. Nothing else
    /// on the desk is touched, and nothing is walked: the agent's place is
    /// a lookup, and the note it hangs under is found by walking up its own
    /// spawn chain, which is as deep as the agent is.
    fn remake_agent_card(
        &mut self,
        host: HostId,
        agent_id: AgentId,
        registry: &AgentRegistry,
        now: chrono::DateTime<chrono::FixedOffset>,
        agent_interactions: &HashMap<AgentId, i64>,
    ) {
        if let Some(id) = self.dealer.of_agent.remove(&agent_id) {
            self.dealer.retire(&id);
        }
        let Some(agent) = agent_deal_facts(registry, agent_id) else {
            return;
        };
        if agent.host != host {
            return;
        }
        let facts = DealerFacts {
            by_agent: std::iter::once((agent_id, &agent)).collect(),
            spawned: HashMap::new(),
            threads: &EMPTY_THREADS,
            now,
            interactions: agent_interactions,
        };
        let order = self.agent_order(host, agent_id);
        let mut carded = HashSet::new();
        // Where the card is shown: the first note in the map's order that
        // reaches this agent, through its own filing or through whoever
        // spawned it. A note that is closed or waiting reaches nothing, and
        // a host with no desk yet reaches nothing either, which is what
        // makes the card a loose one at the root.
        let context = self
            .tree_hosts
            .get(&host)
            .and_then(|source| self.heading_for_agent(source, agent_id, registry, now));
        let card = match (context, self.tree_hosts.get(&host)) {
            (Some(context), Some(source)) => {
                self.agent_card(host, source, agent_id, &context, order, &facts, &mut carded)
            }
            _ => self.loose_agent_card(&agent, order, &facts),
        };
        if let Some(card) = card {
            self.dealer.insert(card);
        }
    }

    /// The tie-break an agent's card carries: where its row sits in the
    /// map, or after everything when it has no row.
    fn agent_order(&self, host: HostId, agent_id: AgentId) -> usize {
        self.tree_hosts
            .get(&host)
            .and_then(|source| source.index.by_agent.get(&agent_id).copied())
            .unwrap_or(usize::MAX)
    }

    /// The note an agent's card hangs under: the earliest in the map's
    /// order among its own filing and the filings of everyone who spawned
    /// it. `None` when nothing live reaches it, which makes it a loose
    /// card at the root.
    fn heading_for_agent(
        &self,
        source: &TreeHostSource,
        agent_id: AgentId,
        registry: &AgentRegistry,
        now: chrono::DateTime<chrono::FixedOffset>,
    ) -> Option<HeadingContext> {
        let mut best: Option<(usize, &crate::desk_view::DeskNode)> = None;
        let mut cursor = Some(agent_id);
        let mut guard = 0;
        while let Some(agent) = cursor {
            guard += 1;
            if guard > 64 {
                break;
            }
            if let Some(node) = source.agent_node(agent)
                && let Some(parent) = &node.parent
                && let Some(heading) = source.node(parent)
                && heading.is_note()
                && let Some(at) = source.index.by_id.get(&heading.id).copied()
                && best.is_none_or(|(held, _)| at < held)
            {
                best = Some((at, heading));
            }
            cursor = registry.agent_parent(agent);
        }
        let (_, heading) = best?;
        self.heading_context(source, heading, now)
    }

    /// When the ranking changes next without anybody touching anything: a
    /// deferral ripening, a deadline arriving, a skip's cooldown running
    /// out. A priority also slides with the clock between these, which is
    /// why the caller keeps a ceiling of its own; this is what says the
    /// wake cannot be later than.
    pub fn next_deal_expiry(
        &self,
        now: chrono::DateTime<chrono::FixedOffset>,
    ) -> Option<chrono::TimeDelta> {
        let now = now.naive_local();
        let mut soonest: Option<chrono::NaiveDateTime> = None;
        let mut consider = |at: chrono::NaiveDateTime| {
            if at > now && soonest.is_none_or(|held| at < held) {
                soonest = Some(at);
            }
        };
        for source in self.tree_hosts.values() {
            for node in &source.nodes {
                for at in [node.defer_until, node.deadline].into_iter().flatten() {
                    if let Some(at) = desk_time(at) {
                        consider(at);
                    }
                }
            }
        }
        for skip in self.skipped.values() {
            consider((skip.at + SKIP_COOLDOWN).naive_local());
        }
        soonest.map(|at| at - now)
    }

    /// What the timeline records about a verdict. There is no session to
    /// ask: the card is the one the reader was on, and the caller has it.
    pub fn record_verdict(
        &mut self,
        card: &DealCard,
        verdict: DealerVerdict,
        now: chrono::DateTime<chrono::FixedOffset>,
    ) {
        let skip_until = (verdict == DealerVerdict::Skip).then(|| now + SKIP_COOLDOWN);
        self.record_dealer_event(DealerEvent {
            card: card.identity.clone(),
            kind: card.kind,
            verdict,
            at: now,
            skip_until,
        });
    }

    pub fn record_dealer_event(&mut self, event: DealerEvent) {
        let verdict = event.verdict;
        if verdict != DealerVerdict::Skip {
            self.skipped.remove(&event.card);
        }
        fn identity(card: &DealCardId) -> crate::journal::DealerCardIdentity {
            crate::journal::DealerCardIdentity {
                host: card.host.0,
                node_id: card.node_id.clone().into(),
            }
        }
        let kind = match event.kind {
            DealCardKind::Desk => crate::journal::DealerCardKind::Note,
            DealCardKind::Agent => crate::journal::DealerCardKind::Agent,
            DealCardKind::Thread => crate::journal::DealerCardKind::Thread,
        };
        let verdict = match event.verdict {
            DealerVerdict::Skip => crate::journal::DealerVerdict::Skip,
            DealerVerdict::Done => crate::journal::DealerVerdict::Done,
            DealerVerdict::Mute => crate::journal::DealerVerdict::Mute,
            DealerVerdict::Defer => crate::journal::DealerVerdict::Defer,
            DealerVerdict::Open => crate::journal::DealerVerdict::Open,
            DealerVerdict::File => crate::journal::DealerVerdict::File,
        };
        crate::journal::record(crate::journal::Event::Dealer {
            card: identity(&event.card),
            kind,
            verdict,
            occurred_at: event.at.to_rfc3339(),
            skip_until: event.skip_until.map(|until| until.to_rfc3339()),
        });
    }

    /// Passes over a card: the next pull opens something else until the
    /// cooldown runs out or the card's source moves past where it stood.
    /// The card stays open the whole time, and Home says it was skipped.
    pub fn skip_card(
        &mut self,
        card: &DealCard,
        cursor: CardCursor,
        now: chrono::DateTime<chrono::FixedOffset>,
    ) {
        self.skipped
            .insert(card.identity.clone(), SkippedCard { at: now, cursor });
        self.record_verdict(card, DealerVerdict::Skip, now);
    }

    pub fn clear_skip(&mut self, identity: &DealCardId) -> bool {
        self.skipped.remove(identity).is_some()
    }

    #[cfg(test)]
    pub fn has_skip_for_test(&self, identity: &DealCardId) -> bool {
        self.skipped.contains_key(identity)
    }

    /// The room a card belongs to: the note it hangs under, walked up to
    /// the outermost note, which is what `shift-s` snoozes.
    pub fn tree_room_node(&self, card: &DealCard) -> Option<(HostId, rho_desk::cells::Id)> {
        let source = self.tree_hosts.get(&card.host)?;
        let mut node_id = card.topic_node_id.clone();
        loop {
            let node = source.nodes.iter().find(|node| node.id == node_id)?;
            let Some(ref parent) = node.parent else {
                return Some((card.host, node_id));
            };
            let parent_node = source.nodes.iter().find(|node| node.id == *parent)?;
            if !parent_node.is_note() {
                return Some((card.host, node_id));
            }
            node_id = parent.clone();
        }
    }

    /// Opens (or returns to) the inline new-agent draft. Like a reply
    /// draft it parks when left and survives refreshes.
    pub fn open_new_draft(
        &mut self,
        topic: Option<(HostId, rho_desk::cells::Id)>,
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
                .insert(LineKey::NewDraft(topic.clone()), buffer.clone());
            self.new_draft = Some((topic.clone(), buffer, subscription));
        }
        let topic = self
            .new_draft
            .as_ref()
            .map(|draft| draft.0.clone())
            .unwrap_or(topic);
        self.pending_cursor = Some(LineKey::NewDraft(topic));
        cx.notify();
    }

    pub fn open_new_tree_draft(
        &mut self,
        topic: (HostId, rho_desk::cells::Id),
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        self.tree_new_draft_parent = Some(topic.clone());
        self.open_new_draft(Some(topic), window, cx);
    }

    /// Takes the new-agent draft's text and closes it. `None` when empty.
    pub fn take_new_draft(&mut self, cx: &mut Context<Workspace>) -> Option<String> {
        let (topic, buffer, _) = self.new_draft.take()?;
        self.tree_new_draft_parent = None;
        let text = buffer.read(cx).text().trim().to_owned();
        self.buffers.remove(&LineKey::NewDraft(topic));
        cx.notify();
        (!text.is_empty()).then_some(text)
    }

    pub fn discard_new_draft(&mut self, cx: &mut Context<Workspace>) -> bool {
        let Some((topic, _, _)) = self.new_draft.take() else {
            return false;
        };
        self.tree_new_draft_parent = None;
        self.buffers.remove(&LineKey::NewDraft(topic));
        cx.notify();
        true
    }

    pub fn new_draft_topic(&self) -> Option<(HostId, rho_desk::cells::Id)> {
        self.new_draft.as_ref().and_then(|draft| draft.0.clone())
    }

    /// Renders the authoritative tree as one native editor composition. Each
    /// row is the node's own CRDT buffer; stars and typed machine/meta fields
    /// are display-only inlays, so structural state never leaks into text.
    fn sync_tree(
        &mut self,
        registry: &AgentRegistry,
        threads: &HashMap<SlackUnit, SlackFacts>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        self.tree_heading_agents.clear();
        self.tree_heading_pages.clear();
        self.referenced_pages.clear();
        for (host, source) in &self.tree_hosts {
            for node in &source.nodes {
                let Some(parent) = node.parent.clone() else {
                    continue;
                };
                if let Some(agent_id) = node.agent() {
                    self.tree_heading_agents
                        .entry((*host, parent.clone()))
                        .or_default()
                        .push(agent_id);
                }
                if let Some(page_id) = node_page(node) {
                    self.tree_heading_pages
                        .entry((*host, parent))
                        .or_default()
                        .push(page_id);
                    self.referenced_pages.insert(page_id);
                }
            }
        }
        // Machine rows carry no stored text: their titles are derived from
        // live metadata every reconcile. Only the rows whose title actually
        // moved are written; the rest are left alone, buffer and all.
        let mut written = HashMap::with_capacity(self.derived_titles.len());
        let mut writes = Vec::new();
        for (host, source) in &self.tree_hosts {
            for node in &source.nodes {
                let Some(buffer) = source.buffers.get(&node.id) else {
                    continue;
                };
                if is_note(node) {
                    continue;
                }
                let title = derived_title(node, registry, threads);
                let key = (*host, node.id.clone());
                let held = self.derived_titles.get(&key);
                if held != Some(&(buffer.entity_id(), title.clone())) {
                    writes.push((buffer.clone(), title.clone()));
                }
                written.insert(key, (buffer.entity_id(), title));
            }
        }
        self.derived_titles = written;
        for (buffer, title) in writes {
            crate::desk_view::write_derived_title(&buffer, &title, cx);
        }
        let cursor_anchor = self.editor.update(cx, |editor, cx| {
            let head = editor.selections.newest_anchor().head();
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            snapshot
                .anchor_to_buffer_anchor(head)
                .map(|(anchor, buffer)| buffer.anchor_after(anchor.to_offset(buffer)))
        });
        // Decorations are anchored in the current composition. Remove them
        // before replacing row buffers; asking the display map to translate
        // old inlay/fold edits through a replacement can underflow, and the
        // anchors cannot refer to the new buffer entities anyway.
        let old = std::mem::take(&mut self.tree_inlay_ids);
        self.editor
            .update(cx, |editor, cx| editor.splice_inlays(&old, Vec::new(), cx));
        self.apply_tree_folds(&[], &[], cx);
        let raw_mode = self.raw_mode;
        let rows = self
            .tree_hosts
            .iter()
            .flat_map(|(host, source)| {
                source.nodes.iter().filter_map(move |node| {
                    if raw_mode && !is_note(node) {
                        return None;
                    }
                    Some((*host, node.clone(), source.buffers.get(&node.id)?.clone()))
                })
            })
            .collect::<Vec<_>>();
        let semantic_rows = rows
            .iter()
            .filter(|(_, node, _)| node.is_note())
            .map(|(_, _, buffer)| buffer.read(cx).remote_id())
            .collect();
        self.editor.update(cx, |editor, _| {
            editor.set_semantic_row_buffers(semantic_rows)
        });
        let mut spec = CompositionSpec::default();
        for (host, node, buffer) in &rows {
            let key = (*host, node.id.clone(), node.under.clone());
            let id = *self.tree_element_keys.entry(key).or_insert_with(|| {
                self.next_element_key += 1;
                self.next_element_key
            });
            spec.tail.push(RowSpec {
                id,
                buffer: buffer.clone(),
            });
            if self.tree_new_draft_parent == Some((*host, node.id.clone()))
                && let Some((_, draft, _)) = &self.new_draft
            {
                let key = LineKey::NewDraft(Some((*host, node.id.clone())));
                let id = *self.element_keys.entry(key.clone()).or_insert_with(|| {
                    self.next_element_key += 1;
                    self.next_element_key
                });
                self.buffers.insert(key.clone(), draft.clone());
                self.targets.insert(
                    key.clone(),
                    RowTarget::NewTreeDraft((*host, node.id.clone())),
                );
                spec.tail.push(RowSpec {
                    id,
                    buffer: draft.clone(),
                });
            }
        }
        if self.tree_new_draft_parent.is_none()
            && let Some((_, draft, _)) = &self.new_draft
        {
            let key = LineKey::NewDraft(None);
            let id = *self.element_keys.entry(key.clone()).or_insert_with(|| {
                self.next_element_key += 1;
                self.next_element_key
            });
            self.buffers.insert(key, draft.clone());
            self.targets
                .insert(LineKey::NewDraft(None), RowTarget::NewDraft);
            spec.tail.push(RowSpec {
                id,
                buffer: draft.clone(),
            });
        }
        let changed = self.composition.sync(&self.multi_buffer, &spec, cx);
        if changed
            && self.pending_tree_cursor.is_none()
            && let Some(anchor) = cursor_anchor
        {
            self.select_buffer_anchor(anchor, None, window, cx);
        }
        if let Some((host, ref node_id, offset)) = self.pending_tree_cursor {
            if let Some(buffer) = self
                .tree_hosts
                .get(&host)
                .and_then(|source| source.buffers.get(&node_id))
            {
                let buffer = buffer.read(cx);
                let anchor = buffer.anchor_after(offset.min(buffer.len()));
                self.select_buffer_anchor(anchor, None, window, cx);
            }
            self.pending_tree_cursor = None;
        }
        if self.pending_cursor.as_ref().is_some_and(|key| {
            self.buffers.get(key).is_some_and(|candidate| {
                self.new_draft
                    .as_ref()
                    .is_some_and(|(_, buffer, _)| candidate == buffer)
            })
        }) && let Some((_, buffer, _)) = &self.new_draft
        {
            let buffer = buffer.read(cx);
            self.select_buffer_anchor(buffer.anchor_after(buffer.len()), None, window, cx);
            self.pending_cursor = None;
        }
        self.ensure_headerless(cx);
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        if self.raw_mode {
            self.editor.update(cx, |editor, cx| {
                for class in DashClass::ALL {
                    editor.highlight_text(class.key(), Vec::new(), class.style(cx), cx);
                }
            });
            self.apply_tree_folds(&[], &[], cx);
            return;
        }
        let mut inlays = Vec::new();
        #[cfg(test)]
        {
            self.composed += 1;
        }
        self.tree_draw.clear();
        self.tree_draw_at.clear();
        let mut eol_hints: Vec<(editor::Anchor, editor::EolHintRenderer)> = Vec::new();
        let mut highlights = DashClass::ALL
            .into_iter()
            .map(|class| (class, Vec::new()))
            .collect::<Vec<_>>();
        // Depth belongs to the row, not to the thing the row is of: a
        // labelled thing is drawn where it is filed and again under its
        // label, and the two places sit at different depths. The rows come
        // in the order the tree walks them, so the row a row hangs under is
        // the nearest one above it with that id, which is a stack rather
        // than a lookup.
        let mut row_depths: Vec<RowDepth> = Vec::with_capacity(rows.len());
        let mut open: Vec<(HostId, rho_desk::cells::Id, RowDepth)> = Vec::new();
        for (host, node, _) in &rows {
            match &node.under {
                None => open.clear(),
                Some(parent) => {
                    while open
                        .last()
                        .is_some_and(|(open_host, id, _)| open_host != host || id != parent)
                    {
                        open.pop();
                    }
                }
            }
            let above = open.last().map(|(_, _, depth)| *depth).unwrap_or_default();
            let depth = RowDepth {
                tree: above.tree + usize::from(node.under.is_some()),
                note: above.note + usize::from(node.is_note()),
                card: above.card + usize::from(!node.is_note()),
            };
            row_depths.push(depth);
            open.push((*host, node.id.clone(), depth));
        }
        // Where each row starts and ends in the map, found once. A thing
        // drawn in two places is one buffer in two excerpts, and asking the
        // buffer for its anchor lands in whichever excerpt came first, so
        // the second row would take the first one's prefix and highlight.
        // The excerpt boundaries name each excerpt's own rows, and the
        // composition names the path a row was put at, which pairs them.
        let mut rows_by_path: HashMap<multi_buffer::PathKey, (MultiBufferRow, MultiBufferRow)> =
            HashMap::new();
        for boundary in snapshot.excerpt_boundaries_in_range(multi_buffer::MultiBufferOffset(0)..) {
            if let multi_buffer::Anchor::Excerpt(excerpt) = boundary.next.start_anchor {
                rows_by_path
                    .entry(snapshot.path_for_anchor(excerpt).clone())
                    .or_insert((boundary.row, boundary.next.end_row));
            }
        }
        for (index, (host, node, _)) in rows.iter().enumerate() {
            // The visible heading prefix belongs to this row. Right-biased
            // at the row's first column, because a left-biased anchor sits
            // at the end of the row above instead, which made commands
            // issued on `*` edit that previous row (notably `dd`, `O`, `R`,
            // and subtree toggles).
            let key = (*host, node.id.clone(), node.under.clone());
            let Some((start_row, end_row)) = self
                .tree_element_keys
                .get(&key)
                .and_then(|id| self.composition.path_for_row(*id))
                .and_then(|path| rows_by_path.get(&path).copied())
            else {
                continue;
            };
            let start = snapshot.anchor_after(Point::new(start_row.0, 0));
            let end = snapshot.anchor_before(Point::new(end_row.0, snapshot.line_len(end_row)));
            let hidden_by_fold = self.tree_hosts.get(host).is_some_and(|source| {
                let mut parent = node.under.clone();
                while let Some(parent_id) = parent {
                    if self.tree_collapsed.contains(&(*host, parent_id.clone())) {
                        return true;
                    }
                    parent = source
                        .nodes
                        .iter()
                        .find(|candidate| candidate.id == parent_id)
                        .and_then(|candidate| candidate.under.clone());
                }
                false
            });
            // A card hangs under the card above it, so its marker starts
            // past that one's. Its own depth counts itself, which is the
            // indent a root card has: none.
            let indent = "    ".repeat(row_depths[index].card.saturating_sub(1));
            let prefix = match &node.id {
                rho_desk::cells::Id::Note(_) => {
                    // A note under a card is indented past the card's marker
                    // and words. At the left edge its `*` read as the next
                    // root rather than as something belonging to the card.
                    format!(
                        "{}{} ",
                        "    ".repeat(row_depths[index].card),
                        "*".repeat(row_depths[index].note.max(1))
                    )
                }
                rho_desk::cells::Id::Agent(_) => {
                    let label = node
                        .agent()
                        .map(|agent_id| {
                            format!(
                                "{} {} ",
                                match registry.attention(agent_id) {
                                    UiAttention::Quiet => "○",
                                    UiAttention::Working => "·",
                                    UiAttention::Pending => "●",
                                    UiAttention::NeedsInput => "!",
                                },
                                registry.agent_human_name(agent_id)
                            )
                        })
                        .unwrap_or_default();
                    format!("{indent}  • {label}")
                }
                _ => format!("{indent}  ◦ "),
            };
            let class = match &node.id {
                rho_desk::cells::Id::Note(_) => Some(DashClass::for_depth(row_depths[index].note)),
                _ => Some(DashClass::Muted),
            };
            if let Some(class) = class
                && let Some((_, ranges)) = highlights.iter_mut().find(|(key, _)| *key == class)
            {
                ranges.push(start..end);
            }
            let mut row_inlays = Vec::new();
            if !hidden_by_fold && !prefix.is_empty() {
                // A body runs to as many lines as it wants, and only its
                // first carries the bullet. The rest are padded to the same
                // column so the note reads as one block under it.
                let padding = " ".repeat(prefix.chars().count());
                let inlay = Inlay::custom(TREE_INLAY_ID_BASE + index * 2, start, prefix.clone());
                self.tree_inlay_ids.push(inlay.id);
                row_inlays.push(inlay.id);
                inlays.push(inlay);
                for (line, row) in (start_row.0 + 1..=end_row.0).enumerate() {
                    let anchor = snapshot.anchor_after(Point::new(row, 0));
                    let inlay = Inlay::custom(
                        CONTINUATION_INLAY_ID_BASE + index * 256 + line,
                        anchor,
                        padding.clone(),
                    );
                    self.tree_inlay_ids.push(inlay.id);
                    row_inlays.push(inlay.id);
                    inlays.push(inlay);
                }
            }
            let hint = (!hidden_by_fold).then(|| row_hint(node)).flatten();
            if let Some(text) = &hint {
                eol_hints.push((end, eol_hint(text.clone())));
            }
            self.tree_draw_at
                .entry((*host, node.id.clone()))
                .or_default()
                .push(self.tree_draw.len());
            self.tree_draw.push(TreeRowDraw {
                host: *host,
                node_id: node.id.clone(),
                start,
                end,
                prefix,
                inlays: row_inlays,
                at: index,
                hint,
                hidden_by_fold,
            });
        }
        self.editor.update(cx, |editor, cx| {
            editor.splice_inlays(&[], inlays, cx);
            editor.set_eol_hints(eol_hints, cx);
            for (class, ranges) in highlights {
                editor.highlight_text(class.key(), ranges, class.style(cx), cx);
            }
        });
        self.apply_tree_folds(rows.as_slice(), &row_depths, cx);
    }

    #[cfg(test)]
    pub(crate) fn display_text_for_test(&self, cx: &mut App) -> String {
        self.editor.update(cx, |editor, cx| editor.display_text(cx))
    }

    fn apply_tree_folds(
        &self,
        rows: &[(HostId, crate::desk_view::DeskNode, Entity<Buffer>)],
        depths: &[RowDepth],
        cx: &mut Context<Workspace>,
    ) {
        struct TreeSubtreeFold;
        let type_id = std::any::TypeId::of::<TreeSubtreeFold>();
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let mut creases = Vec::new();
        for (index, (host, node, _)) in rows.iter().enumerate() {
            if !self.tree_collapsed.contains(&(*host, node.id.clone())) {
                continue;
            }
            let depth = depths[index].tree;
            let end_index = rows[index + 1..]
                .iter()
                .enumerate()
                .position(|(offset, (candidate_host, _, _))| {
                    *candidate_host != *host || depths[index + 1 + offset].tree <= depth
                })
                .map_or(rows.len(), |offset| index + 1 + offset);
            if end_index == index + 1 {
                continue;
            }
            let first = &rows[index + 1].2;
            let last = &rows[end_index - 1].2;
            let start_snapshot = first.read(cx).snapshot();
            let end_snapshot = last.read(cx).snapshot();
            let (Some(start), Some(end)) = (
                snapshot.anchor_in_excerpt(start_snapshot.anchor_before(0)),
                snapshot.anchor_in_excerpt(end_snapshot.anchor_before(end_snapshot.len())),
            ) else {
                continue;
            };
            creases.push(editor::display_map::Crease::simple(
                start..end,
                editor::FoldPlaceholder {
                    render: std::sync::Arc::new(|_, _, _| gpui::Empty.into_any_element()),
                    constrain_width: false,
                    merge_adjacent: false,
                    type_tag: Some(type_id),
                    collapsed_text: Some(" …".into()),
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

    /// Regenerates the listing: the host documents are sliced at bound
    /// headings, generated rows and drafts are interleaved between the
    /// slices, and highlights and lamps reapplied. The cursor follows
    /// its buffer through the rearrangement.
    pub fn sync(
        &mut self,
        registry: &AgentRegistry,
        threads: &HashMap<SlackUnit, SlackFacts>,
        agent_interactions: &HashMap<AgentId, i64>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        self.sync_hand(agent_interactions);
        self.sync_tree(registry, threads, window, cx);
    }

    /// What the hand says, without touching the map. A delta that only
    /// redrew its own rows still moves the depth the deal bar shows, and
    /// that is a read of the ranking rather than a composition.
    pub fn sync_hand(&mut self, agent_interactions: &HashMap<AgentId, i64>) {
        let now = chrono::Local::now();
        let queue = self.dealer_hand(now.fixed_offset(), agent_interactions);
        self.queue_depth = DealQueueDepth {
            dealt_count: queue.dealt_count,
            total_alive: queue.total_alive,
        };
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
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            snapshot
                .anchor_to_buffer_anchor(anchor)
                .map(|(text_anchor, buffer)| {
                    (anchor, buffer.remote_id(), text_anchor.to_offset(buffer))
                })
        })?;
        self.place_for_anchor(anchor, buffer_id, offset, cx)
    }

    fn place_for_anchor(
        &self,
        _anchor: multi_buffer::Anchor,
        buffer_id: BufferId,
        offset: usize,
        cx: &App,
    ) -> Option<CursorPlace> {
        for (host, source) in &self.tree_hosts {
            if let Some(node_id) = source.buffers.iter().find_map(|(node_id, buffer)| {
                (buffer.read(cx).remote_id() == buffer_id).then_some(node_id.clone())
            }) {
                return Some(CursorPlace::Tree(*host, node_id, offset));
            }
        }
        self.buffers
            .iter()
            .find(|(_, buffer)| buffer.read(cx).remote_id() == buffer_id)
            .map(|(key, _)| CursorPlace::Row(key.clone()))
    }

    /// The row at a window-space position, resolved from the editor's painted
    /// layout without focusing it or moving its selection.
    pub fn target_at_window_position(
        &self,
        position: gpui::Point<gpui::Pixels>,
        registry: &AgentRegistry,
        cx: &mut Context<Workspace>,
    ) -> Option<RowTarget> {
        let place = self
            .editor
            .read(cx)
            .buffer_location_for_window_position(position, Bias::Left)?;
        let place = self.place_for_anchor(place.0, place.1, place.2, cx)?;
        self.target_for_place(place, registry, cx)
    }

    /// The row under the cursor.
    pub fn cursor_target(
        &self,
        registry: &AgentRegistry,
        cx: &mut Context<Workspace>,
    ) -> Option<RowTarget> {
        let place = self.cursor_place(cx)?;
        self.target_for_place(place, registry, cx)
    }

    fn target_for_place(
        &self,
        place: CursorPlace,
        registry: &AgentRegistry,
        _cx: &App,
    ) -> Option<RowTarget> {
        match place {
            CursorPlace::Row(key) => self.targets.get(&key).cloned(),
            CursorPlace::Tree(host, node_id, _) => {
                let source = self.tree_hosts.get(&host)?;
                let node = source.nodes.iter().find(|node| node.id == node_id)?;
                let topic = if node.is_note() {
                    Some(node.id.clone())
                } else {
                    nearest_tree_heading(source, node.parent.clone())
                };
                match &node.id {
                    rho_desk::cells::Id::Agent(_) => match node.agent() {
                        Some(agent_id) => Some(RowTarget::TreeAgent {
                            host,
                            node_id,
                            topic_node_id: topic?,
                            agent_id,
                        }),
                        None => Some(RowTarget::None),
                    },
                    rho_desk::cells::Id::Page(_) => match node_page(node) {
                        Some(page_id) => Some(RowTarget::TreePage {
                            host,
                            node_id,
                            topic_node_id: topic?,
                            page_id,
                        }),
                        None => Some(RowTarget::None),
                    },
                    _ => {
                        let topic = topic?;
                        let first_attention = self
                            .tree_heading_agents
                            .get(&(host, topic.clone()))
                            .into_iter()
                            .flatten()
                            .copied()
                            .find(|agent_id| registry.attention(*agent_id) >= UiAttention::Pending);
                        Some(RowTarget::TreeTopic {
                            host,
                            node_id: topic,
                            first_attention,
                            on_heading_line: node.is_note(),
                        })
                    }
                }
            }
        }
    }

    /// The heading that owns the cursor position: the containing heading
    /// for document positions, the bound heading for agent rows.
    pub fn cursor_topic(
        &self,
        cx: &mut Context<Workspace>,
    ) -> Option<(HostId, rho_desk::cells::Id)> {
        match self.cursor_place(cx)? {
            CursorPlace::Tree(host, node_id, _) => {
                let source = self.tree_hosts.get(&host)?;
                let node = source.nodes.iter().find(|node| node.id == node_id)?;
                let topic = if node.is_note() {
                    node.id.clone()
                } else {
                    nearest_tree_heading(source, node.parent.clone())?
                };
                Some((host, topic))
            }
            CursorPlace::Row(LineKey::NewDraft(topic)) => topic,
        }
    }

    /// Capture metadata for the heading under the cursor. The room is the
    /// top-level ancestor, not the leaf task, so capture never asks the user
    /// to classify a thought while still preserving the surrounding scene.
    pub fn capture_position(
        &self,
        cx: &mut Context<Workspace>,
    ) -> Option<(HostId, rho_desk::cells::Id, String)> {
        let (host, node_id) = self.cursor_topic(cx)?;
        let room = self.room_for_node(host, node_id.clone())?;
        Some((host, node_id, room.name))
    }

    /// Whether the cursor is somewhere dashboard verbs apply: a heading
    /// line of the document or a generated agent row.
    /// No rows at all, on any host: the desk the user is looking at is
    /// blank rather than merely scrolled away from its rows.
    pub fn tree_is_empty(&self) -> bool {
        self.tree_hosts
            .values()
            .all(|source| source.nodes.is_empty())
    }

    pub fn cursor_on_heading_line(&self, cx: &mut Context<Workspace>) -> bool {
        self.tree_node_at_cursor(cx).is_some_and(|(host, node_id)| {
            self.tree_hosts.get(&host).is_some_and(|source| {
                source
                    .nodes
                    .iter()
                    .any(|node| node.id == node_id && node.is_note())
            })
        })
    }

    /// Org-style visibility cycling on the heading under the cursor.
    pub fn toggle_subagents(&mut self, cx: &mut Context<Workspace>) -> bool {
        let Some((host, node_id)) = self.tree_node_at_cursor(cx) else {
            return false;
        };
        let is_heading = self.tree_hosts.get(&host).is_some_and(|source| {
            source
                .nodes
                .iter()
                .any(|node| node.id == node_id && node.is_note())
        });
        if !is_heading {
            return false;
        }
        if !self.tree_collapsed.insert((host, node_id.clone())) {
            self.tree_collapsed.remove(&(host, node_id));
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

    pub fn hint(&self, _cx: &mut Context<Workspace>) -> String {
        format!(
            "{} dealt · {} waiting",
            self.queue_depth.dealt_count, self.queue_depth.total_alive
        )
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
#[derive(Clone, Debug, PartialEq)]
pub struct DealAgentFacts {
    pub agent_id: AgentId,
    pub parent: Option<AgentId>,
    pub host: HostId,
    pub heading: String,
    pub facts: rho_registry::AgentFacts,
    pub attention: rho_registry::Attention,
}

/// One agent's facts, for a remake that names exactly it.
fn agent_deal_facts(registry: &AgentRegistry, agent_id: AgentId) -> Option<DealAgentFacts> {
    Some(DealAgentFacts {
        agent_id,
        parent: registry.agent_parent(agent_id),
        host: registry.host_of_agent(agent_id)?,
        heading: registry
            .agent_human_name(agent_id)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned(),
        facts: registry.agent_facts(agent_id),
        attention: registry.attention(agent_id),
    })
}

/// An agent's card is never about a Slack unit, so the remake that makes
/// one has no threads to offer.
static EMPTY_THREADS: std::sync::LazyLock<HashMap<SlackUnit, SlackFacts>> =
    std::sync::LazyLock::new(HashMap::new);

fn deal_agent_facts(registry: &AgentRegistry) -> Vec<DealAgentFacts> {
    registry
        .known_agents()
        .filter_map(|agent_id| {
            let host = registry.host_of_agent(*agent_id)?;
            Some(DealAgentFacts {
                agent_id: *agent_id,
                parent: registry.agent_parent(*agent_id),
                host,
                heading: registry
                    .agent_human_name(*agent_id)
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
                facts: registry.agent_facts(*agent_id),
                attention: registry.attention(*agent_id),
            })
        })
        .collect()
}

/// What an agent's card says and how loudly, or `None` when the agent is
/// not asking anything of the user: no finished turn, a turn in flight, or
/// the user has spoken since it finished.
/// The names an agent answers to besides its title: its tag, and the last
/// thing the user said to it.
fn agent_card_facts(
    facts: &rho_registry::AgentFacts,
    agent_id: AgentId,
    now: chrono::DateTime<chrono::FixedOffset>,
    agent_interactions: &HashMap<AgentId, i64>,
) -> Option<(f64, String)> {
    let ended = facts.last_turn_ended?;
    if facts.turn_running || ended <= facts.last_user_message_at {
        return None;
    }
    let wait_days = reply_wait_days(ended, now);
    // A dead turn is not an FYI: reading it as one gave it a decaying
    // priority and the word "finished", so a crashed agent quietly aged
    // out of Home. Only the user can start it again.
    let base_priority = if facts.errored || facts.needs_you_hint {
        blocked_reply_priority(wait_days)
    } else {
        fyi_reply_priority(wait_days)
    };
    let label = outcome_label(facts, wait_days);
    let recency_bonus = agent_interactions.get(&agent_id).map_or(0.0, |last| {
        let elapsed = (now.timestamp_millis() - *last).clamp(0, AGENT_RECENCY_WINDOW_MS);
        let remaining = 1.0 - elapsed as f64 / AGENT_RECENCY_WINDOW_MS as f64;
        AGENT_RECENCY_BONUS * remaining * remaining
    });
    let priority = base_priority + recency_bonus;
    (priority > DEAL_QUEUE_FLOOR).then_some((priority, label))
}

/// What an agent's own status line says, which is what its head is doing:
/// a running turn and how long it has run, or else how the last one ended.
/// Never a card's label; a card is the dealer's reason for showing the
/// agent, and a running agent has no card at all.
pub(crate) fn agent_state_label(
    facts: &rho_registry::AgentFacts,
    now: chrono::DateTime<chrono::FixedOffset>,
) -> Option<String> {
    if facts.turn_running {
        return Some(match facts.turn_started_at {
            Some(started) => format!(
                "working · {}",
                crate::home::elapsed_label(started.0 as i64, now.timestamp_millis())
            ),
            // The head says a turn runs without saying since when, which is
            // every turn that started before the client was listening.
            None => "working".to_owned(),
        });
    }
    let ended = facts.last_turn_ended?;
    Some(outcome_label(facts, reply_wait_days(ended, now)))
}

/// How the last finished turn ended, in the words Home's cards use.
fn outcome_label(facts: &rho_registry::AgentFacts, wait_days: f64) -> String {
    if facts.errored {
        format!("errored · {} ago", age_label(wait_days))
    } else if facts.needs_you_hint {
        format!("waiting on reply · {}", age_label(wait_days))
    } else {
        format!("finished · {} ago", age_label(wait_days))
    }
}

fn reply_wait_days(ended: rho_core::UnixMs, now: chrono::DateTime<chrono::FixedOffset>) -> f64 {
    (now.timestamp_millis() - ended.0 as i64) as f64 / 86_400_000.0
}

fn blocked_reply_priority(wait_days: f64) -> f64 {
    BLOCKED_REPLY_HEAD_START + BLOCKED_REPLY_SLOPE_PER_DAY * wait_days
}

fn fyi_reply_priority(wait_days: f64) -> f64 {
    // Rising curves belong to work waiting on the user. Finished FYI work is
    // reminder-like: visible immediately, then implicitly accepted if it has
    // not mattered after a few days (and still reachable from the Desk).
    -wait_days / FYI_REPLY_PACE_DAYS
}

pub(crate) fn age_label(days: f64) -> String {
    if days < 1.0 / 24.0 {
        format!("{}m", (days * 1_440.0).max(0.0).round() as i64)
    } else if days < 1.0 {
        format!("{:.1}h", days * 24.0)
    } else {
        format!("{days:.1}d")
    }
}

/// A dated mark on a Desk node, as the dealer reads it: a field, never text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeskMark {
    /// `DeferUntil`: nothing until the date, then it ages. A migrated todo
    /// keeps its cadence in `PaceDays`; a migrated defer has none.
    Wakes,
    Deadline,
}

fn desk_time(at: rho_desk::cells::Timestamp) -> Option<chrono::NaiveDateTime> {
    chrono::DateTime::from_timestamp_millis(at.unix_ms).map(|time| time.naive_local())
}

/// Days between a mark and now, whole days when the mark is only a date.
fn desk_elapsed(at: rho_desk::cells::Timestamp, now: chrono::NaiveDateTime) -> Option<f64> {
    let time = desk_time(at)?;
    Some(
        if at.precision == rho_desk::cells::TimestampPrecision::Day {
            now.date().signed_duration_since(time.date()).num_days() as f64
        } else {
            now.signed_duration_since(time).num_seconds() as f64 / 86_400.0
        },
    )
}

/// The dated marks a node carries. An Open node with neither is a note, not
/// a card: the desk is where you write, and writing is not a queue.
fn desk_marks(node: &crate::desk_view::DeskNode) -> Vec<(DeskMark, rho_desk::cells::Timestamp)> {
    if node.state != rho_desk::cells::State::Open {
        return Vec::new();
    }
    let mut marks = Vec::new();
    if let Some(at) = node.defer_until {
        marks.push((DeskMark::Wakes, at));
    }
    if let Some(at) = node.deadline {
        marks.push((DeskMark::Deadline, at));
    }
    marks
}

/// Whether a node is still waiting for its date, which hides it and every
/// card under it.
/// Whether the user has already dealt with a node: handled through what
/// the story told, muted, or snoozed to a time still ahead.
fn node_closed(
    node: &crate::desk_view::DeskNode,
    now: chrono::DateTime<chrono::FixedOffset>,
) -> bool {
    node.state != rho_desk::cells::State::Open || desk_deferred(node, now.naive_local())
}

/// The same question for an agent reached through a note, which knows the
/// agent id but not which row stands for it.
fn agent_node_closed(
    source: &TreeHostSource,
    agent_id: AgentId,
    now: chrono::DateTime<chrono::FixedOffset>,
) -> bool {
    source
        .agent_node(agent_id)
        .is_some_and(|node| node_closed(node, now))
}

fn desk_deferred(node: &crate::desk_view::DeskNode, now: chrono::NaiveDateTime) -> bool {
    node.defer_until
        .and_then(|at| desk_elapsed(at, now))
        .is_some_and(|elapsed| elapsed < 0.0)
}

fn desk_mark_priority(
    mark: DeskMark,
    at: rho_desk::cells::Timestamp,
    pace_days: u32,
    now: chrono::NaiveDateTime,
) -> f64 {
    let Some(elapsed) = desk_elapsed(at, now) else {
        return f64::NEG_INFINITY;
    };
    let pace = f64::from(pace_days);
    match mark {
        // One curve serves both of yesterday's marks: a todo carried its
        // cadence, a defer had none, and `elapsed - pace` is each of them.
        DeskMark::Wakes if elapsed < 0.0 => f64::NEG_INFINITY,
        DeskMark::Wakes => elapsed - pace,
        DeskMark::Deadline if elapsed < -pace => f64::NEG_INFINITY,
        DeskMark::Deadline if elapsed <= 0.0 => elapsed / pace.max(1.0),
        DeskMark::Deadline => 1_000_000.0 + elapsed,
    }
}

fn desk_mark_label(
    mark: DeskMark,
    at: rho_desk::cells::Timestamp,
    now: chrono::NaiveDateTime,
) -> String {
    let Some(elapsed) = desk_elapsed(at, now) else {
        return String::new();
    };
    match mark {
        DeskMark::Deadline if elapsed > 0.0 => {
            format!("deadline · {}d late", elapsed.floor() as u64)
        }
        DeskMark::Deadline => format!("deadline · {}d", (-elapsed).ceil() as u64),
        // A woken node reads the same whether it was a todo or a defer:
        // the cadence lives in the curve, not in two words for one field.
        DeskMark::Wakes => format!("deferred · woke {}", age_label(elapsed)),
    }
}

/// The agent an agent row is: the id is the agent, so there is nothing to
/// look up.
pub(crate) fn node_agent(node: &crate::desk_view::DeskNode) -> Option<AgentId> {
    node.agent()
}

/// The Slack unit a row stands for.
pub(crate) fn node_unit(node: &crate::desk_view::DeskNode) -> Option<SlackUnit> {
    node.slack().cloned()
}

fn node_page(node: &crate::desk_view::DeskNode) -> Option<rho_browser::PageId> {
    node.page()
        .map(|page| rho_browser::PageId(uuid::Uuid::from_bytes(page.0)))
}

/// A note's workdir is a `File` filed under it, so callers look one level
/// down; an agent's own workdir comes from the registry instead.
pub(crate) fn node_file_path(
    nodes: &[crate::desk_view::DeskNode],
    id: &rho_desk::cells::Id,
) -> Option<camino::Utf8PathBuf> {
    nodes
        .iter()
        .filter(|node| node.parent.as_ref() == Some(id))
        .find_map(|node| node.path().map(ToOwned::to_owned))
}

/// What a row that is not a note says. Agent rows carry their name in the
/// row prefix already, so their buffer stays empty rather than repeating it.
fn derived_title(
    node: &crate::desk_view::DeskNode,
    _registry: &AgentRegistry,
    threads: &HashMap<SlackUnit, SlackFacts>,
) -> String {
    use rho_desk::cells::Id;

    // A name the user wrote wins over anything derived, whatever the row
    // is: a Slack conversation they renamed reads as what they called it,
    // not as what Slack calls it. A note is the exception, because its
    // title is the first line of its body.
    if let Some(name) = node.name.as_ref().filter(|name| !name.trim().is_empty())
        && !matches!(node.id, Id::Note(_))
    {
        return name.clone();
    }
    match &node.id {
        Id::Agent(_) | Id::Note(_) => String::new(),
        Id::Label(_) => "label".to_owned(),
        Id::Host(_) => "this host".to_owned(),
        Id::Page(_) => node_page(node)
            .and_then(rho_browser::live_page_name)
            .unwrap_or_else(|| "page".to_owned()),
        Id::File { path, .. } => path.to_string(),
        // The store holds the unit's identity, which is ids and a
        // timestamp; what it is called lives in the mirror. A thread rho
        // has not caught up with yet says so rather than showing its keys.
        Id::Slack(unit) => match threads.get(unit) {
            Some(facts) if facts.title.is_empty() => facts.conversation.clone(),
            Some(facts) => format!("{} · {}", facts.conversation, facts.title),
            // A unit rho has not caught up with yet is named by any sibling
            // that knows what Slack calls the conversation, so its ids are
            // never shown.
            None => threads
                .iter()
                .find(|(key, _)| key.workspace == unit.workspace && key.channel == unit.channel)
                .map(|(_, facts)| facts.conversation.clone())
                .unwrap_or_else(|| "conversation".to_owned()),
        },
        Id::PullRequest { repo, number } => format!("{repo}#{number}"),
    }
}

/// What an area row calls itself in the picker.
/// Every label as the path a person would type, `rho/agent`. The label
/// axis only: a label filed under something that is not a label is named
/// by its own name, because the path is the filing rather than the place.
fn label_paths(
    nodes: &HashMap<rho_desk::cells::Id, &crate::desk_view::DeskNode>,
) -> HashMap<rho_desk::cells::Id, String> {
    let mut paths = HashMap::new();
    for (id, node) in nodes {
        if !matches!(id, rho_desk::cells::Id::Label(_)) {
            continue;
        }
        let Some(name) = node.name.clone() else {
            continue;
        };
        let mut segments = vec![name];
        let mut parent = node.parent.clone();
        while let Some(above) =
            parent.filter(|above| matches!(above, rho_desk::cells::Id::Label(_)))
        {
            let Some(node) = nodes.get(&above) else { break };
            let Some(name) = node.name.clone() else { break };
            segments.push(name);
            parent = node.parent.clone();
        }
        segments.reverse();
        paths.insert(id.clone(), segments.join("/"));
    }
    paths
}

fn area_kind(id: &rho_desk::cells::Id) -> &'static str {
    use rho_desk::cells::Id;

    match id {
        Id::Note(_) => "note",
        Id::Label(_) => "label",
        Id::Agent(_) => "agent",
        Id::Host(_) => "host",
        Id::Page(_) => "page",
        Id::Slack(unit) if unit.thread.is_some() => "thread",
        Id::Slack(_) => "conversation",
        Id::File { .. } => "file",
        Id::PullRequest { .. } => "pull request",
    }
}

/// A note's title is the first line of its body. The rest of the body is
/// the note itself: it belongs on the note's own surface, never in a path,
/// a card, or a picker row.
pub(crate) fn note_title(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
}

/// A note is the user's to write in; every other kind is the machine's row.
fn is_note(node: &crate::desk_view::DeskNode) -> bool {
    node.is_note()
}

fn tree_breadcrumb(id: &rho_desk::cells::Id, source: &TreeHostSource) -> String {
    let mut path = Vec::new();
    let mut cursor = Some(id.clone());
    while let Some(id) = cursor {
        let Some(node) = source.node(&id) else {
            break;
        };
        if node.is_note() {
            path.push(source.title(&id).unwrap_or(""));
        }
        cursor = node.parent.clone();
    }
    path.reverse();
    path.join(" › ")
}

#[derive(Clone, Debug)]
struct RankedDealCard {
    priority: f64,
    virtual_reply: bool,
    order: usize,
    cursor: CardCursor,
    card: DealCard,
    /// How this card's priority and label move with waiting. A card is
    /// made when the thing it is about changes; the clock moving is not
    /// that, so a read brings the card up to date through this rather than
    /// making it again.
    curve: PriorityCurve,
    /// The row this card hangs under, when a row lends it one. A verdict on
    /// a heading moves every card it holds, and this is how the set finds
    /// them without looking at the others.
    heading: Option<rho_desk::cells::Id>,
}

/// The part of a card that slides with the clock, separated from the card
/// so that keeping a card and keeping it current are different things.
#[derive(Clone, Copy, Debug)]
enum PriorityCurve {
    /// A dated mark on a note. Rises as the mark ripens, or once it is
    /// overdue; the note's pace is the scale.
    DeskMark {
        mark: DeskMark,
        at: rho_desk::cells::Timestamp,
        pace_days: u32,
    },
    /// An agent whose turn has ended. The curve is blocked or FYI by what
    /// the turn said, and the user's own recent attention lifts it.
    AgentReply { agent_id: AgentId },
    /// A Slack unit. Its wait is the mirror's measurement, not the
    /// clock's, so it stands still until the mirror says otherwise.
    Thread,
}

impl RankedDealCard {
    /// Brings the card up to the moment it is read at. Returns false when
    /// the curve has taken it under the floor or the facts behind it no
    /// longer make a card at all, which is the read's own way of dropping
    /// what has aged out.
    fn rescore(
        &mut self,
        now: chrono::DateTime<chrono::FixedOffset>,
        agent_interactions: &HashMap<AgentId, i64>,
    ) -> bool {
        let (priority, label) = match self.curve {
            PriorityCurve::Thread => return true,
            PriorityCurve::DeskMark {
                mark,
                at,
                pace_days,
            } => (
                desk_mark_priority(mark, at, pace_days, now.naive_local()),
                desk_mark_label(mark, at, now.naive_local()),
            ),
            PriorityCurve::AgentReply { agent_id } => {
                let CardCursor::Agent(ref facts, _) = self.cursor else {
                    return true;
                };
                match agent_card_facts(facts, agent_id, now, agent_interactions) {
                    Some(scored) => scored,
                    None => return false,
                }
            }
        };
        if priority <= DEAL_QUEUE_FLOOR {
            return false;
        }
        self.priority = priority;
        self.card.priority = priority;
        self.card.label = label;
        true
    }
}

impl Dashboard {
    pub fn heading_destination_candidates(
        &self,
        _cx: &App,
    ) -> Vec<(String, String, HostId, rho_desk::cells::Id)> {
        self.tree_hosts
            .iter()
            .flat_map(|(host, source)| {
                source.nodes.iter().filter_map(move |node| {
                    if !node.is_note() {
                        return None;
                    }
                    let title = source.title(&node.id)?.to_owned();
                    Some((
                        title.clone(),
                        self.breadcrumb_for_node_for_source(node.id.clone(), source)?,
                        *host,
                        node.id.clone(),
                    ))
                })
            })
            .collect()
    }

    /// Every node a new thing can be filed under, as its full path. Any
    /// kind is an area: a note under a Slack thread is notes for that
    /// thread, an agent under a page is the engineer on it. A row with
    /// nothing readable to type at is left out.
    pub(crate) fn area_candidates(
        &self,
        registry: &AgentRegistry,
        threads: &HashMap<SlackUnit, SlackFacts>,
        cx: &App,
    ) -> Vec<(String, &'static str, HostId, rho_desk::cells::Id)> {
        let mut areas = Vec::new();
        for (host, source) in &self.tree_hosts {
            let titles = source.all_titles(cx);
            for node in &source.nodes {
                let breadcrumb = tree_breadcrumb(&node.id, source);
                // A note's breadcrumb already ends with the note itself;
                // every other kind hangs its title under its parent's.
                let path = if is_note(node) {
                    breadcrumb
                } else {
                    let title = titles
                        .get(&node.id)
                        .and_then(|text| text.lines().next())
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .map(str::to_owned)
                        .or_else(|| {
                            node.agent()
                                .map(|agent_id| registry.agent_human_name(agent_id))
                        })
                        .unwrap_or_else(|| derived_title(node, registry, threads));
                    if breadcrumb.is_empty() {
                        title
                    } else {
                        format!("{breadcrumb} › {title}")
                    }
                };
                if path.trim().is_empty() {
                    continue;
                }
                areas.push((path, area_kind(&node.id), *host, node.id.clone()));
            }
        }
        areas
    }

    /// Every tree node the finder can open, as its full path and target.
    /// Headings carry their own breadcrumb; an agent or a page hangs its
    /// title under its parent's.
    pub(crate) fn find_candidates(
        &self,
        registry: &AgentRegistry,
        cx: &App,
    ) -> Vec<crate::find::FindCandidate> {
        use crate::find::{FindCandidate, FindTarget};

        let mut candidates = Vec::new();
        for (host, source) in &self.tree_hosts {
            let nodes = source
                .nodes
                .iter()
                .map(|node| (node.id.clone(), node))
                .collect::<HashMap<_, _>>();
            let titles = source.all_titles(cx);
            let title_of = |node_id: rho_desk::cells::Id| {
                titles
                    .get(&node_id)
                    .and_then(|text| text.lines().next())
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned)
            };
            let label_paths = label_paths(&nodes);
            for node in &source.nodes {
                let breadcrumb = tree_breadcrumb(&node.id, source);
                let under = |title: String| {
                    if breadcrumb.is_empty() {
                        title
                    } else {
                        format!("{breadcrumb} › {title}")
                    }
                };
                // A thing is as often remembered by what it is filed under
                // as by where it sits, so each label names it too.
                let labelled = |title: &str| {
                    node.labels
                        .iter()
                        .filter_map(|label| label_paths.get(label))
                        .map(|path| format!("{path} › {title}"))
                        .collect::<Vec<_>>()
                };
                let candidate = match &node.id {
                    rho_desk::cells::Id::Note(_) => {
                        if breadcrumb.is_empty() {
                            continue;
                        }
                        FindCandidate {
                            labels: labelled(&breadcrumb),
                            aka: Vec::new(),
                            path: breadcrumb.clone(),
                            kind: "topic",
                            target: FindTarget::Topic {
                                host: *host,
                                node_id: node.id.clone(),
                            },
                            recency: self
                                .tree_heading_agents
                                .get(&(*host, node.id.clone()))
                                .into_iter()
                                .flatten()
                                .filter_map(|agent_id| registry.agent_last_active(*agent_id))
                                .map(|active| active.0 as i64)
                                .max()
                                .unwrap_or_default(),
                        }
                    }
                    rho_desk::cells::Id::Agent(_) => {
                        let Some(agent_id) = node.agent() else {
                            continue;
                        };
                        // Which names an agent answers to is the agent
                        // crate's; where it sits in the tree is this row's.
                        let hit =
                            rho_agents::find::hit(registry, agent_id, title_of(node.id.clone()));
                        FindCandidate {
                            labels: labelled(&hit.title),
                            aka: hit.aka,
                            path: under(hit.title),
                            kind: "agent",
                            target: FindTarget::Agent(agent_id),
                            recency: hit.recency,
                        }
                    }
                    rho_desk::cells::Id::Page(_) => {
                        let Some(page_id) = node_page(node) else {
                            continue;
                        };
                        let Some(title) = title_of(node.id.clone()) else {
                            continue;
                        };
                        FindCandidate {
                            labels: labelled(&title),
                            aka: Vec::new(),
                            path: under(title),
                            kind: "page",
                            target: FindTarget::Page(page_id),
                            recency: 0,
                        }
                    }
                    _ => continue,
                };
                candidates.push(candidate);
            }
        }
        // An agent nobody filed is findable all the same: filing says where
        // a thing sits, and the finder is for the ones the reader cannot
        // point at.
        let filed = self
            .tree_hosts
            .values()
            .flat_map(|source| source.nodes.iter().filter_map(|node| node.agent()))
            .collect::<std::collections::HashSet<_>>();
        for agent_id in registry.known_agents().copied() {
            if filed.contains(&agent_id) || registry.agent_hidden(agent_id) {
                continue;
            }
            let hit = rho_agents::find::hit(registry, agent_id, None);
            candidates.push(crate::find::FindCandidate {
                labels: Vec::new(),
                aka: hit.aka,
                path: hit.title,
                kind: "agent",
                target: crate::find::FindTarget::Agent(agent_id),
                recency: hit.recency,
            });
        }
        candidates
    }

    pub fn heading_candidates(
        &self,
        _registry: &AgentRegistry,
        needle: &str,
        _cx: &App,
    ) -> Vec<(String, String)> {
        let needle = needle.to_lowercase();
        self.tree_hosts
            .iter()
            .flat_map(|(_, source)| {
                source
                    .nodes
                    .iter()
                    .filter(|node| node.is_note())
                    .filter_map(|node| {
                        let title = source.title(&node.id)?.to_owned();
                        title.to_lowercase().contains(&needle).then(|| {
                            (
                                title.clone(),
                                self.breadcrumb_for_node_for_source(node.id.clone(), source)
                                    .unwrap_or(title),
                            )
                        })
                    })
            })
            .collect()
    }

    fn breadcrumb_for_node_for_source(
        &self,
        node_id: rho_desk::cells::Id,
        source: &TreeHostSource,
    ) -> Option<String> {
        Some(tree_breadcrumb(&node_id, source))
    }

    pub fn jump_to_heading(
        &mut self,
        query: &str,
        _registry: &AgentRegistry,
        _window: &mut Window,
        _cx: &mut Context<Workspace>,
    ) -> bool {
        let Some((host, node_id)) = self.tree_heading_named(query) else {
            return false;
        };
        self.move_to_tree_node_when_ready(host, node_id);
        true
    }

    pub fn rename_cursor_topic(&mut self, title: &str, cx: &mut Context<Workspace>) -> bool {
        let Some((host, node_id)) = self.tree_node_at_cursor(cx) else {
            return false;
        };
        let Some(buffer) = self
            .tree_hosts
            .get(&host)
            .and_then(|source| source.buffers.get(&node_id))
            .cloned()
        else {
            return false;
        };
        let len = buffer.read(cx).len();
        buffer.update(cx, |buffer, cx| buffer.edit([(0..len, title)], None, cx));
        true
    }

    pub fn staffing_target_for(
        &self,
        topic: (HostId, rho_desk::cells::Id),
        cx: &App,
    ) -> Result<(HostId, rho_desk::cells::Id, String, Option<String>), &'static str> {
        let (host, node_id) = topic;
        let source = self.tree_hosts.get(&host).ok_or("Desk host unavailable")?;
        let node = source
            .nodes
            .iter()
            .find(|node| node.id == node_id)
            .ok_or("Desk node unavailable")?;
        let text = source
            .buffers
            .get(&node_id)
            .ok_or("Desk text unavailable")?
            .read(cx)
            .text();
        let project = node_file_path(&source.nodes, &node.id).map(|path| path.to_string());
        Ok((host, node_id, text, project))
    }

    pub fn next_now(
        &mut self,
        registry: &AgentRegistry,
        _window: &mut Window,
        _cx: &mut Context<Workspace>,
    ) -> Option<AgentId> {
        let ((host, node_id), agent_id) =
            self.tree_heading_agents
                .iter()
                .find_map(|(topic, agents)| {
                    agents
                        .iter()
                        .copied()
                        .find(|id| registry.attention(*id) >= UiAttention::Pending)
                        .map(|agent| (topic.clone(), agent))
                })?;
        self.move_to_tree_node_when_ready(host, node_id);
        Some(agent_id)
    }

    pub fn back(
        &mut self,
        _registry: &AgentRegistry,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let Some((host, node_id)) = self.tree_node_at_cursor(cx) else {
            return false;
        };
        let Some(parent) = self
            .tree_hosts
            .get(&host)
            .and_then(|source| source.nodes.iter().find(|node| node.id == node_id))
            .and_then(|node| node.parent.clone())
        else {
            return false;
        };
        self.move_to_tree_node_when_ready(host, parent);
        true
    }

    pub fn cycle_global_folds(&mut self, cx: &mut Context<Workspace>) -> bool {
        let headings = self
            .tree_hosts
            .iter()
            .flat_map(|(host, source)| {
                source
                    .nodes
                    .iter()
                    .filter(|node| node.is_note())
                    .map(move |node| (*host, node.id.clone()))
            })
            .collect::<HashSet<_>>();
        if headings.is_empty() {
            return false;
        }
        if self.tree_collapsed == headings {
            self.tree_collapsed.clear();
        } else {
            self.tree_collapsed = headings;
        }
        cx.notify();
        true
    }

    pub fn toggle_agent_tree(&mut self, cx: &mut Context<Workspace>) -> bool {
        let Some(key) = self.tree_node_at_cursor(cx) else {
            return false;
        };
        let has_children = self.tree_hosts.get(&key.0).is_some_and(|source| {
            source
                .nodes
                .iter()
                .any(|node| node.parent == Some(key.1.clone()))
        });
        if !has_children {
            return false;
        }
        if !self.tree_collapsed.insert(key.clone()) {
            self.tree_collapsed.remove(&key);
        }
        cx.notify();
        true
    }
}

/// What a thread's card says and how hard it pushes. The state, then how
/// long it has been in that state: whose turn it is is the whole of what a
/// thread's card says. Somebody waiting outranks a note of the same age,
/// the way a blocked agent outranks an FYI.
fn thread_card_facts(
    thread: &SlackFacts,
    now: chrono::DateTime<chrono::FixedOffset>,
) -> (String, f64) {
    let _ = now;
    let (state, priority) = match thread.waiting_on.is_some() {
        // The user answered: the thread is theirs to carry now, so the card
        // is a reminder that fades, not a demand that grows.
        true => ("replied", fyi_reply_priority(thread.wait_days)),
        false => (
            "needs reply",
            THREAD_REPLY_HEAD_START + BLOCKED_REPLY_SLOPE_PER_DAY * thread.wait_days,
        ),
    };
    (
        format!("{state} · {}", age_label(thread.wait_days)),
        priority,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name the user wrote wins over the name the source gave, on any
    /// id: a Slack conversation they renamed reads as what they called it,
    /// and one they have not is still named by the mirror.
    #[test]
    fn a_name_the_user_wrote_beats_the_one_the_source_gave() {
        let unit = rho_desk::cells::SlackUnit {
            workspace: "acme".to_owned(),
            channel: "C1".to_owned(),
            thread: None,
        };
        let node = |name: Option<&str>| crate::desk_view::DeskNode {
            id: rho_desk::cells::Id::Slack(unit.clone()),
            parent: None,
            under: None,
            state: rho_desk::cells::State::Open,
            defer_until: None,
            deadline: None,
            pace_days: 0,
            labels: Default::default(),
            name: name.map(str::to_owned),
            created_at: None,
        };
        let registry = AgentRegistry::default();
        let facts = HashMap::from([(
            unit.clone(),
            SlackFacts {
                title: "any update?".to_owned(),
                conversation: "#design".to_owned(),
                raised_at: chrono::Local::now().fixed_offset(),
                wait_days: 0.0,
                waiting_on: None,
                latest: "500.0".to_owned(),
                newest_from_other: Some("500.0".to_owned()),
            },
        )]);
        assert_eq!(
            derived_title(&node(None), &registry, &facts),
            "#design · any update?"
        );
        assert_eq!(
            derived_title(&node(Some("the release room")), &registry, &facts),
            "the release room"
        );
    }

    #[test]
    fn a_slack_thread_deals_like_an_agent_waiting_on_a_reply() {
        let now = chrono::NaiveDate::from_ymd_opt(2026, 8, 23)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
            .fixed_offset();
        let thread = |waiting_on: Option<&str>, wait_days: f64| SlackFacts {
            title: "can you look at the deploy?".into(),
            conversation: "#design".into(),
            raised_at: now - chrono::Duration::days(2),
            wait_days,
            waiting_on: waiting_on.map(str::to_owned),
            latest: "500.0".into(),
            newest_from_other: Some("500.0".into()),
        };

        let (label, priority) = thread_card_facts(&thread(None, 2.0), now);
        assert_eq!(label, "needs reply · 2.0d");
        // Nothing addressed to the machine reaches what the reader sees.
        assert!(!label.contains("C1"));
        assert!(!label.contains("500.0"));
        assert_eq!(
            priority,
            THREAD_REPLY_HEAD_START + 2.0 * BLOCKED_REPLY_SLOPE_PER_DAY
        );

        // Answering flips the word and the curve: the card fades from the
        // reply instead of rising, and is under the floor after three days.
        let (label, replied) = thread_card_facts(&thread(Some("#design"), 2.0), now);
        assert_eq!(label, "replied · 2.0d");
        assert_eq!(replied, fyi_reply_priority(2.0));
        assert!(replied > DEAL_QUEUE_FLOOR);
        assert!(thread_card_facts(&thread(Some("#design"), 3.5), now).1 <= DEAL_QUEUE_FLOOR);
    }

    #[test]
    fn a_ping_outranks_a_blocked_agent_of_the_same_wait_but_not_a_fresh_one() {
        // The user's rule: someone waiting on them comes first, unless they
        // were talking to that agent minutes ago.
        let wait_days = 2.0 / 24.0;
        let ping = THREAD_REPLY_HEAD_START + BLOCKED_REPLY_SLOPE_PER_DAY * wait_days;
        let blocked = blocked_reply_priority(wait_days);
        assert!(
            ping > blocked,
            "a ping ({ping}) outranks an agent ({blocked})"
        );

        let elapsed = 10 * 60 * 1_000;
        let remaining = 1.0 - elapsed as f64 / AGENT_RECENCY_WINDOW_MS as f64;
        let just_spoken_to = blocked + AGENT_RECENCY_BONUS * remaining * remaining;
        assert!(
            just_spoken_to > ping,
            "an agent spoken to 10 minutes ago ({just_spoken_to}) still comes first"
        );
    }
}

/// A mark's date as the reader sees it in an end-of-line hint.
/// A mark's date, with its clock time when it has one: a snooze of an hour
/// comes back this afternoon, and a bare date would not say when.
fn desk_date(at: rho_desk::cells::Timestamp) -> String {
    let format = match at.precision {
        rho_desk::cells::TimestampPrecision::Day => "%Y-%m-%d",
        _ => "%Y-%m-%d %H:%M",
    };
    desk_time(at).map_or_else(
        || "unknown".to_owned(),
        |time| time.format(format).to_string(),
    )
}

/// How far in one row is drawn, in the three counts the map indents by.
/// It is per row rather than per thing, because a labelled thing is drawn
/// in more than one place and the places are at different depths.
#[derive(Clone, Copy, Default)]
struct RowDepth {
    /// Rows above it in the tree, whatever they are: what a fold spans.
    tree: usize,
    /// Notes above it, which is how many `*` its bullet carries.
    note: usize,
    /// Cards above it: a card's marker is two columns in, so anything
    /// below it starts past those columns.
    card: usize,
}
