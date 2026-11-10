//! The host's chat ([`rho_agent_types::ChatEvent`]): one log, dense from
//! one, and the indexes folded from it in the commit of every append.
//!
//! Each conversation keeps its members; a message is in the inbox of every
//! member and every agent it mentions, but not its sender's. The inbox is
//! written with the message, so an agent's delivery reads only its own.

use redb::TableDefinition;
use rho_agent_types::{
    Address, AgentId, ChatEvent, ChatRecord, ChatSeq, ContentPart, Mention, Message, MessageId,
    PeerAddress, StreamId, TopicId, UnixMs,
};
use rho_db::{ReadTxn, Sen, SenValue, WriteTxn};
use senax_encoder::{Decode, Encode};

use super::{Appended, tell};

const CHAT_LOG: TableDefinition<u64, Sen<ChatRecord>> = TableDefinition::new("chat_log");
const STREAMS: TableDefinition<u64, String> = TableDefinition::new("chat_streams");
const STREAM_NAMES: TableDefinition<String, u64> = TableDefinition::new("chat_stream_names");
const TOPICS: TableDefinition<u64, Sen<TopicHead>> = TableDefinition::new("chat_topics");
/// `(topic, message)`: a topic's messages in order.
const TOPIC_MESSAGES: TableDefinition<(u64, u64), ()> = TableDefinition::new("chat_topic_messages");
/// `(agent, message)`: what each agent receives, in order.
const INBOX: TableDefinition<(AgentId, u64), ()> = TableDefinition::new("chat_inbox");
/// The user's posts by the client's own id for them, so a resend is
/// logged once: the seq of the event the post made.
const CLIENT_IDS: TableDefinition<u64, u64> = TableDefinition::new("chat_client_ids");
/// `(a, b)` with `a < b`: the direct conversation of two agents.
const DIRECTS: TableDefinition<(AgentId, AgentId), u64> = TableDefinition::new("chat_directs");
/// A moved message's topic now. A message never moved is in the topic its
/// `Posted` names.
const MOVED: TableDefinition<u64, u64> = TableDefinition::new("chat_moved");
/// `(agent, address)`: the open peer conversation of an agent and an
/// address it accepted.
const PEERS: TableDefinition<(AgentId, String), u64> = TableDefinition::new("chat_peers");
/// By peer conversation: the newest of the agent's messages in it that is
/// done with, delivered to the peer or given up on. The ones after it are
/// still to send, so the log is the outbox.
const PEER_SENT: TableDefinition<u64, u64> = TableDefinition::new("chat_peer_sent");

/// Where a conversation is.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Place {
    /// A topic: anyone may post in it, and agents join and leave it.
    Stream {
        stream: StreamId,
        name: String,
        resolved: bool,
    },
    /// Two agents' own conversation: only they post in it.
    Direct,
    /// An agent's conversation with an address it accepted, under the name
    /// it gave it: only the two post in it. Once it is not `open`, the
    /// acceptance was taken back and no one does.
    Peer {
        peer: PeerAddress,
        name: String,
        open: bool,
    },
}

impl Place {
    /// What only a topic in a stream allows, refused with `what` for any
    /// other conversation.
    fn stream_only(&self, direct: &str, peer: &str) -> anyhow::Result<()> {
        match self {
            Self::Stream { .. } => Ok(()),
            Self::Direct => anyhow::bail!("{direct}"),
            Self::Peer { .. } => anyhow::bail!("{peer}"),
        }
    }
}

/// An agent's message a peer has not taken yet: the oldest of its
/// conversation, since a peer is sent them in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PeerOutgoing {
    pub topic: TopicId,
    pub message: MessageId,
    pub agent: AgentId,
    pub peer: PeerAddress,
    pub name: String,
    pub content: Vec<ContentPart>,
    /// The newest of the agent's messages that wait with this one, which is
    /// this one when none do.
    pub newest: MessageId,
    /// How many wait, this one among them.
    pub waiting: u64,
}

/// An open peer conversation.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct PeerContact {
    pub topic: TopicId,
    pub agent: AgentId,
    pub peer: PeerAddress,
    pub name: String,
    /// How many of the agent's messages the peer has not taken.
    pub unsent: u64,
}

