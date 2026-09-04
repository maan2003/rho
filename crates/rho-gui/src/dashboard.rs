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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use editor::scroll::Autoscroll;
use editor::{Editor, EditorMode, HighlightKey, Inlay, SelectionEffects, SizingBehavior};
use gpui::prelude::*;
use gpui::{App, Context, Entity, Focusable as _, HighlightStyle, Window};
use language::{Buffer, Capability, InlayId};
use multi_buffer::MultiBuffer;
use multi_buffer::composition::{Composition, CompositionSpec, RowSpec};
use rho_ui_proto::{AgentId, UiAttention};
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
const SKIP_COOLDOWN_MINUTES: i64 = 15;
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
        skip_cooldown_minutes: SKIP_COOLDOWN_MINUTES,
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
    Thread(ThreadRef),
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

/// What a Slack thread node is currently about, read live from the mirror.
/// The tree holds the thread's identity and its verdicts; the words, the
/// wait, and the newest message stay in Slack.
#[derive(Clone, Debug, PartialEq)]
pub struct DealerThread {
    pub title: String,
    pub conversation: String,
    pub raised_at: chrono::DateTime<chrono::FixedOffset>,
    /// How long the ball has been where it is, counted from the newest
    /// message: the wait a `needs reply` card rises on, and the age a
    /// `replied` card decays from.
    pub wait_days: f64,
    /// Who the ball is with, when it is not the user.
    pub waiting_on: Option<String>,
    /// The newest message in the thread: a new one voids a skip.
    pub latest: String,
}

