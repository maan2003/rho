use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use futures::StreamExt as _;
use rho_agent::db::{
    AgentAttentionRecord, AgentDisposition, AgentId, AgentMode, AgentReadTxnExt as _, AgentRuntime,
    AgentWriteTxnExt as _, Status, TopicId,
};
use rho_agent::pool::{AgentPool, RunningAgent, SpawnWorkspace};
use rho_agent::{AgentStateKind, MessageDelivery};
use rho_core::text_content;
use rho_db::RhoDb;
use rho_inference::InferenceAuth;
use rho_ui_proto::remote::AgentRemoteEncoder;
use rho_ui_proto::server::{Server, ServerConnection};
use rho_ui_proto::{
    ClientMessage, JoinTarget, LandLeaseHolder, LandStatus, McpAgentToolRequest,
    McpAgentToolResponse, McpSpawnWorkspace, ServerMessage, StartMode, UiAgentSummary, UiAttention,
    UiTopic, UiWorkdir, read_frame_counted, write_frame_counted,
};
use tokio::sync::{Mutex, Mutex as TokioMutex, Notify, OwnedMutexGuard, broadcast, mpsc};

pub mod debug;
mod voice;

pub fn default_socket_path() -> anyhow::Result<PathBuf> {
    let base = dirs::runtime_dir()
        .or_else(dirs::state_dir)
        .ok_or_else(|| anyhow::anyhow!("runtime directory not available"))?;
    Ok(base.join("rho").join("rho.sock"))
}

pub fn default_db_path() -> anyhow::Result<PathBuf> {
    let base = dirs::state_dir().ok_or_else(|| anyhow::anyhow!("state directory not available"))?;
    Ok(base.join("rho").join("rho.redb"))
}

/// Re-exported so daemon entry points can set up the user+mount namespace
/// before the async runtime starts (see
/// [`rho_workspaces::init_daemon_namespace`]).
pub use rho_workspaces::{PathOverrides, init_daemon_namespace};

#[derive(Clone, Debug, clap::Args)]
pub struct DaemonArgs {
    #[arg(long = "auth", default_value = "default")]
    pub auth: String,
    #[arg(long = "socket-path")]
    pub socket_path: Option<PathBuf>,
    /// Exit once the last UI client disconnects.
    #[arg(long = "die-on-detached")]
    pub die_on_detached: bool,
    #[arg(long = "extra-before-path", env = "RHO_EXTRA_BEFORE_PATH")]
    pub extra_before_path: Option<OsString>,
    #[arg(long = "extra-after-path", env = "RHO_EXTRA_AFTER_PATH")]
    pub extra_after_path: Option<OsString>,
}

