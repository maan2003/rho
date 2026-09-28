//! What the user wrote to agents that no host has taken yet.
//!
//! A message goes in here the moment it is written, under a random id the
//! client picks, and onto disk with it. It leaves when its host answers
//! that the message is logged. Until then it is sent again whenever the
//! host comes back: a host logs an id once, so sending twice is harmless,
//! and words written while the network was down are not lost. An agent's
//! unsent messages go together in one call, in the order they were
//! written, which the host keeps.

use std::collections::HashMap;

use rho_agent_types::{AgentId, ContentPart, UnixMs};

use crate::protocol::{AgentCommand, UserMessage};

/// One message on disk until its host takes it.
#[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub struct Outgoing {
    pub at: UnixMs,
    pub content: Vec<ContentPart>,
}

#[derive(Default)]
pub struct Outbox {
    /// Oldest first.
    waiting: Vec<(AgentId, u64, Outgoing)>,
    /// The ids on their way to each agent; what is written meanwhile
    /// waits for their answer.
    sending: HashMap<AgentId, Vec<u64>>,
}

impl Outbox {
    /// What the last session left unsent.
    pub fn load() -> Self {
        let mut waiting = crate::cache::read_outbox();
        waiting.sort_by_key(|(_, _, outgoing)| outgoing.at);
        Self {
            waiting,
            sending: HashMap::new(),
        }
    }

    /// Keeps a message the user wrote, on disk before anything is sent.
    pub fn push(&mut self, agent_id: AgentId, content: Vec<ContentPart>) -> u64 {
        let id = rand::random();
        let outgoing = Outgoing {
            at: UnixMs::now(),
            content,
        };
        crate::cache::write_outgoing(agent_id, id, Some(outgoing.clone()));
        self.waiting.push((agent_id, id, outgoing));
        id
    }

    /// Everything unsent to the agent, to send now, unless a send is
    /// already on its way.
    pub fn next(&mut self, agent_id: AgentId) -> Option<AgentCommand> {
        if self.sending.contains_key(&agent_id) {
            return None;
        }
        let messages: Vec<UserMessage> = self
            .waiting
            .iter()
            .filter(|(to, ..)| *to == agent_id)
            .map(|(_, id, outgoing)| UserMessage {
                id: *id,
                content: outgoing.content.clone(),
            })
            .collect();
        if messages.is_empty() {
            return None;
        }
        self.sending.insert(
            agent_id,
            messages.iter().map(|message| message.id).collect(),
        );
        Some(AgentCommand::Send { agent_id, messages })
    }

    /// The host logged what was on its way: it is the agent's now.
    pub fn delivered(&mut self, agent_id: AgentId) {
        let Some(sent) = self.sending.remove(&agent_id) else {
            return;
        };
        self.waiting
            .retain(|(to, id, _)| *to != agent_id || !sent.contains(id));
        for id in sent {
            crate::cache::write_outgoing(agent_id, id, None);
        }
    }

    /// The send failed; the messages wait for the next try.
    pub fn failed(&mut self, agent_id: AgentId) {
        self.sending.remove(&agent_id);
    }

    /// The text of an agent's unsent messages, oldest first.
    pub fn texts(&self, agent_id: AgentId) -> Vec<String> {
        self.waiting
            .iter()
            .filter(|(to, ..)| *to == agent_id)
            .map(|(_, _, outgoing)| rho_agent_types::transcript::text_content(&outgoing.content))
            .collect()
    }

    /// Every agent with something unsent.
    pub fn agents(&self) -> Vec<AgentId> {
        let mut agents = Vec::new();
        for (agent_id, ..) in &self.waiting {
            if !agents.contains(agent_id) {
                agents.push(*agent_id);
            }
        }
        agents
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(n: u64) -> AgentId {
        AgentId::from_counter(n, &rho_agent_types::AgentIdDomain(0)).expect("an agent id")
    }

    fn text(text: &str) -> Vec<ContentPart> {
        vec![ContentPart::Text { text: text.into() }]
    }

    fn sent(command: Option<AgentCommand>) -> Vec<(u64, String)> {
        match command {
            Some(AgentCommand::Send { messages, .. }) => messages
                .into_iter()
                .map(|message| {
                    let text = rho_agent_types::transcript::text_content(&message.content);
                    (message.id, text)
                })
                .collect(),
            other => panic!("expected a send, got {other:?}"),
        }
    }

    /// Everything unsent to an agent goes in one call, oldest first, and
    /// one call at a time: a failed call goes again whole, under the same
    /// ids, and what is written while one is on its way waits for its
    /// answer rather than being taken as delivered with it.
    #[test]
    fn an_agents_messages_go_together_in_order() {
        let (a, b) = (agent(1), agent(2));
        let mut outbox = Outbox::default();
        let first = outbox.push(a, text("a1"));
        outbox.push(b, text("b1"));
        let second = outbox.push(a, text("a2"));
        assert_eq!(outbox.agents(), [a, b]);

        let both = vec![(first, "a1".to_owned()), (second, "a2".to_owned())];
        assert_eq!(sent(outbox.next(a)), both);
        assert!(outbox.next(a).is_none(), "one call at a time");
        assert_eq!(
            sent(outbox.next(b)).len(),
            1,
            "another agent is not held up"
        );

        outbox.failed(a);
        assert_eq!(sent(outbox.next(a)), both, "sent again whole, same ids");
        let third = outbox.push(a, text("a3"));
        outbox.delivered(a);
        assert_eq!(outbox.texts(a), ["a3"]);
        assert_eq!(sent(outbox.next(a)), [(third, "a3".to_owned())]);
        outbox.delivered(a);
        assert!(outbox.texts(a).is_empty());
        assert!(outbox.next(a).is_none());
        assert_eq!(outbox.agents(), [b]);
    }
}
