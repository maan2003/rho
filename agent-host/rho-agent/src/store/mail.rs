//! The host's chat ([`rho_agent_types::ChatEvent`]): one log, dense from
//! one, and the indexes folded from it in the commit of every append.
//!
//! The log holds a forest of messages. Each message keeps its parent now
//! ([`PARENT`]) and each parent its children ([`CHILDREN`]), so a thread is
//! read as a range and a move rewrites two rows. Membership is explicit per
//! thread ([`MEMBERS`]) and read up the chain of parents: the nearest
//! record for an agent decides, so a join on a root is a bot in a channel
//! and a leave on a subthread cuts that branch. The project's words to the
//! user are readable in their thread but never delivered to agents. The
//! user's words and one agent's words to another go to their recipients'
//! inboxes when written, so delivery reads only its own inbox.

use redb::TableDefinition;
use rho_agent_types::{
    Address, AgentId, ChatEvent, ChatRecord, ChatSeq, ContentPart, FLASH_LIFETIME, Message,
    MessageId, ProjectPlace, Revision, UnixMs, Voice,
};
use rho_db::{ReadTxn, Sen, SenValue, WriteTxn};
use senax_encoder::{Decode, Encode};

use super::{Appended, ProjectRead as _, ProjectWrite as _, tell};

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
/// copying its body. The original posting still owns the identity.
const REVISIONS: TableDefinition<u64, u64> = TableDefinition::new("chat_revisions");

const AGENT_MAIL_PROJECT: &str = "Agent mail";

/// The log as hosts wrote it before a post had one voice (format
/// `7c2a5f9d`). Only the one-hop store migration reads it; remove it with
/// that hop.
mod prior_voice {
    use rho_agent_types::ThreadReply;

    use super::{Address, AgentId, ChatSeq, ContentPart, Decode, Encode, MessageId, UnixMs};

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
    pub(super) enum Mention {
        User { ping: bool },
        Agent(AgentId),
        Direct(AgentId),
    }

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
    pub(super) enum MessageKind {
        #[default]
        Kept,
        Flash,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    pub(super) enum ChatEvent {
        Posted {
            parent: Option<MessageId>,
            reply_to: Option<MessageId>,
            thread_reply: Option<ThreadReply>,
            from: Address,
            #[senax(default)]
            kind: MessageKind,
            mentions: Vec<Mention>,
            content: Vec<ContentPart>,
        },
        Reparented {
            message: MessageId,
            parent: Option<MessageId>,
        },
        Joined {
            thread: MessageId,
            agent: AgentId,
        },
        Left {
            thread: MessageId,
            agent: AgentId,
        },
        Named {
            thread: MessageId,
            name: String,
        },
        Edited {
            message: MessageId,
            content: Vec<ContentPart>,
        },
        Deleted {
            message: MessageId,
        },
        Revised {
            message: MessageId,
            author: AgentId,
            content: Vec<ContentPart>,
            ping: Option<bool>,
        },
        TakenBack {
            message: MessageId,
            version: ChatSeq,
            author: AgentId,
        },
        Recovered {
            message: MessageId,
            content: Vec<ContentPart>,
        },
    }

    // Sen's table type name is the unqualified Rust name; this opens the
    // same table with the old row shape.
    #[derive(Clone, Debug, Encode, Decode)]
    pub(super) struct ChatRecord {
        pub seq: ChatSeq,
        pub at: UnixMs,
        pub event: ChatEvent,
    }

    /// The project settings as hosts kept them beside the log.
    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    pub(super) struct ProjectSettings {
        pub place: rho_agent_types::ProjectPlace,
        pub routing: Routing,
        #[senax(default)]
        pub runtime: rho_agent_types::RuntimeLabel,
    }

