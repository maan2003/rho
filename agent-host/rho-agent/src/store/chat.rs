//! The host's chat ([`rho_agent_types::ChatEvent`]): one log, dense from
//! one, and the indexes folded from it in the commit of every append.
//!
//! Each topic keeps its agents; a message is in the inbox of every agent of
//! its topic and every agent it mentions, but not its sender's. The inbox is
//! written with the message, so an agent's delivery reads only its own.

use redb::TableDefinition;
use rho_agent_types::{
    Address, AgentId, ChatEvent, ChatRecord, ChatSeq, ContentPart, Mention, Message, MessageId,
    StreamId, TopicId, UnixMs,
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

/// The stream every host has from its first start.
pub const DEFAULT_STREAM: &str = "rho";

/// A topic as the log has made it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct TopicHead {
    pub stream: StreamId,
    pub name: String,
    /// The agents that receive its messages, in the order they joined.
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
        Message::from_record(&record)
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
}

pub trait ChatWrite {
    fn init_chat_tables(&mut self);
    fn create_stream(&mut self, at: UnixMs, name: String) -> anyhow::Result<StreamId>;
    /// A topic with its first message. With `client_id`, a topic already
    /// started under that id is returned instead of a second one.
    fn start_topic(
        &mut self,
        at: UnixMs,
        stream: StreamId,
        name: String,
        agents: Vec<AgentId>,
        first: NewMessage,
        client_id: Option<u64>,
    ) -> anyhow::Result<(TopicId, MessageId)>;
    fn join(&mut self, at: UnixMs, topic: TopicId, agent: AgentId) -> anyhow::Result<()>;
    /// With `client_id`, as in [`ChatWrite::start_topic`].
    fn post(
        &mut self,
        at: UnixMs,
        topic: TopicId,
        message: NewMessage,
        client_id: Option<u64>,
    ) -> anyhow::Result<MessageId>;
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
        if self.open_table(CHAT_LOG).iter().next().is_none() {
            self.create_stream(UnixMs::now(), DEFAULT_STREAM.to_owned())
                .expect("an empty chat has no streams");
        }
    }

    fn create_stream(&mut self, at: UnixMs, name: String) -> anyhow::Result<StreamId> {
        anyhow::ensure!(!name.trim().is_empty(), "a stream needs a name");
        anyhow::ensure!(
            self.open_table(STREAM_NAMES).get(name.clone()).is_none(),
            "a stream named {name:?} exists"
        );
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
        agents: Vec<AgentId>,
        first: NewMessage,
        client_id: Option<u64>,
    ) -> anyhow::Result<(TopicId, MessageId)> {
        if let Some(seq) = sent_before(self, client_id) {
            // A topic's first message is the event after it.
            return Ok((TopicId(seq), MessageId(seq.next())));
        }
        anyhow::ensure!(
            self.open_table(STREAMS).get(stream.0.0).is_some(),
            "no stream {}",
            stream.0.0
        );
        anyhow::ensure!(!name.trim().is_empty(), "a topic needs a name");
        let mut unique = Vec::with_capacity(agents.len());
        for agent in agents {
            if !unique.contains(&agent) {
                unique.push(agent);
            }
        }
        let by = first.from;
        let seq = append(
            self,
            at,
            ChatEvent::TopicStarted {
                stream,
                name: name.clone(),
                by,
                agents: unique.clone(),
            },
        );
        let topic = TopicId(seq);
        self.open_table(TOPICS).insert(
            seq.0,
            SenValue::borrowed(&TopicHead {
                stream,
                name,
                agents: unique,
            }),
        );
        remember(self, client_id, seq);
        let first = self.post(at, topic, first, None)?;
        Ok((topic, first))
    }

    fn join(&mut self, at: UnixMs, topic: TopicId, agent: AgentId) -> anyhow::Result<()> {
        let mut head = self
            .chat_topic(topic)
            .ok_or_else(|| anyhow::anyhow!("no topic {}", topic.0.0))?;
        if head.agents.contains(&agent) {
            return Ok(());
        }
        append(self, at, ChatEvent::Joined { topic, agent });
        head.agents.push(agent);
        self.open_table(TOPICS)
            .insert(topic.0.0, SenValue::borrowed(&head));
        Ok(())
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
        let mut head = self
            .chat_topic(topic)
            .ok_or_else(|| anyhow::anyhow!("no topic {}", topic.0.0))?;
        anyhow::ensure!(
            message.content.iter().any(|part| match part {
                ContentPart::Text { text } => !text.trim().is_empty(),
                _ => true,
            }),
            "a message needs content"
        );
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
            if !head.agents.contains(&sender) {
                head.agents.push(sender);
                self.open_table(TOPICS)
                    .insert(topic.0.0, SenValue::borrowed(&head));
            }
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
