//! Process-local pool of running agents.
//!
//! The pool owns the id → running-agent map and the worksets agents work
//! in. Higher layers (the agent host) own product policy around it: topics,
//! titles, land leases.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Context as _;
use camino::Utf8PathBuf;
use rho_agent_types::{AgentId, AgentRole, EngineerIntelligence, Place};
use rho_db::RhoDb;
use rho_fs_view::{Workset, Worksets};
use tokio::sync::{Mutex, broadcast};

use super::AgentClient;
use crate::AgentStatus;
use crate::db::{AgentProfileWriteTxnExt as _, AgentReadTxnExt as _, AgentWriteTxnExt as _};
use crate::inference::Accounts;
use crate::log::{
    AGENT_USAGE_BUCKET_MS, AgentOrigin, AgentRoleSessionProfile as _, AgentRuntime,
    AgentUsageBucket, SessionBinding,
};

/// Persisted placement and an optional one-shot checkout operation.
/// Creation commits the agent record before preparing its directory, then
/// starts the workset worker. Preparation never survives in the loaded agent.
pub struct StartPlace {
    pub place: Place,
    pub(crate) prepare:
        Option<std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>>>,
}

impl StartPlace {
    pub fn new(place: Place) -> Self {
        Self {
            place,
            prepare: None,
        }
    }

    pub fn pending(
        place: Place,
        prepare: impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
    ) -> Self {
        Self {
            place,
            prepare: Some(Box::pin(prepare)),
        }
    }
}

/// Runaway protection, not policy: children are user-visible agents.
const MAX_SPAWN_DEPTH: usize = 3;
const MAX_WORKING_CHILDREN: usize = 20;
/// How many agents stay loaded. Past this the least recently used one
/// that is settled and nobody is looking at is dropped; its log is the
/// whole of it, so nothing is lost.
pub const MAX_LOADED: usize = 100;
const ID_LABEL_HEADROOM: u64 = 200;

struct ResponseNotification {
    sender: AgentId,
    recipients: Vec<AgentId>,
    body: String,
}

#[derive(Default)]
struct ExecutionSlot {
    admission: Arc<tokio::sync::RwLock<()>>,
    process: Mutex<Option<Arc<crate::host::Process>>>,
}

pub struct AgentPool {
    processes: Mutex<HashMap<String, Arc<ExecutionSlot>>>,

    responses: tokio::sync::mpsc::Sender<ResponseNotification>,
    db: RhoDb,
    inference: Accounts,
    /// The worksets agents work in, named by the agent host rather than
    /// resolved here: a library does not reach for the user's state
    /// directory.
    worksets: Arc<Worksets>,
    /// The Claude configuration agents run against, named by the agent host for
    /// the same reason as `worksets`: a library that resolves `$HOME` puts
    /// every caller on the user's live `~/.claude`.
    claude: rho_claude::accounts::ClaudePaths,
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
    /// Fires for every agent created in this pool — including agents spawned
    /// by other agents — so every UI connection can pick them up.
    created: broadcast::Sender<AgentCreated>,
    usage: Mutex<HashMap<(AgentId, u64), AgentUsageBucket>>,
}

/// Broadcast when any agent is created in the pool. Carries no handle:
/// a handle sitting in the channel's buffer would keep an evicted loop
/// alive.
#[derive(Clone)]
pub struct AgentCreated {
    pub agent_id: AgentId,
    /// The agent that spawned this one, when another agent did.
    pub parent: Option<AgentId>,
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

/// An explicit message, mailed to whoever subscribed to this agent.
#[derive(Clone, Debug)]
pub struct AgentMessage {
    pub agent_id: AgentId,
    pub text: String,
}

impl AgentPool {
    /// Opens the pool over `db`, initializing the agent tables.
    pub async fn new(
        db: RhoDb,
        inference: Accounts,
        worksets: Arc<Worksets>,
        claude: rho_claude::accounts::ClaudePaths,
    ) -> Arc<Self> {
        crate::db::prepare(&db).await;
        // The account agents run on has to exist before the first spawn.
        let account = db.read().claude_account();
        if let Err(error) = claude.bootstrap(&account) {
            panic!("Claude account {account} could not be prepared: {error:#}");
        }
        let (responses, mut notifications) =
            tokio::sync::mpsc::channel::<ResponseNotification>(256);
        let pool = Arc::new(Self {
            processes: Mutex::new(HashMap::new()),
            responses,
            db,
            inference: inference.clone(),
            worksets,
            claude,
            agents: Mutex::new(HashMap::new()),
            recent: std::sync::Mutex::new(std::collections::VecDeque::new()),
            live_wants: std::sync::Mutex::new(HashMap::new()),
            live: std::sync::Mutex::new(HashSet::new()),
            statuses: std::sync::Mutex::new(Vec::new()),
            load_locks: Mutex::new(HashMap::new()),
            created: broadcast::channel(64).0,
            usage: Mutex::new(HashMap::new()),
        });
        let weak = Arc::downgrade(&pool);
        tokio::spawn(async move {
            // Completion publication must not await another serialized actor.
            // A single host-owned consumer preserves notification order, and
            // never participates in a worker generation's retirement barrier.
            while let Some(notification) = notifications.recv().await {
                let Some(pool) = weak.upgrade() else { break };
                for recipient in notification.recipients {
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(30),
                        pool.deliver_mail(
                            notification.sender,
                            recipient,
                            notification.body.clone(),
                        ),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        result => eprintln!(
                            "response notification {:?} -> {:?} was not confirmed: {result:?}",
                            notification.sender, recipient
                        ),
                    }
                }
            }
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

    /// The worksets agents work in.
    async fn execution_slot(&self, workset: &str) -> Arc<ExecutionSlot> {
        self.processes
            .lock()
            .await
            .entry(workset.to_owned())
            .or_default()
            .clone()
    }

    pub async fn execution(
        self: &Arc<Self>,
        agent: AgentId,
    ) -> anyhow::Result<Arc<crate::host::Process>> {
        let place = self.db.read().get_agent(agent).config.place;
        let slot = self.execution_slot(&place.workset).await;
        let _admission = slot.admission.clone().read_owned().await;
        let place = self.db.read().get_agent(agent).config.place;
        self.process(&place).await
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

    pub(crate) async fn process(
        self: &Arc<Self>,
        place: &Place,
    ) -> anyhow::Result<Arc<crate::host::Process>> {
        let slot = self.execution_slot(&place.workset).await;
        let mut process = slot.process.lock().await;
        if let Some(process) = process.as_ref().filter(|process| !*process.closed.borrow()) {
            return Ok(process.clone());
        }
        let workset = self.worksets.open_workset(&place.workset).await?;
        let started = crate::host::Process::start(
            self,
            &workset,
            self.claude.clone(),
            slot.admission.clone(),
        )
        .await?;
        *process = Some(started.clone());
        Ok(started)
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
            .map(|(workset, slot)| (workset.clone(), slot.clone()))
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
            idle().await;
            futures::future::try_join_all(
                processes.iter().map(|(_, process)| process.stop_reading()),
            )
            .await?;
            futures::future::try_join_all(
                processes
                    .iter()
                    .map(|(workset, process)| process.hand(workset.clone())),
            )
            .await
        })
        .await
        .context("workset processes did not settle")
        .flatten();
        if handed.is_err() {
            for (_, process) in &processes {
                process.resume();
            }
        }
        handed
    }

    /// Undoes a [`AgentPool::hand_over`] whose exec failed.
    pub async fn resume(&self) {
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
        workset: &str,
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
                    .map(|(workset, slot)| (workset.clone(), slot.clone()))
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
                    match pool.restart_workset(&workset, false).await {
                        Ok(true) => eprintln!("rho-agent: replaced stale workset {workset}"),
                        Ok(false) => {}
                        Err(error) => {
                            eprintln!("rho-agent: replacing stale workset {workset}: {error:#}")
                        }
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
                eprintln!("rho-agent: not taking over {}: {error:#}", handed.workset);
            }
        }
    }

