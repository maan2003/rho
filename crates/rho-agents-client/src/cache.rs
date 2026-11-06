//! The client's own copy of what agent hosts told it: every host's chat
//! log and its cursor, the agent directory, and the workset rows of the
//! transcripts the user opened.
//!
//! The rails are not folded from the network at startup: what an agent is
//! and each host's chat are read back from here, so the GUI comes up
//! already knowing them and asks a host only for the chat after its
//! cursor. A workset's rows are kept once read, so opening a transcript
//! again asks only for the rows after them.
//!
//! It is a copy, never a source, except for what the user said that no
//! host has: their unsent messages and their drafts. Anything else doubted
//! is thrown away and asked for again from the start.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use redb::{TableDefinition, TableHandle};
use rho_agent_types::{AgentId, ChatEvent, ChatRecord, ChatSeq, Message, MessageId, TopicId, Wpos};
use rho_db::{RhoDb, Sen, SenValue};

use crate::chat::HostTopic;
use crate::fold::AgentIdentity;
use crate::outbox::Outgoing;
use crate::protocol::worksets::LogEvent;

/// Where this client stands in a host's chat log, by the host's name. The
/// name rather than the host id: ids are handed out in attach order and
/// mean nothing across a restart. The seed says which database the cursor
/// counts in; an agent host with another one starts the copy over.
const HOSTS: TableDefinition<&str, Sen<StoredHost>> = TableDefinition::new("gui_chat_host_v1");
/// The directory as each host last told it.
const AGENTS: TableDefinition<AgentId, Sen<StoredAgent>> = TableDefinition::new("gui_agent_v1");
/// Every chat event but a message, by host and position: the streams,
/// the topics and who joined them.
const CHAT: TableDefinition<(&str, u64), Sen<ChatRecord>> = TableDefinition::new("gui_chat_v1");
/// Every message, by host, topic and position: a range read gives one
/// topic and nothing else.
const MESSAGES: TableDefinition<(&str, u64, u64), Sen<ChatRecord>> =
    TableDefinition::new("gui_chat_message_v1");
/// The messages each agent posted or received, by agent and position:
/// what its transcript reads. Written with the messages, as the fold
/// decided who they reached.
const DELIVERED: TableDefinition<(AgentId, u64), ()> = TableDefinition::new("gui_chat_agent_v2");
/// The topic each message is in now, by host and position: where a move
/// finds it, and where a link to it leads.
const WHERE: TableDefinition<(&str, u64), u64> = TableDefinition::new("gui_chat_where_v1");
/// The rows of each agent whose transcript was opened, by position.
const ROWS: TableDefinition<(AgentId, u64), Sen<LogEvent>> =
    TableDefinition::new("gui_agent_rows_v2");
/// What the user wrote that no host has taken yet, by the database of the
/// host it goes to and the id the client gave it. The user's own words,
/// with no copy anywhere else: never dropped to be asked for again.
const OUTBOX: TableDefinition<(u64, u64), Sen<Outgoing>> =
    TableDefinition::new("gui_chat_outbox_v1");
/// What the user is writing in each topic and has not sent, by the
/// topic's host database and id. Sending moves it to the outbox in the
/// same write, so a message is never in both or in neither.
const DRAFTS: TableDefinition<(u64, u64), &str> = TableDefinition::new("gui_topic_draft_v1");

#[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub(crate) struct StoredHost {
    machine_seed: u64,
    /// The newest chat event held.
    chat: ChatSeq,
}

/// One host as the copy holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirroredHost {
    pub name: String,
    pub machine_seed: u64,
    /// The newest chat event this client holds; what `FollowChat` sends.
    pub chat: ChatSeq,
}

/// One agent of the directory as the copy keeps it.
#[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub struct StoredAgent {
    /// The host's name.
    pub host: String,
    pub identity: AgentIdentity,
}

/// One agent as the copy holds it: whose it is and what it is. What its
/// messages fold to is folded again from the chat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedAgent {
    pub host: String,
    pub identity: AgentIdentity,
}

#[derive(Default)]
pub struct Loaded {
    pub hosts: Vec<MirroredHost>,
    pub agents: Vec<LoadedAgent>,
}

