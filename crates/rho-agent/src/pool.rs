//! Process-local pool of running agents.
//!
//! The pool owns the id → running-agent map and the worksets agents work
//! in. Higher layers (the daemon) own product policy around it: topics,
//! titles, land leases.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::Context as _;
use camino::Utf8PathBuf;
use rho_db::RhoDb;
use rho_fs_view::{Mode, Workset, Worksets};
use rho_inference::Inference;
use tokio::sync::{Mutex, broadcast};

use crate::agent::AgentHandle;
use crate::claude::ClaudeAgent;
use crate::db::{
    AGENT_USAGE_BUCKET_MS, AgentId, AgentReadTxnExt as _, AgentRole, AgentRoleSessionProfile as _,
    AgentRuntime, AgentUsageBucket, AgentWriteTxnExt as _, EngineerIntelligence, SessionBinding,
};
use crate::lazy::Lazy;
use crate::{AgentStatus, MessageDelivery, StartPlace, View, WorkspaceInfo};

/// Runaway protection, not policy: children are user-visible agents.
const MAX_SPAWN_DEPTH: usize = 3;
const MAX_WORKING_CHILDREN: usize = 20;
/// How many agents stay loaded. Past this the least recently used one
/// that is settled and nobody is looking at is dropped; its log is the
/// whole of it, so nothing is lost.
pub const MAX_LOADED: usize = 100;
const ID_LABEL_HEADROOM: u64 = 200;