    async fn adopt_process(self: &Arc<Self>, handed: &crate::host::Handed) -> anyhow::Result<()> {
        let slot = self.execution_slot(&handed.workset).await;
        let process = crate::host::Process::adopt(self, handed, slot.admission.clone()).await?;
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

    pub fn subscribe_created(&self) -> broadcast::Receiver<AgentCreated> {
        self.created.subscribe()
    }

    pub async fn publish_message(self: &Arc<Self>, completed: AgentMessage) {
        self.flush_agent_usage(Some(completed.agent_id)).await;
        self.deliver_response(completed.agent_id, completed.text);
    }

    pub async fn publish_failed_turn(self: &Arc<Self>, agent_id: AgentId, error: String) {
        self.deliver_response(agent_id, format!("Agent hit an error and stopped: {error}"));
    }

    fn deliver_response(&self, target: AgentId, body: String) {
        let recipients = self.db.read().agent_response_subscribers(target);
        if recipients.is_empty() {
            return;
        }
        // Waiting for capacity would recreate the actor dependency cycle when
        // the consumer is waiting for a recipient to accept an earlier message.
        if let Err(error) = self.responses.try_send(ResponseNotification {
            sender: target,
            recipients,
            body,
        }) {
            eprintln!("response notification from {target:?} was dropped: {error}");
        }
    }

    pub async fn set_response_subscription(
        &self,
        subscriber: AgentId,
        target: AgentId,
        subscribed: bool,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(subscriber != target, "an agent cannot subscribe to itself");
        anyhow::ensure!(
            self.agent_exists(subscriber),
            "subscriber agent does not exist"
        );
        anyhow::ensure!(self.agent_exists(target), "target agent does not exist");
        let mut write = self.db.write().await;
        write.set_agent_response_subscription(subscriber, target, subscribed);
        write.commit();
        Ok(())
    }

    /// Execution stopped: the usage it accrued lands now rather than at
    /// the next flush. The turn's edge itself is the log's (`Turn`).
    pub async fn settle_turn(&self, agent_id: AgentId) {
        self.flush_agent_usage(Some(agent_id)).await;
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
            write.add_agent_usage(*agent_id, usage);
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

    pub async fn create(
        self: &Arc<Self>,
        config: AgentRole,
        display_name: Option<String>,
        start: StartPlace,
    ) -> anyhow::Result<(AgentId, AgentClient)> {
        self.create_with_origin(config, display_name, start, AgentOrigin::User)
            .await
    }

    async fn create_with_origin(
        self: &Arc<Self>,
        config: AgentRole,
        display_name: Option<String>,
        start: StartPlace,
        origin: AgentOrigin,
    ) -> anyhow::Result<(AgentId, AgentClient)> {
        let pool = self.clone();
        // Admission and its per-ID ownership lock outlive cancellation of
        // the API caller. Never leave an unregistered worker still draining.
        tokio::spawn(async move {
            let slot = pool.execution_slot(&start.place.workset).await;
            let admission = slot.admission.clone().read_owned().await;
            let mode = config.session_profile();
            let runtime = match mode {
                SessionBinding::ClaudeFable { .. }
                | SessionBinding::ClaudeOpus { .. }
                | SessionBinding::ClaudeAdvisor { .. } => AgentRuntime::Claude {
                    session_id: uuid::Uuid::new_v4(),
                },
                _ => AgentRuntime::Rho {
                    prompt_cache_key: crate::inference::PromptCacheKey::generate(),
                },
            };
            let StartPlace { place, prepare } = start;
            let workset = pool.worksets.open_workset(&place.workset).await?;
            let host_cwd = workset.host_path(&place.cwd)?;
            if prepare.is_none() {
                anyhow::ensure!(
                    host_cwd.is_dir(),
                    "working directory does not exist: {}",
                    place.cwd
                );
            }
            let mut write = pool.db.write().await;
            let agent_id = write.alloc_agent_id();
            let lock = pool
                .load_locks
                .lock()
                .await
                .entry(agent_id)
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone();
            let loading = lock.lock_owned().await;
            write.create_agent(
                rho_agent_types::UnixMs::now(),
                agent_id,
                display_name,
                place,
                config,
                mode,
                runtime,
                origin,
            );
            write.commit();
            // Once its record commits, the workset belongs to that record,
            // even if the companion cannot start. Do not discard it on error.
            if let Some(prepare) = prepare {
                prepare.await?;
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
            let _ = pool.created.send(AgentCreated {
                agent_id,
                parent: origin.parent(),
            });
            Ok((agent_id, agent))
        })
        .await?
    }

    /// Create a child in the parent's workset and mail it its task. `workdir`
    /// selects an existing absolute directory; omission inherits the parent's
    /// directory. Returns once the child has accepted its task.
    pub async fn spawn_child(
        self: &Arc<Self>,
        parent: AgentId,
        task_name: String,
        prompt: String,
        config: AgentRole,
        workdir: Option<camino::Utf8PathBuf>,
    ) -> anyhow::Result<AgentId> {
        self.enforce_spawn_limits(parent).await?;
        self.spawn(
            AgentOrigin::Child { parent },
            task_name,
            prompt,
            config,
            workdir,
        )
        .await
    }

    /// Create an Engineer the user manages, briefed by `by` in its workset.
    /// It has no parent, so nothing it says is mailed to `by`, but it
    /// counts against `by`'s spawn limits like a child.
    pub async fn spawn_user_owned(
        self: &Arc<Self>,
        by: AgentId,
        task_name: String,
        prompt: String,
        workdir: Option<camino::Utf8PathBuf>,
    ) -> anyhow::Result<AgentId> {
        self.enforce_spawn_limits(by).await?;
        self.spawn(
            AgentOrigin::UserOwned { by },
            task_name,
            prompt,
            AgentRole::default(),
            workdir,
        )
        .await
    }

    /// Create an agent in its spawner's workset and mail it its task from
    /// the spawner. Returns once the agent has accepted its task.
    async fn spawn(
        self: &Arc<Self>,
        origin: AgentOrigin,
        task_name: String,
        prompt: String,
        config: AgentRole,
        workdir: Option<camino::Utf8PathBuf>,
    ) -> anyhow::Result<AgentId> {
        let spawner = match origin {
            AgentOrigin::Child { parent: spawner } | AgentOrigin::UserOwned { by: spawner } => {
                spawner
            }
            AgentOrigin::User => anyhow::bail!("only an agent can spawn an agent"),
        };
        let (spawner_place, spawner_role) = {
            let record = self.db.read().get_agent(spawner);
            (record.place().clone(), record.config.role)
        };
        let mut place = spawner_place;
        place.cwd = workdir.unwrap_or(place.cwd);
        anyhow::ensure!(place.cwd.is_absolute(), "workdir must be an absolute path");
        let start = StartPlace::new(place);
        let config = child_role(spawner_role, config);
        let (agent_id, agent) = self
            .create_with_origin(config, Some(task_name), start, origin)
            .await?;
        // Subscribe before the task goes out, or a quick first turn ends
        // with no one to tell.
        if let AgentOrigin::Child { parent } = origin {
            self.set_response_subscription(parent, agent_id, true)
                .await?;
        }
        let spawner_label = self.agent_handle(spawner);
        agent
            .send_agent_message_accepted(spawner, spawner_label, prompt)
            .await
            .with_context(|| {
                format!(
                    "created {} but it did not accept its initial task",
                    self.agent_handle(agent_id)
                )
            })?;
        Ok(agent_id)
    }

    async fn enforce_spawn_limits(&self, spawner: AgentId) -> anyhow::Result<()> {
        let read = self.db.read();
        let mut depth = 0;
        let mut cursor = Some(spawner);
        while let Some(id) = cursor {
            depth += 1;
            if depth > MAX_SPAWN_DEPTH {
                anyhow::bail!("spawn depth limit ({MAX_SPAWN_DEPTH}) reached");
            }
            cursor = read.agent_spawner(id);
        }
        drop(read);
        // Only loaded agents have a working runtime; settled agents can be
        // evicted. Identify loaded children without visiting every persisted
        // agent, then inspect their statuses under the pool lock.
        let loaded = self.agents.lock().await.keys().copied().collect::<Vec<_>>();
        let read = self.db.read();
        let children = loaded
            .into_iter()
            .filter(|id| read.agent_spawner(*id) == Some(spawner))
            .collect::<Vec<_>>();
        drop(read);
        let agents = self.agents.lock().await;
        let working_children = children
            .into_iter()
            .filter_map(|id| agents.get(&id))
            .filter(|agent| agent.status().runtime.is_working())
            .count();
        if working_children >= MAX_WORKING_CHILDREN {
            anyhow::bail!(
                "working sub-agent limit ({MAX_WORKING_CHILDREN}) reached; wait for an existing \
                 sub-agent to finish"
            );
        }
        Ok(())
    }

    /// Deliver inter-agent mail, loading the recipient internally and waiting
    /// until its loop accepts the input.
    pub async fn deliver_mail(
        self: &Arc<Self>,
        from: AgentId,
        to: AgentId,
        mut body: String,
    ) -> anyhow::Result<()> {
        let sender_role = {
            let read = self.db.read();
            anyhow::ensure!(read.agent_exists(from), "sender agent no longer exists");
            read.get_agent(from).config.role
        };
        let (_, agent, _) = self.load(to).await?;
        let sender_label = self.agent_handle(from);
        if matches!(sender_role, AgentRole::Advisor { .. }) {
            body.push_str(&format!(
                "\n\nAdvisor {sender_label} remains available. Use message_agent with this ID \
                 to continue."
            ));
        }
        agent
            .send_agent_message_accepted(from, sender_label, body)
            .await
    }

    /// Resolve an agent id string or prefix against all generated agent ids.
    pub fn resolve_agent_id(
        &self,
        text: &str,
    ) -> anyhow::Result<prefix_id::PrefixResolution<rho_agent_types::AgentIdDomain>> {
        let text = text.trim();
        let read = self.db.read();
        let domain = rho_agent_types::AgentIdDomain(read.machine_seed());
        Ok(AgentId::from_prefix(
            text,
            read.last_agent_counter() + 1,
            &domain,
        )?)
    }

    pub fn agent_exists(&self, agent_id: AgentId) -> bool {
        self.db.read().agent_exists(agent_id)
    }

    /// Short raw prefix for an agent id.
    pub fn agent_id_prefix(&self, agent_id: AgentId) -> String {
        let read = self.db.read();
        let prefix_len =
            prefix_id::uniform_prefix_len(read.last_agent_counter(), ID_LABEL_HEADROOM).max(4);
        agent_id.encoded()[..prefix_len].to_owned()
    }

    pub fn agent_handle(&self, agent_id: AgentId) -> String {
        // A loaded loop keeps its head; only a cold agent folds its log.
        let loaded = self
            .agents
            .try_lock()
            .ok()
            .and_then(|agents| agents.get(&agent_id).map(|agent| agent.head().config.role));
        let role = loaded.unwrap_or_else(|| self.db.read().get_agent(agent_id).config.role);
        format!(
            "{}-{}",
            role.handle_prefix(),
            self.agent_id_prefix(agent_id)
        )
    }

    /// The workset behind a persisted place and its working directory on the
    /// host.
    pub async fn open_workset(&self, place: &Place) -> anyhow::Result<(Workset, Utf8PathBuf)> {
        let workset = self.worksets.open_workset(&place.workset).await?;
        let host_cwd = workset.host_path(&place.cwd)?;
        Ok((workset, host_cwd))
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
                let workset = pool.db.read().get_agent(agent_id).place().workset.clone();
                let slot = pool.execution_slot(&workset).await;
                let admission = slot.admission.clone().read_owned().await;
                let lock = pool
                    .load_locks
                    .lock()
                    .await
                    .entry(agent_id)
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone();
                let loading = lock.lock_owned().await;
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

fn child_role(parent: AgentRole, child: AgentRole) -> AgentRole {
    match (parent, child) {
        (
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Mini,
            },
            AgentRole::Engineer { .. },
        ) => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Mini,
        },
        (_, AgentRole::Engineer { .. }) => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Medium,
        },
        (_, child) => child,
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::EngineerIntelligence;

    use super::*;

    #[test]
    fn mini_engineers_spawn_mini_engineers() {
        let mini = AgentRole::Engineer {
            intelligence: EngineerIntelligence::Mini,
        };
        let engineer = AgentRole::Engineer {
            intelligence: EngineerIntelligence::Medium,
        };

        assert_eq!(child_role(mini, engineer), mini);
        assert_eq!(
            child_role(
                mini,
                AgentRole::Advisor {
                    intelligence: rho_agent_types::AdvisorIntelligence::Medium,
                }
            ),
            AgentRole::Advisor {
                intelligence: rho_agent_types::AdvisorIntelligence::Medium,
            }
        );
    }

    #[test]
    fn every_non_mini_engineer_spawns_a_medium_engineer() {
        let medium = AgentRole::Engineer {
            intelligence: EngineerIntelligence::Medium,
        };
        for intelligence in [
            EngineerIntelligence::Medium,
            EngineerIntelligence::High,
            EngineerIntelligence::Medium1,
            EngineerIntelligence::High1,
        ] {
            assert_eq!(
                child_role(
                    AgentRole::Engineer { intelligence },
                    AgentRole::Engineer {
                        intelligence: EngineerIntelligence::High,
                    },
                ),
                medium
            );
        }
    }

    async fn test_pool(root: &std::path::Path) -> (Arc<AgentPool>, Place) {
        let worker = std::env::current_exe()
            .unwrap()
            .ancestors()
            .map(|path| path.join("rho-agent-worker"))
            .chain(std::iter::once(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/debug/rho-agent-worker"),
            ))
            .find(|path| path.is_file())
            .expect("build rho-agent-worker before the pool process test")
            .canonicalize()
            .unwrap();
        let bin = worker.parent().unwrap().to_owned();
        let mut environment = std::env::vars_os()
            .filter(|(key, _)| key != "PATH")
            .collect::<Vec<_>>();
        environment.push((
            "PATH".into(),
            std::env::join_paths(
                std::iter::once(bin)
                    .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
            )
            .unwrap(),
        ));
        let worksets = Worksets::open(
            root.join("state"),
            rho_fs_view::UserEnvironment::new(environment),
            Default::default(),
            rho_fs_view::StoreService::None,
        )
        .await
        .unwrap();
        let workset = worksets.create().await.unwrap();
        let place = Place {
            workset: workset.id().to_owned(),
            cwd: "/src".into(),
            origin: None,
        };
        let db = RhoDb::open(root.join("agents.redb"));
        let inference = crate::inference::testing::accounts();
        let pool = AgentPool::new(
            db,
            inference,
            worksets,
            rho_claude::accounts::ClaudePaths::at(
                camino::Utf8PathBuf::from_path_buf(root.join("claude")).unwrap(),
            ),
        )
        .await;
        (pool, place)
    }

    #[tokio::test]
    async fn a_connection_holds_each_agents_latest_status_with_bodies_only_while_live() {
        let directory = tempfile::tempdir().unwrap();
        let (pool, _) = test_pool(directory.path()).await;
        let (watched, other) = {
            let mut write = pool.db.write().await;
            let ids = (write.alloc_agent_id(), write.alloc_agent_id());
            write.commit();
            ids
        };
        let status = |queued| AgentStatus {
            response: Some(rho_agents_client::protocol::transcript::StreamingResponse {
                id: "response".into(),
                items: Vec::new(),
            }),
            draft: Some("draft".into()),
            queued,
            ..Default::default()
        };
        let tell = |agent_id, queued| {
            pool.status_changed(agent_id, &tokio::sync::watch::Sender::new(status(queued)))
        };
        let slow = pool.watch_statuses().await;
        assert!(slow.take().is_empty(), "no agent is loaded");

        // A connection that reads nothing meanwhile holds the latest status
        // of each agent, never a backlog.
        for queued in 0..1000 {
            tell(watched, queued);
        }
        tell(other, 7);
        tokio::time::timeout(std::time::Duration::from_secs(1), slow.changed())
            .await
            .expect("a waiting connection is woken");
        let taken = slow.take();
        assert_eq!(taken.len(), 2);
        assert_eq!(taken[&watched].queued, 999);
        assert_eq!(taken[&other].queued, 7);
        assert!(slow.take().is_empty());

        // Nobody is looking: the body stays home. Once one connection looks,
        // every connection gets it.
        assert_eq!(
            (&taken[&watched].response, &taken[&watched].draft),
            (&None, &None)
        );
        pool.set_live_wants(1, HashSet::from([watched])).await;
        tell(watched, 1);
        tell(other, 1);
        let taken = slow.take();
        assert_eq!(*taken[&watched], status(1));
        assert_eq!(taken[&other].response, None);

        // A connection that went away is forgotten.
        drop(slow);
        tell(other, 2);
        assert!(pool.statuses.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn pending_placement_commits_before_preparing_and_outlives_caller_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let (pool, mut place) = test_pool(directory.path()).await;
        let workset = pool.worksets.open_workset(&place.workset).await.unwrap();
        place.cwd = "/src/not-cloned-yet".into();
        let host_cwd = workset.host_path(&place.cwd).unwrap();
        let (entered, preparing) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let db = pool.db.clone();
        let create = tokio::spawn({
            let pool = pool.clone();
            async move {
                pool.create(
                    AgentRole::default(),
                    None,
                    StartPlace::pending(place, async move {
                        let ids = db.read().list_agent_ids();
                        assert_eq!(ids.len(), 1, "the record must commit before preparation");
                        entered.send(ids[0]).unwrap();
                        released.await.unwrap();
                        std::fs::create_dir(host_cwd)?;
                        Ok(())
                    }),
                )
                .await
            }
        });
        let id = tokio::time::timeout(std::time::Duration::from_secs(5), preparing)
            .await
            .unwrap()
            .unwrap();
        assert!(
            pool.executions().await.is_empty(),
            "no worker before placement"
        );
        create.abort();
        assert!(create.await.err().unwrap().is_cancelled());
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if pool.agents.lock().await.contains_key(&id) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let (_, agent, newly_loaded) = pool.load(id).await.unwrap();
        assert!(
            !newly_loaded,
            "cancelled caller must not leave an unregistered agent"
        );
        assert_eq!(agent.head().config.place.cwd, "/src/not-cloned-yet");
        agent.shutdown().await;
    }

    #[tokio::test]
    async fn failed_placement_keeps_the_record_without_starting_execution() {
        let directory = tempfile::tempdir().unwrap();
        let (pool, place) = test_pool(directory.path()).await;
        let result = pool
            .create(
                AgentRole::default(),
                None,
                StartPlace::pending(place, async { anyhow::bail!("checkout failed") }),
            )
            .await;
        assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("checkout failed")
        );
        assert_eq!(pool.db.read().list_agent_ids().len(), 1);
        assert!(pool.executions().await.is_empty());
    }

    #[tokio::test]
    async fn child_workdir_selects_existing_directory_before_creation() {
        let directory = tempfile::tempdir().unwrap();
        let (pool, place) = test_pool(directory.path()).await;
        let workset = pool.worksets.open_workset(&place.workset).await.unwrap();
        for name in ["parent", "checkout"] {
            let dir = workset.host_path(&place.cwd).unwrap().join(name);
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("AGENTS.md"), format!("Guidance for {name}.")).unwrap();
            let skill_dir = dir.join(".agents/skills/catalogue-fixture");
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(skill_dir.join("SKILL.md"), "---\nname: catalogue-fixture\ndescription: Directory-specific skill.\n---\nPrivate skill body.\n").unwrap();
        }
        let place = Place {
            cwd: "/src/parent".into(),
            ..place
        };
        let (parent_id, parent) = pool
            .create(
                AgentRole::default(),
                Some("parent".into()),
                StartPlace::new(place.clone()),
            )
            .await
            .unwrap();

        let tools =
            crate::multi_agent_tools::MultiAgentTools::new(Arc::downgrade(&pool), parent_id, None);
        let spawn = |workdir: Option<&str>| {
            crate::multi_agent_tools::AgentCall::SpawnEngineer(
                crate::multi_agent_tools::SpawnArgs {
                    task_name: "child".into(),
                    prompt: "No work is required.".into(),
                    workdir: workdir.map(str::to_owned),
                },
            )
        };
        for (workdir, expected) in [
            (Some("/src/checkout"), "/src/checkout"),
            (None, "/src/parent"),
        ] {
            let before = pool.db.read().list_agent_ids();
            let output = crate::multi_agent_tools::call_agent_tool(tools.clone(), spawn(workdir))
                .await
                .unwrap();
            assert!(output.contains(&format!("It works in {expected}.")));
            let read = pool.db.read();
            let child_id = read
                .list_agent_ids()
                .into_iter()
                .find(|id| !before.contains(id))
                .unwrap();
            let child = read.get_agent(child_id);
            assert_eq!(child.place().cwd.as_str(), expected);
            assert_eq!(
                child.place().workset,
                read.get_agent(parent_id).place().workset
            );

            // The selected directory supplies the child's guidance, immediately
            // after workspace context; identity belongs with
            // collaboration, not that guidance.
            let team = crate::multi_agent_tools::MultiAgentTools::new(
                Arc::downgrade(&pool),
                child_id,
                Some(parent_id),
            )
            .team()
            .unwrap();
            let rendered = crate::prompt::prompt(
                &crate::prompt::WorksetPrompt::for_host(&workset, child.place()),
                Some(&team),
                child.config.role,
            );
            let collaboration = rendered
                .split("## Working with other agents")
                .nth(1)
                .unwrap()
                .split("## Context and continuity")
                .next()
                .unwrap();
            assert!(collaboration.contains(&team.agent));
            assert!(collaboration.contains(team.parent.as_ref().unwrap()));
            let workspace = rendered.split("## Workspace Context").nth(1).unwrap();
            assert!(workspace.starts_with(&format!("\n\nWorking directory: {expected}\n")));
            assert!(workspace.contains(&format!(
                "Guidance for {}.",
                expected.rsplit('/').next().unwrap()
            )));
            assert!(
                !workspace
                    .split("## AGENTS.md instructions")
                    .next()
                    .unwrap()
                    .contains("## Skills")
            );
            assert!(!rendered.contains("## Team Context"));
            assert!(!rendered.lines().any(|line| line == "## Environment"));
            for role in [
                AgentRole::default(),
                AgentRole::Advisor {
                    intelligence: rho_agent_types::AdvisorIntelligence::Medium,
                },
            ] {
                let native = crate::prompt::prompt(
                    &crate::prompt::WorksetPrompt::for_host(&workset, child.place()),
                    Some(&team),
                    role,
                );
                let claude = crate::prompt::claude_prompt(
                    Some(&crate::prompt::WorksetPrompt::for_host(
                        &workset,
                        child.place(),
                    )),
                    Some(&team),
                    role,
                );
                let native_catalogue = native.split("## Skills\n").nth(1).unwrap();
                let claude_catalogue = claude.split("## Skills\n").nth(1).unwrap();
                assert_eq!(native_catalogue, claude_catalogue);
                assert!(
                    native_catalogue
                        .contains("- catalogue-fixture: Directory-specific skill. (file: r")
                );
                assert!(native_catalogue.contains(&format!("`{expected}/.agents/skills`")));
                assert!(!native_catalogue.contains("### How to use skills"));
                assert!(!native_catalogue.contains("Private skill body."));
            }
            drop(read);
            crate::multi_agent_tools::call_agent_tool(
                tools.clone(),
                crate::multi_agent_tools::AgentCall::Cancel(
                    crate::multi_agent_tools::InterruptArgs {
                        agent_id: team.agent.to_string(),
                    },
                ),
            )
            .await
            .unwrap();
        }

        let count = pool.db.read().list_agent_ids().len();
        for invalid in ["/tmp", "/src/missing", "checkout", "/src/../src/checkout"] {
            let result =
                crate::multi_agent_tools::call_agent_tool(tools.clone(), spawn(Some(invalid)))
                    .await;
            assert!(result.is_err(), "{invalid}");
            assert_eq!(pool.db.read().list_agent_ids().len(), count);
        }
        pool.execution(parent_id).await.unwrap().shutdown().await;
        drop(parent);
    }

    #[tokio::test]
    async fn user_owned_engineers_answer_to_the_user() {
        use crate::multi_agent_tools::{
            AgentCall, InterruptArgs, MultiAgentTools, SendArgs, SpawnArgs, call_agent_tool,
        };
        let directory = tempfile::tempdir().unwrap();
        let (pool, place) = test_pool(directory.path()).await;
        let (creator_id, creator) = pool
            .create(
                AgentRole::default(),
                Some("creator".into()),
                StartPlace::new(place),
            )
            .await
            .unwrap();
        let tools_of =
            |agent_id, parent| MultiAgentTools::new(Arc::downgrade(&pool), agent_id, parent);
        let spawn_args = |task_name: &str| SpawnArgs {
            task_name: task_name.into(),
            prompt: "No work is required.".into(),
            workdir: None,
        };
        let new_agent = |before: Vec<AgentId>| {
            pool.db
                .read()
                .list_agent_ids()
                .into_iter()
                .find(|id| !before.contains(id))
                .unwrap()
        };

        let before = pool.db.read().list_agent_ids();
        let output = call_agent_tool(
            tools_of(creator_id, None),
            AgentCall::SpawnUserOwnedEngineer(spawn_args("side")),
        )
        .await
        .unwrap();
        let owned = new_agent(before);
        let owned_handle = pool.agent_handle(owned);
        assert!(output.contains(&owned_handle), "{output}");
        {
            let read = pool.db.read();
            assert_eq!(read.agent_parent(owned), None);
            assert_eq!(
                read.get_agent(owned).config.spawned_by,
                crate::log::AgentSpawnedBy::UserOwned { by: creator_id }
            );
            assert!(read.agent_response_subscribers(owned).is_empty());
        }

        // The creator may still answer it, but no longer manages it.
        call_agent_tool(
            tools_of(creator_id, None),
            AgentCall::Message(SendArgs {
                agent_id: owned_handle.clone(),
                message: "more context".into(),
            }),
        )
        .await
        .unwrap();
        // The bare id behind the handle names the same agent; a prefix of
        // the wrong role does not.
        let bare = pool.agent_id_prefix(owned);
        let output = call_agent_tool(
            tools_of(creator_id, None),
            AgentCall::Message(SendArgs {
                agent_id: bare.clone(),
                message: "by bare id".into(),
            }),
        )
        .await
        .unwrap();
        assert!(output.contains(&owned_handle), "{output}");
        let error = call_agent_tool(
            tools_of(creator_id, None),
            AgentCall::Message(SendArgs {
                agent_id: format!("adv-{bare}"),
                message: "wrong role".into(),
            }),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("role prefix differs"), "{error}");
        let error = call_agent_tool(
            tools_of(creator_id, None),
            AgentCall::Cancel(InterruptArgs {
                agent_id: owned_handle.clone(),
            }),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("managed by the user"), "{error}");

        // Its own children still reach it, but one working for an agent
        // cannot open a thread for the user.
        let before = pool.db.read().list_agent_ids();
        call_agent_tool(
            tools_of(owned, None),
            AgentCall::SpawnEngineer(spawn_args("helper")),
        )
        .await
        .unwrap();
        let helper = new_agent(before);
        call_agent_tool(
            tools_of(helper, Some(owned)),
            AgentCall::Message(SendArgs {
                agent_id: owned_handle,
                message: "question".into(),
            }),
        )
        .await
        .unwrap();
        let count = pool.db.read().list_agent_ids().len();
        let error = call_agent_tool(
            tools_of(helper, Some(owned)),
            AgentCall::SpawnUserOwnedEngineer(spawn_args("nested")),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("only an agent the user manages"),
            "{error}"
        );
        assert_eq!(pool.db.read().list_agent_ids().len(), count);

        // Spawn limits follow the starter: a chain of user-owned Engineers
        // stops at the depth limit like a chain of children.
        let mut starter = owned;
        for _ in 2..=MAX_SPAWN_DEPTH {
            let before = pool.db.read().list_agent_ids();
            call_agent_tool(
                tools_of(starter, None),
                AgentCall::SpawnUserOwnedEngineer(spawn_args("deeper")),
            )
            .await
            .unwrap();
            starter = new_agent(before);
        }
        let error = call_agent_tool(
            tools_of(starter, None),
            AgentCall::SpawnUserOwnedEngineer(spawn_args("too-deep")),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("spawn depth limit"), "{error}");

        pool.execution(creator_id).await.unwrap().shutdown().await;
        drop(creator);
    }

    #[tokio::test]
    async fn external_handles_and_cancelled_eviction_cannot_create_overlapping_workers() {
        use std::time::Duration;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let (pool, place) = test_pool(root).await;
        let role = AgentRole::Engineer {
            intelligence: EngineerIntelligence::High,
        };
        let (first_id, first) = pool
            .create(role, Some("first".into()), StartPlace::new(place.clone()))
            .await
            .unwrap();
        let (second_id, second) = pool
            .create(
                AgentRole::default(),
                Some("second".into()),
                StartPlace::new(place),
            )
            .await
            .unwrap();
        // Neither completion publication nor queue saturation may wait for
        // actor acceptance. Hold both activation locks to stall recipients.
        pool.set_response_subscription(second_id, first_id, true)
            .await
            .unwrap();
        pool.set_response_subscription(first_id, second_id, true)
            .await
            .unwrap();
        let sender_lock = pool.load_locks.lock().await.get(&first_id).unwrap().clone();
        let receiver_lock = pool
            .load_locks
            .lock()
            .await
            .get(&second_id)
            .unwrap()
            .clone();
        let sender_guard = sender_lock.clone().lock_owned().await;
        let receiver_guard = receiver_lock.lock_owned().await;
        tokio::time::timeout(
            Duration::from_secs(3),
            pool.publish_message(AgentMessage {
                agent_id: first_id,
                text: "completed".into(),
            }),
        )
        .await
        .expect("completion waited for a recipient");

        // Reserve the remaining bounded slots without queuing hundreds of
        // irrelevant messages. The reverse subscription exercises full-queue
        // admission while its recipient is equally unable to accept mail.
        let reserved = pool
            .responses
            .try_reserve_many(pool.responses.capacity())
            .unwrap();
        tokio::time::timeout(
            Duration::from_secs(3),
            pool.publish_message(AgentMessage {
                agent_id: second_id,
                text: "saturated reverse notification".into(),
            }),
        )
        .await
        .expect("completion waited for notification capacity");
        drop(reserved);
        pool.publish_message(AgentMessage {
            agent_id: first_id,
            text: "second".into(),
        })
        .await;
        pool.set_response_subscription(first_id, second_id, false)
            .await
            .unwrap();
        drop(receiver_guard);
        drop(sender_guard);
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let messages = pool
                    .db
                    .read()
                    .agent_event_records(second_id)
                    .1
                    .into_iter()
                    .filter_map(|(_, event)| match event {
                        crate::AgentEvent::Entry(crate::entry::Entry::Received {
                            from: crate::entry::Party::Agent(_),
                            body,
                            ..
                        }) => Some(
                            body.iter()
                                .filter_map(|block| match block {
                                    crate::entry::Block::Text(text) => Some(text.as_str()),
                                    crate::entry::Block::Image(_) => None,
                                })
                                .collect::<String>(),
                        ),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if messages.len() >= 2 {
                    assert_eq!(&messages[..2], &["completed", "second"]);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("notifications did not preserve FIFO delivery");

        let process = pool.execution(first_id).await.unwrap();
        assert!(Arc::ptr_eq(
            &process,
            &pool.execution(second_id).await.unwrap()
        ));
        pool.trim(1).await;
        assert!(pool.agents.lock().await.contains_key(&first_id));
        assert!(pool.agents.lock().await.contains_key(&second_id));
        drop(first);

        // Hold the real workset process, not a fake runtime, during retirement.
        let pid = rustix::process::Pid::from_raw(process.pid as i32).unwrap();
        rustix::process::kill_process(pid, rustix::process::Signal::STOP).unwrap();
        let trim = tokio::spawn({
            let pool = pool.clone();
            async move {
                pool.trim(1).await;
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while sender_lock.try_lock().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        trim.abort();
        let load = || {
            let pool = pool.clone();
            tokio::spawn(async move { pool.load(first_id).await.unwrap() })
        };
        let one = load();
        let two = load();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !one.is_finished() && !two.is_finished(),
            "cancelled eviction released ownership"
        );
        rustix::process::kill_process(pid, rustix::process::Signal::CONT).unwrap();
        let ((_, one, loaded_one), (_, two, loaded_two)) =
            tokio::time::timeout(Duration::from_secs(10), async {
                (one.await.unwrap(), two.await.unwrap())
            })
            .await
            .unwrap();
        assert_ne!(loaded_one, loaded_two);
        assert!(Arc::ptr_eq(
            &process,
            &pool.execution(first_id).await.unwrap()
        ));

        // A local service failure must not leave its runtime alive after
        // reload.
        process.fail_agent_service(first_id);
        tokio::time::timeout(Duration::from_secs(10), async {
            let _ = process.closed.clone().wait_for(|closed| *closed).await;
        })
        .await
        .unwrap();
        let (_, reloaded, _) = pool.load(first_id).await.unwrap();
        let replacement = pool.execution(first_id).await.unwrap();
        assert!(!Arc::ptr_eq(&process, &replacement));
        assert!(!std::path::Path::new(&format!("/proc/{}", process.pid)).exists());

        // A failed Stop enqueue must take the termination-and-drain fallback.
        replacement.fail_stop_send();
        tokio::time::timeout(Duration::from_secs(10), reloaded.shutdown())
            .await
            .unwrap();
        assert!(!std::path::Path::new(&format!("/proc/{}", replacement.pid)).exists());
        let (_, active, _) = pool.load(first_id).await.unwrap();
        let replacement = pool.execution(first_id).await.unwrap();

        // Buffer agent traffic briefly; workset control still completes, then
        // restore the route without dropping its queued messages.
        let (old_route, mut blocked) = replacement.pause_agent_route(first_id);
        for _ in 0..4 {
            active.tell_tail();
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while blocked.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(std::path::Path::new(&format!("/proc/{}", replacement.pid)).exists());
        tokio::time::timeout(
            Duration::from_secs(2),
            replacement.action(crate::WorksetAction::TerminalList),
        )
        .await
        .unwrap()
        .unwrap();
        replacement.restore_agent_route(first_id, old_route.clone(), &mut blocked);
        assert!(blocked.is_empty(), "buffered messages were not delivered");
        drop(old_route);
        let final_agent = active.clone();
        // A failing shutdown reply consumes the join result exactly once.
        let process = pool.execution(first_id).await.unwrap();
        let pid = rustix::process::Pid::from_raw(process.pid as i32).unwrap();
        rustix::process::kill_process(pid, rustix::process::Signal::STOP).unwrap();
        let shutdown = tokio::spawn({
            let agent = final_agent.clone();
            async move { agent.shutdown().await }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        process.fail_shutdown_reply(first_id);
        // Resumed only once the failing reply is queued, so the worker exits
        // on its own rather than after the kill grace.
        rustix::process::kill_process(pid, rustix::process::Signal::CONT).unwrap();
        tokio::time::timeout(Duration::from_secs(10), shutdown)
            .await
            .unwrap()
            .unwrap();
        assert!(!std::path::Path::new(&format!("/proc/{}", process.pid)).exists());

        // Caller cancellation cannot release admission for an enqueued create.
        let process = pool.execution(first_id).await.unwrap();
        let slot = pool
            .execution_slot(&pool.db().read().get_agent(first_id).config.place.workset)
            .await;
        let pid = rustix::process::Pid::from_raw(process.pid as i32).unwrap();
        rustix::process::kill_process(pid, rustix::process::Signal::STOP).unwrap();
        let attach = tokio::spawn({
            let process = process.clone();
            async move {
                process
                    .attach(crate::WorksetAttach::Terminal {
                        agent: first_id,
                        terminal: 99,
                        create: true,
                        cols: 80,
                        rows: 24,
                        cwd: "/src".into(),
                        shell: "bash".into(),
                    })
                    .await
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while slot.admission.try_write().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        attach.abort();
        let _ = attach.await;
        assert!(
            slot.admission.try_write().is_err(),
            "cancelled create released admission"
        );
        rustix::process::kill_process(pid, rustix::process::Signal::CONT).unwrap();
        // The in-flight attach must finish before exclusive admission succeeds.
        let _admission = tokio::time::timeout(Duration::from_secs(10), slot.admission.write())
            .await
            .unwrap();
        process.shutdown().await;
        drop((one, two, second, active));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn only_a_stale_quiet_workset_is_replaced_unless_forced() {
        let directory = tempfile::tempdir().unwrap();
        let (pool, place) = test_pool(directory.path()).await;
        let (agent_id, _) = pool
            .create(AgentRole::default(), None, StartPlace::new(place.clone()))
            .await
            .unwrap();
        let first = pool.execution(agent_id).await.unwrap();
        assert!(
            !pool.restart_workset(&place.workset, false).await.unwrap(),
            "current build"
        );
        assert!(!*first.closed.borrow());

        first.make_stale();
        assert!(pool.restart_workset(&place.workset, false).await.unwrap());
        assert!(*first.closed.borrow());
        let second = pool.execution(agent_id).await.unwrap();
        assert_ne!(second.pid, first.pid);
        assert!(!second.stale());
        assert!(
            pool.agents.lock().await.contains_key(&agent_id),
            "loaded again"
        );

        let _terminal = second
            .attach(crate::WorksetAttach::Terminal {
                agent: agent_id,
                terminal: 1,
                create: true,
                cols: 80,
                rows: 24,
                cwd: "/src".into(),
                shell: "bash".into(),
            })
            .await
            .unwrap();
        second.make_stale();
        assert!(
            !pool.restart_workset(&place.workset, false).await.unwrap(),
            "a terminal is open"
        );
        assert!(!*second.closed.borrow());

        assert!(pool.restart_workset(&place.workset, true).await.unwrap());
        assert!(*second.closed.borrow());
        let third = pool.execution(agent_id).await.unwrap();
        assert_ne!(third.pid, second.pid);
        third.shutdown().await;
    }
}
