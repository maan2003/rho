//! GCCP ([docs/gccp.md](../../../../../docs/gccp.md)) as this host speaks it:
//! what its agents accept, what it takes in for them, and what it sends
//! out for them.
//!
//! A peer conversation in the chat log is both mailboxes. What the peer
//! sent is a message in it from the peer, handed to the agent like any
//! other; what the agent posts in it is the outbox, sent in order. The
//! wire is whoever carries it ([`Transport`]): this module never sees a
//! connection.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use rho_agent_types::{
    AgentId, ContentPart, LocalId, MessageId, PeerAddress, Role, TopicId, UnixMs,
};

use super::AgentPool;
use crate::ipc::protocol::Control;
use crate::store::{
    Appended, ChatRead as _, ChatWrite as _, PeerContact, PeerOutgoing, StoreRead as _,
};

/// One message as GCCP carries it. The harness it is from is not in it: the
/// transport proves that.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
    pub to: LocalId,
    pub from: LocalId,
    /// The sender's own id for the message.
    pub id: u64,
    pub body: String,
}

/// What a harness says of a message it was sent: its result, as
/// docs/gccp.md has it. It is not a reply from the recipient.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageResult {
    /// Stored for its recipient.
    Delivered,
    /// Not taken: no such recipient, or it has not accepted the sender. A
    /// harness does not say which.
    Refused,
}

/// Carries messages to other harnesses.
pub trait Transport: Send + Sync + 'static {
    /// Takes one message to the harness at `endpoint` and brings back its
    /// result. An error is a harness that could not be reached or gave
    /// none; the message may have arrived all the same.
    fn send<'a>(
        &'a self,
        endpoint: [u8; 32],
        envelope: &'a Envelope,
    ) -> BoxFuture<'a, anyhow::Result<MessageResult>>;
}

/// The longest body GCCP carries, in bytes of UTF-8.
pub const MAX_BODY_LEN: usize = 64 * 1024;
/// The waits before the second and the third try at a message whose
/// harness could not be reached. After the last the message is given up on:
/// an agent that is told so decides better than a queue when to send again.
pub const RETRY_WAITS: [Duration; 2] = [Duration::from_secs(60), Duration::from_secs(120)];
/// How long one try may take before the harness counts as unreachable.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// A message whose harness could not be reached, and when it is tried next.
struct Retry {
    message: MessageId,
    failures: usize,
    at: Instant,
}

/// An agent's id as the local id of its address.
fn local_id(agent: AgentId) -> LocalId {
    agent.encoded().parse().expect("an agent id is a local id")
}

impl AgentPool {
    /// Names the host's iroh endpoint: the half of every agent's address
    /// that says where it is. A host without one speaks no GCCP.
    pub fn set_endpoint(&self, endpoint: [u8; 32]) {
        self.endpoint
            .set(endpoint)
            .expect("the host's endpoint is named once");
    }

    fn endpoint(&self) -> anyhow::Result<[u8; 32]> {
        self.endpoint.get().copied().context(
            "this host has no iroh endpoint, so its agents have no address: \
             start rho-agent-host with --iroh",
        )
    }

    /// The address other harnesses reach `agent` at.
    pub(super) fn address_of(&self, agent: AgentId) -> anyhow::Result<PeerAddress> {
        Ok(PeerAddress {
            endpoint: self.endpoint()?,
            local: local_id(agent),
        })
    }

