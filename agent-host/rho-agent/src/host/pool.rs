//! The agent host's worksets and the agents loaded in them.
//!
//! The pool owns one process per workset
//! ([ARCH-rho2](../../specs/ARCH-rho2.md)), the id → loaded-agent map, and the
//! answers to each workset's `_rho` calls. The store is the truth about
//! worksets, agents and mail; the pool holds only what runs.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Context as _;
use rho_agent_types::{Address, AgentId, ContentPart, Mail, MailId, UnixMs, WorksetKind};
use rho_db::RhoDb;
use rho_fs_view::{Layout, WorksetId, Worksets};
use tokio::sync::Mutex;

use super::AgentClient;
use crate::AgentStatus;
use crate::db::{AgentReadTxnExt as _, AgentWriteTxnExt as _};
use crate::inference::Accounts;
use crate::ipc::protocol::Control;
use crate::ipc::workset::{PyReply, PyRequest};
use crate::log::{AGENT_USAGE_BUCKET_MS, AgentUsageBucket};
use crate::record::{AgentSpec, Event, Record, Runtime};
use crate::store::{NewMail, StoreRead as _, StoreWrite as _};

/// How many agents stay loaded. Past this the least recently used one
/// that is settled and nobody is looking at is dropped; its log is the
/// whole of it, so nothing is lost.
pub const MAX_LOADED: usize = 100;
/// How long a `_rho.inbox` call waits for mail before it answers with none.
/// A handoff answers it at once instead.
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
    /// Per-id activation serialization; unrelated cold loads remain
    /// concurrent, while one persisted agent can never restore two loops.
    load_locks: Mutex<HashMap<AgentId, Arc<Mutex<()>>>>,
    usage: Mutex<HashMap<(AgentId, u64), AgentUsageBucket>>,
    /// The user's message the running `Eval` of a workset carries: the
    /// first agent its code creates gets it as mail.
    first_mail: std::sync::Mutex<HashMap<WorksetId, FirstMail>>,
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
    unsent: std::sync::Mutex<HashMap<AgentId, Arc<AgentStatus>>>,
    changed: tokio::sync::Notify,
}

impl Statuses {
    /// Every status not yet taken.
    pub fn take(&self) -> HashMap<AgentId, Arc<AgentStatus>> {
        std::mem::take(&mut *self.unsent.lock().expect("poison"))
    }

    /// Resolves once a status arrives after the last wait.
    pub async fn changed(&self) {
        self.changed.notified().await;
    }

    fn put(&self, agent_id: AgentId, status: Arc<AgentStatus>) {
        self.unsent.lock().expect("poison").insert(agent_id, status);
        self.changed.notify_one();
    }
}

/// What code the GUI ran printed, its traceback if it raised, and the
/// agent its message was mailed to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evaluated {
    pub output: String,
    pub error: Option<String>,
    pub agent: Option<AgentId>,
}

