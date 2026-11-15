//! The host's chat ([`rho_agent_types::ChatEvent`]): one log, dense from
//! one, and the indexes folded from it in the commit of every append.
//!
//! The log holds a forest of messages. Each message keeps its parent now
//! ([`PARENT`]) and each parent its children ([`CHILDREN`]), so a thread is
//! read as a range and a move rewrites two rows. Membership is explicit per
//! thread ([`MEMBERS`]) and read up the chain of parents: the nearest
//! record for an agent decides, so a join on a root is a bot in a channel
//! and a leave on a subthread cuts that branch. A message is in the inbox of
//! every member and every agent it mentions, but not its sender's. The inbox
//! is written with the message, so an agent's delivery reads only its own.

use redb::TableDefinition;
use rho_agent_types::{
    Address, AgentId, ChatEvent, ChatRecord, ChatSeq, ContentPart, Mention, Message, MessageId,
    UnixMs,
};
use rho_db::{ReadTxn, Sen, SenValue, WriteTxn};
use senax_encoder::{Decode, Encode};

use super::{Appended, tell};

const CHAT_LOG: TableDefinition<u64, Sen<ChatRecord>> = TableDefinition::new("chat_log");
/// A message's parent now; zero for a root.
const PARENT: TableDefinition<u64, u64> = TableDefinition::new("chat_parent");
/// `(parent, message)`: a thread's messages in order, under zero the roots.
const CHILDREN: TableDefinition<(u64, u64), ()> = TableDefinition::new("chat_children");
/// `(thread, agent)`: what the agent last said about the thread, joined
/// (`true`) or left.
const MEMBERS: TableDefinition<(u64, AgentId), bool> = TableDefinition::new("chat_members");
/// `(agent, message)`: what each agent receives, in order.
const INBOX: TableDefinition<(AgentId, u64), ()> = TableDefinition::new("chat_inbox");
/// The user's posts by the client's own id for them, so a resend is
/// logged once: the seq of the event the post made.
const CLIENT_IDS: TableDefinition<u64, u64> = TableDefinition::new("chat_client_ids");
/// A thread's given name, when it has one.
const NAMES: TableDefinition<u64, String> = TableDefinition::new("chat_names");

/// The root of the forest in the children index.
const ROOT: u64 = 0;

/// A message as its sender gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewMessage {
    pub from: Address,
    pub mentions: Vec<Mention>,
    pub content: Vec<ContentPart>,
}

/// A thread as a reader needs it: its messages are read separately.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ThreadHead {
    pub id: MessageId,
    /// Its given name, or the first line of its message.
    pub name: String,
    /// From the root down to this thread, each by id and name.
    pub path: Vec<(MessageId, String)>,
    /// Who receives what is posted in it, by the nearest record up the
    /// chain, in the order they joined.
    pub members: Vec<AgentId>,
}

fn seq_of(parent: Option<MessageId>) -> u64 {
    parent.map_or(ROOT, |parent| parent.0.0)
}

fn parent_of(seq: u64) -> Option<MessageId> {
    (seq != ROOT).then_some(MessageId(ChatSeq(seq)))
}

