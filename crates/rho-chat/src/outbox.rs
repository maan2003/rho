//! What the user wrote that no host has taken yet.
//!
//! A message goes in here the moment it is written, under a random id the
//! client picks, and onto disk with it. It leaves when its host answers
//! that the message is logged. Until then it is posted again whenever the
//! host comes back: a host logs a `client_id` once, so posting twice is
//! harmless, and words written while the network was down are not lost.
//! A host's messages go one at a time, oldest first, so its chat log holds
//! them in the order they were written.

use std::collections::HashMap;

use rho_agent_types::{AgentId, ContentPart, EndpointId, MessageId, UnixMs};
use senax_encoder::{Decode, Encode};

use crate::protocol::Post;

/// One message on disk until its host takes it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Outgoing {
    pub at: UnixMs,
    /// The thread it goes in; none starts a root.
    pub parent: Option<MessageId>,
    /// Agents that receive it without being members of its thread.
    pub mentions: Vec<AgentId>,
    pub content: Vec<ContentPart>,
}

impl Outgoing {
    fn call(&self, client_id: u64) -> Post {
        Post {
            client_id,
            parent: self.parent,
            mentions: self.mentions.clone(),
            content: self.content.clone(),
        }
    }
}

/// What the outbox did, for the disk copy to follow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kept {
    /// Written, under this id.
    Put(u64, Outgoing),
    /// Taken by its host.
    Gone(u64),
}

#[derive(Default)]
pub struct Outbox {
    /// By the host each goes to; oldest first.
    waiting: Vec<(EndpointId, u64, Outgoing)>,
    /// The id on its way to each host; what is written meanwhile waits
    /// for its answer.
    sending: HashMap<EndpointId, u64>,
}

impl Outbox {
    /// What the last session left unsent, in the order written.
    pub fn load(mut waiting: Vec<(EndpointId, u64, Outgoing)>) -> Self {
        waiting.sort_by_key(|(_, _, outgoing)| outgoing.at);
        Self {
            waiting,
            sending: HashMap::new(),
        }
    }

    /// Takes in what was kept under another name for the host
    /// (`temp-migration`).
    pub fn adopt(&mut self, endpoint: EndpointId, kept: Vec<(u64, Outgoing)>) {
        for (id, outgoing) in kept {
            self.waiting.push((endpoint, id, outgoing));
        }
        self.waiting.sort_by_key(|(_, _, outgoing)| outgoing.at);
    }

    /// Keeps a message the user wrote to the host on `endpoint`.
    /// Answers its client id and what the disk copy must keep.
    pub fn push(
        &mut self,
        endpoint: EndpointId,
        parent: Option<MessageId>,
        mentions: Vec<AgentId>,
        content: Vec<ContentPart>,
    ) -> (u64, Kept) {
        let id = rand::random();
        let outgoing = Outgoing {
            at: UnixMs::now(),
            parent,
            mentions,
            content,
        };
        self.waiting.push((endpoint, id, outgoing.clone()));
        (id, Kept::Put(id, outgoing))
    }

    /// The host's oldest unsent message, to post now, unless a post is
    /// already on its way.
    pub fn next(&mut self, endpoint: EndpointId) -> Option<Post> {
        if self.sending.contains_key(&endpoint) {
            return None;
        }
        let (_, id, outgoing) = self.waiting.iter().find(|(host, ..)| *host == endpoint)?;
        self.sending.insert(endpoint, *id);
        Some(outgoing.call(*id))
    }

    /// The host logged what was on its way. Answers its client id, what it
    /// was, and what the disk copy drops.
    pub fn delivered(&mut self, endpoint: EndpointId) -> Option<(u64, Outgoing, Kept)> {
        let sent = self.sending.remove(&endpoint)?;
        let at = self
            .waiting
            .iter()
            .position(|(host, id, _)| *host == endpoint && *id == sent)?;
        let (_, id, outgoing) = self.waiting.remove(at);
        Some((id, outgoing, Kept::Gone(id)))
    }

    /// The post failed; the message waits for the next try.
    pub fn failed(&mut self, endpoint: EndpointId) {
        self.sending.remove(&endpoint);
    }

