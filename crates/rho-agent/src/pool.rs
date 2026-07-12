//! Process-local pool of running agents.
//!
//! The pool owns the id → running-agent map and the shared repo handles that
//! make live-workspace sharing possible. Higher layers (the daemon) own
//! product policy around it: topics, titles, land leases, voice.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use futures::StreamExt as _;
use futures::stream::BoxStream;
use rho_db::RhoDb;
use rho_inference::InferenceAuth;
use rho_workspaces::{PathOverrides, Repo, UserEnvironment, View, WorkspaceInfo};
use tokio::sync::{Mutex, broadcast};

use crate::claude::ClaudeAgent;
use crate::db::{
    AgentDisposition, AgentId, AgentReadTxnExt as _, AgentRole, AgentRuntime,
    AgentWriteTxnExt as _, InferenceModel, InferenceProfile, SessionBinding, TopicId,
};
use crate::lazy::Lazy;
use crate::{
    Agent, AgentInputId, AgentState, AgentToolExtension, AgentToolExtensionFactory, InputSourceId,
    MessageDelivery, StartWorkdir,
};

/// Runaway protection, not policy: children are user-visible agents.
const MAX_SPAWN_DEPTH: usize = 3;
const MAX_LIVE_CHILDREN: usize = 8;
const ID_LABEL_HEADROOM: u64 = 200;

pub struct AgentPool {
    db: RhoDb,
    auth: InferenceAuth,
    path_overrides: PathOverrides,
    user_environment: UserEnvironment,
    agents: Mutex<HashMap<AgentId, RunningAgent>>,
    /// One shared handle per repo root: live-workspace sharing (joined
    /// agents get one checkout + namespace) only holds within one instance.
    repos: Mutex<HashMap<Utf8PathBuf, Arc<Repo>>>,
    /// Fires for every agent created in this pool — including agents spawned
    /// by other agents — so every UI connection can pick them up.
    created: broadcast::Sender<AgentCreated>,
    /// Fires when a loaded agent completes a turn with a final answer.
    completed_turns: broadcast::Sender<AgentTurnCompleted>,
    /// Fires after a user input has been durably accepted into an agent log.
    accepted_inputs: broadcast::Sender<AgentInputAccepted>,
    tool_extension_provider: std::sync::RwLock<Option<Arc<dyn AgentToolExtensionProvider>>>,
}

pub trait AgentToolExtensionProvider: Send + Sync + 'static {
    fn tool_extension(&self, agent_id: AgentId) -> Option<Arc<dyn AgentToolExtension>>;
}

/// Broadcast when any agent is created in the pool.
#[derive(Clone)]
pub struct AgentCreated {
    pub topic_id: TopicId,
    pub agent_id: AgentId,
    pub agent: RunningAgent,
}

