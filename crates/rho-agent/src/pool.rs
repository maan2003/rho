//! Process-local pool of running agents.
//!
//! The pool owns the id → running-agent map and the shared repo handles that
//! make live-workspace sharing possible. Higher layers (the daemon) own
//! product policy around it: topics, titles, land leases, voice.

use std::collections::HashMap;
use std::sync::Arc;

use camino::{Utf8Path, Utf8PathBuf};
use futures::StreamExt as _;
use futures::stream::BoxStream;
use rho_db::RhoDb;
use rho_inference::InferenceAuth;
use rho_workspaces::{PathOverrides, Repo, WorkspaceInfo};
use tokio::sync::{Mutex, broadcast};

use crate::claude::ClaudeAgent;
use crate::db::{
    AgentDisposition, AgentId, AgentMode, AgentReadTxnExt as _, AgentRuntime,
    AgentWriteTxnExt as _, DeepConfig, DeepModel, TopicId,
};
use crate::{Agent, AgentState, MessageDelivery, StartWorkspace};

/// Runaway protection, not policy: children are user-visible agents.
const MAX_SPAWN_DEPTH: usize = 3;
const MAX_LIVE_CHILDREN: usize = 8;
const ID_LABEL_HEADROOM: u64 = 200;

pub struct AgentPool {
    db: RhoDb,
    auth: InferenceAuth,
    path_overrides: PathOverrides,
    agents: Mutex<HashMap<AgentId, RunningAgent>>,
    /// One shared handle per repo root: live-workspace sharing (joined
    /// agents get one checkout + namespace) only holds within one instance.
    repos: Mutex<HashMap<Utf8PathBuf, Arc<Repo>>>,
    /// Fires for every agent created in this pool — including agents spawned
    /// by other agents — so every UI connection can pick them up.
    created: broadcast::Sender<AgentCreated>,
}

/// Broadcast when any agent is created in the pool.
#[derive(Clone)]
pub struct AgentCreated {
    pub topic_id: TopicId,
    pub agent_id: AgentId,
    pub agent: RunningAgent,
}

/// Where a spawned child works, relative to its parent.
pub enum SpawnWorkspace {
    /// Share the parent's working copy.
    Join,
    /// Own jj workspace forked from the parent's current change.
    Fork,
    /// Own jj workspace on a new change atop `revset` (default `trunk()`).
    New { revset: Option<String> },
}

impl AgentPool {
    /// Opens the pool over `db`, initializing the agent tables.
    pub async fn new(db: RhoDb, auth: InferenceAuth, path_overrides: PathOverrides) -> Arc<Self> {
        let mut write = db.write().await;
        write.init_agent_tables();
        write.commit();
        Arc::new(Self {
            db,
            auth,
            path_overrides,
            agents: Mutex::new(HashMap::new()),
            repos: Mutex::new(HashMap::new()),
            created: broadcast::channel(64).0,
        })
    }

    pub fn subscribe_created(&self) -> broadcast::Receiver<AgentCreated> {
        self.created.subscribe()
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
        mode: AgentMode,
        display_name: Option<String>,
        start: StartWorkspace,
    ) -> anyhow::Result<(AgentId, RunningAgent)> {
        self.create_with_parent(topic_id, mode, display_name, start, None)
            .await
    }

    async fn create_with_parent(
        self: &Arc<Self>,
        topic_id: TopicId,
        mode: AgentMode,
        display_name: Option<String>,
        start: StartWorkspace,
        parent: Option<AgentId>,
    ) -> anyhow::Result<(AgentId, RunningAgent)> {
        let (agent_id, agent) = match mode {
            AgentMode::Deep(_) | AgentMode::Sol(_) | AgentMode::Luna(_) | AgentMode::Terra(_) => {
                let (agent_id, agent) = Agent::create(
                    self.db.clone(),
                    self.auth.clone(),
                    mode,
                    topic_id,
                    display_name,
                    start,
                    parent,
                    Arc::downgrade(self),
                )
                .await?;
                (agent_id, RunningAgent::Rho(agent))
            }
            AgentMode::Fable { .. } | AgentMode::Opus { .. } => {
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
    /// mail it its task. Returns once the child is running.
    pub async fn spawn_child(
        self: &Arc<Self>,
        parent: AgentId,
        task_name: String,
        prompt: String,
        workspace: SpawnWorkspace,
        mode: AgentMode,
    ) -> anyhow::Result<AgentId> {
        let (topic_id, parent_workspace) = {
            let read = self.db.read();
            let topic_id = read
                .agent_topic(parent)
                .ok_or_else(|| anyhow::anyhow!("spawning agent belongs to no topic"))?;
            let record = read.get_agent(parent);
            self.enforce_spawn_limits(&read, parent)?;
            (topic_id, record.workspace)
        };
        let start = match workspace {
            SpawnWorkspace::Join => {
                StartWorkspace::Existing(self.open_workspace(&parent_workspace).await?)
            }
            SpawnWorkspace::Fork => StartWorkspace::Create {
                repo: self.repo(parent_workspace.repo()).await?,
                // The child's change forks off whatever the parent's checkout
                // currently points at.
                parent_revset: match parent_workspace.workspace_name() {
                    Some(name) => format!("{name}@"),
                    None => "@".to_owned(),
                },
            },
            SpawnWorkspace::New { revset } => StartWorkspace::Create {
                repo: self.repo(parent_workspace.repo()).await?,
                parent_revset: revset.unwrap_or_else(|| "trunk()".to_owned()),
            },
        };
        let (child_id, child) = self
            .create_with_parent(topic_id, mode, Some(task_name), start, Some(parent))
            .await?;
        let parent_label = format!("ag-{}", self.agent_id_prefix(parent));
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
        body: String,
        delivery: MessageDelivery,
    ) -> anyhow::Result<()> {
        let (_, agent, _) = self.load(to).await?;
        let sender_label = format!("ag-{}", self.agent_id_prefix(from));
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

    /// The shared handle for the repo rooted at (or containing) `path`.
    pub async fn repo(&self, path: &Utf8Path) -> anyhow::Result<Arc<Repo>> {
        let repo = Repo::open_with_path_overrides(path.as_std_path(), self.path_overrides.clone())?;
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
        let repo = self.repo(info.repo()).await?;
        match info {
            WorkspaceInfo::UserCheckout { .. } => repo.user_checkout().await,
            WorkspaceInfo::Workspace { id, .. } => repo.open_workspace(*id).await,
        }
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
        let workspace = self.open_workspace(&record.workspace).await?;
        let agent = match record.runtime {
            AgentRuntime::Rho { .. } => RunningAgent::Rho(Agent::load(
                self.db.clone(),
                self.auth.clone(),
                agent_id,
                workspace,
                Arc::downgrade(self),
            )),
            AgentRuntime::Claude { .. } => {
                let agent =
                    ClaudeAgent::load(self.db.clone(), agent_id, workspace, Arc::downgrade(self))
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
        match self {
            Self::Rho(agent) => agent.send_user_message(text, delivery),
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

    pub fn set_deep_config(&self, config: DeepConfig, model: DeepModel) -> anyhow::Result<()> {
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
