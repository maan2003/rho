//! The hand: every node that wants the user, ranked, read straight off the
//! sources and what the user said about them.
//!
//! [`rank`] is the whole algorithm. It takes the agents as the agents map
//! holds them, Slack as its model holds it, and the user's own facts, and
//! works each card out from scratch; nothing it decides is kept between
//! calls except the [`Cache`], which only saves reading the same Slack
//! title twice and never changes an answer. The cases it must meet are in
//! `cases.md`, and each has a test under its name.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use jiff::{Timestamp, Zoned};
use rho_agents_client::AgentMap;
use rho_slack::model::{Attention, Model, Unit};

use crate::curve::{self, Curve, DEAL_QUEUE_FLOOR};
use crate::facts::{Snooze, slack_ts_order};
use crate::marks::Marks;
use crate::node::{NodeId, SlackUnit};

/// What kind of thing a card is, which decides how it opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CardKind {
    Agent,
    Slack,
    /// A note, or anything else only the user's own dates bring back.
    Dated,
}

/// A node wanting the user, ranked.
#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    pub node: NodeId,
    pub kind: CardKind,
    pub title: String,
    pub context: String,
    pub label: String,
    pub priority: f64,
    /// Where the node's source stood: a skip holds on to it, and the source
    /// moving past it voids the skip.
    pub cursor: String,
    /// Passed over a moment ago, and lower for it until the skip fades.
    pub skipped: bool,
}

/// Every card above the floor, pressing hardest first, and when the hand
/// next changes with nobody touching anything.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hand {
    pub cards: Vec<Card>,
    pub next_change: Option<Timestamp>,
}

impl Hand {
    /// The card a pull deals: the top one that is not already in view.
    pub fn top(&self, exclude: Option<&NodeId>) -> Option<&Card> {
        self.cards.iter().find(|card| exclude != Some(&card.node))
    }
}

/// Slack, as the session holds it: the model, and the mirror the words
/// of a card come from.
#[derive(Clone, Copy)]
pub struct Slack<'a> {
    pub model: &'a Model,
    pub mirror: Option<&'a rho_slack::mirror::Mirror>,
}

/// Everything [`rank`] reads.
#[derive(Clone, Copy)]
pub struct Sources<'a> {
    pub agents: &'a AgentMap,
    pub slack: Option<Slack<'a>>,
    pub marks: &'a Marks,
    pub skips: &'a Skips,
}

/// What [`rank`] keeps between calls to save work: the words of each Slack
/// card, by the message they were read from.
#[derive(Default)]
pub struct Cache {
    titles: BTreeMap<Unit, (rho_slack::types::Ts, String)>,
}

#[derive(Clone, Debug)]
struct Skip {
    at: Timestamp,
    cursor: String,
}

/// The cards the user passed over, in memory only: a skip is about this
/// sitting, not something to remember.
#[derive(Default)]
pub struct Skips {
    skips: HashMap<NodeId, Skip>,
}

impl Skips {
    pub fn skip(&mut self, node: NodeId, cursor: String, now: Timestamp) {
        self.skips.insert(node, Skip { at: now, cursor });
    }

    pub fn clear(&mut self, node: &NodeId) -> bool {
        self.skips.remove(node).is_some()
    }

    pub fn contains(&self, node: &NodeId) -> bool {
        self.skips.contains_key(node)
    }

    /// What a skip still takes off the card, if its source has not moved.
    fn penalty(&self, node: &NodeId, cursor: &str, now: Timestamp) -> Option<(f64, Timestamp)> {
        let skip = self.skips.get(node).filter(|skip| skip.cursor == cursor)?;
        let gone = skip.at + curve::SKIP_FADE;
        (now < gone).then(|| {
            (
                curve::fading(skip.at, now, curve::SKIP_FADE, curve::SKIP_PENALTY),
                gone,
            )
        })
    }
}

/// One reason a node wants the user. A node's card is its strongest.
struct Part {
    curve: Curve,
    reason: String,
    /// On top of the curve: the user having just written to it.
    bonus: f64,
    cursor: String,
    /// Said by the node's source, rather than by the user's own dates: the
    /// kind a snooze ending moves to count from its end.
    from_source: bool,
    /// An agent reply to the user's message at this time, soon enough
    /// after it to reach them through a snooze said before it.
    breaks_snooze_set_by: Option<Timestamp>,
    /// When somebody else wrote, in a conversation where each message
    /// makes a snoozed card come back a little higher.
    others: Vec<Timestamp>,
}

