//! Agent lifecycle and selection.
//!
//! One explicit state machine instead of parallel sets: an agent is *known*
//! (appears in topics or was announced), and optionally *live* (this
//! connection has received frames for it). Selection and next/previous
//! cycling operate over live agents only.

use std::collections::BTreeMap;
use std::path::PathBuf;

use rho_ui_proto::{AgentId, UiTopic};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentLife {
    Known,
    Live,
}

#[derive(Default)]
pub struct AgentRegistry {
    agents: BTreeMap<AgentId, AgentLife>,
    topics: Vec<UiTopic>,
    selected: Option<AgentId>,
}

impl AgentRegistry {
    pub fn set_topics(&mut self, topics: Vec<UiTopic>) {
        for topic in &topics {
            for agent_id in topic.agent_ids() {
                self.agents.entry(agent_id).or_insert(AgentLife::Known);
            }
        }
        self.topics = topics;
    }

    /// Where a new agent should work: the newest agent in the topic sets the
    /// precedent, since sibling agents usually share a project.
    pub fn last_working_directory(&self, topic_id: rho_ui_proto::TopicId) -> Option<PathBuf> {
        self.topics
            .iter()
            .find(|topic| topic.topic_id == topic_id)?
            .agents
            .last()
            .map(|agent| agent.working_directory.clone())
    }

    /// The topic an agent currently belongs to, from topic summaries.
    pub fn topic_of(&self, agent_id: AgentId) -> Option<rho_ui_proto::TopicId> {
        self.topics
            .iter()
            .find(|topic| topic.agent_ids().any(|id| id == agent_id))
            .map(|topic| topic.topic_id)
    }

    /// The working directory of an agent, from topic summaries.
    pub fn working_directory(&self, agent_id: AgentId) -> Option<&PathBuf> {
        self.topics
            .iter()
            .flat_map(|topic| topic.agents.iter())
            .find(|agent| agent.agent_id == agent_id)
            .map(|agent| &agent.working_directory)
    }

    pub fn add_topic(&mut self, topic: UiTopic) {
        let mut topics = std::mem::take(&mut self.topics);
        topics.retain(|existing| existing.topic_id != topic.topic_id);
        topics.push(topic);
        topics.sort_by_key(|left| left.topic_id);
        self.set_topics(topics);
    }

    pub fn topics(&self) -> &[UiTopic] {
        &self.topics
    }

    pub fn mark_known(&mut self, agent_id: AgentId) {
        self.agents.entry(agent_id).or_insert(AgentLife::Known);
    }

    pub fn mark_live(&mut self, agent_id: AgentId) {
        self.agents.insert(agent_id, AgentLife::Live);
    }

    pub fn is_live(&self, agent_id: AgentId) -> bool {
        self.agents.get(&agent_id) == Some(&AgentLife::Live)
    }

    pub fn selected(&self) -> Option<&AgentId> {
        self.selected.as_ref()
    }

    pub fn select(&mut self, agent_id: Option<AgentId>) {
        self.selected = agent_id;
    }

    /// Cycles through live agents by `delta`, starting from the current
    /// selection.
    pub fn next_live_agent(&self, delta: isize) -> Option<AgentId> {
        let live = self
            .agents
            .iter()
            .filter(|(_, life)| **life == AgentLife::Live)
            .map(|(agent_id, _)| agent_id)
            .collect::<Vec<_>>();
        if live.is_empty() {
            return None;
        }
        let len = live.len() as isize;
        let index = self
            .selected
            .as_ref()
            .and_then(|selected| live.iter().position(|agent_id| *agent_id == selected))
            .map(|index| (index as isize + delta).rem_euclid(len) as usize)
            .unwrap_or_else(|| if delta < 0 { live.len() - 1 } else { 0 });
        live.get(index).map(|agent_id| *(*agent_id))
    }

    pub fn known_agents(&self) -> impl Iterator<Item = &AgentId> {
        self.agents.keys()
    }

    pub fn live_agents(&self) -> impl Iterator<Item = &AgentId> {
        self.agents
            .iter()
            .filter(|(_, life)| **life == AgentLife::Live)
            .map(|(agent_id, _)| agent_id)
    }
}
