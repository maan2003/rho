//! One agent as this CLI follows it: its rows folded into a transcript,
//! the host's live tail on top.

use std::collections::HashMap;

use rho_agent_types::AgentId;
use rho_agents_client::protocol::transcript::{Live, LogEntry};
use rho_agents_client::state::UiAgentState;
use rho_agents_client::store::AgentStore;
use rho_agents_client::{AgentIdentity, HostId, MirroredAgent, TranscriptFold};

pub struct Followed {
    pub agent_id: AgentId,
    fold: TranscriptFold,
    store: AgentStore,
}

impl Followed {
    pub fn new(agent_id: AgentId) -> Self {
        Self {
            agent_id,
            fold: TranscriptFold::default(),
            store: AgentStore::default(),
        }
    }

    /// Folds this agent's rows out of a run of the journal. Returns whether
    /// the transcript moved.
    pub fn tell(&mut self, entries: &[LogEntry]) -> bool {
        for entry in entries
            .iter()
            .filter(|entry| entry.agent_id == self.agent_id)
        {
            self.fold.tell(entry.pos, &entry.event);
        }
        match self.fold.delta() {
            Some(delta) => {
                self.store.apply_fold_delta(self.agent_id, delta);
                true
            }
            None => false,
        }
    }

    pub fn live(&mut self, live: Live) {
        self.store.apply_live(self.agent_id, live);
    }

    pub fn state(&self) -> Option<&UiAgentState> {
        self.store.get(&self.agent_id)
    }
}

/// Every agent the journal has named so far, from its `Created` row: for
/// finding the one to attach to, and for naming the ones it talks to.
#[derive(Default)]
pub struct Directory {
    agents: HashMap<AgentId, AgentIdentity>,
}

impl Directory {
    pub fn tell(&mut self, entries: &[LogEntry]) {
        for entry in entries {
            if let Some(agent) = MirroredAgent::new(HostId(0), entry.agent_id, &entry.event) {
                self.agents.insert(entry.agent_id, agent.identity);
            }
        }
    }

    pub fn get(&self, agent_id: AgentId) -> Option<&AgentIdentity> {
        self.agents.get(&agent_id)
    }

    /// The agent a handle or the start of an id names: `eng-k3f9…`, or
    /// just `k3f9`.
    pub fn find(&self, text: &str) -> Result<AgentId, String> {
        let text = text.trim();
        let prefix = match text.split_once('-') {
            Some(("eng" | "adv", id)) => id,
            _ => text,
        };
        let mut matches = self
            .agents
            .keys()
            .filter(|agent_id| agent_id.encoded().starts_with(prefix));
        match (matches.next(), matches.next()) {
            (Some(agent_id), None) if !prefix.is_empty() => Ok(*agent_id),
            (Some(_), Some(_)) => Err(format!("`{text}` names more than one agent")),
            _ => Err(format!("no agent `{text}` on this host")),
        }
    }

    /// How a message names another agent: its handle, cut short.
    pub fn label(&self, agent_id: AgentId) -> String {
        short_handle(
            self.agents
                .get(&agent_id)
                .map_or("agent", |identity| identity.role.handle_prefix()),
            agent_id,
        )
    }
}

/// `eng-k3f9a1`: enough of the id to tell agents apart on one host and to
/// attach with.
pub fn short_handle(prefix: &str, agent_id: AgentId) -> String {
    let encoded = agent_id.encoded();
    format!("{prefix}-{}", &encoded[..encoded.len().min(6)])
}
