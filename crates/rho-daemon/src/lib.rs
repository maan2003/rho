use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use futures::StreamExt as _;
use futures::stream::BoxStream;
use rho_agent::claude::ClaudeAgent;
use rho_agent::db::{
    AgentId, AgentMode, AgentReadTxnExt as _, AgentRuntime, AgentWriteTxnExt as _, Status, TopicId,
};
use rho_agent::{Agent, AgentState, MessageDelivery};
use rho_core::text_content;
use rho_db::RhoDb;
use rho_inference::InferenceAuth;
use rho_ui_proto::remote::AgentRemoteEncoder;
use rho_ui_proto::server::{Server, ServerConnection};
use rho_ui_proto::{
    ClientMessage, JoinTarget, LandLeaseHolder, LandStatus, ServerMessage, StartMode,
    UiAgentSummary, UiTopic, UiWorkdir, read_frame_counted, write_frame_counted,
};
use rho_workspaces::{Repo, WorkspaceInfo};
use tokio::sync::{Mutex, Mutex as TokioMutex, Notify, OwnedMutexGuard, mpsc};

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
pub use rho_workspaces::init_daemon_namespace;

#[derive(Clone, Debug, clap::Args)]
pub struct DaemonArgs {
    #[arg(long = "auth", default_value = "default")]
    pub auth: String,
    #[arg(long = "socket-path")]
    pub socket_path: Option<PathBuf>,
    /// Exit once the last UI client disconnects.
    #[arg(long = "die-on-detached")]
    pub die_on_detached: bool,
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
    let agents = Arc::new(AgentRegistry::new(db, auth).await);

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
    db: RhoDb,
    auth: InferenceAuth,
    /// The daemon-created topic agents are born into; announced in `Ready`
    /// so clients never guess it from topic ordering.
    default_topic_id: TopicId,
    /// The database's machine seed, announced in `Ready` so clients can
    /// encode agent IDs.
    machine_seed: u64,
    agents: Mutex<HashMap<AgentId, RunningAgent>>,
    /// One shared handle per repo root: live-workspace sharing (joined
    /// agents get one checkout + namespace) only holds within one instance.
    repos: Mutex<HashMap<Utf8PathBuf, Arc<Repo>>>,
    /// Agents with a title generation in flight, so a burst of messages to an
    /// untitled agent starts at most one task.
    title_tasks: Mutex<HashSet<AgentId>>,
    land_locks: Mutex<HashMap<Utf8PathBuf, Arc<TokioMutex<()>>>>,
    land_holders: Mutex<HashMap<Utf8PathBuf, LandLeaseHolder>>,
    land_statuses: Mutex<HashMap<Utf8PathBuf, LandStatus>>,
    /// At most one realtime voice session per daemon (see [`voice`]).
    voice_active: Arc<std::sync::atomic::AtomicBool>,
}

impl AgentRegistry {
    async fn new(db: RhoDb, auth: InferenceAuth) -> Self {
        let mut write = db.write().await;
        write.init_agent_tables();
        write.commit();
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
            db,
            auth,
            default_topic_id,
            machine_seed,
            agents: Mutex::new(HashMap::new()),
            repos: Mutex::new(HashMap::new()),
            title_tasks: Mutex::new(HashSet::new()),
            land_locks: Mutex::new(HashMap::new()),
            land_holders: Mutex::new(HashMap::new()),
            land_statuses: Mutex::new(HashMap::new()),
            voice_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        registry.load_non_archived_agents().await;
        registry
    }

    fn topics(&self) -> Vec<UiTopic> {
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

    fn ready_message(&self) -> ServerMessage {
        ServerMessage::Ready {
            topics: self.topics(),
            workdirs: self.workdirs(),
            default_topic_id: self.default_topic_id,
            machine_seed: self.machine_seed,
            agent_counter: self.db.read().last_agent_counter(),
            workspace_counter: self.db.read().last_workspace_counter(),
        }
    }

    async fn loaded(&self) -> Vec<(AgentId, RunningAgent)> {
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

    async fn load_non_archived_agents(&self) {
        let agent_ids = self.non_archived_agent_ids();
        for agent_id in agent_ids {
            if let Err(error) = self.load(agent_id).await {
                eprintln!("rho-daemon: failed to load active agent {agent_id:?}: {error:#}");
            }
        }
    }

    fn non_archived_agent_ids(&self) -> Vec<AgentId> {
        let read = self.db.read();
        read.list_topics()
            .into_iter()
            .filter(|(_, topic)| topic.status != Status::Archived)
            .flat_map(|(topic_id, _)| read.list_topic_agents(topic_id))
            .filter(|agent_id| read.get_agent(*agent_id).status != Status::Archived)
            .collect()
    }

    async fn get(&self, agent_id: AgentId) -> Option<RunningAgent> {
        self.agents.lock().await.get(&agent_id).cloned()
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
                    repo: self.repo(&repo).await?,
                    parent_revset: revset,
                }
            }
            StartMode::Join(JoinTarget::Workspace(info)) => {
                rho_agent::StartWorkspace::Existing(self.open_workspace(&info).await?)
            }
            StartMode::Join(JoinTarget::User { repo }) => {
                let repo = validate_repo_root(repo)?;
                rho_agent::StartWorkspace::Existing(self.repo(&repo).await?.user_checkout().await?)
            }
        };
        let (agent_id, agent) = match mode {
            AgentMode::Deep(_) => {
                let (agent_id, agent) = Agent::create(
                    self.db.clone(),
                    self.auth.clone(),
                    mode,
                    topic_id,
                    None,
                    start,
                )
                .await?;
                (agent_id, RunningAgent::Rho(agent))
            }
            AgentMode::Fable { .. } | AgentMode::Opus { .. } => {
                let (agent_id, agent) =
                    ClaudeAgent::create(self.db.clone(), topic_id, None, start, mode).await?;
                (agent_id, RunningAgent::Claude(agent))
            }
        };
        self.agents.lock().await.insert(agent_id, agent.clone());
        Ok((topic_id, agent_id, agent))
    }

    /// The shared handle for the repo rooted at (or containing) `path`.
    async fn repo(&self, path: &Utf8Path) -> anyhow::Result<Arc<Repo>> {
        let repo = Repo::open(path.as_std_path())?;
        let mut repos = self.repos.lock().await;
        Ok(match repos.entry(repo.root().to_owned()) {
            std::collections::hash_map::Entry::Occupied(entry) => Arc::clone(entry.get()),
            std::collections::hash_map::Entry::Vacant(entry) => {
                Arc::clone(entry.insert(Arc::new(repo)))
            }
        })
    }

    async fn open_workspace(
        &self,
        info: &WorkspaceInfo,
    ) -> anyhow::Result<Arc<rho_workspaces::Workspace>> {
        let repo = self.repo(info.repo()).await?;
        match info {
            WorkspaceInfo::UserCheckout { .. } => repo.user_checkout().await,
            WorkspaceInfo::Workspace { id, .. } => repo.open_workspace(*id).await,
        }
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
                        let _ = outgoing_tx.send(registry.ready_message());
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
            )),
            AgentRuntime::Claude { .. } => {
                RunningAgent::Claude(ClaudeAgent::load(self.db.clone(), agent_id, workspace).await?)
            }
        };
        self.agents.lock().await.insert(agent_id, agent.clone());
        Ok((agent_id, agent, true))
    }
}

