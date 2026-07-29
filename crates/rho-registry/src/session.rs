//! Connection-session bookkeeping shared by every client surface: which
//! agents this connection subscribes to, and which agent-stream incarnation
//! is current. Pure state machines; each client supplies its own transport.

use std::collections::{HashMap, HashSet, VecDeque};

use rho_ui_proto::{AgentId, UiAgentSummary, UiWorkstream, WorkstreamId};

pub const MAX_AGENT_SUBSCRIPTIONS: usize = 128;
pub const INITIAL_AGENT_SUBSCRIPTIONS: usize = 10;

/// Connection-local live transcript streams. Recently selected agents stay
/// subscribed until this deliberately generous bound is reached; unloaded
/// agents reject any transport frames buffered before the server's notice.
#[derive(Default)]
pub struct AgentSubscriptions {
    lru: VecDeque<AgentId>,
    unloaded: HashSet<AgentId>,
}

impl AgentSubscriptions {
    pub fn reset(&mut self, agent_ids: &[AgentId]) {
        self.lru.clear();
        self.unloaded.clear();
        for agent_id in agent_ids.iter().rev().copied() {
            if !self.lru.contains(&agent_id) {
                self.lru.push_back(agent_id);
            }
        }
    }

    pub fn contains(&self, agent_id: AgentId) -> bool {
        self.lru.contains(&agent_id)
    }

    /// Currently subscribed agents, oldest first.
    pub fn iter(&self) -> impl Iterator<Item = AgentId> + '_ {
        self.lru.iter().copied()
    }

    /// Promotes an existing subscription or adds one. Returns whether a new
    /// subscribe request is needed and the oldest subscription to release.
    pub fn touch(&mut self, agent_id: AgentId) -> (bool, Option<AgentId>) {
        self.unloaded.remove(&agent_id);
        let was_subscribed = self
            .lru
            .iter()
            .position(|subscribed| *subscribed == agent_id)
            .map(|index| self.lru.remove(index))
            .is_some();
        self.lru.push_back(agent_id);
        let evicted = (self.lru.len() > MAX_AGENT_SUBSCRIPTIONS)
            .then(|| self.lru.pop_front())
            .flatten();
        if let Some(evicted) = evicted {
            self.unloaded.insert(evicted);
        }
        (!was_subscribed, evicted)
    }

    pub fn mark_unloaded(
        &mut self,
        agent_id: AgentId,
        reason: rho_ui_proto::AgentUnloadReason,
    ) -> bool {
        // An unsubscribe acknowledgement can cross a rapid re-subscribe on
        // QUIC's independent streams. Ignore it once touch() cleared the
        // pending-unload marker. Daemon idle eviction is always authoritative.
        if reason == rho_ui_proto::AgentUnloadReason::Unsubscribed
            && !self.unloaded.contains(&agent_id)
        {
            return false;
        }
        if let Some(index) = self.lru.iter().position(|id| *id == agent_id) {
            self.lru.remove(index);
        }
        self.unloaded.insert(agent_id);
        true
    }

    pub fn accepts_frames(&self, agent_id: AgentId) -> bool {
        !self.unloaded.contains(&agent_id)
    }
}

/// Agent state arrives on server-opened unidirectional streams that may be
/// reopened at any time; frames from a superseded stream must not reach the
/// state. `open` registers a stream's incarnation, `is_current` gates each
/// frame before it is applied.
#[derive(Default)]
pub struct AgentStreamGenerations {
    generations: HashMap<AgentId, u64>,
}

impl AgentStreamGenerations {
    pub fn open(&mut self, agent_id: AgentId) -> u64 {
        let generation = self.generations.entry(agent_id).or_default();
        *generation = generation.wrapping_add(1);
        *generation
    }

    pub fn is_current(&self, agent_id: AgentId, generation: u64) -> bool {
        self.generations.get(&agent_id) == Some(&generation)
    }
}