/// A conversation as the log has made it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct TopicHead {
    pub place: Place,
    /// Who receives its messages: a topic's in the order they joined, a
    /// direct conversation's two.
    pub agents: Vec<AgentId>,
}

/// A message as its sender gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewMessage {
    pub from: Address,
    pub mentions: Vec<Mention>,
    pub content: Vec<ContentPart>,
}

pub trait ChatRead {
    /// The newest event; zero for none.
    fn chat_head(&self) -> ChatSeq;
    /// Up to `limit` events after `after`, in order.
    fn chat_since(&self, after: ChatSeq, limit: usize) -> Vec<ChatRecord>;
    fn message(&self, id: MessageId) -> Option<Message>;
    fn topic(&self, id: TopicId) -> Option<TopicHead>;
    fn stream_name(&self, id: StreamId) -> Option<String>;
    fn stream_named(&self, name: &str) -> Option<StreamId>;
    /// What `agent` receives after `after`, in order.
    fn inbox(&self, agent: AgentId, after: ChatSeq) -> Vec<Message>;
    /// A topic's `newest` latest messages, oldest first.
    fn topic_messages(&self, topic: TopicId, newest: usize) -> Vec<Message>;
    /// The open peer conversation of `agent` and `peer`.
    fn peer_topic(&self, agent: AgentId, peer: &PeerAddress) -> Option<TopicId>;
    /// Every open peer conversation, an agent's together.
    fn peers(&self) -> Vec<PeerContact>;
    /// The next message to send in each open peer conversation that has one.
    fn peer_outgoing(&self) -> Vec<PeerOutgoing>;
}

impl ChatRead for ReadTxn {
    fn chat_head(&self) -> ChatSeq {
        self.open_table(CHAT_LOG)
            .iter()
            .next_back()
            .map_or(ChatSeq::ZERO, |(key, _)| ChatSeq(key.value()))
    }

    fn chat_since(&self, after: ChatSeq, limit: usize) -> Vec<ChatRecord> {
        self.open_table(CHAT_LOG)
            .range(after.0 + 1..)
            .take(limit)
            .map(|(_, record)| record.value().into_owned())
            .collect()
    }

    fn message(&self, id: MessageId) -> Option<Message> {
        let record = self.open_table(CHAT_LOG).get(id.0.0)?.value().into_owned();
        let moved = self
            .open_table(MOVED)
            .get(id.0.0)
            .map(|topic| TopicId(ChatSeq(topic.value())));
        where_now(Message::from_record(&record)?, moved)
    }

    fn topic(&self, id: TopicId) -> Option<TopicHead> {
        self.open_table(TOPICS)
            .get(id.0.0)
            .map(|head| head.value().into_owned())
    }

    fn stream_name(&self, id: StreamId) -> Option<String> {
        self.open_table(STREAMS)
            .get(id.0.0)
            .map(|name| name.value())
    }

    fn stream_named(&self, name: &str) -> Option<StreamId> {
        self.open_table(STREAM_NAMES)
            .get(name.to_owned())
            .map(|id| StreamId(ChatSeq(id.value())))
    }

    fn inbox(&self, agent: AgentId, after: ChatSeq) -> Vec<Message> {
        let seqs: Vec<u64> = self
            .open_table(INBOX)
            .range((agent, after.0 + 1)..=(agent, u64::MAX))
            .map(|(key, _)| key.value().1)
            .collect();
        seqs.into_iter()
            .map(|seq| {
                self.message(MessageId(ChatSeq(seq)))
                    .expect("an inbox names a message")
            })
            .collect()
    }

    fn topic_messages(&self, topic: TopicId, newest: usize) -> Vec<Message> {
        let mut seqs: Vec<u64> = self
            .open_table(TOPIC_MESSAGES)
            .range((topic.0.0, 0)..=(topic.0.0, u64::MAX))
            .rev()
            .take(newest)
            .map(|(key, _)| key.value().1)
            .collect();
        seqs.reverse();
        seqs.into_iter()
            .map(|seq| {
                self.message(MessageId(ChatSeq(seq)))
                    .expect("a topic names its messages")
            })
            .collect()
    }

    fn peer_topic(&self, agent: AgentId, peer: &PeerAddress) -> Option<TopicId> {
        self.open_table(PEERS)
            .get((agent, peer.to_string()))
            .map(|seq| TopicId(ChatSeq(seq.value())))
    }

