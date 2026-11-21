//! What the user wrote that no host has taken yet.
//!
//! A message goes in here the moment it is written, under a random id the
//! client picks, and onto disk with it. It leaves when the host has answered
//! and its log contains the message. Until then it is posted again whenever the
//! host comes back: a host logs a `client_id` once, so posting twice is
//! harmless, and words written while the network was down are not lost.
//! A host's messages go one at a time, oldest first, so its mail log holds
//! them in the order they were written.

use std::collections::HashMap;

use rho_agent_types::{AgentId, ContentPart, EndpointId, MessageId, RuntimeLabel, UnixMs};
use senax_encoder::{Decode, Encode};

use crate::protocol::Post;

/// One message on disk until its host takes it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Outgoing {
    pub at: UnixMs,
    /// The thread it goes in; none starts a root.
    pub parent: Option<MessageId>,
    pub reply_to: Option<MessageId>,
    /// Agents that receive it without being members of its thread.
    pub mentions: Vec<AgentId>,
    pub content: Vec<ContentPart>,
    #[senax(default)]
    pub delivery: Delivery,
    pub runtime: Option<RuntimeLabel>,
    #[senax(default)]
    pub state: OutgoingState,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub enum OutgoingState {
    #[default]
    Queued,
    /// An answer from the host, not a failure to reach it. The words
    /// remain readable in the thread but do not get retried or settle it.
    Refused { reason: String },
}

impl Outgoing {
    fn call(&self, client_id: u64) -> Post {
        Post {
            client_id,
            parent: self.parent,
            reply_to: self.reply_to,
            mentions: self.mentions.clone(),
            content: self.content.clone(),
        }
    }
}

/// Whether the host starts a project agent for this message. Former
/// outbox rows are ordinary posts; they keep exactly their former meaning.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub enum Delivery {
    #[default]
    Mail,
    Idea,
    Retry,
}

/// What the outbox did, for the disk copy to follow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kept {
    /// Written, under this id.
    Put(u64, Outgoing),
    /// Taken by its host.
    Gone(u64),
}

/// A host's current delivery, first in flight, then awaiting its log.
enum Sending {
    Posting(u64),
    Logged { client_id: u64, message: MessageId },
}

#[derive(Default)]
pub struct Outbox {
    /// By the host each goes to; oldest first.
    waiting: Vec<(EndpointId, u64, Outgoing)>,
    /// One delivery per host. Keep the local row until the arrival of the
    /// matching log entry, even if the request itself has already answered.
    sending: HashMap<EndpointId, Sending>,
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

    /// Keeps a message the user wrote to the host on `endpoint`.
    /// Answers its client id and what the disk copy must keep.
    pub fn push(
        &mut self,
        endpoint: EndpointId,
        parent: Option<MessageId>,
        reply_to: Option<MessageId>,
        mentions: Vec<AgentId>,
        content: Vec<ContentPart>,
        delivery: Delivery,
    ) -> (u64, Kept) {
        self.keep(
            endpoint,
            rand::random(),
            Outgoing {
                at: UnixMs::now(),
                parent,
                reply_to,
                mentions,
                content,
                delivery,
                runtime: None,
                state: OutgoingState::Queued,
            },
        )
    }

    pub(crate) fn keep(
        &mut self,
        endpoint: EndpointId,
        id: u64,
        outgoing: Outgoing,
    ) -> (u64, Kept) {
        self.waiting.push((endpoint, id, outgoing.clone()));
        (id, Kept::Put(id, outgoing))
    }

    /// Whether the pending idea still belongs to the outbox.
    pub(crate) fn contains(&self, id: u64) -> bool {
        self.waiting
            .iter()
            .any(|(_, client_id, _)| *client_id == id)
    }

    /// The host's oldest unsent message, to post now, unless a post is
    /// already on its way.
    pub fn next(&mut self, endpoint: EndpointId) -> Option<(Post, Delivery, Option<RuntimeLabel>)> {
        if self.sending.contains_key(&endpoint) {
            return None;
        }
        let (_, id, outgoing) = self.waiting.iter().find(|(host, _, outgoing)| {
            *host == endpoint && outgoing.state == OutgoingState::Queued
        })?;
        self.sending.insert(endpoint, Sending::Posting(*id));
        Some((
            outgoing.call(*id),
            outgoing.delivery,
            outgoing.runtime.clone(),
        ))
    }

