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

use rho_agent_types::{AgentId, ContentPart, ThreadId, UnixMs};

use crate::protocol::worksets::PostMail;

/// One message on disk until its host takes it.
#[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub struct Outgoing {
    pub at: UnixMs,
    pub content: Vec<ContentPart>,
    /// The thread it answers in; `None` starts a new one.
    #[senax(default)]
    pub thread: Option<ThreadId>,
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
    pub fn push(
        &mut self,
        agent_id: AgentId,
        thread: Option<ThreadId>,
        content: Vec<ContentPart>,
    ) -> u64 {
        let id = rand::random();
        let outgoing = Outgoing {
            at: UnixMs::now(),
            content,
            thread,
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
            thread: outgoing.thread,
            content: outgoing.content.clone(),
        })
    }

    /// The host logged what was on its way: it is the agent's mail now.
    /// Answers its client id and what it was.
    pub fn delivered(&mut self, agent_id: AgentId) -> Option<(u64, Outgoing)> {
        let sent = self.sending.remove(&agent_id)?;
        let at = self
            .waiting
            .iter()
            .position(|(to, id, _)| *to == agent_id && *id == sent)?;
        let (_, id, outgoing) = self.waiting.remove(at);
        crate::cache::write_outgoing(agent_id, sent, None);
        Some((id, outgoing))
    }

    /// The post failed; the message waits for the next try.
    pub fn failed(&mut self, agent_id: AgentId) {
        self.sending.remove(&agent_id);
    }

    /// The text of the unsent messages in one of an agent's threads,
    /// oldest first.
    pub fn texts(&self, agent_id: AgentId, thread: ThreadId) -> Vec<String> {
        self.waiting
            .iter()
            .filter(|(to, _, outgoing)| *to == agent_id && outgoing.thread == Some(thread))
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

    fn thread() -> ThreadId {
        ThreadId(rho_agent_types::MailId(1))
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
        let first = outbox.push(a, Some(thread()), text("a1"));
        outbox.push(b, None, text("b1"));
        let second = outbox.push(a, Some(thread()), text("a2"));
        assert_eq!(outbox.agents(), [a, b]);

        assert_eq!(sent(outbox.next(a)), (first, "a1".to_owned()));
        assert!(outbox.next(a).is_none(), "one post at a time");
        let to_b = outbox.next(b).expect("a post");
        assert_eq!(to_b.thread, None, "a new thread");
        assert_eq!(sent(Some(to_b)).1, "b1", "another agent is not held up");

        outbox.failed(a);
        assert_eq!(
            sent(outbox.next(a)),
            (first, "a1".to_owned()),
            "posted again under the same id"
        );
        assert_eq!(
            outbox.delivered(a).map(|(id, _)| id),
            Some(first),
            "what was delivered is the post that went"
        );
        assert_eq!(outbox.texts(a, thread()), ["a2"]);
        assert_eq!(sent(outbox.next(a)), (second, "a2".to_owned()));
        outbox.delivered(a);
        assert!(outbox.texts(a, thread()).is_empty());
        assert!(outbox.next(a).is_none());
        assert_eq!(outbox.agents(), [b]);
        assert_eq!(
            outbox.delivered(b).map(|(_, outgoing)| outgoing.thread),
            Some(None),
            "a delivered new thread says it was one"
        );
        assert!(outbox.delivered(b).is_none(), "nothing more was on its way");
    }
}