    fn peers(&self) -> Vec<PeerContact> {
        let topics: Vec<(AgentId, u64)> = self
            .open_table(PEERS)
            .iter()
            .map(|(key, seq)| (key.value().0, seq.value()))
            .collect();
        topics
            .into_iter()
            .map(|(agent, seq)| {
                let topic = TopicId(ChatSeq(seq));
                let Some(Place::Peer { peer, name, .. }) = self.topic(topic).map(|head| head.place)
                else {
                    panic!("a peer index names a peer conversation");
                };
                PeerContact {
                    topic,
                    agent,
                    peer,
                    name,
                    unsent: self.peer_unsent(topic, agent).count() as u64,
                }
            })
            .collect()
    }

    fn peer_outgoing(&self) -> Vec<PeerOutgoing> {
        self.peers()
            .into_iter()
            .filter_map(|contact| {
                let mut unsent = self.peer_unsent(contact.topic, contact.agent);
                let message = unsent.next()?;
                let (newest, after) =
                    unsent.fold((message.id, 0), |(_, after), later| (later.id, after + 1));
                Some(PeerOutgoing {
                    topic: contact.topic,
                    message: message.id,
                    agent: contact.agent,
                    peer: contact.peer,
                    name: contact.name,
                    content: message.content,
                    newest,
                    waiting: after + 1,
                })
            })
            .collect()
    }
}

trait PeerRead {
    /// The agent's messages in its peer conversation after the last one
    /// done with, oldest first.
    fn peer_unsent(&self, topic: TopicId, agent: AgentId) -> impl Iterator<Item = Message>;
}

impl PeerRead for ReadTxn {
    fn peer_unsent(&self, topic: TopicId, agent: AgentId) -> impl Iterator<Item = Message> {
        let sent = self
            .open_table(PEER_SENT)
            .get(topic.0.0)
            .map_or(0, |seq| seq.value());
        let seqs: Vec<u64> = self
            .open_table(TOPIC_MESSAGES)
            .range((topic.0.0, sent + 1)..=(topic.0.0, u64::MAX))
            .map(|(key, _)| key.value().1)
            .collect();
        seqs.into_iter()
            .map(|seq| {
                self.message(MessageId(ChatSeq(seq)))
                    .expect("a topic names its messages")
            })
            .filter(move |message| message.from == Address::Agent(agent))
    }
}

fn where_now(mut message: Message, moved: Option<TopicId>) -> Option<Message> {
    if let Some(topic) = moved {
        message.topic = topic;
    }
    Some(message)
}

/// Where messages move to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MoveTo {
    Topic(TopicId),
    NewTopic { stream: StreamId, name: String },
}

pub trait ChatWrite {
    fn init_chat_tables(&mut self);
    /// The stream named `name`, created if there is none.
    fn stream(&mut self, at: UnixMs, name: String) -> anyhow::Result<StreamId>;
    /// A topic with its first message, and no members. With `client_id`, a
    /// topic already started under that id is returned instead of a second
    /// one.
    fn start_topic(
        &mut self,
        at: UnixMs,
        stream: StreamId,
        name: String,
        first: NewMessage,
        client_id: Option<u64>,
    ) -> anyhow::Result<(TopicId, MessageId)>;
    /// A topic with no message yet: the start of [`ChatWrite::start_topic`],
    /// and the new topic of a move.
    fn open_topic(
        &mut self,
        at: UnixMs,
        stream: StreamId,
        name: String,
        by: Address,
    ) -> anyhow::Result<TopicId>;
    /// The direct conversation of `a` and `b`, started if they have none.
    fn direct(&mut self, at: UnixMs, a: AgentId, b: AgentId) -> anyhow::Result<TopicId>;
    fn join(&mut self, at: UnixMs, topic: TopicId, agent: AgentId) -> anyhow::Result<()>;
    fn leave(&mut self, at: UnixMs, topic: TopicId, agent: AgentId) -> anyhow::Result<()>;
    /// Gives a topic its whole head: a name, a stream, and whether it is
    /// resolved. A change to nothing logs nothing.
    fn change_topic(
        &mut self,
        at: UnixMs,
        topic: TopicId,
        stream: StreamId,
        name: String,
        resolved: bool,
    ) -> anyhow::Result<()>;
    /// Moves messages of topics to `to`; returns the topic they are in.
    /// With `client_id`, a move already made under it is returned instead.
    fn move_messages(
        &mut self,
        at: UnixMs,
        messages: Vec<MessageId>,
        to: MoveTo,
        by: Address,
        client_id: Option<u64>,
    ) -> anyhow::Result<TopicId>;
    /// With `client_id`, as in [`ChatWrite::start_topic`].
    fn post(
        &mut self,
        at: UnixMs,
        topic: TopicId,
        message: NewMessage,
        client_id: Option<u64>,
    ) -> anyhow::Result<MessageId>;
    /// `agent` accepts `peer` under `name`: their conversation, started if
    /// they have none open. One already open keeps its name, so a second
    /// name for it is refused.
    fn accept_peer(
        &mut self,
        at: UnixMs,
        agent: AgentId,
        peer: PeerAddress,
        name: String,
    ) -> anyhow::Result<TopicId>;
    /// Takes back the acceptance behind an open peer conversation.
    fn remove_peer(&mut self, at: UnixMs, topic: TopicId) -> anyhow::Result<()>;
    /// The agent's messages in `topic` up to `message` are done with:
    /// delivered, or given up on and never sent.
    fn peer_sent(&mut self, topic: TopicId, message: MessageId);
    fn chat_topic(&mut self, id: TopicId) -> Option<TopicHead>;
    fn chat_stream_named(&mut self, name: &str) -> Option<StreamId>;
}