#[derive(Clone)]
enum RunningAgent {
    Rho(Agent),
    Claude(ClaudeAgent),
}

impl RunningAgent {
    fn state(&self) -> AgentState {
        match self {
            Self::Rho(agent) => agent.state(),
            Self::Claude(agent) => agent.state(),
        }
    }

    fn send_user_message(&self, text: String, delivery: MessageDelivery) {
        match self {
            Self::Rho(agent) => agent.send_user_message(text, delivery),
            // The Claude CLI does its own mid-turn steering; there is no
            // lane choice to forward.
            Self::Claude(agent) => agent.send_user_message(text),
        }
    }

    fn compact(&self, delivery: MessageDelivery) -> anyhow::Result<()> {
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

    fn cancel(&self) {
        match self {
            Self::Rho(agent) => agent.cancel(),
            Self::Claude(agent) => agent.cancel(),
        }
    }

    fn continue_unfinished(&self) {
        match self {
            Self::Rho(agent) => agent.continue_unfinished(),
            Self::Claude(_) => {}
        }
    }

    fn set_deep_config(&self, config: rho_agent::db::DeepConfig) -> anyhow::Result<()> {
        match self {
            Self::Rho(agent) => {
                agent.set_deep_config(config);
                Ok(())
            }
            Self::Claude(_) => anyhow::bail!("cannot apply deep config to Claude agent"),
        }
    }

    async fn set_claude_effort(&self, effort: rho_claude::Effort) -> anyhow::Result<()> {
        match self {
            Self::Claude(agent) => agent.set_effort(effort).await,
            Self::Rho(_) => anyhow::bail!("cannot apply Claude effort to Rho agent"),
        }
    }

    async fn rewind(&self, turns: u32) -> anyhow::Result<()> {
        match self {
            Self::Rho(agent) => agent.rewind(turns).await,
            Self::Claude(_) => anyhow::bail!("rewind is only available for Rho agents"),
        }
    }

    fn subscribe(&self) -> BoxStream<'static, AgentState> {
        match self {
            Self::Rho(agent) => agent.subscribe().boxed(),
            Self::Claude(agent) => agent.subscribe().boxed(),
        }
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

    let _ = outgoing_tx.send(agents.ready_message());

    for (agent_id, agent) in agents.loaded().await {
        subscribe_agent(agent_id, agent, outgoing_tx.clone());
    }

    let mut reader = reader;
    let mut land_leases: Vec<(Utf8PathBuf, OwnedMutexGuard<()>)> = Vec::new();
    // The voice session (at most one) is owned by this connection: dropping
    // the handle on disconnect ends the session task.
    let mut voice: Option<voice::VoiceHandle> = None;
    loop {
        let message =
            match read_frame_counted::<_, ClientMessage>(&mut reader, Some(&counters)).await {
                Ok(message) => message,
                Err(error) => {
                    for (repo, _) in &land_leases {
                        agents.clear_land_holder(repo).await;
                    }
                    return Err(error);
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
                let _ = outgoing_tx.send(agents.ready_message());
            }
            Ok(Refresh::None) => {}
            Err(error) => {
                let _ = outgoing_tx.send(ServerMessage::Error {
                    message: error.to_string(),
                });
            }
        }
    }
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
            let (topic_id, agent_id, agent) = agents.create(topic_id, mode, start).await?;
            subscribe_agent(agent_id, agent.clone(), outgoing_tx.clone());
            let _ = outgoing_tx.send(ServerMessage::AgentCreated { topic_id, agent_id });
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
