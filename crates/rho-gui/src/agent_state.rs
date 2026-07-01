use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy)]
pub(crate) struct AgentContextUsage {
    pub(crate) input_tokens: Option<u64>,
    pub(crate) percent_used: Option<u8>,
    pub(crate) context_window: Option<u64>,
}

#[derive(Default)]
pub(crate) struct AgentState {
    current_agent_id: Option<String>,
    known_agents: HashSet<String>,
    live_agents: HashSet<String>,
    context_usage: HashMap<String, AgentContextUsage>,
    query_agents: HashMap<String, String>,
    prompt_agents: HashMap<String, String>,
    tool_agents: HashMap<String, String>,
    shell_agents: HashMap<String, String>,
    running_agents: HashSet<String>,
}

impl AgentState {
    pub(crate) fn current_agent_id(&self) -> Option<&str> {
        self.current_agent_id.as_deref()
    }

    pub(crate) fn current_agent_id_owned(&self) -> Option<String> {
        self.current_agent_id.clone()
    }

    pub(crate) fn clear_current_agent(&mut self) {
        self.current_agent_id = None;
    }

    pub(crate) fn remember(&mut self, agent_id: impl Into<String>) {
        self.known_agents.insert(agent_id.into());
    }

    pub(crate) fn mark_live(&mut self, agent_id: impl Into<String>) {
        let agent_id = agent_id.into();
        self.known_agents.insert(agent_id.clone());
        self.live_agents.insert(agent_id);
    }

    pub(crate) fn select(&mut self, agent_id: impl Into<String>) {
        let agent_id = agent_id.into();
        self.known_agents.insert(agent_id.clone());
        self.live_agents.insert(agent_id.clone());
        if self.current_agent_id.as_deref() != Some(agent_id.as_str()) {
            self.current_agent_id = Some(agent_id);
        }
    }

    pub(crate) fn unload(&mut self, agent_id: &str) {
        self.live_agents.remove(agent_id);
        if self.current_agent_id.as_deref() == Some(agent_id) {
            self.current_agent_id = None;
        }
    }

    pub(crate) fn known(&self, agent_id: &str) -> bool {
        self.known_agents.contains(agent_id)
    }

    pub(crate) fn selected_is_active(&self) -> bool {
        let Some(agent_id) = self.current_agent_id.as_deref() else {
            return true;
        };
        self.live_agents.contains(agent_id)
    }

    #[cfg(test)]
    pub(crate) fn running(&self, agent_id: &str) -> bool {
        self.running_agents.contains(agent_id)
    }

    pub(crate) fn known_agents_sorted(&self) -> Vec<String> {
        let mut known_agents = self.known_agents.iter().cloned().collect::<Vec<_>>();
        known_agents.sort();
        known_agents
    }

    pub(crate) fn completion_snapshot(&self) -> (Vec<String>, HashSet<String>) {
        (self.known_agents_sorted(), self.live_agents.clone())
    }

    pub(crate) fn next_active_agent(&self, delta: isize) -> Option<String> {
        let active_agents = self
            .known_agents_sorted()
            .into_iter()
            .filter(|agent| self.live_agents.contains(agent))
            .collect::<Vec<_>>();
        if active_agents.is_empty() {
            return None;
        }
        let len = active_agents.len() as isize;
        let index = self
            .current_agent_id
            .as_deref()
            .and_then(|current| active_agents.iter().position(|agent| agent == current))
            .map(|index| (index as isize + delta).rem_euclid(len) as usize)
            .unwrap_or_else(|| {
                if delta < 0 {
                    active_agents.len() - 1
                } else {
                    0
                }
            });
        active_agents.get(index).cloned()
    }

    pub(crate) fn record_context_usage(&mut self, agent_id: String, usage: AgentContextUsage) {
        self.context_usage.insert(agent_id, usage);
    }

    pub(crate) fn selected_context_usage(&self) -> Option<AgentContextUsage> {
        self.current_agent_id
            .as_deref()
            .and_then(|agent_id| self.context_usage.get(agent_id).copied())
    }

    pub(crate) fn clear_context_usage(&mut self) {
        self.context_usage.clear();
    }

    pub(crate) fn clear_routing(&mut self) {
        self.query_agents.clear();
        self.prompt_agents.clear();
        self.tool_agents.clear();
        self.shell_agents.clear();
        self.running_agents.clear();
    }

}
