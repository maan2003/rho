//! The client's own copy of what the daemon told it about every agent.
//!
//! The registry folds an agent's story in memory, which is enough while
//! the daemon is up and nothing at all after a restart: the rails would
//! be blank until `Ready` and the whole story arrived again. This keeps
//! the same rows on disk - the heads, the story events, and the attention
//! the view derived - so the GUI comes up already knowing them and asks
//! the daemon only for what it lacks.
//!
//! It is a mirror, never a source. Every row here came from the daemon or
//! from the view's own fold of it; anything doubted is thrown away and
//! asked for again.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use redb::TableDefinition;
use rho_db::{RhoDb, Sen, SenValue};
use rho_registry::Attention;
use rho_ui_proto::AgentId;
use rho_ui_proto::story::{UiAgentHead, UiStoryEvent, UiStoryPos};

pub const FILE_NAME: &str = "agent-mirror.redb";

/// The head, with the name of the host it was heard from. The name rather
/// than the host id: ids are handed out in attach order and mean nothing
/// across a restart, and a row whose host is no longer attached is dropped
/// rather than filed under the wrong daemon.
const HEADS: TableDefinition<AgentId, Sen<StoredHead>> = TableDefinition::new("gui_agent_head_v1");
/// One agent's story, ordered by position, agent first: a range read gives
/// one agent's events and nothing else.
const STORY: TableDefinition<(AgentId, u64), Sen<UiStoryEvent>> =
    TableDefinition::new("gui_agent_story_v1");
/// What the view decided an agent wants, so Home ranks the same way on the
/// first frame as it did before the restart. Derived, never authoritative:
/// the view overwrites it as soon as it has the store again.
const ATTENTION: TableDefinition<AgentId, u8> = TableDefinition::new("gui_agent_attention_v1");

#[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
struct StoredHead {
    host: String,
    head: UiAgentHead,
}

/// One agent as the mirror holds it: what it is, what has happened to it,
/// and what the view last decided it wants.
pub struct MirroredAgent {
    pub host: String,
    pub head: UiAgentHead,
    /// From position zero, in order. A gap would make the fold wrong, so a
    /// story that is not contiguous from zero is dropped and asked for
    /// again instead.
    pub story: Vec<UiStoryEvent>,
    pub attention: Option<Attention>,
}

enum Write {
    Head(String, Box<UiAgentHead>),
    Story(AgentId, UiStoryPos, Vec<UiStoryEvent>),
    Attention(AgentId, Attention),
    /// The agents this host no longer has. Their rows go, story and all.
    Departed(Vec<AgentId>),
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
            write.open_table(HEADS);
            write.open_table(STORY);
            write.open_table(ATTENTION);
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

    /// Everything the mirror holds, for the fold the GUI starts from.
    pub fn load(&self) -> Vec<(AgentId, MirroredAgent)> {
        let read = self.db.read();
        let attention = read.open_table(ATTENTION);
        let story = read.open_table(STORY);
        read.open_table(HEADS)
            .iter()
            .map(|(key, value)| {
                let agent_id = key.value();
                let stored = value.value().into_owned();
                let told = story
                    .range((agent_id, 0)..=(agent_id, u64::MAX))
                    .map(|(key, value)| (key.value().1, value.value().into_owned()))
                    .collect::<Vec<_>>();
                // Contiguous from zero or nothing: a fold over a story with
                // a hole in it says the wrong thing, and the daemon will
                // send the whole run again for the asking.
                let contiguous = told
                    .iter()
                    .enumerate()
                    .all(|(index, (pos, _))| *pos == index as u64);
                let story = contiguous
                    .then(|| told.into_iter().map(|(_, event)| event).collect())
                    .unwrap_or_default();
                (
                    agent_id,
                    MirroredAgent {
                        host: stored.host,
                        head: stored.head,
                        story,
                        attention: attention
                            .get(&agent_id)
                            .and_then(|value| attention_of(value.value())),
                    },
                )
            })
            .collect()
    }

    /// One agent's story, oldest first, for the transcript a reader opens
    /// before any frame arrives. Empty unless the run is contiguous from
    /// zero, for the reason `load` gives.
    pub fn read_story(&self, agent_id: AgentId) -> Vec<UiStoryEvent> {
        let read = self.db.read();
        let told = read
            .open_table(STORY)
            .range((agent_id, 0)..=(agent_id, u64::MAX))
            .map(|(key, value)| (key.value().1, value.value().into_owned()))
            .collect::<Vec<_>>();
        let contiguous = told
            .iter()
            .enumerate()
            .all(|(index, (pos, _))| *pos == index as u64);
        if !contiguous {
            return Vec::new();
        }
        told.into_iter().map(|(_, event)| event).collect()
    }

