//! The host's mail ([`rho_agent_types::ChatEvent`]): one log, dense from
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

// Table names are stable on disk across the mail protocol rename.
const MAIL_LOG: TableDefinition<u64, Sen<ChatRecord>> = TableDefinition::new("chat_log");
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
/// The first Joined names where an agent's work began.
const FIRST_THREAD: TableDefinition<AgentId, u64> = TableDefinition::new("chat_first_thread");
/// A thread's given name, when it has one.
const NAMES: TableDefinition<u64, String> = TableDefinition::new("chat_names");

/// `(message, latest edit/delete event)`, pointing into the log rather than
/// copying its body. The original posting still owns the identity and sender.
const REVISIONS: TableDefinition<u64, u64> = TableDefinition::new("chat_revisions");

const AGENT_MAIL_PROJECT: &str = "Agent mail";

/// The root of the forest in the children index.
const ROOT: u64 = 0;

/// A message as its sender gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewMessage {
    pub from: Address,
    pub reply_to: Option<MessageId>,
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

pub trait MailRead {
    /// The newest event; zero for none.
    fn mail_head(&self) -> ChatSeq;
    /// Up to `limit` events after `after`, in order.
    fn mail_since(&self, after: ChatSeq, limit: usize) -> Vec<ChatRecord>;
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
    fn recipients_of(&self, message: &Message) -> Vec<AgentId>;
    /// The enclosing root for any mail.
    fn root_of(&self, message: MessageId) -> Option<MessageId>;
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

impl MailRead for ReadTxn {
    fn mail_head(&self) -> ChatSeq {
        self.open_table(MAIL_LOG)
            .iter()
            .next_back()
            .map_or(ChatSeq::ZERO, |(key, _)| ChatSeq(key.value()))
    }

    fn mail_since(&self, after: ChatSeq, limit: usize) -> Vec<ChatRecord> {
        self.open_table(MAIL_LOG)
            .range(after.0 + 1..)
            .take(limit)
            .map(|(_, record)| record.value().into_owned())
            .collect()
    }