    #[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
    pub(super) enum Routing {
        One { agent: Option<AgentId> },
        PerIdea,
    }
}

/// One hop, `7c2a5f9d` to the voiced log: every post gets the one voice its
/// mentions and ping meant, and each project's settings move from the
/// sidecar `mail_projects` table into the log as a [`ChatEvent::Project`].
///
/// A message that ended kept but silent (`ping: false`, the shape
/// `product/chat.md` rejected) becomes a flash, which expires as any old
/// flash does, unless it anchors something: a root, or a thread right under
/// one, stays a message so the work under it keeps its head. A root that
/// had no settings and is not the peer container becomes a project at the
/// place of the agent that made it or first worked under it, else `~`.
pub(crate) fn migrate_voices(
    write: &mut WriteTxn,
    place_of: impl Fn(AgentId) -> Option<ProjectPlace>,
) {
    use prior_voice::{ChatEvent as Old, Mention as OldMention, MessageKind as OldKind};
    use redb::TableHandle as _;
    let old = TableDefinition::<u64, Sen<prior_voice::ChatRecord>>::new(MAIL_LOG.name());
    let rows: Vec<prior_voice::ChatRecord> = write
        .open_table(old)
        .iter()
        .map(|(_, row)| row.value().into_owned())
        .collect();
    // The ping each message ended with, after its revisions.
    let mut pinging = std::collections::HashMap::new();
    let mut parents = std::collections::HashMap::new();
    let mut has_children = std::collections::HashSet::new();
    let mut start_failures = std::collections::HashSet::new();
    // Roots the old rule kept out of the projects: addressed ones.
    let mut addressed = std::collections::HashSet::new();
    for row in &rows {
        match &row.event {
            Old::Posted {
                parent,
                from,
                mentions,
                ..
            } => {
                let ping = mentions
                    .iter()
                    .any(|m| *m == OldMention::User { ping: true });
                pinging.insert(row.seq, ping);
                parents.insert(row.seq, *parent);
                if let Some(parent) = parent {
                    has_children.insert(parent.0);
                }
                if *from == Address::StartFailed {
                    start_failures.insert(row.seq);
                }
                if parent.is_none() && !mentions.is_empty() {
                    addressed.insert(row.seq.0);
                }
            }
            Old::Revised {
                message,
                ping: Some(ping),
                ..
            } => {
                pinging.insert(message.0, *ping);
            }
            _ => {}
        }
    }
    let anchors = |seq: ChatSeq| {
        has_children.contains(&seq)
            || match parents.get(&seq).copied().flatten() {
                None => true,
                Some(parent) => parents.get(&parent.0).copied().flatten().is_none(),
            }
    };
    let mut table = write.open_table(MAIL_LOG);
    for row in rows {
        let event = match row.event {
            Old::Posted {
                parent,
                reply_to,
                thread_reply,
                from,
                kind,
                mentions,
                content,
            } => {
                let direct = mentions.iter().find_map(|mention| match mention {
                    OldMention::Direct(agent) | OldMention::Agent(agent) => Some(*agent),
                    OldMention::User { .. } => None,
                });
                let voice = match (from, kind, direct) {
                    (_, OldKind::Flash, _) => Voice::Flash,
                    (Address::Agent(_), OldKind::Kept, Some(to)) => Voice::ToAgent(to),
                    (Address::Agent(_), OldKind::Kept, None)
                        if !pinging[&row.seq] && !anchors(row.seq) =>
                    {
                        Voice::Flash
                    }
                    _ => Voice::Message,
                };
                // A user's reply that named one message settled exactly
                // that version; a retry of a failed start keeps its target.
                let (retry_of, thread_reply) = match (from, reply_to, thread_reply) {
                    (Address::User, Some(target), None) if start_failures.contains(&target.0) => {
                        (Some(target), None)
                    }
                    (Address::User, Some(target), None) => (
                        None,
                        Some(rho_agent_types::ThreadReply {
                            settles: Some(rho_agent_types::MessageVersion {
                                message: target,
                                version: row.seq,
                            }),
                        }),
                    ),
                    (_, _, thread_reply) => (None, thread_reply),
                };
                ChatEvent::Posted {
                    parent,
                    retry_of,
                    thread_reply,
                    from,
                    voice,
                    content,
                }
            }
            Old::Reparented { message, parent } => ChatEvent::Reparented { message, parent },
            Old::Joined { thread, agent } => ChatEvent::Joined { thread, agent },
            Old::Left { thread, agent } => ChatEvent::Left { thread, agent },
            Old::Named { thread, name } => ChatEvent::Named { thread, name },
            Old::Edited { message, content } => ChatEvent::Edited { message, content },
            Old::Deleted { message } => ChatEvent::Deleted { message },
            Old::Revised {
                message,
                author,
                content,
                ..
            } => ChatEvent::Revised {
                message,
                author,
                content,
            },
            Old::TakenBack {
                message,
                version,
                author,
            } => ChatEvent::TakenBack {
                message,
                version,
                author,
            },
            Old::Recovered { message, content } => ChatEvent::Recovered { message, content },
        };
        table.insert(
            row.seq.0,
            SenValue::borrowed(&ChatRecord {
                seq: row.seq,
                at: row.at,
                event,
            }),
        );
    }
    drop(table);

    // Settings: the sidecar table's rows, then every other root the old
    // rule called a project.
    const OLD_PROJECTS: &str = "mail_projects";
    let settings: Vec<(u64, prior_voice::ProjectSettings)> = {
        let old = TableDefinition::<u64, Sen<prior_voice::ProjectSettings>>::new(OLD_PROJECTS);
        let rows = write
            .open_table(old)
            .iter()
            .map(|(id, row)| (id.value(), row.value().into_owned()))
            .collect();
        write.delete_table(old.name());
        rows
    };
    let at = UnixMs::now();
    let mut configured = std::collections::HashSet::new();
    for (root, settings) in settings {
        let project = rho_agent_types::ProjectId(MessageId(ChatSeq(root)));
        if !is_message(write, project.0) {
            continue;
        }
        let mode = match settings.routing {
            prior_voice::Routing::One { agent } => {
                if let Some(agent) = agent {
                    write.set_standing_agent(project, agent);
                }
                rho_agent_types::ProjectMode::One
            }
            prior_voice::Routing::PerIdea => rho_agent_types::ProjectMode::PerIdea,
        };
        configured.insert(root);
        write.set_project(
            at,
            project,
            rho_agent_types::ProjectConfig {
                place: settings.place,
                mode,
                runtime: settings.runtime,
            },
        );
    }
    let roots: Vec<u64> = write
        .open_table(CHILDREN)
        .range((ROOT, 0)..=(ROOT, u64::MAX))
        .map(|(key, _)| key.value().1)
        .collect();
    let first_joined: Vec<(AgentId, u64)> = write
        .open_table(FIRST_THREAD)
        .iter()
        .map(|(agent, thread)| (agent.value(), thread.value()))
        .collect();
    for root in roots {
        let id = MessageId(ChatSeq(root));
        if configured.contains(&root) {
            continue;
        }
        let Some(anchor) = current_message(write, id) else {
            continue;
        };
        let named = write.open_table(NAMES).get(root).map(|name| name.value());
        if anchor.revision == Revision::Deleted
            || addressed.contains(&root)
            || named.unwrap_or_else(|| anchor.headline()) == AGENT_MAIL_PROJECT
        {
            continue;
        }
        let creator = match anchor.from {
            Address::Agent(agent) => Some(agent),
            _ => None,
        };
        let worker = first_joined
            .iter()
            .filter(|(_, thread)| root_of_write(write, MessageId(ChatSeq(*thread))) == id)
            .map(|(agent, _)| *agent);
        let place = creator
            .into_iter()
            .chain(worker)
            .find_map(&place_of)
            .unwrap_or_else(|| ProjectPlace::parse("~").expect("a place"));
        write.set_project(
            at,
            rho_agent_types::ProjectId(id),
            rho_agent_types::ProjectConfig {
                place,
                mode: rho_agent_types::ProjectMode::One,
                runtime: rho_agent_types::RuntimeLabel::default(),
            },
        );
    }
}

fn root_of_write(write: &mut WriteTxn, mut message: MessageId) -> MessageId {
    while let Some(parent) = parent_now(write, message) {
        message = parent;
    }
    message
}

/// The root of the forest in the children index.
const ROOT: u64 = 0;

/// A message as its sender gives it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewMessage {
    pub from: Address,
    pub voice: Voice,
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
    /// The host's newest project with this name.
    fn project_named(&self, name: &str) -> Option<MessageId>;
    fn posted_by_client(&self, client_id: u64) -> Option<MessageId>;
    fn search_mail(
        &self,
        query: &str,
        project: Option<MessageId>,
        limit: usize,
    ) -> anyhow::Result<Vec<MailHit>>;
    fn mail_thread(&self, message: &Message) -> MessageId;
    fn first_thread(&self, agent: AgentId) -> Option<MessageId>;
    fn flash_deadlines(&self) -> Vec<(MessageId, UnixMs)>;
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
            apply_revision(&mut message, &record, |seq| {
                self.open_table(MAIL_LOG)
                    .get(seq.0)
                    .expect("mail version")
                    .value()
                    .into_owned()
            });
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
            .filter(|message| {
                !matches!(message.revision, Revision::Deleted | Revision::TakenBack { .. })
                    // Older inbox indexes can still contain the project's
                    // words to the user.
                    && !user_facing(message.from, message.voice)
            })
            .collect()
    }

    fn recipients_of(&self, message: &Message) -> Vec<AgentId> {
        recipients(message.from, message.voice, || {
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
                    && self.project(rho_agent_types::ProjectId(root.id)).is_some()
                    && names
                        .get(root.id.0.0)
                        .map(|name| name.value())
                        .unwrap_or_else(|| root.headline())
                        .eq_ignore_ascii_case(&name)
            })
            .map(|root| root.id)
    }

    fn posted_by_client(&self, client_id: u64) -> Option<MessageId> {
        self.open_table(CLIENT_IDS)
            .get(client_id)
            .map(|row| MessageId(ChatSeq(row.value())))
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

    fn flash_deadlines(&self) -> Vec<(MessageId, UnixMs)> {
        let log = self.open_table(MAIL_LOG);
        let revisions = self.open_table(REVISIONS);
        log.iter()
            .filter_map(|(_, row)| {
                let record = row.value().into_owned();
                if !matches!(
                    record.event,
                    ChatEvent::Posted {
                        voice: Voice::Flash,
                        ..
                    }
                ) {
                    return None;
                }
                if revisions.get(record.seq.0).is_some_and(|seq| {
                    matches!(
                        log.get(seq.value())
                            .expect("a revision is a chat event")
                            .value()
                            .into_owned()
                            .event,
                        ChatEvent::Deleted { .. }
                    )
                }) {
                    return None;
                }
                Some((MessageId(record.seq), record.at + FLASH_LIFETIME))
            })
            .collect()
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

/// The project's words speak to the user, not to every agent reading the
/// thread.
fn user_facing(sender: Address, voice: Voice) -> bool {
    match sender {
        Address::System { .. } | Address::StartFailed => true,
        Address::Agent(_) => !matches!(voice, Voice::ToAgent(_)),
        Address::User => false,
    }
}

/// Delivery is fixed by the record, not by which process happens to be
/// running: the user's words reach the thread's members, and an agent's
/// words to another reach that agent alone.
fn recipients(
    sender: Address,
    voice: Voice,
    members: impl FnOnce() -> Vec<AgentId>,
) -> Vec<AgentId> {
    match (sender, voice) {
        (Address::User, _) => members(),
        (Address::Agent(from), Voice::ToAgent(to)) if from != to => vec![to],
        _ => Vec::new(),
    }
}

fn peer_pair(from: Address, voice: Voice) -> Option<[AgentId; 2]> {
    let (Address::Agent(from), Voice::ToAgent(to)) = (from, voice) else {
        return None;
    };
    let mut pair = [from, to];
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
    /// A user reply uses thread membership; its card mark is not routing.
    fn post_user_reply(
        &mut self,
        at: UnixMs,
        parent: Option<MessageId>,
        reply: rho_agent_types::ThreadReply,
        content: Vec<ContentPart>,
        client_id: u64,
    ) -> anyhow::Result<MessageId>;
    /// An idea the user writes under a project: a message, logged once per
    /// `client_id`, and a retry of a failed start when it names one.
    fn post_idea(
        &mut self,
        at: UnixMs,
        thread: MessageId,
        retry_of: Option<MessageId>,
        content: Vec<ContentPart>,
        client_id: u64,
    ) -> anyhow::Result<MessageId>;
    fn post_flash(
        &mut self,
        at: UnixMs,
        thread: MessageId,
        author: AgentId,
        content: Vec<ContentPart>,
    ) -> anyhow::Result<MessageId>;
    /// Apply the original post's deadline even after edits or a host restart.
    fn expire_flash(&mut self, at: UnixMs, id: MessageId) -> bool;
    /// The host hands an existing message to an agent it did not reach when
    /// posted: a project's standing agent, or the agent a retry started.
    /// Its words and position do not change.
    fn deliver(&mut self, agent: AgentId, message: MessageId);
    /// One agent's words to another, under a project. Their first message
    /// is the anchor; later messages between the same pair go under it.
    fn post_peer(
        &mut self,
        at: UnixMs,
        project: Option<MessageId>,
        message: NewMessage,
    ) -> anyhow::Result<MessageId>;
    /// The agent's live message in this thread since the user's last one.
    fn live_project_mail(&mut self, thread: MessageId, agent: AgentId) -> Option<MessageId>;
    fn mail_version(&mut self, mail: MessageId) -> Option<ChatSeq>;
    /// Mark a still-current version taken back, without erasing its words.
    fn take_back_mail(
        &mut self,
        at: UnixMs,
        author: AgentId,
        mail: MessageId,
        version: Option<ChatSeq>,
    );
    /// Settle a stuck streak's durable stop when the model answered again.
    fn recover_stuck(&mut self, at: UnixMs, author: AgentId, since: UnixMs);

    /// Change an agent's own words, except a message the user answered.
    /// The voice stays as it was posted.
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
    /// Makes a root a project, or changes its settings. Logs nothing when
    /// they are what the project already has.
    fn set_project(
        &mut self,
        at: UnixMs,
        project: rho_agent_types::ProjectId,
        config: rho_agent_types::ProjectConfig,
    );
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
    let ChatEvent::Posted { from, voice, .. } = row.event else {
        return None;
    };
    peer_pair(from, voice)
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
            voice: Voice::Message,
            content: vec![ContentPart::Text {
                text: AGENT_MAIL_PROJECT.into(),
            }],
        },
        None,
    )
}