/// A Slack thread's address, as the tree stores it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ThreadRef {
    pub workspace: String,
    pub channel: String,
    pub thread_ts: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DealerVerdict {
    Skip,
    Done,
    Dismiss,
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
    pub time_to_verdict_ms: u64,
    pub considered_not_dealt: Vec<DealCardId>,
    pub skip_until: Option<chrono::DateTime<chrono::FixedOffset>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DealQueue {
    pub cards: Vec<DealCard>,
    /// Number of live headings whose winning mark is above the queue floor.
    pub total_alive: usize,
    /// Number selected by global priority.
    pub dealt_count: usize,
    considered_not_dealt: Vec<DealCardId>,
    fingerprints: HashMap<DealCardId, DealFingerprint>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DealQueueDepth {
    pub dealt_count: usize,
    pub total_alive: usize,
}

#[derive(Clone, Debug)]
struct DealSession {
    card: DealCard,
    fingerprint: DealFingerprint,
    started_at: Instant,
    considered_not_dealt: Vec<DealCardId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DealFingerprint(String);

#[derive(Clone, Debug)]
struct SkippedCard {
    at: chrono::DateTime<chrono::FixedOffset>,
    fingerprint: DealFingerprint,
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
    tree_element_keys: HashMap<(HostId, rho_desk::cells::Id), u64>,
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
    deal_active: bool,
    deal: Option<DealSession>,
    skipped: HashMap<DealCardId, SkippedCard>,
    deal_empty_success: bool,
    queue_depth: DealQueueDepth,
    /// Portal occurrences whose complete runtime subtree is visible.
    /// This is transient display state and is never written to Desk.
    /// Move the cursor into this key's buffer on the next sync — how a
    /// freshly opened reply draft receives the cursor.
    pending_cursor: Option<LineKey>,
    /// Move the cursor to this document offset on the next sync.
    /// Reply placeholder inlays currently spliced in.
    tree_inlay_ids: Vec<InlayId>,
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

    pub fn tree_heading_named(
        &self,
        title: &str,
        cx: &App,
    ) -> Option<(HostId, rho_desk::cells::Id)> {
        self.tree_hosts.iter().find_map(|(host, source)| {
            source.nodes.iter().find_map(|node| {
                (node.is_note()
                    && source
                        .buffers
                        .get(&node.id)
                        .is_some_and(|buffer| note_title(&buffer.read(cx).text()) == title.trim()))
                .then_some((*host, node.id.clone()))
            })
        })
    }

    fn tree_dealer_queue(
        &self,
        registry: &AgentRegistry,
        threads: &HashMap<ThreadRef, DealerThread>,
        now: chrono::DateTime<chrono::FixedOffset>,
        agent_interactions: &HashMap<AgentId, i64>,
        cx: &App,
    ) -> DealQueue {
        let facts = deal_agent_facts(registry);
        let by_agent = facts
            .iter()
            .map(|facts| (facts.agent_id, facts))
            .collect::<HashMap<_, _>>();
        let mut ranked = Vec::new();
        let mut order = 0usize;
        for (host, source) in &self.tree_hosts {
            let nodes = source
                .nodes
                .iter()
                .map(|node| (node.id.clone(), node))
                .collect::<HashMap<_, _>>();
            let titles = source
                .buffers
                .iter()
                .map(|(id, buffer)| (id.clone(), note_title(&buffer.read(cx).text()).to_owned()))
                .collect::<HashMap<_, _>>();
            for heading in source.nodes.iter().filter(|node| node.is_note()) {
                let terminal = heading.state != rho_desk::cells::State::Open;
                let ancestor_deferred = std::iter::successors(heading.parent.clone(), |parent| {
                    nodes.get(parent).and_then(|node| node.parent.clone())
                })
                .filter_map(|parent| nodes.get(&parent))
                .any(|node| desk_deferred(node, now.naive_local()));
                let locally_deferred = desk_deferred(heading, now.naive_local());
                if terminal || ancestor_deferred || locally_deferred {
                    order += 1;
                    continue;
                }
                let breadcrumb = tree_breadcrumb(&heading.id, &nodes, &titles);
                let room = breadcrumb.split(" › ").next().map(str::to_owned);
                let bindings = source
                    .nodes
                    .iter()
                    .filter(|node| node.parent == Some(heading.id.clone()))
                    .filter_map(|node| Some((node.id.clone(), node.agent()?)))
                    .collect::<Vec<_>>();
                for (mark, at) in desk_marks(heading) {
                    let priority =
                        desk_mark_priority(mark, at, heading.pace_days, now.naive_local());
                    if priority <= DEAL_QUEUE_FLOOR {
                        continue;
                    }
                    let identity = DealCardId {
                        host: *host,
                        node_id: heading.id.clone(),
                    };
                    ranked.push(RankedDealCard {
                        priority,
                        virtual_reply: false,
                        order,
                        fingerprint: DealFingerprint(format!("{mark:?}:{at:?}")),
                        card: DealCard {
                            label: desk_mark_label(mark, at, now.naive_local()),
                            priority,
                            host: *host,
                            topic_node_id: heading.id.clone(),
                            agent_id: bindings.first().map(|(_, id)| *id),
                            agent_tag: None,
                            breadcrumb: breadcrumb.clone(),
                            room: room.clone(),
                            kind: DealCardKind::Desk,
                            identity,
                        },
                    });
                }
                // The old model gated a topic's agent cards on its todo
                // mark not being ripe yet. With marks as fields the same
                // gate is the note being deferred, which is checked above.
                {
                    for (machine_node_id, root_agent) in bindings {
                        let mut agents = vec![root_agent];
                        let mut cursor = 0;
                        while cursor < agents.len() {
                            let parent = agents[cursor];
                            agents.extend(
                                facts
                                    .iter()
                                    .filter(|agent| agent.parent == Some(parent))
                                    .map(|agent| agent.agent_id),
                            );
                            cursor += 1;
                        }
                        for agent_id in agents {
                            let Some(agent) = by_agent.get(&agent_id).copied() else {
                                continue;
                            };
                            let Some(ended) = agent.facts.last_turn_ended else {
                                continue;
                            };
                            if agent.facts.turn_running || ended <= agent.facts.last_user_message_at
                            {
                                continue;
                            }
                            let wait_days = reply_wait_days(ended, now);
                            let (base_priority, label) = if agent.facts.needs_you_hint {
                                (
                                    blocked_reply_priority(wait_days),
                                    format!("waiting on reply · {}", age_label(wait_days)),
                                )
                            } else {
                                (
                                    fyi_reply_priority(wait_days),
                                    format!("finished · {} ago", age_label(wait_days)),
                                )
                            };
                            let recency_bonus =
                                agent_interactions.get(&agent_id).map_or(0.0, |last| {
                                    let elapsed = (now.timestamp_millis() - *last)
                                        .clamp(0, AGENT_RECENCY_WINDOW_MS);
                                    let remaining =
                                        1.0 - elapsed as f64 / AGENT_RECENCY_WINDOW_MS as f64;
                                    AGENT_RECENCY_BONUS * remaining * remaining
                                });
                            let priority = base_priority + recency_bonus;
                            if priority <= DEAL_QUEUE_FLOOR {
                                continue;
                            }
                            ranked.push(RankedDealCard {
                                priority,
                                virtual_reply: true,
                                order,
                                fingerprint: DealFingerprint(format!(
                                    "{:?}:{:?}",
                                    agent.facts, agent.attention
                                )),
                                card: DealCard {
                                    label,
                                    priority,
                                    host: *host,
                                    topic_node_id: heading.id.clone(),
                                    agent_id: Some(agent_id),
                                    agent_tag: None,
                                    breadcrumb: breadcrumb.clone(),
                                    room: room.clone(),
                                    kind: DealCardKind::Agent,
                                    identity: DealCardId {
                                        host: *host,
                                        node_id: machine_node_id.clone(),
                                    },
                                },
                            });
                        }
                    }
                }
                order += 1;
            }
            // Slack threads that started to matter are nodes like any other,
            // so they rank in the same queue. What the card says comes from
            // the mirror: the tree holds the thread's identity and verdicts,
            // never its words.
            for node in source
                .nodes
                .iter()
                .filter(|node| node.slack().is_some_and(|unit| unit.thread.is_some()))
            {
                if node.state != rho_desk::cells::State::Open
                    || desk_deferred(node, now.naive_local())
                {
                    order += 1;
                    continue;
                }
                let Some(thread) = node_thread(node).and_then(|key| threads.get(&key)) else {
                    order += 1;
                    continue;
                };
                let (label, priority) = thread_card_facts(&thread, now);
                if priority <= DEAL_QUEUE_FLOOR {
                    order += 1;
                    continue;
                }
                ranked.push(RankedDealCard {
                    priority,
                    virtual_reply: false,
                    order,
                    fingerprint: DealFingerprint(thread.latest.clone()),
                    card: DealCard {
                        label,
                        priority,
                        host: *host,
                        topic_node_id: node.id.clone(),
                        agent_id: None,
                        agent_tag: None,
                        breadcrumb: thread.title.clone(),
                        room: Some(thread.conversation.clone()),
                        kind: DealCardKind::Thread,
                        identity: DealCardId {
                            host: *host,
                            node_id: node.id.clone(),
                        },
                    },
                });
                order += 1;
            }
        }
        // One winning card per topic; a virtual reply wins an exact tie.
        let mut by_topic = HashMap::new();
        for candidate in ranked {
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
        let mut ranked = by_topic
            .into_values()
            .filter(|ranked| {
                self.skipped.get(&ranked.card.identity).is_none_or(|skip| {
                    now >= skip.at + chrono::Duration::minutes(SKIP_COOLDOWN_MINUTES)
                        || skip.fingerprint != ranked.fingerprint
                })
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|a, b| {
            b.priority
                .total_cmp(&a.priority)
                .then_with(|| b.virtual_reply.cmp(&a.virtual_reply))
                .then_with(|| a.order.cmp(&b.order))
        });
        let queue = DealQueue {
            total_alive: ranked.len(),
            dealt_count: usize::from(!ranked.is_empty()),
            considered_not_dealt: ranked
                .iter()
                .skip(1)
                .take(5)
                .map(|ranked| ranked.card.identity.clone())
                .collect(),
            fingerprints: ranked
                .iter()
                .map(|ranked| (ranked.card.identity.clone(), ranked.fingerprint.clone()))
                .collect(),
            cards: ranked.into_iter().map(|ranked| ranked.card).collect(),
        };
        queue
    }

    fn breadcrumb_for_node(
        &self,
        host: HostId,
        node_id: rho_desk::cells::Id,
        cx: &App,
    ) -> Option<String> {
        let source = self.tree_hosts.get(&host)?;
        let nodes = source
            .nodes
            .iter()
            .map(|node| (node.id.clone(), node))
            .collect::<HashMap<_, _>>();
        let titles = source
            .buffers
            .iter()
            .map(|(id, buffer)| (id.clone(), note_title(&buffer.read(cx).text()).to_owned()))
            .collect::<HashMap<_, _>>();
        Some(tree_breadcrumb(&node_id, &nodes, &titles))
    }

    fn room_for_node(
        &self,
        host: HostId,
        mut node_id: rho_desk::cells::Id,
        cx: &App,
    ) -> Option<DeskRoom> {
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
        let name = note_title(&source.buffers.get(&node_id)?.read(cx).text()).to_owned();
        Some(DeskRoom {
            host,
            node_id,
            name,
        })
    }

    pub fn cursor_room(&self, cx: &mut Context<Workspace>) -> Option<DeskRoom> {
        let (host, node_id) = self.cursor_topic(cx)?;
        self.room_for_node(host, node_id, cx)
    }

    pub fn cursor_breadcrumb(&self, cx: &mut Context<Workspace>) -> Option<String> {
        let (host, node_id) = self.cursor_topic(cx)?;
        self.breadcrumb_for_node(host, node_id, cx)
    }

    pub fn breadcrumb_for_agent(&self, agent_id: AgentId, cx: &App) -> Option<String> {
        let (host, node_id) = self
            .tree_heading_agents
            .iter()
            .find_map(|(topic, agents)| agents.contains(&agent_id).then_some(topic.clone()))?;
        self.breadcrumb_for_node(host, node_id, cx)
    }

    pub fn breadcrumb_for_page(&self, page_id: rho_browser::PageId, cx: &App) -> Option<String> {
        let (host, node_id) = self
            .tree_heading_pages
            .iter()
            .find_map(|(topic, pages)| pages.contains(&page_id).then_some(topic.clone()))?;
        self.breadcrumb_for_node(host, node_id, cx)
    }

    pub fn room_for_agent(&self, agent_id: AgentId, cx: &App) -> Option<DeskRoom> {
        let (host, node_id) = self
            .tree_heading_agents
            .iter()
            .find_map(|(topic, agents)| agents.contains(&agent_id).then_some(topic.clone()))?;
        self.room_for_node(host, node_id, cx)
    }

    pub fn room_for_page(&self, page_id: rho_browser::PageId, cx: &App) -> Option<DeskRoom> {
        let (host, node_id) = self
            .tree_heading_pages
            .iter()
            .find_map(|(topic, pages)| pages.contains(&page_id).then_some(topic.clone()))?;
        self.room_for_node(host, node_id, cx)
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
            deal_active: false,
            deal: None,
            skipped: HashMap::new(),
            deal_empty_success: false,
            queue_depth: DealQueueDepth::default(),
            pending_cursor: None,
            tree_inlay_ids: Vec::new(),
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
        self.tree_hosts
            .insert(host, TreeHostSource { nodes, buffers });
    }

    /// What a card's node is, which decides the surface it opens.
    pub fn card_target(&self, card: DealCardId) -> CardTarget {
        let Some(node) = self
            .tree_hosts
            .get(&card.host)
            .and_then(|source| source.nodes.iter().find(|node| node.id == card.node_id))
        else {
            return CardTarget::Missing;
        };
        match &node.id {
            rho_desk::cells::Id::Agent(_) => {
                node.agent().map_or(CardTarget::Missing, CardTarget::Agent)
            }
            rho_desk::cells::Id::Page(_) => {
                node_page(node).map_or(CardTarget::Missing, CardTarget::Page)
            }
            rho_desk::cells::Id::Slack(_) => {
                node_thread(node).map_or(CardTarget::Missing, CardTarget::Thread)
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

    pub fn thread_card_id(&self, thread: &ThreadRef) -> Option<DealCardId> {
        self.node_card(|node| node_thread(node).as_ref() == Some(thread))
    }

    /// The thread a card stands for, if it is a thread card at all.
    pub fn card_thread(&self, card: DealCardId) -> Option<ThreadRef> {
        self.tree_hosts
            .get(&card.host)?
            .nodes
            .iter()
            .find(|node| node.id == card.node_id)
            .and_then(node_thread)
    }

    /// Every open Slack thread node, with the thread it stands for. The
    /// backlog command needs them all at once rather than the one the
    /// cursor is on.
    pub fn open_thread_cards(&self) -> Vec<(DealCardId, ThreadRef)> {
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
                            node_thread(node)?,
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

    pub fn deal_mode(&self) -> bool {
        self.deal_active
    }

    pub fn deal_waiting(&self) -> usize {
        usize::from(self.deal.is_some())
    }

    #[cfg(test)]
    pub fn deal_highlight_active_for_test(&self, cx: &App) -> bool {
        self.editor
            .read(cx)
            .highlighted_rows::<DealCardHighlight>(cx)
            .next()
            .is_some()
    }

    pub fn dealer_hand(
        &self,
        registry: &AgentRegistry,
        threads: &HashMap<ThreadRef, DealerThread>,
        now: chrono::DateTime<chrono::FixedOffset>,
        agent_interactions: &HashMap<AgentId, i64>,
        cx: &App,
    ) -> DealQueue {
        self.tree_dealer_queue(registry, threads, now, agent_interactions, cx)
    }

    /// Re-evaluates the complete dealer world and presents its highest-scoring
    /// claim. There is deliberately no retained hand: each pull sees current
    /// Desk text, agent facts, inbox state, and cooldowns.
    pub fn pull_deal(
        &mut self,
        registry: &AgentRegistry,
        threads: &HashMap<ThreadRef, DealerThread>,
        now: chrono::DateTime<chrono::FixedOffset>,
        exclude: Option<&DealCardId>,
        agent_interactions: &HashMap<AgentId, i64>,
        cx: &mut Context<Workspace>,
    ) -> Option<DealCard> {
        let hand = self.dealer_hand(registry, threads, now, agent_interactions, cx);
        let (card, fingerprint, considered_not_dealt) = select_deal(&hand, exclude)?;
        Some(self.begin_deal(card, fingerprint, considered_not_dealt))
    }

    /// Deals a card the user picked out of the hand — a Home row — instead
    /// of the top of it. Everything else about the session is the same:
    /// the surface, the verdict keys, undo and the timeline cannot tell
    /// the two apart.
    pub fn deal_chosen(
        &mut self,
        registry: &AgentRegistry,
        threads: &HashMap<ThreadRef, DealerThread>,
        now: chrono::DateTime<chrono::FixedOffset>,
        wanted: &DealCardId,
        agent_interactions: &HashMap<AgentId, i64>,
        cx: &mut Context<Workspace>,
    ) -> Option<DealCard> {
        let hand = self.dealer_hand(registry, threads, now, agent_interactions, cx);
        let at = hand
            .cards
            .iter()
            .position(|card| card.identity == *wanted)?;
        let card = hand.cards[at].clone();
        let fingerprint = hand.fingerprints.get(&card.identity)?.clone();
        let considered_not_dealt = hand
            .cards
            .iter()
            .skip(at + 1)
            .take(5)
            .map(|card| card.identity.clone())
            .collect();
        Some(self.begin_deal(card, fingerprint, considered_not_dealt))
    }

    fn begin_deal(
        &mut self,
        card: DealCard,
        fingerprint: DealFingerprint,
        considered_not_dealt: Vec<DealCardId>,
    ) -> DealCard {
        self.deal = Some(DealSession {
            card: card.clone(),
            fingerprint,
            started_at: Instant::now(),
            considered_not_dealt,
        });
        self.raw_mode = false;
        self.deal_active = true;
        self.deal_empty_success = false;
        self.pending_tree_cursor = Some((card.host, card.topic_node_id.clone(), 0));
        card
    }

    pub fn reopen_deal(&mut self, card: DealCard) {
        self.pending_tree_cursor = Some((card.host, card.topic_node_id.clone(), 0));
        self.deal = Some(DealSession {
            card,
            fingerprint: DealFingerprint("verdict undo".to_owned()),
            started_at: Instant::now(),
            considered_not_dealt: Vec::new(),
        });
        self.raw_mode = false;
        self.deal_active = true;
        self.deal_empty_success = false;
    }

    pub fn end_deal(&mut self, cx: &mut Context<Workspace>) -> bool {
        self.deal = None;
        self.exit_deal_mode(cx)
    }

    pub fn exit_deal_mode(&mut self, _cx: &mut Context<Workspace>) -> bool {
        self.deal = None;
        std::mem::take(&mut self.deal_active)
    }

    pub fn discard_deal_session(&mut self, cx: &mut Context<Workspace>) {
        self.end_deal(cx);
    }

    pub fn deal_accepts_verdict(&self) -> bool {
        self.deal_active && self.deal.is_some()
    }

    pub fn record_deal_verdict_as(
        &mut self,
        verdict: DealerVerdict,
        now: chrono::DateTime<chrono::FixedOffset>,
    ) {
        let Some(event) = self.prepare_deal_verdict(verdict, now) else {
            return;
        };
        self.record_dealer_event(event);
    }

    pub fn prepare_deal_verdict(
        &self,
        verdict: DealerVerdict,
        now: chrono::DateTime<chrono::FixedOffset>,
    ) -> Option<DealerEvent> {
        let deal = self.deal.as_ref()?;
        let skip_until = (verdict == DealerVerdict::Skip)
            .then(|| now + chrono::Duration::minutes(SKIP_COOLDOWN_MINUTES));
        Some(DealerEvent {
            card: deal.card.identity.clone(),
            kind: deal.card.kind,
            verdict,
            at: now,
            time_to_verdict_ms: deal
                .started_at
                .elapsed()
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
            considered_not_dealt: deal.considered_not_dealt.clone(),
            skip_until,
        })
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
            DealerVerdict::Dismiss => crate::journal::DealerVerdict::Dismiss,
            DealerVerdict::Defer => crate::journal::DealerVerdict::Defer,
            DealerVerdict::Open => crate::journal::DealerVerdict::Open,
            DealerVerdict::File => crate::journal::DealerVerdict::File,
        };
        crate::journal::record(crate::journal::Event::Dealer {
            card: identity(&event.card),
            kind,
            verdict,
            occurred_at: event.at.to_rfc3339(),
            time_to_verdict_ms: event.time_to_verdict_ms,
            considered_not_dealt: event.considered_not_dealt.iter().map(identity).collect(),
            skip_until: event.skip_until.map(|until| until.to_rfc3339()),
        });
    }

    pub fn skip_card(
        &mut self,
        identity: DealCardId,
        now: chrono::DateTime<chrono::FixedOffset>,
        _cx: &mut Context<Workspace>,
    ) -> bool {
        let Some(fingerprint) = self
            .deal
            .as_ref()
            .filter(|deal| deal.card.identity == identity)
            .map(|deal| deal.fingerprint.clone())
        else {
            return false;
        };
        self.skipped.insert(
            identity,
            SkippedCard {
                at: now,
                fingerprint,
            },
        );
        self.record_deal_verdict_as(DealerVerdict::Skip, now);
        true
    }

    pub fn clear_skip(&mut self, identity: &DealCardId) -> bool {
        self.skipped.remove(identity).is_some()
    }

    #[cfg(test)]
    pub fn has_skip_for_test(&self, identity: &DealCardId) -> bool {
        self.skipped.contains_key(identity)
    }

    pub fn current_deal_card(&self) -> Option<&DealCard> {
        Some(&self.deal.as_ref()?.card)
    }

    pub fn current_tree_room_node(&self) -> Option<(HostId, rho_desk::cells::Id)> {
        let card = self.current_deal_card()?;
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
        threads: &HashMap<ThreadRef, DealerThread>,
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
        // live metadata every reconcile.
        for source in self.tree_hosts.values() {
            for node in &source.nodes {
                let Some(buffer) = source.buffers.get(&node.id) else {
                    continue;
                };
                if is_note(node) {
                    continue;
                }
                let title = derived_title(node, registry, threads);
                crate::desk_view::write_derived_title(buffer, &title, cx);
            }
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
        self.apply_tree_folds(&[], &HashMap::new(), cx);
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
            let key = (*host, node.id.clone());
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
            self.apply_tree_folds(&[], &HashMap::new(), cx);
            return;
        }
        let mut inlays = Vec::new();
        let mut eol_hints: Vec<(editor::Anchor, editor::EolHintRenderer)> = Vec::new();
        let mut highlights = DashClass::ALL
            .into_iter()
            .map(|class| (class, Vec::new()))
            .collect::<Vec<_>>();
        let mut depths = HashMap::new();
        let mut tree_depths = HashMap::new();
        // How many cards a row hangs under: a card's marker is two columns
        // in, so anything below it starts past those columns.
        let mut card_depths = HashMap::new();
        for (host, node, _) in &rows {
            let tree_depth = node
                .parent
                .clone()
                .and_then(|parent| tree_depths.get(&(*host, parent)).copied())
                .unwrap_or(0usize)
                + usize::from(node.parent.is_some());
            let depth = node
                .parent
                .clone()
                .and_then(|parent| depths.get(&(*host, parent)).copied())
                .unwrap_or(0usize)
                + usize::from(node.is_note());
            let card_depth = node
                .parent
                .clone()
                .and_then(|parent| card_depths.get(&(*host, parent)).copied())
                .unwrap_or(0usize)
                + usize::from(!node.is_note());
            depths.insert((*host, node.id.clone()), depth);
            tree_depths.insert((*host, node.id.clone()), tree_depth);
            card_depths.insert((*host, node.id.clone()), card_depth);
        }
        for (index, (host, node, buffer)) in rows.iter().enumerate() {
            let buffer_snapshot = buffer.read(cx).snapshot();
            // The visible heading prefix belongs to this row. A left-biased
            // zero anchor resolves to the preceding excerpt at a row
            // boundary, which made commands issued on `*` edit that previous
            // row instead (notably `dd`, `O`, `R`, and subtree toggles).
            let Some(start) = snapshot.anchor_in_excerpt(buffer_snapshot.anchor_after(0)) else {
                continue;
            };
            let Some(end) =
                snapshot.anchor_in_excerpt(buffer_snapshot.anchor_after(buffer_snapshot.len()))
            else {
                continue;
            };
            let hidden_by_fold = self.tree_hosts.get(host).is_some_and(|source| {
                let mut parent = node.parent.clone();
                while let Some(parent_id) = parent {
                    if self.tree_collapsed.contains(&(*host, parent_id.clone())) {
                        return true;
                    }
                    parent = source
                        .nodes
                        .iter()
                        .find(|candidate| candidate.id == parent_id)
                        .and_then(|candidate| candidate.parent.clone());
                }
                false
            });
            let prefix = match &node.id {
                rho_desk::cells::Id::Note(_) => {
                    // A note under a card is indented past the card's marker
                    // and words. At the left edge its `*` read as the next
                    // root rather than as something belonging to the card.
                    format!(
                        "{}{} ",
                        "    ".repeat(card_depths[&(*host, node.id.clone())]),
                        "*".repeat(depths[&(*host, node.id.clone())].max(1))
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
                    format!("  • {label}")
                }
                _ => "  ◦ ".to_owned(),
            };
            let class = match &node.id {
                rho_desk::cells::Id::Note(_) => {
                    Some(DashClass::for_depth(depths[&(*host, node.id.clone())]))
                }
                _ => Some(DashClass::Muted),
            };
            if let Some(class) = class
                && let Some((_, ranges)) = highlights.iter_mut().find(|(key, _)| *key == class)
            {
                ranges.push(start..end);
            }
            if !hidden_by_fold && !prefix.is_empty() {
                // A body runs to as many lines as it wants, and only its
                // first carries the bullet. The rest are padded to the same
                // column so the note reads as one block under it.
                let padding = " ".repeat(prefix.chars().count());
                let inlay = Inlay::custom(TREE_INLAY_ID_BASE + index * 2, start, prefix);
                self.tree_inlay_ids.push(inlay.id);
                inlays.push(inlay);
                let text = buffer_snapshot.text();
                for (line, offset) in text
                    .match_indices('\n')
                    .enumerate()
                    .map(|(line, (offset, _))| (line, offset + 1))
                {
                    let Some(anchor) =
                        snapshot.anchor_in_excerpt(buffer_snapshot.anchor_after(offset))
                    else {
                        continue;
                    };
                    let inlay = Inlay::custom(
                        CONTINUATION_INLAY_ID_BASE + index * 256 + line,
                        anchor,
                        padding.clone(),
                    );
                    self.tree_inlay_ids.push(inlay.id);
                    inlays.push(inlay);
                }
            }
            let mut hints = Vec::new();
            match node.state {
                rho_desk::cells::State::Done => hints.push("done".to_owned()),
                rho_desk::cells::State::Dismissed => hints.push("discarded".to_owned()),
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
                let path = path.to_string();
                hints.push(path);
            }
            if !hidden_by_fold && !hints.is_empty() {
                let text: gpui::SharedString = hints.join(" · ").into();
                let renderer: editor::EolHintRenderer = std::sync::Arc::new(move |_, cx| {
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
                });
                eol_hints.push((end, renderer));
            }
        }
        self.editor.update(cx, |editor, cx| {
            editor.splice_inlays(&[], inlays, cx);
            editor.set_eol_hints(eol_hints, cx);
            for (class, ranges) in highlights {
                editor.highlight_text(class.key(), ranges, class.style(cx), cx);
            }
        });
        self.apply_tree_folds(rows.as_slice(), &tree_depths, cx);
    }

    fn apply_tree_folds(
        &self,
        rows: &[(HostId, crate::desk_view::DeskNode, Entity<Buffer>)],
        depths: &HashMap<(HostId, rho_desk::cells::Id), usize>,
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
            let depth = depths[&(*host, node.id.clone())];
            let end_index = rows[index + 1..]
                .iter()
                .position(|(candidate_host, candidate, _)| {
                    *candidate_host != *host
                        || depths[&(*candidate_host, candidate.id.clone())] <= depth
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
        threads: &HashMap<ThreadRef, DealerThread>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let now = chrono::Local::now();
        let queue =
            self.tree_dealer_queue(registry, threads, now.fixed_offset(), &HashMap::new(), cx);
        self.queue_depth = DealQueueDepth {
            dealt_count: queue.dealt_count,
            total_alive: queue.total_alive,
        };
        self.sync_tree(registry, threads, window, cx);
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
        let room = self.room_for_node(host, node_id.clone(), cx)?;
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
        if self.deal_active
            && let Some(deal) = &self.deal
        {
            return deal_hint(deal);
        }
        if self.deal_empty_success {
            return "nothing needs attention".to_owned();
        }
        format!(
            "{} dealt · {} waiting",
            self.queue_depth.dealt_count, self.queue_depth.total_alive
        )
    }
}

fn select_deal(
    hand: &DealQueue,
    exclude: Option<&DealCardId>,
) -> Option<(DealCard, DealFingerprint, Vec<DealCardId>)> {
    let mut cards = hand
        .cards
        .iter()
        .filter(|card| exclude.is_none_or(|excluded| card.identity != *excluded));
    let card = cards.next()?.clone();
    let fingerprint = hand.fingerprints.get(&card.identity)?.clone();
    let considered_not_dealt = cards.take(5).map(|card| card.identity.clone()).collect();
    Some((card, fingerprint, considered_not_dealt))
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
    pub facts: rho_ui_proto::UiAgentFacts,
    pub attention: rho_ui_proto::UiAttention,
}

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
enum DeskMark {
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

/// The Slack thread a row stands for.
pub(crate) fn node_thread(node: &crate::desk_view::DeskNode) -> Option<ThreadRef> {
    let unit = node.slack()?;
    Some(ThreadRef {
        workspace: unit.workspace.clone(),
        channel: unit.channel.clone(),
        thread_ts: unit.thread.clone()?,
    })
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
    threads: &HashMap<ThreadRef, DealerThread>,
) -> String {
    use rho_desk::cells::Id;

    match &node.id {
        Id::Agent(_) | Id::Note(_) => String::new(),
        Id::Label(_) => node.name.clone().unwrap_or_else(|| "label".to_owned()),
        Id::Host(_) => "this host".to_owned(),
        Id::Page(_) => node_page(node)
            .and_then(rho_browser::live_page_name)
            .unwrap_or_else(|| "page".to_owned()),
        Id::File { path, .. } => path.to_string(),
        // The store holds the unit's identity, which is ids and a
        // timestamp; what it is called lives in the mirror. A thread rho
        // has not caught up with yet says so rather than showing its keys.
        Id::Slack(unit) => match node_thread(node).and_then(|key| threads.get(&key)) {
            Some(thread) => format!("{} · {}", thread.conversation, thread.title),
            // The conversation row is named by the mirror too: any thread
            // in it knows what Slack calls it, and its ids are never shown.
            None => threads
                .iter()
                .find(|(key, _)| key.workspace == unit.workspace && key.channel == unit.channel)
                .map(|(_, thread)| thread.conversation.clone())
                .unwrap_or_else(|| "conversation".to_owned()),
        },
        Id::PullRequest { repo, number } => format!("{repo}#{number}"),
    }
}

/// What an area row calls itself in the picker.
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

fn tree_breadcrumb(
    id: &rho_desk::cells::Id,
    nodes: &HashMap<rho_desk::cells::Id, &crate::desk_view::DeskNode>,
    titles: &HashMap<rho_desk::cells::Id, String>,
) -> String {
    let mut path = Vec::new();
    let mut cursor = Some(id.clone());
    while let Some(id) = cursor {
        let Some(node) = nodes.get(&id) else {
            break;
        };
        if node.is_note() {
            path.push(titles.get(&id).map_or("", String::as_str));
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
    fingerprint: DealFingerprint,
    card: DealCard,
}

fn deal_hint(deal: &DealSession) -> String {
    format!("DEAL · {} · {}", deal.card.breadcrumb, deal.card.label)
}

impl Dashboard {
    pub fn heading_destination_candidates(
        &self,
        cx: &App,
    ) -> Vec<(String, String, HostId, rho_desk::cells::Id)> {
        self.tree_hosts
            .iter()
            .flat_map(|(host, source)| {
                source.nodes.iter().filter_map(move |node| {
                    if !node.is_note() {
                        return None;
                    }
                    let title =
                        note_title(&source.buffers.get(&node.id)?.read(cx).text()).to_owned();
                    Some((
                        title.clone(),
                        self.breadcrumb_for_node_for_source(node.id.clone(), source, cx)?,
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
        threads: &HashMap<ThreadRef, DealerThread>,
        cx: &App,
    ) -> Vec<(String, &'static str, HostId, rho_desk::cells::Id)> {
        let mut areas = Vec::new();
        for (host, source) in &self.tree_hosts {
            let nodes = source
                .nodes
                .iter()
                .map(|node| (node.id.clone(), node))
                .collect::<HashMap<_, _>>();
            let titles = source
                .buffers
                .iter()
                .map(|(id, buffer)| (id.clone(), note_title(&buffer.read(cx).text()).to_owned()))
                .collect::<HashMap<_, _>>();
            for node in &source.nodes {
                let breadcrumb = tree_breadcrumb(&node.id, &nodes, &titles);
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
            let titles = source
                .buffers
                .iter()
                .map(|(id, buffer)| (id.clone(), note_title(&buffer.read(cx).text()).to_owned()))
                .collect::<HashMap<_, _>>();
            let title_of = |node_id: rho_desk::cells::Id| {
                titles
                    .get(&node_id)
                    .and_then(|text| text.lines().next())
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned)
            };
            for node in &source.nodes {
                let breadcrumb = tree_breadcrumb(&node.id, &nodes, &titles);
                let under = |title: String| {
                    if breadcrumb.is_empty() {
                        title
                    } else {
                        format!("{breadcrumb} › {title}")
                    }
                };
                let candidate = match &node.id {
                    rho_desk::cells::Id::Note(_) => {
                        if breadcrumb.is_empty() {
                            continue;
                        }
                        FindCandidate {
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
                        FindCandidate {
                            path: under(
                                title_of(node.id.clone())
                                    .unwrap_or_else(|| registry.agent_human_name(agent_id)),
                            ),
                            kind: "agent",
                            target: FindTarget::Agent(agent_id),
                            recency: registry
                                .agent_last_active(agent_id)
                                .map_or(0, |active| active.0 as i64),
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
        candidates
    }

    pub fn heading_candidates(
        &self,
        _registry: &AgentRegistry,
        needle: &str,
        cx: &App,
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
                        let title =
                            note_title(&source.buffers.get(&node.id)?.read(cx).text()).to_owned();
                        title.to_lowercase().contains(&needle).then(|| {
                            (
                                title.clone(),
                                self.breadcrumb_for_node_for_source(node.id.clone(), source, cx)
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
        cx: &App,
    ) -> Option<String> {
        let nodes = source
            .nodes
            .iter()
            .map(|node| (node.id.clone(), node))
            .collect::<HashMap<_, _>>();
        let titles = source
            .buffers
            .iter()
            .map(|(id, buffer)| (id.clone(), note_title(&buffer.read(cx).text()).to_owned()))
            .collect::<HashMap<_, _>>();
        Some(tree_breadcrumb(&node_id, &nodes, &titles))
    }

    pub fn jump_to_heading(
        &mut self,
        query: &str,
        _registry: &AgentRegistry,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let Some((host, node_id)) = self.tree_heading_named(query, cx) else {
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
    pub fn prepare_taken_deal_edit(&mut self, _cx: &mut Context<Workspace>) -> bool {
        self.current_deal_card().is_some()
    }
}

/// What a thread's card says and how hard it pushes. The state, then how
/// long it has been in that state: whose turn it is is the whole of what a
/// thread's card says. Somebody waiting outranks a note of the same age,
/// the way a blocked agent outranks an FYI.
fn thread_card_facts(
    thread: &DealerThread,
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

    #[test]
    fn a_slack_thread_deals_like_an_agent_waiting_on_a_reply() {
        let now = chrono::NaiveDate::from_ymd_opt(2026, 8, 23)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_utc()
            .fixed_offset();
        let thread = |waiting_on: Option<&str>, wait_days: f64| DealerThread {
            title: "can you look at the deploy?".into(),
            conversation: "#design".into(),
            raised_at: now - chrono::Duration::days(2),
            wait_days,
            waiting_on: waiting_on.map(str::to_owned),
            latest: "500.0".into(),
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
