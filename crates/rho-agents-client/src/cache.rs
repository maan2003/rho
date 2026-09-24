//! The client's own copy of what the agent host told it about every agent.
//!
//! The registry folds transcripts in memory, which is enough while the
//! agent host is up and nothing at all after a restart: the rails would be
//! blank until the whole log arrived again. This keeps the same rows on
//! disk - every `Log` entry, keyed by agent and position, one journal
//! cursor per host, and the attention the view derived - so the GUI comes
//! up already knowing them and asks the agent host only for what came after.
//!
//! What the rails read is not folded again at startup: the digest of
//! every agent is written in the same transaction as the rows it folds,
//! and read back whole. The rows themselves are read only for the few
//! agents whose transcript is open.
//!
//! It is a copy, never a source. Every row here came from the agent host or
//! from the view's own fold of it; anything doubted is thrown away and
//! asked for again from the start.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use redb::{TableDefinition, TableHandle};
use rho_agent_types::{AgentId, AgentPos, Seq};
use rho_db::{RhoDb, Sen, SenValue};

use crate::protocol::transcript::{LogEntry, TranscriptEvent};
use crate::{AgentIdentity, DIGEST_VERSION, Digest, Verdict};

/// Where this client stands in a host's journal, by the host's name. The
/// name rather than the host id: ids are handed out in attach order and
/// mean nothing across a restart. The seed says which database the
/// cursor counts in; an agent host with another one starts the copy over.
const HOSTS: TableDefinition<&str, Sen<StoredHost>> = TableDefinition::new("gui_mirror_host_v6");
/// Which host an agent was heard from, so a host's rows can go together.
const AGENT_HOSTS: TableDefinition<AgentId, &str> = TableDefinition::new("gui_agent_host_v4");
/// One agent's mirror, ordered by position, agent first: a range read
/// gives one agent's events and nothing else.
/// The version in the name is the story's format, not redb's. A fold that
/// drops something the agent host sent cannot be repaired from what is on
/// disk — the rows here are all the client has — so the version moves and
/// the old table goes, and the copy starts over from the agent host's raw log
/// with the cursor beside it. That is why `HOSTS` moves with it: a cursor
/// kept past the rows it counted would ask only for what is new.
///
/// v2: tool calls carry the arguments the model sent. v1 kept only the one
/// field a label names, so a code-mode `exec` call — whose arguments are
/// JavaScript, not JSON — stored nothing at all.
/// v3: `Created` names one place instead of a list of workdirs.
/// v4: canonical native entries preserve every response boundary; recurring
/// presentation was replaced by one-shot titles. Old projections must refetch.
/// v5: ordered response items replace flattened text and calls.
/// v6: nothing new in the fold; retired Gemini agents were deleted from
/// the agent host, and rows naming their role no longer decode.
const EVENTS: TableDefinition<(AgentId, u64), Sen<TranscriptEvent>> =
    TableDefinition::new("gui_mirror_events_v6");
/// What the registry made of an agent's rows, as of the newest row held:
/// written with the rows, so the two never disagree.
const DIGESTS: TableDefinition<AgentId, Sen<AgentSnapshot>> =
    TableDefinition::new("gui_agent_digest_v4");
/// What the user last said about an agent, so Home ranks the same way on
/// the first frame as it did before the restart: attention is derived
/// from this and the digest. The store overwrites it as soon as the GUI
/// has it again.
const VERDICTS: TableDefinition<AgentId, Sen<Verdict>> =
    TableDefinition::new("gui_agent_verdict_v1");

