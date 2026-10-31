//! The client's own copy of what agent hosts told it: every host's mail
//! log and its cursor, the agent directory, and the workset rows of the
//! transcripts the user opened.
//!
//! The rails are not folded from the network at startup: what an agent is
//! and its mail are read back from here, so the GUI comes up already
//! knowing them and asks a host only for the mail after its cursor. A
//! workset's rows are kept once read, so opening a transcript again asks
//! only for the rows after them.
//!
//! It is a copy, never a source, except for what the user said that no
//! host has: their verdicts, their unsent mail and their drafts. Anything
//! else doubted is thrown away and asked for again from the start.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use redb::{TableDefinition, TableHandle};
use rho_agent_types::{Address, AgentId, Mail, MailId, WorksetId, Wpos};
use rho_db::{RhoDb, Sen, SenValue};

use crate::Verdict;
use crate::fold::{AgentIdentity, Digest, Unread};
use crate::outbox::Outgoing;
use crate::protocol::rho2::LogEvent;

/// Where this client stands in a host's mail log, by the host's name. The
/// name rather than the host id: ids are handed out in attach order and
/// mean nothing across a restart. The seed says which database the cursor
/// counts in; an agent host with another one starts the copy over.
const HOSTS: TableDefinition<&str, Sen<StoredHost>> = TableDefinition::new("gui_host_v1");
/// The directory as each host last told it, with the stop the client last
/// saw of each agent.
const AGENTS: TableDefinition<AgentId, Sen<StoredAgent>> = TableDefinition::new("gui_agent_v1");
/// Every mail, under each agent it is from or to, in log order: a range
/// read gives one agent's conversation and nothing else. Mail between two
/// agents is kept under both.
const MAIL: TableDefinition<(AgentId, u64), Sen<Mail>> = TableDefinition::new("gui_mail_v1");
/// The rows of each workset whose transcript was opened, by position.
const ROWS: TableDefinition<([u8; 6], u64), Sen<LogEvent>> =
    TableDefinition::new("gui_workset_rows_v1");
/// What the user last said about an agent, so Home ranks the same way on
/// the first frame as it did before the restart: attention is derived
/// from this and the digest. The store overwrites it as soon as the GUI
/// has it again.
const VERDICTS: TableDefinition<AgentId, Sen<Verdict>> =
    TableDefinition::new("gui_agent_verdict_v2");
/// What the user wrote that no host has taken yet, by agent and the id
/// the client gave it. The user's own words, with no copy anywhere else:
/// never dropped to be asked for again.
const OUTBOX: TableDefinition<(AgentId, u64), Sen<Outgoing>> =
    TableDefinition::new("gui_agent_outbox_v1");
/// What the user is writing to each agent and has not sent. Sending moves
/// it to the outbox in the same write, so a message is never in both or
/// in neither.
const DRAFTS: TableDefinition<AgentId, &str> = TableDefinition::new("gui_agent_draft_v1");

/// Tables nothing reads: the per-agent journal mirror rho2 replaced, and
/// its retired formats. Dropped on open, every open.
const RETIRED_TABLES: [&str; 36] = [
    "gui_mirror_host_v10",
    "gui_mirror_events_v10",
    "gui_agent_digest_v7",
    "gui_agent_host_v5",
    // Verdicts counted journal positions; rho2 counts the conversation.
    "gui_agent_verdict_v1",
    "gui_mirror_host_v9",
    "gui_mirror_events_v9",
    "gui_agent_digest_v6",
    "gui_mirror_host_v8",
    "gui_mirror_events_v8",
    "gui_agent_digest_v5",
    "gui_mirror_host_v7",
    "gui_mirror_events_v7",
    "gui_mirror_host_v6",
    "gui_mirror_events_v6",
    "gui_agent_host_v4",
    "gui_agent_digest_v4",
    "gui_agent_host_v2",
    "gui_agent_digest_v2",
    "gui_mirror_host_v4",
    "gui_mirror_events_v4",
    "gui_agent_head_v1",
    "gui_agent_story_v1",
    "gui_agent_attention_v1",
    "gui_mirror_events_v1",
    "gui_mirror_host_v1",
    "gui_mirror_events_v2",
    "gui_mirror_host_v2",
    "gui_mirror_host_v3",
    "gui_mirror_events_v3",
    "gui_agent_host_v1",
    "gui_agent_digest_v1",
    "gui_mirror_host_v5",
    "gui_mirror_events_v5",
    "gui_agent_host_v3",
    "gui_agent_digest_v3",
];

#[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub(crate) struct StoredHost {
    machine_seed: u64,
    /// The newest mail held.
    mail: MailId,
}