pub struct AgentPool {
    db: RhoDb,
    inference: Inference,
    /// The worksets agents work in, named by the daemon rather than
    /// resolved here: a library does not reach for the user's state
    /// directory.
    worksets: Arc<Worksets>,
    /// The Claude configuration agents run against, named by the daemon for
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
    presentation_changes: broadcast::Sender<AgentPresentationChanged>,
    turn_reports: broadcast::Sender<AgentTurnReported>,
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

#[derive(Clone, Debug)]
pub struct AgentPresentationChanged {
    pub agent_id: AgentId,
    pub generated_title: Option<String>,
    pub activity: Option<String>,
}

/// Broadcast after a runtime persisted a turn report, for client fan-out.
#[derive(Clone, Debug)]
pub struct AgentTurnReported {
    pub agent_id: AgentId,
    pub report: crate::db::TurnReport,
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
        let pool = Arc::new(Self {
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
            presentation_changes: broadcast::channel(64).0,
            turn_reports: broadcast::channel(64).0,
            usage: Mutex::new(HashMap::new()),
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
    pub fn worksets(&self) -> &Arc<Worksets> {
        &self.worksets
    }

    pub fn subscribe_created(&self) -> broadcast::Receiver<AgentCreated> {
        self.created.subscribe()
    }

    pub fn subscribe_presentation_changes(&self) -> broadcast::Receiver<AgentPresentationChanged> {
        self.presentation_changes.subscribe()
    }

    pub fn subscribe_turn_reports(&self) -> broadcast::Receiver<AgentTurnReported> {
        self.turn_reports.subscribe()
    }

    pub(crate) fn publish_turn_report(&self, agent_id: AgentId, report: crate::db::TurnReport) {
        let _ = self
            .turn_reports
            .send(AgentTurnReported { agent_id, report });
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
        )
        .await;
    }

    pub async fn publish_failed_turn(self: &Arc<Self>, agent_id: AgentId, error: String) {
        self.deliver_response(agent_id, format!("Agent hit an error and stopped: {error}"))
            .await;
    }

    async fn deliver_response(self: &Arc<Self>, target: AgentId, body: String) {
        let subscribers = self.db.read().agent_response_subscribers(target);
        for subscriber in subscribers {
            let _ = self
                .deliver_mail(
                    target,
                    subscriber,
                    body.clone(),
                    MessageDelivery::NextRequest,
                )
                .await;
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

    pub(crate) fn publish_presentation_changed(
        &self,
        agent_id: AgentId,
        generated_title: Option<String>,
        activity: Option<String>,
    ) {
        let _ = self.presentation_changes.send(AgentPresentationChanged {
            agent_id,
            generated_title,
            activity,
        });
    }

    pub fn db(&self) -> &RhoDb {
        &self.db
    }

    pub async fn record_agent_usage(&self, agent_id: AgentId, mut usage: AgentUsageBucket) {
        let now = rho_core::UnixMs::now().0;
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
        let (joined, left) = {
            let mut live = self.live.lock().expect("poison");
            let left = live
                .iter()
                .copied()
                .filter(|agent_id| !union.contains(agent_id))
                .collect::<Vec<_>>();
            let joined = union
                .iter()
                .copied()
                .filter(|agent_id| !live.contains(agent_id))
                .collect::<Vec<_>>();
            *live = union;
            (joined, left)
        };
        let agents = self.agents.lock().await;
        for agent_id in left {
            if let Some(agent) = agents.get(&agent_id) {
                agent.set_watched(false);
            }
        }
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
        agent.set_watched(true);
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
    fn trim(&self, agents: &mut HashMap<AgentId, RunningAgent>) {
        if agents.len() <= MAX_LOADED {
            return;
        }
        let candidates = self
            .recent
            .lock()
            .expect("poison")
            .iter()
            .copied()
            .collect::<Vec<_>>();
        for agent_id in candidates {
            if agents.len() <= MAX_LOADED {
                break;
            }
            if self.is_live(agent_id) {
                continue;
            }
            let settled = agents.get(&agent_id).is_some_and(RunningAgent::settled);
            if !settled {
                continue;
            }
            agents.remove(&agent_id);
            self.recent
                .lock()
                .expect("poison")
                .retain(|id| *id != agent_id);
        }
    }

    pub async fn create(
        self: &Arc<Self>,
        config: AgentRole,
        display_name: Option<String>,
        start: StartPlace,
    ) -> anyhow::Result<(AgentId, RunningAgent)> {
        self.create_with_parent(config, display_name, start, None)
            .await
    }

    async fn create_with_parent(
        self: &Arc<Self>,
        config: AgentRole,
        display_name: Option<String>,
        start: StartPlace,
        parent: Option<AgentId>,
    ) -> anyhow::Result<(AgentId, RunningAgent)> {
        let owned_workset = start.owned_workset.clone();
        match self.create_agent(config, display_name, start, parent).await {
            Ok(created) => Ok(created),
            Err(error) => {
                // A workset made for an agent that never came to be.
                if let Some(workset) = owned_workset
                    && let Err(discard) = self.worksets.discard_workset(&workset).await
                {
                    eprintln!("rho-agent: discard workset {workset}: {discard:#}");
                }
                Err(error)
            }
        }
    }

    async fn create_agent(
        self: &Arc<Self>,
        config: AgentRole,
        display_name: Option<String>,
        start: StartPlace,
        parent: Option<AgentId>,
    ) -> anyhow::Result<(AgentId, RunningAgent)> {
        let mode = config.session_profile()?;
        let (agent_id, agent) = match mode {
            SessionBinding::ResponsesGpt55(_)
            | SessionBinding::ResponsesSol(_)
            | SessionBinding::ResponsesLuna(_)
            | SessionBinding::ResponsesTerra(_)
            | SessionBinding::ResponsesAstra(_)
            | SessionBinding::AdvisorSol(_)
            | SessionBinding::AdvisorTerra(_)
            | SessionBinding::AdvisorAstra(_)
            | SessionBinding::AntigravityFlashLow(_) => {
                let (agent_id, agent) = AgentHandle::create(
                    self.db.clone(),
                    self.inference.clone(),
                    mode,
                    config,
                    display_name,
                    start,
                    parent,
                    Arc::downgrade(self),
                )
                .await?;
                (agent_id, RunningAgent::Rho(agent))
            }
            SessionBinding::ClaudeFable { .. }
            | SessionBinding::ClaudeOpus { .. }
            | SessionBinding::ClaudeAdvisor { .. } => {
                let (agent_id, agent) = ClaudeAgent::create(
                    self.db.clone(),
                    self.inference.clone(),
                    self.claude.clone(),
                    display_name,
                    start,
                    mode,
                    config,
                    parent,
                    Arc::downgrade(self),
                )
                .await?;
                (agent_id, RunningAgent::Claude(agent))
            }
        };
        {
            let mut agents = self.agents.lock().await;
            agents.insert(agent_id, agent.clone());
            self.touch(agent_id);
            self.attach_live(agent_id, &agent);
            self.trim(&mut agents);
        }
        let _ = self.created.send(AgentCreated { agent_id, parent });
        Ok((agent_id, agent))
    }

    /// Create a child agent for `parent` in the parent's workset, in the
    /// parent's working directory, and mail it its task. Returns once the
    /// child has accepted that task. A parent that wants the child elsewhere
    /// (its own git worktree, say) makes that directory first
    /// and says so in the prompt.
    pub async fn spawn_child(
        self: &Arc<Self>,
        parent: AgentId,
        task_name: String,
        prompt: String,
        config: AgentRole,
    ) -> anyhow::Result<AgentId> {
        self.enforce_spawn_limits(parent).await?;
        let (parent_place, parent_role) = {
            let record = self.load(parent).await?.1.head();
            (record.primary_workdir().clone(), record.config.role)
        };
        let WorkspaceInfo::Workset {
            workset,
            cwd,
            mode,
            origin,
        } = parent_place
        else {
            anyhow::bail!("this agent predates worksets and cannot spawn children");
        };
        let workset = self.worksets.open_workset(&workset).await?;
        let mode = Mode::from_workset_mode(mode);
        let view = workset.enter(mode, &cwd)?;
        let start = StartPlace::new(view, origin);
        let config = child_role(parent_role, config);
        let (child_id, child) = self
            .create_with_parent(config, Some(task_name), start, Some(parent))
            .await?;
        self.set_response_subscription(parent, child_id, true)
            .await?;
        let parent_label = self.agent_handle(parent);
        child
            .send_agent_message_accepted(parent, parent_label, prompt, MessageDelivery::NextRequest)
            .await
            .with_context(|| {
                format!(
                    "created child {} but it did not accept its initial task",
                    self.agent_handle(child_id)
                )
            })?;
        Ok(child_id)
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
        let (_, agent, _) = self.load(to).await?;
        let sender_label = self.agent_handle(from);
        if matches!(
            self.load(from).await?.1.head().config.role,
            AgentRole::Advisor { .. }
        ) {
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
    ) -> anyhow::Result<prefix_id::PrefixResolution<crate::db::AgentIdDomain>> {
        let text = text.trim();
        let read = self.db.read();
        let domain = crate::db::AgentIdDomain(read.machine_seed());
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
        info: &WorkspaceInfo,
    ) -> anyhow::Result<(Workset, Mode, Utf8PathBuf)> {
        let WorkspaceInfo::Workset {
            workset, cwd, mode, ..
        } = info
        else {
            anyhow::bail!(
                "this agent predates worksets; its transcript is readable but it cannot run"
            );
        };
        let workset = self.worksets.open_workset(workset).await?;
        let mode = Mode::from_workset_mode(*mode);
        let host_cwd = workset.host_path(cwd)?;
        Ok((workset, mode, host_cwd))
    }

    /// Materializes an agent's persisted place into a live view.
    pub async fn materialize_view(&self, info: &WorkspaceInfo) -> anyhow::Result<Arc<View>> {
        let (workset, mode, _) = self.open_workset(info).await?;
        workset.enter(mode, info.repo())
    }

    fn lazy_view(
        self: &Arc<Self>,
        _agent_id: AgentId,
        info: WorkspaceInfo,
    ) -> Arc<Lazy<Arc<View>>> {
        let pool = Arc::downgrade(self);
        Arc::new(Lazy::new(move || {
            let pool = pool.clone();
            let info = info.clone();
            async move {
                let pool = pool.upgrade().context("agent pool dropped")?;
                pool.materialize_view(&info).await
            }
        }))
    }

    /// Loads a persisted agent if it is not already running. The returned
    /// bool is true when this call started it.
    pub async fn load(
        self: &Arc<Self>,
        agent_id: AgentId,
    ) -> anyhow::Result<(AgentId, RunningAgent, bool)> {
        // Lazy loading makes concurrent UI subscriptions, mail, and
        // integrations commonplace. Serialize this id through construction,
        // then recheck after waiting so two loops cannot restore at the same
        // event position. Other agents still load concurrently.
        let load_lock = self
            .load_locks
            .lock()
            .await
            .entry(agent_id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        let _loading = load_lock.lock().await;
        if let Some(agent) = self.agents.lock().await.get(&agent_id).cloned() {
            self.touch(agent_id);
            return Ok((agent_id, agent, false));
        }
        let record = self.db.read().get_agent(agent_id);
        let view = self.lazy_view(agent_id, record.primary_workdir().clone());
        let agent = match record.config.runtime {
            AgentRuntime::Rho { .. } => RunningAgent::Rho(
                AgentHandle::load(
                    self.db.clone(),
                    self.inference.clone(),
                    agent_id,
                    view,
                    Arc::downgrade(self),
                )
                .await?,
            ),
            AgentRuntime::Claude { .. } => {
                let agent = ClaudeAgent::load(
                    self.db.clone(),
                    self.inference.clone(),
                    self.claude.clone(),
                    agent_id,
                    view,
                    Arc::downgrade(self),
                )
                .await?;
                RunningAgent::Claude(agent)
            }
        };
        {
            let mut agents = self.agents.lock().await;
            agents.insert(agent_id, agent.clone());
            self.touch(agent_id);
            self.attach_live(agent_id, &agent);
            self.trim(&mut agents);
        }
        Ok((agent_id, agent, true))
    }
}

fn child_role(parent: AgentRole, child: AgentRole) -> AgentRole {
    match (parent, child) {
        (
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Alt,
            },
            AgentRole::Engineer { .. },
        ) => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Cheap,
        },
        (
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Cheap,
            },
            AgentRole::Engineer { .. },
        ) => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Cheap,
        },
        (
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Cheap,
            },
            AgentRole::Advisor { .. },
        ) => AgentRole::Advisor {
            intelligence: crate::db::AdvisorIntelligence::Cheap,
        },
        (
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Mini,
            },
            AgentRole::Engineer { .. },
        ) => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Mini,
        },
        (_, child) => child,
    }
}