fn next_seq(write: &mut WriteTxn) -> ChatSeq {
    write
        .open_table(CHAT_LOG)
        .iter()
        .next_back()
        .map_or(ChatSeq(1), |(key, _)| ChatSeq(key.value()).next())
}

fn append(write: &mut WriteTxn, at: UnixMs, event: ChatEvent) -> ChatSeq {
    let seq = next_seq(write);
    write
        .open_table(CHAT_LOG)
        .insert(seq.0, SenValue::borrowed(&ChatRecord { seq, at, event }));
    tell(write, Appended::Chat(seq));
    seq
}

fn has_content(content: &[ContentPart]) -> bool {
    content.iter().any(|part| match part {
        ContentPart::Text { text } => !text.trim().is_empty(),
        _ => true,
    })
}

fn sent_before(write: &mut WriteTxn, client_id: Option<u64>) -> Option<ChatSeq> {
    let client_id = client_id?;
    write
        .open_table(CLIENT_IDS)
        .get(client_id)
        .map(|seq| ChatSeq(seq.value()))
}

fn remember(write: &mut WriteTxn, client_id: Option<u64>, seq: ChatSeq) {
    if let Some(client_id) = client_id {
        write.open_table(CLIENT_IDS).insert(client_id, seq.0);
    }
}

impl ChatWrite for WriteTxn {
    fn init_chat_tables(&mut self) {
        self.open_table(CHAT_LOG);
        self.open_table(STREAMS);
        self.open_table(STREAM_NAMES);
        self.open_table(TOPICS);
        self.open_table(TOPIC_MESSAGES);
        self.open_table(INBOX);
        self.open_table(CLIENT_IDS);
        self.open_table(DIRECTS);
        self.open_table(MOVED);
        self.open_table(PEERS);
        self.open_table(PEER_SENT);
    }

    fn stream(&mut self, at: UnixMs, name: String) -> anyhow::Result<StreamId> {
        let name = name.trim().to_owned();
        anyhow::ensure!(!name.is_empty(), "a stream needs a name");
        if let Some(stream) = self.chat_stream_named(&name) {
            return Ok(stream);
        }
        let seq = append(self, at, ChatEvent::StreamCreated { name: name.clone() });
        self.open_table(STREAMS).insert(seq.0, name.clone());
        self.open_table(STREAM_NAMES).insert(name, seq.0);
        Ok(StreamId(seq))
    }