    fn message(&self, id: MessageId) -> Option<Message> {
        let record = self.open_table(MAIL_LOG).get(id.0.0)?.value().into_owned();
        let mut message = Message::from_record(&record)?;
        message.parent = parent_of(
            self.open_table(PARENT)
                .get(id.0.0)
                .expect("a message has a parent row")
                .value(),
        );
        if let Some(revision) = self.open_table(REVISIONS).get(id.0.0) {
            let record = self
                .open_table(MAIL_LOG)
                .get(revision.value())
                .expect("a revision is a mail event")
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

    fn recipients_of(&self, message: &Message) -> Vec<AgentId> {
        recipients(message.from, &message.mentions, || {
            self.members_under(message.parent)
        })
    }

    fn root_of(&self, mut message: MessageId) -> Option<MessageId> {
        let parents = self.open_table(PARENT);
        while let Some(parent) = parent_of(parents.get(message.0.0)?.value()) {
            message = parent;
        }
        Some(message)
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
        self.open_table(FIRST_THREAD)
            .get(agent)
            .map(|thread| MessageId(ChatSeq(thread.value())))
    }

    fn last_user_mail(&self, thread: MessageId) -> Option<UnixMs> {
        let log = self.open_table(MAIL_LOG);
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

/// Delivery is fixed by the record, not by which process happens to be
/// running. Directed mail never fans out to inherited thread members.
fn recipients(
    sender: Address,
    mentions: &[Mention],
    members: impl FnOnce() -> Vec<AgentId>,
) -> Vec<AgentId> {
    if matches!(sender, Address::System { .. }) {
        return Vec::new();
    }
    let mut agents = if mentions.iter().any(|m| m.kind == MentionKind::Direct) {
        Vec::new()
    } else {
        members()
    };
    for mention in mentions {
        if let Address::Agent(agent) = mention.to
            && sender != mention.to
            && !agents.contains(&agent)
        {
            agents.push(agent);
        }
    }
    if let Address::Agent(sender) = sender {
        agents.retain(|agent| *agent != sender);
    }
    agents
}

fn peer_pair(from: Address, mentions: &[Mention]) -> Option<[AgentId; 2]> {
    let (
        Address::Agent(from),
        [
            Mention {
                to: Address::Agent(to),
                kind: MentionKind::Direct,
            },
        ],
    ) = (from, mentions)
    else {
        return None;
    };
    let mut pair = [from, *to];
    pair.sort_unstable();
    Some(pair)
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

/// [`MailRead::members_under`] on a write transaction.
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

pub trait MailWrite {
    fn init_mail_tables(&mut self);
    /// A message under `parent`, or a root. With `client_id`, a message
    /// already posted under that id is returned instead of a second one.
    fn post(
        &mut self,
        at: UnixMs,
        parent: Option<MessageId>,
        message: NewMessage,
        client_id: Option<u64>,
    ) -> anyhow::Result<MessageId>;
    /// A direct conversation under a project. Its first mail is the
    /// anchor; later mail between the same pair goes under that anchor.
    fn post_peer(
        &mut self,
        at: UnixMs,
        project: Option<MessageId>,
        message: NewMessage,
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
    /// Replays one event into the indexes: what [`MailWrite::post`] and the
    /// others do after they append. The migration rebuilds the indexes
    /// with it.
    fn index_mail_event(&mut self, seq: ChatSeq, event: &ChatEvent);
}

fn next_seq(write: &mut WriteTxn) -> ChatSeq {
    write
        .open_table(MAIL_LOG)
        .iter()
        .next_back()
        .map_or(ChatSeq(1), |(key, _)| ChatSeq(key.value()).next())
}

fn append(write: &mut WriteTxn, at: UnixMs, event: ChatEvent) -> ChatSeq {
    let seq = next_seq(write);
    write
        .open_table(MAIL_LOG)
        .insert(seq.0, SenValue::borrowed(&ChatRecord { seq, at, event }));
    tell(write, Appended::Mail(seq));
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

fn direct_pair_at(write: &mut WriteTxn, id: MessageId) -> Option<[AgentId; 2]> {
    let row = write.open_table(MAIL_LOG).get(id.0.0)?.value().into_owned();
    let ChatEvent::Posted { from, mentions, .. } = row.event else {
        return None;
    };
    peer_pair(from, &mentions)
}

/// A direct conversation may be read one level down, but it cannot
/// produce user attention. Find the anchor even for a moved conversation,
/// a deleted posting, or a tangent inside the conversation.
fn peer_thread(write: &mut WriteTxn, id: MessageId) -> Option<(MessageId, [AgentId; 2])> {
    let mut cursor = Some(id);
    let mut thread = None;
    while let Some(id) = cursor {
        if let Some(pair) = direct_pair_at(write, id) {
            if thread.is_some_and(|(_, nearest)| nearest != pair) {
                break;
            }
            thread = Some((id, pair));
        }
        cursor = parent_now(write, id);
    }
    thread
}

fn unscoped_peer_project(
    write: &mut WriteTxn,
    at: UnixMs,
    from: Address,
) -> anyhow::Result<MessageId> {
    let root_ids: Vec<_> = write
        .open_table(CHILDREN)
        .range((ROOT, 0)..=(ROOT, u64::MAX))
        .rev()
        .map(|(key, _)| key.value().1)
        .collect();
    for id in root_ids {
        let named = write.open_table(NAMES).get(id).map(|name| name.value());
        let name = named.or_else(|| {
            let row = write.open_table(MAIL_LOG).get(id)?.value().into_owned();
            Some(Message::from_record(&row)?.headline())
        });
        if name.as_deref() == Some(AGENT_MAIL_PROJECT) {
            return Ok(MessageId(ChatSeq(id)));
        }
    }
    write.post(
        at,
        None,
        NewMessage {
            from,
            reply_to: None,
            mentions: Vec::new(),
            content: vec![ContentPart::Text {
                text: AGENT_MAIL_PROJECT.into(),
            }],
        },
        None,
    )
}

fn check_revision(write: &mut WriteTxn, agent: AgentId, id: MessageId) -> anyhow::Result<()> {
    let log = write.open_table(MAIL_LOG);
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
    let log = write.open_table(MAIL_LOG);
    anyhow::ensure!(
        !revision.is_some_and(|seq| matches!(
            log.get(seq)
                .expect("a revision is a mail event")
                .value()
                .into_owned()
                .event,
            ChatEvent::Deleted { .. }
        )),
        "mail #{} was deleted",
        id.0.0
    );
    if original.mentions(Address::User) == Some(MentionKind::Ask) {
        // An explicit answer doesn't depend on either mail's current
        // parent. Older clients could also reply directly under a mail.
        for (_, row) in log.range(id.0.0 + 1..) {
            if let ChatEvent::Posted {
                from: Address::User,
                parent,
                reply_to,
                ..
            } = row.value().into_owned().event
                && reply_to.or(parent) == Some(id)
            {
                anyhow::bail!(
                    "you cannot change ask #{}: the user already answered it",
                    id.0.0
                );
            }
        }
    }
    Ok(())
}

impl MailWrite for WriteTxn {
    fn init_mail_tables(&mut self) {
        self.open_table(MAIL_LOG);
        self.open_table(PARENT);
        self.open_table(CHILDREN);
        self.open_table(MEMBERS);
        self.open_table(INBOX);
        self.open_table(CLIENT_IDS);
        self.open_table(NAMES);
        self.open_table(REVISIONS);
        self.open_table(FIRST_THREAD);
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
            reply_to,
            mentions,
            content,
        } = message;
        if let Some(reply_to) = reply_to {
            anyhow::ensure!(is_message(self, reply_to), "no mail #{}", reply_to.0.0);
        }
        if let Some(parent) = parent
            && let Some((_, pair)) = peer_thread(self, parent)
            && let Address::Agent(agent) = from
        {
            anyhow::ensure!(
                peer_pair(from, &mentions) == Some(pair) && pair.contains(&agent),
                "address the other agent to reply in a direct conversation"
            );
        }
        let event = ChatEvent::Posted {
            parent,
            reply_to,
            from,
            mentions,
            content,
        };
        let seq = append(self, at, event.clone());
        self.index_mail_event(seq, &event);
        if let Some(client_id) = client_id {
            self.open_table(CLIENT_IDS).insert(client_id, seq.0);
        }
        Ok(MessageId(seq))
    }

    fn post_peer(
        &mut self,
        at: UnixMs,
        project: Option<MessageId>,
        message: NewMessage,
    ) -> anyhow::Result<MessageId> {
        let pair = peer_pair(message.from, &message.mentions)
            .filter(|pair| pair[0] != pair[1])
            .ok_or_else(|| anyhow::anyhow!("direct mail needs two different agents"))?;
        if let Some(reply_to) = message.reply_to {
            anyhow::ensure!(is_message(self, reply_to), "no mail #{}", reply_to.0.0);
            if let Some((thread, addressed_pair)) = peer_thread(self, reply_to) {
                anyhow::ensure!(
                    pair == addressed_pair,
                    "address the same pair to reply to direct mail"
                );
                return self.post(at, Some(thread), message, None);
            }
        }
        // Standalone agents (e.g. from the CLI) still have durable mail.
        // Keep their conversations together rather than making each a project.
        let project = match project {
            Some(project) => project,
            None => unscoped_peer_project(self, at, message.from)?,
        };
        let scope = project.0.0;
        let children: Vec<_> = self
            .open_table(CHILDREN)
            .range((scope, 0)..=(scope, u64::MAX))
            .map(|(key, _)| key.value().1)
            .collect();
        // Read original postings so a deleted first mail still identifies
        // its conversation. Moves change which project's thread it is.
        let thread = children
            .into_iter()
            .map(|id| MessageId(ChatSeq(id)))
            .find(|id| direct_pair_at(self, *id) == Some(pair));
        let id = self.post(at, thread.or(Some(project)), message, None)?;
        if thread.is_none() {
            self.name(
                at,
                id,
                format!("{} ↔ {}", pair[0].encoded(), pair[1].encoded()),
            )?;
        }
        Ok(id)
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
        self.index_mail_event(seq, &event);
        Ok(())
    }

    fn delete(&mut self, at: UnixMs, agent: AgentId, message: MessageId) -> anyhow::Result<()> {
        check_revision(self, agent, message)?;
        let event = ChatEvent::Deleted { message };
        let seq = append(self, at, event.clone());
        self.index_mail_event(seq, &event);
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
        if peer_thread(self, thread).is_some() {
            return Ok(None);
        }
        // The durable stop mail itself is the deduplication record, including
        // across host re-exec, and if its message or thread is later moved.
        if self.open_table(MAIL_LOG).iter().rev().any(|(_, row)| {
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
                reply_to: None,
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
        self.index_mail_event(seq, &event);
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
        self.index_mail_event(seq, &event);
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
        self.index_mail_event(seq, &event);
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
        self.index_mail_event(seq, &event);
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

    fn index_mail_event(&mut self, seq: ChatSeq, event: &ChatEvent) {
        match event {
            ChatEvent::Posted {
                parent,
                from,
                mentions,
                ..
            } => {
                let receivers = recipients(*from, mentions, || members_under_write(self, *parent));
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
                index_first_join(self, *agent, *thread);
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

fn index_first_join(write: &mut WriteTxn, agent: AgentId, thread: MessageId) {
    let mut first = write.open_table(FIRST_THREAD);
    if first.get(agent).is_none() {
        first.insert(agent, thread.0.0);
    }
}

/// Existing posts and workset rows keep their original encoding. Seed the
/// first-thread projection from the same log old hosts wrote.
pub(crate) fn migrate_mail(write: &mut WriteTxn) {
    let joined = write
        .open_table(MAIL_LOG)
        .iter()
        .filter_map(|(_, row)| match row.value().into_owned().event {
            ChatEvent::Joined { thread, agent } => Some((agent, thread)),
            _ => None,
        })
        .collect::<Vec<_>>();
    for (agent, thread) in joined {
        index_first_join(write, agent, thread);
    }
}