fn has_user_reply(write: &mut WriteTxn, mail: &Message) -> bool {
    // A dealt card keeps its identity if either message moves. Older
    // clients could answer a specific mail by posting directly under it.
    write
        .open_table(MAIL_LOG)
        .range(mail.id.0.0 + 1..)
        .any(|(_, row)| {
            let ChatEvent::Posted {
                from: Address::User,
                parent,
                retry_of,
                thread_reply,
                ..
            } = row.value().into_owned().event
            else {
                return false;
            };
            match thread_reply {
                Some(reply) => reply.settles.is_some_and(|seen| {
                    seen.message == mail.id && seen.version >= mail.version().0
                }),
                None => retry_of.or(parent) == Some(mail.id),
            }
        })
}

fn apply_revision(
    mail: &mut Message,
    revision: &ChatRecord,
    read: impl FnOnce(ChatSeq) -> ChatRecord,
) {
    if let ChatEvent::TakenBack { version, .. } = revision.event
        && version != mail.id.0
    {
        mail.apply_revision(&read(version));
    }
    mail.apply_revision(revision);
}

fn current_message(write: &mut WriteTxn, id: MessageId) -> Option<Message> {
    let record = write.open_table(MAIL_LOG).get(id.0.0)?.value().into_owned();
    let mut message = Message::from_record(&record)?;
    let revision = write
        .open_table(REVISIONS)
        .get(id.0.0)
        .map(|seq| seq.value());
    if let Some(seq) = revision {
        let record = write
            .open_table(MAIL_LOG)
            .get(seq)
            .expect("a revision is a mail event")
            .value()
            .into_owned();
        apply_revision(&mut message, &record, |seq| {
            write
                .open_table(MAIL_LOG)
                .get(seq.0)
                .expect("mail version")
                .value()
                .into_owned()
        });
    }
    message.parent = parent_now(write, id);
    Some(message)
}