enum Write {
    /// A host's stream opened, on this database.
    Host {
        host: String,
        machine_seed: u64,
    },
    /// A run of a host's chat log, who each message reached, and the
    /// cursor it moves to. One transaction, so the cursor never claims
    /// events that are not there.
    Chat {
        host: String,
        through: ChatSeq,
        records: Vec<ChatRecord>,
        delivered: Vec<(AgentId, MessageId)>,
    },
    /// Directory entries as they now stand.
    Agents(Vec<(AgentId, StoredAgent)>),
    /// Agents gone from their host's directory, with what they received.
    Forget(Vec<AgentId>),
    Rows {
        agent: AgentId,
        rows: Vec<(Wpos, LogEvent)>,
    },
    /// A message kept until its host takes it, or `None` once it has.
    /// A new one in a topic was the topic's draft, which goes with it.
    Outgoing(u64, u64, Option<Outgoing>),
    /// A message kept, told where it now goes; no draft goes with it.
    Retarget(u64, u64, Outgoing),
    /// A topic's draft as it stands; empty drops it.
    Draft(HostTopic, String),
    /// Everything heard from a host, gone: its agent host has another
    /// database.
    Reset(String),
    Flush(mpsc::SyncSender<()>),
}

/// Opens a copied table, dropping it first if the file records it under
/// other key/value types: redb writes the Rust path of a value type into
/// the table, and everything in these is a copy the host still has.
fn open_or_rebuild<K, V>(write: &mut rho_db::WriteTxn, definition: TableDefinition<K, V>)
where
    K: redb::Key + 'static,
    V: redb::Value + 'static,
{
    let ours = write.try_open_table(definition).is_some();
    if !ours {
        write.delete_table(definition.name());
        write.open_table(definition);
    }
}

pub struct Mirror {
    db: RhoDb,
    /// Taken on drop: closing the channel is what tells the writer thread
    /// to finish, and the database file stays open until it has.
    sender: Option<mpsc::Sender<Write>>,
    writer: Option<std::thread::JoinHandle<()>>,
}

impl Mirror {
    /// Opens the client's database at `state_dir` and takes the copy's
    /// tables in it. For tests and tools; the session's own database is
    /// opened once by `main` at startup and handed to [`Mirror::open_on`].
    pub fn open(state_dir: &Path) -> std::io::Result<Self> {
        Self::open_on(rho_db::client::open(state_dir)?)
    }

