//! The host's chat ([`rho_agent_types::ChatEvent`]): one log, dense from
//! one, and the indexes folded from it in the commit of every append.
//!
//! Each conversation keeps its members; a message is in the inbox of every
//! member and every agent it mentions, but not its sender's. The inbox is
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
/// `(a, b)` with `a < b`: the direct conversation of two agents.
const DIRECTS: TableDefinition<(AgentId, AgentId), u64> = TableDefinition::new("chat_directs");

/// Where a conversation is.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Place {
    /// A topic: anyone may post in it, and agents join it.
    Stream { stream: StreamId, name: String },
    /// Two agents' own conversation: only they post in it.
    Direct,
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
    /// A topic with no message yet, for a writer that must have members
    /// join before the first one: the migration from `main`. Everything
    /// else starts a topic with its message ([`ChatWrite::start_topic`]).
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
            return Ok((message.topic, message.id));
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
                place: Place::Stream { stream, name },
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
        anyhow::ensure!(
            head.place != Place::Direct,
            "no one joins a direct conversation"
        );
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
        let head = self
            .chat_topic(topic)
            .ok_or_else(|| anyhow::anyhow!("no topic {}", topic.0.0))?;
        anyhow::ensure!(has_content(&message.content), "a message needs content");
        if head.place == Place::Direct {
            anyhow::ensure!(
                matches!(message.from, Address::Agent(agent) if head.agents.contains(&agent)),
                "only its two agents post in a direct conversation"
            );
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