/// Tables nothing reads: retired folds, and the rows and cursor of a story
/// format the client has moved past. Dropped on open, every open.
const RETIRED_TABLES: [&str; 19] = [
    "gui_agent_host_v2",
    "gui_agent_digest_v2",
    "gui_mirror_host_v4",
    "gui_mirror_events_v4",
    "gui_agent_head_v1",
    "gui_agent_story_v1",
    "gui_agent_attention_v1",
    // The story's v1 and v2 rows and the cursors that counted them.
    // Deleted rather than migrated: the agent host has the raw log and the
    // client re-derives.
    "gui_mirror_events_v1",
    "gui_mirror_host_v1",
    "gui_mirror_events_v2",
    "gui_mirror_host_v2",
    // Native history was rewritten at the same journal positions. Refetch all
    // projections together; a digest refold cannot recover omitted data.
    "gui_mirror_host_v3",
    "gui_mirror_events_v3",
    "gui_agent_host_v1",
    "gui_agent_digest_v1",
    // Rows of deleted Gemini agents, whose role no longer decodes.
    "gui_mirror_host_v5",
    "gui_mirror_events_v5",
    "gui_agent_host_v3",
    "gui_agent_digest_v3",
];

#[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub(crate) struct StoredHost {
    machine_seed: u64,
    seq: Seq,
}

/// One host as the mirror holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirroredHost {
    pub name: String,
    pub machine_seed: u64,
    /// The newest journal entry this client holds; what `Follow` sends.
    pub seq: Seq,
}

/// The registry's fold of one agent, as it stood after the newest row.
#[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub struct AgentSnapshot {
    /// Which fold made the digest; another version on disk is folded
    /// again from the rows at startup.
    #[senax(default)]
    pub version: u32,
    pub identity: AgentIdentity,
    pub digest: Digest,
}

impl AgentSnapshot {
    pub fn new(identity: AgentIdentity, digest: Digest) -> Self {
        Self {
            version: DIGEST_VERSION,
            identity,
            digest,
        }
    }
}

/// One agent as the mirror holds it: whose it is, what the registry made
/// of it, and what the user last said about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirroredAgent {
    pub host: String,
    pub snapshot: AgentSnapshot,
    pub verdict: Option<Verdict>,
}

#[derive(Default)]
pub struct Loaded {
    pub hosts: Vec<MirroredHost>,
    pub agents: Vec<(AgentId, MirroredAgent)>,
}

enum Write {
    /// A run of one host's log, with the seq the cursor moves to and the
    /// digests the rows brought up to date. One transaction, so the
    /// cursor never claims rows that are not there, and a digest never
    /// stands ahead of its rows.
    Log {
        host: String,
        machine_seed: u64,
        /// How far the page ran. "Seen through here", never "kept a row
        /// here": a page whose rows this client could make nothing of has
        /// still been seen, and asking for it again on the next start is
        /// how the tail grew without bound.
        seq: Seq,
        entries: Vec<LogEntry>,
        digests: Vec<(AgentId, AgentSnapshot)>,
    },
    Verdict(AgentId, Verdict),
    /// Digests folded again at startup, from rows already held.
    Digests(Vec<(AgentId, AgentSnapshot)>),
    /// Everything heard from a host, gone: its agent host has another
    /// database, or this client doubts what it holds.
    Reset(String),
    Flush(mpsc::SyncSender<()>),
}

