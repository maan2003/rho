//! Process-local pool of running agents.
//!
//! The pool owns the id → running-agent map and the shared repo handles that
//! make live-workspace sharing possible. Higher layers (the daemon) own
//! product policy around it: topics, titles, land leases.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use futures::StreamExt as _;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use rho_db::RhoDb;
use rho_inference::Inference;
use rho_workspaces::{PathOverrides, Repo, UserEnvironment, View, WorkspaceInfo};
use tokio::sync::{Mutex, broadcast};

use crate::claude::ClaudeAgent;
use crate::db::{
    AGENT_USAGE_BUCKET_MS, AgentDisposition, AgentId, AgentReadTxnExt as _, AgentRole,
    AgentRoleSessionProfile as _, AgentRuntime, AgentUsageBucket, AgentWorkflow,
    AgentWriteTxnExt as _, EngineerIntelligence, InferenceModel, InferenceProfile, SessionBinding,
    WorkstreamId,
};
use crate::lazy::Lazy;
use crate::{Agent, AgentInputId, AgentState, InputSourceId, MessageDelivery, StartWorkdir};

/// Runaway protection, not policy: children are user-visible agents.
const MAX_SPAWN_DEPTH: usize = 3;
const MAX_WORKING_CHILDREN: usize = 20;
const ID_LABEL_HEADROOM: u64 = 200;

pub struct AgentPool {
    db: RhoDb,
    inference: Inference,
    path_overrides: PathOverrides,
    user_environment: UserEnvironment,
    agents: Mutex<HashMap<AgentId, RunningAgent>>,
    /// Per-id activation serialization; unrelated cold loads remain
    /// concurrent, while one persisted agent can never restore two loops.
    load_locks: Mutex<HashMap<AgentId, Arc<Mutex<()>>>>,
    /// One shared handle per repo root: live-workspace sharing (joined
    /// agents get one checkout but retain separate View namespaces) only
    /// holds within one instance.
    repos: Mutex<HashMap<Utf8PathBuf, Arc<Repo>>>,
    /// Fires for every agent created in this pool — including agents spawned
    /// by other agents — so every UI connection can pick them up.
    created: broadcast::Sender<AgentCreated>,
    /// Synchronously pre-arms daemon-owned observation before a newly loaded
    /// runtime is returned to callers that may immediately start work.
    activation_observer: std::sync::RwLock<Option<Arc<ActivationObserver>>>,
    /// Fires when a loaded agent completes a turn with a final answer.
    completed_turns: broadcast::Sender<AgentTurnCompleted>,
    /// Fires once for each fully-finished assistant message item.
    completed_assistant_items: broadcast::Sender<AgentAssistantItemCompleted>,
    /// Fires after a user input has been durably accepted into an agent log.
    accepted_inputs: broadcast::Sender<AgentInputAccepted>,
    presentation_changes: broadcast::Sender<AgentPresentationChanged>,
    turn_reports: broadcast::Sender<AgentTurnReported>,
    usage: Mutex<HashMap<(AgentId, u64), AgentUsageBucket>>,
    iris_tool_host: std::sync::RwLock<Option<crate::iris_tools::SharedIrisToolHost>>,
    slack_tool_host: std::sync::RwLock<Option<crate::slack_tools::SharedSlackToolHost>>,
}

/// Broadcast when any agent is created in the pool.
#[derive(Clone)]
pub struct AgentCreated {
    pub workstream: WorkstreamId,
    pub agent_id: AgentId,
    pub agent: RunningAgent,
}

pub type ActivationObserver = dyn Fn(AgentId, RunningAgent) -> BoxFuture<'static, ()> + Send + Sync;

/// Broadcast when an agent completes a turn.
#[derive(Clone, Debug)]
pub struct AgentTurnCompleted {
    pub agent_id: AgentId,
    pub final_answer: String,
}

#[derive(Clone, Debug)]
pub struct AgentAssistantItemCompleted {
    pub agent_id: AgentId,
    pub phase: rho_core::MessagePhase,
    pub text: String,
}

