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

type DraftTopic = Option<(HostId, rho_desk::NodeId)>;
type DraftState = (DraftTopic, Entity<Buffer>, gpui::Subscription);

// Dealer curve tuning. These are deliberately all in one place: rho has one
// user, so policy changes are edits, not a configuration system.
const DEAL_QUEUE_FLOOR: f64 = -1.0;
const SKIP_COOLDOWN_MINUTES: i64 = 15;
const BLOCKED_REPLY_HEAD_START: f64 = 1.0;
const BLOCKED_REPLY_SLOPE_PER_DAY: f64 = 12.0;
const FYI_REPLY_PACE_DAYS: f64 = 3.0;
const INBOX_OBLIGATION_PACE_DAYS: u32 = 0;
const INBOX_CAPTURE_PACE_DAYS: u32 = 1;
/// Half a curve unit is enough to mark the hand visibly dirty without
/// turning every newly-ripe reminder into persistent chrome.
pub(crate) const LAMP_THRESHOLD: f64 = 0.5;
/// At 1.2 curve units a blocked agent chimes after about 24 minutes
/// unnoticed, an agent completed within about 12 minutes of interaction
/// chimes immediately through the recency bonus, and a ping takes about
/// 14 hours to cross. Sound therefore marks pressure, not every new card.
pub(crate) const CHIME_THRESHOLD: f64 = 1.2;
/// A recent agent interaction contributes 1.5 curve units. This must remain
/// above the 1.2 chime threshold or recently-driven agents lose their instant
/// completion chime; linear decay gives about 12 minutes of instant chime and
/// about 40 minutes above the 0.5 lamp threshold.
const AGENT_RECENCY_BONUS: f64 = 1.5;
/// The engagement nudge fades linearly over one hour so it cannot become a
/// hidden long-lived preference.
const AGENT_RECENCY_WINDOW_MS: i64 = 60 * 60 * 1_000;