    pub(crate) fn posted(&mut self, endpoint: EndpointId, message: MessageId) {
        let sending = self
            .sending
            .get_mut(&endpoint)
            .expect("a post was in flight");
        let Sending::Posting(client_id) = *sending else {
            unreachable!("one receipt per request")
        };
        *sending = Sending::Logged { client_id, message };
    }

    pub(crate) fn receipt(&self, endpoint: EndpointId) -> Option<MessageId> {
        match self.sending.get(&endpoint)? {
            Sending::Logged { message, .. } => Some(*message),
            Sending::Posting(_) => None,
        }
    }

    /// A posted mail is in the local log, or the host refused definitively.
    pub fn answered(&mut self, endpoint: EndpointId) -> Option<(u64, Outgoing, Kept)> {
        let sent = match self.sending.remove(&endpoint)? {
            Sending::Posting(id) | Sending::Logged { client_id: id, .. } => id,
        };
        let at = self
            .waiting
            .iter()
            .position(|(host, id, _)| *host == endpoint && *id == sent)?;
        let (_, id, outgoing) = self.waiting.remove(at);
        Some((id, outgoing, Kept::Gone(id)))
    }

    pub(crate) fn refuse_reply(&mut self, endpoint: EndpointId, reason: String) -> Kept {
        let Some(Sending::Posting(id)) = self.sending.remove(&endpoint) else {
            unreachable!("a refused reply was in flight")
        };
        let (_, _, outgoing) = self
            .waiting
            .iter_mut()
            .find(|(host, client_id, _)| *host == endpoint && *client_id == id)
            .expect("the in-flight reply is still in the outbox");
        outgoing.state = OutgoingState::Refused { reason };
        Kept::Put(id, outgoing.clone())
    }

    pub(crate) fn reply_targets(&self, endpoint: EndpointId) -> impl Iterator<Item = MessageId> {
        self.waiting.iter().filter_map(move |(host, _, outgoing)| {
            if *host != endpoint || outgoing.state != OutgoingState::Queued {
                return None;
            }
            outgoing.reply_to
        })
    }

    /// The post failed; the message waits for the next try.
    pub fn failed(&mut self, endpoint: EndpointId) {
        self.sending.remove(&endpoint);
    }

    /// The still-local mail under one parent on one host, oldest first.
    pub fn local_under(&self, endpoint: EndpointId, parent: Option<MessageId>) -> Vec<&Outgoing> {
        self.waiting
            .iter()
            .filter(|(host, _, outgoing)| *host == endpoint && outgoing.parent == parent)
            .map(|(_, _, outgoing)| outgoing)
            .collect()
    }

    pub(crate) fn ideas(&self) -> impl Iterator<Item = (EndpointId, u64, &Outgoing)> {
        self.waiting
            .iter()
            .filter(|(_, _, outgoing)| outgoing.delivery == Delivery::Idea)
            .map(|(host, id, outgoing)| (*host, *id, outgoing))
    }

