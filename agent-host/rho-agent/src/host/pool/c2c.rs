//! C2C ([docs/c2c.md](../../../../../docs/c2c.md)) as this host speaks it:
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

/// One message as C2C carries it. The harness it is from is not in it: the
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
/// docs/c2c.md has it. It is not a reply from the recipient.
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

/// The longest body C2C carries, in bytes of UTF-8.
pub const MAX_BODY_LEN: usize = 64 * 1024;
/// How long a message a peer keeps refusing is tried before the agent is
/// told it bounced: the peer may accept later.
const REFUSED_FOR: Duration = Duration::from_secs(24 * 60 * 60);
/// The wait before a second try; each later one waits twice as long, up
/// to [`MAX_RETRY`].
const FIRST_RETRY: Duration = Duration::from_secs(5);
const MAX_RETRY: Duration = Duration::from_secs(10 * 60);
/// How long one try may take before the harness counts as unreachable.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);

/// A message that did not go through, and when it is tried next.
struct Retry {
    message: MessageId,
    failures: u32,
    at: Instant,
}

fn retry_wait(failures: u32) -> Duration {
    FIRST_RETRY
        .saturating_mul(1 << failures.saturating_sub(1).min(16))
        .min(MAX_RETRY)
}

/// An agent's id as the local id of its address.
fn local_id(agent: AgentId) -> LocalId {
    agent.encoded().parse().expect("an agent id is a local id")
}

impl AgentPool {
    /// Names the host's iroh endpoint: the half of every agent's address
    /// that says where it is. A host without one speaks no C2C.
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

    /// Fails if `content`, posted in `topic`, is more than C2C carries to a
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
    pub async fn c2c_receive(
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
    pub async fn c2c_send(self: &Arc<Self>, transport: Arc<dyn Transport>) {
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
                        self.peer_tried(&mut retries, out, result).await;
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
        out: PeerOutgoing,
        result: anyhow::Result<MessageResult>,
    ) {
        let refused = match result {
            Ok(MessageResult::Delivered) => {
                self.peer_done(&out).await;
                return;
            }
            Ok(MessageResult::Refused) => true,
            Err(_) => false,
        };
        let waited = Duration::from_millis(UnixMs::now().0.saturating_sub(out.at.0));
        if refused && waited >= REFUSED_FOR {
            self.peer_done(&out).await;
            self.peer_notice(&out, Notice::Bounced).await;
            return;
        }
        let retry = retries.entry(out.topic).or_insert(Retry {
            message: out.message,
            failures: 0,
            at: Instant::now(),
        });
        retry.failures += 1;
        retry.at = Instant::now() + retry_wait(retry.failures);
        if retry.failures == 1 {
            let notice = if refused {
                Notice::Refused
            } else {
                Notice::Unreachable
            };
            self.peer_notice(&out, notice).await;
        }
    }

    /// `out` is done with: delivered, or given up on.
    async fn peer_done(&self, out: &PeerOutgoing) {
        let mut write = self.db.write().await;
        write.peer_sent(out.topic, out.message);
        write.commit();
    }

    /// Tells the agent what became of a message it sent a peer. It is told
    /// once that a message waits, and once that one bounced: a message that
    /// goes through says nothing.
    async fn peer_notice(self: &Arc<Self>, out: &PeerOutgoing, notice: Notice) {
        let to = format!("{} ({})", out.name, out.peer);
        let text = match notice {
            Notice::Refused => {
                let own = self
                    .address_of(out.agent)
                    .map_or_else(|_| "your address".to_owned(), |own| own.to_string());
                format!(
                    "[message to {to} not delivered yet]\nIts host refused the message: it has \
                     not accepted your address {own}, or there is no such address. rho keeps \
                     trying for 24 hours; the messages you send it meanwhile wait behind this one."
                )
            }
            Notice::Unreachable => format!(
                "[message to {to} not delivered yet]\nIts host cannot be reached now. rho keeps \
                 trying; the messages you send it meanwhile wait behind this one."
            ),
            Notice::Bounced => format!(
                "[message to {to} bounced]\nIts host refused the message for 24 hours, so rho \
                 dropped it. The message began: {}",
                rho_agent_types::transcript::text_content(&out.content)
                    .chars()
                    .take(200)
                    .collect::<String>()
            ),
        };
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

/// What an agent is told of a message it sent a peer.
enum Notice {
    Refused,
    Unreachable,
    Bounced,
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

    #[test]
    fn retries_wait_longer_up_to_a_limit() {
        let waits: Vec<u64> = [1, 2, 3, 7, 8, 9, 40, u32::MAX]
            .map(|failures| retry_wait(failures).as_secs())
            .to_vec();
        assert_eq!(waits, [5, 10, 20, 320, 600, 600, 600, 600]);
    }
}