    pub fn write_head(&self, host: &str, head: UiAgentHead) {
        self.send(Write::Head(host.to_owned(), Box::new(head)));
    }

    pub fn write_story(&self, agent_id: AgentId, from: UiStoryPos, events: Vec<UiStoryEvent>) {
        if events.is_empty() {
            return;
        }
        self.send(Write::Story(agent_id, from, events));
    }

    pub fn write_attention(&self, agent_id: AgentId, attention: Attention) {
        self.send(Write::Attention(agent_id, attention));
    }

    /// Agents a host no longer lists. Keeping them would rank work that
    /// does not exist.
    pub fn forget(&self, agent_ids: Vec<AgentId>) {
        if agent_ids.is_empty() {
            return;
        }
        self.send(Write::Departed(agent_ids));
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
    for write in receiver {
        runtime.block_on(async {
            let mut transaction = db.write().await;
            match write {
                Write::Head(host, head) => {
                    let stored = StoredHead { host, head: *head };
                    transaction
                        .open_table(HEADS)
                        .insert(&stored.head.agent_id, SenValue::borrowed(&stored));
                }
                Write::Story(agent_id, from, events) => {
                    let mut table = transaction.open_table(STORY);
                    for (offset, event) in events.iter().enumerate() {
                        table.insert(
                            &(agent_id, from.0 + offset as u64),
                            SenValue::borrowed(event),
                        );
                    }
                }
                Write::Attention(agent_id, attention) => {
                    transaction
                        .open_table(ATTENTION)
                        .insert(&agent_id, &attention_code(attention));
                }
                Write::Departed(agent_ids) => {
                    let mut heads = transaction.open_table(HEADS);
                    for agent_id in &agent_ids {
                        heads.remove(agent_id);
                    }
                    drop(heads);
                    let mut attention = transaction.open_table(ATTENTION);
                    for agent_id in &agent_ids {
                        attention.remove(agent_id);
                    }
                    drop(attention);
                    let mut story = transaction.open_table(STORY);
                    for agent_id in agent_ids {
                        let told = story
                            .range((agent_id, 0)..=(agent_id, u64::MAX))
                            .map(|(key, _)| key.value())
                            .collect::<Vec<_>>();
                        for key in told {
                            story.remove(&key);
                        }
                    }
                }
                Write::Flush(done) => {
                    let _ = done.send(());
                }
            }
            transaction.commit();
        });
    }
}

/// Attention as one byte. The view derives it fresh every time it has the
/// store, so an unknown code is simply forgotten rather than migrated.
fn attention_code(attention: Attention) -> u8 {
    match attention {
        Attention::Quiet => 0,
        Attention::Working => 1,
        Attention::Pending => 2,
        Attention::NeedsInput => 3,
    }
}

fn attention_of(code: u8) -> Option<Attention> {
    Some(match code {
        0 => Attention::Quiet,
        1 => Attention::Working,
        2 => Attention::Pending,
        3 => Attention::NeedsInput,
        _ => return None,
    })
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

pub fn load() -> Vec<(AgentId, MirroredAgent)> {
    GLOBAL.get().map(Mirror::load).unwrap_or_default()
}

pub fn read_story(agent_id: AgentId) -> Vec<UiStoryEvent> {
    GLOBAL
        .get()
        .map(|mirror| mirror.read_story(agent_id))
        .unwrap_or_default()
}

pub fn write_head(host: &str, head: UiAgentHead) {
    if let Some(mirror) = GLOBAL.get() {
        mirror.write_head(host, head);
    }
}

pub fn write_story(agent_id: AgentId, from: UiStoryPos, events: Vec<UiStoryEvent>) {
    if let Some(mirror) = GLOBAL.get() {
        mirror.write_story(agent_id, from, events);
    }
}

pub fn write_attention(agent_id: AgentId, attention: Attention) {
    if let Some(mirror) = GLOBAL.get() {
        mirror.write_attention(agent_id, attention);
    }
}

pub fn forget(agent_ids: Vec<AgentId>) {
    if let Some(mirror) = GLOBAL.get() {
        mirror.forget(agent_ids);
    }
}

pub fn flush() {
    if let Some(mirror) = GLOBAL.get() {
        mirror.flush();
    }
}

#[cfg(test)]
mod tests {
    use rho_ui_proto::story::{UiRuntimeKind, UiSpawnedBy, UiTurnOutcome};

    use super::*;

    fn agent_id(counter: u64) -> AgentId {
        AgentId::from_counter(counter, &rho_ui_proto::AgentIdDomain(7)).expect("agent id")
    }

    fn head(agent_id: AgentId) -> UiAgentHead {
        UiAgentHead {
            agent_id,
            story_pos: UiStoryPos(2),
            role: Default::default(),
            runtime_kind: UiRuntimeKind::Rho,
            workdirs: Vec::new(),
            spawned_by: UiSpawnedBy::Direct,
            parent: None,
            spawn_name: Some("the deploy".to_owned()),
            generated_title: None,
            activity: None,
            turn_running: false,
            created_at: rho_core::UnixMs(1_000),
        }
    }

    fn told() -> Vec<UiStoryEvent> {
        vec![
            UiStoryEvent::UserMessage {
                text: "have a look".to_owned(),
                at: rho_core::UnixMs(1_000),
            },
            UiStoryEvent::TurnEnded {
                outcome: UiTurnOutcome::Completed,
                at: rho_core::UnixMs(2_000),
            },
        ]
    }

    /// What a transcript reads when it is opened before any frame: one
    /// agent's story, without the other agents' events.
    #[test]
    fn a_story_is_read_back_for_one_agent_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mine = agent_id(1);
        let theirs = agent_id(2);
        let mirror = Mirror::open(dir.path()).expect("open");
        mirror.write_head("local", head(mine));
        mirror.write_story(mine, UiStoryPos(0), told());
        mirror.write_story(
            theirs,
            UiStoryPos(0),
            vec![UiStoryEvent::UserMessage {
                text: "not mine".to_owned(),
                at: rho_core::UnixMs(3_000),
            }],
        );
        mirror.flush();

        assert_eq!(mirror.read_story(mine), told());
        assert!(mirror.read_story(agent_id(3)).is_empty());
    }

    /// What the GUI comes up holding after a restart: the head, the story
    /// in order, and the attention the view had decided.
    #[test]
    fn a_reopened_mirror_holds_what_was_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent = agent_id(1);
        {
            let mirror = Mirror::open(dir.path()).expect("open");
            mirror.write_head("local", head(agent));
            mirror.write_story(agent, UiStoryPos(0), told());
            mirror.write_attention(agent, Attention::NeedsInput);
            mirror.flush();
        }

        let mirror = Mirror::open(dir.path()).expect("reopen");
        let loaded = mirror.load();
        assert_eq!(loaded.len(), 1);
        let (loaded_id, mirrored) = &loaded[0];
        assert_eq!(*loaded_id, agent);
        assert_eq!(mirrored.host, "local");
        assert_eq!(mirrored.head, head(agent));
        assert_eq!(mirrored.story, told());
        assert_eq!(mirrored.attention, Some(Attention::NeedsInput));
    }

    /// A story with a hole in it is worse than none: the fold over it would
    /// say the wrong thing, so it is dropped and asked for again.
    #[test]
    fn a_story_that_does_not_start_at_zero_is_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let agent = agent_id(2);
        let mirror = Mirror::open(dir.path()).expect("open");
        mirror.write_head("local", head(agent));
        mirror.write_story(agent, UiStoryPos(3), told());
        mirror.flush();

        let loaded = mirror.load();
        assert!(loaded[0].1.story.is_empty());
    }

    /// An agent the daemon no longer lists leaves nothing behind, or Home
    /// would rank work that does not exist.
    #[test]
    fn forgetting_an_agent_takes_its_story_with_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (kept, gone) = (agent_id(3), agent_id(4));
        let mirror = Mirror::open(dir.path()).expect("open");
        for agent in [kept, gone] {
            mirror.write_head("local", head(agent));
            mirror.write_story(agent, UiStoryPos(0), told());
        }
        mirror.forget(vec![gone]);
        mirror.flush();

        let loaded = mirror.load();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].0, kept);
        assert_eq!(loaded[0].1.story, told());
    }
}