fn check_revision(write: &mut WriteTxn, agent: AgentId, id: MessageId) -> anyhow::Result<Message> {
    let mail =
        current_message(write, id).ok_or_else(|| anyhow::anyhow!("no message #{}", id.0.0))?;
    anyhow::ensure!(
        mail.from == Address::Agent(agent),
        "you can only change your own words"
    );
    anyhow::ensure!(
        !matches!(
            mail.revision,
            Revision::Deleted | Revision::TakenBack { .. }
        ),
        "#{} was deleted or taken back",
        id.0.0
    );
    anyhow::ensure!(
        mail.voice == Voice::Flash || !has_user_reply(write, &mail),
        "you cannot change #{}: the user already answered it",
        id.0.0
    );
    Ok(mail)
}

fn revise(
    write: &mut WriteTxn,
    at: UnixMs,
    mail: &Message,
    author: AgentId,
    content: Vec<ContentPart>,
) {
    if mail.content == content {
        return;
    }
    let event = ChatEvent::Revised {
        message: mail.id,
        author,
        content,
    };
    let seq = append(write, at, event.clone());
    write.index_mail_event(seq, &event);
}

fn post_kind(
    write: &mut WriteTxn,
    at: UnixMs,
    parent: Option<MessageId>,
    message: NewMessage,
    client_id: Option<u64>,
    retry_of: Option<MessageId>,
    thread_reply: Option<rho_agent_types::ThreadReply>,
) -> anyhow::Result<MessageId> {
    if let Some(seq) = sent_before(write, client_id) {
        return Ok(MessageId(seq));
    }
    if let Some(parent) = parent {
        anyhow::ensure!(is_message(write, parent), "no message {}", parent.0.0);
    }
    anyhow::ensure!(has_content(&message.content), "a message needs content");
    let NewMessage {
        from,
        voice,
        content,
    } = message;
    if let Some(retry_of) = retry_of {
        anyhow::ensure!(is_message(write, retry_of), "no message #{}", retry_of.0.0);
    }
    if let Some(parent) = parent
        && let Some((_, pair)) = peer_thread(write, parent)
        && let Address::Agent(agent) = from
    {
        anyhow::ensure!(
            peer_pair(from, voice) == Some(pair) && pair.contains(&agent),
            "write to the other agent to reply in a conversation between agents"
        );
    }
    let event = ChatEvent::Posted {
        parent,
        retry_of,
        thread_reply,
        from,
        voice,
        content,
    };
    let seq = append(write, at, event.clone());
    write.index_mail_event(seq, &event);
    if let Some(client_id) = client_id {
        write.open_table(CLIENT_IDS).insert(client_id, seq.0);
    }
    if let Some(thread) = parent {
        expire_older_flashes(write, at, thread, MessageId(seq));
    }
    Ok(MessageId(seq))
}

