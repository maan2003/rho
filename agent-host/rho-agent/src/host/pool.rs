//! The agent host's worksets and the agents loaded in them.
//!
//! The pool owns one process per workset
//! ([ARCH-agent-host](../../specs/ARCH-agent-host.md)), the id → loaded-agent
//! map, and the answers to each workset's `_rho` calls. The store is the truth
//! about worksets, agents and chat; the pool holds only what runs.

mod gccp;
mod mail;
mod naming;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Context as _;
pub use gccp::{
    Envelope, MAX_BODY_LEN, MessageResult, RETRY_WAITS, Reason, Transport, WIRE_MAX_BODY_LEN,
};
pub use mail::MailView;
use rho_agent_types::{
    Address, AgentId, ChatSeq, ContentPart, Mention, Message, MessageId, UnixMs, WorksetKind,
};
use rho_db::RhoDb;
use rho_fs_view::{Layout, WorksetId, Worksets};
use tokio::sync::Mutex;

use super::AgentClient;
use crate::db::{AgentReadTxnExt as _, AgentWriteTxnExt as _};
use crate::inference::Accounts;
use crate::ipc::protocol::Control;
use crate::ipc::workset::{PyReply, PyRequest};
use crate::log::{AGENT_USAGE_BUCKET_MS, AgentUsageBucket};
use crate::record::{AgentSpec, Event, Record, Runtime};
use crate::store::{
    GccpWrite as _, MailRead as _, MailWrite as _, NewMessage, StoreRead as _, StoreWrite as _,
};
use crate::{AgentStatus, InferenceState, RuntimeState};

/// How many agents stay loaded. Past this the least recently used one
/// that is settled and nobody is looking at is dropped; its log is the
/// whole of it, so nothing is lost.
pub const MAX_LOADED: usize = 100;
/// How long a `_rho.inbox` call waits for a message before it answers with
/// none. A handoff answers it at once instead.
const INBOX_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Default)]
struct ExecutionSlot {
    admission: Arc<tokio::sync::RwLock<()>>,
    process: Mutex<Option<Arc<crate::host::Process>>>,
}
pub struct AgentPool {
    processes: Mutex<HashMap<WorksetId, Arc<ExecutionSlot>>>,
    db: RhoDb,
    inference: Accounts,
    /// The worksets' directories, named by the agent host rather than
    /// resolved here: a library does not reach for the user's state
    /// directory.
    worksets: Arc<Worksets>,
    /// The Claude configuration agents run against, named by the agent host for
    /// the same reason as `worksets`: a library that resolves `$HOME` puts
    /// every caller on the user's live `~/.claude`.
    claude: rho_claude::accounts::ClaudePaths,
    /// The host's `init.py`, run by each workset process at its start.
    init: Option<camino::Utf8PathBuf>,
    /// The host's iroh endpoint id, once it has one: where GCCP reaches its
    /// agents.
    endpoint: std::sync::OnceLock<rho_agent_types::EndpointId>,
    /// The GCCP messages taken last, so that one sent again is not given to
    /// its agent twice.
    gccp_stored: std::sync::Mutex<gccp::Stored>,
    /// What carries GCCP messages out, while the host's endpoint is up, and
    /// the waits before the tries after the first.
    gccp_transport: std::sync::Mutex<Option<gccp::Carrier>>,
    agents: Mutex<HashMap<AgentId, AgentClient>>,
    /// Loaded agents, least recently used first. Touched by every load.
    recent: std::sync::Mutex<std::collections::VecDeque<AgentId>>,
    /// Which agents each connection is looking at; the union is the live
    /// set. A connection that goes away takes its wants with it.
    live_wants: std::sync::Mutex<HashMap<u64, HashSet<AgentId>>>,
    /// The live set: agents whose statuses carry their response body.
    live: std::sync::Mutex<HashSet<AgentId>>,
    /// Every GUI connection's statuses still to send.
    statuses: std::sync::Mutex<Vec<std::sync::Weak<Statuses>>>,
    /// What each agent is doing as its status last said, and since when:
    /// the one clock every device reads an agent's age by.
    doing: std::sync::Mutex<HashMap<AgentId, rho_agent_types::Doing>>,
    mail: mail::Mail,
    /// Per-id activation serialization; unrelated cold loads remain
    /// concurrent, while one persisted agent can never restore two loops.
    load_locks: Mutex<HashMap<AgentId, Arc<Mutex<()>>>>,
    usage: Mutex<HashMap<(AgentId, u64), AgentUsageBucket>>,
    /// The first agent the running `Eval` of a workset created, once it
    /// created one.
    first_agent: std::sync::Mutex<HashMap<WorksetId, Option<AgentId>>>,
    /// A handoff is waiting for every request to be answered: a long
    /// `inbox` wait answers empty at once, and Python asks again of the
    /// next host.
    handing: tokio::sync::watch::Sender<bool>,
}

/// One GUI connection's statuses still to send: the latest of each loaded
/// agent whose status changed since the connection last took them. A newer
/// status replaces an unsent one, so a slow connection holds at most one
/// per agent and never falls behind.
#[derive(Default)]
pub struct Statuses {
    unsent: std::sync::Mutex<HashMap<AgentId, Shown>>,
    changed: tokio::sync::Notify,
}

/// A loaded agent's status as a connection is told it, and since when the
/// agent has been doing what the status says.
#[derive(Clone, Debug)]
pub struct Shown {
    pub status: Arc<AgentStatus>,
    pub since: UnixMs,
}

impl Statuses {
    /// Every status not yet taken.
    pub fn take(&self) -> HashMap<AgentId, Shown> {
        std::mem::take(&mut *self.unsent.lock().expect("poison"))
    }

    /// Resolves once a status arrives after the last wait.
    pub async fn changed(&self) {
        self.changed.notified().await;
    }

    fn put(&self, agent_id: AgentId, shown: Shown) {
        self.unsent.lock().expect("poison").insert(agent_id, shown);
        self.changed.notify_one();
    }
}

/// When the agent started what `runtime` says it does: `now`, unless its
/// last status said it was doing it already. A retry streak keeps the
/// start its runtime gives: its attempts show as working in between.
fn stamp(
    doing: &mut HashMap<AgentId, rho_agent_types::Doing>,
    agent_id: AgentId,
    runtime: &RuntimeState,
    now: UnixMs,
) -> UnixMs {
    let activity = runtime.activity();
    let held = doing.entry(agent_id).or_insert(rho_agent_types::Doing {
        activity,
        since: now,
    });
    if held.activity != activity {
        *held = rho_agent_types::Doing {
            activity,
            since: now,
        };
    }
    // The runtime knows when a streak or a stop began; the host only when
    // it heard of it, and two stops can reach it as one.
    let said = match runtime.inference {
        InferenceState::Retrying { since, .. } => Some(since),
        InferenceState::Failed { since, .. } | InferenceState::Lost { since, .. } => since,
        _ => None,
    };
    if let Some(since) = said {
        held.since = since;
    }
    held.since
}

/// What code the GUI ran printed, its traceback if it raised, and the
/// first agent it created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evaluated {
    pub output: String,
    pub error: Option<String>,
    pub agent: Option<AgentId>,
}

/// Why a requested host directory cannot be used. Callers may describe
/// it with the name the user gave this host.
#[derive(Debug)]
pub struct HostDirectoryError {
    path: camino::Utf8PathBuf,
    reason: DirectoryProblem,
}

