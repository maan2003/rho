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

use editor::{Editor, EditorMode, SizingBehavior};
use gpui::prelude::*;
use gpui::{App, Context, Entity, Focusable as _, Window};
use language::{Buffer, Capability};
use multi_buffer::MultiBuffer;
use rho_agents::{AgentMap, HostId};
pub use rho_desk::cells::SlackUnit;
use rho_ui_proto::AgentId;

use crate::workspace::Workspace;

type DraftTopic = Option<(HostId, rho_desk::cells::Id)>;
type DraftState = (DraftTopic, Entity<Buffer>, gpui::Subscription);

// Dealer curve tuning. These are deliberately all in one place: rho has one
// user, so policy changes are edits, not a configuration system.
pub(crate) const DEAL_QUEUE_FLOOR: f64 = -1.0;
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
/// Unread traffic in a channel nobody addressed the user in. It starts far
/// below a direct message or a thread they are in and fades instead of
/// rising, so it can never overtake one however long it sits: a room is
/// worth a look today and worth nothing by the weekend.
const CHANNEL_TRAFFIC_HEAD_START: f64 = 0.3;
/// What being answered by somebody else takes off a channel's card. The
/// room is already being dealt with, so it falls under the floor in a
/// little over two days instead of four.
const CHANNEL_ANSWERED_DROP: f64 = 0.6;
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