enum FirstMail {
    Waiting(Vec<ContentPart>),
    Sent(AgentId),
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
            agents: Mutex::new(HashMap::new()),
            recent: std::sync::Mutex::new(std::collections::VecDeque::new()),
            live_wants: std::sync::Mutex::new(HashMap::new()),
            live: std::sync::Mutex::new(HashSet::new()),
            statuses: std::sync::Mutex::new(Vec::new()),
            load_locks: Mutex::new(HashMap::new()),
            usage: Mutex::new(HashMap::new()),
            first_mail: std::sync::Mutex::default(),
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
        let kind = self
            .db
            .read()
            .workset(workset)
            .context("workset was deleted or does not exist")?
            .kind;
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
    /// mail just arrived for one of its agents.
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
        for agent_id in ids {
            if let Err(error) = self.load(agent_id).await {
                eprintln!("rho-agent: reloading {}: {error:#}", agent_id.encoded());
            }
        }
        Ok(replace)
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
    pub async fn set_live_wants(&self, connection: u64, wants: HashSet<AgentId>) {
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
    fn tell_status(&self, agent_id: AgentId, agent: &AgentClient) {
        self.status_changed(agent_id, agent.status_cell());
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
        &self,
        agent_id: AgentId,
        status: &tokio::sync::watch::Sender<AgentStatus>,
    ) {
        let mut watchers = self.statuses.lock().expect("poison");
        let shown = self.shown(agent_id, &status.borrow());
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
    fn shown(&self, agent_id: AgentId, status: &AgentStatus) -> Arc<AgentStatus> {
        Arc::new(if self.is_live(agent_id) {
            status.clone()
        } else {
            AgentStatus {
                runtime: status.runtime.clone(),
                response: None,
                draft: None,
                queued: status.queued,
            }
        })
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

    /// A new workset, its first row, and its process running `code`. With a
    /// `message`, the first agent the code creates gets it as mail from the
    /// user.
    pub async fn create_workset(
        self: &Arc<Self>,
        kind: WorksetKind,
        code: String,
        message: Option<Vec<ContentPart>>,
    ) -> anyhow::Result<(WorksetId, Evaluated)> {
        if let WorksetKind::Host { cwd } = &kind {
            anyhow::ensure!(
                cwd.is_absolute() && cwd.is_dir(),
                "a host workset needs an absolute directory: {cwd}"
            );
        }
        // Every workset has a directory: its state lives there, and an
        // isolated one's `/src`.
        let workset = self.worksets.create().await?.id();
        let mut write = self.db.write().await;
        write.create_workset(workset, UnixMs::now(), kind, code.clone());
        write.commit();
        let evaluated = self.run(workset, code, message).await?;
        Ok((workset, evaluated))
    }

    /// Run `code` in the workset namespace, logged first. With a `message`,
    /// the first agent the code creates gets it as mail from the user.
    pub async fn eval(
        self: &Arc<Self>,
        workset: WorksetId,
        code: String,
        message: Option<Vec<ContentPart>>,
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
        self.run(workset, code, message).await
    }

    async fn run(
        self: &Arc<Self>,
        workset: WorksetId,
        code: String,
        message: Option<Vec<ContentPart>>,
    ) -> anyhow::Result<Evaluated> {
        let slot = self.execution_slot(workset).await;
        let process = {
            let _admission = slot.admission.clone().read_owned().await;
            self.process(workset).await?
        };
        if let Some(message) = message {
            self.first_mail
                .lock()
                .expect("poison")
                .insert(workset, FirstMail::Waiting(message));
        }
        let result = process.eval(code).await;
        let first = self.first_mail.lock().expect("poison").remove(&workset);
        let (output, mut error) = result?;
        let agent = match first {
            Some(FirstMail::Sent(agent)) => Some(agent),
            Some(FirstMail::Waiting(_)) => {
                let note = "The code created no agent, so the message was not sent.";
                error = Some(match error {
                    Some(error) => format!("{error}\n{note}"),
                    None => note.to_owned(),
                });
                None
            }
            None => None,
        };
        Ok(Evaluated {
            output,
            error,
            agent,
        })
    }

    /// Mail from the user. A resend under the same `client_id` is the same
    /// mail. The agent's workset starts, so its policy can deliver it.
    pub async fn post_user_mail(
        self: &Arc<Self>,
        client_id: u64,
        to: AgentId,
        thread: Option<rho_agent_types::ThreadId>,
        content: Vec<ContentPart>,
    ) -> anyhow::Result<MailId> {
        let workset = self.workset_of(to)?;
        let id = self
            .post_mail(
                NewMail {
                    from: Address::User,
                    to: Address::Agent(to),
                    thread,
                    kind: None,
                    content,
                },
                Some(client_id),
            )
            .await?;
        self.wake_workset(workset);
        Ok(id)
    }

    async fn post_mail(&self, mail: NewMail, client_id: Option<u64>) -> anyhow::Result<MailId> {
        let mut write = self.db.write().await;
        let id = write.post_mail(UnixMs::now(), mail, client_id)?;
        write.commit();
        Ok(id)
    }

    /// Cut off an agent's turn, if it is loaded; one that is not has none.
    pub async fn cancel(&self, agent: AgentId) -> anyhow::Result<()> {
        if let Some(client) = self.get(agent).await {
            client.control(Control::Cancel).await?;
        }
        Ok(())
    }

    /// Stop each workset's process and erase it: its log, heads, usage,
    /// Claude sessions and directory. Mail stays. Every workset is checked
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
            write.delete_workset(workset);
            for &agent in &agents {
                write.delete_agent_usage(agent);
            }
            write.commit();
        }
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
                // The message the running `Eval` carries, if this is the
                // first agent it creates.
                let first = {
                    let mut first_mail = self.first_mail.lock().expect("poison");
                    match first_mail.remove(&workset) {
                        Some(FirstMail::Waiting(content)) => {
                            first_mail.insert(workset, FirstMail::Sent(agent));
                            Some(content)
                        }
                        other => {
                            if let Some(other) = other {
                                first_mail.insert(workset, other);
                            }
                            None
                        }
                    }
                };
                if let Some(content) = first {
                    write
                        .post_mail(
                            UnixMs::now(),
                            NewMail {
                                from: Address::User,
                                to: Address::Agent(agent),
                                thread: None,
                                kind: None,
                                content,
                            },
                            None,
                        )
                        .expect("a new thread names no mail");
                }
                write.commit();
                PyReply::Agent(agent)
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
                let current = self.own_agent(workset, agent)?;
                anyhow::ensure!(
                    std::mem::discriminant(&current.runtime)
                        == std::mem::discriminant(&spec.runtime),
                    "an agent keeps its runtime kind"
                );
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
                PyReply::Mail(
                    self.db
                        .read()
                        .agent(agent)
                        .context("agent was deleted")?
                        .delivered,
                )
            }
            PyRequest::SendMail {
                sender,
                to,
                content,
                kind,
                thread,
            } => {
                self.own_agent(workset, sender)?;
                let recipient = match to {
                    Address::User => None,
                    Address::Agent(to) => Some(self.workset_of(to)?),
                };
                let id = self
                    .post_mail(
                        NewMail {
                            from: Address::Agent(sender),
                            to,
                            thread,
                            kind,
                            content,
                        },
                        None,
                    )
                    .await?;
                if let Some(recipient) = recipient
                    && recipient != workset
                {
                    self.wake_workset(recipient);
                }
                PyReply::Mail(id)
            }
            PyRequest::Inbox { address, after } => PyReply::Mails(self.inbox(address, after).await),
            PyRequest::NewestMailBetween(a, b) => {
                PyReply::NewestMail(self.db.read().newest_between(a, b))
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
        })
    }

    /// Mail to `address` after `after`, waiting up to [`INBOX_WAIT`] for
    /// some.
    async fn inbox(&self, address: Address, after: MailId) -> Vec<Mail> {
        let mut feed = crate::store::feed(&self.db);
        let deadline = tokio::time::sleep(INBOX_WAIT);
        tokio::pin!(deadline);
        let mut handing = self.handing.subscribe();
        let handing = handing.wait_for(|handing| *handing);
        tokio::pin!(handing);
        loop {
            let mails = self.db.read().inbox(address, after);
            if !mails.is_empty() {
                return mails;
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
        InferenceModel::Gpt61Sol,
        InferenceModel::Gpt61Terra,
        InferenceModel::Gpt6Astra,
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
    ]
    .map(|model| Runtime::Claude {
        model,
        effort: rho_claude::Effort::High,
        session_id: uuid::Uuid::nil(),
    });
    native.into_iter().chain(claude).collect()
}