    /// The copy's tables in a database somebody else opened. Every other
    /// kind of client state is in there too, under its own names; this
    /// touches the copy's and nothing else.
    pub fn open_on(db: RhoDb) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread().build()?;
        runtime.block_on(async {
            let mut write = db.write().await;
            open_or_rebuild(&mut write, HOSTS);
            open_or_rebuild(&mut write, AGENTS);
            open_or_rebuild(&mut write, CHAT);
            open_or_rebuild(&mut write, MESSAGES);
            open_or_rebuild(&mut write, DELIVERED);
            open_or_rebuild(&mut write, WHERE);
            // Temporary (`temp-migration`): the host's move renumbered the
            // worksets, so rows cached by their old positions are wrong. The
            // protocol epoch keeps this build off a host that has not moved.
            write.delete_table("gui_agent_rows_v1");
            open_or_rebuild(&mut write, ROWS);
            write.open_table(OUTBOX);
            write.open_table(DRAFTS);
            write.commit();
        });
        let (sender, receiver) = mpsc::channel();
        let writer_db = db.clone();
        // Off the UI thread: a frame must never wait on a commit.
        let handle = std::thread::Builder::new()
            .name("rho-agent-mirror".into())
            .spawn(move || writer(writer_db, receiver))?;
        Ok(Self {
            db,
            sender: Some(sender),
            writer: Some(handle),
        })
    }

    /// Every host's cursor and every agent: what the GUI starts from. No
    /// chat, which is read a host at a time as each attaches, and no
    /// workset rows, which are read when a transcript opens.
    pub fn load(&self) -> Loaded {
        let read = self.db.read();
        let hosts = read
            .open_table(HOSTS)
            .iter()
            .map(|(key, value)| {
                let stored = value.value().into_owned();
                MirroredHost {
                    name: key.value().to_owned(),
                    machine_seed: stored.machine_seed,
                    chat: stored.chat,
                }
            })
            .collect();
        let agents = read
            .open_table(AGENTS)
            .iter()
            .map(|(_, value)| {
                let stored = value.value().into_owned();
                LoadedAgent {
                    host: stored.host,
                    identity: stored.identity,
                }
            })
            .collect();
        Loaded { hosts, agents }
    }

    /// A host's whole chat log: every event but the messages in log order,
    /// then the messages a topic at a time, each topic's oldest first.
    pub fn read_chat(&self, host: &str) -> Vec<ChatRecord> {
        let read = self.db.read();
        let mut records = read
            .open_table(CHAT)
            .range((host, 0)..=(host, u64::MAX))
            .map(|(_, value)| value.value().into_owned())
            .collect::<Vec<_>>();
        records.extend(
            read.open_table(MESSAGES)
                .range((host, 0, 0)..=(host, u64::MAX, u64::MAX))
                .map(|(_, value)| value.value().into_owned()),
        );
        records
    }

    /// One topic's messages, oldest first, for the reader that opens it.
    pub fn read_topic(&self, host: &str, topic: TopicId) -> Vec<Message> {
        let topic = topic.0.0;
        self.db
            .read()
            .open_table(MESSAGES)
            .range((host, topic, 0)..=(host, topic, u64::MAX))
            .filter_map(|(_, value)| Message::from_record(&value.value().into_owned()))
            .collect()
    }

    /// The messages an agent posted or received, oldest first, for its
    /// transcript.
    pub fn read_agent_messages(&self, agent_id: AgentId) -> Vec<Message> {
        let read = self.db.read();
        let Some(host) = read
            .open_table(AGENTS)
            .get(agent_id)
            .map(|stored| stored.value().into_owned().host)
        else {
            return Vec::new();
        };
        let messages = read.open_table(MESSAGES);
        let index = read.open_table(WHERE);
        read.open_table(DELIVERED)
            .range((agent_id, 0)..=(agent_id, u64::MAX))
            .filter_map(|(key, _)| {
                let seq = key.value().1;
                let topic = index.get((host.as_str(), seq))?.value();
                let record = messages.get((host.as_str(), topic, seq))?;
                Message::from_record(&record.value().into_owned())
            })
            .collect()
    }

    /// The conversation a message is in now, if the copy holds it.
    pub fn message_topic(&self, host: &str, message: MessageId) -> Option<TopicId> {
        self.db
            .read()
            .open_table(WHERE)
            .get((host, message.0.0))
            .map(|topic| TopicId(ChatSeq(topic.value())))
    }

    /// The rows held of an agent, oldest first.
    pub fn read_rows(&self, agent: AgentId) -> Vec<(Wpos, LogEvent)> {
        self.db
            .read()
            .open_table(ROWS)
            .range((agent, 0)..=(agent, u64::MAX))
            .map(|(key, value)| (Wpos(key.value().1), value.value().into_owned()))
            .collect()
    }

    /// One past the newest row held of an agent: where reading its log
    /// goes on.
    pub fn rows_next(&self, agent: AgentId) -> Wpos {
        self.db
            .read()
            .open_table(ROWS)
            .range((agent, 0)..=(agent, u64::MAX))
            .next_back()
            .map_or(Wpos::ZERO, |(key, _)| Wpos(key.value().1).next())
    }

    pub fn read_drafts(&self) -> Vec<(HostTopic, String)> {
        self.db
            .read()
            .open_table(DRAFTS)
            .iter()
            .map(|(key, value)| {
                let (machine_seed, topic) = key.value();
                (
                    HostTopic {
                        machine_seed,
                        topic: TopicId(ChatSeq(topic)),
                    },
                    value.value().to_owned(),
                )
            })
            .collect()
    }

    /// Every unsent message, by the database of the host it goes to and
    /// its client id.
    pub fn read_outbox(&self) -> Vec<(u64, u64, Outgoing)> {
        self.db
            .read()
            .open_table(OUTBOX)
            .iter()
            .map(|(key, value)| {
                let (machine_seed, id) = key.value();
                (machine_seed, id, value.value().into_owned())
            })
            .collect()
    }

    /// Waits for everything already queued to commit.
    pub fn flush(&self) {
        let (send, receive) = mpsc::sync_channel(0);
        if let Some(sender) = self.sender.as_ref()
            && sender.send(Write::Flush(send)).is_ok()
        {
            let _ = receive.recv();
        }
    }

    fn send(&self, write: Write) {
        if self
            .sender
            .as_ref()
            .is_none_or(|sender| sender.send(write).is_err())
        {
            tracing::error!("agent mirror writer stopped");
        }
    }
}

impl Drop for Mirror {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

fn writer(db: RhoDb, receiver: mpsc::Receiver<Write>) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("build agent mirror runtime");
    // Everything queued goes in one transaction: a catch-up arrives as
    // many pages faster than each can commit on its own.
    while let Ok(first) = receiver.recv() {
        let mut batch = vec![first];
        while let Ok(next) = receiver.try_recv() {
            batch.push(next);
        }
        let mut flushed = Vec::new();
        runtime.block_on(async {
            let mut transaction = db.write().await;
            for write in batch {
                apply(&mut transaction, write, &mut flushed);
            }
            transaction.commit();
        });
        for done in flushed {
            let _ = done.send(());
        }
    }
}

