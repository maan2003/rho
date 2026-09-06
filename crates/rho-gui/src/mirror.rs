//! The client's own copy of what the daemon told it about every agent.
//!
//! The registry folds the mirror in memory, which is enough while the
//! daemon is up and nothing at all after a restart: the rails would be
//! blank until the whole log arrived again. This keeps the same rows on
//! disk - every `Log` entry, keyed by agent and position, one journal
//! cursor per host, and the attention the view derived - so the GUI comes
//! up already knowing them and asks the daemon only for what came after.
//!
//! What the rails read is not folded again at startup: the digest of
//! every agent is written in the same transaction as the rows it folds,
//! and read back whole. The rows themselves are read only for the few
//! agents whose transcript is open.
//!
//! It is a mirror, never a source. Every row here came from the daemon or
//! from the view's own fold of it; anything doubted is thrown away and
//! asked for again from the start.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use redb::TableDefinition;
use rho_db::{RhoDb, Sen, SenValue};
use rho_registry::{AgentIdentity, DIGEST_VERSION, Digest, Verdict};
use rho_ui_proto::AgentId;
use rho_ui_proto::mirror::{AgentPos, LogEntry, MirrorEvent, Seq};

pub const FILE_NAME: &str = "agent-mirror.redb";

/// Where this client stands in a host's journal, by the host's name. The
/// name rather than the host id: ids are handed out in attach order and
/// mean nothing across a restart. The seed says which database the
/// cursor counts in; a daemon with another one starts the copy over.
const HOSTS: TableDefinition<&str, Sen<StoredHost>> = TableDefinition::new("gui_mirror_host_v1");
/// Which host an agent was heard from, so a host's rows can go together.
const AGENT_HOSTS: TableDefinition<AgentId, &str> = TableDefinition::new("gui_agent_host_v1");
/// One agent's mirror, ordered by position, agent first: a range read
/// gives one agent's events and nothing else.
const EVENTS: TableDefinition<(AgentId, u64), Sen<MirrorEvent>> =
    TableDefinition::new("gui_mirror_events_v1");
/// What the registry made of an agent's rows, as of the newest row held:
/// written with the rows, so the two never disagree.
const DIGESTS: TableDefinition<AgentId, Sen<AgentSnapshot>> =
    TableDefinition::new("gui_agent_digest_v1");
/// What the user last said about an agent, so Home ranks the same way on
/// the first frame as it did before the restart: attention is derived
/// from this and the digest. The store overwrites it as soon as the GUI
/// has it again.
const VERDICTS: TableDefinition<AgentId, Sen<Verdict>> =
    TableDefinition::new("gui_agent_verdict_v1");
/// The rows the story kept, and the attention the view once stored.
/// Nothing reads them.
const RETIRED_TABLES: [&str; 3] = [
    "gui_agent_head_v1",
    "gui_agent_story_v1",
    "gui_agent_attention_v1",
];

#[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
struct StoredHost {
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
        entries: Vec<LogEntry>,
        digests: Vec<(AgentId, AgentSnapshot)>,
    },
    Verdict(AgentId, Verdict),
    /// Digests folded again at startup, from rows already held.
    Digests(Vec<(AgentId, AgentSnapshot)>),
    /// Everything heard from a host, gone: its daemon has another
    /// database, or this client doubts what it holds.
    Reset(String),
    Flush(mpsc::SyncSender<()>),
}

pub struct Mirror {
    db: RhoDb,
    /// Taken on drop: closing the channel is what tells the writer thread
    /// to finish, and the database file stays open until it has.
    sender: Option<mpsc::Sender<Write>>,
    writer: Option<std::thread::JoinHandle<()>>,
}

