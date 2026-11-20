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
    Address, AgentId, ChatEvent, ChatRecord, ChatSeq, ContentPart, Mention, MentionKind, Message,
    MessageId, Revision, UnixMs,
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

/// `(message, latest edit/delete event)`, pointing into the log rather than
/// copying its body. The original posting still owns the identity and sender.
const REVISIONS: TableDefinition<u64, u64> = TableDefinition::new("chat_revisions");

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

/// A search hit in the current (not historical) mail.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct MailHit {
    pub project: MessageId,
    pub thread: MessageId,
    pub message: MessageId,
    pub subject: String,
    pub excerpt: String,
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
    /// The host's newest channel with this name.
    fn project_named(&self, name: &str) -> Option<MessageId>;
    fn search_mail(
        &self,
        query: &str,
        project: Option<MessageId>,
        limit: usize,
    ) -> anyhow::Result<Vec<MailHit>>;
    fn mail_thread(&self, message: &Message) -> MessageId;
    fn first_thread(&self, agent: AgentId) -> Option<MessageId>;
    fn last_user_mail(&self, thread: MessageId) -> Option<UnixMs>;
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
        if let Some(revision) = self.open_table(REVISIONS).get(id.0.0) {
            let record = self
                .open_table(CHAT_LOG)
                .get(revision.value())
                .expect("a revision is a chat event")
                .value()
                .into_owned();
            message.apply_revision(&record.event);
        }
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
            cursor = parents.get(cursor).expect("a thread is a message").value();
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
            .filter(|message| message.revision != Revision::Deleted)
            .collect()
    }

    fn project_named(&self, name: &str) -> Option<MessageId> {
        let name = rho_agent_types::one_line(name);
        let names = self.open_table(NAMES);
        self.thread_messages(None, usize::MAX)
            .into_iter()
            .rev()
            .find(|root| {
                root.revision != Revision::Deleted
                    && names
                        .get(root.id.0.0)
                        .map(|name| name.value())
                        .unwrap_or_else(|| root.headline())
                        == name
            })
            .map(|root| root.id)
    }

    fn search_mail(
        &self,
        query: &str,
        project: Option<MessageId>,
        limit: usize,
    ) -> anyhow::Result<Vec<MailHit>> {
        let query = query.to_lowercase();
        let terms = query.split_whitespace().collect::<Vec<_>>();
        anyhow::ensure!(!terms.is_empty(), "search needs words");
        if let Some(project) = project {
            anyhow::ensure!(
                self.message(project)
                    .is_some_and(|root| root.parent.is_none()),
                "no project #{}",
                project.0.0
            );
        }
        let names = self.open_table(NAMES);
        let name_of = |message: &Message| {
            names
                .get(message.id.0.0)
                .map(|name| name.value())
                .unwrap_or_else(|| message.headline())
        };
        let mut hits = Vec::new();
        let parents = self.open_table(PARENT);
        let mut candidates = parents
            .iter()
            .rev()
            .map(|(key, _)| MessageId(ChatSeq(key.value())));
        for id in &mut candidates {
            if hits.len() == limit {
                break;
            }
            let mail = self.message(id).expect("a parent row names a message");
            if mail.revision == Revision::Deleted {
                continue;
            }
            let thread = self.mail_thread(&mail);
            let mut root = id;
            while let Some(parent) =
                parent_of(parents.get(root.0.0).expect("known message").value())
            {
                root = parent;
            }
            if project.is_some_and(|project| project != root) {
                continue;
            }
            let body = mail.text();
            let subject = name_of(&self.message(thread).expect("a thread is a message"));
            let searchable = if id == thread {
                format!("{subject}\n{body}").to_lowercase()
            } else {
                // A subject names its thread once; it does not make each reply
                // a hit.
                body.to_lowercase()
            };
            if terms.iter().all(|term| searchable.contains(term)) {
                let excerpt = body
                    .lines()
                    .find(|line| terms.iter().any(|term| line.to_lowercase().contains(term)))
                    .unwrap_or(&body)
                    .chars()
                    .take(180)
                    .collect();
                hits.push(MailHit {
                    project: root,
                    thread,
                    message: id,
                    subject,
                    excerpt,
                });
            }
        }
        Ok(hits)
    }

    fn first_thread(&self, agent: AgentId) -> Option<MessageId> {
        self.open_table(CHAT_LOG)
            .iter()
            .find_map(|(_, row)| match row.value().into_owned().event {
                ChatEvent::Joined {
                    thread,
                    agent: member,
                } if agent == member => Some(thread),
                _ => None,
            })
    }

    fn last_user_mail(&self, thread: MessageId) -> Option<UnixMs> {
        let log = self.open_table(CHAT_LOG);
        self.open_table(CHILDREN)
            .range((thread.0.0, 0)..=(thread.0.0, u64::MAX))
            .rev()
            .find_map(|(key, _)| {
                let record = log
                    .get(key.value().1)
                    .expect("a child is a message")
                    .value()
                    .into_owned();
                matches!(
                    record.event,
                    ChatEvent::Posted {
                        from: Address::User,
                        ..
                    }
                )
                .then_some(record.at)
            })
    }

    fn mail_thread(&self, message: &Message) -> MessageId {
        match message.parent {
            Some(parent)
                if parent_of(
                    self.open_table(PARENT)
                        .get(parent.0.0)
                        .expect("a parent is a message")
                        .value(),
                )
                .is_some() =>
            {
                parent
            }
            _ => message.id,
        }
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
    /// Change an agent's own posted mail, except an answered ask.
    fn edit(
        &mut self,
        at: UnixMs,
        agent: AgentId,
        message: MessageId,
        content: Vec<ContentPart>,
    ) -> anyhow::Result<()>;
    fn delete(&mut self, at: UnixMs, agent: AgentId, message: MessageId) -> anyhow::Result<()>;
    /// A stop is logged at most once for a particular agent and start time.
    fn post_stop(
        &mut self,
        at: UnixMs,
        thread: MessageId,
        sender: Address,
        text: String,
    ) -> anyhow::Result<Option<MessageId>>;
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
    /// Names a thread nothing has named: an existing name wins, whoever
    /// gave it. Whether the thread was named now.
    fn name_unnamed(&mut self, at: UnixMs, thread: MessageId, name: String)
    -> anyhow::Result<bool>;
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

fn check_revision(write: &mut WriteTxn, agent: AgentId, id: MessageId) -> anyhow::Result<()> {
    let log = write.open_table(CHAT_LOG);
    let original = log
        .get(id.0.0)
        .and_then(|record| Message::from_record(&record.value().into_owned()))
        .ok_or_else(|| anyhow::anyhow!("no message #{}", id.0.0))?;
    anyhow::ensure!(
        original.from == Address::Agent(agent),
        "you can only change your own mail"
    );
    drop(log);
    let revision = write
        .open_table(REVISIONS)
        .get(id.0.0)
        .map(|seq| seq.value());
    let log = write.open_table(CHAT_LOG);
    anyhow::ensure!(
        !revision.is_some_and(|seq| matches!(
            log.get(seq)
                .expect("a revision is a chat event")
                .value()
                .into_owned()
                .event,
            ChatEvent::Deleted { .. }
        )),
        "mail #{} was deleted",
        id.0.0
    );
    if original.mentions(Address::User) == Some(MentionKind::Ask) {
        // An answer remains an answer even if either mail is moved later.
        // Follow the ask's location as it was when each reply was posted.
        let mut thread = original.parent;
        let mut is_reply = thread.is_some_and(|parent| {
            log.get(parent.0.0).is_some_and(|row| {
                matches!(
                    row.value().into_owned().event,
                    ChatEvent::Posted {
                        parent: Some(_),
                        ..
                    }
                )
            })
        });
        for (_, row) in log.range(id.0.0 + 1..) {
            match row.value().into_owned().event {
                ChatEvent::Reparented { message, parent } if message == id => {
                    thread = parent;
                    is_reply = thread.is_some_and(|parent| {
                        log.get(parent.0.0).is_some_and(|row| {
                            matches!(
                                row.value().into_owned().event,
                                ChatEvent::Posted {
                                    parent: Some(_),
                                    ..
                                }
                            )
                        })
                    });
                }
                ChatEvent::Reparented { message, parent } if Some(message) == thread => {
                    is_reply = parent.is_some();
                }
                ChatEvent::Posted {
                    from: Address::User,
                    parent,
                    ..
                } if parent == Some(id) || (is_reply && parent == thread) => {
                    anyhow::bail!(
                        "you cannot change ask #{}: the user already answered it",
                        id.0.0
                    );
                }
                _ => {}
            }
        }
    }
    Ok(())
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
        self.open_table(REVISIONS);
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

    fn edit(
        &mut self,
        at: UnixMs,
        agent: AgentId,
        message: MessageId,
        content: Vec<ContentPart>,
    ) -> anyhow::Result<()> {
        check_revision(self, agent, message)?;
        anyhow::ensure!(has_content(&content), "a message needs content");
        let event = ChatEvent::Edited { message, content };
        let seq = append(self, at, event.clone());
        self.index_chat_event(seq, &event);
        Ok(())
    }

    fn delete(&mut self, at: UnixMs, agent: AgentId, message: MessageId) -> anyhow::Result<()> {
        check_revision(self, agent, message)?;
        let event = ChatEvent::Deleted { message };
        let seq = append(self, at, event.clone());
        self.index_chat_event(seq, &event);
        Ok(())
    }

    fn post_stop(
        &mut self,
        at: UnixMs,
        thread: MessageId,
        sender: Address,
        text: String,
    ) -> anyhow::Result<Option<MessageId>> {
        anyhow::ensure!(
            matches!(sender, Address::System { .. }),
            "a stop is sent by the system bot"
        );
        // The durable stop mail itself is the deduplication record, including
        // across host re-exec, and if its message or thread is later moved.
        if self.open_table(CHAT_LOG).iter().rev().any(|(_, row)| {
            matches!(
                row.value().into_owned().event, ChatEvent::Posted { from, .. } if from == sender
            )
        }) {
            return Ok(None);
        }
        self.post(
            at,
            Some(thread),
            NewMessage {
                from: sender,
                mentions: vec![Mention {
                    to: Address::User,
                    kind: MentionKind::Ask,
                }],
                content: vec![ContentPart::Text { text }],
            },
            None,
        )
        .map(Some)
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

    fn name_unnamed(
        &mut self,
        at: UnixMs,
        thread: MessageId,
        name: String,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(is_message(self, thread), "no message {}", thread.0.0);
        if self.open_table(NAMES).get(thread.0.0).is_some() || name.trim().is_empty() {
            return Ok(false);
        }
        self.name(at, thread, name)?;
        Ok(true)
    }

    fn index_chat_event(&mut self, seq: ChatSeq, event: &ChatEvent) {
        match event {
            ChatEvent::Posted {
                parent,
                from,
                mentions,
                ..
            } => {
                // A system stop is mail for the user, not fresh input to the
                // stopped agent or to another member of its thread.
                let mut receivers = if matches!(from, Address::System { .. }) {
                    Vec::new()
                } else {
                    members_under_write(self, *parent)
                };
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
            ChatEvent::Edited { message, .. } | ChatEvent::Deleted { message } => {
                self.open_table(REVISIONS).insert(message.0.0, seq.0);
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