fn apply(
    transaction: &mut rho_db::WriteTxn,
    write: Write,
    flushed: &mut Vec<mpsc::SyncSender<()>>,
) {
    match write {
        Write::Host { host, machine_seed } => {
            let mut hosts = transaction.open_table(HOSTS);
            let chat = hosts
                .get(host.as_str())
                .map(|stored| stored.value().into_owned())
                .filter(|stored| stored.machine_seed == machine_seed)
                .map_or(ChatSeq::ZERO, |stored| stored.chat);
            hosts.insert(
                host.as_str(),
                SenValue::borrowed(&StoredHost { machine_seed, chat }),
            );
        }
        Write::Chat {
            host,
            through,
            records,
            delivered,
        } => {
            // In log order: a move finds what it moves where an earlier
            // record of the run put it.
            for record in &records {
                match &record.event {
                    ChatEvent::Posted { topic, .. } => {
                        transaction.open_table(MESSAGES).insert(
                            (host.as_str(), topic.0.0, record.seq.0),
                            SenValue::borrowed(record),
                        );
                        transaction
                            .open_table(WHERE)
                            .insert((host.as_str(), record.seq.0), topic.0.0);
                    }
                    ChatEvent::Moved { messages, to } => {
                        move_messages(transaction, &host, messages, *to);
                    }
                    _ => {
                        transaction
                            .open_table(CHAT)
                            .insert((host.as_str(), record.seq.0), SenValue::borrowed(record));
                    }
                }
            }
            {
                let mut table = transaction.open_table(DELIVERED);
                for (agent_id, message) in &delivered {
                    table.insert((*agent_id, message.0.0), ());
                }
            }
            let mut hosts = transaction.open_table(HOSTS);
            if let Some(mut stored) = hosts
                .get(host.as_str())
                .map(|stored| stored.value().into_owned())
            {
                stored.chat = stored.chat.max(through);
                hosts.insert(host.as_str(), SenValue::borrowed(&stored));
            }
        }
        Write::Agents(agents) => {
            let mut table = transaction.open_table(AGENTS);
            for (agent_id, stored) in &agents {
                table.insert(agent_id, SenValue::borrowed(stored));
            }
        }
        Write::Forget(agents) => {
            for agent_id in agents {
                forget_agent(transaction, agent_id);
            }
        }
        Write::Rows { agent, rows } => {
            let mut table = transaction.open_table(ROWS);
            for (pos, event) in &rows {
                table.insert((agent, pos.0), SenValue::borrowed(event));
            }
        }
        Write::Outgoing(machine_seed, id, Some(outgoing)) => {
            transaction
                .open_table(OUTBOX)
                .insert((machine_seed, id), SenValue::borrowed(&outgoing));
            if let crate::outbox::Destination::Topic(topic) = outgoing.to {
                transaction
                    .open_table(DRAFTS)
                    .remove((machine_seed, topic.0.0));
            }
        }
        Write::Retarget(machine_seed, id, outgoing) => {
            transaction
                .open_table(OUTBOX)
                .insert((machine_seed, id), SenValue::borrowed(&outgoing));
        }
        Write::Outgoing(machine_seed, id, None) => {
            transaction.open_table(OUTBOX).remove((machine_seed, id));
        }
        Write::Draft(topic, text) => {
            let mut table = transaction.open_table(DRAFTS);
            let key = (topic.machine_seed, topic.topic.0.0);
            if text.is_empty() {
                table.remove(key);
            } else {
                table.insert(key, text.as_str());
            }
        }
        Write::Reset(host) => {
            transaction.open_table(HOSTS).remove(host.as_str());
            {
                let mut chat = transaction.open_table(CHAT);
                let held = chat
                    .range((host.as_str(), 0)..=(host.as_str(), u64::MAX))
                    .map(|(key, _)| key.value().1)
                    .collect::<Vec<_>>();
                for seq in held {
                    chat.remove((host.as_str(), seq));
                }
            }
            {
                let mut messages = transaction.open_table(MESSAGES);
                let held = messages
                    .range((host.as_str(), 0, 0)..=(host.as_str(), u64::MAX, u64::MAX))
                    .map(|(key, _)| {
                        let (_, topic, seq) = key.value();
                        (topic, seq)
                    })
                    .collect::<Vec<_>>();
                for (topic, seq) in held {
                    messages.remove((host.as_str(), topic, seq));
                }
            }
            {
                let mut index = transaction.open_table(WHERE);
                let held = index
                    .range((host.as_str(), 0)..=(host.as_str(), u64::MAX))
                    .map(|(key, _)| key.value().1)
                    .collect::<Vec<_>>();
                for seq in held {
                    index.remove((host.as_str(), seq));
                }
            }
            let departed = transaction
                .open_table(AGENTS)
                .iter()
                .map(|(key, value)| (key.value(), value.value().into_owned()))
                .filter(|(_, stored)| stored.host == host)
                .collect::<Vec<_>>();
            for (agent_id, _) in departed {
                forget_agent(transaction, agent_id);
            }
        }
        Write::Flush(done) => flushed.push(done),
    }
}