pub trait ChatRead {
    /// The newest event; zero for none.
    fn chat_head(&self) -> ChatSeq;
    /// Up to `limit` events after `after`, in order.
    fn chat_since(&self, after: ChatSeq, limit: usize) -> Vec<ChatRecord>;
    /// A message, under its parent now.
    fn message(&self, id: MessageId) -> Option<Message>;
    /// The thread under `id`, if `id` is a message.
    fn thread(&self, id: MessageId) -> Option<ThreadHead>;
    /// A thread's `newest` latest messages, oldest first; the roots for
    /// `None`.
    fn thread_messages(&self, thread: Option<MessageId>, newest: usize) -> Vec<Message>;
    /// Who receives a message posted under `parent`: the agents whose
    /// nearest membership record up the chain is a join, oldest join first.
    fn members_under(&self, parent: Option<MessageId>) -> Vec<AgentId>;
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
        let mut message = Message::from_record(&record)?;
        message.parent = parent_of(
            self.open_table(PARENT)
                .get(id.0.0)
                .expect("a message has a parent row")
                .value(),
        );
        Some(message)
    }

    fn thread(&self, id: MessageId) -> Option<ThreadHead> {
        let names = self.open_table(NAMES);
        let name_of = |message: &Message| {
            names
                .get(message.id.0.0)
                .map(|name| name.value())
                .unwrap_or_else(|| message.headline())
        };
        let anchor = self.message(id)?;
        let mut path = vec![(id, name_of(&anchor))];
        let mut cursor = anchor.parent;
        while let Some(parent) = cursor {
            let message = self.message(parent).expect("a parent is a message");
            path.push((parent, name_of(&message)));
            cursor = message.parent;
        }
        path.reverse();
        Some(ThreadHead {
            id,
            name: path.last().expect("the path ends in the thread").1.clone(),
            path,
            members: self.members_under(Some(id)),
        })
    }

    fn thread_messages(&self, thread: Option<MessageId>, newest: usize) -> Vec<Message> {
        let parent = seq_of(thread);
        let mut seqs: Vec<u64> = self
            .open_table(CHILDREN)
            .range((parent, 0)..=(parent, u64::MAX))
            .rev()
            .take(newest)
            .map(|(key, _)| key.value().1)
            .collect();
        seqs.reverse();
        seqs.into_iter()
            .map(|seq| {
                self.message(MessageId(ChatSeq(seq)))
                    .expect("a thread names its messages")
            })
            .collect()
    }

    fn members_under(&self, parent: Option<MessageId>) -> Vec<AgentId> {
        let members = self.open_table(MEMBERS);
        let parents = self.open_table(PARENT);
        let mut decided: Vec<(AgentId, bool)> = Vec::new();
        let mut cursor = seq_of(parent);
        while cursor != ROOT {
            let records = members
                .range((cursor, AgentId::MIN)..=(cursor, AgentId::MAX))
                .map(|(key, joined)| (key.value().1, joined.value()))
                .collect();
            decide(&mut decided, records);
            cursor = parents
                .get(cursor)
                .expect("a thread is a message")
                .value();
        }
        joined(decided)
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

/// Decides each agent by the nearest record up the chain: a record for an
/// agent already decided says nothing new.
fn decide(decided: &mut Vec<(AgentId, bool)>, records: Vec<(AgentId, bool)>) {
    for (agent, joined) in records {
        if decided.iter().all(|(known, _)| *known != agent) {
            decided.push((agent, joined));
        }
    }
}

fn joined(decided: Vec<(AgentId, bool)>) -> Vec<AgentId> {
    decided
        .into_iter()
        .filter_map(|(agent, joined)| joined.then_some(agent))
        .collect()
}

/// [`ChatRead::members_under`] on a write transaction.
fn members_under_write(write: &mut WriteTxn, parent: Option<MessageId>) -> Vec<AgentId> {
    let mut decided: Vec<(AgentId, bool)> = Vec::new();
    let mut cursor = seq_of(parent);
    while cursor != ROOT {
        let records = write
            .open_table(MEMBERS)
            .range((cursor, AgentId::MIN)..=(cursor, AgentId::MAX))
            .map(|(key, joined)| (key.value().1, joined.value()))
            .collect();
        decide(&mut decided, records);
        cursor = write
            .open_table(PARENT)
            .get(cursor)
            .expect("a thread is a message")
            .value();
    }
    joined(decided)
}

pub trait ChatWrite {
    fn init_chat_tables(&mut self);
    /// A message under `parent`, or a root. With `client_id`, a message
    /// already posted under that id is returned instead of a second one.
    fn post(
        &mut self,
        at: UnixMs,
        parent: Option<MessageId>,
        message: NewMessage,
        client_id: Option<u64>,
    ) -> anyhow::Result<MessageId>;
    /// Puts a message and its thread under `parent`, or among the roots. A
    /// move to where it is logs nothing.
    fn reparent(
        &mut self,
        at: UnixMs,
        message: MessageId,
        parent: Option<MessageId>,
    ) -> anyhow::Result<()>;
    /// `agent` receives `thread` and what is below it from now on. Logs
    /// nothing when the thread's own record already says so.
    fn join(&mut self, at: UnixMs, thread: MessageId, agent: AgentId) -> anyhow::Result<()>;
    /// `agent` stops receiving `thread` and what is below it. Logs nothing
    /// when the thread's own record already says so, or when nothing up the
    /// chain made it a member.
    fn leave(&mut self, at: UnixMs, thread: MessageId, agent: AgentId) -> anyhow::Result<()>;
    /// Names a thread; empty takes the name back. A name it has logs
    /// nothing.
    fn name(&mut self, at: UnixMs, thread: MessageId, name: String) -> anyhow::Result<()>;
    /// Replays one event into the indexes: what [`ChatWrite::post`] and the
    /// others do after they append. The migration rebuilds the indexes
    /// with it.
    fn index_chat_event(&mut self, seq: ChatSeq, event: &ChatEvent);
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

fn is_message(write: &mut WriteTxn, id: MessageId) -> bool {
    write.open_table(PARENT).get(id.0.0).is_some()
}

fn parent_now(write: &mut WriteTxn, id: MessageId) -> Option<MessageId> {
    parent_of(
        write
            .open_table(PARENT)
            .get(id.0.0)
            .expect("a message has a parent row")
            .value(),
    )
}

/// Whether `ancestor` is `message` or above it.
fn is_above_or_self(write: &mut WriteTxn, ancestor: MessageId, message: MessageId) -> bool {
    let mut cursor = Some(message);
    while let Some(id) = cursor {
        if id == ancestor {
            return true;
        }
        cursor = parent_now(write, id);
    }
    false
}

impl ChatWrite for WriteTxn {
    fn init_chat_tables(&mut self) {
        self.open_table(CHAT_LOG);
        self.open_table(PARENT);
        self.open_table(CHILDREN);
        self.open_table(MEMBERS);
        self.open_table(INBOX);
        self.open_table(CLIENT_IDS);
        self.open_table(NAMES);
    }

    fn post(
        &mut self,
        at: UnixMs,
        parent: Option<MessageId>,
        message: NewMessage,
        client_id: Option<u64>,
    ) -> anyhow::Result<MessageId> {
        if let Some(seq) = sent_before(self, client_id) {
            return Ok(MessageId(seq));
        }
        if let Some(parent) = parent {
            anyhow::ensure!(is_message(self, parent), "no message {}", parent.0.0);
        }
        anyhow::ensure!(has_content(&message.content), "a message needs content");
        let NewMessage {
            from,
            mentions,
            content,
        } = message;
        let event = ChatEvent::Posted {
            parent,
            from,
            mentions,
            content,
        };
        let seq = append(self, at, event.clone());
        self.index_chat_event(seq, &event);
        if let Some(client_id) = client_id {
            self.open_table(CLIENT_IDS).insert(client_id, seq.0);
        }
        Ok(MessageId(seq))
    }

    fn reparent(
        &mut self,
        at: UnixMs,
        message: MessageId,
        parent: Option<MessageId>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(is_message(self, message), "no message {}", message.0.0);
        if let Some(parent) = parent {
            anyhow::ensure!(is_message(self, parent), "no message {}", parent.0.0);
            anyhow::ensure!(
                !is_above_or_self(self, message, parent),
                "a message does not move under itself"
            );
        }
        if parent_now(self, message) == parent {
            return Ok(());
        }
        let event = ChatEvent::Reparented { message, parent };
        let seq = append(self, at, event.clone());
        self.index_chat_event(seq, &event);
        Ok(())
    }

    fn join(&mut self, at: UnixMs, thread: MessageId, agent: AgentId) -> anyhow::Result<()> {
        anyhow::ensure!(is_message(self, thread), "no message {}", thread.0.0);
        let said = self
            .open_table(MEMBERS)
            .get((thread.0.0, agent))
            .map(|joined| joined.value());
        if said == Some(true) {
            return Ok(());
        }
        let event = ChatEvent::Joined { thread, agent };
        let seq = append(self, at, event.clone());
        self.index_chat_event(seq, &event);
        Ok(())
    }

    fn leave(&mut self, at: UnixMs, thread: MessageId, agent: AgentId) -> anyhow::Result<()> {
        anyhow::ensure!(is_message(self, thread), "no message {}", thread.0.0);
        let said = self
            .open_table(MEMBERS)
            .get((thread.0.0, agent))
            .map(|joined| joined.value());
        if said == Some(false) {
            return Ok(());
        }
        let member_above = said.is_none() && {
            let above = parent_now(self, thread);
            members_under_write(self, above).contains(&agent)
        };
        if said.is_none() && !member_above {
            return Ok(());
        }
        let event = ChatEvent::Left { thread, agent };
        let seq = append(self, at, event.clone());
        self.index_chat_event(seq, &event);
        Ok(())
    }

    fn name(&mut self, at: UnixMs, thread: MessageId, name: String) -> anyhow::Result<()> {
        anyhow::ensure!(is_message(self, thread), "no message {}", thread.0.0);
        let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
        let now = self
            .open_table(NAMES)
            .get(thread.0.0)
            .map(|name| name.value())
            .unwrap_or_default();
        if now == name {
            return Ok(());
        }
        let event = ChatEvent::Named { thread, name };
        let seq = append(self, at, event.clone());
        self.index_chat_event(seq, &event);
        Ok(())
    }

    fn index_chat_event(&mut self, seq: ChatSeq, event: &ChatEvent) {
        match event {
            ChatEvent::Posted {
                parent,
                from,
                mentions,
                ..
            } => {
                let mut receivers = members_under_write(self, *parent);
                for mention in mentions {
                    if let Address::Agent(agent) = mention.to
                        && !receivers.contains(&agent)
                    {
                        receivers.push(agent);
                    }
                }
                if let Address::Agent(sender) = from {
                    receivers.retain(|agent| agent != sender);
                }
                self.open_table(PARENT).insert(seq.0, seq_of(*parent));
                self.open_table(CHILDREN)
                    .insert((seq_of(*parent), seq.0), ());
                let mut inbox = self.open_table(INBOX);
                for agent in receivers {
                    inbox.insert((agent, seq.0), ());
                }
            }
            ChatEvent::Reparented { message, parent } => {
                let was = self
                    .open_table(PARENT)
                    .get(message.0.0)
                    .expect("a moved message has a parent row")
                    .value();
                self.open_table(PARENT).insert(message.0.0, seq_of(*parent));
                let mut children = self.open_table(CHILDREN);
                children.remove((was, message.0.0));
                children.insert((seq_of(*parent), message.0.0), ());
            }
            ChatEvent::Joined { thread, agent } => {
                self.open_table(MEMBERS).insert((thread.0.0, *agent), true);
            }
            ChatEvent::Left { thread, agent } => {
                self.open_table(MEMBERS).insert((thread.0.0, *agent), false);
            }
            ChatEvent::Named { thread, name } => {
                let mut names = self.open_table(NAMES);
                if name.is_empty() {
                    names.remove(thread.0.0);
                } else {
                    names.insert(thread.0.0, name.clone());
                }
            }
        }
    }
}

/// Temporary (`temp-migration`): the log as it was before chat became a
/// forest, folded into the forest once. Every event keeps its seq, so the
/// agents' `origin` props and the messages the transcripts name stay
/// valid. A stream becomes a root message with its name as text; a topic
/// becomes a message under its stream; a message's topic becomes its
/// parent; a move becomes a reparent; a rename becomes a `Named`. Direct
/// conversations are not chat any more: their events become `Named`
/// records that name nothing, which the fold ignores. Delete this module
/// once every host has run it.
pub(crate) mod legacy {
    use rho_agent_types::{
        Address, AgentId, ChatEvent as NewEvent, ChatRecord as NewRecord, ChatSeq, ContentPart,
        Mention, MessageId, UnixMs,
    };
    use rho_db::{Sen, SenValue, WriteTxn};
    use senax_encoder::{Decode, Encode};

    use super::{CHAT_LOG, ChatWrite as _};

    const RETIRED: [&str; 6] = [
        "chat_streams",
        "chat_stream_names",
        "chat_topics",
        "chat_topic_messages",
        "chat_directs",
        "chat_moved",
    ];
    /// The old log, by its own record type: the table's type is its name.
    pub(crate) const OLD_LOG: redb::TableDefinition<u64, Sen<ChatRecord>> =
        redb::TableDefinition::new("chat_log");

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
    pub struct StreamId(pub ChatSeq);
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
    pub struct TopicId(pub ChatSeq);

    /// The old event, under its old name so the old table opens.
    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    pub enum ChatEvent {
        StreamCreated {
            name: String,
        },
        TopicStarted {
            stream: StreamId,
            name: String,
            by: Address,
        },
        DirectStarted {
            members: [AgentId; 2],
        },
        Joined {
            topic: TopicId,
            agent: AgentId,
        },
        Left {
            topic: TopicId,
            agent: AgentId,
        },
        TopicChanged {
            topic: TopicId,
            stream: StreamId,
            name: String,
            resolved: bool,
        },
        Moved {
            messages: Vec<MessageId>,
            to: TopicId,
        },
        Posted {
            topic: TopicId,
            from: Address,
            mentions: Vec<Mention>,
            content: Vec<ContentPart>,
        },
    }

    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    pub struct ChatRecord {
        pub seq: ChatSeq,
        pub at: UnixMs,
        pub event: ChatEvent,
    }

    /// A record that changes nothing: a `Named` of the seq's own position,
    /// which is no thread, with no name.
    fn nothing(seq: ChatSeq) -> NewEvent {
        NewEvent::Named {
            thread: MessageId(seq),
            name: String::new(),
        }
    }

    /// The forest the old log comes to, in log order, with every seq kept.
    /// Events that no longer fit at their seq go after the last.
    pub fn convert(old: &[ChatRecord]) -> Vec<NewRecord> {
        use std::collections::{HashMap, HashSet};
        let mut directs: HashSet<u64> = HashSet::new();
        // Each topic's stream and name as the old log last said.
        let mut topics: HashMap<u64, (StreamId, String)> = HashMap::new();
        let mut out = Vec::with_capacity(old.len());
        let mut tail: Vec<NewEvent> = Vec::new();
        for record in old {
            let seq = record.seq;
            let text = |name: &str| {
                vec![ContentPart::Text {
                    text: name.to_owned(),
                }]
            };
            let event = match &record.event {
                ChatEvent::StreamCreated { name } => NewEvent::Posted {
                    parent: None,
                    from: Address::User,
                    mentions: Vec::new(),
                    content: text(name),
                },
                ChatEvent::TopicStarted { stream, name, by } => {
                    topics.insert(seq.0, (*stream, name.clone()));
                    NewEvent::Posted {
                        parent: Some(MessageId(stream.0)),
                        from: *by,
                        mentions: Vec::new(),
                        content: text(name),
                    }
                }
                ChatEvent::DirectStarted { .. } => {
                    directs.insert(seq.0);
                    nothing(seq)
                }
                ChatEvent::Joined { topic, agent } => NewEvent::Joined {
                    thread: MessageId(topic.0),
                    agent: *agent,
                },
                ChatEvent::Left { topic, agent } => NewEvent::Left {
                    thread: MessageId(topic.0),
                    agent: *agent,
                },
                ChatEvent::TopicChanged {
                    topic,
                    stream,
                    name,
                    ..
                } => {
                    let was = topics.get(&topic.0.0).cloned();
                    topics.insert(topic.0.0, (*stream, name.clone()));
                    if was.as_ref().is_some_and(|(was, _)| was != stream) {
                        tail.push(NewEvent::Reparented {
                            message: MessageId(topic.0),
                            parent: Some(MessageId(stream.0)),
                        });
                    }
                    NewEvent::Named {
                        thread: MessageId(topic.0),
                        name: name.clone(),
                    }
                }
                ChatEvent::Moved { messages, to } => {
                    let mut moved = messages.iter().map(|message| NewEvent::Reparented {
                        message: *message,
                        parent: Some(MessageId(to.0)),
                    });
                    let first = moved.next().unwrap_or_else(|| nothing(seq));
                    tail.extend(moved);
                    first
                }
                ChatEvent::Posted {
                    topic,
                    from,
                    mentions,
                    content,
                } => {
                    if directs.contains(&topic.0.0) {
                        nothing(seq)
                    } else {
                        NewEvent::Posted {
                            parent: Some(MessageId(topic.0)),
                            from: *from,
                            mentions: mentions.clone(),
                            content: content.clone(),
                        }
                    }
                }
            };
            out.push(NewRecord {
                seq,
                at: record.at,
                event,
            });
        }
        let at = old.last().map_or(UnixMs(0), |record| record.at);
        let mut seq = old.last().map_or(ChatSeq::ZERO, |record| record.seq);
        for event in tail {
            seq = seq.next();
            out.push(NewRecord { seq, at, event });
        }
        out
    }

    /// Rewrites the log in place and rebuilds every index from it.
    pub fn migrate_topics_to_forest(write: &mut WriteTxn) {
        let old: Vec<ChatRecord> = write
            .open_table(OLD_LOG)
            .iter()
            .map(|(_, record)| record.value().into_owned())
            .collect();
        let records = convert(&old);
        for table in RETIRED {
            write.delete_table(table);
        }
        write.delete_table("chat_log");
        write.delete_table("chat_inbox");
        write.init_chat_tables();
        let mut log = write.open_table(CHAT_LOG);
        for record in &records {
            log.insert(record.seq.0, SenValue::borrowed(record));
        }
        drop(log);
        for record in &records {
            write.index_chat_event(record.seq, &record.event);
        }
    }
}