    fn start_topic(
        &mut self,
        at: UnixMs,
        stream: StreamId,
        name: String,
        first: NewMessage,
        client_id: Option<u64>,
    ) -> anyhow::Result<(TopicId, MessageId)> {
        if let Some(seq) = sent_before(self, client_id) {
            // A client id names a message.
            let record = self
                .open_table(CHAT_LOG)
                .get(seq.0)
                .expect("a client id names a logged message")
                .value()
                .into_owned();
            let message = Message::from_record(&record).expect("a client id names a message");
            let moved = self
                .open_table(MOVED)
                .get(message.id.0.0)
                .map(|topic| TopicId(ChatSeq(topic.value())));
            return Ok((moved.unwrap_or(message.topic), message.id));
        }
        anyhow::ensure!(has_content(&first.content), "a message needs content");
        let topic = self.open_topic(at, stream, name, first.from)?;
        let first = self.post(at, topic, first, client_id)?;
        Ok((topic, first))
    }

    fn open_topic(
        &mut self,
        at: UnixMs,
        stream: StreamId,
        name: String,
        by: Address,
    ) -> anyhow::Result<TopicId> {
        anyhow::ensure!(
            self.open_table(STREAMS).get(stream.0.0).is_some(),
            "no stream {}",
            stream.0.0
        );
        anyhow::ensure!(!name.trim().is_empty(), "a topic needs a name");
        let seq = append(
            self,
            at,
            ChatEvent::TopicStarted {
                stream,
                name: name.clone(),
                by,
            },
        );
        self.open_table(TOPICS).insert(
            seq.0,
            SenValue::borrowed(&TopicHead {
                place: Place::Stream {
                    stream,
                    name,
                    resolved: false,
                },
                agents: Vec::new(),
            }),
        );
        Ok(TopicId(seq))
    }

    fn direct(&mut self, at: UnixMs, a: AgentId, b: AgentId) -> anyhow::Result<TopicId> {
        anyhow::ensure!(a != b, "an agent has no direct conversation with itself");
        let members = if a < b { [a, b] } else { [b, a] };
        if let Some(seq) = self.open_table(DIRECTS).get((members[0], members[1])) {
            return Ok(TopicId(ChatSeq(seq.value())));
        }
        let seq = append(self, at, ChatEvent::DirectStarted { members });
        self.open_table(TOPICS).insert(
            seq.0,
            SenValue::borrowed(&TopicHead {
                place: Place::Direct,
                agents: members.to_vec(),
            }),
        );
        self.open_table(DIRECTS)
            .insert((members[0], members[1]), seq.0);
        Ok(TopicId(seq))
    }

    fn join(&mut self, at: UnixMs, topic: TopicId, agent: AgentId) -> anyhow::Result<()> {
        let mut head = self
            .chat_topic(topic)
            .ok_or_else(|| anyhow::anyhow!("no topic {}", topic.0.0))?;
        head.place.stream_only(
            "no one joins a direct conversation",
            "no one joins a peer conversation",
        )?;
        if head.agents.contains(&agent) {
            return Ok(());
        }
        append(self, at, ChatEvent::Joined { topic, agent });
        head.agents.push(agent);
        self.open_table(TOPICS)
            .insert(topic.0.0, SenValue::borrowed(&head));
        Ok(())
    }

    fn leave(&mut self, at: UnixMs, topic: TopicId, agent: AgentId) -> anyhow::Result<()> {
        let mut head = self
            .chat_topic(topic)
            .ok_or_else(|| anyhow::anyhow!("no topic {}", topic.0.0))?;
        head.place.stream_only(
            "no one leaves a direct conversation",
            "no one leaves a peer conversation",
        )?;
        if !head.agents.contains(&agent) {
            return Ok(());
        }
        append(self, at, ChatEvent::Left { topic, agent });
        head.agents.retain(|member| *member != agent);
        self.open_table(TOPICS)
            .insert(topic.0.0, SenValue::borrowed(&head));
        Ok(())
    }

    fn change_topic(
        &mut self,
        at: UnixMs,
        topic: TopicId,
        stream: StreamId,
        name: String,
        resolved: bool,
    ) -> anyhow::Result<()> {
        let mut head = self
            .chat_topic(topic)
            .ok_or_else(|| anyhow::anyhow!("no topic {}", topic.0.0))?;
        head.place.stream_only(
            "a direct conversation has no name or stream",
            "a peer conversation has no stream, and its name is the peer's",
        )?;
        anyhow::ensure!(
            self.open_table(STREAMS).get(stream.0.0).is_some(),
            "no stream {}",
            stream.0.0
        );
        let name = name.trim().to_owned();
        anyhow::ensure!(!name.is_empty(), "a topic needs a name");
        let place = Place::Stream {
            stream,
            name: name.clone(),
            resolved,
        };
        if head.place == place {
            return Ok(());
        }
        append(
            self,
            at,
            ChatEvent::TopicChanged {
                topic,
                stream,
                name,
                resolved,
            },
        );
        head.place = place;
        self.open_table(TOPICS)
            .insert(topic.0.0, SenValue::borrowed(&head));
        Ok(())
    }