/// A move is kept as its outcome: each message rewritten into the topic it
/// is in now, so reading the copy back folds it there.
fn move_messages(transaction: &mut rho_db::WriteTxn, host: &str, ids: &[MessageId], to: TopicId) {
    for id in ids {
        let Some(from) = transaction
            .open_table(WHERE)
            .get((host, id.0.0))
            .map(|topic| topic.value())
        else {
            continue;
        };
        let mut messages = transaction.open_table(MESSAGES);
        let Some(mut record) = messages
            .remove((host, from, id.0.0))
            .map(|record| record.value().into_owned())
        else {
            continue;
        };
        if let ChatEvent::Posted { topic, .. } = &mut record.event {
            *topic = to;
        }
        messages.insert((host, to.0.0, id.0.0), SenValue::borrowed(&record));
        drop(messages);
        transaction.open_table(WHERE).insert((host, id.0.0), to.0.0);
    }
}

/// Only host-owned copies go away: the agent, what reached it, and its
/// rows. Drafts and outbox have no other copy, and the messages stay with
/// their topics.
fn forget_agent(transaction: &mut rho_db::WriteTxn, agent_id: AgentId) {
    transaction.open_table(AGENTS).remove(agent_id);
    let mut delivered = transaction.open_table(DELIVERED);
    let held = delivered
        .range((agent_id, 0)..=(agent_id, u64::MAX))
        .map(|(key, _)| key.value())
        .collect::<Vec<_>>();
    for key in held {
        delivered.remove(key);
    }
    drop(delivered);
    let mut rows = transaction.open_table(ROWS);
    let held = rows
        .range((agent_id, 0)..=(agent_id, u64::MAX))
        .map(|(key, _)| key.value())
        .collect::<Vec<_>>();
    for key in held {
        rows.remove(key);
    }
}

/// Held behind a lock only so that `close` can take it: every reader
/// takes the lock uncontended, and the writes go down a channel anyway.
static GLOBAL: std::sync::OnceLock<std::sync::RwLock<Option<Mirror>>> = std::sync::OnceLock::new();

fn global() -> std::sync::RwLockReadGuard<'static, Option<Mirror>> {
    GLOBAL
        .get_or_init(|| std::sync::RwLock::new(None))
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Where the session's state lives, said once by `main` at startup, for
/// the crates that keep files of their own beside the database.
static STATE_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
static CLOSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_state_dir(state_dir: PathBuf) {
    let _ = STATE_DIR.set(state_dir);
}

/// The client state directory `main` named, if it named one. Nothing but
/// `main` resolves it: a test never sets it, so a test can reach none of
/// the user's files through anything that asks here.
pub fn state_dir() -> Option<&'static Path> {
    STATE_DIR.get().map(PathBuf::as_path)
}

/// Takes the copy's tables in the client's database, which `main` opens
/// at startup before anything reads. Without it every write below is a
/// no-op and the GUI simply starts empty, which is what tests want.
pub fn init(db: RhoDb) -> std::io::Result<()> {
    let mirror = Mirror::open_on(db)?;
    let mut global = GLOBAL
        .get_or_init(|| std::sync::RwLock::new(None))
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if global.is_some() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "the agent mirror is already initialized",
        ));
    }
    // A quit that beat the open closes nothing and installs nothing: the
    // file this holds would otherwise outlive the session that asked for
    // it, and be found unclean next time.
    if CLOSED.load(std::sync::atomic::Ordering::Acquire) {
        return Ok(());
    }
    *global = Some(mirror);
    Ok(())
}

/// Drains the writer and closes the file, so that the next start finds it
/// shut cleanly and skips redb's rebuild.
pub fn close() {
    CLOSED.store(true, std::sync::atomic::Ordering::Release);
    let mirror = GLOBAL
        .get_or_init(|| std::sync::RwLock::new(None))
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    drop(mirror);
}

fn send(write: Write) {
    if let Some(mirror) = global().as_ref() {
        mirror.send(write);
    }
}

fn read<T: Default>(read: impl FnOnce(&Mirror) -> T) -> T {
    global().as_ref().map(read).unwrap_or_default()
}

pub fn load() -> Loaded {
    read(Mirror::load)
}

pub fn read_chat(host: &str) -> Vec<ChatRecord> {
    read(|mirror| mirror.read_chat(host))
}

pub fn read_topic(host: &str, topic: TopicId) -> Vec<Message> {
    read(|mirror| mirror.read_topic(host, topic))
}

pub fn message_topic(host: &str, message: MessageId) -> Option<TopicId> {
    read(|mirror| mirror.message_topic(host, message))
}

pub fn read_agent_messages(agent_id: AgentId) -> Vec<Message> {
    read(|mirror| mirror.read_agent_messages(agent_id))
}