pub async fn run(args: DaemonArgs) -> anyhow::Result<()> {
    // The daemon's own cwd must never matter: agents each carry their own
    // working directory. Park the process somewhere empty and read-only so
    // any code still depending on process cwd fails loudly.
    let _ = std::env::set_current_dir("/var/empty").or_else(|_| std::env::set_current_dir("/"));

    let socket_path = args.socket_path.unwrap_or(default_socket_path()?);
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).context("create socket directory")?;
    }
    let _ = std::fs::remove_file(&socket_path);
    let server = Server::bind(&socket_path).context("bind rho daemon socket")?;

    let db = RhoDb::open(default_db_path()?);
    let auth = InferenceAuth::named(&args.auth)?;
    let path_overrides = PathOverrides {
        before: args
            .extra_before_path
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default(),
        after: args
            .extra_after_path
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default(),
    };
    let agents = Arc::new(AgentRegistry::new(db, auth, path_overrides).await);

    // Attention watchers: one per loaded agent, daemon-owned (not tied to
    // any connection). Preloaded agents are covered here; later creations
    // ride the pool's `created` broadcast, and late loads the LoadAgent
    // handler.
    for (agent_id, agent) in agents.loaded().await {
        spawn_attention_watcher(agents.db.clone(), agents.events.clone(), agent_id, agent);
    }
    {
        let mut created_rx = agents.pool.subscribe_created();
        let db = agents.db.clone();
        let events = agents.events.clone();
        tokio::spawn(async move {
            loop {
                match created_rx.recv().await {
                    Ok(created) => spawn_attention_watcher(
                        db.clone(),
                        events.clone(),
                        created.agent_id,
                        created.agent,
                    ),
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
    // Re-arm snooze wake-ups that were pending when the daemon last stopped.
    for (agent_id, record) in agents.db.read().list_agent_attention() {
        if let AgentDisposition::Snoozed { until } = record.disposition
            && until > rho_core::UnixMs::now()
        {
            spawn_snooze_timer(agents.db.clone(), agents.events.clone(), agent_id, until);
        }
    }

    let active_connections = Arc::new(AtomicUsize::new(0));
    let connection_closed = Arc::new(Notify::new());
    let mut accepted_connection = false;

    loop {
        if args.die_on_detached
            && accepted_connection
            && active_connections.load(Ordering::Relaxed) == 0
        {
            return Ok(());
        }

        tokio::select! {
            connection = server.accept() => {
                let connection = connection?;
                accepted_connection = true;
                active_connections.fetch_add(1, Ordering::Relaxed);
                let agents = agents.clone();
                let active_connections = active_connections.clone();
                let connection_closed = connection_closed.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve_connection(agents, connection).await {
                        eprintln!("rho daemon connection error: {error:#}");
                    }
                    active_connections.fetch_sub(1, Ordering::Relaxed);
                    connection_closed.notify_one();
                });
            }
            () = connection_closed.notified(), if active_connections.load(Ordering::Relaxed) > 0 => {}
        }
    }
}

struct AgentRegistry {
    pool: Arc<AgentPool>,
    db: RhoDb,
    auth: InferenceAuth,
    /// The daemon-created topic agents are born into; announced in `Ready`
    /// so clients never guess it from topic ordering.
    default_topic_id: TopicId,
    /// The database's machine seed, announced in `Ready` so clients can
    /// encode agent IDs.
    machine_seed: u64,
    /// Agents with a title generation in flight, so a burst of messages to an
    /// untitled agent starts at most one task.
    title_tasks: Mutex<HashSet<AgentId>>,
    land_locks: Mutex<HashMap<Utf8PathBuf, Arc<TokioMutex<()>>>>,
    land_holders: Mutex<HashMap<Utf8PathBuf, LandLeaseHolder>>,
    land_statuses: Mutex<HashMap<Utf8PathBuf, LandStatus>>,
    /// At most one realtime voice session per daemon (see [`voice`]).
    voice_active: Arc<std::sync::atomic::AtomicBool>,
    /// Daemon-wide fanout for messages every client must hear regardless of
    /// which connection caused them (attention changes); each connection
    /// forwards this onto its own outgoing channel.
    events: broadcast::Sender<ServerMessage>,
}

impl AgentRegistry {
    async fn new(db: RhoDb, auth: InferenceAuth, path_overrides: PathOverrides) -> Self {
        let pool = AgentPool::new(db.clone(), auth.clone(), path_overrides).await;
        let machine_seed = db.read().machine_seed();
        // Topics are ad-hoc tab groups; every agent starts in the default
        // one (the oldest topic) until it is moved somewhere more specific.
        let oldest_topic = db
            .read()
            .list_topics()
            .into_iter()
            .min_by_key(|(_, topic)| topic.created_at);
        let default_topic_id = match oldest_topic {
            Some((topic_id, _)) => topic_id,
            None => {
                let mut write = db.write().await;
                let topic_id = write.create_topic(
                    rho_core::UnixMs::now(),
                    "default".to_owned(),
                    Status::Normal,
                );
                write.commit();
                topic_id
            }
        };
        let registry = Self {
            pool,
            db,
            auth,
            default_topic_id,
            machine_seed,
            title_tasks: Mutex::new(HashSet::new()),
            land_locks: Mutex::new(HashMap::new()),
            land_holders: Mutex::new(HashMap::new()),
            land_statuses: Mutex::new(HashMap::new()),
            voice_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            events: broadcast::channel(1024).0,
        };
        registry.pool.load_non_archived_agents().await;
        registry
    }

    /// Attention for an agent that is not mid-turn: pending turn ends demand
    /// the user, dispositions and unexpired snoozes silence them, and
    /// sub-agents are always quiet (their turns are the parent's court).
    fn settled_attention(&self, agent_id: AgentId) -> UiAttention {
        settled_attention(&self.db, agent_id)
    }

    /// Agents currently mid-turn, for summary building.
    async fn working_agents(&self) -> HashSet<AgentId> {
        self.pool
            .loaded()
            .await
            .into_iter()
            .filter(|(_, agent)| is_working(&agent.state().kind))
            .map(|(agent_id, _)| agent_id)
            .collect()
    }

    /// Applies the user's verdict and tells every client the new level; for
    /// snoozes, arms the wake-up timer.
    async fn set_disposition(&self, agent_id: AgentId, disposition: AgentDisposition) {
        let mut write = self.db.write().await;
        write.set_agent_disposition(agent_id, disposition);
        write.commit();
        if let AgentDisposition::Snoozed { until } = disposition {
            spawn_snooze_timer(self.db.clone(), self.events.clone(), agent_id, until);
        }
        let attention = match self.get(agent_id).await {
            Some(agent) if is_working(&agent.state().kind) => UiAttention::Working,
            _ => self.settled_attention(agent_id),
        };
        let _ = self.events.send(ServerMessage::AgentAttention {
            agent_id,
            attention,
        });
    }

    fn topics(&self, working: &HashSet<AgentId>) -> Vec<UiTopic> {
        let read = self.db.read();
        // Key order over ids is meaningless (scrambled characters); creation
        // order comes from the timestamps.
        let mut records = read.list_topics();
        records.sort_by_key(|(_, topic)| topic.created_at);
        records
            .into_iter()
            .map(|(topic_id, topic)| {
                let mut agents = read
                    .list_topic_agents(topic_id)
                    .into_iter()
                    .map(|agent_id| (agent_id, read.get_agent(agent_id)))
                    .collect::<Vec<_>>();
                agents.sort_by_key(|(_, agent)| agent.created_at);
                UiTopic {
                    topic_id,
                    name: topic.name,
                    status: topic.status,
                    agents: agents
                        .into_iter()
                        .map(|(agent_id, agent)| UiAgentSummary {
                            agent_id,
                            display_name: agent.display_name,
                            created_at: agent.created_at,
                            updated_at: agent.updated_at,
                            mode: agent.mode,
                            workspace: agent.workspace,
                            status: agent.status,
                            attention: if working.contains(&agent_id) {
                                UiAttention::Working
                            } else {
                                self.settled_attention(agent_id)
                            },
                        })
                        .collect(),
                }
            })
            .collect()
    }

    fn workdirs(&self) -> Vec<UiWorkdir> {
        let mut workdirs = self
            .db
            .read()
            .list_workdirs()
            .into_iter()
            .map(|(path, record)| UiWorkdir {
                path,
                name: record.name,
            })
            .collect::<Vec<_>>();
        workdirs.sort_by(|left, right| left.name.cmp(&right.name));
        workdirs
    }

    async fn ready_message(&self) -> ServerMessage {
        ServerMessage::Ready {
            topics: self.topics(&self.working_agents().await),
            workdirs: self.workdirs(),
            default_topic_id: self.default_topic_id,
            machine_seed: self.machine_seed,
            agent_counter: self.db.read().last_agent_counter(),
            workspace_counter: self.db.read().last_workspace_counter(),
        }
    }

    async fn loaded(&self) -> Vec<(AgentId, RunningAgent)> {
        self.pool.loaded().await
    }

    async fn get(&self, agent_id: AgentId) -> Option<RunningAgent> {
        self.pool.get(agent_id).await
    }

    async fn land_lock(&self, repo: Utf8PathBuf) -> Arc<TokioMutex<()>> {
        let mut locks = self.land_locks.lock().await;
        Arc::clone(
            locks
                .entry(repo)
                .or_insert_with(|| Arc::new(TokioMutex::new(()))),
        )
    }

    async fn land_holder(&self, repo: &Utf8PathBuf) -> Option<LandLeaseHolder> {
        self.land_holders.lock().await.get(repo).cloned()
    }

    async fn set_land_holder(&self, repo: Utf8PathBuf, holder: LandLeaseHolder) {
        self.land_holders.lock().await.insert(repo, holder);
    }

    async fn clear_land_holder(&self, repo: &Utf8PathBuf) {
        self.land_holders.lock().await.remove(repo);
    }

    async fn set_land_status(&self, repo: Utf8PathBuf, status: LandStatus) {
        self.land_statuses.lock().await.insert(repo, status);
    }

    async fn create_topic(&self, name: String) -> UiTopic {
        let mut write = self.db.write().await;
        let topic_id = write.create_topic(rho_core::UnixMs::now(), name, Status::Normal);
        write.commit();
        UiTopic {
            topic_id,
            name: self.db.read().get_topic(topic_id).name,
            status: Status::Normal,
            agents: Vec::new(),
        }
    }

    async fn create(
        &self,
        topic_id: TopicId,
        mode: AgentMode,
        start: StartMode,
    ) -> anyhow::Result<(TopicId, AgentId, RunningAgent)> {
        let start = match start {
            StartMode::NewOn { repo, revset } => {
                let repo = validate_repo_root(repo)?;
                rho_agent::StartWorkspace::Create {
                    repo: self.pool.repo(&repo).await?,
                    parent_revset: revset,
                }
            }
            StartMode::Join(JoinTarget::Workspace(info)) => {
                rho_agent::StartWorkspace::Existing(self.pool.open_workspace(&info).await?)
            }
            StartMode::Join(JoinTarget::User { repo }) => {
                let repo = validate_repo_root(repo)?;
                rho_agent::StartWorkspace::Existing(
                    self.pool.repo(&repo).await?.user_checkout().await?,
                )
            }
        };
        let (agent_id, agent) = self.pool.create(topic_id, mode, None, start).await?;
        Ok((topic_id, agent_id, agent))
    }

    async fn mcp_agent_tool(
        &self,
        self_agent_id: AgentId,
        request: McpAgentToolRequest,
    ) -> anyhow::Result<String> {
        if !self.pool.agent_exists(self_agent_id) {
            anyhow::bail!("agent is not known: {self_agent_id:?}");
        }
        match request {
            McpAgentToolRequest::SpawnAgent {
                task_name,
                prompt,
                workspace,
                mode,
            } => {
                if prompt.trim().is_empty() {
                    anyhow::bail!("prompt must not be empty");
                }
                let workspace = match workspace {
                    McpSpawnWorkspace::Join => SpawnWorkspace::Join,
                    McpSpawnWorkspace::Fork => SpawnWorkspace::Fork,
                    McpSpawnWorkspace::New { revset } => SpawnWorkspace::New { revset },
                };
                let child_id = self
                    .pool
                    .spawn_child(
                        self_agent_id,
                        task_name.clone(),
                        prompt,
                        workspace,
                        rho_agent::multi_agent_tools::parse_spawn_mode(&mode)?,
                    )
                    .await?;
                let child_workspace = self.pool.db().read().get_agent(child_id).workspace;
                let workspace_note = match child_workspace.workspace_name() {
                    Some(workspace) => format!(
                        " Its jj workspace is `{workspace}`; inspect its working-copy commit with \
                         `jj diff -r '{workspace}@' --stat`."
                    ),
                    None => " It is running in the shared user checkout workspace; there is no \
                             separate `<workspace>@` handle."
                        .to_owned(),
                };
                Ok(format!(
                    "Spawned agent {} for task \"{}\". It is working now; its results will arrive \
                     as mail from that agent.{} Use send_message to follow up and wait to block for \
                     its results.",
                    self.display_agent_id(child_id),
                    task_name,
                    workspace_note,
                ))
            }
            McpAgentToolRequest::SendMessage { agent_id, message } => {
                if message.trim().is_empty() {
                    anyhow::bail!("message must not be empty");
                }
                let recipient = self.resolve_display_agent_id(&agent_id)?;
                if recipient == self_agent_id {
                    anyhow::bail!("cannot send a message to yourself");
                }
                self.pool
                    .deliver_mail(
                        self_agent_id,
                        recipient,
                        message,
                        MessageDelivery::NextRequest,
                    )
                    .await?;
                Ok(format!(
                    "Message sent to agent {}.",
                    self.display_agent_id(recipient)
                ))
            }
            McpAgentToolRequest::InterruptAgent { agent_id } => {
                let target = self.resolve_display_agent_id(&agent_id)?;
                if target == self_agent_id {
                    anyhow::bail!("cannot interrupt yourself");
                }
                let (_, agent, _) = self.pool.load(target).await?;
                agent.cancel();
                Ok(format!(
                    "Agent {} interrupted. It remains available for follow-up messages.",
                    self.display_agent_id(target)
                ))
            }
            McpAgentToolRequest::Wait { timeout_seconds } => {
                let timeout_seconds = timeout_seconds.unwrap_or(300).clamp(1, 3600);
                let (_, agent, _) = self.pool.load(self_agent_id).await?;
                if agent
                    .wait_for_input(std::time::Duration::from_secs(timeout_seconds))
                    .await
                {
                    Ok("Message(s) arrived for this agent.".to_owned())
                } else {
                    Ok("Timed out waiting for agent messages or user input.".to_owned())
                }
            }
        }
    }

    fn resolve_display_agent_id(&self, agent_id: &str) -> anyhow::Result<AgentId> {
        let raw_agent_id = agent_id
            .trim()
            .strip_prefix("ag-")
            .ok_or_else(|| anyhow::anyhow!("agent_id must start with ag-"))?;
        let resolved = match self.pool.resolve_agent_id(raw_agent_id)? {
            prefix_id::PrefixResolution::Unique(agent_id)
            | prefix_id::PrefixResolution::Ambiguous {
                first: agent_id, ..
            } => agent_id,
            prefix_id::PrefixResolution::NotFound => {
                anyhow::bail!("no agent with id {agent_id}")
            }
        };
        if !self.pool.agent_exists(resolved) {
            anyhow::bail!("no agent with id {agent_id}");
        }
        Ok(resolved)
    }

    fn display_agent_id(&self, agent_id: AgentId) -> String {
        format!("ag-{}", self.pool.agent_id_prefix(agent_id))
    }

    async fn move_agent(
        &self,
        agent_id: AgentId,
        target: rho_ui_proto::TopicTarget,
    ) -> anyhow::Result<()> {
        let mut write = self.db.write().await;
        let topic_id = match target {
            rho_ui_proto::TopicTarget::Existing(topic_id) => topic_id,
            rho_ui_proto::TopicTarget::Named(name) => self
                .db
                .read()
                .list_topics()
                .into_iter()
                .find(|(_, topic)| topic.name == name)
                .map(|(topic_id, _)| topic_id)
                .unwrap_or_else(|| {
                    write.create_topic(rho_core::UnixMs::now(), name, Status::Normal)
                }),
        };
        write.move_agent_to_topic(agent_id, topic_id);
        write.commit();
        Ok(())
    }

    async fn set_agent_status(&self, agent_id: AgentId, status: Status) -> anyhow::Result<()> {
        let mut write = self.db.write().await;
        write.set_agent_status(rho_core::UnixMs::now(), agent_id, status);
        write.commit();
        Ok(())
    }

    async fn set_agent_mode(&self, agent_id: AgentId, mode: AgentMode) -> anyhow::Result<()> {
        let record = self.db.read().get_agent(agent_id);
        match (record.runtime, mode) {
            (AgentRuntime::Rho { .. }, AgentMode::Deep(config)) => {
                if let Some(agent) = self.get(agent_id).await {
                    agent.set_deep_config(config)?;
                }
                let mut write = self.db.write().await;
                write.set_agent_mode(rho_core::UnixMs::now(), agent_id, mode);
                write.commit();
                Ok(())
            }
            (AgentRuntime::Rho { .. }, AgentMode::Fable { .. } | AgentMode::Opus { .. }) => {
                anyhow::bail!("cannot switch a Rho agent to a Claude runtime")
            }
            (AgentRuntime::Claude { .. }, AgentMode::Deep(_)) => {
                anyhow::bail!("cannot switch a Claude agent to a Deep runtime")
            }
            (AgentRuntime::Claude { .. }, AgentMode::Fable { .. } | AgentMode::Opus { .. }) => {
                if record.mode.claude_model() != mode.claude_model() {
                    anyhow::bail!("cannot switch a Claude agent model")
                }
                let effort = mode
                    .claude_effort()
                    .ok_or_else(|| anyhow::anyhow!("Claude mode missing effort"))?;
                if let Some(agent) = self.get(agent_id).await {
                    agent.set_claude_effort(effort).await?;
                }
                let mut write = self.db.write().await;
                write.set_agent_mode(rho_core::UnixMs::now(), agent_id, mode);
                write.commit();
                Ok(())
            }
        }
    }

    /// Titles an untitled agent from its first user message, in the
    /// background. Policy: only fills an empty `display_name` — a manual
    /// rename, before or during generation, always wins — and at most one
    /// generation runs per agent at a time. The requesting connection gets a
    /// `Ready` refresh when the title lands.
    async fn maybe_generate_title(
        self: &Arc<Self>,
        agent_id: AgentId,
        text: String,
        outgoing_tx: mpsc::UnboundedSender<ServerMessage>,
    ) {
        if text.trim().is_empty() || self.db.read().get_agent(agent_id).display_name.is_some() {
            return;
        }
        if !self.title_tasks.lock().await.insert(agent_id) {
            return;
        }
        let registry = Arc::clone(self);
        tokio::spawn(async move {
            let generate = rho_agent::title::generate_title(registry.auth.clone(), &text);
            match tokio::time::timeout(std::time::Duration::from_secs(60), generate).await {
                Ok(Ok(title)) => {
                    let mut write = registry.db.write().await;
                    // The write txn is the single writer, so this read can't
                    // race a rename committing between check and set.
                    if registry
                        .db
                        .read()
                        .get_agent(agent_id)
                        .display_name
                        .is_none()
                    {
                        write.set_agent_display_name(rho_core::UnixMs::now(), agent_id, title);
                        write.commit();
                        let _ = outgoing_tx.send(registry.ready_message().await);
                    }
                }
                Ok(Err(error)) => eprintln!("rho-daemon: title generation failed: {error:#}"),
                Err(_) => eprintln!("rho-daemon: title generation timed out"),
            }
            registry.title_tasks.lock().await.remove(&agent_id);
        });
    }

    async fn rename_agent(&self, agent_id: AgentId, name: String) -> anyhow::Result<()> {
        if name.trim().is_empty() {
            anyhow::bail!("agent name cannot be empty");
        }
        let mut write = self.db.write().await;
        write.set_agent_display_name(rho_core::UnixMs::now(), agent_id, name);
        write.commit();
        Ok(())
    }

    async fn rename_topic(&self, topic_id: TopicId, name: String) -> anyhow::Result<()> {
        if name.trim().is_empty() {
            anyhow::bail!("topic name cannot be empty");
        }
        let mut write = self.db.write().await;
        write.set_topic_name(rho_core::UnixMs::now(), topic_id, name);
        write.commit();
        Ok(())
    }

    async fn set_topic_status(&self, topic_id: TopicId, status: Status) -> anyhow::Result<()> {
        // New agents land in the default topic; archiving it would hide them
        // as they are created.
        if topic_id == self.default_topic_id && status == Status::Archived {
            anyhow::bail!("cannot archive the default topic");
        }
        let mut write = self.db.write().await;
        write.set_topic_status(rho_core::UnixMs::now(), topic_id, status);
        write.commit();
        Ok(())
    }

    async fn set_workdir(&self, path: Utf8PathBuf, name: Option<String>) -> anyhow::Result<()> {
        let path = validate_repo_root(path)?;
        let name = match name {
            Some(name) => name,
            None => path
                .file_name()
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("workdir path has no basename: {path}"))?,
        };
        let mut write = self.db.write().await;
        write.upsert_workdir(rho_core::UnixMs::now(), path.as_str(), name);
        write.commit();
        Ok(())
    }

    async fn remove_workdir(&self, path: Utf8PathBuf) -> anyhow::Result<()> {
        let mut write = self.db.write().await;
        write.remove_workdir(path.as_str());
        write.commit();
        Ok(())
    }

    async fn load(&self, agent_id: AgentId) -> anyhow::Result<(AgentId, RunningAgent, bool)> {
        self.pool.load(agent_id).await
    }
}

async fn serve_connection(
    agents: Arc<AgentRegistry>,
    connection: ServerConnection,
) -> anyhow::Result<()> {
    let counters = connection.io_counters();
    let land_holder = connection.peer_cred().ok().map(|cred| LandLeaseHolder {
        pid: cred.pid().and_then(|pid| u32::try_from(pid).ok()),
        uid: cred.uid(),
        gid: cred.gid(),
    });
    let stream = connection.into_stream();
    let (reader, writer) = stream.into_split();

    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let writer_counters = counters.clone();
    tokio::spawn(async move {
        let mut writer = writer;
        while let Some(message) = outgoing_rx.recv().await {
            if write_frame_counted(&mut writer, &message, Some(&writer_counters))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let _ = outgoing_tx.send(agents.ready_message().await);

    // Subscribe to creations before snapshotting the loaded set so no agent
    // slips between the two.
    let mut created_rx = agents.pool.subscribe_created();
    for (agent_id, agent) in agents.loaded().await {
        subscribe_agent(agent_id, agent, outgoing_tx.clone());
    }

    // Announce every agent created in the pool — by clients or by other
    // agents spawning children — so it shows up on this connection.
    {
        let agents = Arc::clone(&agents);
        let outgoing_tx = outgoing_tx.clone();
        tokio::spawn(async move {
            loop {
                match created_rx.recv().await {
                    Ok(created) => {
                        subscribe_agent(created.agent_id, created.agent, outgoing_tx.clone());
                        if outgoing_tx
                            .send(ServerMessage::AgentCreated {
                                topic_id: created.topic_id,
                                agent_id: created.agent_id,
                            })
                            .is_err()
                            || outgoing_tx.send(agents.ready_message().await).is_err()
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Missed creations still appear in the refreshed
                        // agent list.
                        if outgoing_tx.send(agents.ready_message().await).is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    let mut reader = reader;
    // Attention changes fan out to every client, not just the one whose
    // agent turned; aborted on disconnect so the writer channel can close.
    let mut events_rx = agents.events.subscribe();
    let events_tx = outgoing_tx.clone();
    let events_task = tokio::spawn(async move {
        loop {
            match events_rx.recv().await {
                Ok(message) => {
                    if events_tx.send(message).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let mut land_leases: Vec<(Utf8PathBuf, OwnedMutexGuard<()>)> = Vec::new();
    // The voice session (at most one) is owned by this connection: dropping
    // the handle on disconnect ends the session task.
    let mut voice: Option<voice::VoiceHandle> = None;
    let result = loop {
        let message =
            match read_frame_counted::<_, ClientMessage>(&mut reader, Some(&counters)).await {
                Ok(message) => message,
                Err(error) => {
                    for (repo, _) in &land_leases {
                        agents.clear_land_holder(repo).await;
                    }
                    break Err(error);
                }
            };
        match voice::handle_client_message(&agents, &outgoing_tx, &mut voice, &message) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => {
                let _ = outgoing_tx.send(ServerMessage::Error {
                    message: error.to_string(),
                });
                continue;
            }
        }
        match handle_message(
            &agents,
            &outgoing_tx,
            &mut land_leases,
            land_holder.clone(),
            message,
        )
        .await
        {
            Ok(Refresh::Ready) => {
                let _ = outgoing_tx.send(agents.ready_message().await);
            }
            Ok(Refresh::None) => {}
            Err(error) => {
                let _ = outgoing_tx.send(ServerMessage::Error {
                    message: error.to_string(),
                });
            }
        }
    };
    events_task.abort();
    result
}

/// Mid-turn from the rail's point of view: the states that render as a
/// running lamp rather than a settled one.
fn is_working(kind: &AgentStateKind) -> bool {
    matches!(
        kind,
        AgentStateKind::ApiStreaming { .. } | AgentStateKind::ToolCalling { .. }
    )
}

/// Attention for an agent that is not mid-turn, from its persisted record.
/// Sub-agent turn ends never reach the record (see the watcher), so children
/// stay quiet here by construction.
fn settled_attention(db: &RhoDb, agent_id: AgentId) -> UiAttention {
    match db.read().agent_attention(agent_id) {
        Some(AgentAttentionRecord {
            needs_input,
            disposition,
            ..
        }) => {
            let pending = match disposition {
                AgentDisposition::Pending => true,
                AgentDisposition::Done => false,
                // An expired snooze is pending again; the timer only exists
                // to broadcast that moment.
                AgentDisposition::Snoozed { until } => until <= rho_core::UnixMs::now(),
            };
            match (pending, needs_input) {
                (false, _) => UiAttention::Quiet,
                (true, true) => UiAttention::NeedsInput,
                (true, false) => UiAttention::Pending,
            }
        }
        None => UiAttention::Quiet,
    }
}

/// Watches one running agent for the daemon itself (not any particular
/// connection): records turn ends and broadcasts attention level changes to
/// every client. Spawned exactly once per loaded agent.
///
/// Sub-agents (a parent spawned them) get Working broadcasts but no turn-end
/// records: their finished turns are the parent's court, not the user's.
fn spawn_attention_watcher(
    db: RhoDb,
    events: broadcast::Sender<ServerMessage>,
    agent_id: AgentId,
    agent: RunningAgent,
) {
    tokio::spawn(async move {
        let is_child = db.read().get_agent(agent_id).parent_agent.is_some();
        let changes = agent.subscribe();
        futures::pin_mut!(changes);
        let mut was_working = is_working(&agent.state().kind);
        let mut last_sent = None;
        while let Some(state) = changes.next().await {
            let working = is_working(&state.kind);
            let attention = if working {
                UiAttention::Working
            } else {
                if was_working && !is_child {
                    let needs_input = matches!(
                        state.kind,
                        AgentStateKind::Error(_) | AgentStateKind::UnfinishedTurn { .. }
                    );
                    let mut write = db.write().await;
                    write.record_agent_turn_end(rho_core::UnixMs::now(), agent_id, needs_input);
                    write.commit();
                }
                settled_attention(&db, agent_id)
            };
            was_working = working;
            if last_sent != Some(attention) {
                let _ = events.send(ServerMessage::AgentAttention {
                    agent_id,
                    attention,
                });
                last_sent = Some(attention);
            }
        }
    });
}

/// Wakes a snoozed agent: at `until`, rebroadcasts its (by then pending)
/// level. Harmless if the disposition changed meanwhile — it just sends the
/// then-current level.
fn spawn_snooze_timer(
    db: RhoDb,
    events: broadcast::Sender<ServerMessage>,
    agent_id: AgentId,
    until: rho_core::UnixMs,
) {
    tokio::spawn(async move {
        let delay = until.saturating_duration_since(rho_core::UnixMs::now());
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        let _ = events.send(ServerMessage::AgentAttention {
            agent_id,
            attention: settled_attention(&db, agent_id),
        });
    });
}

/// Whether a handled message changed registry state that clients see through
/// `Ready` (topics, agents, workdirs).
enum Refresh {
    Ready,
    None,
}

/// One client request. `Err` becomes a [`ServerMessage::Error`]; extra replies
/// (creation events, pongs) are sent inline before the caller's `Ready`.
async fn handle_message(
    agents: &Arc<AgentRegistry>,
    outgoing_tx: &mpsc::UnboundedSender<ServerMessage>,
    land_leases: &mut Vec<(Utf8PathBuf, OwnedMutexGuard<()>)>,
    land_holder: Option<LandLeaseHolder>,
    message: ClientMessage,
) -> anyhow::Result<Refresh> {
    match message {
        ClientMessage::Ping => {
            let _ = outgoing_tx.send(ServerMessage::Pong);
            Ok(Refresh::None)
        }
        ClientMessage::Subscribe => Ok(Refresh::None),
        ClientMessage::NewTopic { name } => {
            let topic = agents.create_topic(name).await;
            let _ = outgoing_tx.send(ServerMessage::TopicCreated { topic });
            Ok(Refresh::Ready)
        }
        ClientMessage::NewAgent {
            topic_id,
            mode,
            start,
            content,
        } => {
            // Subscription and the AgentCreated announcement ride the pool's
            // creation broadcast (all connections, including this one).
            let (_topic_id, agent_id, agent) = agents.create(topic_id, mode, start).await?;
            if let Some(content) = content {
                let text = text_content(&content);
                // The agent is fresh, so the lanes are equivalent here.
                agent.send_user_message(text.clone(), MessageDelivery::NextRequest);
                agents
                    .maybe_generate_title(agent_id, text, outgoing_tx.clone())
                    .await;
            }
            Ok(Refresh::Ready)
        }
        ClientMessage::WorkdirSet { path, name } => {
            agents.set_workdir(path, name).await?;
            Ok(Refresh::Ready)
        }
        ClientMessage::WorkdirRemove { path } => {
            agents.remove_workdir(path).await?;
            Ok(Refresh::Ready)
        }
        ClientMessage::AcquireLandLease { repo } => {
            let lock = agents.land_lock(repo.clone()).await;
            let lease = match lock.clone().try_lock_owned() {
                Ok(lease) => lease,
                Err(_) => {
                    agents
                        .set_land_status(repo.clone(), LandStatus::Queued)
                        .await;
                    let holder = agents.land_holder(&repo).await;
                    let _ = outgoing_tx.send(ServerMessage::LandLeaseQueued {
                        repo: repo.clone(),
                        holder,
                    });
                    lock.lock_owned().await
                }
            };
            if let Some(holder) = land_holder {
                agents.set_land_holder(repo.clone(), holder).await;
            }
            land_leases.push((repo.clone(), lease));
            let _ = outgoing_tx.send(ServerMessage::LandLeaseGranted { repo });
            Ok(Refresh::None)
        }
        ClientMessage::LandStatus { repo, status } => {
            agents.set_land_status(repo.clone(), status.clone()).await;
            let _ = outgoing_tx.send(ServerMessage::LandStatus { repo, status });
            Ok(Refresh::None)
        }
        ClientMessage::ReleaseLandLease { repo } => {
            if let Some(index) = land_leases
                .iter()
                .position(|(leased_repo, _)| *leased_repo == repo)
            {
                land_leases.swap_remove(index);
                agents.clear_land_holder(&repo).await;
            }
            Ok(Refresh::None)
        }
        ClientMessage::LoadAgent { agent_id } => {
            let (agent_id, agent, loaded_now) = agents.load(agent_id).await?;
            if loaded_now {
                spawn_attention_watcher(
                    agents.db.clone(),
                    agents.events.clone(),
                    agent_id,
                    agent.clone(),
                );
                subscribe_agent(agent_id, agent, outgoing_tx.clone());
            }
            let _ = outgoing_tx.send(ServerMessage::AgentLoaded { agent_id });
            Ok(Refresh::None)
        }
        ClientMessage::SendUserMessage {
            agent_id,
            content,
            delivery,
        } => {
            let agent = agents
                .get(agent_id)
                .await
                .ok_or_else(|| anyhow::anyhow!("agent is not loaded: {agent_id:?}"))?;
            let text = text_content(&content);
            agent.send_user_message(text.clone(), delivery);
            agents
                .maybe_generate_title(agent_id, text, outgoing_tx.clone())
                .await;
            Ok(Refresh::None)
        }
        ClientMessage::CompactAgent { agent_id, delivery } => {
            let agent = agents
                .get(agent_id)
                .await
                .ok_or_else(|| anyhow::anyhow!("agent is not loaded: {agent_id:?}"))?;
            agent.compact(delivery)?;
            Ok(Refresh::None)
        }
        ClientMessage::MoveAgent { agent_id, topic } => {
            agents.move_agent(agent_id, topic).await?;
            Ok(Refresh::Ready)
        }
        ClientMessage::SetAgentStatus { agent_id, status } => {
            agents.set_agent_status(agent_id, status).await?;
            Ok(Refresh::Ready)
        }
        ClientMessage::SetAgentMode { agent_id, mode } => {
            agents.set_agent_mode(agent_id, mode).await?;
            Ok(Refresh::Ready)
        }
        ClientMessage::RenameAgent { agent_id, name } => {
            agents.rename_agent(agent_id, name).await?;
            Ok(Refresh::Ready)
        }
        ClientMessage::RenameTopic { topic_id, name } => {
            agents.rename_topic(topic_id, name).await?;
            Ok(Refresh::Ready)
        }
        ClientMessage::SetAgentDisposition {
            agent_id,
            disposition,
        } => {
            agents.set_disposition(agent_id, disposition).await;
            Ok(Refresh::None)
        }
        ClientMessage::SetTopicStatus { topic_id, status } => {
            agents.set_topic_status(topic_id, status).await?;
            Ok(Refresh::Ready)
        }
        ClientMessage::CancelTurn { agent_id } => {
            if let Some(agent) = agents.get(agent_id).await {
                agent.cancel();
                let _ = outgoing_tx.send(ServerMessage::TurnCancelled { agent_id });
            }
            Ok(Refresh::None)
        }
        ClientMessage::RewindAgent { agent_id, turns } => {
            let agent = agents
                .get(agent_id)
                .await
                .ok_or_else(|| anyhow::anyhow!("agent is not loaded: {agent_id:?}"))?;
            agent.rewind(turns).await?;
            Ok(Refresh::Ready)
        }
        ClientMessage::ContinueTurn { agent_id } => {
            if let Some(agent) = agents.get(agent_id).await {
                agent.continue_unfinished();
            }
            Ok(Refresh::None)
        }
        ClientMessage::McpAgentTool {
            request_id,
            self_agent_id,
            request,
        } => {
            let result = agents.mcp_agent_tool(self_agent_id, request).await;
            let response = match result {
                Ok(output) => McpAgentToolResponse {
                    request_id,
                    output,
                    is_error: false,
                },
                Err(error) => McpAgentToolResponse {
                    request_id,
                    output: error.to_string(),
                    is_error: true,
                },
            };
            let _ = outgoing_tx.send(ServerMessage::McpAgentToolResult(response));
            Ok(Refresh::None)
        }
        // Intercepted by `voice::handle_client_message` before this dispatch.
        ClientMessage::VoiceStart
        | ClientMessage::VoiceStop
        | ClientMessage::VoiceAudio { .. }
        | ClientMessage::VoiceFocus { .. } => Ok(Refresh::None),
    }
}

fn subscribe_agent(
    agent_id: AgentId,
    agent: RunningAgent,
    state_tx: mpsc::UnboundedSender<ServerMessage>,
) {
    tokio::spawn(async move {
        let changes = agent.subscribe();
        let mut encoder = AgentRemoteEncoder::new();
        let _ = state_tx.send(ServerMessage::Agent {
            agent_id,
            frame: encoder.encode(agent.state()),
        });
        futures::pin_mut!(changes);
        while let Some(state) = changes.next().await {
            if state_tx
                .send(ServerMessage::Agent {
                    agent_id,
                    frame: encoder.encode(state),
                })
                .is_err()
            {
                break;
            }
        }
    });
}

/// Repo roots must be absolute (the daemon's cwd is meaningless by design)
/// jj repo roots: agents work in daemon-created jj workspaces, so both
/// workdir registration and agent creation take repos. A leading `~` expands
/// to the daemon's home: clients may run on another machine, so path
/// interpretation belongs here.
fn validate_repo_root(path: Utf8PathBuf) -> anyhow::Result<Utf8PathBuf> {
    let path = expand_home(&path).unwrap_or(path);
    rho_workspaces::resolve_repo_root(path.as_std_path())
}

fn expand_home(path: &Utf8Path) -> Option<Utf8PathBuf> {
    let rest = path.strip_prefix("~").ok()?;
    let home = Utf8PathBuf::try_from(dirs::home_dir()?).ok()?;
    Some(home.join(rest))
}