    fn move_messages(
        &mut self,
        at: UnixMs,
        mut messages: Vec<MessageId>,
        to: MoveTo,
        by: Address,
        client_id: Option<u64>,
    ) -> anyhow::Result<TopicId> {
        if let Some(seq) = sent_before(self, client_id) {
            // A client id names the move's event.
            let record = self
                .open_table(CHAT_LOG)
                .get(seq.0)
                .expect("a client id names a logged event")
                .value()
                .into_owned();
            let ChatEvent::Moved { to, .. } = record.event else {
                anyhow::bail!("client id {client_id:?} named no move");
            };
            return Ok(to);
        }
        messages.sort();
        messages.dedup();
        anyhow::ensure!(!messages.is_empty(), "a move needs messages");
        // Where each is now.
        let mut from = Vec::with_capacity(messages.len());
        for id in &messages {
            let record = self
                .open_table(CHAT_LOG)
                .get(id.0.0)
                .map(|record| record.value().into_owned());
            let Some(message) = record.as_ref().and_then(Message::from_record) else {
                anyhow::bail!("no message {}", id.0.0);
            };
            let now = self
                .open_table(MOVED)
                .get(id.0.0)
                .map_or(message.topic, |topic| TopicId(ChatSeq(topic.value())));
            let head = self.chat_topic(now).expect("a message's topic exists");
            head.place.stream_only(
                "a direct conversation's messages stay in it",
                "a peer conversation's messages stay in it",
            )?;
            from.push((*id, message.topic, now));
        }
        let to = match to {
            MoveTo::Topic(topic) => {
                let head = self
                    .chat_topic(topic)
                    .ok_or_else(|| anyhow::anyhow!("no topic {}", topic.0.0))?;
                head.place.stream_only(
                    "messages move only to a topic in a stream",
                    "messages move only to a topic in a stream",
                )?;
                topic
            }
            MoveTo::NewTopic { stream, name } => self.open_topic(at, stream, name, by)?,
        };
        let seq = append(
            self,
            at,
            ChatEvent::Moved {
                messages: messages.clone(),
                to,
            },
        );
        let mut index = self.open_table(TOPIC_MESSAGES);
        for (id, _, now) in &from {
            index.remove((now.0.0, id.0.0));
            index.insert((to.0.0, id.0.0), ());
        }
        drop(index);
        let mut moved = self.open_table(MOVED);
        for (id, posted_in, _) in &from {
            if *posted_in == to {
                moved.remove(id.0.0);
            } else {
                moved.insert(id.0.0, to.0.0);
            }
        }
        drop(moved);
        remember(self, client_id, seq);
        Ok(to)
    }

    fn post(
        &mut self,
        at: UnixMs,
        topic: TopicId,
        message: NewMessage,
        client_id: Option<u64>,
    ) -> anyhow::Result<MessageId> {
        if let Some(seq) = sent_before(self, client_id) {
            return Ok(MessageId(seq));
        }
        let head = self
            .chat_topic(topic)
            .ok_or_else(|| anyhow::anyhow!("no topic {}", topic.0.0))?;
        anyhow::ensure!(has_content(&message.content), "a message needs content");
        anyhow::ensure!(
            message
                .mentions
                .iter()
                .all(|mention| mention.to != Address::Peer),
            "a peer is not mentioned"
        );
        match &head.place {
            Place::Stream { .. } => anyhow::ensure!(
                message.from != Address::Peer,
                "a peer posts only in its peer conversation"
            ),
            Place::Direct => anyhow::ensure!(
                matches!(message.from, Address::Agent(agent) if head.agents.contains(&agent)),
                "only its two agents post in a direct conversation"
            ),
            Place::Peer { open, name, .. } => {
                anyhow::ensure!(*open, "{name} is no longer accepted");
                anyhow::ensure!(
                    match message.from {
                        Address::Agent(agent) => head.agents.contains(&agent),
                        Address::Peer => true,
                        Address::User => false,
                    },
                    "only its agent and its peer post in a peer conversation"
                );
            }
        }
        let NewMessage {
            from,
            mentions,
            content,
        } = message;
        let mut receivers: Vec<AgentId> = head.agents.clone();
        for mention in &mentions {
            if let Address::Agent(agent) = mention.to
                && !receivers.contains(&agent)
            {
                receivers.push(agent);
            }
        }
        if let Address::Agent(sender) = from {
            receivers.retain(|agent| *agent != sender);
        }
        let seq = append(
            self,
            at,
            ChatEvent::Posted {
                topic,
                from,
                mentions,
                content,
            },
        );
        self.open_table(TOPIC_MESSAGES)
            .insert((topic.0.0, seq.0), ());
        let mut inbox = self.open_table(INBOX);
        for agent in receivers {
            inbox.insert((agent, seq.0), ());
        }
        drop(inbox);
        remember(self, client_id, seq);
        Ok(MessageId(seq))
    }