pub(crate) fn dealer_policy_snapshot() -> rho_journal::DealerPolicySnapshot {
    rho_journal::DealerPolicySnapshot {
        queue_floor: DEAL_QUEUE_FLOOR,
        skip_cooldown_minutes: SKIP_COOLDOWN.num_minutes(),
        blocked_reply_head_start: BLOCKED_REPLY_HEAD_START,
        blocked_reply_slope_per_day: BLOCKED_REPLY_SLOPE_PER_DAY,
        fyi_reply_pace_days: FYI_REPLY_PACE_DAYS,
        thread_reply_head_start: THREAD_REPLY_HEAD_START,
        channel_traffic_head_start: CHANNEL_TRAFFIC_HEAD_START,
        channel_answered_drop: CHANNEL_ANSWERED_DROP,
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
    /// Why Slack is asking for the reader here, or `None` when it is not
    /// asking at all: a unit whose messages have all been read is still a
    /// unit — Find reaches it — but it is no longer a card. The fact, not
    /// the sentence.
    /// The words are made from this and `conversation` when the card is
    /// drawn, so a conversation named late reads as `#design` and never as
    /// a stale line written when the message landed.
    pub reason: Option<rho_slack::model::Attention>,
    pub raised_at: chrono::DateTime<chrono::FixedOffset>,
    /// How long the ball has been where it is, counted from the newest
    /// message: the wait a `needs reply` card rises on, and the age a
    /// `replied` card decays from.
    pub wait_days: f64,
    /// The newest message in the unit: a new one voids a skip.
    pub latest: String,
    /// The newest message from someone else, which is what a verdict cursor
    /// is compared against. `None` when only the user has written here.
    pub newest_from_other: Option<String>,
    /// Whether somebody else has already answered in this run. A room where
    /// the talk is going on without the reader asks for them less, so a
    /// channel's card is lower again for it.
    pub others_replied: bool,
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
    threads: &'a HashMap<SlackUnit, SlackFacts>,
    now: chrono::DateTime<chrono::FixedOffset>,
    interactions: &'a HashMap<AgentId, i64>,
}

/// What a note lends the cards under it.
struct HeadingContext {
    /// The machine the heading is on. A card is made from a row on a host
    /// and stays about that host's agent, so the two travel together.
    host: HostId,
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
    Agent(rho_agents::AgentFacts, rho_agents::Attention),
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

/// One generated segment: a slice of a host document, or a generated
/// line (row or draft slot). Equality against the previous pass lets a
/// sync bail out before touching the editor at all.
///
/// A document slice's `id` is its stable identity across passes: a hash
/// of the title of the heading whose cut opens the slice (0 for the
/// slice that starts the document). The composition keys the excerpt on
pub struct Dashboard {
    editor: Entity<Editor>,
    /// One buffer per generated line key: read-only listing lines and
    /// writable reply drafts alike.
    buffers: HashMap<LineKey, Entity<Buffer>>,
    /// Non-owning references to the workspace-owned Desk source buffers.
    /// What the dealer reads: one host's nodes as the store client holds
    /// them, with no buffers and no editor behind them.
    ///
    /// This used to be `tree_hosts`, which the dashboard knew because it
    /// had just composed those nodes into a map. A card's facts are the
    /// store's, not a surface's, so they are read from the store — and the
    /// dealer keeps working when the map is gone.
    deal_hosts: BTreeMap<HostId, crate::candidates::HostNodes>,
    /// Reconciles the multibuffer to the generated spec by element
    /// identity, so unchanged excerpts — and cursors in them — survive.
    /// Stable composition keys per line, allocated once and never reused.
    /// One key per row, and a row is a thing in one of its places: a
    /// labelled thing has a row in its own place and one under each label.

    /// Generated rows in display order, from the last sync.
    /// What each generated key means, for cursor lookup.
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
    /// What each row of the map is drawn as, in the order the composition
    /// put them. A delta that keeps the shape redraws the rows it names
    /// out of this and leaves the rest of the map alone; without it the
    /// only way to move one row's hint was to draw every row again.
    #[cfg(test)]
    deal_taken: usize,
    #[cfg(test)]
    deal_patched: usize,
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
        DealerFacts {
            by_agent,
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
        host: HostId,
        source: &crate::candidates::HostNodes,
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
        let breadcrumb = source.breadcrumb(&heading.id);
        let room = breadcrumb.split(" › ").next().map(str::to_owned);
        let bindings = source
            .children(&heading.id)
            .filter_map(|node| node.agent())
            .collect::<Vec<_>>();
        Some(HeadingContext {
            host,
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
        source: &crate::candidates::HostNodes,
        agent_id: AgentId,
        context: &HeadingContext,
        order: usize,
        facts: &DealerFacts<'_>,
        carded: &mut HashSet<AgentId>,
    ) -> Option<RankedDealCard> {
        let host = context.host;
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
        // but the loudest disappeared from Home.
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

    /// Every agent a heading holds: the ones filed under it. An agent
    /// created by an agent is nobody's to deal (see `deal_agent_facts`),
    /// so a filing is never walked down into what its agent spawned.
    fn agent_cards_under(
        &self,
        source: &crate::candidates::HostNodes,
        context: &HeadingContext,
        order: usize,
        facts: &DealerFacts<'_>,
        carded: &mut HashSet<AgentId>,
    ) -> Vec<RankedDealCard> {
        let mut cards = Vec::new();
        for agent_id in &context.bindings {
            if let Some(card) = self.agent_card(source, *agent_id, context, order, facts, carded) {
                cards.push(card);
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
        // A handled, muted or deferred agent is the user's verdict on this
        // very card, and the verdict is in the store: a client that has not
        // read the store cannot say the user did not put this agent down
        // yesterday. It used to deal anyway — the guard below was written
        // as `is_some_and`, so no desk meant no verdict and the card went
        // out — which is how a snoozed agent was dealt again on every cold
        // open. Nothing is dealt until the client's replica is loaded,
        // which is off this disk and does not wait on a daemon; when it
        // is, the whole host is made again.
        let source = self
            .deal_hosts
            .get(&agent.host)
            .filter(|source| source.desk_loaded())?;
        let node = source.agent_node(agent.agent_id);
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
        let source = self.deal_hosts.get(&host)?;
        let node = source.node(&node_id);
        // An agent nobody has filed and that is not asking for the user has
        // no row on the desk, and a verdict over it is still about the
        // agent: `Id::Agent` names it whether or not anything was ever said
        // about it, which is what the transcript's own snooze writes.
        // Wanting a row here is what let Home open the verdicts over a
        // running agent and then refuse them.
        let agent_id = match node {
            Some(node) => node_agent(node),
            None => match &node_id {
                rho_desk::cells::Id::Agent(agent_id) => Some(*agent_id),
                _ => return None,
            },
        };
        let kind = match (node.is_some_and(|node| node.slack().is_some()), agent_id) {
            (true, _) => DealCardKind::Thread,
            (false, Some(_)) => DealCardKind::Agent,
            (false, None) => DealCardKind::Desk,
        };
        Some(DealCard {
            label: String::new(),
            priority: 0.,
            host,
            topic_node_id: node_id.clone(),
            agent_id,
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
        Some(self.deal_hosts.get(&host)?.breadcrumb(&node_id))
    }

    fn room_for_node(&self, host: HostId, mut node_id: rho_desk::cells::Id) -> Option<DeskRoom> {
        let source = self.deal_hosts.get(&host)?;
        loop {
            let node = source.node(&node_id)?;
            let Some(ref parent) = node.parent else { break };
            let parent_node = source.node(parent)?;
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

    /// Where an agent or a page is filed, as the path of headings above it.
    ///
    /// The map kept a heading-to-agents index as it composed, and these read
    /// it back; nothing composes now, so the row is found in the dealer's
    /// own source instead. Same answer, one source.
    fn filed_node(
        &self,
        find: impl Fn(&crate::candidates::HostNodes) -> Option<rho_desk::cells::Id>,
    ) -> Option<(HostId, rho_desk::cells::Id)> {
        self.deal_hosts
            .iter()
            .find_map(|(host, source)| find(source).map(|node_id| (*host, node_id)))
    }

    fn agent_node_id(&self, agent_id: AgentId) -> Option<(HostId, rho_desk::cells::Id)> {
        self.filed_node(|source| source.agent_node(agent_id).map(|node| node.id.clone()))
    }

    fn page_node_id(&self, page_id: rho_browser::PageId) -> Option<(HostId, rho_desk::cells::Id)> {
        let page = rho_desk::PageId(*page_id.0.as_bytes());
        self.filed_node(|source| source.page_node(page).map(|node| node.id.clone()))
    }

    pub fn breadcrumb_for_agent(&self, agent_id: AgentId, _cx: &App) -> Option<String> {
        let (host, node_id) = self.agent_node_id(agent_id)?;
        self.breadcrumb_for_node(host, node_id)
    }

    pub fn breadcrumb_for_page(&self, page_id: rho_browser::PageId, _cx: &App) -> Option<String> {
        let (host, node_id) = self.page_node_id(page_id)?;
        self.breadcrumb_for_node(host, node_id)
    }

    pub fn room_for_agent(&self, agent_id: AgentId, _cx: &App) -> Option<DeskRoom> {
        let (host, node_id) = self.agent_node_id(agent_id)?;
        self.room_for_node(host, node_id)
    }

    pub fn room_for_page(&self, page_id: rho_browser::PageId, _cx: &App) -> Option<DeskRoom> {
        let (host, node_id) = self.page_node_id(page_id)?;
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
            rho_window::editor_config::configure(&mut editor, window, cx);
            // Unlike the chat editors, clicking a row to put the cursor on
            // it is the whole point.
            editor.set_mouse_click_selection_enabled(true, cx);
            editor
        });
        Self {
            editor,
            buffers: HashMap::new(),
            deal_hosts: BTreeMap::new(),
            referenced_pages: HashSet::new(),
            new_draft: None,
            tree_new_draft_parent: None,
            phone_browse_mode: false,
            skipped: HashMap::new(),
            queue_depth: DealQueueDepth::default(),
            dealer: DealerSet::default(),
            pending_cursor: None,
            #[cfg(test)]
            deal_taken: 0,
            #[cfg(test)]
            deal_patched: 0,
        }
    }

    pub fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    pub fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.editor.read(cx).focus_handle(cx)
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

    /// The nodes the dealer deals from, read out of the store client.
    ///
    /// Held rather than rebuilt per card: making a hand walks a host's
    /// nodes once, and the incremental paths — one row's cards, one
    /// agent's card — are lookups against these indexes. Rebuilding the
    /// source for each of those would make every event cost the desk.
    /// Every id at or under a node, taken from the dealer's source. The
    /// map used to be asked this; the answer never needed a drawn row.
    pub fn subtree_ids(&self, host: HostId, id: &rho_desk::cells::Id) -> Vec<rho_desk::cells::Id> {
        let Some(source) = self.deal_hosts.get(&host) else {
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

    /// The first agent filed under a heading, which is what a card on that
    /// heading stands for. Answered from the dealer's source, which indexes
    /// the agents under each heading as it is built.
    pub fn first_agent_for_topic(&self, topic: (HostId, rho_desk::cells::Id)) -> Option<AgentId> {
        self.deal_hosts
            .get(&topic.0)
            .and_then(|source| source.agents_under(&topic.1).first())
            .copied()
    }

    /// The note a card's room is named after: the highest note above it
    /// that still hangs under notes. From the dealer's source.
    pub fn room_node(&self, card: &DealCard) -> Option<(HostId, rho_desk::cells::Id)> {
        let source = self.deal_hosts.get(&card.host)?;
        let mut node_id = card.topic_node_id.clone();
        loop {
            let node = source.node(&node_id)?;
            let Some(ref parent) = node.parent else {
                return Some((card.host, node_id));
            };
            let parent_node = source.node(parent)?;
            if !parent_node.is_note() {
                return Some((card.host, node_id));
            }
            node_id = parent.clone();
        }
    }

    pub(crate) fn set_deal_source(&mut self, host: HostId, source: crate::candidates::HostNodes) {
        #[cfg(test)]
        {
            self.deal_taken += 1;
        }
        self.deal_hosts.insert(host, source);
    }

    /// Whether the desk's nodes sit where the dealer's source has them.
    /// Asked before the source is built, because the answer decides whether
    /// building it is needed at all: a held shape means the delta is
    /// patched where it lands and the desk is not read again.
    pub(crate) fn deal_shape_held(
        &self,
        host: HostId,
        nodes: &[crate::desk_view::DeskNode],
    ) -> bool {
        self.deal_hosts
            .get(&host)
            .is_some_and(|held| held.same_shape_as(nodes))
    }

    /// The rows a delta named, copied into the dealer's source. Answers
    /// false when a named row is not where it was, which is the caller's
    /// cue to take the whole source again.
    pub(crate) fn patch_deal_source(
        &mut self,
        host: HostId,
        touched: &BTreeSet<rho_desk::cells::Id>,
        nodes: &[crate::desk_view::DeskNode],
    ) -> bool {
        #[cfg(test)]
        {
            self.deal_patched += 1;
        }
        self.deal_hosts
            .get_mut(&host)
            .is_some_and(|source| source.patch(touched, nodes))
    }

    /// What a card's node is, which decides the surface it opens.
    pub fn card_target(&self, card: DealCardId) -> CardTarget {
        let Some(node) = self
            .deal_hosts
            .get(&card.host)
            .and_then(|source| source.node(&card.node_id))
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
        self.deal_hosts
            .get(&card.host)?
            .node(&card.node_id)
            .and_then(node_unit)
    }

    /// Every open Slack thread node, with the thread it stands for. The
    /// backlog command needs them all at once rather than the one the
    /// cursor is on.
    pub fn open_thread_cards(&self) -> Vec<(DealCardId, SlackUnit)> {
        self.deal_hosts
            .iter()
            .flat_map(|(host, source)| {
                source
                    .nodes()
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
        self.deal_hosts
            .get(&card.host)
            .and_then(|source| source.node(&card.node_id))
            .is_some_and(|node| node.state == rho_desk::cells::State::Open)
    }

    /// Whether the user has put this agent away: muted, or snoozed to a
    /// time still ahead. Neither is a cursor, so nothing the agent does
    /// takes either back — this is the one question every list that draws
    /// agents asks, and it is the same one the dealer asks per card.
    pub(crate) fn agent_put_down(
        &self,
        agent_id: AgentId,
        now: chrono::DateTime<chrono::FixedOffset>,
    ) -> bool {
        // The verdict that put the agent away is in the store, so until the
        // client's replica is loaded there is nothing to read and the
        // honest answer is that the agent is not the reader's to see.
        // Answering "not put down" instead is how Home listed two snoozed
        // agents as running for the first half second of every cold open.
        if !self.deal_hosts.values().any(|source| source.desk_loaded()) {
            return true;
        }
        self.deal_hosts
            .values()
            .any(|source| agent_node_closed(source, agent_id, now))
    }

    /// When a card is put down until.
    pub fn node_defer_until(&self, card: DealCardId) -> Option<rho_desk::cells::Timestamp> {
        self.deal_hosts
            .get(&card.host)
            .and_then(|source| source.node(&card.node_id))
            .and_then(|node| node.defer_until)
    }

    fn node_card(
        &self,
        matches: impl Fn(&crate::desk_view::DeskNode) -> bool,
    ) -> Option<DealCardId> {
        self.deal_hosts.iter().find_map(|(host, source)| {
            source
                .nodes()
                .iter()
                .find(|node| matches(node))
                .map(|node| DealCardId {
                    host: *host,
                    node_id: node.id.clone(),
                })
        })
    }

    /// How many cards have been made in this dashboard's life.
    #[cfg(test)]
    pub(crate) fn cards_made_for_test(&self) -> usize {
        self.dealer.made
    }

    /// What the dealer's source cost: times it was taken whole, and times
    /// a delta was patched into it. One agent's news must patch and never
    /// take, or every event costs the desk.
    #[cfg(test)]
    pub(crate) fn deal_work_for_test(&self) -> (usize, usize) {
        (self.deal_taken, self.deal_patched)
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
        registry: &AgentMap,
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

    /// The cards of one row, made again. What it costs is the row and the
    /// agents it heads: a note that was deferred or closed stops lending
    /// its subtree a place in the hand, and that is the whole of what one
    /// verdict on a heading can move.
    fn remake_node_cards(
        &mut self,
        host: HostId,
        node_id: &rho_desk::cells::Id,
        registry: &AgentMap,
        threads: &HashMap<SlackUnit, SlackFacts>,
        now: chrono::DateTime<chrono::FixedOffset>,
        agent_interactions: &HashMap<AgentId, i64>,
    ) {
        let identity = DealCardId {
            host,
            node_id: node_id.clone(),
        };
        self.dealer.retire(&identity);
        let Some(source) = self.deal_hosts.get(&host) else {
            return;
        };
        let Some(order) = source.order_of(node_id) else {
            return;
        };
        let node = source.nodes()[order].clone();
        // The row's own cards need no agent facts: a dated mark is the
        // note's, and a thread's wait is the mirror's.
        let facts = DealerFacts {
            by_agent: HashMap::new(),
            threads,
            now,
            interactions: agent_interactions,
        };
        let made = if node.is_note() {
            match self.heading_context(host, source, &node, now) {
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
        // the ones already carded there.
        let mut agents = self
            .deal_hosts
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
        registry: &AgentMap,
        threads: &HashMap<SlackUnit, SlackFacts>,
        now: chrono::DateTime<chrono::FixedOffset>,
        agent_interactions: &HashMap<AgentId, i64>,
    ) {
        for id in self.dealer.of_host.remove(&host).unwrap_or_default() {
            self.dealer.cards.remove(&id);
        }
        self.dealer.of_agent.retain(|_, id| id.host != host);
        let Some(source) = self.deal_hosts.get(&host) else {
            return;
        };
        let agents = deal_agent_facts(registry);
        let facts = self.dealer_facts(threads, now, agent_interactions, &agents);
        let mut made = Vec::new();
        // Filing decides where a card is shown, never whether it exists, so
        // an agent reached through a note is only skipped by the pass below
        // to keep it from being carded twice.
        let mut carded = HashSet::new();
        for (order, node) in source.nodes().iter().enumerate() {
            if node.is_note() {
                let Some(context) = self.heading_context(host, source, node, now) else {
                    continue;
                };
                made.extend(self.desk_cards(host, node, &context, order, &facts));
                made.extend(self.agent_cards_under(source, &context, order, &facts, &mut carded));
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
        registry: &AgentMap,
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
            threads: &EMPTY_THREADS,
            now,
            interactions: agent_interactions,
        };
        let order = self.agent_order(host, agent_id);
        let mut carded = HashSet::new();
        // Where the card is shown: the note this agent is filed under. A
        // note that is closed or waiting reaches nothing, and
        // a host with no desk yet reaches nothing either, which is what
        // makes the card a loose one at the root.
        let context = self
            .deal_hosts
            .get(&host)
            .and_then(|source| self.heading_for_agent(host, source, agent_id, now));
        let card = match (context, self.deal_hosts.get(&host)) {
            (Some(context), Some(source)) => {
                self.agent_card(source, agent_id, &context, order, &facts, &mut carded)
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
        self.deal_hosts
            .get(&host)
            .map(|source| source.agent_order(agent_id))
            .unwrap_or(usize::MAX)
    }

    /// The note an agent's card hangs under: the one its row is filed
    /// under. `None` when no live note reaches it, which makes it a loose
    /// card at the root.
    fn heading_for_agent(
        &self,
        host: HostId,
        source: &crate::candidates::HostNodes,
        agent_id: AgentId,
        now: chrono::DateTime<chrono::FixedOffset>,
    ) -> Option<HeadingContext> {
        let node = source.agent_node(agent_id)?;
        let heading = source.node(node.parent.as_ref()?)?;
        if !heading.is_note() {
            return None;
        }
        self.heading_context(host, source, heading, now)
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
        for source in self.deal_hosts.values() {
            for node in source.nodes() {
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
        fn identity(card: &DealCardId) -> rho_journal::DealerCardIdentity {
            rho_journal::DealerCardIdentity {
                host: card.host.0,
                node_id: card.node_id.clone().into(),
            }
        }
        let kind = match event.kind {
            DealCardKind::Desk => rho_journal::DealerCardKind::Note,
            DealCardKind::Agent => rho_journal::DealerCardKind::Agent,
            DealCardKind::Thread => rho_journal::DealerCardKind::Thread,
        };
        let verdict = match event.verdict {
            DealerVerdict::Skip => rho_journal::DealerVerdict::Skip,
            DealerVerdict::Done => rho_journal::DealerVerdict::Done,
            DealerVerdict::Mute => rho_journal::DealerVerdict::Mute,
            DealerVerdict::Defer => rho_journal::DealerVerdict::Defer,
            DealerVerdict::Open => rho_journal::DealerVerdict::Open,
            DealerVerdict::File => rho_journal::DealerVerdict::File,
        };
        rho_journal::record(rho_journal::Event::Dealer {
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
            let subscription = cx.subscribe_in(&buffer, window, |this, _, event, _window, cx| {
                if matches!(event, language::BufferEvent::Edited { .. }) {
                    this.refresh_dashboard(cx);
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

    /// Regenerates the listing: the host documents are sliced at bound
    /// headings, generated rows and drafts are interleaved between the
    /// slices, and highlights and lamps reapplied. The cursor follows
    /// its buffer through the rearrangement.
    pub fn sync(&mut self, agent_interactions: &HashMap<AgentId, i64>) {
        self.sync_hand(agent_interactions);
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

    pub fn hint(&self, _cx: &mut Context<Workspace>) -> String {
        format!(
            "{} dealt · {} waiting",
            self.queue_depth.dealt_count, self.queue_depth.total_alive
        )
    }
}

/// Gutter highlight marker type for reply drafts.
pub struct ReplyGutter;

/// One generated dashboard line: identity, text, semantic spans, and
/// the object addressed by dashboard verbs.
#[derive(Clone, Debug, PartialEq)]
pub struct DealAgentFacts {
    pub agent_id: AgentId,
    pub host: HostId,
    pub heading: String,
    pub facts: rho_agents::AgentFacts,
    pub attention: rho_agents::Attention,
}

/// One agent's facts, for a remake that names exactly it. `None` for an
/// agent created by an agent: that one belongs to its creator and is
/// never dealt, so its remake makes nothing.
fn agent_deal_facts(registry: &AgentMap, agent_id: AgentId) -> Option<DealAgentFacts> {
    if !registry.created_by_user(agent_id) {
        return None;
    }
    Some(DealAgentFacts {
        agent_id,
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

/// Every agent the dealer may card: the ones the user created. An agent
/// created by an agent belongs to its creator and is left out here, which
/// is the one place the rule is applied for every card the dealer makes;
/// its waiting reaches the user through its creator's card.
fn deal_agent_facts(registry: &AgentMap) -> Vec<DealAgentFacts> {
    registry
        .known_agents()
        .filter(|agent_id| registry.created_by_user(**agent_id))
        .filter_map(|agent_id| {
            let host = registry.host_of_agent(*agent_id)?;
            Some(DealAgentFacts {
                agent_id: *agent_id,
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
    facts: &rho_agents::AgentFacts,
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
    facts: &rho_agents::AgentFacts,
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
fn outcome_label(facts: &rho_agents::AgentFacts, wait_days: f64) -> String {
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
    source: &crate::candidates::HostNodes,
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

/// A note's title is the first line of its body. The rest of the body is
/// the note itself: it belongs on the note's own surface, never in a path,
/// a card, or a picker row.
pub(crate) fn note_title(text: &str) -> &str {
    text.lines().next().unwrap_or("").trim()
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

/// What a thread's card says and how hard it pushes. Why it is here, then
/// whose turn it is, then how long it has been that way. Somebody waiting
/// outranks a note of the same age, the way a blocked agent outranks an FYI.
///
/// The reason is made here rather than carried, out of the fact and the
/// conversation's name as it reads now: a card that says `mentioned in
/// #design` is a card the reader can answer without opening it.
pub(crate) fn thread_card_facts(
    thread: &SlackFacts,
    now: chrono::DateTime<chrono::FixedOffset>,
) -> (String, f64) {
    let _ = now;
    let reason = thread
        .reason
        .map(|reason| rho_slack::model::reason_text(reason, &thread.conversation));
    // A room is not a person waiting: it fades from the moment it is seen,
    // and lower again when somebody else is already answering in it. A
    // mention in the same room is not this card — it carries `Mentioned`
    // and rises like any other ping.
    let (state, priority) = match thread.reason {
        Some(rho_slack::model::Attention::ChannelTraffic) => {
            let answered = match thread.others_replied {
                true => CHANNEL_ANSWERED_DROP,
                false => 0.0,
            };
            (
                "unread",
                CHANNEL_TRAFFIC_HEAD_START - answered - thread.wait_days / FYI_REPLY_PACE_DAYS,
            )
        }
        _ => (
            "needs reply",
            THREAD_REPLY_HEAD_START + BLOCKED_REPLY_SLOPE_PER_DAY * thread.wait_days,
        ),
    };
    let age = age_label(thread.wait_days);
    let line = match reason {
        Some(reason) => format!("{reason} · {state} · {age}"),
        None => format!("{state} · {age}"),
    };
    (line, priority)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name the user wrote wins over the name the source gave, on any
    /// id: a Slack conversation they renamed reads as what they called it,
    /// and one they have not is still named by the mirror.

    #[test]
    fn a_slack_thread_deals_like_an_agent_waiting_on_a_reply() {
        let now = chrono::NaiveDate::from_ymd_opt(2026, 8, 23)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
            .fixed_offset();
        let thread = |wait_days: f64| SlackFacts {
            title: "can you look at the deploy?".into(),
            conversation: "#design".into(),
            raised_at: now - chrono::Duration::days(2),
            wait_days,
            latest: "500.0".into(),
            newest_from_other: Some("500.0".into()),
            others_replied: false,
            reason: Some(rho_slack::model::Attention::FollowedThread),
        };

        let (label, priority) = thread_card_facts(&thread(2.0), now);
        assert_eq!(
            label, "a reply in a followed thread in #design · needs reply · 2.0d",
            "the card says why it is here before it says whose turn it is"
        );
        // Nothing addressed to the machine reaches what the reader sees.
        assert!(!label.contains("C1"));
        assert!(!label.contains("500.0"));
        assert_eq!(
            priority,
            THREAD_REPLY_HEAD_START + 2.0 * BLOCKED_REPLY_SLOPE_PER_DAY
        );
    }

    /// A room is not a person waiting. It says `unread`, it starts far
    /// below a thread of any age, it fades instead of rising, and it drops
    /// further still when the talk is going on without the reader.
    ///
    /// There is no `replied` card any more: a unit the reader answered has
    /// no attention at all until somebody answers back, which the model
    /// says by giving it no reason.
    #[test]
    fn a_channel_fades_and_never_overtakes_a_thread() {
        let now = chrono::NaiveDate::from_ymd_opt(2026, 8, 23)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
            .fixed_offset();
        let room = |wait_days: f64, others_replied: bool| SlackFacts {
            title: "deploy is green".into(),
            conversation: "#random".into(),
            raised_at: now - chrono::Duration::days(2),
            wait_days,
            latest: "500.0".into(),
            newest_from_other: Some("500.0".into()),
            others_replied,
            reason: Some(rho_slack::model::Attention::ChannelTraffic),
        };
        let thread = |wait_days: f64| SlackFacts {
            reason: Some(rho_slack::model::Attention::FollowedThread),
            conversation: "#design".into(),
            ..room(wait_days, false)
        };

        let (label, fresh) = thread_card_facts(&room(0.0, false), now);
        assert_eq!(label, "unread in #random · unread · 0m");
        assert_eq!(fresh, CHANNEL_TRAFFIC_HEAD_START);

        // Under the floor in four days, and in a little over two when
        // somebody else is already answering in there.
        assert!(thread_card_facts(&room(3.0, false), now).1 > DEAL_QUEUE_FLOOR);
        assert!(thread_card_facts(&room(4.0, false), now).1 <= DEAL_QUEUE_FLOOR);
        assert!(thread_card_facts(&room(2.0, true), now).1 > DEAL_QUEUE_FLOOR);
        assert!(thread_card_facts(&room(2.5, true), now).1 <= DEAL_QUEUE_FLOOR);
        assert!(
            thread_card_facts(&room(1.0, true), now).1
                < thread_card_facts(&room(1.0, false), now).1,
            "a room being answered without the reader asks for them less"
        );

        // The oldest room there can be against the freshest thread there
        // can be: the room is still below it, because the curves never
        // cross.
        assert!(fresh < thread_card_facts(&thread(0.0), now).1);
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
