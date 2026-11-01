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
    /// A topic this message starts in `stream`.
    NewTopic { stream: StreamId, name: String },
    /// The topic the message under this client id starts, once its host
    /// has said which: a message written after it in the same new topic.
    AfterStart(u64),
}

/// One message on disk until its host takes it.
#[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub struct Outgoing {
    pub at: UnixMs,
    pub content: Vec<ContentPart>,
    /// Agents that receive it without being members of its topic. A
    /// message kept before there were mentions has none.
    #[senax(default)]
    pub mentions: Vec<AgentId>,
    pub to: Destination,
}

/// The call that posts a message, logged once under its client id.
#[derive(Clone, Debug, PartialEq)]
pub enum Sending {
    Post(Post),
    StartTopic(StartTopic),
}

impl Outgoing {
    /// The call, once where it goes is known.
    fn call(&self, client_id: u64) -> Option<Sending> {
        Some(match &self.to {
            Destination::Topic(topic) => Sending::Post(Post {
                client_id,
                topic: *topic,
                mentions: self.mentions.clone(),
                content: self.content.clone(),
            }),
            Destination::NewTopic { stream, name } => Sending::StartTopic(StartTopic {
                client_id,
                stream: *stream,
                name: name.clone(),
                mentions: self.mentions.clone(),
                content: self.content.clone(),
            }),
            Destination::AfterStart(_) => return None,
        })
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
    pub fn push(
        &mut self,
        machine_seed: u64,
        to: Destination,
        mentions: Vec<AgentId>,
        content: Vec<ContentPart>,
    ) -> u64 {
        let id = rand::random();
        let outgoing = Outgoing {
            at: UnixMs::now(),
            content,
            mentions,
            to,
        };
        crate::cache::write_outgoing(machine_seed, id, Some(outgoing.clone()));
        self.waiting.push((machine_seed, id, outgoing));
        id
    }

    /// A message carried from `main`'s outbox, already on disk under its
    /// old id; it waits in the order it was written.
    pub fn adopt(&mut self, machine_seed: u64, id: u64, outgoing: Outgoing) {
        let at = self
            .waiting
            .partition_point(|(_, _, waiting)| waiting.at <= outgoing.at);
        self.waiting.insert(at, (machine_seed, id, outgoing));
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
        // A message after a start waits for it: the start is older, so it
        // has gone and said where by the time this is the oldest.
        let call = outgoing.call(*id)?;
        self.sending.insert(machine_seed, *id);
        Some(call)
    }

    /// The host logged what was on its way, in `topic`. Answers its
    /// client id and what it was. The messages that follow a start now go
    /// to the topic it started.
    pub fn delivered(&mut self, machine_seed: u64, topic: TopicId) -> Option<(u64, Outgoing)> {
        let sent = self.sending.remove(&machine_seed)?;
        let at = self
            .waiting
            .iter()
            .position(|(host, id, _)| *host == machine_seed && *id == sent)?;
        let (_, id, outgoing) = self.waiting.remove(at);
        crate::cache::write_outgoing(machine_seed, sent, None);
        for (host, follower, waiting) in &mut self.waiting {
            if *host == machine_seed && waiting.to == Destination::AfterStart(id) {
                waiting.to = Destination::Topic(topic);
                crate::cache::retarget_outgoing(machine_seed, *follower, waiting.clone());
            }
        }
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
        let agent = AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let first = outbox.push(a, Destination::Topic(topic(4)), vec![agent], text("a1"));
        outbox.push(
            b,
            Destination::NewTopic {
                stream: rho_agent_types::StreamId(ChatSeq(1)),
                name: "b1".into(),
            },
            vec![agent],
            text("b1"),
        );
        let second = outbox.push(a, Destination::Topic(topic(4)), Vec::new(), text("a2"));
        assert_eq!(outbox.hosts(), [a, b]);

        let to_a = outbox.next(a);
        assert!(
            matches!(&to_a, Some(Sending::Post(post)) if post.client_id == first && post.mentions == [agent]),
            "a post mentions whom it mentions: {to_a:?}"
        );
        assert!(outbox.next(a).is_none(), "one post at a time");
        let to_b = outbox.next(b).expect("a post");
        assert!(
            matches!(&to_b, Sending::StartTopic(start) if start.mentions == [agent] && start.name == "b1"),
            "a new topic starts it: {to_b:?}"
        );

        outbox.failed(a);
        assert_eq!(
            sent(outbox.next(a)),
            (first, "a1".to_owned()),
            "posted again under the same id"
        );
        assert_eq!(
            outbox.delivered(a, topic(4)).map(|(id, _)| id),
            Some(first),
            "what was delivered is the post that went"
        );
        assert_eq!(outbox.texts(in_topic(a)), ["a2"]);
        assert!(
            outbox.texts(in_topic(b)).is_empty(),
            "the topic of a's host"
        );
        assert_eq!(sent(outbox.next(a)), (second, "a2".to_owned()));
        outbox.delivered(a, topic(4));
        assert!(outbox.texts(in_topic(a)).is_empty());
        assert!(outbox.next(a).is_none());
        assert_eq!(outbox.hosts(), [b]);
        assert!(matches!(
            outbox.delivered(b, topic(9)),
            Some((
                _,
                Outgoing {
                    to: Destination::NewTopic { .. },
                    ..
                }
            ))
        ));
        assert!(
            outbox.delivered(b, topic(9)).is_none(),
            "nothing more was on its way"
        );
    }

    /// A message after a start waits for it, then goes to the topic the
    /// host says the start made, under its own id.
    #[test]
    fn a_message_after_a_start_follows_it_into_its_topic() {
        let host = 1;
        let agent = AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let mut outbox = Outbox::default();
        outbox.adopt(
            host,
            32,
            Outgoing {
                at: UnixMs(2),
                content: text("second"),
                mentions: vec![agent],
                to: Destination::AfterStart(31),
            },
        );
        outbox.adopt(
            host,
            31,
            Outgoing {
                at: UnixMs(1),
                content: text("first"),
                mentions: vec![agent],
                to: Destination::NewTopic {
                    stream: StreamId(ChatSeq(1)),
                    name: "first".into(),
                },
            },
        );
        let start = outbox.next(host);
        assert!(
            matches!(&start, Some(Sending::StartTopic(start)) if start.client_id == 31),
            "{start:?}"
        );
        outbox.failed(host);
        assert!(
            matches!(outbox.next(host), Some(Sending::StartTopic(_))),
            "a failed start goes again before what follows it"
        );
        outbox.delivered(host, topic(60));
        assert_eq!(
            outbox.next(host),
            Some(Sending::Post(Post {
                client_id: 32,
                topic: topic(60),
                mentions: vec![agent],
                content: text("second"),
            }))
        );
    }

    /// A message kept before messages had mentions reads back with none.
    #[test]
    fn a_message_kept_without_mentions_reads_back() {
        #[derive(senax_encoder::Encode)]
        struct Before {
            at: UnixMs,
            content: Vec<ContentPart>,
            to: Destination,
        }
        let mut bytes = senax_encoder::encode(&Before {
            at: UnixMs(5),
            content: text("kept"),
            to: Destination::Topic(topic(4)),
        })
        .unwrap();
        let read: Outgoing = senax_encoder::decode(&mut bytes).unwrap();
        assert_eq!(
            read,
            Outgoing {
                at: UnixMs(5),
                content: text("kept"),
                mentions: Vec::new(),
                to: Destination::Topic(topic(4)),
            }
        );
    }
}
