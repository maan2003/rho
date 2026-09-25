//! Process-local pool of running agents.
//!
//! The pool owns the id → running-agent map and the worksets agents work
//! in. Higher layers (the agent host) own product policy around it: topics,
//! titles, land leases.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Context as _;
use camino::Utf8PathBuf;
use rho_agent_types::{
    AgentId, AgentRole, EngineerIntelligence, MessageDelivery, Place, WorksetMode,
};
use rho_db::RhoDb;
use rho_fs_view::{Mode, Workset, Worksets};
use rho_inference::Inference;
use tokio::sync::{Mutex, broadcast};

use crate::db::{
    AGENT_USAGE_BUCKET_MS, AgentOrigin, AgentProfileWriteTxnExt as _, AgentReadTxnExt as _,
    AgentRoleSessionProfile as _, AgentRuntime, AgentUsageBucket, AgentWriteTxnExt as _,
    SessionBinding,
};
use crate::lazy::Lazy;
use crate::{StartPlace, View};

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
    process: Mutex<Option<Arc<crate::worker::Process>>>,
}

pub struct AgentPool {
    processes: Mutex<HashMap<String, Arc<ExecutionSlot>>>,

    responses: tokio::sync::mpsc::Sender<ResponseNotification>,
    db: RhoDb,
    inference: Inference,
    /// The worksets agents work in, named by the agent host rather than
    /// resolved here: a library does not reach for the user's state
    /// directory.
    worksets: Arc<Worksets>,
    /// The Claude configuration agents run against, named by the agent host for
    /// the same reason as `worksets`: a library that resolves `$HOME` puts
    /// every caller on the user's live `~/.claude`.
    claude: rho_claude::accounts::ClaudePaths,
    agents: Mutex<HashMap<AgentId, RunningAgent>>,
    /// Loaded agents, least recently used first. Touched by every load.
    recent: std::sync::Mutex<std::collections::VecDeque<AgentId>>,
    /// Which agents each connection is looking at; the union is the live
    /// set. A connection that goes away takes its wants with it.
    live_wants: std::sync::Mutex<HashMap<u64, HashSet<AgentId>>>,
    /// The live set: agents whose loops tell the tail as it changes. A
    /// loaded live agent is also told it is watched, which is what lets
    /// its title and activity refresh.
    live: std::sync::Mutex<HashSet<AgentId>>,
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

/// An agent completed a turn: its final answer is mailed to whoever
/// subscribed to its responses.
#[derive(Clone, Debug)]
pub struct AgentTurnCompleted {
    pub agent_id: AgentId,
    pub final_answer: String,
}

impl AgentPool {
    /// Opens the pool over `db`, initializing the agent tables.
    pub async fn new(
        db: RhoDb,
        inference: Inference,
        worksets: Arc<Worksets>,
        claude: rho_claude::accounts::ClaudePaths,
    ) -> Arc<Self> {
        crate::db::prepare(&db).await;
        // The account agents run on has to exist before the first spawn.
        let account = db.read().claude_account();
        if let Err(error) = claude.bootstrap(&account) {
            panic!("Claude account {account} could not be prepared: {error:#}");
        }
        // Cached flake dev shells, shared by every workset process and
        // every `nix develop` in a view, over the shared cache directory.
        let devshell = Arc::new(
            rho_devshell_daemon::Store::open(
                db.clone(),
                worksets.devshell_cache_dir().into_std_path_buf(),
            )
            .await,
        );
        match devshell.serve() {
            Ok(serve) => {
                tokio::spawn(serve);
            }
            Err(error) => eprintln!("dev shell cache unavailable: {error:#}"),
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
                            MessageDelivery::NextRequest,
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
    ) -> anyhow::Result<Arc<crate::worker::Process>> {
        let place = self.db.read().get_agent(agent).config.place;
        let slot = self.execution_slot(&place.workset).await;
        let _admission = slot.admission.clone().read_owned().await;
        let place = self.db.read().get_agent(agent).config.place;
        let view = self.materialize_view(&place).await?;
        self.process(&view).await
    }

    pub async fn executions(&self) -> Vec<Arc<crate::worker::Process>> {
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
        view: &crate::View,
    ) -> anyhow::Result<Arc<crate::worker::Process>> {
        anyhow::ensure!(
            self.db
                .read()
                .list_agents()
                .iter()
                .filter(|(_, head)| head.place().workset == view.workset_id())
                .all(|(_, head)| head.place().mode == view.workset_mode()),
            "workset contains mixed filesystem modes; explicitly select a workset mode first"
        );
        let slot = self.execution_slot(view.workset_id()).await;
        let mut process = slot.process.lock().await;
        if let Some(process) = process.as_ref().filter(|process| !*process.closed.borrow()) {
            anyhow::ensure!(
                process.mode == view.workset_mode(),
                "workset mode must be changed before loading this agent"
            );
            return Ok(process.clone());
        }
        let started =
            crate::worker::Process::start(self, view, self.claude.clone(), slot.admission.clone())
                .await?;
        *process = Some(started.clone());
        Ok(started)
    }

    pub fn worksets(&self) -> &Arc<Worksets> {
        &self.worksets
    }

    pub fn subscribe_created(&self) -> broadcast::Receiver<AgentCreated> {
        self.created.subscribe()
    }

    pub async fn publish_completed_turn(self: &Arc<Self>, completed: AgentTurnCompleted) {
        self.flush_agent_usage(Some(completed.agent_id)).await;
        self.deliver_response(
            completed.agent_id,
            if completed.final_answer.is_empty() {
                "(turn finished with no text response)".to_owned()
            } else {
                completed.final_answer.clone()
            },
        );
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

    pub fn is_response_subscribed(&self, subscriber: AgentId, target: AgentId) -> bool {
        self.db
            .read()
            .is_agent_response_subscribed(subscriber, target)
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

    pub fn inference(&self) -> &Inference {
        &self.inference
    }

    pub async fn get(&self, agent_id: AgentId) -> Option<RunningAgent> {
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
    /// The live set is the union over connections; an agent entering it
    /// starts telling its tail (whole, once) and an agent leaving it stops.
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
                self.attach_live(agent_id, agent);
            }
        }
    }

    /// A live agent is loaded: it is watched (titles and activity get
    /// made) and tells its tail whole. Leaving the live set unwatches it.
    fn attach_live(&self, agent_id: AgentId, agent: &RunningAgent) {
        if !self.live.lock().expect("poison").contains(&agent_id) {
            return;
        }
        agent.tell_tail();
    }

    /// Every loaded live agent tells its tail whole again: a connection
    /// just caught up from the journal and holds nothing of the tails.
    pub async fn tell_tails(&self) {
        let live = self
            .live
            .lock()
            .expect("poison")
            .iter()
            .copied()
            .collect::<Vec<_>>();
        let agents = self.agents.lock().await;
        for agent_id in live {
            if let Some(agent) = agents.get(&agent_id) {
                agent.tell_tail();
            }
        }
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
    ) -> anyhow::Result<(AgentId, RunningAgent)> {
        self.create_with_origin(config, display_name, start, AgentOrigin::User)
            .await
    }

    async fn create_with_origin(
        self: &Arc<Self>,
        config: AgentRole,
        display_name: Option<String>,
        start: StartPlace,
        origin: AgentOrigin,
    ) -> anyhow::Result<(AgentId, RunningAgent)> {
        let pool = self.clone();
        // Admission and its per-ID ownership lock outlive cancellation of
        // the API caller. Never leave an unregistered worker still draining.
        tokio::spawn(async move {
            let slot = pool.execution_slot(&start.place.workset).await;
            let admission = slot.admission.clone().read_owned().await;
            anyhow::ensure!(
                pool.db
                    .read()
                    .list_agents()
                    .iter()
                    .filter(|(_, head)| head.place().workset == start.place.workset)
                    .all(|(_, head)| head.place().mode == start.place.mode),
                "new agents must use the workset's filesystem mode"
            );
            let mode = config.session_profile();
            let runtime = match mode {
                SessionBinding::ClaudeFable { .. }
                | SessionBinding::ClaudeOpus { .. }
                | SessionBinding::ClaudeAdvisor { .. } => AgentRuntime::Claude {
                    session_id: uuid::Uuid::new_v4(),
                },
                _ => AgentRuntime::Rho {
                    prompt_cache_key: rho_inference::PromptCacheKey::generate(),
                },
            };
            let StartPlace { view, place, .. } = start;
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
            let agent = RunningAgent::start(&pool, pool.claude.clone(), agent_id, view).await?;
            {
                let mut agents = pool.agents.lock().await;
                agents.insert(agent_id, agent.clone());
                pool.touch(agent_id);
                pool.attach_live(agent_id, &agent);
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
    /// It has no parent: nothing it says is mailed to `by`, and it counts
    /// against none of `by`'s limits.
    pub async fn spawn_user_owned(
        self: &Arc<Self>,
        by: AgentId,
        task_name: String,
        prompt: String,
        workdir: Option<camino::Utf8PathBuf>,
    ) -> anyhow::Result<AgentId> {
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
        let Place {
            workset,
            cwd,
            mode,
            origin: place_origin,
        } = spawner_place;
        let cwd = workdir.unwrap_or(cwd);
        anyhow::ensure!(cwd.is_absolute(), "workdir must be an absolute path");
        let workset = self.worksets.open_workset(&workset).await?;
        let mode = Mode::from_workset_mode(mode);
        let view = workset.enter(mode, &cwd)?;
        let start = StartPlace::new(view, place_origin);
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
            .send_agent_message_accepted(
                spawner,
                spawner_label,
                prompt,
                MessageDelivery::NextRequest,
            )
            .await
            .with_context(|| {
                format!(
                    "created {} but it did not accept its initial task",
                    self.agent_handle(agent_id)
                )
            })?;
        Ok(agent_id)
    }

    async fn enforce_spawn_limits(&self, parent: AgentId) -> anyhow::Result<()> {
        let child_ids = {
            let read = self.db.read();
            let mut depth = 0;
            let mut cursor = Some(parent);
            while let Some(id) = cursor {
                depth += 1;
                if depth > MAX_SPAWN_DEPTH {
                    anyhow::bail!("spawn depth limit ({MAX_SPAWN_DEPTH}) reached");
                }
                cursor = read.agent_parent(id);
            }
            read.list_agent_ids()
                .into_iter()
                .filter(|id| read.agent_parent(*id) == Some(parent))
                .collect::<Vec<_>>()
        };
        let agents = self.agents.lock().await;
        let working_children = child_ids
            .into_iter()
            .filter_map(|id| agents.get(&id))
            .filter(|agent| agent.status().kind.is_working())
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
        delivery: MessageDelivery,
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
            .send_agent_message_accepted(from, sender_label, body, delivery)
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

    /// The workset behind a persisted place, its mode, and the place's
    /// working directory on the host.
    pub async fn open_workset(
        &self,
        place: &Place,
    ) -> anyhow::Result<(Workset, Mode, Utf8PathBuf)> {
        let workset = self.worksets.open_workset(&place.workset).await?;
        let mode = Mode::from_workset_mode(place.mode);
        let host_cwd = workset.host_path(&place.cwd)?;
        Ok((workset, mode, host_cwd))
    }

    /// Materializes an agent's persisted place into a live view.
    pub async fn materialize_view(&self, place: &Place) -> anyhow::Result<Arc<View>> {
        let (workset, mode, _) = self.open_workset(place).await?;
        workset.enter(mode, &place.cwd)
    }

    fn lazy_view(self: &Arc<Self>, _agent_id: AgentId, place: Place) -> Arc<Lazy<Arc<View>>> {
        let pool = Arc::downgrade(self);
        Arc::new(Lazy::new(move || {
            let pool = pool.clone();
            let place = place.clone();
            async move {
                let pool = pool.upgrade().context("agent pool dropped")?;
                pool.materialize_view(&place).await
            }
        }))
    }

    /// Mode belongs to the workset. Existing execution must be idle and
    /// retained terminals/shells closed before its base namespace can change.
    pub async fn change_mode(
        self: &Arc<Self>,
        agent_id: AgentId,
        mode: WorksetMode,
    ) -> anyhow::Result<Vec<AgentId>> {
        let pool = self.clone();
        tokio::spawn(async move {
            let workset = pool.db.read().get_agent(agent_id).place().workset.clone();
            let slot = pool.execution_slot(&workset).await;
            let _admission = slot.admission.clone().write_owned().await;
            let records = pool
                .db
                .read()
                .list_agents()
                .into_iter()
                .filter(|(_, head)| head.place().workset == workset)
                .collect::<Vec<_>>();
            if records.iter().all(|(_, head)| head.place().mode == mode) {
                return Ok(Vec::new());
            }
            let mut process = slot.process.lock().await;
            if let Some(process) = process.as_ref().filter(|process| !*process.closed.borrow()) {
                anyhow::ensure!(
                    process.no_sessions().await?,
                    "close every terminal and shell in the workset before changing its mode"
                );
            }
            let ids = records.iter().map(|(id, _)| *id).collect::<Vec<_>>();
            let locks = {
                let mut locks = pool.load_locks.lock().await;
                ids.iter()
                    .map(|id| locks.entry(*id).or_default().clone())
                    .collect::<Vec<_>>()
            };
            let mut guards = Vec::new();
            for lock in locks {
                guards.push(lock.lock_owned().await);
            }
            let candidates = {
                let agents = pool.agents.lock().await;
                ids.iter()
                    .filter_map(|id| agents.get(id).map(|agent| (*id, agent.clone())))
                    .collect::<Vec<_>>()
            };
            anyhow::ensure!(
                candidates.iter().all(|(_, agent)| agent.settled()),
                "cancel active work in this workset before changing its mode"
            );
            for (id, agent) in candidates {
                agent
                    .retire()
                    .await
                    .context("workset became active while changing its mode")?;
                pool.agents.lock().await.remove(&id);
                pool.recent
                    .lock()
                    .expect("poison")
                    .retain(|candidate| *candidate != id);
                agent.shutdown().await;
            }
            if let Some(process) = process.take() {
                process.shutdown().await;
            }
            let mut write = pool.db.write().await;
            for id in &ids {
                write.set_agent_mode(*id, mode);
            }
            write.commit();
            Ok(ids)
        })
        .await?
    }

    /// Loads a persisted agent if it is not already running. The returned
    /// bool is true when this call started it.
    pub fn load(
        self: &Arc<Self>,
        agent_id: AgentId,
    ) -> futures::future::BoxFuture<'_, anyhow::Result<(AgentId, RunningAgent, bool)>> {
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
                let record = pool.db.read().get_agent(agent_id);
                let view = pool.lazy_view(agent_id, record.place().clone());
                let agent = RunningAgent::start(&pool, pool.claude.clone(), agent_id, view).await?;
                {
                    let mut agents = pool.agents.lock().await;
                    agents.insert(agent_id, agent.clone());
                    pool.touch(agent_id);
                    pool.attach_live(agent_id, &agent);
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

pub use crate::worker::Remote as RunningAgent;

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

    async fn test_pool(root: &std::path::Path) -> (Arc<AgentPool>, Arc<View>) {
        let worker = std::env::current_exe()
            .unwrap()
            .ancestors()
            .map(|path| path.join("rho-agent-worker"))
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
        let view = worksets
            .create()
            .await
            .unwrap()
            .enter(
                Mode::View {
                    home_skeleton: None,
                },
                camino::Utf8Path::new("/src"),
            )
            .unwrap();
        let db = RhoDb::open(root.join("agents.redb"));
        let inference = Inference::new_with_config(
            db.clone(),
            rho_inference::InferenceConfig::with_responses_base_url("http://127.0.0.1:1").unwrap(),
        )
        .await
        .unwrap();
        let pool = AgentPool::new(
            db,
            inference,
            worksets,
            rho_claude::accounts::ClaudePaths::at(
                camino::Utf8PathBuf::from_path_buf(root.join("claude")).unwrap(),
            ),
        )
        .await;
        (pool, view)
    }

    #[tokio::test]
    async fn child_workdir_selects_existing_directory_before_creation() {
        let directory = tempfile::tempdir().unwrap();
        let (pool, view) = test_pool(directory.path()).await;
        for name in ["parent", "checkout"] {
            let dir = view.host_cwd().join(name);
            std::fs::create_dir(&dir).unwrap();
            std::fs::write(dir.join("AGENTS.md"), format!("Guidance for {name}.")).unwrap();
            let skill_dir = dir.join(".agents/skills/catalogue-fixture");
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(skill_dir.join("SKILL.md"), "---\nname: catalogue-fixture\ndescription: Directory-specific skill.\n---\nPrivate skill body.\n").unwrap();
        }
        let view = view.for_cwd(camino::Utf8Path::new("/src/parent")).unwrap();
        let (parent_id, parent) = pool
            .create(
                AgentRole::default(),
                Some("parent".into()),
                StartPlace::new(view.clone(), None),
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

            // The selected directory supplies the child's guidance, immediately after
            // workspace context; identity belongs with collaboration, not that guidance.
            let team = crate::multi_agent_tools::MultiAgentTools::new(
                Arc::downgrade(&pool),
                child_id,
                Some(parent_id),
            )
            .team()
            .unwrap();
            let child_view = view.for_cwd(camino::Utf8Path::new(expected)).unwrap();
            let rendered = crate::prompt::prompt(&child_view, Some(&team), child.config.role);
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
                let native = crate::prompt::prompt(&child_view, Some(&team), role);
                let claude = crate::prompt::claude_prompt(Some(&child_view), Some(&team), role);
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
        let (pool, view) = test_pool(directory.path()).await;
        let (creator_id, creator) = pool
            .create(
                AgentRole::default(),
                Some("creator".into()),
                StartPlace::new(view, None),
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
                crate::db::AgentSpawnedBy::UserOwned { by: creator_id }
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

        pool.execution(creator_id).await.unwrap().shutdown().await;
        drop(creator);
    }

    #[tokio::test]
    async fn external_handles_and_cancelled_eviction_cannot_create_overlapping_workers() {
        use std::time::Duration;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let (pool, view) = test_pool(root).await;
        let role = AgentRole::Engineer {
            intelligence: EngineerIntelligence::High,
        };
        let (first_id, first) = pool
            .create(
                role,
                Some("first".into()),
                StartPlace::new(view.clone(), None),
            )
            .await
            .unwrap();
        let (second_id, second) = pool
            .create(
                AgentRole::default(),
                Some("second".into()),
                StartPlace::new(view, None),
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
            pool.publish_completed_turn(AgentTurnCompleted {
                agent_id: first_id,
                final_answer: "completed".into(),
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
            pool.publish_completed_turn(AgentTurnCompleted {
                agent_id: second_id,
                final_answer: "saturated reverse notification".into(),
            }),
        )
        .await
        .expect("completion waited for notification capacity");
        drop(reserved);
        pool.publish_completed_turn(AgentTurnCompleted {
            agent_id: first_id,
            final_answer: "second".into(),
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
                        crate::AgentEvent::Accepted(crate::QueuedInput {
                            kind: crate::InputKind::Message { content },
                            ..
                        }) => Some(rho_inference::types::text_content(&content)),
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
        tokio::time::timeout(Duration::from_secs(3), async {
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

        // A local service failure must not leave its runtime alive after reload.
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

        // Hold receipt credit in one agent's inbox; other workset traffic
        // continues, and draining the route delivers the original replies.
        let (old_route, mut blocked) = replacement.pause_agent_route(first_id);
        for _ in 0..80 {
            active.tell_tail();
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while blocked.len() < 16 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(std::path::Path::new(&format!("/proc/{}", replacement.pid)).exists());
        assert_eq!(blocked.len(), 16, "receipt credit must bound the route");
        tokio::time::timeout(
            Duration::from_secs(2),
            replacement.action(crate::WorksetAction::TerminalList),
        )
        .await
        .unwrap()
        .unwrap();
        replacement.restore_agent_route(first_id, old_route.clone(), &mut blocked);
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
        tokio::time::timeout(Duration::from_secs(3), async {
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
        let error = tokio::time::timeout(
            Duration::from_secs(10),
            pool.change_mode(first_id, rho_agent_types::WorksetMode::Exposed),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.to_string().contains("terminal"));
        process.shutdown().await;
        drop((one, two, second, active));
    }
}