    /// Every host with something unsent.
    pub fn hosts(&self) -> Vec<EndpointId> {
        let mut hosts = Vec::new();
        for (host, _, outgoing) in &self.waiting {
            if outgoing.state == OutgoingState::Queued && !hosts.contains(host) {
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
        let (first, kept) = outbox.push(
            a,
            thread,
            Some(MessageId(ChatSeq(7))),
            vec![agent],
            text("a1"),
            Delivery::Mail,
        );
        assert!(matches!(kept, Kept::Put(id, _) if id == first));
        outbox.push(b, None, None, vec![agent], text("b1"), Delivery::Mail);
        let (second, _) = outbox.push(a, thread, None, Vec::new(), text("a2"), Delivery::Mail);
        assert_eq!(outbox.hosts(), [a, b]);

        let to_a = outbox.next(a).expect("a post").0;
        assert_eq!(
            (
                to_a.client_id,
                to_a.mentions.clone(),
                to_a.parent,
                to_a.reply_to
            ),
            (first, vec![agent], thread, Some(MessageId(ChatSeq(7))))
        );
        assert!(outbox.next(a).is_none(), "one post at a time");
        let to_b = outbox.next(b).expect("a post").0;
        assert_eq!((to_b.parent, to_b.mentions), (None, vec![agent]));

        outbox.failed(a);
        assert_eq!(
            outbox
                .next(a)
                .map(|(post, ..)| (post.client_id, post.reply_to)),
            Some((first, Some(MessageId(ChatSeq(7))))),
            "posted again under the same id"
        );
        let (id, outgoing, kept) = outbox.answered(a).unwrap();
        assert_eq!((id, kept), (first, Kept::Gone(first)));
        assert_eq!(
            rho_agent_types::transcript::text_content(&outgoing.content),
            "a1"
        );
        assert_eq!(texts(outbox.local_under(a, thread)), ["a2"]);
        assert!(
            outbox.local_under(b, thread).is_empty(),
            "the thread of a's host"
        );
        assert_eq!(
            outbox.next(a).map(|(post, ..)| post.client_id),
            Some(second)
        );
        outbox.answered(a);
        assert!(outbox.local_under(a, thread).is_empty());
        assert!(outbox.next(a).is_none());
        assert_eq!(outbox.hosts(), [b]);
        assert!(outbox.answered(b).is_some());
        assert!(outbox.answered(b).is_none(), "nothing more was on its way");
    }

    #[test]
    fn mail_queued_before_reply_addresses_keeps_the_original_text() {
        #[derive(Encode)]
        struct OldOutgoing {
            at: UnixMs,
            parent: Option<MessageId>,
            mentions: Vec<AgentId>,
            content: Vec<ContentPart>,
        }
        let content = text("queued offline");
        let bytes = senax_encoder::encode(&OldOutgoing {
            at: UnixMs(9),
            parent: Some(MessageId(ChatSeq(4))),
            mentions: Vec::new(),
            content: content.clone(),
        })
        .unwrap();
        let decoded = senax_encoder::decode::<Outgoing>(&mut bytes.as_ref()).unwrap();
        assert_eq!(decoded.content, content);
        assert_eq!(decoded.parent, Some(MessageId(ChatSeq(4))));
        assert_eq!(decoded.reply_to, None);
        assert_eq!(decoded.delivery, Delivery::Mail);
    }

    #[test]
    fn an_idea_keeps_its_choice_when_resent_and_a_refusal_does_not_block_later_mail() {
        let host = EndpointId([2; 32]);
        let label = RuntimeLabel::parse("sol-high").unwrap();
        let outgoing = Outgoing {
            at: UnixMs(11),
            parent: Some(MessageId(ChatSeq(75))),
            reply_to: None,
            mentions: Vec::new(),
            content: text("explicit choice"),
            delivery: Delivery::Idea,
            runtime: Some(label.clone()),
            state: OutgoingState::Queued,
        };
        let bytes = senax_encoder::encode(&outgoing).unwrap();
        let saved = senax_encoder::decode::<Outgoing>(&mut bytes.as_ref()).unwrap();
        let mut outbox = Outbox::load(vec![(host, 31, saved)]);
        let (next, _) = outbox.push(host, None, None, Vec::new(), text("later"), Delivery::Mail);
        for _ in 0..2 {
            assert_eq!(
                outbox.next(host).map(|(post, delivery, runtime)| (
                    post.client_id,
                    delivery,
                    runtime
                )),
                Some((31, Delivery::Idea, Some(label.clone()))),
            );
            outbox.failed(host); // No definite answer keeps the same request.
        }
        outbox.next(host).unwrap();
        assert_eq!(outbox.answered(host).unwrap().2, Kept::Gone(31));
        assert_eq!(
            outbox.next(host).map(|(post, ..)| post.client_id),
            Some(next)
        );
    }

    #[test]
    fn what_was_left_unsent_goes_in_the_order_written() {
        let agent = AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let outgoing = |at: u64, text: &str| Outgoing {
            at: UnixMs(at),
            parent: None,
            reply_to: None,
            mentions: vec![agent],
            content: vec![ContentPart::Text { text: text.into() }],
            delivery: Delivery::Mail,
            runtime: None,
            state: OutgoingState::Queued,
        };
        let mut outbox = Outbox::load(vec![
            (EndpointId([1; 32]), 32, outgoing(2, "second")),
            (EndpointId([1; 32]), 31, outgoing(1, "first")),
        ]);
        assert_eq!(
            outbox
                .next(EndpointId([1; 32]))
                .map(|(post, ..)| post.client_id),
            Some(31)
        );
        outbox.answered(EndpointId([1; 32]));
        assert_eq!(
            outbox
                .next(EndpointId([1; 32]))
                .map(|(post, ..)| post.client_id),
            Some(32)
        );
    }
}