/// Broadcast when a user input is accepted into an agent.
#[derive(Clone, Debug)]
pub struct AgentInputAccepted {
    pub input_id: AgentInputId,
    pub sender: rho_core::MessageSender,
    pub content: Vec<rho_core::ContentPart>,
    pub delivery: MessageDelivery,
    pub source_id: Option<InputSourceId>,
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

/// One agent used as the execution backend for another agent runtime.
///
/// The caller owns provider-specific request correlation. This handle only
/// supplies generic agent input and structured final-output delivery.
pub struct DelegationBackend {
    agent_id: AgentId,
    agent: RunningAgent,
    source_id: InputSourceId,
    completed: broadcast::Receiver<AgentTurnCompleted>,
}

impl DelegationBackend {
    pub fn submit(&self, text: String) {
        self.agent.send_user_message_with_source(
            text,
            MessageDelivery::Immediate,
            Some(self.source_id),
        );
    }

    pub async fn next_final(&mut self) -> anyhow::Result<String> {
        loop {
            let completed = self.completed.recv().await?;
            if completed.agent_id == self.agent_id {
                return Ok(completed.final_answer);
            }
        }
    }
}

/// One entry of a spawned child's working set.
pub struct SpawnWorkdir {
    /// Absolute path anywhere inside the repository (or plain directory).
    pub repo: Utf8PathBuf,
    pub checkout: SpawnCheckout,
}

/// Which checkout a child workdir works in.
pub enum SpawnCheckout {
    /// The checkout the parent uses for this repo (its workspace or a live
    /// checkout), or the user's live checkout when the repo is outside the
    /// parent's working set.
    Shared,
    /// The child's own jj workspace on a new change atop `revset`, which
    /// defaults to the parent's current change when the parent has this repo
    /// and `trunk()` otherwise. Plain (non-jj) directories have no
    /// workspaces and are shared instead.
    Own { revset: Option<String> },
}

impl AgentPool {
    /// Opens the pool over `db`, initializing the agent tables.
    pub async fn new(
        db: RhoDb,
        inference: Inference,
        path_overrides: PathOverrides,
        user_environment: UserEnvironment,
    ) -> Arc<Self> {
        let mut write = db.write().await;
        write.init_agent_tables();
        write.commit();
        let pool = Arc::new(Self {
            db,
            inference: inference.clone(),
            path_overrides,
            user_environment,
            agents: Mutex::new(HashMap::new()),
            load_locks: Mutex::new(HashMap::new()),
            repos: Mutex::new(HashMap::new()),
            created: broadcast::channel(64).0,
            activation_observer: std::sync::RwLock::new(None),
            completed_turns: broadcast::channel(64).0,
            completed_assistant_items: broadcast::channel(64).0,
            accepted_inputs: broadcast::channel(64).0,
            presentation_changes: broadcast::channel(64).0,
            turn_reports: broadcast::channel(64).0,
            usage: Mutex::new(HashMap::new()),
            iris_tool_host: std::sync::RwLock::new(None),
            slack_tool_host: std::sync::RwLock::new(None),
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

    pub fn set_iris_tool_host(&self, host: crate::iris_tools::SharedIrisToolHost) {
        *self.iris_tool_host.write().expect("poison") = Some(host);
    }

    pub(crate) fn iris_tool_host(&self) -> Option<crate::iris_tools::SharedIrisToolHost> {
        self.iris_tool_host.read().expect("poison").clone()
    }

    pub fn set_slack_tool_host(&self, host: crate::slack_tools::SharedSlackToolHost) {
        *self.slack_tool_host.write().expect("poison") = Some(host);
    }

    pub(crate) fn slack_tool_host(&self) -> Option<crate::slack_tools::SharedSlackToolHost> {
        self.slack_tool_host.read().expect("poison").clone()
    }

    pub fn subscribe_created(&self) -> broadcast::Receiver<AgentCreated> {
        self.created.subscribe()
    }

    pub fn set_activation_observer(&self, observer: Arc<ActivationObserver>) {
        *self.activation_observer.write().expect("poison") = Some(observer);
    }

    async fn observe_activation(&self, agent_id: AgentId, agent: RunningAgent) {
        let observer = self.activation_observer.read().expect("poison").clone();
        if let Some(observer) = observer {
            observer(agent_id, agent).await;
        }
    }

    pub fn subscribe_completed_turns(&self) -> broadcast::Receiver<AgentTurnCompleted> {
        self.completed_turns.subscribe()
    }

    pub fn subscribe_completed_assistant_items(
        &self,
    ) -> broadcast::Receiver<AgentAssistantItemCompleted> {
        self.completed_assistant_items.subscribe()
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

    /// Keeps Luna work for this agent alive while the returned handle
    /// exists. Dropping the last handle cancels any in-flight provider call.
    pub async fn watch_presentation(
        &self,
        agent_id: AgentId,
    ) -> Option<crate::presentation::Watch> {
        self.agents
            .lock()
            .await
            .get(&agent_id)
            .and_then(RunningAgent::watch_presentation)
    }

    pub fn subscribe_accepted_inputs(&self) -> broadcast::Receiver<AgentInputAccepted> {
        self.accepted_inputs.subscribe()
    }

    /// Load an agent as a generic delegated-work backend.
    pub async fn delegation_backend(
        self: &Arc<Self>,
        agent_id: AgentId,
    ) -> anyhow::Result<DelegationBackend> {
        let completed = self.subscribe_completed_turns();
        let (_, agent, _) = self.load(agent_id).await?;
        Ok(DelegationBackend {
            agent_id,
            agent,
            source_id: InputSourceId::fresh_internal(),
            completed,
        })
    }

    pub async fn publish_completed_turn(&self, completed: AgentTurnCompleted) {
        self.flush_agent_usage(Some(completed.agent_id)).await;
        let _ = self.completed_turns.send(completed);
    }

    /// Persist that execution stopped and the agent is back in the user's
    /// court. Runtimes call this from their non-coalescing state machines
    /// only when no newer queued turn took over, or on terminal failure
    /// where queued work cannot proceed. Sub-agent turn ends are the
    /// parent's court unless the user has personally engaged the agent.
    pub async fn settle_turn(&self, agent_id: AgentId) {
        self.flush_agent_usage(Some(agent_id)).await;
        let record = self.db.read().get_agent(agent_id);
        if record.parent_agent.is_none() || record.user_interacted {
            let mut write = self.db.write().await;
            write.record_agent_turn_end(crate::db::UnixMillis::now(), agent_id);
            write.commit();
        }
    }

    pub(crate) fn publish_completed_assistant_item(&self, item: AgentAssistantItemCompleted) {
        let _ = self.completed_assistant_items.send(item);
    }

    pub fn publish_accepted_input(&self, accepted: AgentInputAccepted) {
        let _ = self.accepted_inputs.send(accepted);
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

    pub async fn loaded(&self) -> Vec<(AgentId, RunningAgent)> {
        let mut agents = self
            .agents
            .lock()
            .await
            .iter()
            .map(|(agent_id, agent)| (*agent_id, agent.clone()))
            .collect::<Vec<_>>();
        agents.sort_by_key(|(agent_id, _)| *agent_id);
        agents
    }

    pub async fn get(&self, agent_id: AgentId) -> Option<RunningAgent> {
        self.agents.lock().await.get(&agent_id).cloned()
    }

    pub async fn create(
        self: &Arc<Self>,
        workstream: WorkstreamId,
        config: AgentRole,
        display_name: Option<String>,
        start: Vec<StartWorkdir>,
    ) -> anyhow::Result<(AgentId, RunningAgent)> {
        self.create_with_parent(workstream, config, display_name, start, None)
            .await
    }

    async fn create_with_parent(
        self: &Arc<Self>,
        workstream: WorkstreamId,
        config: AgentRole,
        display_name: Option<String>,
        start: Vec<StartWorkdir>,
        parent: Option<AgentId>,
    ) -> anyhow::Result<(AgentId, RunningAgent)> {
        let mode = config.session_profile()?;
        let (agent_id, agent) = match mode {
            SessionBinding::ResponsesGpt55(_)
            | SessionBinding::ResponsesSol(_)
            | SessionBinding::ResponsesLuna(_)
            | SessionBinding::ResponsesTerra(_)
            | SessionBinding::CoordinatorTerra(_)
            | SessionBinding::CoordinatorSol(_)
            | SessionBinding::AdvisorSol(_) => {
                let (agent_id, agent) = Agent::create(
                    self.db.clone(),
                    self.inference.clone(),
                    mode,
                    config,
                    workstream,
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
                    workstream,
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
        self.agents.lock().await.insert(agent_id, agent.clone());
        self.observe_activation(agent_id, agent.clone()).await;
        if config != AgentRole::Iris {
            let _ = self.created.send(AgentCreated {
                workstream,
                agent_id,
                agent: agent.clone(),
            });
        }
        Ok((agent_id, agent))
    }

    /// Create a child agent for `parent` in the parent's mode, joining the
    /// parent's workstream, and mail it its task. Returns once the child has
    /// accepted that task. An empty `workdirs` forks the parent's whole
    /// working set.
    pub async fn spawn_child(
        self: &Arc<Self>,
        parent: AgentId,
        task_name: String,
        prompt: String,
        workdirs: Vec<SpawnWorkdir>,
        config: AgentRole,
    ) -> anyhow::Result<AgentId> {
        self.enforce_spawn_limits(parent).await?;
        let (workstream, parent_workdirs, parent_role) = {
            let read = self.db.read();
            let record = read.get_agent(parent);
            (record.workstream, record.workdirs, record.role)
        };
        let workdirs = if workdirs.is_empty() {
            parent_workdirs
                .iter()
                .map(|info| SpawnWorkdir {
                    repo: info.repo().to_owned(),
                    checkout: SpawnCheckout::Own { revset: None },
                })
                .collect()
        } else {
            workdirs
        };
        let parent_is_sandboxed = parent_workdirs[0].is_sandbox();
        let mut start = Vec::with_capacity(workdirs.len());
        for entry in workdirs {
            let repo = self.repo(&entry.repo).await?;
            let parent_entry = parent_workdirs
                .iter()
                .find(|info| info.repo() == repo.root());
            start.push(match entry.checkout {
                SpawnCheckout::Own { revset } if repo.is_jj() => {
                    // The child's change forks off whatever the parent's
                    // checkout currently points at; repos outside the
                    // parent's working set start from trunk.
                    let parent_revset = revset
                        .unwrap_or_else(|| parent_entry.map_or("trunk()", |_| "@").to_owned());
                    let source = match parent_entry {
                        Some(info) => self.open_workspace(info).await?,
                        None => repo.user_checkout().await?,
                    };
                    let workspace = if parent_is_sandboxed {
                        repo.create_sandbox_from(&source, &parent_revset).await?
                    } else {
                        repo.create_workspace_from(&source, &parent_revset).await?
                    };
                    StartWorkdir::Existing(workspace)
                }
                SpawnCheckout::Own { revset } => {
                    anyhow::ensure!(
                        revset.is_none(),
                        "revset is only supported inside a jj repository: {}",
                        repo.root()
                    );
                    anyhow::ensure!(
                        !parent_is_sandboxed,
                        "sandboxed Engineers cannot spawn into plain directories: {}",
                        repo.root()
                    );
                    // Plain directories have no workspaces to create.
                    StartWorkdir::Existing(repo.user_checkout().await?)
                }
                SpawnCheckout::Shared => {
                    anyhow::ensure!(
                        !parent_is_sandboxed || parent_entry.is_some_and(WorkspaceInfo::is_sandbox),
                        "sandboxed Engineers cannot share an ordinary checkout: {}",
                        repo.root()
                    );
                    match parent_entry {
                        Some(info) => StartWorkdir::Existing(self.open_workspace(info).await?),
                        None => StartWorkdir::Existing(repo.user_checkout().await?),
                    }
                }
            });
        }
        let config = child_role(parent_role, config);
        let (child_id, child) = self
            .create_with_parent(workstream, config, Some(task_name), start, Some(parent))
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
                cursor = read.get_agent(id).parent_agent;
            }
            read.list_agents()
                .into_iter()
                .filter(|(_, record)| {
                    record.parent_agent == Some(parent)
                        && record.disposition != AgentDisposition::Hidden
                })
                .map(|(id, _)| id)
                .collect::<Vec<_>>()
        };
        let agents = self.agents.lock().await;
        let working_children = child_ids
            .into_iter()
            .filter_map(|id| agents.get(&id))
            .filter(|agent| agent.state().kind.is_working())
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
            self.db.read().get_agent(from).role,
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
        let read = self.db.read();
        read.list_agents()
            .iter()
            .any(|(existing, _)| *existing == agent_id)
    }

    /// Short raw prefix for an agent id.
    pub fn agent_id_prefix(&self, agent_id: AgentId) -> String {
        let read = self.db.read();
        let prefix_len =
            prefix_id::uniform_prefix_len(read.last_agent_counter(), ID_LABEL_HEADROOM).max(4);
        agent_id.encoded()[..prefix_len].to_owned()
    }

    pub fn agent_handle(&self, agent_id: AgentId) -> String {
        let role = self.db.read().get_agent(agent_id).role;
        format!(
            "{}-{}",
            role.handle_prefix(),
            self.agent_id_prefix(agent_id)
        )
    }

    /// The shared handle for the workdir containing `path`: the enclosing jj
    /// repo when there is one, otherwise the plain directory itself
    /// (live-only, no separate workspaces). Cache-keyed by the resolved root
    /// so agents in the same repo share one instance.
    pub async fn repo(&self, path: &Utf8Path) -> anyhow::Result<Arc<Repo>> {
        let (root, is_jj) = rho_workspaces::resolve_workdir_root(path.as_std_path())?;
        let repo = if is_jj {
            Repo::open_with_environment(
                root.as_std_path(),
                self.path_overrides.clone(),
                self.user_environment.clone(),
            )?
        } else {
            Repo::open_plain_with_environment(
                root.as_std_path(),
                self.path_overrides.clone(),
                self.user_environment.clone(),
            )?
        };
        let mut repos = self.repos.lock().await;
        Ok(match repos.entry(repo.root().to_owned()) {
            std::collections::hash_map::Entry::Occupied(entry) => Arc::clone(entry.get()),
            std::collections::hash_map::Entry::Vacant(entry) => {
                Arc::clone(entry.insert(Arc::new(repo)))
            }
        })
    }

    pub async fn open_workspace(
        &self,
        info: &WorkspaceInfo,
    ) -> anyhow::Result<Arc<rho_workspaces::Workspace>> {
        match info {
            WorkspaceInfo::UserCheckout { repo } => self.repo(repo).await?.user_checkout().await,
            WorkspaceInfo::Workspace { repo, id } => {
                self.repo(repo).await?.open_workspace(*id).await
            }
            WorkspaceInfo::Sandbox { repo, id } => self.repo(repo).await?.open_sandbox(*id).await,
        }
    }

    /// Materializes an agent's persisted working set into a live view.
    pub async fn materialize_view(&self, workdirs: &[WorkspaceInfo]) -> anyhow::Result<Arc<View>> {
        let mut entries = Vec::with_capacity(workdirs.len());
        for info in workdirs {
            entries.push(self.open_workspace(info).await?);
        }
        View::new(entries)
    }

    fn lazy_view(
        self: &Arc<Self>,
        _agent_id: AgentId,
        workdirs: Vec<WorkspaceInfo>,
    ) -> Arc<Lazy<Arc<View>>> {
        let pool = Arc::downgrade(self);
        Arc::new(Lazy::new(move || {
            let pool = pool.clone();
            let workdirs = workdirs.clone();
            async move {
                let pool = pool.upgrade().context("agent pool dropped")?;
                pool.materialize_view(&workdirs).await
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
            return Ok((agent_id, agent, false));
        }
        let record = self.db.read().get_agent(agent_id);
        let view = self.lazy_view(agent_id, record.workdirs.clone());
        let agent = match record.runtime {
            AgentRuntime::Rho { .. } => RunningAgent::Rho(Agent::load_lazy(
                self.db.clone(),
                self.inference.clone(),
                agent_id,
                view,
                Arc::downgrade(self),
            )),
            AgentRuntime::Claude { .. } => {
                let agent = ClaudeAgent::load(
                    self.db.clone(),
                    self.inference.clone(),
                    agent_id,
                    view,
                    Arc::downgrade(self),
                )
                .await?;
                RunningAgent::Claude(agent)
            }
        };
        self.agents.lock().await.insert(agent_id, agent.clone());
        self.observe_activation(agent_id, agent.clone()).await;
        Ok((agent_id, agent, true))
    }
}

fn child_role(parent: AgentRole, child: AgentRole) -> AgentRole {
    match (parent, child) {
        (
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Alt,
            }
            | AgentRole::WorkflowEngineer {
                intelligence: EngineerIntelligence::Alt,
                ..
            },
            AgentRole::Engineer { .. } | AgentRole::WorkflowEngineer { .. },
        ) => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Cheap,
        },
        (
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Mini,
            }
            | AgentRole::WorkflowEngineer {
                intelligence: EngineerIntelligence::Mini,
                ..
            },
            AgentRole::Engineer { .. } | AgentRole::WorkflowEngineer { .. },
        ) => AgentRole::Engineer {
            intelligence: EngineerIntelligence::Mini,
        },
        (
            AgentRole::WorkflowPM {
                workflow: AgentWorkflow::PrFriendly,
            },
            AgentRole::Engineer { intelligence, .. },
        ) => AgentRole::WorkflowEngineer {
            intelligence,
            workflow: AgentWorkflow::PrFriendly,
        },
        (
            AgentRole::WorkflowPM {
                workflow: AgentWorkflow::PrFriendly,
            },
            AgentRole::WorkflowEngineer { intelligence, .. },
        ) => AgentRole::WorkflowEngineer {
            intelligence,
            workflow: AgentWorkflow::PrFriendly,
        },
        (_, child) => child,
    }
}

#[derive(Clone)]
pub enum RunningAgent {
    Rho(Agent),
    Claude(ClaudeAgent),
}

impl RunningAgent {
    pub(crate) fn watch_presentation(&self) -> Option<crate::presentation::Watch> {
        match self {
            Self::Rho(agent) => Some(agent.watch_presentation()),
            Self::Claude(agent) => Some(agent.watch_presentation()),
        }
    }

    pub fn state(&self) -> AgentState {
        match self {
            Self::Rho(agent) => agent.state(),
            Self::Claude(agent) => agent.state(),
        }
    }

    pub fn send_user_message(&self, text: String, delivery: MessageDelivery) {
        self.send_user_message_with_source(text, delivery, None);
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

    pub async fn send_user_content_accepted(
        &self,
        content: Vec<rho_core::ContentPart>,
        delivery: MessageDelivery,
        source_id: Option<InputSourceId>,
    ) -> anyhow::Result<()> {
        match self {
            Self::Rho(agent) => {
                agent
                    .send_user_content_accepted(content, delivery, source_id)
                    .await
            }
            Self::Claude(agent) => agent.send_user_content_accepted(content).await,
        }
    }

    pub fn send_user_message_with_source(
        &self,
        text: String,
        delivery: MessageDelivery,
        source_id: Option<InputSourceId>,
    ) {
        match self {
            Self::Rho(agent) => agent.send_user_message_with_source(text, delivery, source_id),
            // The Claude CLI does its own mid-turn steering; there is no
            // lane choice to forward.
            Self::Claude(agent) => agent.send_user_message(text),
        }
    }

    /// Deliver mail from another agent.
    pub fn send_agent_message(
        &self,
        sender: AgentId,
        sender_label: String,
        body: String,
        delivery: MessageDelivery,
    ) {
        match self {
            Self::Rho(agent) => agent.send_agent_message(sender, body, delivery),
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
        delivery: MessageDelivery,
    ) -> anyhow::Result<()> {
        match self {
            Self::Rho(agent) => {
                agent
                    .send_agent_message_accepted(sender, body, delivery)
                    .await
            }
            Self::Claude(agent) => {
                agent
                    .send_agent_message_accepted(format!(
                        "Message Type: MESSAGE\nSender: {sender_label}\nPayload:\n{body}"
                    ))
                    .await
            }
        }
    }

    pub fn compact(&self, delivery: MessageDelivery) -> anyhow::Result<()> {
        match self {
            Self::Claude(agent) => {
                agent.compact();
                Ok(())
            }
            Self::Rho(agent) => {
                agent.compact(delivery);
                Ok(())
            }
        }
    }

    pub fn cancel(&self) {
        match self {
            Self::Rho(agent) => agent.cancel(),
            Self::Claude(agent) => agent.cancel(),
        }
    }

    pub fn continue_unfinished(&self) {
        match self {
            Self::Rho(agent) => agent.continue_unfinished(),
            Self::Claude(_) => {}
        }
    }

    pub async fn wait_for_input(&self, timeout: std::time::Duration) -> bool {
        match self {
            Self::Claude(agent) => agent.wait_for_input(timeout).await,
            Self::Rho(agent) => {
                let deadline = tokio::time::Instant::now() + timeout;
                loop {
                    if !agent.state().queued_inputs.is_empty() {
                        return true;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return false;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
        }
    }

    pub fn set_deep_config(
        &self,
        config: InferenceProfile,
        model: InferenceModel,
    ) -> anyhow::Result<()> {
        match self {
            Self::Rho(agent) => {
                agent.set_deep_config(config, model);
                Ok(())
            }
            Self::Claude(_) => anyhow::bail!("cannot apply deep config to Claude agent"),
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
            Self::Claude(_) => {
                anyhow::bail!("prompt cache keys are only available for Rho agents")
            }
        }
    }

    pub async fn rewind(&self, turns: u32) -> anyhow::Result<()> {
        match self {
            Self::Rho(agent) => agent.rewind(turns).await,
            Self::Claude(agent) => agent.rewind(turns).await,
        }
    }

    pub fn subscribe(&self) -> BoxStream<'static, AgentState> {
        match self {
            Self::Rho(agent) => agent.subscribe().boxed(),
            Self::Claude(agent) => agent.subscribe().boxed(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{AgentWorkflow, EngineerIntelligence};

    #[test]
    fn github_workflow_only_flows_from_pm_to_engineer() {
        let engineer = AgentRole::Engineer {
            intelligence: EngineerIntelligence::Medium,
        };
        let pr_pm = AgentRole::WorkflowPM {
            workflow: AgentWorkflow::PrFriendly,
        };
        assert_eq!(
            child_role(pr_pm, engineer).workflow(),
            AgentWorkflow::PrFriendly
        );

        let pr_engineer = AgentRole::WorkflowEngineer {
            intelligence: EngineerIntelligence::Medium,
            workflow: AgentWorkflow::PrFriendly,
        };
        assert_eq!(
            child_role(pr_engineer, engineer).workflow(),
            AgentWorkflow::Default
        );
    }

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
        assert_eq!(
            child_role(
                AgentRole::WorkflowEngineer {
                    intelligence: EngineerIntelligence::Alt,
                    workflow: AgentWorkflow::PrFriendly,
                },
                AgentRole::default(),
            ),
            cheap
        );
    }
}
