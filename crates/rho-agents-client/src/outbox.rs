//! What the user wrote to agents that no host has taken yet.
//!
//! A message goes in here the moment it is written, under a random id the
//! client picks, and onto disk with it. It leaves when its host answers
//! that the mail is logged. Until then it is posted again whenever the
//! host comes back: a host logs a `client_id` once, so posting twice is
//! harmless, and words written while the network was down are not lost.
//! An agent's messages go one at a time, oldest first, so the mail log
//! holds them in the order they were written.

use std::collections::HashMap;

use rho_agent_types::{AgentId, ContentPart, UnixMs};

use crate::protocol::rho2::PostMail;

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
    /// The id on its way to each agent; what is written meanwhile waits
    /// for its answer.
    sending: HashMap<AgentId, u64>,
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

    /// The agent's oldest unsent message, to post now, unless a post is
    /// already on its way.
    pub fn next(&mut self, agent_id: AgentId) -> Option<PostMail> {
        if self.sending.contains_key(&agent_id) {
            return None;
        }
        let (_, id, outgoing) = self.waiting.iter().find(|(to, ..)| *to == agent_id)?;
        self.sending.insert(agent_id, *id);
        Some(PostMail {
            client_id: *id,
            to: agent_id,
            thread: None,
            content: outgoing.content.clone(),
        })
    }

    /// The host logged what was on its way: it is the agent's mail now.
    pub fn delivered(&mut self, agent_id: AgentId) {
        let Some(sent) = self.sending.remove(&agent_id) else {
            return;
        };
        self.waiting
            .retain(|(to, id, _)| *to != agent_id || *id != sent);
        crate::cache::write_outgoing(agent_id, sent, None);
    }

    /// The post failed; the message waits for the next try.
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

    fn sent(post: Option<PostMail>) -> (u64, String) {
        let post = post.expect("a post");
        (
            post.client_id,
            rho_agent_types::transcript::text_content(&post.content),
        )
    }

    /// An agent's messages go one at a time, oldest first: a failed post
    /// goes again under the same id, so the host logs it once, and what is
    /// written while one is on its way waits behind it.
    #[test]
    fn an_agents_messages_go_one_at_a_time_in_order() {
        let (a, b) = (agent(1), agent(2));
        let mut outbox = Outbox::default();
        let first = outbox.push(a, text("a1"));
        outbox.push(b, text("b1"));
        let second = outbox.push(a, text("a2"));
        assert_eq!(outbox.agents(), [a, b]);

        assert_eq!(sent(outbox.next(a)), (first, "a1".to_owned()));
        assert!(outbox.next(a).is_none(), "one post at a time");
        assert_eq!(sent(outbox.next(b)).1, "b1", "another agent is not held up");

        outbox.failed(a);
        assert_eq!(
            sent(outbox.next(a)),
            (first, "a1".to_owned()),
            "posted again under the same id"
        );
        outbox.delivered(a);
        assert_eq!(outbox.texts(a), ["a2"]);
        assert_eq!(sent(outbox.next(a)), (second, "a2".to_owned()));
        outbox.delivered(a);
        assert!(outbox.texts(a).is_empty());
        assert!(outbox.next(a).is_none());
        assert_eq!(outbox.agents(), [b]);
    }
}