fn mark_deleted(write: &mut WriteTxn, at: UnixMs, message: MessageId) {
    let event = ChatEvent::Deleted { message };
    let seq = append(write, at, event.clone());
    write.index_mail_event(seq, &event);
}

fn earlier_messages(write: &mut WriteTxn, thread: MessageId, after: MessageId) -> Vec<MessageId> {
    write
        .open_table(CHILDREN)
        .range((thread.0.0, 0)..(thread.0.0, after.0.0))
        .map(|(key, _)| MessageId(ChatSeq(key.value().1)))
        .collect()
}

// SPEC-chat-flash: a new item and any flash deletion commit as one log update.
fn expire_older_flashes(write: &mut WriteTxn, at: UnixMs, thread: MessageId, newer: MessageId) {
    for id in earlier_messages(write, thread, newer) {
        if flash_due(write, at, id).is_some() {
            mark_deleted(write, at, id);
        }
    }
}

fn flash_due(write: &mut WriteTxn, at: UnixMs, id: MessageId) -> Option<MessageId> {
    let flash = current_message(write, id)?;
    flash.flash_deadline().filter(|deadline| *deadline <= at)?;
    flash.parent
}

fn newer_visible(write: &mut WriteTxn, thread: MessageId, than: MessageId) -> bool {
    let newer: Vec<_> = write
        .open_table(CHILDREN)
        .range((thread.0.0, than.0.next().0)..=(thread.0.0, u64::MAX))
        .rev()
        .map(|(key, _)| MessageId(ChatSeq(key.value().1)))
        .collect();
    newer.into_iter().any(|id| {
        current_message(write, id).is_some_and(|message| message.revision != Revision::Deleted)
    })
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
        post_kind(self, at, parent, message, client_id, None, None)
    }

    fn post_user_reply(
        &mut self,
        at: UnixMs,
        parent: Option<MessageId>,
        reply: rho_agent_types::ThreadReply,
        content: Vec<ContentPart>,
        client_id: u64,
    ) -> anyhow::Result<MessageId> {
        post_kind(
            self,
            at,
            parent,
            NewMessage {
                from: Address::User,
                voice: Voice::Message,
                content,
            },
            Some(client_id),
            None,
            Some(reply),
        )
    }

    fn post_idea(
        &mut self,
        at: UnixMs,
        thread: MessageId,
        retry_of: Option<MessageId>,
        content: Vec<ContentPart>,
        client_id: u64,
    ) -> anyhow::Result<MessageId> {
        post_kind(
            self,
            at,
            Some(thread),
            NewMessage {
                from: Address::User,
                voice: Voice::Message,
                content,
            },
            Some(client_id),
            retry_of,
            None,
        )
    }

    fn post_flash(
        &mut self,
        at: UnixMs,
        thread: MessageId,
        author: AgentId,
        content: Vec<ContentPart>,
    ) -> anyhow::Result<MessageId> {
        anyhow::ensure!(
            current_message(self, thread).is_some_and(|anchor| {
                anchor.voice == Voice::Message
                    && anchor.parent.is_some()
                    && anchor.revision != Revision::Deleted
            }),
            "a flash needs a thread"
        );
        let message = NewMessage {
            from: Address::Agent(author),
            voice: Voice::Flash,
            content,
        };
        post_kind(self, at, Some(thread), message, None, None, None)
    }

    fn expire_flash(&mut self, at: UnixMs, id: MessageId) -> bool {
        if let Some(thread) = flash_due(self, at, id)
            && newer_visible(self, thread, id)
        {
            mark_deleted(self, at, id);
            return true;
        }
        false
    }

    fn deliver(&mut self, agent: AgentId, message: MessageId) {
        self.open_table(INBOX).insert((agent, message.0.0), ());
        // The sequence did not move, but the workset can read new inbox mail.
        tell(self, Appended::Mail(message.0));
    }

    fn post_peer(
        &mut self,
        at: UnixMs,
        project: Option<MessageId>,
        message: NewMessage,
    ) -> anyhow::Result<MessageId> {
        let pair = peer_pair(message.from, message.voice)
            .filter(|pair| pair[0] != pair[1])
            .ok_or_else(|| anyhow::anyhow!("a word to an agent needs two different agents"))?;
        // Standalone agents (e.g. from the CLI) still keep their words.
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

    fn live_project_mail(&mut self, thread: MessageId, agent: AgentId) -> Option<MessageId> {
        // SPEC-live-project-mail: the same transaction checks the writer's
        // slot and appends the new message. Thread membership is the current
        // parent, but a top-level thread starts with its opening message.
        let anchor = current_message(self, thread)?;
        if anchor.parent.is_none() {
            return None; // a project, not an exchange
        }
        let mut candidates: Vec<_> = self
            .open_table(CHILDREN)
            .range((thread.0.0, 0)..=(thread.0.0, u64::MAX))
            .rev()
            .map(|(key, _)| MessageId(ChatSeq(key.value().1)))
            .collect();
        if anchor
            .parent
            .is_none_or(|parent| parent_now(self, parent).is_none())
        {
            candidates.insert(candidates.partition_point(|id| *id > thread), thread);
        }
        for id in candidates {
            let mail = current_message(self, id).expect("a child is a message");
            if mail.from == Address::User {
                break;
            }
            if mail.from == Address::Agent(agent)
                && mail.voice == Voice::Message
                && !matches!(
                    mail.revision,
                    Revision::Deleted | Revision::TakenBack { .. }
                )
                && !has_user_reply(self, &mail)
            {
                return Some(id);
            }
        }
        None
    }

    fn mail_version(&mut self, mail: MessageId) -> Option<ChatSeq> {
        current_message(self, mail).map(|mail| mail.version().0)
    }

    fn take_back_mail(
        &mut self,
        at: UnixMs,
        author: AgentId,
        mail: MessageId,
        version: Option<ChatSeq>,
    ) {
        let Some(current) = current_message(self, mail) else {
            return;
        };
        if current.from != Address::Agent(author)
            || matches!(
                current.revision,
                Revision::Deleted | Revision::TakenBack { .. }
            )
            || version.is_some_and(|version| version != current.version().0)
        {
            return;
        }
        let version = self
            .open_table(REVISIONS)
            .get(mail.0.0)
            .map_or(mail.0, |found| ChatSeq(found.value()));
        let event = ChatEvent::TakenBack {
            message: mail,
            version,
            author,
        };
        let seq = append(self, at, event.clone());
        self.index_mail_event(seq, &event);
    }

    fn recover_stuck(&mut self, at: UnixMs, author: AgentId, since: UnixMs) {
        let sender = Address::System {
            agent: author,
            stop: rho_agent_types::Stop::Stuck,
            since,
        };
        let id = self.open_table(MAIL_LOG).iter().rev().find_map(|(key, row)| {
            matches!(row.value().into_owned().event, ChatEvent::Posted { from, .. } if from == sender)
                .then(|| MessageId(ChatSeq(key.value())))
        });
        let Some(mail) = id.and_then(|id| current_message(self, id)) else {
            return;
        };
        if !mail.pings_user() || has_user_reply(self, &mail) {
            return;
        }
        let event = ChatEvent::Recovered {
            message: mail.id,
            content: vec![ContentPart::Text {
                text: "The model answered again. Work carried on.".into(),
            }],
        };
        let seq = append(self, at, event.clone());
        self.index_mail_event(seq, &event);
    }

    fn edit(
        &mut self,
        at: UnixMs,
        agent: AgentId,
        message: MessageId,
        content: Vec<ContentPart>,
    ) -> anyhow::Result<()> {
        let mail = check_revision(self, agent, message)?;
        anyhow::ensure!(has_content(&content), "a message needs content");
        revise(self, at, &mail, agent, content);
        Ok(())
    }

    fn delete(&mut self, at: UnixMs, agent: AgentId, message: MessageId) -> anyhow::Result<()> {
        check_revision(self, agent, message)?;
        mark_deleted(self, at, message);
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
                voice: Voice::Message,
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
        if let Some(thread) = parent
            && current_message(self, message).is_some_and(|item| item.revision != Revision::Deleted)
        {
            expire_older_flashes(self, at, thread, message);
            self.expire_flash(at, message);
        }
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

    fn set_project(
        &mut self,
        at: UnixMs,
        project: rho_agent_types::ProjectId,
        config: rho_agent_types::ProjectConfig,
    ) {
        if self.held_project(project).as_ref() == Some(&config) {
            return;
        }
        let event = ChatEvent::Project { project, config };
        let seq = append(self, at, event.clone());
        self.index_mail_event(seq, &event);
    }

    fn index_mail_event(&mut self, seq: ChatSeq, event: &ChatEvent) {
        match event {
            ChatEvent::Posted {
                parent,
                from,
                voice,
                ..
            } => {
                let receivers = recipients(*from, *voice, || members_under_write(self, *parent));
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
            ChatEvent::Edited { message, .. }
            | ChatEvent::Revised { message, .. }
            | ChatEvent::TakenBack { message, .. }
            | ChatEvent::Recovered { message, .. }
            | ChatEvent::Deleted { message } => {
                self.open_table(REVISIONS).insert(message.0.0, seq.0);
            }
            ChatEvent::Project { project, config } => {
                self.index_project(*project, config);
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

#[cfg(test)]
pub(crate) mod voice_migration_tests {
    use rho_agent_types::{
        AgentIdDomain, MessageVersion, ProjectConfig, ProjectId, ProjectMode, RuntimeLabel,
        ThreadReply,
    };

    use super::prior_voice::{
        ChatEvent as Old, ChatRecord as OldRecord, Mention as OldMention, MessageKind as OldKind,
        ProjectSettings, Routing,
    };
    use super::*;
    use crate::store::StoreWrite as _;
    use crate::store::projects::ProjectRead as _;

    fn words(text: &str) -> Vec<ContentPart> {
        vec![ContentPart::Text { text: text.into() }]
    }

    /// A post the old way: indexes built by today's `post`, and the log
    /// row in the old shape, which [`old_rows`] writes once every post is
    /// in.
    struct OldPost {
        parent: Option<u64>,
        reply_to: Option<u64>,
        from: Address,
        kind: OldKind,
        mentions: Vec<OldMention>,
        text: &'static str,
    }

    fn old_post(write: &mut WriteTxn, rows: &mut Vec<(ChatSeq, Old)>, post: OldPost) -> u64 {
        let id = write
            .post(
                UnixMs(1),
                post.parent.map(|parent| MessageId(ChatSeq(parent))),
                NewMessage {
                    from: post.from,
                    voice: Voice::Message,
                    content: words(post.text),
                },
                None,
            )
            .unwrap();
        rows.push((
            id.0,
            Old::Posted {
                parent: post.parent.map(|parent| MessageId(ChatSeq(parent))),
                reply_to: post.reply_to.map(|target| MessageId(ChatSeq(target))),
                thread_reply: None,
                from: post.from,
                kind: post.kind,
                mentions: post.mentions,
                content: words(post.text),
            },
        ));
        id.0.0
    }

    fn old_rows(write: &mut WriteTxn, rows: Vec<(ChatSeq, Old)>) {
        let table = TableDefinition::<u64, Sen<OldRecord>>::new("chat_log");
        let mut table = write.open_table(table);
        for (seq, event) in rows {
            table.insert(
                seq.0,
                SenValue::borrowed(&OldRecord {
                    seq,
                    at: UnixMs(1),
                    event,
                }),
            );
        }
    }

    /// A root written as a build before voices wrote it, for the store's
    /// own upgrade test.
    pub(crate) fn old_root(write: &mut WriteTxn, from: Address, text: &'static str) -> MessageId {
        let mut rows = Vec::new();
        let root = old_post(
            write,
            &mut rows,
            OldPost {
                parent: None,
                reply_to: None,
                from,
                kind: OldKind::Kept,
                mentions: Vec::new(),
                text,
            },
        );
        old_rows(write, rows);
        MessageId(ChatSeq(root))
    }

    /// Every way an old post maps to one voice, the reply fields, and the
    /// projects: settings from the sidecar table, a settingless root at
    /// the place of the agent that made it or at `~`, and no project for
    /// the peer container or an addressed root.
    #[tokio::test]
    async fn the_hop_gives_each_post_one_voice_and_each_project_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(dir.path().join("db"));
        let (a, b) = (
            AgentId::from_counter(1, &AgentIdDomain(0)).unwrap(),
            AgentId::from_counter(2, &AgentIdDomain(0)).unwrap(),
        );
        let user = |parent, text| OldPost {
            parent,
            reply_to: None,
            from: Address::User,
            kind: OldKind::Kept,
            mentions: Vec::new(),
            text,
        };
        let from_a = |parent, kind, mentions, text| OldPost {
            parent,
            reply_to: None,
            from: Address::Agent(a),
            kind,
            mentions,
            text,
        };
        let ping = |ping| vec![OldMention::User { ping }];
        let mut write = db.write().await;
        write.init_store_tables();
        let mut rows = Vec::new();
        let rho = old_post(&mut write, &mut rows, user(None, "rho"));
        let thread = old_post(&mut write, &mut rows, user(Some(rho), "Fix the clock"));
        let asked = old_post(
            &mut write,
            &mut rows,
            from_a(Some(thread), OldKind::Kept, ping(true), "Ask"),
        );
        let quiet = old_post(
            &mut write,
            &mut rows,
            from_a(Some(thread), OldKind::Kept, ping(false), "fyi"),
        );
        let flash = old_post(
            &mut write,
            &mut rows,
            from_a(Some(thread), OldKind::Flash, ping(false), "busy"),
        );
        let peer = old_post(
            &mut write,
            &mut rows,
            from_a(
                Some(thread),
                OldKind::Kept,
                vec![OldMention::Direct(b)],
                "b, help",
            ),
        );
        let answer = old_post(
            &mut write,
            &mut rows,
            OldPost {
                reply_to: Some(asked),
                ..user(Some(thread), "Yes")
            },
        );
        let lab = old_post(
            &mut write,
            &mut rows,
            from_a(None, OldKind::Kept, Vec::new(), "Clock lab"),
        );
        let lab_thread = old_post(
            &mut write,
            &mut rows,
            from_a(Some(lab), OldKind::Kept, ping(false), "Bench"),
        );
        let failed = old_post(
            &mut write,
            &mut rows,
            OldPost {
                from: Address::StartFailed,
                ..from_a(Some(thread), OldKind::Kept, ping(true), "Could not start")
            },
        );
        let retry = old_post(
            &mut write,
            &mut rows,
            OldPost {
                reply_to: Some(failed),
                ..user(Some(thread), "Try again")
            },
        );
        let peers = old_post(&mut write, &mut rows, user(None, AGENT_MAIL_PROJECT));
        let addressed = old_post(
            &mut write,
            &mut rows,
            OldPost {
                mentions: vec![OldMention::Agent(a)],
                ..user(None, "Restart fixture")
            },
        );
        let ops = old_post(&mut write, &mut rows, user(None, "ops"));
        // The ask's last revision stopped its ping: it ends a quiet fyi.
        rows.push((
            ChatSeq(ops + 1),
            Old::Revised {
                message: MessageId(ChatSeq(asked)),
                author: a,
                content: words("Ask, settled"),
                ping: Some(false),
            },
        ));
        write.open_table(REVISIONS).insert(asked, ops + 1);
        old_rows(&mut write, rows);
        let settings = TableDefinition::<u64, Sen<ProjectSettings>>::new("mail_projects");
        write.open_table(settings).insert(
            rho,
            SenValue::borrowed(&ProjectSettings {
                place: ProjectPlace::parse("~/src/rho").unwrap(),
                routing: Routing::One { agent: Some(b) },
                runtime: RuntimeLabel::parse("sol-high").unwrap(),
            }),
        );

        migrate_voices(&mut write, |agent| {
            (agent == a).then(|| ProjectPlace::parse("~/src/clock").unwrap())
        });
        write.commit();

        let read = db.read();
        let voice = |seq: u64| read.message(MessageId(ChatSeq(seq))).unwrap().voice;
        assert_eq!(voice(rho), Voice::Message);
        assert_eq!(voice(asked), Voice::Flash, "its last ping was false");
        assert_eq!(voice(quiet), Voice::Flash);
        assert_eq!(voice(flash), Voice::Flash);
        assert_eq!(voice(peer), Voice::ToAgent(b));
        assert_eq!(voice(lab), Voice::Message, "a root anchors its project");
        assert_eq!(
            voice(lab_thread),
            Voice::Message,
            "a thread anchors its work"
        );
        assert_eq!(voice(failed), Voice::Message);
        let message = |seq: u64| read.message(MessageId(ChatSeq(seq))).unwrap();
        assert_eq!(
            (message(answer).retry_of, message(answer).thread_reply),
            (
                None,
                Some(ThreadReply {
                    settles: Some(MessageVersion {
                        message: MessageId(ChatSeq(asked)),
                        version: ChatSeq(answer),
                    }),
                }),
            )
        );
        assert_eq!(
            (message(retry).retry_of, message(retry).thread_reply),
            (Some(MessageId(ChatSeq(failed))), None)
        );
        assert_eq!(message(asked).text(), "Ask, settled");

        let project = |seq: u64| read.project(ProjectId(MessageId(ChatSeq(seq))));
        let config = |place: &str, mode, runtime: &str| ProjectConfig {
            place: ProjectPlace::parse(place).unwrap(),
            mode,
            runtime: RuntimeLabel::parse(runtime).unwrap(),
        };
        let usual = RuntimeLabel::default();
        assert_eq!(
            project(rho),
            Some(config("~/src/rho", ProjectMode::One, "sol-high"))
        );
        assert_eq!(
            read.standing_agent(ProjectId(MessageId(ChatSeq(rho)))),
            Some(b)
        );
        assert_eq!(
            project(lab),
            Some(config("~/src/clock", ProjectMode::One, usual.as_str()))
        );
        assert_eq!(
            project(ops),
            Some(config("~", ProjectMode::One, usual.as_str()))
        );
        assert_eq!(project(peers), None);
        assert_eq!(project(addressed), None);
        assert_eq!(project(thread), None);
        // The settings live in the log now, once each.
        let logged = read
            .mail_since(ChatSeq::ZERO, usize::MAX)
            .into_iter()
            .filter(|record| matches!(record.event, ChatEvent::Project { .. }))
            .count();
        assert_eq!(logged, 3);
        let mut write = db.write().await;
        assert!(
            !write.delete_table("mail_projects"),
            "the sidecar table is gone"
        );
    }
}