/// One host as the copy holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirroredHost {
    pub name: String,
    pub machine_seed: u64,
    /// The newest mail this client holds; what `FollowMail` sends.
    pub mail: MailId,
}

/// One agent of the directory as the copy keeps it.
#[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub struct StoredAgent {
    /// The host's name.
    pub host: String,
    pub identity: AgentIdentity,
    /// The stop the client last saw and the user has not answered, which
    /// no mail records.
    pub stop: Option<Unread>,
}

/// One agent as the copy holds it: whose it is, what it is, what its mail
/// folds to, and what the user last said about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedAgent {
    pub host: String,
    pub identity: AgentIdentity,
    pub digest: Digest,
    pub verdict: Option<Verdict>,
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
    /// A run of a host's mail log, and the cursor it moves to. One
    /// transaction, so the cursor never claims mail that is not there.
    Mail {
        host: String,
        through: MailId,
        mails: Vec<Mail>,
    },
    /// Directory entries as they now stand.
    Agents(Vec<(AgentId, StoredAgent)>),
    /// Agents gone from their host's directory, with their mail.
    Forget(Vec<AgentId>),
    Rows {
        workset: WorksetId,
        rows: Vec<(Wpos, LogEvent)>,
    },
    /// A workset gone from its host, with its rows.
    ForgetWorkset(WorksetId),
    Verdict(AgentId, Verdict),
    /// A message kept until its host takes it, or `None` once it has.
    /// A new one was the agent's draft, which goes with it.
    Outgoing(AgentId, u64, Option<Outgoing>),
    /// An agent's draft as it stands; empty drops it.
    Draft(AgentId, String),
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
            open_or_rebuild(&mut write, MAIL);
            open_or_rebuild(&mut write, ROWS);
            write.open_table(VERDICTS);
            write.open_table(OUTBOX);
            write.open_table(DRAFTS);
            for table in RETIRED_TABLES {
                write.delete_table(table);
            }
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

    /// Every host's cursor and every agent, its mail folded: what the GUI
    /// starts from. No workset rows; those are read when a transcript
    /// opens.
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
                    mail: stored.mail,
                }
            })
            .collect();
        let verdicts = read.open_table(VERDICTS);
        let mail = read.open_table(MAIL);
        let agents = read
            .open_table(AGENTS)
            .iter()
            .map(|(key, value)| {
                let agent_id = key.value();
                let stored = value.value().into_owned();
                let mut digest = Digest::default();
                for (_, mail) in mail.range((agent_id, 0)..=(agent_id, u64::MAX)) {
                    digest.tell(agent_id, &mail.value().into_owned());
                }
                if let Some(stop) = stored.stop {
                    digest.restore_stop(stop);
                }
                LoadedAgent {
                    host: stored.host,
                    identity: stored.identity,
                    digest,
                    verdict: verdicts
                        .get(agent_id)
                        .map(|value| value.value().into_owned()),
                }
            })
            .collect();
        Loaded { hosts, agents }
    }

    /// One agent's mail, oldest first, for the transcript a reader opens.
    pub fn read_mail(&self, agent_id: AgentId) -> Vec<Mail> {
        self.db
            .read()
            .open_table(MAIL)
            .range((agent_id, 0)..=(agent_id, u64::MAX))
            .map(|(_, value)| value.value().into_owned())
            .collect()
    }

    /// The rows held of a workset, oldest first.
    pub fn read_rows(&self, workset: WorksetId) -> Vec<(Wpos, LogEvent)> {
        self.db
            .read()
            .open_table(ROWS)
            .range((workset.0, 0)..=(workset.0, u64::MAX))
            .map(|(key, value)| (Wpos(key.value().1), value.value().into_owned()))
            .collect()
    }

    /// One past the newest row held of a workset: where reading it goes on.
    pub fn rows_next(&self, workset: WorksetId) -> Wpos {
        self.db
            .read()
            .open_table(ROWS)
            .range((workset.0, 0)..=(workset.0, u64::MAX))
            .next_back()
            .map_or(Wpos::ZERO, |(key, _)| Wpos(key.value().1).next())
    }

    pub fn read_drafts(&self) -> Vec<(AgentId, String)> {
        self.db
            .read()
            .open_table(DRAFTS)
            .iter()
            .map(|(key, value)| (key.value(), value.value().to_owned()))
            .collect()
    }

    pub fn read_outbox(&self) -> Vec<(AgentId, u64, Outgoing)> {
        self.db
            .read()
            .open_table(OUTBOX)
            .iter()
            .map(|(key, value)| {
                let (agent_id, id) = key.value();
                (agent_id, id, value.value().into_owned())
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

/// The agents a mail is kept under.
fn parties(mail: &Mail) -> impl Iterator<Item = AgentId> {
    let agent = |address| match address {
        Address::Agent(agent) => Some(agent),
        Address::User => None,
    };
    let from = agent(mail.from);
    let to = agent(mail.to).filter(|to| Some(*to) != from);
    from.into_iter().chain(to)
}

fn apply(
    transaction: &mut rho_db::WriteTxn,
    write: Write,
    flushed: &mut Vec<mpsc::SyncSender<()>>,
) {
    match write {
        Write::Host { host, machine_seed } => {
            let mut hosts = transaction.open_table(HOSTS);
            let mail = hosts
                .get(host.as_str())
                .map(|stored| stored.value().into_owned())
                .filter(|stored| stored.machine_seed == machine_seed)
                .map_or(MailId::default(), |stored| stored.mail);
            hosts.insert(
                host.as_str(),
                SenValue::borrowed(&StoredHost { machine_seed, mail }),
            );
        }
        Write::Mail {
            host,
            through,
            mails,
        } => {
            {
                let mut table = transaction.open_table(MAIL);
                for mail in &mails {
                    for agent in parties(mail) {
                        table.insert((agent, mail.id.0), SenValue::borrowed(mail));
                    }
                }
            }
            let mut hosts = transaction.open_table(HOSTS);
            if let Some(mut stored) = hosts
                .get(host.as_str())
                .map(|stored| stored.value().into_owned())
            {
                stored.mail = stored.mail.max(through);
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
        Write::Rows { workset, rows } => {
            let mut table = transaction.open_table(ROWS);
            for (pos, event) in &rows {
                table.insert((workset.0, pos.0), SenValue::borrowed(event));
            }
        }
        Write::ForgetWorkset(workset) => drop_rows(transaction, workset),
        Write::Verdict(agent_id, verdict) => {
            transaction
                .open_table(VERDICTS)
                .insert(agent_id, SenValue::borrowed(&verdict));
        }
        Write::Outgoing(agent_id, id, Some(outgoing)) => {
            transaction
                .open_table(OUTBOX)
                .insert((agent_id, id), SenValue::borrowed(&outgoing));
            transaction.open_table(DRAFTS).remove(agent_id);
        }
        Write::Outgoing(agent_id, id, None) => {
            transaction.open_table(OUTBOX).remove((agent_id, id));
        }
        Write::Draft(agent_id, text) => {
            let mut table = transaction.open_table(DRAFTS);
            if text.is_empty() {
                table.remove(agent_id);
            } else {
                table.insert(agent_id, text.as_str());
            }
        }
        Write::Reset(host) => {
            transaction.open_table(HOSTS).remove(host.as_str());
            let departed = transaction
                .open_table(AGENTS)
                .iter()
                .map(|(key, value)| (key.value(), value.value().into_owned()))
                .filter(|(_, stored)| stored.host == host)
                .collect::<Vec<_>>();
            for (agent_id, stored) in departed {
                forget_agent(transaction, agent_id);
                drop_rows(transaction, stored.identity.entry.workset);
            }
        }
        Write::Flush(done) => flushed.push(done),
    }
}

/// Only host-owned copies go away. Drafts and outbox have no other copy.
fn forget_agent(transaction: &mut rho_db::WriteTxn, agent_id: AgentId) {
    transaction.open_table(AGENTS).remove(agent_id);
    transaction.open_table(VERDICTS).remove(agent_id);
    let mut mail = transaction.open_table(MAIL);
    let held = mail
        .range((agent_id, 0)..=(agent_id, u64::MAX))
        .map(|(key, _)| key.value())
        .collect::<Vec<_>>();
    for key in held {
        mail.remove(key);
    }
}

fn drop_rows(transaction: &mut rho_db::WriteTxn, workset: WorksetId) {
    let mut rows = transaction.open_table(ROWS);
    let held = rows
        .range((workset.0, 0)..=(workset.0, u64::MAX))
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

pub fn read_mail(agent_id: AgentId) -> Vec<Mail> {
    read(|mirror| mirror.read_mail(agent_id))
}

pub fn read_rows(workset: WorksetId) -> Vec<(Wpos, LogEvent)> {
    read(|mirror| mirror.read_rows(workset))
}

pub fn rows_next(workset: WorksetId) -> Wpos {
    read(|mirror| mirror.rows_next(workset))
}

pub fn write_host(host: &str, machine_seed: u64) {
    send(Write::Host {
        host: host.to_owned(),
        machine_seed,
    });
}

pub fn write_mail(host: &str, through: MailId, mails: Vec<Mail>) {
    send(Write::Mail {
        host: host.to_owned(),
        through,
        mails,
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

pub fn write_rows(workset: WorksetId, rows: Vec<(Wpos, LogEvent)>) {
    send(Write::Rows { workset, rows });
}

pub fn forget_workset(workset: WorksetId) {
    send(Write::ForgetWorkset(workset));
}

pub fn write_verdict(agent_id: AgentId, verdict: Verdict) {
    send(Write::Verdict(agent_id, verdict));
}

pub fn write_outgoing(agent_id: AgentId, id: u64, outgoing: Option<Outgoing>) {
    send(Write::Outgoing(agent_id, id, outgoing));
}

pub fn write_draft(agent_id: AgentId, text: String) {
    send(Write::Draft(agent_id, text));
}

pub fn read_drafts() -> Vec<(AgentId, String)> {
    read(Mirror::read_drafts)
}

pub fn read_outbox() -> Vec<(AgentId, u64, Outgoing)> {
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
    use rho_agent_types::{ContentPart, InputWake, SendKind, UnixMs};

    use super::*;
    use crate::fold::Said;
    use crate::protocol::rho2::{AgentEntry, SpecView};
    use crate::protocol::transcript::RuntimeKind;

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
            stop: None,
        }
    }

    fn mail(id: u64, from: Address, to: Address, kind: Option<SendKind>) -> Mail {
        Mail {
            id: MailId(id),
            at: UnixMs(id),
            from,
            to,
            re: None,
            kind,
            content: vec![ContentPart::Text {
                text: format!("mail {id}"),
            }],
        }
    }

    fn input(agent: AgentId) -> LogEvent {
        LogEvent::Input {
            agent,
            mail: None,
            text: "go".into(),
            wake: InputWake::Quiet,
            at: UnixMs(1),
        }
    }

    /// What the copy reads back is what it was told: the cursor, the
    /// directory, each agent's conversation folded, mail between agents
    /// under both, and a workset's rows with where reading goes on.
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
            let mut with_stop = stored(a, "fern", workset(1));
            with_stop.stop = Some(Unread {
                pos: crate::fold::ConversationPos::of_stop(MailId(5)),
                at: UnixMs(6),
                said: Said::Stopped,
            });
            mirror.send(Write::Agents(vec![
                (a, with_stop),
                (b, stored(b, "fern", workset(1))),
            ]));
            mirror.send(Write::Mail {
                host: "fern".into(),
                through: MailId(5),
                mails: vec![
                    mail(3, Address::Agent(a), Address::User, Some(SendKind::Ask)),
                    mail(5, Address::Agent(a), Address::Agent(b), None),
                ],
            });
            mirror.send(Write::Rows {
                workset: workset(1),
                rows: vec![(Wpos(0), input(a)), (Wpos(1), input(b))],
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
                mail: MailId(5),
            }]
        );
        let digest_of = |agent| {
            loaded
                .agents
                .iter()
                .find(|loaded| loaded.identity.agent_id() == agent)
                .map(|loaded| loaded.digest.clone())
                .expect("the agent is held")
        };
        let saids = digest_of(a)
            .unread
            .iter()
            .map(|unread| unread.said)
            .collect::<Vec<_>>();
        assert_eq!(saids, [Said::Ask, Said::Stopped], "its mail and its stop");
        assert!(digest_of(b).unread.is_empty());
        assert_eq!(mirror.read_mail(a).len(), 2);
        assert_eq!(
            mirror.read_mail(b).len(),
            1,
            "mail between agents is under both"
        );
        assert_eq!(mirror.read_rows(workset(1)).len(), 2);
        assert_eq!(mirror.rows_next(workset(1)), Wpos(2));
        assert_eq!(mirror.rows_next(workset(2)), Wpos::ZERO);
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
        }
        mirror.send(Write::Agents(vec![
            (a, stored(a, "fern", workset(1))),
            (b, stored(b, "oak", workset(2))),
        ]));
        mirror.send(Write::Mail {
            host: "fern".into(),
            through: MailId(1),
            mails: vec![mail(1, Address::User, Address::Agent(a), None)],
        });
        mirror.send(Write::Rows {
            workset: workset(1),
            rows: vec![(Wpos(0), input(a))],
        });
        mirror.send(Write::Outgoing(
            a,
            7,
            Some(Outgoing {
                at: UnixMs(1),
                content: vec![ContentPart::Text {
                    text: "kept".into(),
                }],
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
        assert_eq!(loaded.hosts[0].mail, MailId(0));
        assert_eq!(
            loaded
                .agents
                .iter()
                .map(|loaded| loaded.identity.agent_id())
                .collect::<Vec<_>>(),
            [b]
        );
        assert!(mirror.read_mail(a).is_empty());
        assert!(mirror.read_rows(workset(1)).is_empty());
        assert_eq!(mirror.read_outbox().len(), 1, "unsent words are the user's");
    }
}
