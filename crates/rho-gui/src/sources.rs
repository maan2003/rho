//! The sources: each says what its nodes want of the user, what they are
//! called, and what a verdict on one of them writes. The marks any node
//! can carry — mute, snooze, todo, labels — are applied around them, in
//! [`crate::attention`].

use std::collections::HashMap;

use rho_agent_types::AgentId;
use rho_agents_client::AgentMap;
use rho_dealer::curve::{
    BLOCKED_REPLY_HEAD_START, CHANNEL_ANSWERED_DROP, CHANNEL_TRAFFIC_HEAD_START,
    THREAD_REPLY_HEAD_START,
};
use rho_dealer::marks::{self, Cursor, NodeMarks, Write};
use rho_dealer::{CardKind, Curve, Marks, NodeId, SlackUnit, Want};

use crate::attention::Verdict;

/// The agents, as the agents map knows them.
pub(crate) struct AgentsSource<'a> {
    pub(crate) map: &'a AgentMap,
    /// When the user last wrote to each agent.
    pub(crate) touched: &'a HashMap<AgentId, i64>,
}

impl AgentsSource<'_> {
    pub(crate) fn title(&self, agent_id: AgentId) -> String {
        self.map
            .agent_human_name(agent_id)
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned()
    }

    /// An agent's want: its last turn ended with something the user has
    /// not dealt with. An agent created by an agent belongs to its creator
    /// and wants nothing of its own.
    pub(crate) fn want(&self, agent_id: AgentId, marks: &NodeMarks, context: &str) -> Option<Want> {
        if !self.map.created_by_user(agent_id) || self.map.host_of_agent(agent_id).is_none() {
            return None;
        }
        let digest = self.map.agent_digest(agent_id)?;
        if digest.newest.0 <= handled(marks) {
            return None;
        }
        let facts = self.map.agent_facts(agent_id);
        let ended = facts.last_turn_ended?;
        if facts.turn_running || ended <= facts.last_user_message_at {
            return None;
        }
        let since_ms = ended.0 as i64;
        // A dead turn is not an FYI: only the user can start it again.
        let curve = if facts.errored || facts.needs_you_hint {
            Curve::Waiting {
                head_start: BLOCKED_REPLY_HEAD_START,
                since_ms,
            }
        } else {
            Curve::Fading {
                head_start: 0.0,
                since_ms,
            }
        };
        Some(Want {
            kind: CardKind::Agent,
            title: self.title(agent_id),
            context: context.to_owned(),
            reason: agent_reason(&facts).to_owned(),
            curve,
            touched_ms: self.touched.get(&agent_id).copied(),
            cursor: format!("{}", digest.newest.0),
        })
    }

    /// Done and todo hand the agent over through its newest story; mute is
    /// the ledger's.
    pub(crate) fn verdict(&self, agent_id: AgentId, verdict: Verdict) -> Vec<Write> {
        let node = NodeId::Agent(agent_id);
        match verdict {
            Verdict::Done | Verdict::Todo { .. } => {
                let newest = self
                    .map
                    .agent_digest(agent_id)
                    .map_or(0, |digest| digest.newest.0);
                vec![marks::handled(&node, Some(Cursor::Story(newest)))]
            }
            Verdict::Mute => vec![marks::muted(&node, true)],
            Verdict::Snooze(_) => Vec::new(),
        }
    }

    /// What the agents map is told of an agent's marks: how far it is
    /// handled and whether it is muted.
    pub(crate) fn map_verdict(marks: &NodeMarks) -> rho_agents_client::Verdict {
        rho_agents_client::Verdict {
            handled_through: rho_agent_types::AgentPos(handled(marks)),
            muted: marks.muted,
        }
    }
}

fn handled(marks: &NodeMarks) -> u64 {
    match marks.handled {
        Some(Cursor::Story(pos)) => pos,
        _ => 0,
    }
}

fn agent_reason(facts: &rho_agents_client::AgentFacts) -> &'static str {
    if facts.errored {
        "errored · {age} ago"
    } else if facts.needs_you_hint {
        "waiting on reply · {age}"
    } else {
        "finished · {age} ago"
    }
}

