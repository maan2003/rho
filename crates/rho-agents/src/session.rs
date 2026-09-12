//! Connection-session bookkeeping shared by every client surface: which
//! agents this client holds whole, and which agent-stream incarnation is
//! current. Pure state machines; each client supplies its own transport.

use std::collections::VecDeque;

use rho_ui_proto::AgentId;

/// How many agents a client holds whole at once.
pub const MAX_ACTIVE_AGENTS: usize = 4;

/// The agents a client holds whole: every event, the transcript folded
/// from them, and the daemon's live tail. Everything else is a digest.
/// The most recently looked-at stay; leaving drops the rest.
#[derive(Default)]
pub struct ActiveAgents {
    /// Oldest first.
    lru: VecDeque<AgentId>,
}

impl ActiveAgents {
    pub fn contains(&self, agent_id: AgentId) -> bool {
        self.lru.contains(&agent_id)
    }

    /// Active agents, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = AgentId> + '_ {
        self.lru.iter().copied()
    }

    /// Makes an agent the newest. Returns whether it was not active before.
    pub fn touch(&mut self, agent_id: AgentId) -> bool {
        let was_active = self.remove(agent_id);
        self.lru.push_back(agent_id);
        !was_active
    }

    /// Takes out whoever is beyond the bound, oldest first. An agent on
    /// screen is never taken, so a bound of four can hold five while five
    /// are shown; that is the user's choice, not a leak.
    pub fn evict(&mut self, pinned: impl Fn(AgentId) -> bool) -> Vec<AgentId> {
        let mut evicted = Vec::new();
        let mut index = 0;
        while self.lru.len() - evicted.len() > MAX_ACTIVE_AGENTS && index < self.lru.len() {
            if pinned(self.lru[index]) {
                index += 1;
            } else {
                evicted.push(self.lru.remove(index).expect("index in bounds"));
            }
        }
        evicted
    }

    /// Returns whether it was active.
    pub fn remove(&mut self, agent_id: AgentId) -> bool {
        match self.lru.iter().position(|id| *id == agent_id) {
            Some(index) => {
                self.lru.remove(index);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(count: u64) -> Vec<AgentId> {
        (1..=count)
            .map(|id| AgentId::from_counter(id, &rho_ui_proto::AgentIdDomain(0)).unwrap())
            .collect()
    }

    #[test]
    fn the_oldest_leaves_unless_it_is_on_screen() {
        let ids = ids(MAX_ACTIVE_AGENTS as u64 + 2);
        let mut active = ActiveAgents::default();
        for agent_id in &ids[..MAX_ACTIVE_AGENTS] {
            assert!(active.touch(*agent_id));
        }
        assert!(active.evict(|_| false).is_empty());

        // Looking at the oldest again keeps it.
        assert!(!active.touch(ids[0]));
        assert!(active.touch(ids[MAX_ACTIVE_AGENTS]));
        assert_eq!(active.evict(|_| false), vec![ids[1]]);
        assert!(active.contains(ids[0]));

        // One on screen is passed over for the next oldest.
        assert!(active.touch(ids[MAX_ACTIVE_AGENTS + 1]));
        assert_eq!(active.evict(|id| id == ids[2]), vec![ids[3]]);
        assert!(active.contains(ids[2]));
    }
}