    /// `agent` accepts `peer` under `name`; answers with their conversation.
    pub(super) async fn accept_peer(
        &self,
        agent: AgentId,
        peer: &str,
        name: String,
    ) -> anyhow::Result<TopicId> {
        let peer: PeerAddress = peer.parse().map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            peer.endpoint != self.endpoint()?,
            "{peer} is on this host: message its agent by id"
        );
        let mut write = self.db.write().await;
        let topic = write.accept_peer(UnixMs::now(), agent, peer, name)?;
        write.commit();
        Ok(topic)
    }

    /// The conversation of `agent` and an address it accepted.
    pub(super) fn peer_topic(&self, agent: AgentId, peer: &str) -> anyhow::Result<TopicId> {
        let peer: PeerAddress = peer.parse().map_err(anyhow::Error::msg)?;
        self.db
            .read()
            .peer_topic(agent, &peer)
            .with_context(|| format!("{peer} is not accepted: accept it under a name first"))
    }

    /// Fails if `content`, posted in `topic`, is more than GCCP carries to a
    /// peer: such a message would wait forever.
    pub(super) fn check_peer_body(
        &self,
        topic: TopicId,
        content: &[ContentPart],
    ) -> anyhow::Result<()> {
        let to_peer = matches!(
            self.db.read().topic(topic).map(|head| head.place),
            Some(crate::store::Place::Peer { .. })
        );
        let len = rho_agent_types::transcript::text_content(content).len();
        anyhow::ensure!(
            !to_peer || len <= MAX_BODY_LEN,
            "a message to another host is at most {MAX_BODY_LEN} bytes, and this is {len}"
        );
        Ok(())
    }

    /// Every address an agent of the host has accepted.
    pub fn peers(&self) -> Vec<PeerContact> {
        self.db.read().peers()
    }

    /// Takes back `agent`'s acceptance of `peer`: nothing more crosses
    /// between them, and what waited to be sent is dropped.
    pub async fn remove_peer(&self, agent: AgentId, peer: &PeerAddress) -> anyhow::Result<()> {
        let topic = self
            .db
            .read()
            .peer_topic(agent, peer)
            .with_context(|| format!("{} has not accepted {peer}", agent.encoded()))?;
        let mut write = self.db.write().await;
        write.remove_peer(UnixMs::now(), topic)?;
        write.commit();
        Ok(())
    }

    /// A message the harness at `endpoint` sent. It is delivered only to an
    /// agent that accepted the sender's address; anything else is refused
    /// and leaves no trace.
    pub async fn gccp_receive(
        self: &Arc<Self>,
        endpoint: [u8; 32],
        envelope: Envelope,
    ) -> MessageResult {
        let Envelope { to, from, id, body } = envelope;
        let Ok(agent) = AgentId::from_encoded(to.as_str()) else {
            return MessageResult::Refused;
        };
        let peer = PeerAddress {
            endpoint,
            local: from,
        };
        // A stranger's message costs a read and no more.
        let read = self.db.read();
        let (Some(_), Some(topic)) = (read.agent(agent), read.peer_topic(agent, &peer)) else {
            return MessageResult::Refused;
        };
        drop(read);
        let content = vec![ContentPart::Text { text: body }];
        let mut write = self.db.write().await;
        // The write refuses a conversation closed since the read.
        match write.receive_peer(UnixMs::now(), topic, id, content) {
            Ok(Some(message)) => {
                write.commit();
                self.wake_receivers(message, None);
                MessageResult::Delivered
            }
            // Its newest again: the result of the first was lost.
            Ok(None) => MessageResult::Delivered,
            Err(_) => MessageResult::Refused,
        }
    }

    /// Sends what agents post in their peer conversations, for as long as
    /// the host runs: each conversation's messages in order, the next only
    /// once the peer's harness took the one before. Conversations do not
    /// wait for each other.
    ///
    /// A message its harness refuses is given up on at once. One whose
    /// harness cannot be reached is tried again after each of `retry_waits`
    /// and then given up on. Giving up drops the message and all that wait
    /// behind it, which were written on top of it, and tells the agent: it
    /// sends again if and when it wants to.
    pub async fn gccp_send(
        self: &Arc<Self>,
        transport: Arc<dyn Transport>,
        retry_waits: &'static [Duration],
    ) {
        let mut feed = crate::store::feed(&self.db);
        let mut flying = futures::stream::FuturesUnordered::new();
        let mut busy: HashSet<TopicId> = HashSet::new();
        let mut retries: HashMap<TopicId, Retry> = HashMap::new();
        loop {
            let outgoing = self.db.read().peer_outgoing();
            // A retry is of the message at the head of its conversation.
            retries.retain(|topic, retry| {
                outgoing
                    .iter()
                    .any(|out| out.topic == *topic && out.message == retry.message)
            });
            let now = Instant::now();
            for out in outgoing {
                let waits = retries.get(&out.topic).is_some_and(|retry| retry.at > now);
                if waits || !busy.insert(out.topic) {
                    continue;
                }
                let transport = transport.clone();
                flying.push(async move {
                    let envelope = Envelope {
                        to: out.peer.local.clone(),
                        from: local_id(out.agent),
                        id: out.message.0.0,
                        body: rho_agent_types::transcript::text_content(&out.content),
                    };
                    let send = transport.send(out.peer.endpoint, &envelope);
                    let result = match tokio::time::timeout(SEND_TIMEOUT, send).await {
                        Ok(result) => result,
                        Err(_) => Err(anyhow::anyhow!("no result in {SEND_TIMEOUT:?}")),
                    };
                    (out, result)
                });
            }
            let next_retry = retries.values().map(|retry| retry.at).min();
            let retry_due = async {
                match next_retry {
                    Some(at) => tokio::time::sleep_until(at.into()).await,
                    None => std::future::pending().await,
                }
            };
            tokio::pin!(retry_due);
            // Until something changes what there is to send.
            loop {
                tokio::select! {
                    Some((out, result)) = flying.next() => {
                        busy.remove(&out.topic);
                        self.peer_tried(&mut retries, retry_waits, out, result).await;
                        break;
                    }
                    appended = feed.recv() => match appended {
                        Ok(Appended::Chat(_))
                        | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    },
                    () = &mut retry_due => break,
                }
            }
        }
    }

    /// One try at one message ended.
    async fn peer_tried(
        self: &Arc<Self>,
        retries: &mut HashMap<TopicId, Retry>,
        retry_waits: &[Duration],
        out: PeerOutgoing,
        result: anyhow::Result<MessageResult>,
    ) {
        let failure = match result {
            Ok(MessageResult::Delivered) => {
                let mut write = self.db.write().await;
                write.peer_sent(out.topic, out.message);
                write.commit();
                return;
            }
            Ok(MessageResult::Refused) => Failure::Refused,
            Err(_) => {
                let retry = retries.entry(out.topic).or_insert(Retry {
                    message: out.message,
                    failures: 0,
                    at: Instant::now(),
                });
                retry.failures += 1;
                if let Some(wait) = retry_waits.get(retry.failures - 1) {
                    retry.at = Instant::now() + *wait;
                    return;
                }
                Failure::Unreachable {
                    tries: retry.failures,
                }
            }
        };
        self.peer_give_up(out, failure).await;
    }

    /// Drops `out` and what waits behind it, and tells the agent why.
    async fn peer_give_up(self: &Arc<Self>, out: PeerOutgoing, failure: Failure) {
        // What waits now, not what did when the try began.
        let waiting = self.db.read().peer_outgoing();
        let Some(out) = waiting
            .into_iter()
            .find(|waits| waits.topic == out.topic && waits.message == out.message)
        else {
            // Its acceptance was taken back meanwhile, and its messages with
            // it.
            return;
        };
        {
            let mut write = self.db.write().await;
            write.peer_sent(out.topic, out.newest);
            write.commit();
        }
        let to = format!("{} ({})", out.name, out.peer);
        let why = match failure {
            Failure::Refused => {
                let own = self
                    .address_of(out.agent)
                    .map_or_else(|_| "your address".to_owned(), |own| own.to_string());
                format!(
                    "[message to {to} refused]\nIts host did not take the message: it has not \
                     accepted your address {own}, or there is no such address."
                )
            }
            Failure::Unreachable { tries } => format!(
                "[message to {to} not delivered]\nIts host could not be reached, or gave no \
                 result, in {tries} tries."
            ),
        };
        let began: String = rho_agent_types::transcript::text_content(&out.content)
            .chars()
            .take(200)
            .collect();
        let dropped = match out.waiting - 1 {
            0 => "The message was dropped".to_owned(),
            1 => "It and the message you sent after it were dropped".to_owned(),
            after => format!("It and the {after} messages you sent after it were dropped"),
        };
        let text = format!(
            "{why} {dropped}: nothing is tried again. Send again if and when it is worth it. \
             The message began: {began}"
        );
        let input = Control::Input {
            content: vec![ContentPart::Text { text }],
            role: Role::Tool,
            message: None,
        };
        let told = async {
            let (_, client, _) = self.load(out.agent).await?;
            client.control(input).await
        };
        if let Err(error) = told.await {
            eprintln!(
                "rho-agent: telling {} about its message to {}: {error:#}",
                out.agent.encoded(),
                out.peer
            );
        }
    }
}

/// Why a message is given up on.
enum Failure {
    /// Its harness did not take it.
    Refused,
    /// Its harness was not reached, or gave no result, this many times.
    Unreachable { tries: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every agent has an address, and the address names no other agent.
    #[test]
    fn an_agent_id_is_a_local_id() {
        for domain in [0, u64::MAX] {
            for counter in [0, 1, 35, 36, u64::from(u32::MAX)] {
                let Some(agent) =
                    AgentId::from_counter(counter, &rho_agent_types::AgentIdDomain(domain))
                else {
                    continue;
                };
                let local = local_id(agent);
                assert_eq!(AgentId::from_encoded(local.as_str()).ok(), Some(agent));
            }
        }
    }
}