#[derive(Debug)]
enum DirectoryProblem {
    Relative,
    Missing,
    NotDirectory,
    Unreadable(std::io::Error),
}

impl std::fmt::Display for HostDirectoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let path = &self.path;
        match &self.reason {
            DirectoryProblem::Relative => write!(f, "{path} is not an absolute path"),
            DirectoryProblem::Missing => write!(f, "{path} does not exist"),
            DirectoryProblem::NotDirectory => write!(f, "{path} is not a directory"),
            DirectoryProblem::Unreadable(error) => write!(f, "cannot read {path}: {error}"),
        }
    }
}

impl std::error::Error for HostDirectoryError {}

fn check_host_directory(path: &camino::Utf8Path) -> Result<(), HostDirectoryError> {
    let reason = if !path.is_absolute() {
        DirectoryProblem::Relative
    } else {
        match std::fs::metadata(path) {
            Ok(info) if info.is_dir() => return Ok(()),
            Ok(_) => DirectoryProblem::NotDirectory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => DirectoryProblem::Missing,
            Err(error) => DirectoryProblem::Unreadable(error),
        }
    };
    Err(HostDirectoryError {
        path: path.to_owned(),
        reason,
    })
}

impl AgentPool {
    /// Opens the pool over `db`, initializing its tables.
    pub async fn new(
        db: RhoDb,
        inference: Accounts,
        worksets: Arc<Worksets>,
        claude: rho_claude::accounts::ClaudePaths,
        init: Option<camino::Utf8PathBuf>,
    ) -> Arc<Self> {
        crate::db::prepare(&db).await;
        // The account agents run on has to exist before the first spawn.
        let account = db.read().claude_account();
        if let Err(error) = claude.bootstrap(&account) {
            panic!("Claude account {account} could not be prepared: {error:#}");
        }
        let pool = Arc::new(Self {
            processes: Mutex::new(HashMap::new()),
            db,
            inference: inference.clone(),
            worksets,
            claude,
            init,
            endpoint: std::sync::OnceLock::new(),
            gccp_stored: std::sync::Mutex::default(),
            gccp_transport: std::sync::Mutex::default(),
            agents: Mutex::new(HashMap::new()),
            recent: std::sync::Mutex::new(std::collections::VecDeque::new()),
            live_wants: std::sync::Mutex::new(HashMap::new()),
            live: std::sync::Mutex::new(HashSet::new()),
            statuses: std::sync::Mutex::new(Vec::new()),
            doing: std::sync::Mutex::new(HashMap::new()),
            mail: mail::Mail::default(),
            load_locks: Mutex::new(HashMap::new()),
            usage: Mutex::new(HashMap::new()),
            first_agent: std::sync::Mutex::default(),
            handing: tokio::sync::watch::Sender::new(false),
        });
        let weak = Arc::downgrade(&pool);
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(std::time::Duration::from_millis(AGENT_USAGE_BUCKET_MS));
            interval.tick().await;
            loop {
                interval.tick().await;
                let Some(pool) = weak.upgrade() else {
                    break;
                };
                pool.flush_agent_usage(None).await;
            }
        });
        pool
    }

    async fn execution_slot(&self, workset: WorksetId) -> Arc<ExecutionSlot> {
        self.processes
            .lock()
            .await
            .entry(workset)
            .or_default()
            .clone()
    }

    /// The process of an agent's workset, started if it is not running.
    pub async fn execution(
        self: &Arc<Self>,
        agent: AgentId,
    ) -> anyhow::Result<Arc<crate::host::Process>> {
        let workset = self.workset_of(agent)?;
        let slot = self.execution_slot(workset).await;
        let _admission = slot.admission.clone().read_owned().await;
        self.process(workset).await
    }

    /// The one agent of the host, in any workset, whose id starts with
    /// `prefix`: agents name each other by a short prefix of the id.
    fn resolve_agent(&self, prefix: &str) -> anyhow::Result<AgentId> {
        anyhow::ensure!(!prefix.is_empty(), "no agent id given");
        let found = self
            .db
            .read()
            .agents()
            .into_iter()
            .map(|(agent, _)| agent)
            .filter(|agent| agent.encoded().starts_with(prefix))
            .collect::<Vec<_>>();
        match found[..] {
            [agent] => Ok(agent),
            [] => anyhow::bail!("no agent {prefix} on this host"),
            _ => anyhow::bail!("{prefix} names {} agents; give more of its id", found.len()),
        }
    }

    fn workset_of(&self, agent: AgentId) -> anyhow::Result<WorksetId> {
        Ok(self
            .db
            .read()
            .agent(agent)
            .context("agent was deleted or does not exist")?
            .workset)
    }

    pub async fn executions(&self) -> Vec<Arc<crate::host::Process>> {
        let slots = self
            .processes
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut processes = Vec::new();
        for slot in slots {
            if let Some(process) = slot
                .process
                .lock()
                .await
                .as_ref()
                .filter(|process| !*process.closed.borrow())
            {
                processes.push(process.clone());
            }
        }
        processes
    }

    /// The workset's process, started if it is not running: its view, its
    /// root, and the host's `init.py`.
    pub(crate) async fn process(
        self: &Arc<Self>,
        workset: WorksetId,
    ) -> anyhow::Result<Arc<crate::host::Process>> {
        let slot = self.execution_slot(workset).await;
        let mut process = slot.process.lock().await;
        if let Some(process) = process.as_ref().filter(|process| !*process.closed.borrow()) {
            return Ok(process.clone());
        }
        let head = self
            .db
            .read()
            .workset(workset)
            .context("workset was deleted or does not exist")?;
        let kind = head.kind;
        let directory = self.worksets.open_workset(workset).await?;
        let root = workset_root(&kind);
        let layout = match kind {
            WorksetKind::Isolated => Layout::workset(&directory)?,
            WorksetKind::Host { .. } => Layout {
                state: directory.state_dir()?,
                ..Layout::host(&self.worksets)
            },
        };
        let started = crate::host::Process::start(
            self,
            workset,
            head.props,
            layout,
            root,
            self.init.clone(),
            self.claude.clone(),
            slot.admission.clone(),
            self.python_handler(workset),
        )
        .await?;
        *process = Some(started.clone());
        Ok(started)
    }

    /// Start the workset's process in the background, so its policy runs:
    /// a message just arrived for one of its agents.
    fn wake_workset(self: &Arc<Self>, workset: WorksetId) {
        let pool = self.clone();
        tokio::spawn(async move {
            let slot = pool.execution_slot(workset).await;
            let _admission = slot.admission.clone().read_owned().await;
            if let Err(error) = pool.process(workset).await {
                eprintln!("rho-agent: starting workset {workset}: {error:#}");
            }
        });
    }

    /// Readies every workset process for a re-executed agent host to take
    /// over and describes them for it. The host stops reading worker
    /// requests and keeps reading everything else until every request it
    /// started, anywhere, is answered: one may wait on a frame from another
    /// workset. Then it stops reading altogether. If that takes too long,
    /// all carry on and this errs.
    pub async fn hand_over(&self) -> anyhow::Result<Vec<crate::host::Handed>> {
        let slots = self
            .processes
            .lock()
            .await
            .iter()
            .map(|(workset, slot)| (*workset, slot.clone()))
            .collect::<Vec<_>>();
        let mut processes = Vec::new();
        for (workset, slot) in slots {
            if let Some(process) = slot
                .process
                .lock()
                .await
                .as_ref()
                .filter(|process| !*process.closed.borrow())
            {
                processes.push((workset, process.clone()));
            }
        }
        let idle = || async {
            while !processes.iter().all(|(_, process)| process.idle()) {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        };
        let handed = tokio::time::timeout(std::time::Duration::from_secs(60), async {
            futures::future::try_join_all(
                processes.iter().map(|(_, process)| process.stop_requests()),
            )
            .await?;
            self.handing.send_replace(true);
            idle().await;
            futures::future::try_join_all(
                processes.iter().map(|(_, process)| process.stop_reading()),
            )
            .await?;
            futures::future::try_join_all(
                processes
                    .iter()
                    .map(|(workset, process)| process.hand(*workset)),
            )
            .await
        })
        .await
        .context("workset processes did not settle")
        .flatten();
        if handed.is_err() {
            self.handing.send_replace(false);
            for (_, process) in &processes {
                process.resume();
            }
        }
        handed
    }

    /// Undoes a [`AgentPool::hand_over`] whose exec failed.
    pub async fn resume(&self) {
        self.handing.send_replace(false);
        for process in self.executions().await {
            process.resume();
        }
    }
    /// Replaces a workset's process with one of this agent host's build and
    /// loads its agents again there. Unforced, only a stale process with
    /// nothing running is replaced: every agent retires, and no terminal
    /// or shell is open. Forced, whatever runs there ends. Says
    /// whether it replaced one.
    pub async fn restart_workset(
        self: &Arc<Self>,
        workset: WorksetId,
        force: bool,
    ) -> anyhow::Result<bool> {
        let slot = self.execution_slot(workset).await;
        let admission = slot.admission.clone().write_owned().await;
        let mut current = slot.process.lock().await;
        let Some(process) = current.clone().filter(|process| !*process.closed.borrow()) else {
            return Ok(false);
        };
        if !force && !(process.stale() && process.quiet().await?) {
            return Ok(false);
        }
        let ids = process
            .agents
            .lock()
            .expect("poison")
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let mut agents = Vec::new();
        for agent_id in &ids {
            let lock = self
                .load_locks
                .lock()
                .await
                .entry(*agent_id)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone();
            let loading = lock.lock_owned().await;
            if let Some(agent) = self.agents.lock().await.get(agent_id).cloned() {
                agents.push((*agent_id, agent, loading));
            }
        }
        let mut replace = true;
        if !force {
            // Asking first leaves no agent retired, and so frozen, when
            // another still has work; retirement then only confirms.
            if !agents.iter().all(|(_, agent, _)| agent.settled()) {
                return Ok(false);
            }
            // One that took on work meanwhile keeps the process; those
            // retired already are frozen and load again in it.
            if let Some(busy) =
                futures::future::join_all(agents.iter().map(|(_, agent, _)| agent.retire()))
                    .await
                    .iter()
                    .position(Result::is_err)
            {
                agents.remove(busy);
                replace = false;
            }
        }
        futures::future::join_all(agents.iter().map(|(_, agent, _)| agent.shutdown())).await;
        {
            let mut loaded = self.agents.lock().await;
            let mut recent = self.recent.lock().expect("poison");
            for (agent_id, _, _) in &agents {
                loaded.remove(agent_id);
                recent.retain(|id| id != agent_id);
            }
        }
        if replace {
            process.shutdown().await;
            *current = None;
        }
        let ids = agents
            .iter()
            .map(|(agent_id, _, _)| *agent_id)
            .collect::<Vec<_>>();
        drop((current, admission, agents));
        let mut reload_error = None;
        for agent_id in ids {
            if let Err(error) = self.load(agent_id).await {
                let error = error.context(format!("reload agent {}", agent_id.encoded()));
                eprintln!("rho-agent: {error:#}");
                reload_error.get_or_insert(error);
            }
        }
        if let Some(error) = reload_error {
            return Err(error);
        }
        Ok(replace)
    }

    /// User-requested recovery must work without the workset's Python. A
    /// failed or unloaded selected agent is loaded even if there was no live
    /// worker to replace. Saved tool calls are not replayed.
    pub async fn restart_agent_workset(self: &Arc<Self>, agent: AgentId) -> anyhow::Result<()> {
        let workset = self.workset_of(agent)?;
        self.restart_workset(workset, true).await?;
        self.load(agent).await?;
        Ok(())
    }

    /// Replaces stale workset processes as they fall quiet, checking every
    /// minute.
    pub fn replace_stale_worksets(self: &Arc<Self>) {
        let pool = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                interval.tick().await;
                let Some(pool) = pool.upgrade() else { return };
                let slots = pool
                    .processes
                    .lock()
                    .await
                    .iter()
                    .map(|(workset, slot)| (*workset, slot.clone()))
                    .collect::<Vec<_>>();
                for (workset, slot) in slots {
                    let stale = slot
                        .process
                        .lock()
                        .await
                        .as_ref()
                        .is_some_and(|process| process.stale() && !*process.closed.borrow());
                    if !stale {
                        continue;
                    }
                    let label = format!("workset {workset}");
                    match pool.restart_workset(workset, false).await {
                        Ok(true) => eprintln!("rho-agent: replaced stale {label}"),
                        Ok(false) => {}
                        Err(error) => eprintln!("rho-agent: replacing stale {label}: {error:#}"),
                    }
                }
            }
        });
    }

    /// Takes over the workset processes, and their agents, that the agent
    /// host this one re-executed handed over.
    pub async fn adopt(self: &Arc<Self>, handed: Vec<crate::host::Handed>) {
        for handed in handed {
            if let Err(error) = self.adopt_process(&handed).await {
                let label = format!("workset {}", handed.workset);
                eprintln!("rho-agent: not taking over {label}: {error:#}");
            }
        }
    }

    async fn adopt_process(self: &Arc<Self>, handed: &crate::host::Handed) -> anyhow::Result<()> {
        let slot = self.execution_slot(handed.workset).await;
        let process = crate::host::Process::adopt(
            self,
            handed,
            slot.admission.clone(),
            self.python_handler(handed.workset),
        )
        .await?;
        *slot.process.lock().await = Some(process.clone());
        for &agent_id in &handed.agents {
            match AgentClient::adopt(self, agent_id, process.clone()).await {
                Ok(agent) => {
                    agent.tell_tail();
                    self.agents.lock().await.insert(agent_id, agent);
                    self.touch(agent_id);
                }
                Err(error) => eprintln!(
                    "rho-agent: not taking over {}: {error:#}",
                    agent_id.encoded()
                ),
            }
        }
        process.resume();
        Ok(())
    }

    pub fn worksets(&self) -> &Arc<Worksets> {
        &self.worksets
    }

    pub fn db(&self) -> &RhoDb {
        &self.db
    }

    pub async fn record_agent_usage(&self, agent_id: AgentId, mut usage: AgentUsageBucket) {
        let now = rho_agent_types::UnixMs::now().0;
        usage.bucket_start_ms = now / AGENT_USAGE_BUCKET_MS * AGENT_USAGE_BUCKET_MS;
        let mut pending = self.usage.lock().await;
        pending
            .entry((agent_id, usage.bucket_start_ms))
            .or_insert_with(|| AgentUsageBucket {
                bucket_start_ms: usage.bucket_start_ms,
                ..AgentUsageBucket::default()
            })
            .add(&usage);
    }

    pub async fn flush_agent_usage(&self, only: Option<AgentId>) {
        let drained = {
            let mut pending = self.usage.lock().await;
            let keys = pending
                .keys()
                .filter(|(agent_id, _)| only.is_none_or(|only| only == *agent_id))
                .copied()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| pending.remove(&key).map(|usage| (key.0, usage)))
                .collect::<Vec<_>>()
        };
        if drained.is_empty() {
            return;
        }
        let mut write = self.db.write().await;
        for (agent_id, usage) in &drained {
            if write.agent_head(*agent_id).is_some() {
                write.add_agent_usage(*agent_id, usage);
            }
        }
        write.commit();
    }

    pub fn inference(&self) -> &Accounts {
        &self.inference
    }

    pub async fn get(&self, agent_id: AgentId) -> Option<AgentClient> {
        let lock = self
            .load_locks
            .lock()
            .await
            .entry(agent_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _loading = lock.lock_owned().await;
        let agent = self.agents.lock().await.get(&agent_id).cloned();
        if agent.is_some() {
            self.touch(agent_id);
        }
        agent
    }

    /// The loaded agents with work in hand: responding, running tasks, or
    /// holding input. An agent that is not loaded has none.
    pub async fn working(&self) -> Vec<AgentId> {
        self.agents
            .lock()
            .await
            .iter()
            .filter(|(_, agent)| !agent.status_cell().borrow().settled())
            .map(|(agent_id, _)| *agent_id)
            .collect()
    }

    /// Whether any client is looking at this agent right now. Read by the
    /// loops on every publish, so it is a plain lock and no await.
    pub fn is_live(&self, agent_id: AgentId) -> bool {
        self.live.lock().expect("poison").contains(&agent_id)
    }

    /// One connection's whole focus set, replacing what it wanted before.
    /// The live set is the union over connections; a loaded agent entering
    /// it is told again with its response body, and one leaving it is told
    /// without from its next status on.
    pub async fn set_live_wants(self: &Arc<Self>, connection: u64, wants: HashSet<AgentId>) {
        let union = {
            let mut live_wants = self.live_wants.lock().expect("poison");
            if wants.is_empty() {
                live_wants.remove(&connection);
            } else {
                live_wants.insert(connection, wants);
            }
            live_wants
                .values()
                .flatten()
                .copied()
                .collect::<HashSet<_>>()
        };
        let joined = {
            let mut live = self.live.lock().expect("poison");
            let joined = union
                .iter()
                .copied()
                .filter(|agent_id| !live.contains(agent_id))
                .collect::<Vec<_>>();
            *live = union;
            joined
        };
        let agents = self.agents.lock().await;
        for agent_id in joined {
            if let Some(agent) = agents.get(&agent_id) {
                self.tell_status(agent_id, agent);
            }
        }
    }

    /// Tells every connection a loaded agent's status: once it is loaded,
    /// and again with its response body once it is live.
    fn tell_status(self: &Arc<Self>, agent_id: AgentId, agent: &AgentClient) {
        self.status_changed(agent_id, agent.status_cell());
    }

    /// Puts the current status of each loaded agent named into one
    /// connection's statuses again, so an agent it newly focuses shows its
    /// response and draft at once rather than at its next change.
    pub async fn resend_statuses(
        &self,
        statuses: &Statuses,
        agent_ids: impl IntoIterator<Item = AgentId>,
    ) {
        let agents = self.agents.lock().await;
        for agent_id in agent_ids {
            if let Some(agent) = agents.get(&agent_id) {
                statuses.put(
                    agent_id,
                    self.shown(agent_id, &agent.status_cell().borrow()),
                );
            }
        }
    }

    /// Every loaded agent's status now, and each status that changes from
    /// now on, for one GUI connection. Its statuses go when it drops them.
    pub async fn watch_statuses(&self) -> Arc<Statuses> {
        let statuses = Arc::new(Statuses::default());
        let agents = self.agents.lock().await;
        let mut watchers = self.statuses.lock().expect("poison");
        watchers.push(Arc::downgrade(&statuses));
        for (agent_id, agent) in agents.iter() {
            statuses.put(
                *agent_id,
                self.shown(*agent_id, &agent.status_cell().borrow()),
            );
        }
        statuses
    }

    /// An agent's status changed; every connection gets it as it shows.
    /// It is read under the connections' lock, so of two calls racing, the
    /// later never puts an older status.
    pub(crate) fn status_changed(
        self: &Arc<Self>,
        agent_id: AgentId,
        status: &tokio::sync::watch::Sender<AgentStatus>,
    ) {
        let mut watchers = self.statuses.lock().expect("poison");
        let status = status.borrow();
        let changed = {
            let mut held = self.doing.lock().expect("poison");
            let previous = held.get(&agent_id).copied();
            stamp(&mut held, agent_id, &status.runtime, UnixMs::now());
            let now = held[&agent_id];
            (previous != Some(now)).then_some((previous, now))
        };
        if let Some((previous, doing)) = changed {
            self.activity_changed(agent_id, previous, doing);
            self.observe_stop(agent_id, doing, &status.runtime);
        }
        let shown = self.shown(agent_id, &status);
        watchers.retain(|statuses| match statuses.upgrade() {
            Some(statuses) => {
                statuses.put(agent_id, shown.clone());
                true
            }
            None => false,
        });
    }

    /// Only an agent someone is looking at shows its response body; its
    /// runtime occupancy is useful everywhere.
    fn shown(&self, agent_id: AgentId, status: &AgentStatus) -> Shown {
        let since = self
            .doing
            .lock()
            .expect("poison")
            .get(&agent_id)
            .map_or_else(UnixMs::now, |doing| doing.since);
        let mut status = if self.is_live(agent_id) {
            status.clone()
        } else {
            AgentStatus {
                runtime: status.runtime.clone(),
                response: None,
                draft: None,
                queued: status.queued,
            }
        };
        status.runtime.may_change_runtime_kind =
            self.may_change_runtime_kind(agent_id, status.runtime.activity());
        Shown {
            status: Arc::new(status),
            since,
        }
    }

    fn may_change_runtime_kind(&self, agent: AgentId, activity: rho_agent_types::Activity) -> bool {
        matches!(
            activity,
            rho_agent_types::Activity::Failed | rho_agent_types::Activity::Lost
        ) && self
            .db
            .read()
            .agent(agent)
            .is_some_and(|head| !head.native.woken)
    }

    fn touch(&self, agent_id: AgentId) {
        let mut recent = self.recent.lock().expect("poison");
        recent.retain(|id| *id != agent_id);
        recent.push_back(agent_id);
    }

    /// Drop the least recently used loaded agents past [`MAX_LOADED`],
    /// skipping any that is mid-turn, has input waiting, or is being
    /// looked at. Dropping the last handle ends its loop.
    async fn trim(self: &Arc<Self>, limit: usize) {
        let candidates = self
            .recent
            .lock()
            .expect("poison")
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for agent_id in candidates {
            if self.agents.lock().await.len() <= limit {
                break;
            }
            let pool = self.clone();
            // The task retains the per-ID lock through removal and drain even
            // if the caller that triggered trimming is cancelled.
            let _ = tokio::spawn(async move {
                let lock = pool
                    .load_locks
                    .lock()
                    .await
                    .entry(agent_id)
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone();
                let _loading = lock.lock_owned().await;
                let agent = {
                    let agents = pool.agents.lock().await;
                    if agents.len() <= limit
                        || pool.is_live(agent_id)
                        || !agents
                            .get(&agent_id)
                            .is_some_and(|agent| agent.settled() && agent.pool_only())
                    {
                        return;
                    }
                    let candidate = agents.get(&agent_id).expect("checked above").clone();
                    drop(agents);
                    if candidate.retire().await.is_err() {
                        return;
                    }
                    let mut agents = pool.agents.lock().await;
                    let agent = agents
                        .remove(&agent_id)
                        .expect("per-agent ownership lock held");
                    pool.recent
                        .lock()
                        .expect("poison")
                        .retain(|id| *id != agent_id);
                    agent
                };
                agent.shutdown().await;
            })
            .await;
        }
    }

    /// A new workset, its first row, and its process running `code`.
    pub async fn create_workset(
        self: &Arc<Self>,
        kind: WorksetKind,
        code: String,
    ) -> anyhow::Result<(WorksetId, Evaluated)> {
        if let WorksetKind::Host { cwd } = &kind {
            check_host_directory(cwd)?;
        }
        // Every workset has a directory: its state lives there, and an
        // isolated one's `/src`.
        let workset = self.worksets.create().await?.id();
        let mut write = self.db.write().await;
        write.create_workset(workset, UnixMs::now(), kind, code.clone());
        write.commit();
        let evaluated = self.run(workset, code).await?;
        Ok((workset, evaluated))
    }

    /// Run `code` in the workset namespace, logged first.
    pub async fn eval(
        self: &Arc<Self>,
        workset: WorksetId,
        code: String,
    ) -> anyhow::Result<Evaluated> {
        {
            let mut write = self.db.write().await;
            write.append(
                workset,
                &Record {
                    at: UnixMs::now(),
                    event: Event::Evaluated { code: code.clone() },
                },
            )?;
            write.commit();
        }
        self.run(workset, code).await
    }

    async fn run(self: &Arc<Self>, workset: WorksetId, code: String) -> anyhow::Result<Evaluated> {
        let slot = self.execution_slot(workset).await;
        let process = {
            let _admission = slot.admission.clone().read_owned().await;
            self.process(workset).await?
        };
        self.first_agent
            .lock()
            .expect("poison")
            .insert(workset, None);
        let result = process.eval(code).await;
        let agent = self
            .first_agent
            .lock()
            .expect("poison")
            .remove(&workset)
            .flatten();
        let (output, error) = match result {
            Ok(evaluated) => evaluated,
            Err(error) if agent.is_some() => (String::new(), Some(format!("{error:#}"))),
            Err(error) => return Err(error),
        };
        Ok(Evaluated {
            output,
            error,
            agent,
        })
    }

    /// A message from the user, under `parent` or as a root. A resend under
    /// the same `client_id` is the same message.
    pub async fn post_user(
        self: &Arc<Self>,
        client_id: u64,
        parent: Option<MessageId>,
        reply_to: Option<MessageId>,
        mentions: Vec<AgentId>,
        content: Vec<ContentPart>,
    ) -> anyhow::Result<MessageId> {
        let mut mentions = self.user_mentions(mentions);
        let mut write = self.db.write().await;
        // Resolve the writer on the host, even if they are no longer a
        // thread member. A deleted agent cannot receive mail, but the answer
        // still belongs in the thread.
        if let Some(target) = reply_to
            && let Some(agent) = write.mail_writer(target)
            && self.workset_of(agent).is_ok()
            && mentions
                .iter()
                .all(|mention| mention.agent() != Some(agent))
        {
            mentions.push(Mention::Agent(agent));
        }
        let message = NewMessage {
            from: Address::User,
            reply_to,
            mentions,
            content,
        };
        let id = write.post(UnixMs::now(), parent, message, Some(client_id))?;
        write.commit();
        self.user_visited(id);
        self.wake_receivers(id, None);
        self.consider_naming(id);
        Ok(id)
    }

    /// The user takes `agent` out of `thread`.
    pub async fn leave_user(&self, thread: MessageId, agent: AgentId) -> anyhow::Result<()> {
        let mut write = self.db.write().await;
        write.leave(UnixMs::now(), thread, agent)?;
        write.commit();
        Ok(())
    }

    /// The user puts a message and its thread under `parent`, or among the
    /// roots.
    pub async fn reparent_user(
        &self,
        message: MessageId,
        parent: Option<MessageId>,
    ) -> anyhow::Result<()> {
        let mut write = self.db.write().await;
        write.reparent(UnixMs::now(), message, parent)?;
        write.commit();
        Ok(())
    }

    /// The user names a thread; empty takes the name back.
    pub async fn name_user(&self, thread: MessageId, name: String) -> anyhow::Result<()> {
        let mut write = self.db.write().await;
        write.name(UnixMs::now(), thread, name)?;
        write.commit();
        Ok(())
    }

    /// Give the user's mail to a named writer if they still exist. A writer
    /// can be removed after the mail was read or while the GUI was offline.
    /// That cannot stop the user's reply from being saved in its thread.
    fn user_mentions(&self, agents: Vec<AgentId>) -> Vec<Mention> {
        let read = self.db.read();
        let mut seen = HashSet::new();
        agents
            .into_iter()
            .filter(|agent| seen.insert(*agent) && read.agent(*agent).is_some())
            .map(Mention::Agent)
            .collect()
    }

    /// Start the process of every workset with an agent that receives
    /// `message`, but `except`, so its policy can deliver it.
    fn wake_receivers(self: &Arc<Self>, message: MessageId, except: Option<WorksetId>) {
        let read = self.db.read();
        let Some(message) = read.message(message) else {
            return;
        };
        let mut worksets: Vec<WorksetId> = read
            .recipients_of(&message)
            .into_iter()
            .filter_map(|agent| read.agent(agent).map(|head| head.workset))
            .filter(|workset| Some(*workset) != except)
            .collect();
        drop(read);
        worksets.sort();
        worksets.dedup();
        for workset in worksets {
            self.wake_workset(workset);
        }
    }

    /// End this agent's work without stopping its workset or posting mail.
    pub async fn stop_agent(self: &Arc<Self>, agent: AgentId) -> anyhow::Result<()> {
        let lock = self
            .load_locks
            .lock()
            .await
            .entry(agent)
            .or_default()
            .clone();
        let _ownership = lock.lock().await;
        let workset = self.workset_of(agent)?;
        let removed = { self.agents.lock().await.remove(&agent) };
        if let Some(client) = removed {
            client.shutdown().await;
        }
        let at = UnixMs::now();
        let mut write = self.db.write().await;
        write.append(
            workset,
            &Record {
                at,
                event: Event::Notice {
                    agent,
                    notice: crate::entry::Notice::Archived,
                },
            },
        )?;
        write.commit();
        let (status, _) = tokio::sync::watch::channel(AgentStatus {
            runtime: RuntimeState {
                archived: true,
                ..RuntimeState::default()
            },
            ..AgentStatus::default()
        });
        self.status_changed(agent, &status);
        self.user_stopped(agent);
        Ok(())
    }

    /// Rewind the visible turn, returning the headlines marked taken back.
    pub async fn rewind_agent(
        self: &Arc<Self>,
        agent: AgentId,
        turns: u32,
    ) -> anyhow::Result<Vec<String>> {
        let (_, client, _) = self.load(agent).await?;
        let before = self.db.read().mail_head();
        client.control(Control::Rewind(turns)).await?;
        let read = self.db.read();
        Ok(read
            .mail_since(before, usize::MAX)
            .into_iter()
            .filter_map(|row| match row.event {
                rho_agent_types::ChatEvent::TakenBack {
                    message, author, ..
                } if author == agent => read.message(message).map(|mail| mail.headline()),
                _ => None,
            })
            .collect())
    }

    /// Cut off an agent's turn, if it is loaded; one that is not has none.
    pub async fn cancel(&self, agent: AgentId) -> anyhow::Result<bool> {
        let Some(client) = self.get(agent).await else {
            return Ok(false);
        };
        let asking = matches!(
            client.status().runtime.inference,
            InferenceState::Responding | InferenceState::Retrying { .. }
        );
        if asking {
            client.control(Control::Cancel).await?;
        }
        Ok(asking)
    }

    /// Stop each workset's process and erase it: its log, heads, usage,
    /// Claude sessions and directory. Chat stays. Every workset is checked
    /// first, so none is deleted when one is missing.
    pub async fn delete_worksets(self: &Arc<Self>, ids: Vec<WorksetId>) -> anyhow::Result<()> {
        let pool = self.clone();
        // Deletion completes even if the requesting connection goes away.
        tokio::spawn(async move {
            {
                let read = pool.db.read();
                for &id in &ids {
                    anyhow::ensure!(
                        read.workset(id).is_some(),
                        "workset {id} does not exist; none was deleted"
                    );
                }
            }
            for workset in ids {
                pool.delete_workset(workset).await?;
            }
            Ok(())
        })
        .await?
    }

    async fn delete_workset(self: &Arc<Self>, workset: WorksetId) -> anyhow::Result<()> {
        let slot = self.execution_slot(workset).await;
        let _admission = slot.admission.clone().write_owned().await;
        let agents = self
            .db
            .read()
            .workset(workset)
            .map(|head| head.agents)
            .unwrap_or_default();
        let mut locks = Vec::new();
        for &agent in &agents {
            let lock = self
                .load_locks
                .lock()
                .await
                .entry(agent)
                .or_default()
                .clone();
            locks.push(lock.lock_owned().await);
        }
        for &agent in &agents {
            let client = self.agents.lock().await.remove(&agent);
            if let Some(client) = client {
                client.shutdown().await;
            }
        }
        if let Some(process) = slot.process.lock().await.take() {
            process.shutdown().await;
        }
        // Every Claude session the agents ever had, from their specs.
        let sessions = self
            .db
            .read()
            .workset_rows(workset, rho_agent_types::Wpos::ZERO, usize::MAX)
            .into_iter()
            .filter_map(|(_, record)| match record.event {
                Event::AgentCreated { spec, .. } | Event::AgentChanged { spec, .. } => {
                    match spec.runtime {
                        Runtime::Claude { session_id, .. } => Some(session_id),
                        Runtime::Native { .. } => None,
                    }
                }
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        rho_claude::delete_session_files(&self.claude, &sessions)
            .await
            .context("Claude session cleanup failed; the workset is kept for a retry")?;
        for &agent in &agents {
            self.flush_agent_usage(Some(agent)).await;
        }
        {
            let mut write = self.db.write().await;
            // An agent that is gone accepts no one.
            for &agent in &agents {
                write.gccp_forget(agent);
            }
            write.delete_workset(workset);
            for &agent in &agents {
                write.delete_agent_usage(agent);
            }
            write.commit();
        }
        self.doing
            .lock()
            .expect("poison")
            .retain(|agent, _| !agents.contains(agent));
        self.agents_left_mail(&agents);
        self.recent
            .lock()
            .expect("poison")
            .retain(|id| !agents.contains(id));
        self.usage
            .lock()
            .await
            .retain(|(id, _), _| !agents.contains(id));
        for watcher in self
            .statuses
            .lock()
            .expect("poison")
            .iter()
            .filter_map(|weak| weak.upgrade())
        {
            watcher
                .unsent
                .lock()
                .expect("poison")
                .retain(|id, _| !agents.contains(id));
        }
        self.processes.lock().await.remove(&workset);
        self.worksets.delete(workset).await?;
        Ok(())
    }

    /// What answers a workset's `_rho` calls.
    fn python_handler(self: &Arc<Self>, workset: WorksetId) -> crate::host::process::PythonHandler {
        let pool = Arc::downgrade(self);
        Arc::new(move |request| {
            let pool = pool.clone();
            Box::pin(async move {
                let Some(pool) = pool.upgrade() else {
                    return PyReply::Error("the agent host is shutting down".into());
                };
                pool.python(workset, request)
                    .await
                    .unwrap_or_else(|error| PyReply::Error(format!("{error:#}")))
            })
        })
    }

    /// The agent, if it belongs to `workset`: a workset acts on its own
    /// agents only.
    fn own_agent(&self, workset: WorksetId, agent: AgentId) -> anyhow::Result<AgentSpec> {
        let head = self
            .db
            .read()
            .agent(agent)
            .filter(|head| head.workset == workset)
            .with_context(|| format!("no agent {} in this workset", agent.encoded()))?;
        Ok(head.spec)
    }

    async fn python(
        self: &Arc<Self>,
        workset: WorksetId,
        request: PyRequest,
    ) -> anyhow::Result<PyReply> {
        Ok(match request {
            PyRequest::CreateAgent(spec) => {
                let mut write = self.db.write().await;
                let agent = write.alloc_agent_id();
                write.append(
                    workset,
                    &Record {
                        at: UnixMs::now(),
                        event: Event::AgentCreated { agent, spec },
                    },
                )?;
                write.commit();
                if let Some(first) = self.first_agent.lock().expect("poison").get_mut(&workset)
                    && first.is_none()
                {
                    *first = Some(agent);
                }
                PyReply::Agent(agent)
            }
            PyRequest::SetWorksetProps(props) => {
                let mut write = self.db.write().await;
                write.append(
                    workset,
                    &Record {
                        at: UnixMs::now(),
                        event: Event::WorksetPropsSet { props },
                    },
                )?;
                write.commit();
                PyReply::Done
            }
            PyRequest::Agents => {
                let read = self.db.read();
                let agents = read
                    .workset(workset)
                    .context("workset was deleted")?
                    .agents
                    .into_iter()
                    .filter_map(|agent| read.agent(agent).map(|head| (agent, head.spec)))
                    .collect();
                PyReply::Agents(agents)
            }
            PyRequest::SetSpec { agent, spec } => {
                let lock = self
                    .load_locks
                    .lock()
                    .await
                    .entry(agent)
                    .or_default()
                    .clone();
                let _loading = lock.lock().await;
                let head = self
                    .db
                    .read()
                    .agent(agent)
                    .filter(|head| head.workset == workset)
                    .with_context(|| format!("no agent {} in this workset", agent.encoded()))?;
                let changes_kind = std::mem::discriminant(&head.spec.runtime)
                    != std::mem::discriminant(&spec.runtime);
                if changes_kind {
                    let activity = self
                        .doing
                        .lock()
                        .expect("poison")
                        .get(&agent)
                        .map(|doing| doing.activity);
                    anyhow::ensure!(
                        activity
                            .is_some_and(|activity| self.may_change_runtime_kind(agent, activity))
                            || (head.startup_failure.is_some() && !head.native.woken),
                        "only a stopped agent that has never sent a request can change runtime kind"
                    );
                    // Native and Claude have different loops. Stop the old
                    // one before writing the new spec; policy will redeliver
                    // the still-undelivered mail to the new loop.
                    let client = self.agents.lock().await.remove(&agent);
                    if let Some(client) = client {
                        client.shutdown().await;
                    }
                }
                {
                    let mut write = self.db.write().await;
                    write.append(
                        workset,
                        &Record {
                            at: UnixMs::now(),
                            event: Event::AgentChanged { agent, spec },
                        },
                    )?;
                    write.commit();
                }
                if let Some(client) = self.agents.lock().await.get(&agent)
                    && let Some(head) = self.db.read().agent(agent)
                {
                    client.named(head);
                }
                PyReply::Done
            }
            PyRequest::Control { agent, control } => {
                self.own_agent(workset, agent)?;
                let (_, client, _) = self.load(agent).await?;
                client.control(control).await?;
                PyReply::Done
            }
            PyRequest::Delivered(agent) => {
                self.own_agent(workset, agent)?;
                PyReply::Seq(
                    self.db
                        .read()
                        .agent(agent)
                        .context("agent was deleted")?
                        .delivered,
                )
            }
            PyRequest::SendMail {
                sender,
                parent,
                reply_to,
                mentions,
                content,
            } => {
                self.own_agent(workset, sender)?;
                self.known_mentions(&mentions)?;
                let direct = mentions
                    .iter()
                    .any(|mention| matches!(mention, Mention::Direct(_)));
                let project = if direct {
                    anyhow::ensure!(
                        parent.is_none(),
                        "direct mail chooses its own thread; address the agent"
                    );
                    self.mail_project(sender)
                } else {
                    None
                };
                let message = NewMessage {
                    from: Address::Agent(sender),
                    reply_to,
                    mentions,
                    content,
                };
                let mut write = self.db.write().await;
                let at = UnixMs::now();
                let pings = message.mentions.contains(&Mention::User { ping: true });
                let (id, changed, unanswered) = if direct {
                    (write.post_peer(at, project, message)?, true, None)
                } else {
                    let revised = match parent {
                        Some(thread) if pings => write.restate_ping(at, thread, &message)?,
                        _ => None,
                    };
                    let unanswered = if pings {
                        None
                    } else {
                        parent.and_then(|thread| write.unanswered_project_mail(thread))
                    };
                    let (id, changed) = match revised {
                        Some(revised) => revised,
                        None => (write.post(at, parent, message, None)?, true),
                    };
                    (id, changed, unanswered)
                };
                if changed {
                    let version = write.mail_version(id);
                    write.append(
                        workset,
                        &Record {
                            at,
                            event: Event::MailWritten {
                                agent: sender,
                                message: id,
                                version,
                            },
                        },
                    )?;
                    write.commit();
                    self.wake_receivers(id, Some(workset));
                    if pings {
                        self.watch_recap(id);
                    }
                    if !direct {
                        self.consider_naming(id);
                    }
                }
                PyReply::SentMail { id, unanswered }
            }
            PyRequest::Edit {
                agent,
                message,
                content,
                ping,
            } => {
                self.own_agent(workset, agent)?;
                let mut write = self.db.write().await;
                let at = UnixMs::now();
                let before = write.mail_version(message);
                write.edit(at, agent, message, content, ping)?;
                let version = write.mail_version(message);
                if version == before {
                    return Ok(PyReply::Done);
                }
                write.append(
                    workset,
                    &Record {
                        at,
                        event: Event::MailWritten {
                            agent,
                            message,
                            version,
                        },
                    },
                )?;
                write.commit();
                self.watch_recap(message);
                PyReply::Done
            }
            PyRequest::Delete { agent, message } => {
                self.own_agent(workset, agent)?;
                let mut write = self.db.write().await;
                write.delete(UnixMs::now(), agent, message)?;
                write.commit();
                PyReply::Done
            }
            PyRequest::Search {
                query,
                project,
                limit,
            } => PyReply::Search(self.db.read().search_mail(
                &query,
                project,
                limit.min(100) as usize,
            )?),
            PyRequest::ProjectNamed(name) => PyReply::Project(self.db.read().project_named(&name)),
            PyRequest::Message(id) => {
                PyReply::Messages(self.db.read().message(id).into_iter().collect())
            }
            PyRequest::Read { thread, last } => {
                let read = self.db.read();
                if let Some(thread) = thread {
                    anyhow::ensure!(read.message(thread).is_some(), "no message {}", thread.0.0);
                }
                PyReply::Messages(read.thread_messages(thread, last as usize))
            }
            PyRequest::Join { thread, agent } => {
                self.own_agent(workset, agent)?;
                let mut write = self.db.write().await;
                write.join(UnixMs::now(), thread, agent)?;
                write.commit();
                PyReply::Done
            }
            PyRequest::Leave { thread, agent } => {
                self.own_agent(workset, agent)?;
                let mut write = self.db.write().await;
                write.leave(UnixMs::now(), thread, agent)?;
                write.commit();
                PyReply::Done
            }
            PyRequest::Reparent {
                agent,
                message,
                parent,
            } => {
                self.own_agent(workset, agent)?;
                let mut write = self.db.write().await;
                write.reparent(UnixMs::now(), message, parent)?;
                write.commit();
                PyReply::Done
            }
            PyRequest::Name {
                agent,
                thread,
                name,
            } => {
                self.own_agent(workset, agent)?;
                let mut write = self.db.write().await;
                write.name(UnixMs::now(), thread, name)?;
                write.commit();
                PyReply::Done
            }
            PyRequest::Thread(thread) => PyReply::Thread(self.db.read().thread(thread)),
            PyRequest::Inbox { agent, after } => {
                self.own_agent(workset, agent)?;
                PyReply::Messages(self.inbox(agent, after).await)
            }
            PyRequest::Clone { url, dest, rev } => {
                let kind = self
                    .db
                    .read()
                    .workset(workset)
                    .context("workset was deleted")?
                    .kind;
                anyhow::ensure!(
                    kind == WorksetKind::Isolated,
                    "clone works in an isolated workset only"
                );
                let name = dest
                    .strip_prefix(rho_fs_view::MOUNT_ROOT)
                    .ok()
                    .filter(|name| name.components().count() == 1)
                    .with_context(|| {
                        format!(
                            "clone into a directory directly in {}",
                            rho_fs_view::MOUNT_ROOT
                        )
                    })?
                    .to_string();
                let directory = self.worksets.open_workset(workset).await?;
                let checkout = directory.clone_repo(&url, Some(&name)).await?;
                directory.checkout(&checkout, &rev).await?;
                PyReply::Done
            }
            PyRequest::Models => PyReply::Models(models()),
            PyRequest::ResolveAgent(prefix) => PyReply::Agent(self.resolve_agent(&prefix)?),
            PyRequest::AgentHandle(agent) => {
                PyReply::Handle(self.db.read().agent(agent).map(|head| {
                    let role = match head.spec.props.get("preset").map(String::as_str) {
                        Some("advisor") => "adv",
                        _ => "eng",
                    };
                    format!("{role}-{}", agent.encoded())
                }))
            }
            PyRequest::ExternalAddress(agent) => {
                self.own_agent(workset, agent)?;
                PyReply::Text(self.external_address(agent)?.to_string())
            }
            PyRequest::ExternalAccept { agent, peer, name } => {
                self.own_agent(workset, agent)?;
                self.external_accept(agent, &peer, &name).await?;
                PyReply::Done
            }
            PyRequest::ExternalSend { agent, peer, body } => {
                self.own_agent(workset, agent)?;
                PyReply::Text(self.external_send(agent, &peer, body).await?)
            }
            PyRequest::ExternalContacts(agent) => {
                self.own_agent(workset, agent)?;
                let mut contacts = self.gccp_contacts();
                contacts.retain(|contact| contact.agent == agent);
                PyReply::ExternalContacts(contacts)
            }
        })
    }

    /// Fails unless every agent `mentions` names is on this host.
    fn known_mentions(&self, mentions: &[rho_agent_types::Mention]) -> anyhow::Result<()> {
        for agent in mentions.iter().filter_map(|mention| mention.agent()) {
            self.workset_of(agent)?;
        }
        Ok(())
    }

    /// What `agent` receives after `after`, waiting up to [`INBOX_WAIT`]
    /// for some.
    async fn inbox(&self, agent: AgentId, after: ChatSeq) -> Vec<Message> {
        let mut feed = crate::store::feed(&self.db);
        let deadline = tokio::time::sleep(INBOX_WAIT);
        tokio::pin!(deadline);
        let mut handing = self.handing.subscribe();
        let handing = handing.wait_for(|handing| *handing);
        tokio::pin!(handing);
        loop {
            let messages = self.db.read().inbox(agent, after);
            if !messages.is_empty() {
                return messages;
            }
            loop {
                tokio::select! {
                    appended = feed.recv() => match appended {
                        Ok(crate::store::Appended::Mail(_))
                        | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return Vec::new(),
                    },
                    () = &mut deadline => return Vec::new(),
                    _ = &mut handing => return Vec::new(),
                }
            }
        }
    }

    /// Loads a persisted agent if it is not already running. The returned
    /// bool is true when this call started it.
    pub fn load(
        self: &Arc<Self>,
        agent_id: AgentId,
    ) -> futures::future::BoxFuture<'_, anyhow::Result<(AgentId, AgentClient, bool)>> {
        let pool = self.clone();
        Box::pin(async move {
            tokio::spawn(async move {
                let workset = pool.workset_of(agent_id)?;
                let slot = pool.execution_slot(workset).await;
                let admission = slot.admission.clone().read_owned().await;
                let lock = pool
                    .load_locks
                    .lock()
                    .await
                    .entry(agent_id)
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone();
                let loading = lock.lock_owned().await;
                anyhow::ensure!(
                    pool.db.read().agent(agent_id).is_some(),
                    "agent was deleted or does not exist"
                );
                let existing = pool.agents.lock().await.get(&agent_id).cloned();
                if let Some(agent) = existing {
                    if !agent.stopping() {
                        pool.touch(agent_id);
                        return Ok((agent_id, agent, false));
                    }
                    agent.shutdown().await;
                    pool.agents.lock().await.remove(&agent_id);
                }
                let agent = AgentClient::start(&pool, agent_id).await?;
                {
                    let mut agents = pool.agents.lock().await;
                    agents.insert(agent_id, agent.clone());
                    pool.touch(agent_id);
                    pool.tell_status(agent_id, &agent);
                }
                drop(loading);
                drop(admission);
                pool.trim(MAX_LOADED).await;
                Ok((agent_id, agent, true))
            })
            .await?
        })
    }
}

/// Where a workset's process and its agents work: an isolated workset's
/// `/src`, or a host workset's own directory.
pub fn workset_root(kind: &WorksetKind) -> camino::Utf8PathBuf {
    match kind {
        WorksetKind::Isolated => camino::Utf8PathBuf::from(rho_fs_view::MOUNT_ROOT),
        WorksetKind::Host { cwd } => cwd.clone(),
    }
}

/// The runtimes an agent can be given. Every account's models are listed;
/// a request on one the host cannot reach fails when it is made.
fn models() -> Vec<Runtime> {
    use crate::inference::config::{InferenceModel, InferenceProfile};
    let native = [
        InferenceModel::Sol,
        InferenceModel::Terra,
        InferenceModel::Astra,
        InferenceModel::Hydrogen,
    ]
    .map(|model| Runtime::Native {
        model,
        profile: InferenceProfile::default(),
        cache_key: crate::inference::PromptCacheKey::generate(),
    });
    let claude = [
        rho_claude::Model::Fable,
        rho_claude::Model::Opus,
        rho_claude::Model::Sonnet,
        rho_claude::Model::Haiku,
    ]
    .map(|model| Runtime::Claude {
        model,
        effort: rho_claude::Effort::High,
        session_id: uuid::Uuid::nil(),
    });
    native.into_iter().chain(claude).collect()
}

#[cfg(test)]
mod tests {
    use rho_agent_types::AgentIdDomain;

    use super::*;

    /// An agent's age counts from when it started what it does now: a
    /// status that changes something else keeps it; one that changes what
    /// it does starts it again. A retry streak counts from its first
    /// failure, across the attempts that show as working between.
    #[test]
    fn an_agent_is_doing_what_it_does_since_it_started() {
        let agent = AgentId::from_counter(1, &AgentIdDomain(0)).unwrap();
        let other = AgentId::from_counter(2, &AgentIdDomain(0)).unwrap();
        let working = RuntimeState {
            inference: InferenceState::Responding,
            ..RuntimeState::default()
        };
        let retrying = |at: u64| RuntimeState {
            inference: InferenceState::Retrying {
                at: UnixMs(at),
                error: "503".into(),
                since: UnixMs(5),
            },
            ..RuntimeState::default()
        };
        let failed = |since: Option<u64>| RuntimeState {
            inference: InferenceState::Failed {
                error: "400".into(),
                since: since.map(UnixMs),
            },
            ..RuntimeState::default()
        };
        let mut doing = HashMap::new();
        assert_eq!(stamp(&mut doing, agent, &working, UnixMs(1)), UnixMs(1));
        assert_eq!(
            stamp(&mut doing, agent, &working, UnixMs(3)),
            UnixMs(1),
            "still working"
        );
        assert_eq!(
            stamp(&mut doing, agent, &retrying(12), UnixMs(10)),
            UnixMs(5)
        );
        assert_eq!(
            stamp(&mut doing, agent, &working, UnixMs(12)),
            UnixMs(12),
            "an attempt is working, from when it began"
        );
        assert_eq!(
            stamp(&mut doing, agent, &retrying(30), UnixMs(20)),
            UnixMs(5),
            "the streak runs on from its first failure"
        );
        assert_eq!(stamp(&mut doing, other, &working, UnixMs(25)), UnixMs(25));
        assert_eq!(
            stamp(&mut doing, agent, &failed(None), UnixMs(40)),
            UnixMs(40),
            "a runtime that says no time failed when the host heard it"
        );
        assert_eq!(
            stamp(&mut doing, agent, &working, UnixMs(50)),
            UnixMs(50),
            "a failure ends; what follows starts anew"
        );
        assert_eq!(
            stamp(&mut doing, agent, &failed(Some(55)), UnixMs(60)),
            UnixMs(55),
            "it failed when its runtime says"
        );
        assert_eq!(
            stamp(&mut doing, agent, &failed(Some(70)), UnixMs(75)),
            UnixMs(70),
            "a retry failed again, though the host never heard it work"
        );
        assert_eq!(
            stamp(&mut doing, agent, &failed(Some(70)), UnixMs(90)),
            UnixMs(70),
            "the same failure, heard again"
        );
    }
}