pub fn read_rows(agent: AgentId) -> Vec<(Wpos, LogEvent)> {
    read(|mirror| mirror.read_rows(agent))
}

pub fn rows_next(agent: AgentId) -> Wpos {
    read(|mirror| mirror.rows_next(agent))
}

pub fn write_host(host: &str, machine_seed: u64) {
    send(Write::Host {
        host: host.to_owned(),
        machine_seed,
    });
}

pub fn write_chat(
    host: &str,
    through: ChatSeq,
    records: Vec<ChatRecord>,
    delivered: Vec<(AgentId, MessageId)>,
) {
    send(Write::Chat {
        host: host.to_owned(),
        through,
        records,
        delivered,
    });
}

pub fn write_agents(agents: Vec<(AgentId, StoredAgent)>) {
    if !agents.is_empty() {
        send(Write::Agents(agents));
    }
}

pub fn forget_agents(agents: Vec<AgentId>) {
    if !agents.is_empty() {
        send(Write::Forget(agents));
    }
}

pub fn write_rows(agent: AgentId, rows: Vec<(Wpos, LogEvent)>) {
    if !rows.is_empty() {
        send(Write::Rows { agent, rows });
    }
}

pub fn write_outgoing(machine_seed: u64, id: u64, outgoing: Option<Outgoing>) {
    send(Write::Outgoing(machine_seed, id, outgoing));
}

pub fn retarget_outgoing(machine_seed: u64, id: u64, outgoing: Outgoing) {
    send(Write::Retarget(machine_seed, id, outgoing));
}

pub fn write_draft(topic: HostTopic, text: String) {
    send(Write::Draft(topic, text));
}

pub fn read_drafts() -> Vec<(HostTopic, String)> {
    read(Mirror::read_drafts)
}

pub fn read_outbox() -> Vec<(u64, u64, Outgoing)> {
    read(Mirror::read_outbox)
}

pub fn reset_host(host: &str) {
    send(Write::Reset(host.to_owned()));
}