#[derive(Clone)]
pub enum RunningAgent {
    Rho(AgentHandle),
    Claude(ClaudeAgent),
}

impl RunningAgent {
    /// Whether anyone is looking at this agent; titles and activity are
    /// made only then.
    pub(crate) fn set_watched(&self, watching: bool) {
        match self {
            Self::Rho(agent) => agent.set_watched(watching),
            Self::Claude(agent) => agent.set_watched(watching),
        }
    }

    /// The agent's view, ready once its place is: a new agent's clone may
    /// still be in flight.
    pub async fn view(&self) -> anyhow::Result<Arc<View>> {
        match self {
            Self::Rho(agent) => agent.view().await,
            Self::Claude(agent) => agent.view().await,
        }
    }

    pub fn status(&self) -> AgentStatus {
        match self {
            Self::Rho(agent) => agent.status(),
            Self::Claude(agent) => agent.status(),
        }
    }

    /// The record as the loop keeps it.
    pub fn head(&self) -> crate::db::AgentHead {
        match self {
            Self::Rho(agent) => agent.head(),
            Self::Claude(agent) => agent.head(),
        }
    }

    /// Say the live tail whole again.
    pub fn tell_tail(&self) {
        match self {
            Self::Rho(agent) => agent.tell_tail(),
            Self::Claude(agent) => agent.tell_tail(),
        }
    }

