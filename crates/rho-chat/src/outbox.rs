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

use rho_agent_types::{ContentPart, EndpointId, MessageId, MessageVersion, RuntimeLabel, UnixMs};
use senax_encoder::{Decode, Encode};

/// One message on disk until its host takes it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Outgoing {
    pub at: UnixMs,
    /// The thread it goes in; none starts a root.
    pub parent: Option<MessageId>,
    /// Previously the address of an ordinary reply or the failed start.
    /// On load, ordinary replies convert this to card settlement.
    #[senax(rename = "reply_to")]
    pub failed_start: Option<MessageId>,
    pub settles: Option<MessageVersion>,
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
    pub(crate) fn import_addressed(
        &mut self,
        version: impl FnOnce(MessageId) -> rho_agent_types::ChatSeq,
    ) {
        if self.delivery != Delivery::Retry
            && let Some(message) = self.failed_start.take()
        {
            self.settles = Some(MessageVersion {
                message,
                version: version(message),
            });
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
    pub(crate) fn next(&mut self, endpoint: EndpointId) -> Option<(u64, Outgoing)> {
        if self.sending.contains_key(&endpoint) {
            return None;
        }
        let (_, id, outgoing) = self.waiting.iter().find(|(host, _, outgoing)| {
            *host == endpoint && outgoing.state == OutgoingState::Queued
        })?;
        self.sending.insert(endpoint, Sending::Posting(*id));
        Some((*id, outgoing.clone()))
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

    /// The host refused the message in flight: a reply, or an idea whose
    /// start it would not make. It stays, marked, and is not posted again.
    pub(crate) fn refuse(&mut self, endpoint: EndpointId, reason: String) -> (u64, Kept) {
        let Some(Sending::Posting(id)) = self.sending.remove(&endpoint) else {
            unreachable!("a refused message was in flight")
        };
        let (_, _, outgoing) = self
            .waiting
            .iter_mut()
            .find(|(host, client_id, _)| *host == endpoint && *client_id == id)
            .expect("the in-flight message is still in the outbox");
        outgoing.state = OutgoingState::Refused { reason };
        (id, Kept::Put(id, outgoing.clone()))
    }

    /// Takes a refused message out, for the user to write it again.
    pub(crate) fn withdraw_refused(&mut self, id: u64) -> Option<(EndpointId, Kept)> {
        let at = self.waiting.iter().position(|(_, client_id, outgoing)| {
            *client_id == id && matches!(outgoing.state, OutgoingState::Refused { .. })
        })?;
        let (host, id, _) = self.waiting.remove(at);
        Some((host, Kept::Gone(id)))
    }

    /// The newest reply into `parent` on `endpoint` that its host refused,
    /// for the user to write it again.
    pub(crate) fn refused_reply(
        &self,
        endpoint: EndpointId,
        parent: MessageId,
    ) -> Option<(u64, &Outgoing)> {
        self.waiting
            .iter()
            .rev()
            .find(|(host, _, outgoing)| {
                *host == endpoint
                    && outgoing.parent == Some(parent)
                    && outgoing.delivery != Delivery::Idea
                    && matches!(outgoing.state, OutgoingState::Refused { .. })
            })
            .map(|(_, id, outgoing)| (*id, outgoing))
    }

    pub(crate) fn settlements(&self, endpoint: EndpointId) -> impl Iterator<Item = MessageVersion> {
        self.waiting.iter().filter_map(move |(host, _, outgoing)| {
            if *host != endpoint || outgoing.state != OutgoingState::Queued {
                return None;
            }
            outgoing.settles.or_else(|| {
                outgoing.failed_start.map(|message| MessageVersion {
                    message,
                    version: message.0,
                })
            })
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

    fn queued(parent: Option<MessageId>, words: &str) -> Outgoing {
        Outgoing {
            at: UnixMs::now(),
            parent,
            failed_start: None,
            settles: None,
            content: text(words),
            delivery: Delivery::Mail,
            runtime: None,
            state: OutgoingState::Queued,
        }
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
        let first = 77;
        let second = 78;
        let settlement = MessageVersion {
            message: MessageId(ChatSeq(7)),
            version: ChatSeq(14),
        };
        let (_, kept) = outbox.keep(
            a,
            first,
            Outgoing {
                settles: Some(settlement),
                ..queued(thread, "a1")
            },
        );
        assert!(matches!(kept, Kept::Put(id, _) if id == first));
        outbox.keep(b, 91, queued(None, "b1"));
        outbox.keep(a, second, queued(thread, "a2"));
        assert_eq!(outbox.hosts(), [a, b]);

        let (id, to_a) = outbox.next(a).expect("a post");
        assert_eq!(
            (id, to_a.parent, to_a.settles),
            (first, thread, Some(settlement))
        );
        assert!(outbox.next(a).is_none(), "one post at a time");
        let (_, to_b) = outbox.next(b).expect("a post");
        assert_eq!((to_b.parent, to_b.settles), (None, None));

        outbox.failed(a);
        assert_eq!(
            outbox.next(a).map(|(id, post)| (id, post.settles)),
            Some((first, Some(settlement))),
            "posted again under the same id with the same card observation"
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
        assert_eq!(outbox.next(a).map(|(id, _)| id), Some(second));
        outbox.answered(a);
        assert!(outbox.local_under(a, thread).is_empty());
        assert!(outbox.next(a).is_none());
        assert_eq!(outbox.hosts(), [b]);
        assert!(outbox.answered(b).is_some());
        assert!(outbox.answered(b).is_none(), "nothing more was on its way");
    }

    #[test]
    fn mail_queued_before_reply_addresses_keeps_the_original_text() {
        use rho_agent_types::AgentId;
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
            mentions: vec![AgentId::from_counter(3, &rho_agent_types::AgentIdDomain(0)).unwrap()],
            content: content.clone(),
        })
        .unwrap();
        let decoded = senax_encoder::decode::<Outgoing>(&mut bytes.as_ref()).unwrap();
        assert_eq!(decoded.content, content);
        assert_eq!(decoded.parent, Some(MessageId(ChatSeq(4))));
        assert_eq!(decoded.failed_start, None);
        assert_eq!(decoded.delivery, Delivery::Mail);
    }

    #[test]
    fn an_old_addressed_post_becomes_a_thread_reply_but_a_retry_keeps_its_failed_start() {
        use rho_agent_types::AgentId;
        #[derive(Encode)]
        struct AddressedOutgoing {
            at: UnixMs,
            parent: Option<MessageId>,
            reply_to: Option<MessageId>,
            mentions: Vec<AgentId>,
            content: Vec<ContentPart>,
            delivery: Delivery,
        }
        let message = MessageId(ChatSeq(4));
        for delivery in [Delivery::Mail, Delivery::Retry] {
            let bytes = senax_encoder::encode(&AddressedOutgoing {
                at: UnixMs(9),
                parent: Some(MessageId(ChatSeq(2))),
                reply_to: Some(message),
                mentions: Vec::new(),
                content: text("queued offline"),
                delivery,
            })
            .unwrap();
            let mut decoded = senax_encoder::decode::<Outgoing>(&mut bytes.as_ref()).unwrap();
            decoded.import_addressed(|id| {
                assert_eq!(id, message);
                ChatSeq(8)
            });
            assert_eq!(decoded.content, text("queued offline"));
            if delivery == Delivery::Retry {
                assert_eq!(
                    (decoded.failed_start, decoded.settles),
                    (Some(message), None)
                );
            } else {
                assert_eq!(
                    (decoded.failed_start, decoded.settles),
                    (
                        None,
                        Some(MessageVersion {
                            message,
                            version: ChatSeq(8)
                        })
                    )
                );
            }
        }
    }

    #[test]
    fn an_idea_keeps_its_choice_when_resent_and_a_refusal_does_not_block_later_mail() {
        let host = EndpointId([2; 32]);
        let label = RuntimeLabel::parse("sol-high").unwrap();
        let outgoing = Outgoing {
            at: UnixMs(11),
            parent: Some(MessageId(ChatSeq(75))),
            failed_start: None,
            settles: None,
            content: text("explicit choice"),
            delivery: Delivery::Idea,
            runtime: Some(label.clone()),
            state: OutgoingState::Queued,
        };
        let bytes = senax_encoder::encode(&outgoing).unwrap();
        let saved = senax_encoder::decode::<Outgoing>(&mut bytes.as_ref()).unwrap();
        let mut outbox = Outbox::load(vec![(host, 31, saved)]);
        let (next, _) = outbox.keep(host, 71, queued(None, "later"));
        for _ in 0..2 {
            assert_eq!(
                outbox
                    .next(host)
                    .map(|(id, outgoing)| (id, outgoing.delivery, outgoing.runtime)),
                Some((31, Delivery::Idea, Some(label.clone()))),
            );
            outbox.failed(host); // No definite answer keeps the same request.
        }
        outbox.next(host).unwrap();
        assert_eq!(outbox.answered(host).unwrap().2, Kept::Gone(31));
        assert_eq!(outbox.next(host).map(|(id, _)| id), Some(next));
    }

    #[test]
    fn what_was_left_unsent_goes_in_the_order_written() {
        let outgoing = |at: u64, text: &str| Outgoing {
            at: UnixMs(at),
            parent: None,
            failed_start: None,
            settles: None,
            content: vec![ContentPart::Text { text: text.into() }],
            delivery: Delivery::Mail,
            runtime: None,
            state: OutgoingState::Queued,
        };
        let mut outbox = Outbox::load(vec![
            (EndpointId([1; 32]), 32, outgoing(2, "second")),
            (EndpointId([1; 32]), 31, outgoing(1, "first")),
        ]);
        assert_eq!(outbox.next(EndpointId([1; 32])).map(|(id, _)| id), Some(31));
        outbox.answered(EndpointId([1; 32]));
        assert_eq!(outbox.next(EndpointId([1; 32])).map(|(id, _)| id), Some(32));
    }
}