/// One top-level transcript from each of the most recently active visible
/// workstreams. Descendant activity raises its workstream's recency without
/// making the whole subtree part of the initial subscription set.
pub fn recent_workstream_roots(
    workstreams: &[UiWorkstream],
    agents: &[UiAgentSummary],
    selected: Option<AgentId>,
    limit: usize,
) -> Vec<AgentId> {
    let hidden = workstreams
        .iter()
        .filter(|workstream| {
            workstream
                .labels
                .iter()
                .any(|label| label == crate::HIDE_LABEL)
        })
        .map(|workstream| workstream.workstream_id)
        .collect::<HashSet<_>>();
    let mut recency = HashMap::<WorkstreamId, rho_core::UnixMs>::new();
    let mut roots = HashMap::<WorkstreamId, (rho_core::UnixMs, AgentId)>::new();
    for agent in agents
        .iter()
        .filter(|agent| !agent.hidden && !hidden.contains(&agent.workstream))
    {
        recency
            .entry(agent.workstream)
            .and_modify(|last| *last = (*last).max(agent.last_active))
            .or_insert(agent.last_active);
        if agent.parent_agent.is_none() {
            roots
                .entry(agent.workstream)
                .and_modify(|root| {
                    if agent.last_active > root.0 {
                        *root = (agent.last_active, agent.agent_id);
                    }
                })
                .or_insert((agent.last_active, agent.agent_id));
        }
    }
    let mut roots = roots
        .into_iter()
        .map(|(workstream, (_, agent_id))| (recency[&workstream], workstream, agent_id))
        .collect::<Vec<_>>();
    roots.sort_by_key(|(last_active, workstream, agent_id)| {
        (std::cmp::Reverse(*last_active), *workstream, *agent_id)
    });
    let mut selected_roots = roots
        .into_iter()
        .take(limit)
        .map(|(_, _, agent_id)| agent_id)
        .collect::<Vec<_>>();
    if let Some(selected) = selected
        && agents.iter().any(|agent| {
            agent.agent_id == selected && !agent.hidden && !hidden.contains(&agent.workstream)
        })
    {
        selected_roots.retain(|agent_id| *agent_id != selected);
        selected_roots.insert(0, selected);
        selected_roots.truncate(limit);
    }
    selected_roots
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: u64, workstream: u64, last_active: u64) -> UiAgentSummary {
        UiAgentSummary {
            agent_id: AgentId::from_counter(id, &rho_ui_proto::AgentIdDomain(0)).unwrap(),
            parent_agent: None,
            display_name: None,
            created_at: rho_core::UnixMs(last_active),
            updated_at: rho_core::UnixMs(last_active),
            role: rho_ui_proto::AgentRole::default(),
            workspace: rho_ui_proto::WorkspaceInfo::UserCheckout {
                repo: "/tmp".into(),
            },
            attention: rho_ui_proto::UiAttention::Quiet,
            last_active: rho_core::UnixMs(last_active),
            hidden: false,
            last_user_message_text: String::new(),
            workstream: WorkstreamId(workstream),
            labels: Vec::new(),
        }
    }

    #[test]
    fn initial_subscriptions_are_bounded_recent_visible_roots_with_selection() {
        let mut workstreams = (1..=12)
            .map(|id| UiWorkstream {
                workstream_id: WorkstreamId(id),
                name: format!("workstream-{id}"),
                labels: Vec::new(),
            })
            .collect::<Vec<_>>();
        // The newest workstream is explicitly hidden and must not consume a
        // preload slot.
        workstreams[11].labels.push(crate::HIDE_LABEL.to_owned());
        let agents = (1..=12).map(|id| summary(id, id, id)).collect::<Vec<_>>();
        let selected = agents[0].agent_id;

        let subscriptions = recent_workstream_roots(&workstreams, &agents, Some(selected), 10);

        assert_eq!(subscriptions.len(), 10);
        assert_eq!(subscriptions[0], selected);
        assert!(!subscriptions.contains(&agents[11].agent_id));
        assert!(subscriptions.contains(&agents[10].agent_id));
    }

    #[test]
    fn selecting_oldest_subscription_protects_it_from_next_eviction() {
        let ids = (1..=MAX_AGENT_SUBSCRIPTIONS as u64 + 1)
            .map(|id| AgentId::from_counter(id, &rho_ui_proto::AgentIdDomain(0)).unwrap())
            .collect::<Vec<_>>();
        let mut subscriptions = AgentSubscriptions::default();
        for agent_id in ids[..MAX_AGENT_SUBSCRIPTIONS].iter().copied() {
            assert_eq!(subscriptions.touch(agent_id), (true, None));
        }

        assert_eq!(subscriptions.touch(ids[0]), (false, None));
        assert_eq!(
            subscriptions.touch(ids[MAX_AGENT_SUBSCRIPTIONS]),
            (true, Some(ids[1]))
        );
        assert!(subscriptions.contains(ids[0]));
    }

    #[test]
    fn stale_unsubscribe_ack_does_not_replace_new_subscription() {
        let id = AgentId::from_counter(1, &rho_ui_proto::AgentIdDomain(0)).unwrap();
        let mut subscriptions = AgentSubscriptions::default();
        subscriptions.unloaded.insert(id);
        assert_eq!(subscriptions.touch(id), (true, None));

        assert!(!subscriptions.mark_unloaded(id, rho_ui_proto::AgentUnloadReason::Unsubscribed,));
        assert!(subscriptions.contains(id));
        assert!(subscriptions.accepts_frames(id));
    }

    #[test]
    fn daemon_idle_unload_is_authoritative() {
        let id = AgentId::from_counter(1, &rho_ui_proto::AgentIdDomain(0)).unwrap();
        let mut subscriptions = AgentSubscriptions::default();
        assert_eq!(subscriptions.touch(id), (true, None));

        assert!(subscriptions.mark_unloaded(id, rho_ui_proto::AgentUnloadReason::Idle));
        assert!(!subscriptions.contains(id));
        assert!(!subscriptions.accepts_frames(id));
    }
}