impl Part {
    fn source(curve: Curve, reason: String, cursor: String) -> Self {
        Self {
            curve,
            reason,
            bonus: 0.0,
            cursor,
            from_source: true,
            breaks_snooze_set_by: None,
            others: Vec::new(),
        }
    }

    fn dated(curve: Curve, cursor: String) -> Self {
        Self {
            from_source: false,
            ..Self::source(curve, String::new(), cursor)
        }
    }
}

fn unix(ms: impl Into<i64>) -> Timestamp {
    Timestamp::from_millisecond(ms.into()).unwrap_or(Timestamp::UNIX_EPOCH)
}

/// The hand, as everything stands at `now`.
pub fn rank(sources: &Sources<'_>, now: &Zoned, cache: &mut Cache) -> Hand {
    let at = now.timestamp();
    let marks = sources.marks;
    let mut parts: BTreeMap<NodeId, Vec<Part>> = BTreeMap::new();
    let mut running = BTreeSet::new();

    // Agents (A1–A9): its last turn ended with something the user has not
    // dealt with and has not answered.
    let agents = sources.agents;
    for agent_id in agents.known_agents().copied() {
        let node = NodeId::Agent(agent_id);
        if !agents.created_by_user(agent_id) || agents.host_of_agent(agent_id).is_none() {
            continue;
        }
        let Some(digest) = agents.agent_digest(agent_id) else {
            continue;
        };
        let facts = agents.agent_facts(agent_id);
        if facts.turn_running {
            running.insert(node.clone());
        }
        let seen = marks.get(&node).facts().seen_agent().unwrap_or(0);
        let Some(ended) = facts.last_turn_ended else {
            continue;
        };
        if facts.turn_running || ended <= facts.last_user_message_at || digest.newest.0 <= seen {
            continue;
        }
        let ended = unix(ended.0 as i64);
        let (curve, reason) = if facts.errored || facts.needs_you_hint {
            (
                Curve::Waiting {
                    head_start: curve::AGENT_BLOCKED_HEAD_START,
                    since: ended,
                },
                match facts.errored {
                    true => "errored · {age} ago",
                    false => "waiting on reply · {age}",
                },
            )
        } else {
            (
                Curve::Fading {
                    head_start: curve::AGENT_FINISHED_HEAD_START,
                    since: ended,
                    gone_days: curve::AGENT_FINISHED_GONE_DAYS,
                },
                "finished · {age} ago",
            )
        };
        let mut part = Part::source(curve, reason.to_owned(), digest.newest.0.to_string());
        let spoke = unix(facts.last_user_message_at.0 as i64);
        if facts.last_user_message_at.0 > 0 {
            part.bonus = curve::recency_bonus(spoke, at);
            if ended.duration_since(spoke) <= curve::REPLY_BREAKTHROUGH {
                part.breaks_snooze_set_by = Some(spoke);
            }
        }
        parts.entry(node).or_default().push(part);
    }

    // Slack (S1–S6): whatever Slack itself is badging.
    if let Some(slack) = sources.slack {
        let model = slack.model;
        let workspace = &model.workspace().0;
        for (unit, attention) in model.asking() {
            let Some(facts) = model.unit(unit) else {
                continue;
            };
            // Done on another device, having seen this far.
            let node = NodeId::Slack(slack_unit(workspace, unit));
            if marks
                .get(&node)
                .facts()
                .seen_slack()
                .is_some_and(|seen| slack_ts_order(&facts.newest.0, seen).is_le())
            {
                continue;
            }
            let since = unix(
                facts
                    .newest
                    .millis()
                    .max(facts.first_seen_ms.min(at.as_millisecond())),
            );
            let people = facts.people.len() + usize::from(!facts.people.contains(model.self_id()));
            let small = people <= curve::SLACK_SMALL_THREAD_PEOPLE;
            let (state, curve, direct) = match attention {
                Attention::ChannelTraffic => (
                    "unread",
                    Curve::Fading {
                        head_start: curve::CHANNEL_TRAFFIC_HEAD_START,
                        since,
                        gone_days: match facts.others_replied {
                            true => curve::CHANNEL_ANSWERED_GONE_DAYS,
                            false => curve::CHANNEL_TRAFFIC_GONE_DAYS,
                        },
                    },
                    false,
                ),
                Attention::DirectMessage => (
                    "needs reply",
                    waiting(curve::SLACK_DM_HEAD_START, since),
                    true,
                ),
                Attention::Mentioned => (
                    "needs reply",
                    waiting(curve::SLACK_MENTION_HEAD_START, since),
                    false,
                ),
                Attention::FollowedThread if small => (
                    "needs reply",
                    waiting(curve::SLACK_DM_HEAD_START, since),
                    true,
                ),
                Attention::FollowedThread => (
                    "needs reply",
                    waiting(curve::SLACK_THREAD_HEAD_START, since),
                    false,
                ),
            };
            let why = rho_slack::model::reason_text(attention, &model.label(&unit.channel));
            let mut part = Part::source(
                curve,
                format!("{why} · {state} · {{age}}"),
                facts.newest.0.clone(),
            );
            if direct {
                part.others = facts
                    .from_others
                    .iter()
                    .map(|ts| unix(ts.millis()))
                    .collect();
            }
            parts.entry(node).or_default().push(part);
        }
    }

    // The user's own dates (T1–T6, N2), on any node.
    for (node, held) in marks.nodes() {
        let facts = held.facts();
        if let Some(todo) = facts.todo()
            && !running.contains(node)
        {
            parts.entry(node.clone()).or_default().push(Part::dated(
                Curve::Plate { since: todo.start },
                format!("todo {}", todo.set.timestamp().as_millisecond()),
            ));
        }
        if let Some(deadline) = facts.deadline() {
            parts.entry(node.clone()).or_default().push(Part::dated(
                Curve::Deadline {
                    by: deadline.by,
                    lead_days: deadline.lead_days,
                },
                format!("deadline {}", deadline.set.timestamp().as_millisecond()),
            ));
        }
    }

    // One card per node (X1): what the user said about it decides whether
    // it shows, and its strongest part is the card.
    let mut steps = Vec::new();
    let mut cards = Vec::new();
    for (node, mut node_parts) in parts {
        let held = marks.get(&node);
        if held.deleted || held.facts().muted() {
            continue;
        }
        // A snooze on a Slack room holds every thread in it too.
        let room = match &node {
            NodeId::Slack(unit) if unit.thread.is_some() => Some(NodeId::Slack(SlackUnit {
                thread: None,
                ..unit.clone()
            })),
            _ => None,
        };
        let snoozes: Vec<Snooze> = std::iter::once(&node)
            .chain(room.as_ref())
            .filter_map(|node| marks.get(node).facts().snooze())
            .collect();
        // Z1, and Z4's way through.
        if let Some(snooze) = snoozes
            .iter()
            .filter(|snooze| at < snooze.until)
            .max_by_key(|snooze| snooze.until)
        {
            steps.push(snooze.until);
            let set = snooze.set.timestamp();
            node_parts.retain(|part| part.breaks_snooze_set_by.is_some_and(|spoke| set <= spoke));
            if node_parts.is_empty() {
                continue;
            }
        }
        // Z2 and Z5: a node back from a snooze counts from the snooze's
        // end, a little higher for every message it slept through.
        if let Some(snooze) = snoozes
            .iter()
            .filter(|snooze| snooze.until <= at)
            .max_by_key(|snooze| snooze.until)
        {
            let set = snooze.set.timestamp();
            for part in node_parts.iter_mut().filter(|part| part.from_source) {
                match &mut part.curve {
                    Curve::Waiting { since, .. } | Curve::Fading { since, .. }
                        if *since < snooze.until =>
                    {
                        *since = snooze.until;
                        let slept = part
                            .others
                            .iter()
                            .filter(|wrote| set < **wrote && **wrote <= snooze.until)
                            .count();
                        part.bonus += (curve::SNOOZED_MESSAGE_BUMP * slept as f64)
                            .min(curve::SNOOZED_MESSAGE_BUMP_CAP);
                    }
                    _ => {}
                }
            }
        }
        for part in &node_parts {
            steps.extend(part.curve.steps());
        }
        let Some((part, priority)) = node_parts
            .iter()
            .map(|part| (part, part.curve.priority(at) + part.bonus))
            .filter(|(_, priority)| *priority > DEAL_QUEUE_FLOOR)
            .max_by(|a, b| a.1.total_cmp(&b.1))
        else {
            continue;
        };
        let penalty = sources.skips.penalty(&node, &part.cursor, at);
        if let Some((_, gone)) = penalty {
            steps.push(gone);
        }
        cards.push(Card {
            kind: match node {
                NodeId::Agent(_) => CardKind::Agent,
                NodeId::Slack(_) => CardKind::Slack,
                _ => CardKind::Dated,
            },
            title: String::new(),
            context: String::new(),
            label: part.curve.label(&part.reason, at),
            priority: priority - penalty.map_or(0.0, |(penalty, _)| penalty),
            cursor: part.cursor.clone(),
            skipped: penalty.is_some(),
            node,
        });
    }
    cards.sort_by(|a, b| {
        b.priority
            .total_cmp(&a.priority)
            // An agent wins an exact tie: it is the user's own work coming
            // back.
            .then_with(|| (b.kind == CardKind::Agent).cmp(&(a.kind == CardKind::Agent)))
            .then_with(|| a.node.cmp(&b.node))
    });
    for card in &mut cards {
        card.title = title(sources, &card.node, cache);
        card.context = context(sources, &card.node);
    }
    Hand {
        cards,
        next_change: steps.into_iter().filter(|step| *step > at).min(),
    }
}