    /// Nothing running and nothing waiting: safe to drop.
    pub fn settled(&self) -> bool {
        self.status().settled()
    }

    pub fn send_user_message(&self, text: String, delivery: MessageDelivery) {
        match self {
            Self::Rho(agent) => agent.send_user_message(text, delivery),
            // The Claude CLI does its own mid-turn steering; there is no
            // lane choice to forward.
            Self::Claude(agent) => agent.send_user_message(text),
        }
    }

    pub fn send_user_content(
        &self,
        content: Vec<rho_core::ContentPart>,
        delivery: MessageDelivery,
    ) {
        match self {
            Self::Rho(agent) => agent.send_user_content(content, delivery),
            Self::Claude(agent) => agent.send_user_content(content),
        }
    }

    /// Send user input and return once the agent has durably queued it.
    pub async fn send_user_content_accepted(
        &self,
        content: Vec<rho_core::ContentPart>,
        delivery: MessageDelivery,
    ) -> anyhow::Result<()> {
        match self {
            Self::Rho(agent) => agent.send_user_content_accepted(content, delivery).await,
            Self::Claude(agent) => agent.send_user_content_accepted(content).await,
        }
    }

    /// Deliver mail from another agent.
    pub fn send_agent_message(
        &self,
        sender: AgentId,
        sender_label: String,
        body: String,
        _delivery: MessageDelivery,
    ) {
        match self {
            Self::Rho(agent) => agent.send_agent_message(sender, body),
            // Claude has no agent-mail lane; mail arrives as a labeled user
            // message.
            Self::Claude(agent) => agent.send_user_message(format!(
                "Message Type: MESSAGE\nSender: {sender_label}\nPayload:\n{body}"
            )),
        }
    }

