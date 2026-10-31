//! What the user wrote in topics that no host has taken yet.
//!
//! A message goes in here the moment it is written, under a random id the
//! client picks, and onto disk with it. It leaves when its host answers
//! that the message is logged. Until then it is posted again whenever the
//! host comes back: a host logs a `client_id` once, so posting twice is
//! harmless, and words written while the network was down are not lost.
//! A host's messages go one at a time, oldest first, so its chat log holds
//! them in the order they were written.

use std::collections::HashMap;

use rho_agent_types::{AgentId, ContentPart, StreamId, TopicId, UnixMs};

use crate::chat::HostTopic;
use crate::protocol::worksets::{Post, StartTopic};

/// Where a message the user wrote goes.
#[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub enum Destination {
    /// A topic the host already has.
    Topic(TopicId),
    /// A topic this message starts, with the agents that receive it.
    NewTopic {
        stream: StreamId,
        name: String,
        agents: Vec<AgentId>,
    },
}

/// One message on disk until its host takes it.
#[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub struct Outgoing {
    pub at: UnixMs,
    pub content: Vec<ContentPart>,
    pub to: Destination,
}

/// The call that posts a message, logged once under its client id.
#[derive(Clone, Debug, PartialEq)]
pub enum Sending {
    Post(Post),
    StartTopic(StartTopic),
}

impl Outgoing {
    fn call(&self, client_id: u64) -> Sending {
        match &self.to {
            Destination::Topic(topic) => Sending::Post(Post {
                client_id,
                topic: *topic,
                content: self.content.clone(),
            }),
            Destination::NewTopic {
                stream,
                name,
                agents,
            } => Sending::StartTopic(StartTopic {
                client_id,
                stream: *stream,
                name: name.clone(),
                agents: agents.clone(),
                content: self.content.clone(),
            }),
        }
    }
}

#[derive(Default)]
pub struct Outbox {
    /// By the database of the host each goes to; oldest first.
    waiting: Vec<(u64, u64, Outgoing)>,
    /// The id on its way to each host; what is written meanwhile waits
    /// for its answer.
    sending: HashMap<u64, u64>,
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

    /// Keeps a message the user wrote to the host on `machine_seed`, on
    /// disk before anything is sent. Answers its client id.
    pub fn push(&mut self, machine_seed: u64, to: Destination, content: Vec<ContentPart>) -> u64 {
        let id = rand::random();
        let outgoing = Outgoing {
            at: UnixMs::now(),
            content,
            to,
        };
        crate::cache::write_outgoing(machine_seed, id, Some(outgoing.clone()));
        self.waiting.push((machine_seed, id, outgoing));
        id
    }

    /// The host's oldest unsent message, to post now, unless a post is
    /// already on its way.
    pub fn next(&mut self, machine_seed: u64) -> Option<Sending> {
        if self.sending.contains_key(&machine_seed) {
            return None;
        }
        let (_, id, outgoing) = self
            .waiting
            .iter()
            .find(|(host, ..)| *host == machine_seed)?;
        self.sending.insert(machine_seed, *id);
        Some(outgoing.call(*id))
    }

    /// The host logged what was on its way. Answers its client id and
    /// what it was.
    pub fn delivered(&mut self, machine_seed: u64) -> Option<(u64, Outgoing)> {
        let sent = self.sending.remove(&machine_seed)?;
        let at = self
            .waiting
            .iter()
            .position(|(host, id, _)| *host == machine_seed && *id == sent)?;
        let (_, id, outgoing) = self.waiting.remove(at);
        crate::cache::write_outgoing(machine_seed, sent, None);
        Some((id, outgoing))
    }

    /// The post failed; the message waits for the next try.
    pub fn failed(&mut self, machine_seed: u64) {
        self.sending.remove(&machine_seed);
    }

    /// The text of the unsent messages in one topic, oldest first.
    pub fn texts(&self, topic: HostTopic) -> Vec<String> {
        self.waiting
            .iter()
            .filter(|(host, _, outgoing)| {
                *host == topic.machine_seed && outgoing.to == Destination::Topic(topic.topic)
            })
            .map(|(_, _, outgoing)| rho_agent_types::transcript::text_content(&outgoing.content))
            .collect()
    }

    /// Every host database with something unsent.
    pub fn hosts(&self) -> Vec<u64> {
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

    fn topic(n: u64) -> TopicId {
        TopicId(ChatSeq(n))
    }

    fn text(text: &str) -> Vec<ContentPart> {
        vec![ContentPart::Text { text: text.into() }]
    }

    fn sent(post: Option<Sending>) -> (u64, String) {
        match post.expect("a post") {
            Sending::Post(post) => (
                post.client_id,
                rho_agent_types::transcript::text_content(&post.content),
            ),
            Sending::StartTopic(start) => (
                start.client_id,
                rho_agent_types::transcript::text_content(&start.content),
            ),
        }
    }

    /// A host's messages go one at a time, oldest first: a failed post
    /// goes again under the same id, so the host logs it once, and what is
    /// written while one is on its way waits behind it. Another host is
    /// not held up, and a new topic goes as a `StartTopic`.
    #[test]
    fn a_hosts_messages_go_one_at_a_time_in_order() {
        let (a, b) = (1, 2);
        let in_topic = |seed| HostTopic {
            machine_seed: seed,
            topic: topic(4),
        };
        let mut outbox = Outbox::default();
        let first = outbox.push(a, Destination::Topic(topic(4)), text("a1"));
        let agent = AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap();
        outbox.push(
            b,
            Destination::NewTopic {
                stream: rho_agent_types::StreamId(ChatSeq(1)),
                name: "b1".into(),
                agents: vec![agent],
            },
            text("b1"),
        );
        let second = outbox.push(a, Destination::Topic(topic(4)), text("a2"));
        assert_eq!(outbox.hosts(), [a, b]);

        assert_eq!(sent(outbox.next(a)), (first, "a1".to_owned()));
        assert!(outbox.next(a).is_none(), "one post at a time");
        let to_b = outbox.next(b).expect("a post");
        assert!(
            matches!(&to_b, Sending::StartTopic(start) if start.agents == [agent] && start.name == "b1"),
            "a new topic starts it: {to_b:?}"
        );

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
        assert_eq!(outbox.texts(in_topic(a)), ["a2"]);
        assert!(
            outbox.texts(in_topic(b)).is_empty(),
            "the topic of a's host"
        );
        assert_eq!(sent(outbox.next(a)), (second, "a2".to_owned()));
        outbox.delivered(a);
        assert!(outbox.texts(in_topic(a)).is_empty());
        assert!(outbox.next(a).is_none());
        assert_eq!(outbox.hosts(), [b]);
        assert!(matches!(
            outbox.delivered(b),
            Some((
                _,
                Outgoing {
                    to: Destination::NewTopic { .. },
                    ..
                }
            ))
        ));
        assert!(outbox.delivered(b).is_none(), "nothing more was on its way");
    }
}