    /// The unsent messages under one parent on one host, oldest first.
    pub fn waiting_under(&self, endpoint: EndpointId, parent: Option<MessageId>) -> Vec<&Outgoing> {
        self.waiting
            .iter()
            .filter(|(host, _, outgoing)| *host == endpoint && outgoing.parent == parent)
            .map(|(_, _, outgoing)| outgoing)
            .collect()
    }

    /// Every host with something unsent.
    pub fn hosts(&self) -> Vec<EndpointId> {
        let mut hosts = Vec::new();
        for (host, ..) in &self.waiting {
            if !hosts.contains(host) {
                hosts.push(*host);
            }
        }
        hosts
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::ChatSeq;

    use super::*;

    fn text(text: &str) -> Vec<ContentPart> {
        vec![ContentPart::Text { text: text.into() }]
    }

    fn texts(outgoing: Vec<&Outgoing>) -> Vec<String> {
        outgoing
            .iter()
            .map(|outgoing| rho_agent_types::transcript::text_content(&outgoing.content))
            .collect()
    }

    /// A host's messages go one at a time, oldest first: a failed post
    /// goes again under the same id, so the host logs it once, and what is
    /// written while one is on its way waits behind it. Another host is
    /// not held up.
    #[test]
    fn a_hosts_messages_go_one_at_a_time_in_order() {
        let (a, b) = (EndpointId([1; 32]), EndpointId([2; 32]));
        let thread = Some(MessageId(ChatSeq(4)));
        let mut outbox = Outbox::default();
        let agent = AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let (first, kept) = outbox.push(a, thread, vec![agent], text("a1"));
        assert!(matches!(kept, Kept::Put(id, _) if id == first));
        outbox.push(b, None, vec![agent], text("b1"));
        let (second, _) = outbox.push(a, thread, Vec::new(), text("a2"));
        assert_eq!(outbox.hosts(), [a, b]);

        let to_a = outbox.next(a).expect("a post");
        assert_eq!(
            (to_a.client_id, to_a.mentions.clone(), to_a.parent),
            (first, vec![agent], thread)
        );
        assert!(outbox.next(a).is_none(), "one post at a time");
        let to_b = outbox.next(b).expect("a post");
        assert_eq!((to_b.parent, to_b.mentions), (None, vec![agent]));

        outbox.failed(a);
        assert_eq!(
            outbox.next(a).map(|post| post.client_id),
            Some(first),
            "posted again under the same id"
        );
        let (id, outgoing, kept) = outbox.delivered(a).unwrap();
        assert_eq!((id, kept), (first, Kept::Gone(first)));
        assert_eq!(
            rho_agent_types::transcript::text_content(&outgoing.content),
            "a1"
        );
        assert_eq!(texts(outbox.waiting_under(a, thread)), ["a2"]);
        assert!(
            outbox.waiting_under(b, thread).is_empty(),
            "the thread of a's host"
        );
        assert_eq!(outbox.next(a).map(|post| post.client_id), Some(second));
        outbox.delivered(a);
        assert!(outbox.waiting_under(a, thread).is_empty());
        assert!(outbox.next(a).is_none());
        assert_eq!(outbox.hosts(), [b]);
        assert!(outbox.delivered(b).is_some());
        assert!(outbox.delivered(b).is_none(), "nothing more was on its way");
    }

    #[test]
    fn what_was_left_unsent_goes_in_the_order_written() {
        let agent = AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let outgoing = |at: u64, text: &str| Outgoing {
            at: UnixMs(at),
            parent: None,
            mentions: vec![agent],
            content: vec![ContentPart::Text { text: text.into() }],
        };
        let mut outbox = Outbox::load(vec![
            (EndpointId([1; 32]), 32, outgoing(2, "second")),
            (EndpointId([1; 32]), 31, outgoing(1, "first")),
        ]);
        assert_eq!(
            outbox.next(EndpointId([1; 32])).map(|post| post.client_id),
            Some(31)
        );
        outbox.delivered(EndpointId([1; 32]));
        assert_eq!(
            outbox.next(EndpointId([1; 32])).map(|post| post.client_id),
            Some(32)
        );
    }
}