/// Opens a mirror table, dropping it first if the file records it under
/// other key/value types.
///
/// redb writes the Rust path of a value type into the table it types, so
/// a struct that moves between crates makes every database written
/// before the move refuse to open — the fault this crate already carries
/// a pin for. A pin fixes one direction only: `StoredHost` and
/// `AgentSnapshot` were written under `rho_gui::mirror` before 09-07 and
/// under `crate::cache` after it, and both are on disk in the
/// wild, so no single name opens both. Every row in these four tables is
/// a copy of something the agent host still has, and the crate's own rule is
/// that anything doubted is thrown away and asked for again, so the
/// answer here is to drop and re-copy rather than to name.
///
/// `VERDICTS` is not passed through this: a verdict is the user's own
/// word about an agent and the agent host has no copy of it, so that one
/// stays pinned by name and is never dropped.
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
    /// Opens the client's database at `state_dir` and takes the mirror's
    /// tables in it. For tests and tools; the session's own database is
    /// opened once by `main` at startup and handed to [`Mirror::open_on`].
    pub fn open(state_dir: &Path) -> std::io::Result<Self> {
        Self::open_on(rho_db::client::open(state_dir)?)
    }

    /// The mirror's tables in a database somebody else opened. Every other
    /// kind of client state is in there too, under its own names; this
    /// touches the agent mirror's and nothing else.
    pub fn open_on(db: RhoDb) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread().build()?;
        runtime.block_on(async {
            let mut write = db.write().await;
            open_or_rebuild(&mut write, HOSTS);
            open_or_rebuild(&mut write, AGENT_HOSTS);
            open_or_rebuild(&mut write, EVENTS);
            open_or_rebuild(&mut write, DIGESTS);
            write.open_table(VERDICTS);
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

    /// Every host's cursor and every agent's digest: what the GUI starts
    /// from. No rows; those are read when a transcript opens, and once
    /// here for an agent whose digest an older fold made.
    pub fn load(&self) -> Loaded {
        let loaded = self.load_stored();
        let mut refolded = Vec::new();
        let agents = loaded
            .agents
            .into_iter()
            .filter_map(|(agent_id, mut mirrored)| {
                if mirrored.snapshot.version == DIGEST_VERSION {
                    return Some((agent_id, mirrored));
                }
                let events = self.read_events(agent_id);
                let (first, rest) = events.split_first()?;
                let mut fold =
                    crate::MirroredAgent::new(crate::HostId::default(), agent_id, &first.1)?;
                for (pos, event) in rest {
                    fold.tell(*pos, event);
                }
                mirrored.snapshot = AgentSnapshot::new(fold.identity, fold.digest);
                refolded.push((agent_id, mirrored.snapshot.clone()));
                Some((agent_id, mirrored))
            })
            .collect();
        if !refolded.is_empty() {
            self.send(Write::Digests(refolded));
        }
        Loaded {
            hosts: loaded.hosts,
            agents,
        }
    }

    fn load_stored(&self) -> Loaded {
        let read = self.db.read();
        let hosts = read
            .open_table(HOSTS)
            .iter()
            .map(|(key, value)| {
                let stored = value.value().into_owned();
                MirroredHost {
                    name: key.value().to_owned(),
                    machine_seed: stored.machine_seed,
                    seq: stored.seq,
                }
            })
            .collect();
        let verdicts = read.open_table(VERDICTS);
        let agent_hosts = read.open_table(AGENT_HOSTS);
        let agents = read
            .open_table(DIGESTS)
            .iter()
            .filter_map(|(key, value)| {
                let agent_id = key.value();
                // A digest without a host is a partial reset; the host's
                // copy starts over the next time it is doubted.
                let host = agent_hosts.get(&agent_id)?.value().to_owned();
                Some((
                    agent_id,
                    MirroredAgent {
                        host,
                        snapshot: value.value().into_owned(),
                        verdict: verdicts
                            .get(&agent_id)
                            .map(|value| value.value().into_owned()),
                    },
                ))
            })
            .collect();
        Loaded { hosts, agents }
    }

    /// One agent's mirror, oldest first, for the transcript a reader opens.
    pub fn read_events(&self, agent_id: AgentId) -> Vec<(AgentPos, TranscriptEvent)> {
        self.db
            .read()
            .open_table(EVENTS)
            .range((agent_id, 0)..=(agent_id, u64::MAX))
            .map(|(key, value)| (AgentPos(key.value().1), value.value().into_owned()))
            .collect()
    }

    /// Rows heard from a host, with the digests they brought up to date.
    /// `seq` is how far the page ran, not how far its rows did: a page
    /// whose rows all fold to nothing still moves the cursor.
    pub fn write_log(
        &self,
        host: &str,
        machine_seed: u64,
        seq: Seq,
        entries: Vec<LogEntry>,
        digests: Vec<(AgentId, AgentSnapshot)>,
    ) {
        self.send(Write::Log {
            host: host.to_owned(),
            machine_seed,
            seq,
            entries,
            digests,
        });
    }

    pub fn write_verdict(&self, agent_id: AgentId, verdict: Verdict) {
        self.send(Write::Verdict(agent_id, verdict));
    }

    pub fn reset_host(&self, host: &str) {
        self.send(Write::Reset(host.to_owned()));
    }

    /// Waits for everything already queued to commit. Shutdown calls this;
    /// interaction sites never need to.
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
        Write::Log {
            host,
            machine_seed,
            seq,
            entries,
            digests,
        } => {
            {
                let mut events = transaction.open_table(EVENTS);
                for entry in &entries {
                    events.insert(
                        &(entry.agent_id, entry.pos.0),
                        SenValue::borrowed(&entry.event),
                    );
                }
            }
            {
                let mut agent_hosts = transaction.open_table(AGENT_HOSTS);
                for entry in entries.iter().filter(|e| e.pos == AgentPos::ZERO) {
                    agent_hosts.insert(&entry.agent_id, &host.as_str());
                }
            }
            {
                let mut table = transaction.open_table(DIGESTS);
                for (agent_id, snapshot) in &digests {
                    table.insert(agent_id, SenValue::borrowed(snapshot));
                }
            }
            transaction.open_table(HOSTS).insert(
                &host.as_str(),
                SenValue::borrowed(&StoredHost { machine_seed, seq }),
            );
        }
        Write::Verdict(agent_id, verdict) => {
            transaction
                .open_table(VERDICTS)
                .insert(&agent_id, SenValue::borrowed(&verdict));
        }
        Write::Digests(digests) => {
            let mut table = transaction.open_table(DIGESTS);
            for (agent_id, snapshot) in &digests {
                table.insert(agent_id, SenValue::borrowed(snapshot));
            }
        }
        Write::Reset(host) => {
            transaction.open_table(HOSTS).remove(&host.as_str());
            let mut agent_hosts = transaction.open_table(AGENT_HOSTS);
            let departed = agent_hosts
                .iter()
                .filter(|(_, owner)| owner.value() == host)
                .map(|(key, _)| key.value())
                .collect::<Vec<_>>();
            for agent_id in &departed {
                agent_hosts.remove(agent_id);
            }
            drop(agent_hosts);
            let mut verdicts = transaction.open_table(VERDICTS);
            for agent_id in &departed {
                verdicts.remove(agent_id);
            }
            drop(verdicts);
            let mut digests = transaction.open_table(DIGESTS);
            for agent_id in &departed {
                digests.remove(agent_id);
            }
            drop(digests);
            let mut events = transaction.open_table(EVENTS);
            for agent_id in departed {
                let held = events
                    .range((agent_id, 0)..=(agent_id, u64::MAX))
                    .map(|(key, _)| key.value())
                    .collect::<Vec<_>>();
                for key in held {
                    events.remove(&key);
                }
            }
        }
        Write::Flush(done) => flushed.push(done),
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

/// Takes the mirror's tables in the client's database, which `main` opens
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
/// shut cleanly and skips redb's rebuild. A session that ends without
/// this pays that rebuild once, at the next start's open.
pub fn close() {
    CLOSED.store(true, std::sync::atomic::Ordering::Release);
    let mirror = GLOBAL
        .get_or_init(|| std::sync::RwLock::new(None))
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    drop(mirror);
}

pub fn load() -> Loaded {
    global().as_ref().map(Mirror::load).unwrap_or_default()
}

pub fn read_events(agent_id: AgentId) -> Vec<(AgentPos, TranscriptEvent)> {
    global()
        .as_ref()
        .map(|mirror| mirror.read_events(agent_id))
        .unwrap_or_default()
}

pub fn write_log(
    host: &str,
    machine_seed: u64,
    seq: Seq,
    entries: Vec<LogEntry>,
    digests: Vec<(AgentId, AgentSnapshot)>,
) {
    if let Some(mirror) = global().as_ref() {
        mirror.write_log(host, machine_seed, seq, entries, digests);
    }
}

pub fn write_verdict(agent_id: AgentId, verdict: Verdict) {
    if let Some(mirror) = global().as_ref() {
        mirror.write_verdict(agent_id, verdict);
    }
}

pub fn reset_host(host: &str) {
    if let Some(mirror) = global().as_ref() {
        mirror.reset_host(host);
    }
}

pub fn flush() {
    if let Some(mirror) = global().as_ref() {
        mirror.flush();
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::{TurnEdge, TurnOutcome};

    use super::*;
    use crate::protocol::transcript::{RuntimeKind, SpawnedBy};

    fn agent_id(counter: u64) -> AgentId {
        AgentId::from_counter(counter, &rho_agent_types::AgentIdDomain(7)).expect("agent id")
    }

    fn told(agent: AgentId, from_seq: u64) -> Vec<LogEntry> {
        [
            TranscriptEvent::Created {
                role: Default::default(),
                runtime: RuntimeKind::Rho,
                place: rho_agent_types::Place {
                    workset: "0123456789ab".into(),
                    cwd: "/src/repo".into(),
                    mode: Default::default(),
                    origin: None,
                },
                spawned_by: SpawnedBy::Direct,
                spawn_name: Some("the deploy".to_owned()),
                parent: None,
                model: "sol".to_owned(),
                at: rho_agent_types::UnixMs(1_000),
            },
            TranscriptEvent::Message {
                from: None,
                text: "have a look".to_owned(),
                delivery: rho_agent_types::MessageDelivery::Immediate,
                at: rho_agent_types::UnixMs(1_000),
            },
            TranscriptEvent::Turn {
                edge: TurnEdge::Ended(TurnOutcome::Completed),
                at: rho_agent_types::UnixMs(2_000),
            },
        ]
        .into_iter()
        .enumerate()
        .map(|(offset, event)| LogEntry {
            seq: Seq(from_seq + offset as u64),
            agent_id: agent,
            pos: AgentPos(offset as u64),
            event,
        })
        .collect()
    }

    fn events(entries: &[LogEntry]) -> Vec<(AgentPos, TranscriptEvent)> {
        entries
            .iter()
            .map(|entry| (entry.pos, entry.event.clone()))
            .collect()
    }

    /// What the registry makes of a run of one agent's rows.
    fn snapshot(entries: &[LogEntry]) -> AgentSnapshot {
        let mut mirrored = crate::MirroredAgent::new(
            crate::HostId::default(),
            entries[0].agent_id,
            &entries[0].event,
        )
        .expect("opens with the creation");
        for entry in &entries[1..] {
            mirrored.tell(entry.pos, &entry.event);
        }
        AgentSnapshot::new(mirrored.identity, mirrored.digest)
    }

    fn write(mirror: &Mirror, host: &str, machine_seed: u64, entries: Vec<LogEntry>) {
        let digests = vec![(entries[0].agent_id, snapshot(&entries))];
        let seq = entries.last().expect("a page with rows").seq;
        mirror.write_log(host, machine_seed, seq, entries, digests);
    }

    /// What a transcript reads when it is opened: one agent's mirror,
    /// without the other agents' events.
    #[test]
    fn events_are_read_back_for_one_agent_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mine = agent_id(1);
        let theirs = agent_id(2);
        let mirror = Mirror::open(dir.path()).expect("open");
        write(&mirror, "local", 7, told(mine, 1));
        write(&mirror, "local", 7, told(theirs, 4));
        mirror.flush();

        assert_eq!(mirror.read_events(mine), events(&told(mine, 1)));
        assert!(mirror.read_events(agent_id(3)).is_empty());
    }

    /// What the GUI comes up holding after a restart: the host's cursor,
    /// the digest as it stood, and the attention the view had decided.
    #[test]
    fn a_reopened_mirror_holds_what_was_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent = agent_id(1);
        {
            let mirror = Mirror::open(dir.path()).expect("open");
            write(&mirror, "local", 7, told(agent, 1));
            mirror.write_verdict(
                agent,
                Verdict {
                    handled_through: AgentPos(2),
                    muted: false,
                },
            );
            mirror.flush();
        }

        let mirror = Mirror::open(dir.path()).expect("reopen");
        let loaded = mirror.load();
        assert_eq!(
            loaded.hosts,
            [MirroredHost {
                name: "local".to_owned(),
                machine_seed: 7,
                seq: Seq(3),
            }]
        );
        assert_eq!(loaded.agents.len(), 1);
        let (loaded_id, mirrored) = &loaded.agents[0];
        assert_eq!(*loaded_id, agent);
        assert_eq!(mirrored.host, "local");
        assert_eq!(mirrored.snapshot, snapshot(&told(agent, 1)));
        assert_eq!(mirrored.snapshot.digest.newest, AgentPos(3));
        assert_eq!(
            mirrored.verdict,
            Some(Verdict {
                handled_through: AgentPos(2),
                muted: false,
            })
        );
        assert_eq!(mirrored.snapshot.version, DIGEST_VERSION);
        assert_eq!(mirror.read_events(agent), events(&told(agent, 1)));
    }

    /// A host with another database leaves nothing behind, or Home would
    /// rank work that does not exist.
    #[test]
    fn resetting_a_host_takes_its_agents_with_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (kept, gone) = (agent_id(3), agent_id(4));
        let mirror = Mirror::open(dir.path()).expect("open");
        write(&mirror, "local", 7, told(kept, 1));
        write(&mirror, "remote", 8, told(gone, 1));
        mirror.reset_host("remote");
        mirror.flush();

        let loaded = mirror.load();
        assert_eq!(loaded.hosts.len(), 1);
        assert_eq!(loaded.hosts[0].name, "local");
        assert_eq!(loaded.agents.len(), 1);
        assert_eq!(loaded.agents[0].0, kept);
        assert!(mirror.read_events(gone).is_empty());
    }

    #[test]
    fn canonical_history_retires_all_old_projections_but_keeps_user_verdicts() {
        const OLD_HOSTS: TableDefinition<&str, Sen<StoredHost>> =
            TableDefinition::new("gui_mirror_host_v4");
        const OLD_EVENTS: TableDefinition<(AgentId, u64), Sen<TranscriptEvent>> =
            TableDefinition::new("gui_mirror_events_v4");
        const OLD_DIGESTS: TableDefinition<AgentId, Sen<AgentSnapshot>> =
            TableDefinition::new("gui_agent_digest_v2");
        const OLD_AGENT_HOSTS: TableDefinition<AgentId, &str> =
            TableDefinition::new("gui_agent_host_v2");
        const UNRELATED: TableDefinition<u64, &str> = TableDefinition::new("unrelated_user_data");
        let dir = tempfile::tempdir().unwrap();
        let db = RhoDb::open(dir.path().join("client.redb"));
        let id = agent_id(1);
        let entries = told(id, 1);
        let verdict = Verdict {
            handled_through: AgentPos(2),
            muted: true,
        };
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async {
                let mut write = db.write().await;
                write.open_table(OLD_HOSTS).insert(
                    "local",
                    SenValue::borrowed(&StoredHost {
                        machine_seed: 7,
                        seq: Seq(3),
                    }),
                );
                write.open_table(OLD_AGENT_HOSTS).insert(&id, "local");
                write
                    .open_table(OLD_DIGESTS)
                    .insert(&id, SenValue::borrowed(&snapshot(&entries)));
                for entry in &entries {
                    write
                        .open_table(OLD_EVENTS)
                        .insert(&(id, entry.pos.0), SenValue::borrowed(&entry.event));
                }
                write
                    .open_table(VERDICTS)
                    .insert(&id, SenValue::borrowed(&verdict));
                write.open_table(UNRELATED).insert(&1, "keep me");
                write.commit();
            });
        let mirror = Mirror::open_on(db.clone()).unwrap();
        assert!(
            mirror.load().hosts.is_empty(),
            "a cursor survived its projection"
        );
        assert!(mirror.load().agents.is_empty());
        assert!(mirror.read_events(id).is_empty());
        assert_eq!(db.read().open_table(AGENT_HOSTS).iter().count(), 0);
        assert_eq!(
            db.read()
                .open_table(VERDICTS)
                .get(&id)
                .unwrap()
                .value()
                .into_owned(),
            verdict
        );
        assert_eq!(
            db.read().open_table(UNRELATED).get(&1).unwrap().value(),
            "keep me"
        );
        // Refill from the migrated agent host as on Follow{since:0}; preserve user
        // disposition when the rebuilt agent first appears again.
        write(&mirror, "local", 7, entries);
        mirror.flush();
        let loaded = mirror.load();
        assert_eq!(loaded.hosts[0].seq, Seq(3));
        assert_eq!(loaded.agents[0].1.verdict.as_ref(), Some(&verdict));
    }
}