fn waiting(head_start: f64, since: Timestamp) -> Curve {
    Curve::Waiting { head_start, since }
}

/// The node a Slack unit is.
pub fn slack_unit(workspace: &str, unit: &Unit) -> SlackUnit {
    SlackUnit {
        workspace: workspace.to_owned(),
        channel: unit.channel.0.clone(),
        thread: unit.thread.as_ref().map(|ts| ts.0.clone()),
    }
}

/// The Slack model's unit for a node.
pub fn model_unit(unit: &SlackUnit) -> Unit {
    Unit {
        channel: rho_slack::types::ChannelId(unit.channel.clone()),
        thread: unit.thread.clone().map(rho_slack::types::Ts),
    }
}

/// What a node is called on a card.
pub fn title(sources: &Sources<'_>, node: &NodeId, cache: &mut Cache) -> String {
    match node {
        NodeId::Agent(agent_id) => sources
            .agents
            .agent_human_name(*agent_id)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned(),
        NodeId::Slack(unit) => {
            let Some(slack) = sources.slack else {
                return unit.channel.clone();
            };
            let unit = model_unit(unit);
            let Some(newest) = slack.model.unit(&unit).map(|facts| facts.newest.clone()) else {
                return slack.model.label(&unit.channel);
            };
            if let Some((read, title)) = cache.titles.get(&unit)
                && *read == newest
            {
                return title.clone();
            }
            let title = slack
                .mirror
                .map(|mirror| rho_slack::mirror::unit_summary(slack.model, mirror, &unit))
                .unwrap_or_default();
            cache.titles.insert(unit, (newest, title.clone()));
            title
        }
        NodeId::Note(_) => match sources.marks.get(node).title() {
            "" => "untitled note".to_owned(),
            title => title.to_owned(),
        },
        NodeId::Label(id) => sources.marks.label_path(*id),
        NodeId::PullRequest { repo, number } => format!("{repo}#{number}"),
    }
}

/// Where a node is: a Slack unit's conversation, or the labels on it.
pub fn context(sources: &Sources<'_>, node: &NodeId) -> String {
    if let (NodeId::Slack(unit), Some(slack)) = (node, sources.slack) {
        return slack.model.label(&model_unit(unit).channel);
    }
    let marks = sources.marks;
    marks
        .get(node)
        .labels
        .iter()
        .filter(|label| !marks.get(&NodeId::Label(**label)).deleted)
        .map(|label| marks.label_path(*label))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests;