/// What Slack says about one unit right now, read live from the mirror.
/// Nothing of it is kept: the words, the wait and the newest message stay
/// in Slack, and a card's title is rendered fresh every time.
#[derive(Clone, Debug, PartialEq)]
pub struct SlackFacts {
    pub title: String,
    pub conversation: String,
    /// Why Slack is asking for the reader here, or `None` when it is not
    /// asking at all: a unit whose messages have all been read is still a
    /// unit — Find reaches it — but it is no longer a card.
    pub reason: Option<rho_slack::model::Attention>,
    /// How long the ball has been where it is, counted from the newest
    /// message.
    pub wait_days: f64,
    /// The newest message in the unit: a new one voids a skip.
    pub latest: String,
    /// Whether somebody else has already answered in this run.
    pub others_replied: bool,
}

/// Slack, as the mirror last read. Its verdicts are Slack's own: done
/// moves Slack's cursor and mute mutes in Slack, so the ledger holds
/// nothing of them (see `take_verdict`).
#[derive(Default)]
pub(crate) struct SlackSource {
    units: HashMap<SlackUnit, SlackFacts>,
}

impl SlackSource {
    /// Takes the mirror's units as they now are, and says which units to
    /// make wants for again: every one now, and every one gone.
    pub(crate) fn read(&mut self, units: HashMap<SlackUnit, SlackFacts>) -> Vec<SlackUnit> {
        let gone: Vec<SlackUnit> = self
            .units
            .keys()
            .filter(|unit| !units.contains_key(*unit))
            .cloned()
            .collect();
        self.units = units;
        self.units.keys().cloned().chain(gone).collect()
    }

    pub(crate) fn units(&self) -> impl Iterator<Item = &SlackUnit> {
        self.units.keys()
    }

    pub(crate) fn title(&self, unit: &SlackUnit) -> String {
        self.units
            .get(unit)
            .map(|facts| facts.title.clone())
            .unwrap_or_else(|| unit.channel.clone())
    }

    pub(crate) fn context(&self, unit: &SlackUnit) -> Option<String> {
        self.units.get(unit).map(|facts| facts.conversation.clone())
    }

    /// A unit's want, or `None` when Slack is not asking. A room is not a
    /// person waiting: it fades from the moment it is seen, and lower
    /// again when somebody else is already answering in it. Everything
    /// else is someone waiting, and rises.
    pub(crate) fn want(&self, unit: &SlackUnit, now_ms: i64) -> Option<Want> {
        let facts = self.units.get(unit)?;
        let reason = facts.reason?;
        let since_ms = now_ms - (facts.wait_days * 86_400_000.0) as i64;
        let (state, curve) = match reason {
            rho_slack::model::Attention::ChannelTraffic => (
                "unread",
                Curve::Fading {
                    head_start: CHANNEL_TRAFFIC_HEAD_START
                        - if facts.others_replied {
                            CHANNEL_ANSWERED_DROP
                        } else {
                            0.0
                        },
                    since_ms,
                },
            ),
            _ => (
                "needs reply",
                Curve::Waiting {
                    head_start: THREAD_REPLY_HEAD_START,
                    since_ms,
                },
            ),
        };
        let why = rho_slack::model::reason_text(reason, &facts.conversation);
        Some(Want {
            kind: CardKind::Slack,
            title: facts.title.clone(),
            context: facts.conversation.clone(),
            reason: format!("{why} · {state} · {{age}}"),
            curve,
            touched_ms: None,
            cursor: facts.latest.clone(),
        })
    }
}

/// Notes and labels: made by the user, held whole in the ledger, and
/// wanting nothing but what their own dates say.
pub(crate) struct NotesSource<'a> {
    pub(crate) marks: &'a Marks,
}

impl NotesSource<'_> {
    pub(crate) fn note_title(&self, note: &NodeId) -> String {
        match self.marks.get(note).title() {
            "" => "untitled note".to_owned(),
            title => title.to_owned(),
        }
    }

    pub(crate) fn label_title(&self, label: uuid::Uuid) -> String {
        self.marks.label_path(label)
    }

    /// Mute is the ledger's; the rest ask nothing of a note or a label.
    pub(crate) fn verdict(&self, node: &NodeId, verdict: Verdict) -> Vec<Write> {
        match verdict {
            Verdict::Mute => vec![marks::muted(node, true)],
            Verdict::Done | Verdict::Todo { .. } | Verdict::Snooze(_) => Vec::new(),
        }
    }
}
