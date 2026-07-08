//! Agent lifecycle and selection.
//!
//! One explicit state machine instead of parallel sets: an agent is *known*
//! (appears in topics or was announced), and optionally *live* (this
//! connection has received frames for it). Selection and next/previous
//! cycling operate over live agents only.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use camino::Utf8PathBuf;
use rho_ui_proto::{AgentId, UiTopic};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentLife {
    Known,
    Live,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActivePane {
    /// Initial bootstrapping state: the draft surface is visible, but the
    /// first live agent may claim the view once daemon frames arrive.
    #[default]
    Startup,
    /// The user intentionally opened the new-agent compose surface.
    Draft,
    Agent(AgentId),
}

#[derive(Default)]
pub struct AgentRegistry {
    agents: BTreeMap<AgentId, AgentLife>,
    /// Live attention overlay: broadcasts land here between topic refreshes,
    /// which re-seed it from the summaries' snapshot.
    attention: BTreeMap<AgentId, rho_ui_proto::UiAttention>,
    topics: Vec<UiTopic>,
    active: ActivePane,
    /// The daemon database's machine seed, from `Ready`; kept for consumers
    /// that resolve ids.
    machine_seed: u64,
    /// Last agent id counter, from `Ready`; keys uniform agent label prefix
    /// length.
    agent_counter: u64,
    /// Last workspace id counter, from `Ready`; keys uniform workspace label
    /// prefix length just like the generated-agent population does for agents.
    workspace_counter: u64,
}

impl AgentRegistry {
    pub fn set_machine_seed(&mut self, machine_seed: u64) {
        self.machine_seed = machine_seed;
    }

    pub fn set_agent_counter(&mut self, agent_counter: u64) {
        self.agent_counter = agent_counter;
    }

    pub fn set_workspace_counter(&mut self, workspace_counter: u64) {
        self.workspace_counter = workspace_counter;
    }

    pub fn set_topics(&mut self, topics: Vec<UiTopic>) {
        self.attention.clear();
        for topic in &topics {
            for agent in &topic.agents {
                self.agents
                    .entry(agent.agent_id)
                    .or_insert(AgentLife::Known);
                self.attention.insert(agent.agent_id, agent.attention);
            }
        }
        self.topics = topics;
    }

    pub fn set_attention(&mut self, agent_id: AgentId, attention: rho_ui_proto::UiAttention) {
        self.attention.insert(agent_id, attention);
    }

    pub fn attention(&self, agent_id: AgentId) -> rho_ui_proto::UiAttention {
        self.attention.get(&agent_id).copied().unwrap_or_default()
    }

    /// The rail-visible agent most in need of the user, excluding the one
    /// already on screen: highest attention wins, rail order breaks ties.
    /// Only Pending and above count — jumping to a quiet or merely working
    /// agent would be noise.
    pub fn next_attention_agent(&self) -> Option<AgentId> {
        let selected = self.selected_agent().copied();
        self.rail_order()
            .into_iter()
            .filter(|agent_id| Some(*agent_id) != selected && !self.agent_hidden(*agent_id))
            .map(|agent_id| (agent_id, self.attention(agent_id)))
            .filter(|(_, attention)| *attention >= rho_ui_proto::UiAttention::Pending)
            .min_by_key(|(_, attention)| Reverse(*attention))
            .map(|(agent_id, _)| agent_id)
    }

    /// Where a new agent should work: the newest agent in the topic sets the
    /// precedent, since sibling agents usually share a project.
    pub fn last_working_directory(&self, topic_id: rho_ui_proto::TopicId) -> Option<Utf8PathBuf> {
        self.topics
            .iter()
            .find(|topic| topic.topic_id == topic_id)?
            .agents
            .last()
            .map(|agent| agent.workspace.repo().to_owned())
    }

    /// The topic an agent currently belongs to, from topic summaries.
    pub fn topic_of(&self, agent_id: AgentId) -> Option<rho_ui_proto::TopicId> {
        self.topics
            .iter()
            .find(|topic| topic.agent_ids().any(|id| id == agent_id))
            .map(|topic| topic.topic_id)
    }

    /// The short display label of an agent: an `ag-`-prefixed slice of its
    /// encoded ID, unique among all generated IDs.
    pub fn agent_id_label(&self, agent_id: AgentId) -> String {
        let prefix_len = prefix_id::uniform_prefix_len(self.agent_counter, LABEL_HEADROOM).max(4);
        format!("ag-{}", &agent_id.encoded()[..prefix_len])
    }

    pub fn working_directory(&self, agent_id: AgentId) -> Option<Utf8PathBuf> {
        self.agent_summary(agent_id)
            .map(|agent| agent.workspace.repo().to_owned())
    }

    pub fn agent_workspace(&self, agent_id: AgentId) -> Option<&rho_ui_proto::WorkspaceInfo> {
        self.agent_summary(agent_id).map(|agent| &agent.workspace)
    }

    pub fn workspace_id_label(&self, agent_id: AgentId) -> Option<String> {
        let workspace_id = self
            .agent_summary(agent_id)
            .and_then(|agent| agent.workspace.workspace_id())?;
        let prefix_len =
            prefix_id::uniform_prefix_len(self.workspace_counter, LABEL_HEADROOM).max(2);
        Some(format!("ws-{}", &workspace_id.encoded()[..prefix_len]))
    }

    pub fn agent_mode(&self, agent_id: AgentId) -> Option<rho_ui_proto::AgentMode> {
        self.agent_summary(agent_id).map(|agent| agent.mode)
    }

    /// The pin/archive status of an agent, from topic summaries.
    pub fn agent_status(&self, agent_id: AgentId) -> rho_ui_proto::Status {
        self.agent_summary(agent_id)
            .map(|agent| agent.status)
            .unwrap_or(rho_ui_proto::Status::Normal)
    }

    fn agent_summary(&self, agent_id: AgentId) -> Option<&rho_ui_proto::UiAgentSummary> {
        self.topics
            .iter()
            .flat_map(|topic| topic.agents.iter())
            .find(|agent| agent.agent_id == agent_id)
    }

    pub fn agent_display_name(&self, agent_id: AgentId) -> Option<&str> {
        self.agent_summary(agent_id)
            .and_then(|agent| agent.display_name.as_deref())
    }

    pub fn agent_display_label(&self, agent_id: AgentId) -> String {
        let id_label = self.agent_id_label(agent_id);
        match self.agent_display_name(agent_id) {
            Some(name) if !name.trim().is_empty() => format!("{name} ({id_label})"),
            _ => id_label,
        }
    }

    pub fn add_topic(&mut self, topic: UiTopic) {
        // Topics stay in the daemon's creation order; a new topic is the
        // newest, so it belongs at the end.
        let mut topics = std::mem::take(&mut self.topics);
        topics.retain(|existing| existing.topic_id != topic.topic_id);
        topics.push(topic);
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

    pub fn active_pane(&self) -> ActivePane {
        self.active
    }

    pub fn selected_agent(&self) -> Option<&AgentId> {
        match &self.active {
            ActivePane::Agent(agent_id) => Some(agent_id),
            ActivePane::Startup | ActivePane::Draft => None,
        }
    }

    pub fn select_agent(&mut self, agent_id: AgentId) {
        self.active = ActivePane::Agent(agent_id);
    }

    pub fn enter_draft(&mut self) {
        self.active = ActivePane::Draft;
    }

    /// Hidden from the rail: archived itself, or in an archived topic. Such
    /// agents stay loadable by id but are skipped by cycling.
    pub fn agent_hidden(&self, agent_id: AgentId) -> bool {
        self.topics.iter().any(|topic| {
            topic.agents.iter().any(|agent| {
                agent.agent_id == agent_id
                    && (agent.status == rho_ui_proto::Status::Archived
                        || topic.status == rho_ui_proto::Status::Archived)
            })
        })
    }

    /// Cycles through live, rail-visible agents by `delta`, starting from
    /// the current selection. Cycling follows rail order (topics, then
    /// agents within each topic); agent id order is meaningless.
    pub fn next_live_agent(&self, delta: isize) -> Option<AgentId> {
        let live = self
            .rail_order()
            .into_iter()
            .filter(|agent_id| {
                self.agents.get(agent_id) == Some(&AgentLife::Live) && !self.agent_hidden(*agent_id)
            })
            .collect::<Vec<_>>();
        if live.is_empty() {
            return None;
        }
        let len = live.len() as isize;
        let index = self
            .selected_agent()
            .and_then(|selected| live.iter().position(|agent_id| agent_id == selected))
            .map(|index| (index as isize + delta).rem_euclid(len) as usize)
            .unwrap_or_else(|| if delta < 0 { live.len() - 1 } else { 0 });
        live.get(index).copied()
    }

    /// All agents in rail display order: pinned topics first, and within a
    /// topic attention-first, then pins, then newest. Agents known outside
    /// any topic summary trail at the end.
    fn rail_order(&self) -> Vec<AgentId> {
        let mut topics = self.topics.iter().collect::<Vec<_>>();
        topics.sort_by_key(|topic| topic.status != rho_ui_proto::Status::Pinned);

        let mut candidates = Vec::new();
        for topic in topics {
            if topic.status == rho_ui_proto::Status::Archived {
                continue;
            }
            let mut agents = topic
                .agents
                .iter()
                .filter(|agent| agent.status != rho_ui_proto::Status::Archived)
                .collect::<Vec<_>>();
            agents.sort_by_key(|agent| {
                (
                    Reverse(self.attention(agent.agent_id)),
                    agent.status != rho_ui_proto::Status::Pinned,
                    Reverse(agent.created_at),
                )
            });
            candidates.extend(agents.into_iter().map(|agent| agent.agent_id));
        }
        for agent_id in self.agents.keys() {
            if !candidates.contains(agent_id) {
                candidates.push(*agent_id);
            }
        }
        candidates
    }

    /// Resolves an agent label (as produced by [`Self::agent_id_label`],
    /// with or without a leading `@`) or display name back to the agent id.
    pub fn agent_by_label(&self, label: &str) -> Option<AgentId> {
        let label = label.strip_prefix('@').unwrap_or(label);
        self.agents.keys().copied().find(|agent_id| {
            self.agent_id_label(*agent_id) == label
                || self
                    .agent_display_name(*agent_id)
                    .is_some_and(|name| name.eq_ignore_ascii_case(label))
        })
    }

    pub fn live_agents(&self) -> impl Iterator<Item = &AgentId> {
        self.agents
            .iter()
            .filter(|(_, life)| **life == AgentLife::Live)
            .map(|(agent_id, _)| agent_id)
    }
}

/// New agents guaranteed between two label-length changes.
const LABEL_HEADROOM: u64 = 200;

#[cfg(test)]
mod tests {
    use rho_ui_proto::{
        AgentIdDomain, Status, TopicIdDomain, UiAgentSummary, WorkspaceId, WorkspaceIdDomain,
    };

    use super::*;

    fn agent_id(id: u64) -> AgentId {
        AgentId::from_counter(id, &AgentIdDomain(0)).unwrap()
    }

    fn agent(id: u64, status: Status) -> UiAgentSummary {
        UiAgentSummary {
            agent_id: agent_id(id),
            display_name: None,
            created_at: rho_core::UnixMs(id),
            updated_at: rho_core::UnixMs(id),
            mode: rho_ui_proto::AgentMode::deep_default(),
            workspace: rho_ui_proto::WorkspaceInfo::UserCheckout {
                repo: "/tmp".into(),
            },
            status,
            attention: rho_ui_proto::UiAttention::Quiet,
        }
    }

    fn named_agent(id: u64, name: &str) -> UiAgentSummary {
        UiAgentSummary {
            display_name: Some(name.to_owned()),
            ..agent(id, Status::Normal)
        }
    }

    fn workspace_agent(id: u64, workspace_id: WorkspaceId) -> UiAgentSummary {
        UiAgentSummary {
            agent_id: agent_id(id),
            display_name: None,
            created_at: rho_core::UnixMs(id),
            updated_at: rho_core::UnixMs(id),
            mode: rho_ui_proto::AgentMode::deep_default(),
            workspace: rho_ui_proto::WorkspaceInfo::Workspace {
                repo: "/tmp".into(),
                id: workspace_id,
            },
            status: Status::Normal,
            attention: rho_ui_proto::UiAttention::Quiet,
        }
    }

    fn topic(id: u64, status: Status, agents: Vec<UiAgentSummary>) -> UiTopic {
        UiTopic {
            topic_id: rho_ui_proto::TopicId::from_counter(id, &TopicIdDomain(0)).unwrap(),
            name: id.to_string(),
            status,
            agents,
        }
    }

    #[test]
    fn cycling_follows_active_rail_order() {
        let mut registry = AgentRegistry::default();
        registry.set_topics(vec![topic(
            1,
            Status::Normal,
            vec![
                agent(1, Status::Normal),
                agent(2, Status::Pinned),
                agent(3, Status::Normal),
            ],
        )]);
        for id in 1..=3 {
            registry.mark_live(agent_id(id));
        }

        // Active rail order is pinned first, then newest-created first:
        // 2, 3, 1. Forward cycling should move down that visible order.
        registry.select_agent(agent_id(2));
        assert_eq!(registry.next_live_agent(1), Some(agent_id(3)));
        registry.select_agent(agent_id(3));
        assert_eq!(registry.next_live_agent(1), Some(agent_id(1)));
        assert_eq!(registry.next_live_agent(-1), Some(agent_id(2)));
    }

    #[test]
    fn cycling_skips_archived_agents_and_archived_topics() {
        let mut registry = AgentRegistry::default();
        registry.set_topics(vec![
            topic(
                1,
                Status::Normal,
                vec![agent(1, Status::Normal), agent(2, Status::Archived)],
            ),
            topic(2, Status::Archived, vec![agent(3, Status::Normal)]),
        ]);
        for id in 1..=3 {
            registry.mark_live(agent_id(id));
        }

        let visible = agent_id(1);
        registry.select_agent(visible);
        // Both forward and backward cycling only ever land on the one
        // rail-visible agent.
        assert_eq!(registry.next_live_agent(1), Some(visible));
        assert_eq!(registry.next_live_agent(-1), Some(visible));
        assert!(registry.agent_hidden(agent_id(2)));
        assert!(registry.agent_hidden(agent_id(3)));
    }

    #[test]
    fn attention_jump_picks_most_urgent_excluding_selected() {
        use rho_ui_proto::UiAttention;

        let mut registry = AgentRegistry::default();
        registry.set_topics(vec![topic(
            1,
            Status::Normal,
            vec![
                agent(1, Status::Normal),
                agent(2, Status::Normal),
                agent(3, Status::Normal),
            ],
        )]);
        assert_eq!(registry.next_attention_agent(), None);

        registry.set_attention(agent_id(1), UiAttention::Pending);
        registry.set_attention(agent_id(2), UiAttention::NeedsInput);
        registry.set_attention(agent_id(3), UiAttention::Working);
        assert_eq!(registry.next_attention_agent(), Some(agent_id(2)));

        // The agent already on screen never wins the jump; the next-most
        // urgent one does. Working alone never qualifies.
        registry.select_agent(agent_id(2));
        assert_eq!(registry.next_attention_agent(), Some(agent_id(1)));
        registry.set_attention(agent_id(1), UiAttention::Quiet);
        assert_eq!(registry.next_attention_agent(), None);
    }

    #[test]
    fn attention_outranks_pins_in_rail_order() {
        use rho_ui_proto::UiAttention;

        let mut registry = AgentRegistry::default();
        registry.set_topics(vec![topic(
            1,
            Status::Normal,
            vec![agent(1, Status::Normal), agent(2, Status::Pinned)],
        )]);
        for id in 1..=2 {
            registry.mark_live(agent_id(id));
        }
        registry.set_attention(agent_id(1), UiAttention::NeedsInput);

        // Pinned agent 2 would lead by status, but agent 1's attention
        // pushes it to the top of the rail (and thus of cycling).
        registry.select_agent(agent_id(1));
        assert_eq!(registry.next_live_agent(1), Some(agent_id(2)));
        assert_eq!(registry.next_live_agent(-1), Some(agent_id(2)));
    }

    #[test]
    fn workspace_labels_use_uniform_unique_prefixes() {
        let domain = WorkspaceIdDomain(0);
        let short_workspace = WorkspaceId::from_counter(1, &domain).unwrap();
        let long_workspace = WorkspaceId::from_counter(36 * 36, &domain).unwrap();

        let mut registry = AgentRegistry::default();
        registry.set_machine_seed(0);
        registry.set_workspace_counter(36 * 36);
        registry.set_topics(vec![topic(
            1,
            Status::Normal,
            vec![
                workspace_agent(1, short_workspace),
                workspace_agent(2, long_workspace),
                agent(3, Status::Normal),
            ],
        )]);

        assert_eq!(
            registry.workspace_id_label(agent_id(1)),
            Some(format!("ws-{}", &short_workspace.encoded()[..3]))
        );
        assert_eq!(
            registry.workspace_id_label(agent_id(2)),
            Some(format!("ws-{}", &long_workspace.encoded()[..3]))
        );
        assert_eq!(registry.workspace_id_label(agent_id(3)), None);
    }

    #[test]
    fn agent_labels_use_ready_counter() {
        let id = agent_id(1);
        let mut registry = AgentRegistry::default();
        registry.set_agent_counter(36 * 36);

        assert_eq!(
            registry.agent_id_label(id),
            format!("ag-{}", &id.encoded()[..4])
        );
    }

    #[test]
    fn agent_lookup_accepts_display_name() {
        let mut registry = AgentRegistry::default();
        registry.set_topics(vec![topic(
            1,
            Status::Normal,
            vec![named_agent(1, "Fix Tests")],
        )]);

        assert_eq!(registry.agent_by_label("fix tests"), Some(agent_id(1)));
        assert_eq!(
            registry.agent_by_label(&registry.agent_id_label(agent_id(1))),
            Some(agent_id(1))
        );
    }
}