    fn accept_peer(
        &mut self,
        at: UnixMs,
        agent: AgentId,
        peer: PeerAddress,
        name: String,
    ) -> anyhow::Result<TopicId> {
        let name = name.trim().to_owned();
        anyhow::ensure!(!name.is_empty(), "an accepted address needs a name");
        let key = (agent, peer.to_string());
        let open = self
            .open_table(PEERS)
            .get(key.clone())
            .map(|seq| seq.value());
        if let Some(seq) = open {
            let topic = TopicId(ChatSeq(seq));
            let head = self.chat_topic(topic).expect("a peer index names a topic");
            let Place::Peer { name: held, .. } = head.place else {
                panic!("a peer index names a peer conversation");
            };
            anyhow::ensure!(
                held == name,
                "{peer} is already accepted as {held:?}; remove it to name it again"
            );
            return Ok(topic);
        }
        let seq = append(
            self,
            at,
            ChatEvent::PeerAccepted {
                agent,
                peer: peer.clone(),
                name: name.clone(),
            },
        );
        self.open_table(TOPICS).insert(
            seq.0,
            SenValue::borrowed(&TopicHead {
                place: Place::Peer {
                    peer,
                    name,
                    open: true,
                },
                agents: vec![agent],
            }),
        );
        self.open_table(PEERS).insert(key, seq.0);
        Ok(TopicId(seq))
    }

    fn remove_peer(&mut self, at: UnixMs, topic: TopicId) -> anyhow::Result<()> {
        let mut head = self
            .chat_topic(topic)
            .ok_or_else(|| anyhow::anyhow!("no topic {}", topic.0.0))?;
        let Place::Peer { peer, open, .. } = &mut head.place else {
            anyhow::bail!("topic {} is not a peer conversation", topic.0.0);
        };
        if !*open {
            return Ok(());
        }
        *open = false;
        let key = (head.agents[0], peer.to_string());
        append(self, at, ChatEvent::PeerRemoved { topic });
        self.open_table(TOPICS)
            .insert(topic.0.0, SenValue::borrowed(&head));
        self.open_table(PEERS).remove(key);
        self.open_table(PEER_SENT).remove(topic.0.0);
        Ok(())
    }

    fn peer_sent(&mut self, topic: TopicId, message: MessageId) {
        // A conversation closed meanwhile has nothing left to send.
        if !matches!(
            self.chat_topic(topic).map(|head| head.place),
            Some(Place::Peer { open: true, .. })
        ) {
            return;
        }
        let mut sent = self.open_table(PEER_SENT);
        if sent
            .get(topic.0.0)
            .is_none_or(|seq| seq.value() < message.0.0)
        {
            sent.insert(topic.0.0, message.0.0);
        }
    }

    fn chat_topic(&mut self, id: TopicId) -> Option<TopicHead> {
        self.open_table(TOPICS)
            .get(id.0.0)
            .map(|head| head.value().into_owned())
    }

    fn chat_stream_named(&mut self, name: &str) -> Option<StreamId> {
        self.open_table(STREAM_NAMES)
            .get(name.to_owned())
            .map(|id| StreamId(ChatSeq(id.value())))
    }
}