    pub async fn send_agent_message_accepted(
        &self,
        sender: AgentId,
        sender_label: String,
        body: String,
        _delivery: MessageDelivery,
    ) -> anyhow::Result<()> {
        match self {
            Self::Rho(agent) => agent.send_agent_message_accepted(sender, body).await,
            Self::Claude(agent) => {
                agent
                    .send_agent_message_accepted(format!(
                        "Message Type: MESSAGE\nSender: {sender_label}\nPayload:\n{body}"
                    ))
                    .await
            }
        }
    }

    pub fn compact(&self) {
        match self {
            Self::Claude(agent) => agent.compact(),
            Self::Rho(agent) => agent.compact(),
        }
    }

    pub fn cancel(&self) {
        match self {
            Self::Rho(agent) => agent.cancel(),
            Self::Claude(agent) => agent.cancel(),
        }
    }

    /// Retry after a failure, or resume a turn a restart interrupted.
    pub fn retry(&self) {
        match self {
            Self::Rho(agent) => agent.retry(),
            Self::Claude(_) => {}
        }
    }

    pub async fn set_claude_effort(&self, effort: rho_claude::Effort) -> anyhow::Result<()> {
        match self {
            Self::Claude(agent) => agent.set_effort(effort).await,
            Self::Rho(_) => anyhow::bail!("cannot apply Claude effort to Rho agent"),
        }
    }

    pub async fn change_role(&self, role: AgentRole) -> anyhow::Result<()> {
        match self {
            Self::Claude(agent) => agent.change_role(role).await,
            Self::Rho(agent) => agent.change_role(role).await,
        }
    }

    pub fn change_prompt_cache_key(&self) -> anyhow::Result<()> {
        match self {
            Self::Rho(agent) => {
                agent.change_prompt_cache_key();
                Ok(())
            }
            Self::Claude(_) => anyhow::bail!("prompt cache keys are only available for Rho agents"),
        }
    }

    pub async fn rewind(&self, turns: u32) -> anyhow::Result<()> {
        match self {
            Self::Rho(agent) => agent.rewind(turns).await,
            Self::Claude(agent) => agent.rewind(turns).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::EngineerIntelligence;

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
                    intelligence: crate::db::AdvisorIntelligence::Medium,
                }
            ),
            AgentRole::Advisor {
                intelligence: crate::db::AdvisorIntelligence::Medium,
            }
        );
    }

    #[test]
    fn alt_engineers_spawn_cheap_engineers() {
        let cheap = AgentRole::Engineer {
            intelligence: EngineerIntelligence::Cheap,
        };

        assert_eq!(
            child_role(
                AgentRole::Engineer {
                    intelligence: EngineerIntelligence::Alt,
                },
                AgentRole::default(),
            ),
            cheap
        );
    }

    #[test]
    fn cheap_engineers_spawn_cheap_agents() {
        let cheap = AgentRole::Engineer {
            intelligence: EngineerIntelligence::Cheap,
        };

        assert_eq!(child_role(cheap, AgentRole::default()), cheap);
        assert_eq!(
            child_role(
                cheap,
                AgentRole::Advisor {
                    intelligence: crate::db::AdvisorIntelligence::Medium,
                },
            ),
            AgentRole::Advisor {
                intelligence: crate::db::AdvisorIntelligence::Cheap,
            }
        );
    }
}