impl Mirror {
    pub fn open(state_dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(state_dir)?;
        let db = RhoDb::open(&path(state_dir));
        let runtime = tokio::runtime::Builder::new_current_thread().build()?;
        runtime.block_on(async {
            let mut write = db.write().await;
            write.open_table(HOSTS);
            write.open_table(AGENT_HOSTS);
            write.open_table(EVENTS);
            write.open_table(DIGESTS);
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
                let mut fold = rho_registry::MirroredAgent::new(
                    rho_registry::HostId::default(),
                    agent_id,
                    &first.1,
                )?;
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
    pub fn read_events(&self, agent_id: AgentId) -> Vec<(AgentPos, MirrorEvent)> {
        self.db
            .read()
            .open_table(EVENTS)
            .range((agent_id, 0)..=(agent_id, u64::MAX))
            .map(|(key, value)| (AgentPos(key.value().1), value.value().into_owned()))
            .collect()
    }

    /// Rows heard from a host, with the digests they brought up to date.
    pub fn write_log(
        &self,
        host: &str,
        machine_seed: u64,
        entries: Vec<LogEntry>,
        digests: Vec<(AgentId, AgentSnapshot)>,
    ) {
        if entries.is_empty() {
            return;
        }
        self.send(Write::Log {
            host: host.to_owned(),
            machine_seed,
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

pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join(FILE_NAME)
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
            entries,
            digests,
        } => {
            let Some(last) = entries.last() else {
                return;
            };
            let seq = last.seq;
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

static GLOBAL: std::sync::OnceLock<Mirror> = std::sync::OnceLock::new();

/// Opens the mirror for this session. Without it every write below is a
/// no-op and the GUI simply starts empty, which is what tests want.
pub fn init(state_dir: &Path) -> std::io::Result<()> {
    let mirror = Mirror::open(state_dir)?;
    GLOBAL.set(mirror).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "the agent mirror is already initialized",
        )
    })
}

pub fn load() -> Loaded {
    GLOBAL.get().map(Mirror::load).unwrap_or_default()
}

pub fn read_events(agent_id: AgentId) -> Vec<(AgentPos, MirrorEvent)> {
    GLOBAL
        .get()
        .map(|mirror| mirror.read_events(agent_id))
        .unwrap_or_default()
}

pub fn write_log(
    host: &str,
    machine_seed: u64,
    entries: Vec<LogEntry>,
    digests: Vec<(AgentId, AgentSnapshot)>,
) {
    if let Some(mirror) = GLOBAL.get() {
        mirror.write_log(host, machine_seed, entries, digests);
    }
}

pub fn write_verdict(agent_id: AgentId, verdict: Verdict) {
    if let Some(mirror) = GLOBAL.get() {
        mirror.write_verdict(agent_id, verdict);
    }
}

pub fn reset_host(host: &str) {
    if let Some(mirror) = GLOBAL.get() {
        mirror.reset_host(host);
    }
}

pub fn flush() {
    if let Some(mirror) = GLOBAL.get() {
        mirror.flush();
    }
}

#[cfg(test)]
mod tests {
    use rho_ui_proto::mirror::{RuntimeKind, SpawnedBy, TurnEdge, TurnOutcome};

    use super::*;

    fn agent_id(counter: u64) -> AgentId {
        AgentId::from_counter(counter, &rho_ui_proto::AgentIdDomain(7)).expect("agent id")
    }

    fn told(agent: AgentId, from_seq: u64) -> Vec<LogEntry> {
        [
            MirrorEvent::Created {
                role: Default::default(),
                runtime: RuntimeKind::Rho,
                workdirs: Vec::new(),
                spawned_by: SpawnedBy::Direct,
                spawn_name: Some("the deploy".to_owned()),
                parent: None,
                model: "sol".to_owned(),
                at: rho_core::UnixMs(1_000),
            },
            MirrorEvent::Message {
                from: None,
                text: "have a look".to_owned(),
                delivery: rho_core::MessageDelivery::Immediate,
                at: rho_core::UnixMs(1_000),
            },
            MirrorEvent::Turn {
                edge: TurnEdge::Ended(TurnOutcome::Completed),
                at: rho_core::UnixMs(2_000),
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

    fn events(entries: &[LogEntry]) -> Vec<(AgentPos, MirrorEvent)> {
        entries
            .iter()
            .map(|entry| (entry.pos, entry.event.clone()))
            .collect()
    }

    /// What the registry makes of a run of one agent's rows.
    fn snapshot(entries: &[LogEntry]) -> AgentSnapshot {
        let mut mirrored = rho_registry::MirroredAgent::new(
            rho_registry::HostId::default(),
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
        mirror.write_log(host, machine_seed, entries, digests);
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
}