/// Broadcast when an agent completes a turn.
#[derive(Clone, Debug)]
pub struct AgentTurnCompleted {
    pub agent_id: AgentId,
    pub final_answer: String,
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
        auth: InferenceAuth,
        path_overrides: PathOverrides,
        user_environment: UserEnvironment,
    ) -> Arc<Self> {
        crate::db::prepare_agent_db_migration(&db).await;
        let mut write = db.write().await;
        write.init_agent_tables();
        write.commit();
        Arc::new(Self {
            db,
            auth,
            path_overrides,
            user_environment,
            agents: Mutex::new(HashMap::new()),
            repos: Mutex::new(HashMap::new()),
            created: broadcast::channel(64).0,
            completed_turns: broadcast::channel(64).0,
            accepted_inputs: broadcast::channel(64).0,
            tool_extension_provider: std::sync::RwLock::new(None),
        })
    }

    pub fn set_tool_extension_provider(&self, provider: Arc<dyn AgentToolExtensionProvider>) {
        *self.tool_extension_provider.write().expect("poison") = Some(provider);
    }

    fn tool_extension_for(&self, agent_id: AgentId) -> Option<Arc<dyn AgentToolExtension>> {
        self.tool_extension_provider
            .read()
            .expect("poison")
            .as_ref()
            .and_then(|provider| provider.tool_extension(agent_id))
    }

    pub fn subscribe_created(&self) -> broadcast::Receiver<AgentCreated> {
        self.created.subscribe()
    }

    pub fn subscribe_completed_turns(&self) -> broadcast::Receiver<AgentTurnCompleted> {
        self.completed_turns.subscribe()
    }

    pub fn subscribe_accepted_inputs(&self) -> broadcast::Receiver<AgentInputAccepted> {
        self.accepted_inputs.subscribe()
    }

    pub fn publish_completed_turn(&self, completed: AgentTurnCompleted) {
        let _ = self.completed_turns.send(completed);
    }

    pub fn publish_accepted_input(&self, accepted: AgentInputAccepted) {
        let _ = self.accepted_inputs.send(accepted);
    }

    pub fn db(&self) -> &RhoDb {
        &self.db
    }

    pub fn auth(&self) -> &InferenceAuth {
        &self.auth
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

    pub async fn load_non_hidden_agents(self: &Arc<Self>) {
        let agent_ids = self.non_hidden_agent_ids();
        for agent_id in agent_ids {
            if let Err(error) = self.load(agent_id).await {
                eprintln!("rho-agent: failed to load active agent {agent_id:?}: {error:#}");
            }
        }
    }

    fn non_hidden_agent_ids(&self) -> Vec<AgentId> {
        self.db
            .read()
            .list_agents()
            .into_iter()
            .filter(|(_, agent)| agent.disposition != AgentDisposition::Hidden)
            .map(|(agent_id, _)| agent_id)
            .collect()
    }

    pub async fn get(&self, agent_id: AgentId) -> Option<RunningAgent> {
        self.agents.lock().await.get(&agent_id).cloned()
    }

    pub async fn create(
        self: &Arc<Self>,
        topic_id: TopicId,
        config: AgentRole,
        display_name: Option<String>,
        start: Vec<StartWorkdir>,
    ) -> anyhow::Result<(AgentId, RunningAgent)> {
        self.create_with_parent(
            topic_id,
            config.session_profile()?,
            display_name,
            start,
            None,
            None,
        )
        .await
    }

    pub async fn create_with_tool_extension(
        self: &Arc<Self>,
        topic_id: TopicId,
        config: AgentRole,
        display_name: Option<String>,
        start: Vec<StartWorkdir>,
        tool_extension: AgentToolExtensionFactory,
    ) -> anyhow::Result<(AgentId, RunningAgent)> {
        self.create_with_parent(
            topic_id,
            config.session_profile()?,
            display_name,
            start,
            None,
            Some(tool_extension),
        )
        .await
    }

    async fn create_with_parent(
        self: &Arc<Self>,
        topic_id: TopicId,
        mode: SessionBinding,
        display_name: Option<String>,
        start: Vec<StartWorkdir>,
        parent: Option<AgentId>,
        tool_extension: Option<AgentToolExtensionFactory>,
    ) -> anyhow::Result<(AgentId, RunningAgent)> {
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
                    self.auth.clone(),
                    mode,
                    topic_id,
                    display_name,
                    start,
                    parent,
                    Arc::downgrade(self),
                    tool_extension,
                )
                .await?;
                (agent_id, RunningAgent::Rho(agent))
            }
            SessionBinding::ClaudeFable { .. }
            | SessionBinding::ClaudeOpus { .. }
            | SessionBinding::ClaudeAdvisor { .. } => {
                let (agent_id, agent) = ClaudeAgent::create(
                    self.db.clone(),
                    topic_id,
                    display_name,
                    start,
                    mode,
                    parent,
                    Arc::downgrade(self),
                )
                .await?;
                (agent_id, RunningAgent::Claude(agent))
            }
        };
        self.agents.lock().await.insert(agent_id, agent.clone());
        let _ = self.created.send(AgentCreated {
            topic_id,
            agent_id,
            agent: agent.clone(),
        });
        Ok((agent_id, agent))
    }

    /// Create a child agent for `parent` in the parent's topic and mode, and
    /// mail it its task. Returns once the child is running. An empty
    /// `workdirs` forks the parent's whole working set.
    pub async fn spawn_child(
        self: &Arc<Self>,
        parent: AgentId,
        task_name: String,
        prompt: String,
        workdirs: Vec<SpawnWorkdir>,
        config: AgentRole,
    ) -> anyhow::Result<AgentId> {
        let (topic_id, parent_workdirs) = {
            let read = self.db.read();
            let topic_id = read
                .agent_topic(parent)
                .ok_or_else(|| anyhow::anyhow!("spawning agent belongs to no topic"))?;
            let record = read.get_agent(parent);
            self.enforce_spawn_limits(&read, parent)?;
            (topic_id, record.workdirs)
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
                    let parent_revset = revset.unwrap_or_else(|| match parent_entry {
                        Some(info) => match info.workspace_name() {
                            Some(name) => format!("{name}@"),
                            None => "@".to_owned(),
                        },
                        None => "trunk()".to_owned(),
                    });
                    StartWorkdir::Create {
                        repo,
                        parent_revset,
                    }
                }
                SpawnCheckout::Own { revset } => {
                    anyhow::ensure!(
                        revset.is_none(),
                        "revset is only supported inside a jj repository: {}",
                        repo.root()
                    );
                    // Plain directories have no workspaces to create.
                    StartWorkdir::Existing(repo.user_checkout().await?)
                }
                SpawnCheckout::Shared => match parent_entry {
                    Some(info) => StartWorkdir::Existing(self.open_workspace(info).await?),
                    None => StartWorkdir::Existing(repo.user_checkout().await?),
                },
            });
        }
        let (child_id, child) = self
            .create_with_parent(
                topic_id,
                config.session_profile()?,
                Some(task_name),
                start,
                Some(parent),
                None,
            )
            .await?;
        let parent_label = self.agent_handle(parent);
        child.send_agent_message(parent, parent_label, prompt, MessageDelivery::NextRequest);
        Ok(child_id)
    }

    fn enforce_spawn_limits(&self, read: &rho_db::ReadTxn, parent: AgentId) -> anyhow::Result<()> {
        let mut depth = 0;
        let mut cursor = Some(parent);
        while let Some(id) = cursor {
            depth += 1;
            if depth > MAX_SPAWN_DEPTH {
                anyhow::bail!("spawn depth limit ({MAX_SPAWN_DEPTH}) reached");
            }
            cursor = read.get_agent(id).parent_agent;
        }
        let live_children = read
            .list_agents()
            .into_iter()
            .filter(|(_, record)| {
                record.parent_agent == Some(parent)
                    && record.disposition != AgentDisposition::Hidden
            })
            .count();
        if live_children >= MAX_LIVE_CHILDREN {
            anyhow::bail!(
                "live sub-agent limit ({MAX_LIVE_CHILDREN}) reached; hide or finish existing \
                 sub-agents first"
            );
        }
        Ok(())
    }

    /// Deliver inter-agent mail, loading the recipient first if needed so a
    /// parked agent still hears follow-ups.
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
        agent.send_agent_message(from, sender_label, body, delivery);
        Ok(())
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
        self.db.read().agent_topic(agent_id).is_some()
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

    fn lazy_view(self: &Arc<Self>, workdirs: Vec<WorkspaceInfo>) -> Arc<Lazy<Arc<View>>> {
        let pool = Arc::downgrade(self);
        Arc::new(Lazy::new(move || {
            let pool = pool.clone();
            let workdirs = workdirs.clone();
            async move {
                pool.upgrade()
                    .context("agent pool dropped")?
                    .materialize_view(&workdirs)
                    .await
            }
        }))
    }

    /// Loads a persisted agent if it is not already running. The returned
    /// bool is true when this call started it.
    pub async fn load(
        self: &Arc<Self>,
        agent_id: AgentId,
    ) -> anyhow::Result<(AgentId, RunningAgent, bool)> {
        if let Some(agent) = self.agents.lock().await.get(&agent_id).cloned() {
            return Ok((agent_id, agent, false));
        }
        let record = self.db.read().get_agent(agent_id);
        let view = self.lazy_view(record.workdirs.clone());
        let agent = match record.runtime {
            AgentRuntime::Rho { .. } => RunningAgent::Rho(Agent::load_lazy(
                self.db.clone(),
                self.auth.clone(),
                agent_id,
                view,
                Arc::downgrade(self),
                self.tool_extension_for(agent_id),
            )),
            AgentRuntime::Claude { .. } => {
                let agent =
                    ClaudeAgent::load(self.db.clone(), agent_id, view, Arc::downgrade(self))
                        .await?;
                RunningAgent::Claude(agent)
            }
        };
        self.agents.lock().await.insert(agent_id, agent.clone());
        Ok((agent_id, agent, true))
    }
}

#[derive(Clone)]
pub enum RunningAgent {
    Rho(Agent),
    Claude(ClaudeAgent),
}

impl RunningAgent {
    pub fn state(&self) -> AgentState {
        match self {
            Self::Rho(agent) => agent.state(),
            Self::Claude(agent) => agent.state(),
        }
    }

    pub fn send_user_message(&self, text: String, delivery: MessageDelivery) {
        self.send_user_message_with_source(text, delivery, None);
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

    pub async fn rewind(&self, turns: u32) -> anyhow::Result<()> {
        match self {
            Self::Rho(agent) => agent.rewind(turns).await,
            Self::Claude(_) => anyhow::bail!("rewind is only available for Rho agents"),
        }
    }

    pub fn subscribe(&self) -> BoxStream<'static, AgentState> {
        match self {
            Self::Rho(agent) => agent.subscribe().boxed(),
            Self::Claude(agent) => agent.subscribe().boxed(),
        }
    }
}