pub(crate) fn dealer_policy_snapshot() -> crate::journal::DealerPolicySnapshot {
    crate::journal::DealerPolicySnapshot {
        queue_floor: DEAL_QUEUE_FLOOR,
        skip_cooldown_minutes: SKIP_COOLDOWN_MINUTES,
        blocked_reply_head_start: BLOCKED_REPLY_HEAD_START,
        blocked_reply_slope_per_day: BLOCKED_REPLY_SLOPE_PER_DAY,
        fyi_reply_pace_days: FYI_REPLY_PACE_DAYS,
        inbox_obligation_pace_days: INBOX_OBLIGATION_PACE_DAYS,
        inbox_capture_pace_days: INBOX_CAPTURE_PACE_DAYS,
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
    pub subject_node_id: Option<rho_desk::NodeId>,
    pub topic_node_id: Option<rho_desk::NodeId>,
    pub agent_id: Option<AgentId>,
    pub agent_tag: Option<String>,
    pub breadcrumb: String,
    pub room: Option<String>,
    pub kind: DealCardKind,
    pub identity: DealCardIdentity,
    pub inbox_source: Option<DealerInboxSource>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DeskRoom {
    pub host: HostId,
    pub node_id: rho_desk::NodeId,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DealCardIdentity {
    Tree {
        host: HostId,
        node_id: rho_desk::NodeId,
    },
    TreeAgent {
        host: HostId,
        node_id: rho_desk::NodeId,
        agent_id: AgentId,
    },
    Agent(AgentId),
    Inbox(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DealCardKind {
    Desk,
    Agent,
    Inbox(DealerInboxKind),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DealerInboxKind {
    Ping,
    Obligation,
    Capture,
    /// A Slack thread waiting on the user. Machine-owned, so it carries the
    /// thread's own wait rather than a capture age.
    Slack,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DealerInboxItem {
    pub id: String,
    pub host: HostId,
    pub title: String,
    pub kind: DealerInboxKind,
    pub captured_at: chrono::DateTime<chrono::FixedOffset>,
    pub deferred_until: Option<chrono::DateTime<chrono::FixedOffset>>,
    pub resurfacing_count: u32,
    pub waiting_on: Option<String>,
    pub source: Option<DealerInboxSource>,
    /// Short human context such as the captured room or surface. This is
    /// replayed verbatim; the dealer never summarizes or rewrites it.
    pub context: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DealerInboxSource {
    Page(rho_browser::PageId),
    /// Enough to reopen the conversation surface on the thread. Ids live here
    /// because this is addressing, not display; the card shows the label.
    SlackThread {
        workspace: String,
        channel: String,
        thread_ts: String,
    },
    Other(String),
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
    pub card: DealCardIdentity,
    pub kind: DealCardKind,
    pub verdict: DealerVerdict,
    pub at: chrono::DateTime<chrono::FixedOffset>,
    pub time_to_verdict_ms: u64,
    pub considered_not_dealt: Vec<DealCardIdentity>,
    pub skip_until: Option<chrono::DateTime<chrono::FixedOffset>>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DealQueue {
    pub cards: Vec<DealCard>,
    /// Number of live headings whose winning mark is above the queue floor.
    pub total_alive: usize,
    /// Number selected by global priority.
    pub dealt_count: usize,
    considered_not_dealt: Vec<DealCardIdentity>,
    fingerprints: HashMap<DealCardIdentity, DealFingerprint>,
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
    considered_not_dealt: Vec<DealCardIdentity>,
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
    NewDraft(Option<(HostId, rho_desk::NodeId)>),
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
        node_id: rho_desk::NodeId,
        first_attention: Option<AgentId>,
        on_heading_line: bool,
    },
    TreeAgent {
        host: HostId,
        node_id: rho_desk::NodeId,
        topic_node_id: rho_desk::NodeId,
        agent_id: AgentId,
    },
    TreePage {
        host: HostId,
        node_id: rho_desk::NodeId,
        topic_node_id: rho_desk::NodeId,
        page_id: rho_browser::PageId,
    },
    NewDraft,
    NewTreeDraft((HostId, rho_desk::NodeId)),
}

/// Where the cursor is: on a generated row, or at an offset inside a
/// host's document.
#[derive(Clone, Debug, PartialEq)]
enum CursorPlace {
    Row(LineKey),
    Tree(HostId, rho_desk::NodeId, usize),
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
    tree_element_keys: HashMap<(HostId, rho_desk::NodeId), u64>,
    tree_heading_agents: HashMap<(HostId, rho_desk::NodeId), Vec<AgentId>>,
    tree_heading_pages: HashMap<(HostId, rho_desk::NodeId), Vec<rho_browser::PageId>>,
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
    tree_new_draft_parent: Option<(HostId, rho_desk::NodeId)>,
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
    skipped: HashMap<DealCardIdentity, SkippedCard>,
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
    tree_collapsed: HashSet<(HostId, rho_desk::NodeId)>,
    pending_tree_cursor: Option<(HostId, rho_desk::NodeId, usize)>,
    /// The previous pass's inputs and output, so a sync whose world is
    /// unchanged returns without touching the editor.
    /// Buffers already registered as headerless with the editor. A
    /// boundary onto a headerless buffer draws nothing, so this is what
    /// keeps the interleaved excerpts seamless.
    headers_disabled: std::collections::HashSet<BufferId>,
}

struct TreeHostSource {
    nodes: Vec<rho_desk::MaterializedNode>,
    buffers: BTreeMap<rho_desk::NodeId, Entity<Buffer>>,
}

fn nearest_tree_heading(
    source: &TreeHostSource,
    mut node_id: Option<rho_desk::NodeId>,
) -> Option<rho_desk::NodeId> {
    while let Some(id) = node_id {
        let node = source.nodes.iter().find(|node| node.id == id)?;
        if node.kind == rho_desk::NodeKind::Heading {
            return Some(id);
        }
        node_id = node.parent;
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

    pub fn tree_heading_named(&self, title: &str, cx: &App) -> Option<(HostId, rho_desk::NodeId)> {
        self.tree_hosts.iter().find_map(|(host, source)| {
            source.nodes.iter().find_map(|node| {
                (node.kind == rho_desk::NodeKind::Heading
                    && source
                        .buffers
                        .get(&node.id)
                        .is_some_and(|buffer| buffer.read(cx).text().trim() == title.trim()))
                .then_some((*host, node.id))
            })
        })
    }

    fn tree_dealer_queue(
        &self,
        registry: &AgentRegistry,
        inbox: &[DealerInboxItem],
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
                .map(|node| (node.id, node))
                .collect::<HashMap<_, _>>();
            let titles = source
                .buffers
                .iter()
                .map(|(id, buffer)| (*id, buffer.read(cx).text()))
                .collect::<HashMap<_, _>>();
            for heading in source
                .nodes
                .iter()
                .filter(|node| node.kind == rho_desk::NodeKind::Heading)
            {
                let terminal = heading.temporal.contains_key(&rho_desk::TemporalKind::Done)
                    || heading
                        .temporal
                        .contains_key(&rho_desk::TemporalKind::Discarded);
                let ancestor_deferred = std::iter::successors(heading.parent, |parent| {
                    nodes.get(parent).and_then(|node| node.parent)
                })
                .filter_map(|parent| nodes.get(&parent))
                .any(|node| {
                    node.temporal
                        .get(&rho_desk::TemporalKind::Defer)
                        .is_some_and(|mark| {
                            tree_mark_priority(
                                rho_desk::TemporalKind::Defer,
                                mark,
                                now.naive_local(),
                            ) == f64::NEG_INFINITY
                        })
                });
                let locally_deferred = heading.temporal.iter().any(|(kind, mark)| {
                    matches!(
                        kind,
                        rho_desk::TemporalKind::Defer | rho_desk::TemporalKind::Reminder
                    ) && tree_mark_priority(*kind, mark, now.naive_local()) == f64::NEG_INFINITY
                });
                if terminal || ancestor_deferred || locally_deferred {
                    order += 1;
                    continue;
                }
                let breadcrumb = tree_breadcrumb(heading.id, &nodes, &titles);
                let room = breadcrumb.split(" › ").next().map(str::to_owned);
                let bindings = source
                    .nodes
                    .iter()
                    .filter(|node| node.parent == Some(heading.id))
                    .filter_map(
                        |node| match node.bindings.get(&rho_desk::BindingKind::Agent) {
                            Some(rho_desk::Binding::Agent(agent_id)) => Some((node.id, *agent_id)),
                            _ => None,
                        },
                    )
                    .collect::<Vec<_>>();
                for (kind, mark) in &heading.temporal {
                    let priority = tree_mark_priority(*kind, mark, now.naive_local());
                    if priority <= DEAL_QUEUE_FLOOR {
                        continue;
                    }
                    let identity = DealCardIdentity::Tree {
                        host: *host,
                        node_id: heading.id,
                    };
                    ranked.push(RankedDealCard {
                        priority,
                        virtual_reply: false,
                        order,
                        fingerprint: DealFingerprint(format!("{kind:?}:{mark:?}")),
                        card: DealCard {
                            label: tree_temporal_label(*kind, mark, now.naive_local(), priority),
                            priority,
                            host: *host,
                            subject_node_id: Some(heading.id),
                            topic_node_id: Some(heading.id),
                            agent_id: bindings.first().map(|(_, id)| *id),
                            agent_tag: None,
                            breadcrumb: breadcrumb.clone(),
                            room: room.clone(),
                            kind: DealCardKind::Desk,
                            identity,
                            inbox_source: None,
                        },
                    });
                }
                let todo_gated = heading
                    .temporal
                    .get(&rho_desk::TemporalKind::Todo)
                    .is_some_and(|mark| {
                        tree_mark_priority(rho_desk::TemporalKind::Todo, mark, now.naive_local())
                            <= DEAL_QUEUE_FLOOR
                    });
                if !todo_gated {
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
                                    AGENT_RECENCY_BONUS
                                        * (1.0 - elapsed as f64 / AGENT_RECENCY_WINDOW_MS as f64)
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
                                    subject_node_id: Some(machine_node_id),
                                    topic_node_id: Some(heading.id),
                                    agent_id: Some(agent_id),
                                    agent_tag: None,
                                    breadcrumb: breadcrumb.clone(),
                                    room: room.clone(),
                                    kind: DealCardKind::Agent,
                                    identity: DealCardIdentity::TreeAgent {
                                        host: *host,
                                        node_id: machine_node_id,
                                        agent_id,
                                    },
                                    inbox_source: None,
                                },
                            });
                        }
                    }
                }
                order += 1;
            }
        }
        // One winning card per topic; a virtual reply wins an exact tie.
        let mut by_topic = HashMap::new();
        for candidate in ranked {
            let topic = (candidate.card.host, candidate.card.topic_node_id.unwrap());
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
        let mut queue = DealQueue {
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
        let inbox_queue = inbox_deal_queue(inbox, now, &self.skipped);
        queue.cards.extend(inbox_queue.cards);
        queue
            .cards
            .sort_by(|a, b| b.priority.total_cmp(&a.priority));
        queue.total_alive += inbox_queue.total_alive;
        queue.dealt_count = usize::from(!queue.cards.is_empty());
        queue.fingerprints.extend(inbox_queue.fingerprints);
        queue.considered_not_dealt = queue
            .cards
            .iter()
            .skip(1)
            .take(5)
            .map(|card| card.identity.clone())
            .collect();
        queue
    }

    fn breadcrumb_for_node(
        &self,
        host: HostId,
        node_id: rho_desk::NodeId,
        cx: &App,
    ) -> Option<String> {
        let source = self.tree_hosts.get(&host)?;
        let nodes = source
            .nodes
            .iter()
            .map(|node| (node.id, node))
            .collect::<HashMap<_, _>>();
        let titles = source
            .buffers
            .iter()
            .map(|(id, buffer)| (*id, buffer.read(cx).text()))
            .collect::<HashMap<_, _>>();
        Some(tree_breadcrumb(node_id, &nodes, &titles))
    }

    fn room_for_node(
        &self,
        host: HostId,
        mut node_id: rho_desk::NodeId,
        cx: &App,
    ) -> Option<DeskRoom> {
        let source = self.tree_hosts.get(&host)?;
        loop {
            let node = source.nodes.iter().find(|node| node.id == node_id)?;
            let Some(parent) = node.parent else { break };
            let parent_node = source.nodes.iter().find(|node| node.id == parent)?;
            if parent_node.kind != rho_desk::NodeKind::Heading {
                break;
            }
            node_id = parent;
        }
        let name = source.buffers.get(&node_id)?.read(cx).text();
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
            .find_map(|(topic, agents)| agents.contains(&agent_id).then_some(*topic))?;
        self.breadcrumb_for_node(host, node_id, cx)
    }

    pub fn breadcrumb_for_page(&self, page_id: rho_browser::PageId, cx: &App) -> Option<String> {
        let (host, node_id) = self
            .tree_heading_pages
            .iter()
            .find_map(|(topic, pages)| pages.contains(&page_id).then_some(*topic))?;
        self.breadcrumb_for_node(host, node_id, cx)
    }

    pub fn room_for_agent(&self, agent_id: AgentId, cx: &App) -> Option<DeskRoom> {
        let (host, node_id) = self
            .tree_heading_agents
            .iter()
            .find_map(|(topic, agents)| agents.contains(&agent_id).then_some(*topic))?;
        self.room_for_node(host, node_id, cx)
    }

    pub fn room_for_page(&self, page_id: rho_browser::PageId, cx: &App) -> Option<DeskRoom> {
        let (host, node_id) = self
            .tree_heading_pages
            .iter()
            .find_map(|(topic, pages)| pages.contains(&page_id).then_some(*topic))?;
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
        nodes: Vec<rho_desk::MaterializedNode>,
        buffers: BTreeMap<rho_desk::NodeId, Entity<Buffer>>,
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

    pub fn tree_node_at_cursor(
        &self,
        cx: &mut Context<Workspace>,
    ) -> Option<(HostId, rho_desk::NodeId)> {
        self.tree_node_cursor_offset(cx)
            .map(|(host, node_id, _)| (host, node_id))
    }

    pub fn tree_node_for_buffer(
        &self,
        buffer_id: BufferId,
        cx: &App,
    ) -> Option<(HostId, rho_desk::NodeId)> {
        self.tree_hosts.iter().find_map(|(host, source)| {
            source.buffers.iter().find_map(|(node_id, buffer)| {
                (buffer.read(cx).remote_id() == buffer_id).then_some((*host, *node_id))
            })
        })
    }

    pub fn first_tree_agent_for_topic(&self, topic: (HostId, rho_desk::NodeId)) -> Option<AgentId> {
        self.tree_heading_agents
            .get(&topic)
            .and_then(|agents| agents.first())
            .copied()
    }

    pub fn tree_node_cursor_offset(
        &self,
        cx: &mut Context<Workspace>,
    ) -> Option<(HostId, rho_desk::NodeId, usize)> {
        let (buffer_id, offset) = self.editor.update(cx, |editor, cx| {
            let head = editor.selections.newest_anchor().head();
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            snapshot
                .anchor_to_buffer_anchor(head)
                .map(|(anchor, buffer)| (buffer.remote_id(), anchor.to_offset(buffer)))
        })?;
        self.tree_hosts.iter().find_map(|(host, source)| {
            source.buffers.iter().find_map(|(node_id, buffer)| {
                (buffer.read(cx).remote_id() == buffer_id).then_some((*host, *node_id, offset))
            })
        })
    }

    pub fn move_to_tree_node_when_ready(&mut self, host: HostId, node_id: rho_desk::NodeId) {
        self.pending_tree_cursor = Some((host, node_id, 0));
    }

    pub fn move_to_tree_position_when_ready(
        &mut self,
        host: HostId,
        node_id: rho_desk::NodeId,
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
        inbox: &crate::inbox::InboxStore,
        now: chrono::DateTime<chrono::FixedOffset>,
        agent_interactions: &HashMap<AgentId, i64>,
        cx: &App,
    ) -> DealQueue {
        let host = self.tree_hosts.keys().next().copied().unwrap_or(HostId(0));
        let inbox = dealer_inbox_items(inbox, host, now.timestamp_millis());
        self.tree_dealer_queue(registry, &inbox, now, agent_interactions, cx)
    }

    /// Re-evaluates the complete dealer world and presents its highest-scoring
    /// claim. There is deliberately no retained hand: each pull sees current
    /// Desk text, agent facts, inbox state, and cooldowns.
    pub fn pull_deal(
        &mut self,
        registry: &AgentRegistry,
        inbox: &crate::inbox::InboxStore,
        now: chrono::DateTime<chrono::FixedOffset>,
        exclude: Option<&DealCardIdentity>,
        agent_interactions: &HashMap<AgentId, i64>,
        cx: &mut Context<Workspace>,
    ) -> Option<DealCard> {
        let hand = self.dealer_hand(registry, inbox, now, agent_interactions, cx);
        let (card, fingerprint, considered_not_dealt) = select_deal(&hand, exclude)?;
        self.deal = Some(DealSession {
            card: card.clone(),
            fingerprint,
            started_at: Instant::now(),
            considered_not_dealt,
        });
        self.raw_mode = false;
        self.deal_active = true;
        self.deal_empty_success = false;
        if let Some(node_id) = card.topic_node_id {
            self.pending_tree_cursor = Some((card.host, node_id, 0));
        }
        Some(card)
    }

    pub fn reopen_deal(&mut self, card: DealCard) {
        if let Some(node_id) = card.topic_node_id {
            self.pending_tree_cursor = Some((card.host, node_id, 0));
        }
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
        fn identity(card: &DealCardIdentity) -> crate::journal::DealerCardIdentity {
            match card {
                DealCardIdentity::Tree { host, node_id } => {
                    crate::journal::DealerCardIdentity::DeskNode {
                        host: host.0,
                        node_id: (*node_id).into(),
                    }
                }
                DealCardIdentity::TreeAgent {
                    host,
                    node_id,
                    agent_id,
                } => crate::journal::DealerCardIdentity::AgentNode {
                    host: host.0,
                    node_id: (*node_id).into(),
                    agent_id: (*agent_id).into(),
                },
                DealCardIdentity::Agent(agent_id) => crate::journal::DealerCardIdentity::Agent {
                    agent_id: (*agent_id).into(),
                },
                DealCardIdentity::Inbox(id) => {
                    crate::journal::DealerCardIdentity::Inbox { id: id.clone() }
                }
            }
        }
        let kind = match event.kind {
            DealCardKind::Desk => crate::journal::DealerCardKind::Desk,
            DealCardKind::Agent => crate::journal::DealerCardKind::Agent,
            DealCardKind::Inbox(kind) => crate::journal::DealerCardKind::Inbox(match kind {
                DealerInboxKind::Ping => crate::journal::DealerInboxKind::Ping,
                DealerInboxKind::Obligation => crate::journal::DealerInboxKind::Obligation,
                DealerInboxKind::Capture => crate::journal::DealerInboxKind::Capture,
                DealerInboxKind::Slack => crate::journal::DealerInboxKind::Slack,
            }),
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
        identity: DealCardIdentity,
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

    pub fn clear_skip(&mut self, identity: &DealCardIdentity) -> bool {
        self.skipped.remove(identity).is_some()
    }

    #[cfg(test)]
    pub fn has_skip_for_test(&self, identity: &DealCardIdentity) -> bool {
        self.skipped.contains_key(identity)
    }

    pub fn current_deal_card(&self) -> Option<&DealCard> {
        Some(&self.deal.as_ref()?.card)
    }

    pub fn current_tree_room_node(&self) -> Option<(HostId, rho_desk::NodeId)> {
        let card = self.current_deal_card()?;
        let source = self.tree_hosts.get(&card.host)?;
        let mut node_id = card.topic_node_id?;
        loop {
            let node = source.nodes.iter().find(|node| node.id == node_id)?;
            let Some(parent) = node.parent else {
                return Some((card.host, node_id));
            };
            let parent_node = source.nodes.iter().find(|node| node.id == parent)?;
            if parent_node.kind != rho_desk::NodeKind::Heading {
                return Some((card.host, node_id));
            }
            node_id = parent;
        }
    }

    pub fn current_inbox_source(&self) -> Option<&DealerInboxSource> {
        self.current_deal_card()?.inbox_source.as_ref()
    }

    /// Opens (or returns to) the inline new-agent draft. Like a reply
    /// draft it parks when left and survives refreshes.
    pub fn open_new_draft(
        &mut self,
        topic: Option<(HostId, rho_desk::NodeId)>,
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

    pub fn open_new_tree_draft(
        &mut self,
        topic: (HostId, rho_desk::NodeId),
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        self.tree_new_draft_parent = Some(topic);
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

    pub fn new_draft_topic(&self) -> Option<(HostId, rho_desk::NodeId)> {
        self.new_draft.as_ref().and_then(|draft| draft.0)
    }

    /// Renders the authoritative tree as one native editor composition. Each
    /// row is the node's own CRDT buffer; stars and typed machine/meta fields
    /// are display-only inlays, so structural state never leaks into text.
    fn sync_tree(
        &mut self,
        registry: &AgentRegistry,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        self.tree_heading_agents.clear();
        self.tree_heading_pages.clear();
        self.referenced_pages.clear();
        for (host, source) in &self.tree_hosts {
            for node in &source.nodes {
                let Some(parent) = node.parent else { continue };
                if let Some(rho_desk::Binding::Agent(agent_id)) =
                    node.bindings.get(&rho_desk::BindingKind::Agent)
                {
                    self.tree_heading_agents
                        .entry((*host, parent))
                        .or_default()
                        .push(*agent_id);
                }
                if let Some(rho_desk::Binding::Page(page_id)) =
                    node.bindings.get(&rho_desk::BindingKind::Page)
                {
                    let page_id = rho_browser::PageId(uuid::Uuid::from_bytes(page_id.0));
                    self.tree_heading_pages
                        .entry((*host, parent))
                        .or_default()
                        .push(page_id);
                    self.referenced_pages.insert(page_id);
                }
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
                    if raw_mode && node.owner != rho_desk::NodeOwner::User {
                        return None;
                    }
                    Some((*host, node.clone(), source.buffers.get(&node.id)?.clone()))
                })
            })
            .collect::<Vec<_>>();
        let semantic_rows = rows
            .iter()
            .filter(|(_, node, _)| node.kind == rho_desk::NodeKind::Heading)
            .map(|(_, _, buffer)| buffer.read(cx).remote_id())
            .collect();
        self.editor.update(cx, |editor, _| {
            editor.set_semantic_row_buffers(semantic_rows)
        });
        let mut spec = CompositionSpec::default();
        for (host, node, buffer) in &rows {
            let key = (*host, node.id);
            let id = *self.tree_element_keys.entry(key).or_insert_with(|| {
                self.next_element_key += 1;
                self.next_element_key
            });
            spec.tail.push(RowSpec {
                id,
                buffer: buffer.clone(),
            });
            if self.tree_new_draft_parent == Some((*host, node.id))
                && let Some((_, draft, _)) = &self.new_draft
            {
                let key = LineKey::NewDraft(Some((*host, node.id)));
                let id = *self.element_keys.entry(key.clone()).or_insert_with(|| {
                    self.next_element_key += 1;
                    self.next_element_key
                });
                self.buffers.insert(key.clone(), draft.clone());
                self.targets
                    .insert(key.clone(), RowTarget::NewTreeDraft((*host, node.id)));
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
        if let Some((host, node_id, offset)) = self.pending_tree_cursor {
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
        for (host, node, _) in &rows {
            let tree_depth = node
                .parent
                .and_then(|parent| tree_depths.get(&(*host, parent)).copied())
                .unwrap_or(0usize)
                + usize::from(node.parent.is_some());
            let depth = node
                .parent
                .and_then(|parent| depths.get(&(*host, parent)).copied())
                .unwrap_or(0usize)
                + usize::from(node.kind == rho_desk::NodeKind::Heading);
            depths.insert((*host, node.id), depth);
            tree_depths.insert((*host, node.id), tree_depth);
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
                let mut parent = node.parent;
                while let Some(parent_id) = parent {
                    if self.tree_collapsed.contains(&(*host, parent_id)) {
                        return true;
                    }
                    parent = source
                        .nodes
                        .iter()
                        .find(|candidate| candidate.id == parent_id)
                        .and_then(|candidate| candidate.parent);
                }
                false
            });
            let prefix = match node.kind {
                rho_desk::NodeKind::Heading => {
                    format!("{} ", "*".repeat(depths[&(*host, node.id)].max(1)))
                }
                rho_desk::NodeKind::Agent => {
                    let label = node
                        .bindings
                        .get(&rho_desk::BindingKind::Agent)
                        .and_then(|binding| match binding {
                            rho_desk::Binding::Agent(agent_id) => Some(format!(
                                "{} {} ",
                                match registry.attention(*agent_id) {
                                    UiAttention::Quiet => "○",
                                    UiAttention::Working => "·",
                                    UiAttention::Pending => "●",
                                    UiAttention::NeedsInput => "!",
                                },
                                registry.agent_human_name(*agent_id)
                            )),
                            _ => None,
                        })
                        .unwrap_or_default();
                    format!("  • {label}")
                }
                rho_desk::NodeKind::Page => "  ◦ ".to_owned(),
                rho_desk::NodeKind::File => "  ◦ ".to_owned(),
                rho_desk::NodeKind::Draft => "  › ".to_owned(),
                rho_desk::NodeKind::Prose => String::new(),
            };
            let class = match node.kind {
                rho_desk::NodeKind::Heading => {
                    Some(DashClass::for_depth(depths[&(*host, node.id)]))
                }
                rho_desk::NodeKind::Agent | rho_desk::NodeKind::Page | rho_desk::NodeKind::File => {
                    Some(DashClass::Muted)
                }
                _ => None,
            };
            if let Some(class) = class
                && let Some((_, ranges)) = highlights.iter_mut().find(|(key, _)| *key == class)
            {
                ranges.push(start..end);
            }
            if !hidden_by_fold && !prefix.is_empty() {
                let inlay = Inlay::custom(TREE_INLAY_ID_BASE + index * 2, start, prefix);
                self.tree_inlay_ids.push(inlay.id);
                inlays.push(inlay);
            }
            let mut hints = Vec::new();
            for (kind, mark) in &node.temporal {
                let kind = match kind {
                    rho_desk::TemporalKind::Todo => "todo",
                    rho_desk::TemporalKind::Deadline => "due",
                    rho_desk::TemporalKind::Defer => "defer",
                    rho_desk::TemporalKind::Reminder => "remind",
                    rho_desk::TemporalKind::Done => "done",
                    rho_desk::TemporalKind::Discarded => "discarded",
                };
                hints.push(format!(
                    "{kind} {:04}-{:02}-{:02} · {}d",
                    mark.year, mark.month, mark.day, mark.pace_days
                ));
            }
            for binding in node.bindings.values() {
                match binding {
                    rho_desk::Binding::Agent(_) => {}
                    rho_desk::Binding::Page(_) => hints.push("page".to_owned()),
                    rho_desk::Binding::File(path) => hints.push(path.to_string()),
                }
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
        self.apply_tree_folds(&rows, &tree_depths, cx);
    }

    fn apply_tree_folds(
        &self,
        rows: &[(HostId, rho_desk::MaterializedNode, Entity<Buffer>)],
        depths: &HashMap<(HostId, rho_desk::NodeId), usize>,
        cx: &mut Context<Workspace>,
    ) {
        struct TreeSubtreeFold;
        let type_id = std::any::TypeId::of::<TreeSubtreeFold>();
        let snapshot = self.multi_buffer.read(cx).snapshot(cx);
        let mut creases = Vec::new();
        for (index, (host, node, _)) in rows.iter().enumerate() {
            if !self.tree_collapsed.contains(&(*host, node.id)) {
                continue;
            }
            let depth = depths[&(*host, node.id)];
            let end_index = rows[index + 1..]
                .iter()
                .position(|(candidate_host, candidate, _)| {
                    *candidate_host != *host || depths[&(*candidate_host, candidate.id)] <= depth
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
        inbox: &crate::inbox::InboxStore,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let now = chrono::Local::now();
        let host = self.tree_hosts.keys().next().copied().unwrap_or(HostId(0));
        let inbox = dealer_inbox_items(inbox, host, now.timestamp_millis());
        let queue =
            self.tree_dealer_queue(registry, &inbox, now.fixed_offset(), &HashMap::new(), cx);
        self.queue_depth = DealQueueDepth {
            dealt_count: queue.dealt_count,
            total_alive: queue.total_alive,
        };
        self.sync_tree(registry, window, cx);
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
                (buffer.read(cx).remote_id() == buffer_id).then_some(*node_id)
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
                let topic = if node.kind == rho_desk::NodeKind::Heading {
                    Some(node.id)
                } else {
                    nearest_tree_heading(source, node.parent)
                };
                match node.kind {
                    rho_desk::NodeKind::Agent => {
                        match node.bindings.get(&rho_desk::BindingKind::Agent)? {
                            rho_desk::Binding::Agent(agent_id) => Some(RowTarget::TreeAgent {
                                host,
                                node_id,
                                topic_node_id: topic?,
                                agent_id: *agent_id,
                            }),
                            _ => Some(RowTarget::None),
                        }
                    }
                    rho_desk::NodeKind::Page => {
                        match node.bindings.get(&rho_desk::BindingKind::Page)? {
                            rho_desk::Binding::Page(page_id) => Some(RowTarget::TreePage {
                                host,
                                node_id,
                                topic_node_id: topic?,
                                page_id: rho_browser::PageId(uuid::Uuid::from_bytes(page_id.0)),
                            }),
                            _ => Some(RowTarget::None),
                        }
                    }
                    _ => {
                        let topic = topic?;
                        let first_attention = self
                            .tree_heading_agents
                            .get(&(host, topic))
                            .into_iter()
                            .flatten()
                            .copied()
                            .find(|agent_id| registry.attention(*agent_id) >= UiAttention::Pending);
                        Some(RowTarget::TreeTopic {
                            host,
                            node_id: topic,
                            first_attention,
                            on_heading_line: node.kind == rho_desk::NodeKind::Heading,
                        })
                    }
                }
            }
        }
    }

    /// The heading that owns the cursor position: the containing heading
    /// for document positions, the bound heading for agent rows.
    pub fn cursor_topic(&self, cx: &mut Context<Workspace>) -> Option<(HostId, rho_desk::NodeId)> {
        match self.cursor_place(cx)? {
            CursorPlace::Tree(host, node_id, _) => {
                let source = self.tree_hosts.get(&host)?;
                let node = source.nodes.iter().find(|node| node.id == node_id)?;
                let topic = if node.kind == rho_desk::NodeKind::Heading {
                    node.id
                } else {
                    nearest_tree_heading(source, node.parent)?
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
    ) -> Option<(HostId, rho_desk::NodeId, String)> {
        let (host, node_id) = self.cursor_topic(cx)?;
        let room = self.room_for_node(host, node_id, cx)?;
        Some((host, node_id, room.name))
    }

    /// Whether the cursor is somewhere dashboard verbs apply: a heading
    /// line of the document or a generated agent row.
    pub fn cursor_on_heading_line(&self, cx: &mut Context<Workspace>) -> bool {
        self.tree_node_at_cursor(cx).is_some_and(|(host, node_id)| {
            self.tree_hosts.get(&host).is_some_and(|source| {
                source
                    .nodes
                    .iter()
                    .any(|node| node.id == node_id && node.kind == rho_desk::NodeKind::Heading)
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
                .any(|node| node.id == node_id && node.kind == rho_desk::NodeKind::Heading)
        });
        if !is_heading {
            return false;
        }
        if !self.tree_collapsed.insert((host, node_id)) {
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
            return "nothing needs you".to_owned();
        }
        format!(
            "{} dealt · {} waiting",
            self.queue_depth.dealt_count, self.queue_depth.total_alive
        )
    }
}

fn select_deal(
    hand: &DealQueue,
    exclude: Option<&DealCardIdentity>,
) -> Option<(DealCard, DealFingerprint, Vec<DealCardIdentity>)> {
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

fn age_label(days: f64) -> String {
    if days < 1.0 / 24.0 {
        format!("{}m", (days * 1_440.0).max(0.0).round() as i64)
    } else if days < 1.0 {
        format!("{:.1}h", days * 24.0)
    } else {
        format!("{days:.1}d")
    }
}

fn tree_mark_at(mark: &rho_desk::TemporalMark) -> chrono::NaiveDateTime {
    mark.at().unwrap_or(chrono::NaiveDateTime::MIN)
}

fn tree_mark_priority(
    kind: rho_desk::TemporalKind,
    mark: &rho_desk::TemporalMark,
    now: chrono::NaiveDateTime,
) -> f64 {
    rho_desk::temporal_priority(kind, *mark, now)
}

fn tree_temporal_label(
    kind: rho_desk::TemporalKind,
    mark: &rho_desk::TemporalMark,
    now: chrono::NaiveDateTime,
    priority: f64,
) -> String {
    let elapsed = now.signed_duration_since(tree_mark_at(mark)).num_seconds() as f64 / 86_400.0;
    match kind {
        rho_desk::TemporalKind::Deadline if elapsed > 0.0 => {
            format!("deadline · {}d late", elapsed.floor() as u64)
        }
        rho_desk::TemporalKind::Deadline => format!("deadline · {}d", (-elapsed).ceil() as u64),
        rho_desk::TemporalKind::Todo if priority >= 0.0 => {
            format!("todo · ripe {}d", priority.floor() as u64)
        }
        rho_desk::TemporalKind::Todo => "todo".to_owned(),
        rho_desk::TemporalKind::Reminder => "reminder".to_owned(),
        rho_desk::TemporalKind::Defer => format!("deferred · woke {}", age_label(elapsed)),
        rho_desk::TemporalKind::Done | rho_desk::TemporalKind::Discarded => String::new(),
    }
}

fn tree_breadcrumb(
    node_id: rho_desk::NodeId,
    nodes: &HashMap<rho_desk::NodeId, &rho_desk::MaterializedNode>,
    titles: &HashMap<rho_desk::NodeId, String>,
) -> String {
    let mut path = Vec::new();
    let mut cursor = Some(node_id);
    while let Some(node_id) = cursor {
        let Some(node) = nodes.get(&node_id) else {
            break;
        };
        if node.kind == rho_desk::NodeKind::Heading {
            path.push(titles.get(&node_id).map_or("", String::as_str));
        }
        cursor = node.parent;
    }
    path.reverse();
    path.join(" › ")
}

fn dealer_inbox_items(
    inbox: &crate::inbox::InboxStore,
    default_host: HostId,
    at_ms: i64,
) -> Vec<DealerInboxItem> {
    use crate::inbox::{InboxKind, SourceReference};

    inbox
        .pending_items(at_ms)
        .map(|item| {
            let kind = match item.kind {
                InboxKind::Capture => DealerInboxKind::Capture,
                InboxKind::Obligation => DealerInboxKind::Obligation,
                InboxKind::Ping => DealerInboxKind::Ping,
                InboxKind::Slack => DealerInboxKind::Slack,
            };
            let source = match &item.source {
                SourceReference::Page { id } => id
                    .parse()
                    .map(DealerInboxSource::Page)
                    .unwrap_or_else(|_| DealerInboxSource::Other(id.clone())),
                SourceReference::SlackThread {
                    workspace,
                    channel,
                    thread_ts,
                    ..
                } => DealerInboxSource::SlackThread {
                    workspace: workspace.clone(),
                    channel: channel.clone(),
                    thread_ts: thread_ts.clone(),
                },
                SourceReference::DeskNode { .. } => DealerInboxSource::Other("desk".into()),
                SourceReference::External { source, reference } => {
                    DealerInboxSource::Other(format!("{source}:{reference}"))
                }
                SourceReference::None => DealerInboxSource::Other(String::new()),
            };
            let context = match (
                item.context.room.as_deref(),
                item.context.focused_surface.as_str(),
            ) {
                (Some(room), "") => Some(room.to_owned()),
                (Some(room), surface) => Some(format!("{room} / {surface}")),
                (None, "") => None,
                (None, surface) => Some(surface.to_owned()),
            };
            DealerInboxItem {
                id: item.id.0.clone(),
                host: default_host,
                title: item.text.clone(),
                kind,
                captured_at: chrono::DateTime::<chrono::Utc>::from_timestamp_millis(
                    item.captured_at_ms,
                )
                .unwrap_or(chrono::DateTime::UNIX_EPOCH)
                .fixed_offset(),
                deferred_until: item.deferred_until_ms.and_then(|at| {
                    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(at)
                        .map(|at| at.fixed_offset())
                }),
                resurfacing_count: item.resurfacing_count,
                waiting_on: item.waiting_on.clone(),
                source: (!matches!(item.source, SourceReference::None)).then_some(source),
                context,
            }
        })
        .collect()
}

#[derive(Clone)]
struct RankedDealCard {
    priority: f64,
    virtual_reply: bool,
    order: usize,
    fingerprint: DealFingerprint,
    card: DealCard,
}

fn inbox_deal_queue(
    inbox: &[DealerInboxItem],
    now: chrono::DateTime<chrono::FixedOffset>,
    skipped: &HashMap<DealCardIdentity, SkippedCard>,
) -> DealQueue {
    let mut ranked = Vec::new();
    for (index, item) in inbox.iter().enumerate() {
        let identity = DealCardIdentity::Inbox(item.id.clone());
        if item.deferred_until.is_some_and(|until| until > now) {
            continue;
        }
        let age_days = now
            .signed_duration_since(item.captured_at)
            .num_seconds()
            .max(0) as f64
            / 86_400.0;
        let pace = match item.kind {
            // A Slack thread is someone waiting on a reply, exactly like a
            // ping, so it paces and rises the same way.
            DealerInboxKind::Ping | DealerInboxKind::Slack | DealerInboxKind::Obligation => {
                INBOX_OBLIGATION_PACE_DAYS
            }
            DealerInboxKind::Capture => INBOX_CAPTURE_PACE_DAYS,
        };
        let mut score =
            rho_desk::todo_priority(item.captured_at.naive_local(), pace, now.naive_local());
        if matches!(item.kind, DealerInboxKind::Ping | DealerInboxKind::Slack) {
            score += age_days;
        }
        let fingerprint = DealFingerprint(format!("{item:?}"));
        if score <= DEAL_QUEUE_FLOOR
            || skipped.get(&identity).is_some_and(|skip| {
                now < skip.at + chrono::Duration::minutes(SKIP_COOLDOWN_MINUTES)
                    && skip.fingerprint == fingerprint
            })
        {
            continue;
        }
        let mut label = match (item.kind, &item.waiting_on) {
            (DealerInboxKind::Ping, _) => "ping".to_owned(),
            // Shaped like the agent card: the state, then how long it has
            // been in that state.
            (DealerInboxKind::Slack, Some(_)) => "waiting on them".to_owned(),
            (DealerInboxKind::Slack, None) => "waiting on you".to_owned(),
            (DealerInboxKind::Obligation, _) => "obligation".to_owned(),
            (DealerInboxKind::Capture, _) => "capture".to_owned(),
        };
        label.push_str(&format!(" · {}", age_label(age_days)));
        if let Some(context) = item.context.as_deref().filter(|value| !value.is_empty()) {
            label.push_str(" · from ");
            label.push_str(context);
        }
        ranked.push(RankedDealCard {
            priority: score,
            virtual_reply: false,
            order: index,
            fingerprint,
            card: DealCard {
                label,
                priority: score,
                host: item.host,
                subject_node_id: None,
                topic_node_id: None,
                agent_id: None,
                agent_tag: None,
                breadcrumb: item.title.clone(),
                room: item
                    .context
                    .as_deref()
                    .and_then(|value| value.split(" / ").next())
                    .map(str::to_owned),
                kind: DealCardKind::Inbox(item.kind),
                identity,
                inbox_source: item.source.clone(),
            },
        });
    }
    ranked.sort_by(|a, b| {
        b.priority
            .total_cmp(&a.priority)
            .then_with(|| a.order.cmp(&b.order))
    });
    let fingerprints = ranked
        .iter()
        .map(|item| (item.card.identity.clone(), item.fingerprint.clone()))
        .collect();
    let cards = ranked.into_iter().map(|item| item.card).collect::<Vec<_>>();
    DealQueue {
        total_alive: cards.len(),
        dealt_count: usize::from(!cards.is_empty()),
        considered_not_dealt: cards
            .iter()
            .skip(1)
            .take(5)
            .map(|card| card.identity.clone())
            .collect(),
        cards,
        fingerprints,
    }
}

fn deal_hint(deal: &DealSession) -> String {
    format!("DEAL · {} · {}", deal.card.breadcrumb, deal.card.label)
}

impl Dashboard {
    pub fn heading_destination_candidates(
        &self,
        cx: &App,
    ) -> Vec<(String, String, HostId, rho_desk::NodeId)> {
        self.tree_hosts
            .iter()
            .flat_map(|(host, source)| {
                source.nodes.iter().filter_map(move |node| {
                    if node.kind != rho_desk::NodeKind::Heading {
                        return None;
                    }
                    let title = source.buffers.get(&node.id)?.read(cx).text();
                    Some((
                        title.trim().to_owned(),
                        self.breadcrumb_for_node_for_source(node.id, source, cx)?,
                        *host,
                        node.id,
                    ))
                })
            })
            .collect()
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
                    .filter(|node| node.kind == rho_desk::NodeKind::Heading)
                    .filter_map(|node| {
                        let title = source.buffers.get(&node.id)?.read(cx).text();
                        title.to_lowercase().contains(&needle).then(|| {
                            (
                                title.clone(),
                                self.breadcrumb_for_node_for_source(node.id, source, cx)
                                    .unwrap_or(title),
                            )
                        })
                    })
            })
            .collect()
    }

    fn breadcrumb_for_node_for_source(
        &self,
        node_id: rho_desk::NodeId,
        source: &TreeHostSource,
        cx: &App,
    ) -> Option<String> {
        let nodes = source
            .nodes
            .iter()
            .map(|node| (node.id, node))
            .collect::<HashMap<_, _>>();
        let titles = source
            .buffers
            .iter()
            .map(|(id, buffer)| (*id, buffer.read(cx).text()))
            .collect::<HashMap<_, _>>();
        Some(tree_breadcrumb(node_id, &nodes, &titles))
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
        topic: (HostId, rho_desk::NodeId),
        cx: &App,
    ) -> Result<(HostId, rho_desk::NodeId, String, Option<String>), &'static str> {
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
        let project = match node.bindings.get(&rho_desk::BindingKind::File) {
            Some(rho_desk::Binding::File(path)) => Some(path.to_string()),
            _ => None,
        };
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
                        .map(|agent| (*topic, agent))
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
            .and_then(|node| node.parent)
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
                    .filter(|node| node.kind == rho_desk::NodeKind::Heading)
                    .map(move |node| (*host, node.id))
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
        let has_children = self
            .tree_hosts
            .get(&key.0)
            .is_some_and(|source| source.nodes.iter().any(|node| node.parent == Some(key.1)));
        if !has_children {
            return false;
        }
        if !self.tree_collapsed.insert(key) {
            self.tree_collapsed.remove(&key);
        }
        cx.notify();
        true
    }
    pub fn prepare_taken_deal_edit(&mut self, _cx: &mut Context<Workspace>) -> bool {
        self.current_deal_card()
            .and_then(|card| card.topic_node_id)
            .is_some()
    }
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
        let thread = |waiting_on: Option<&str>| DealerInboxItem {
            id: "slack".into(),
            host: HostId(1),
            title: "can you look at the deploy?".into(),
            kind: DealerInboxKind::Slack,
            captured_at: now - chrono::Duration::days(2),
            deferred_until: None,
            resurfacing_count: 0,
            waiting_on: waiting_on.map(str::to_owned),
            source: Some(DealerInboxSource::SlackThread {
                workspace: "acme".into(),
                channel: "C1".into(),
                thread_ts: "500.0".into(),
            }),
            context: Some("#design".into()),
        };

        let queue = inbox_deal_queue(&[thread(None)], now, &HashMap::new());
        assert_eq!(queue.cards.len(), 1);
        assert_eq!(queue.cards[0].label, "waiting on you · 2.0d · from #design");
        assert_eq!(queue.cards[0].breadcrumb, "can you look at the deploy?");
        assert_eq!(queue.cards[0].room.as_deref(), Some("#design"));
        assert_eq!(
            queue.cards[0].kind,
            DealCardKind::Inbox(DealerInboxKind::Slack)
        );
        // The card carries enough to reopen the thread, and no ids reach the
        // text the user reads.
        assert!(matches!(
            queue.cards[0].inbox_source,
            Some(DealerInboxSource::SlackThread { .. })
        ));
        assert!(!queue.cards[0].label.contains("C1"));
        assert!(!queue.cards[0].label.contains("500.0"));

        let answered = inbox_deal_queue(&[thread(Some("#design"))], now, &HashMap::new());
        assert_eq!(
            answered.cards[0].label,
            "waiting on them · 2.0d · from #design"
        );

        // Two days of someone waiting must outrank two days of a capture,
        // the same way a blocked agent outranks an FYI.
        let capture = DealerInboxItem {
            id: "capture".into(),
            kind: DealerInboxKind::Capture,
            waiting_on: None,
            source: None,
            context: None,
            ..thread(None)
        };
        let mixed = inbox_deal_queue(&[capture, thread(None)], now, &HashMap::new());
        assert_eq!(
            mixed.cards[0].identity,
            DealCardIdentity::Inbox("slack".into())
        );
    }
}