pub fn flush() {
    if let Some(mirror) = global().as_ref() {
        mirror.flush();
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{
        Address, ContentPart, Mention, MentionKind, Role, StreamId, UnixMs, WorksetId,
    };

    use super::*;
    use crate::outbox::Destination;
    use crate::protocol::transcript::RuntimeKind;
    use crate::protocol::worksets::{AgentEntry, InputView, SpecView};

    fn agent_id(counter: u64) -> AgentId {
        AgentId::from_counter(counter, &rho_agent_types::AgentIdDomain(7)).expect("agent id")
    }

    fn workset(n: u8) -> WorksetId {
        WorksetId([n; 6])
    }

    fn stored(agent: AgentId, host: &str, workset: WorksetId) -> StoredAgent {
        StoredAgent {
            host: host.into(),
            identity: AgentIdentity {
                entry: AgentEntry {
                    agent,
                    workset,
                    spec: SpecView {
                        runtime: RuntimeKind::Rho,
                        model: "sol".into(),
                        effort: "medium".into(),
                        props: Default::default(),
                    },
                    created_at: UnixMs(1),
                },
                kind: rho_agent_types::WorksetKind::Isolated,
            },
        }
    }

    fn record(seq: u64, event: ChatEvent) -> ChatRecord {
        ChatRecord {
            seq: ChatSeq(seq),
            at: UnixMs(seq),
            event,
        }
    }

    fn topic(seq: u64) -> TopicId {
        TopicId(ChatSeq(seq))
    }

    fn started(seq: u64) -> ChatRecord {
        record(
            seq,
            ChatEvent::TopicStarted {
                stream: StreamId(ChatSeq(1)),
                name: format!("topic {seq}"),
                by: Address::User,
            },
        )
    }

    fn posted(seq: u64, topic_seq: u64, from: Address) -> ChatRecord {
        record(
            seq,
            ChatEvent::Posted {
                topic: topic(topic_seq),
                from,
                mentions: vec![Mention {
                    to: Address::User,
                    kind: MentionKind::Ask,
                }],
                content: vec![ContentPart::Text {
                    text: format!("message {seq}"),
                }],
            },
        )
    }

    fn input(agent: AgentId) -> LogEvent {
        LogEvent::Request {
            agent,
            inputs: vec![InputView {
                message: None,
                text: "go".into(),
                role: Role::System,
            }],
            calls: Vec::new(),
            compaction: false,
            at: UnixMs(1),
        }
    }

    fn ids(messages: &[Message]) -> Vec<u64> {
        messages.iter().map(|message| message.id.0.0).collect()
    }

    /// What the copy reads back is what it was told: the cursor, the
    /// directory, the chat a topic at a time with the events that made the
    /// topics first, each topic's messages alone, what each agent posted
    /// or received, and a workset's rows with where reading goes on.
    #[test]
    fn a_restart_reads_back_what_was_heard() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (agent_id(1), agent_id(2));
        {
            let mirror = Mirror::open(dir.path()).unwrap();
            mirror.send(Write::Host {
                host: "fern".into(),
                machine_seed: 9,
            });
            mirror.send(Write::Agents(vec![
                (a, stored(a, "fern", workset(1))),
                (b, stored(b, "fern", workset(1))),
            ]));
            mirror.send(Write::Chat {
                host: "fern".into(),
                through: ChatSeq(6),
                records: vec![
                    record(1, ChatEvent::StreamCreated { name: "rho".into() }),
                    started(2),
                    started(3),
                    // Interleaved in the log, kept apart by topic.
                    posted(4, 3, Address::Agent(b)),
                    posted(5, 2, Address::Agent(a)),
                    posted(6, 3, Address::User),
                ],
                delivered: vec![
                    (b, MessageId(ChatSeq(4))),
                    (a, MessageId(ChatSeq(5))),
                    (b, MessageId(ChatSeq(6))),
                ],
            });
            mirror.send(Write::Rows {
                agent: a,
                rows: vec![(Wpos(0), input(a)), (Wpos(2), input(a))],
            });
            mirror.flush();
        }
        let mirror = Mirror::open(dir.path()).unwrap();
        let loaded = mirror.load();
        assert_eq!(
            loaded.hosts,
            vec![MirroredHost {
                name: "fern".into(),
                machine_seed: 9,
                chat: ChatSeq(6),
            }]
        );
        assert_eq!(loaded.agents.len(), 2);
        let order = mirror
            .read_chat("fern")
            .iter()
            .map(|record| record.seq.0)
            .collect::<Vec<_>>();
        assert_eq!(order, [1, 2, 3, 5, 4, 6], "a topic at a time");
        assert!(mirror.read_chat("oak").is_empty());
        assert_eq!(ids(&mirror.read_topic("fern", topic(3))), [4, 6]);
        assert_eq!(ids(&mirror.read_topic("fern", topic(2))), [5]);
        assert_eq!(ids(&mirror.read_agent_messages(b)), [4, 6]);
        assert_eq!(ids(&mirror.read_agent_messages(a)), [5]);
        assert_eq!(mirror.read_rows(a).len(), 2);
        assert_eq!(mirror.rows_next(a), Wpos(3));
        assert_eq!(mirror.rows_next(b), Wpos::ZERO);
    }

    /// A move is kept where the messages now are: the topics read back
    /// without them or with them, a link finds them, an agent's transcript
    /// keeps them, and the copy folds back to what the live fold made.
    #[test]
    fn a_move_is_kept_where_the_messages_now_are() {
        let dir = tempfile::tempdir().unwrap();
        let b = agent_id(2);
        let first = vec![
            record(1, ChatEvent::StreamCreated { name: "rho".into() }),
            started(2),
            started(3),
            posted(4, 3, Address::Agent(b)),
            posted(5, 2, Address::User),
            posted(6, 3, Address::Agent(b)),
        ];
        // 4 joins 2, and 6 a topic started for it at 7, in one run with
        // a message posted after.
        let second = vec![
            record(
                7,
                ChatEvent::Moved {
                    messages: vec![MessageId(ChatSeq(4))],
                    to: topic(2),
                },
            ),
            started(8),
            record(
                9,
                ChatEvent::Moved {
                    messages: vec![MessageId(ChatSeq(6))],
                    to: topic(8),
                },
            ),
            posted(10, 3, Address::Agent(b)),
        ];
        let mut live = crate::chat::HostChat::default();
        for record in first.iter().chain(&second) {
            live.tell(record);
        }
        {
            let mirror = Mirror::open(dir.path()).unwrap();
            mirror.send(Write::Host {
                host: "fern".into(),
                machine_seed: 9,
            });
            mirror.send(Write::Agents(vec![(b, stored(b, "fern", workset(1)))]));
            mirror.send(Write::Chat {
                host: "fern".into(),
                through: ChatSeq(6),
                records: first,
                delivered: vec![(b, MessageId(ChatSeq(4))), (b, MessageId(ChatSeq(6)))],
            });
            mirror.send(Write::Chat {
                host: "fern".into(),
                through: ChatSeq(10),
                records: second,
                delivered: vec![(b, MessageId(ChatSeq(10)))],
            });
            mirror.flush();
        }
        let mirror = Mirror::open(dir.path()).unwrap();
        assert_eq!(ids(&mirror.read_topic("fern", topic(2))), [4, 5]);
        assert_eq!(ids(&mirror.read_topic("fern", topic(3))), [10]);
        assert_eq!(ids(&mirror.read_topic("fern", topic(8))), [6]);
        assert!(
            mirror
                .read_topic("fern", topic(2))
                .iter()
                .all(|message| message.topic == topic(2)),
            "a moved message names the topic it is in now"
        );
        assert_eq!(
            mirror.message_topic("fern", MessageId(ChatSeq(6))),
            Some(topic(8))
        );
        assert_eq!(
            mirror.message_topic("fern", MessageId(ChatSeq(5))),
            Some(topic(2))
        );
        assert_eq!(mirror.message_topic("oak", MessageId(ChatSeq(5))), None);
        assert_eq!(ids(&mirror.read_agent_messages(b)), [4, 6, 10]);
        let mut again = crate::chat::HostChat::default();
        for record in mirror.read_chat("fern") {
            again.tell(&record);
        }
        assert_eq!(again.topics, live.topics);
        assert_eq!(again.topics[&topic(2)].others, [MessageId(ChatSeq(4))]);
        assert_eq!(again.topics[&topic(3)].others, [MessageId(ChatSeq(10))]);
        assert_eq!(
            again.topics[&topic(3)].newest.map(|(id, _)| id.0.0),
            Some(10)
        );
    }

    /// A host with another database starts its copy over; the user's own
    /// words, and another host's copy, stay.
    #[test]
    fn a_reset_drops_one_hosts_copy_and_keeps_the_users_words() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (agent_id(1), agent_id(2));
        let mirror = Mirror::open(dir.path()).unwrap();
        for host in ["fern", "oak"] {
            mirror.send(Write::Host {
                host: host.into(),
                machine_seed: 1,
            });
            mirror.send(Write::Chat {
                host: host.into(),
                through: ChatSeq(2),
                records: vec![started(1), posted(2, 1, Address::User)],
                delivered: Vec::new(),
            });
        }
        mirror.send(Write::Agents(vec![
            (a, stored(a, "fern", workset(1))),
            (b, stored(b, "oak", workset(2))),
        ]));
        mirror.send(Write::Rows {
            agent: a,
            rows: vec![(Wpos(0), input(a))],
        });
        let draft = HostTopic {
            machine_seed: 1,
            topic: topic(1),
        };
        mirror.send(Write::Draft(draft, "half written".into()));
        mirror.send(Write::Rows {
            agent: b,
            rows: vec![(Wpos(0), input(b))],
        });
        mirror.send(Write::Outgoing(
            1,
            7,
            Some(Outgoing {
                at: UnixMs(1),
                mentions: Vec::new(),
                content: vec![ContentPart::Text {
                    text: "kept".into(),
                }],
                to: Destination::Topic(topic(2)),
            }),
        ));
        mirror.send(Write::Reset("fern".into()));
        // The same host again on another database: its cursor starts over.
        mirror.send(Write::Host {
            host: "oak".into(),
            machine_seed: 2,
        });
        mirror.flush();
        let loaded = mirror.load();
        assert_eq!(loaded.hosts.len(), 1);
        assert_eq!(loaded.hosts[0].chat, ChatSeq(0));
        assert_eq!(
            loaded
                .agents
                .iter()
                .map(|loaded| loaded.identity.agent_id())
                .collect::<Vec<_>>(),
            [b]
        );
        assert!(mirror.read_chat("fern").is_empty());
        assert_eq!(
            mirror.read_chat("oak").len(),
            2,
            "another host's copy stays"
        );
        assert!(mirror.read_rows(a).is_empty());
        assert_eq!(mirror.read_rows(b).len(), 1, "another host's rows stay");
        assert_eq!(mirror.read_outbox().len(), 1, "unsent words are the user's");
        assert_eq!(mirror.read_drafts(), [(draft, "half written".to_owned())]);
    }

    /// Sending a message in a topic takes its draft in the same write, so
    /// the words are never in both or in neither.
    #[test]
    fn sending_takes_the_topics_draft() {
        let dir = tempfile::tempdir().unwrap();
        let mirror = Mirror::open(dir.path()).unwrap();
        let draft = |n| HostTopic {
            machine_seed: 1,
            topic: topic(n),
        };
        mirror.send(Write::Draft(draft(2), "to send".into()));
        mirror.send(Write::Draft(draft(3), "another topic".into()));
        mirror.send(Write::Outgoing(
            1,
            7,
            Some(Outgoing {
                at: UnixMs(1),
                mentions: Vec::new(),
                content: Vec::new(),
                to: Destination::Topic(topic(2)),
            }),
        ));
        mirror.flush();
        assert_eq!(
            mirror.read_drafts(),
            [(draft(3), "another topic".to_owned())]
        );
    }
}
