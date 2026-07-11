use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use futures::StreamExt as _;
use rho_agent::db::{
    AgentDisposition, AgentId, AgentReadTxnExt as _, AgentRole, AgentWriteTxnExt as _, Status,
    TopicId,
};
use rho_agent::pool::{AgentPool, AgentTurnCompleted, RunningAgent, SpawnWorkspace};
use rho_agent::{AgentState, AgentStateKind, MessageDelivery};
use rho_core::{ContextBlock, text_content};
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
mod webui;

/// FDNAME under which messaging-platform secrets live in the systemd fd store.
const PLATFORM_SECRETS_FD_STORE_NAME: &str = "platform-secrets";

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

#[derive(Clone, Default)]
struct PlatformSecrets {
    store: Arc<std::sync::Mutex<Option<Arc<rho_slack::SecretStore>>>>,
}

impl PlatformSecrets {
    fn from_fd_store() -> Self {
        let secrets = Self::default();
        match rho_slack::SecretStore::take_from_listen_fds(PLATFORM_SECRETS_FD_STORE_NAME) {
            Ok(Some(store)) => {
                tracing::info!("reclaimed platform secrets from fd store");
                *secrets.store.lock().expect("platform secrets lock") = Some(Arc::new(store));
            }
            Ok(None) => {}
            Err(error) => tracing::error!(%error, "reclaiming platform secrets fd"),
        }
        secrets
    }

    fn current_store(&self) -> Option<Arc<rho_slack::SecretStore>> {
        self.store.lock().expect("platform secrets lock").clone()
    }

    fn read(&self) -> anyhow::Result<BTreeMap<String, String>> {
        let store = self
            .current_store()
            .ok_or_else(|| anyhow::anyhow!("no platform secrets installed"))?;
        store.read().context("reading platform secrets")
    }

    fn get(&self, key: &str) -> anyhow::Result<String> {
        self.read()?
            .remove(key)
            .with_context(|| format!("{key} not among installed platform secrets"))
    }

    fn install_merge(
        &self,
        secrets: impl IntoIterator<Item = (String, String)>,
    ) -> anyhow::Result<(Arc<rho_slack::SecretStore>, bool)> {
        let mut merged = self.read().unwrap_or_default();
        for (key, value) in secrets {
            merged.insert(key, value);
        }
        let store =
            Arc::new(rho_slack::SecretStore::create(&merged).context("sealing platform secrets")?);
        let stashed = store
            .stash_in_fd_store(PLATFORM_SECRETS_FD_STORE_NAME)
            .context("stashing platform secrets in the systemd fd store")?;
        *self.store.lock().expect("platform secrets lock") = Some(store.clone());
        Ok((store, stashed))
    }
}

fn spawn_octo_server(
    socket_path: &std::path::Path,
    secrets: PlatformSecrets,
) -> anyhow::Result<()> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).context("create octo socket directory")?;
    }
    let _ = std::fs::remove_file(socket_path);
    let listener = tokio::net::UnixListener::bind(socket_path)
        .with_context(|| format!("bind octo socket {}", socket_path.display()))?;
    let github_api_url = url::Url::parse("https://api.github.com")?;
    let token_provider: octo::TokenProvider =
        Arc::new(move || secrets.get("GITHUB_TOKEN").context("reading GITHUB_TOKEN"));
    tokio::spawn(async move {
        if let Err(error) = octo::serve(listener, token_provider, github_api_url).await {
            tracing::error!(%error, "octo server stopped");
        }
    });
    Ok(())
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
    /// Also listen for UI clients (including the web UI) over iroh
    /// (relay-backed). Remote clients must be enrolled once via
    /// `rho iroh approve <code>` on this machine.
    #[arg(long = "iroh")]
    pub iroh: bool,
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
    let platform_secrets = PlatformSecrets::from_fd_store();
    let octo_socket_path = socket_path.with_file_name("octo.sock");
    spawn_octo_server(&octo_socket_path, platform_secrets.clone())?;
    // Safe before daemon worker tasks are spawned; child commands inherit this
    // non-secret socket path unless a caller explicitly overrides it.
    unsafe { std::env::set_var("OCTO_SOCKET", &octo_socket_path) };

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
    let iroh = if args.iroh {
        let state_dir = default_db_path()?
            .parent()
            .map(std::path::Path::to_path_buf)
            .context("state directory for iroh secret")?;
        std::fs::create_dir_all(&state_dir).context("create state directory")?;
        let secret = load_or_create_iroh_secret(&state_dir.join("iroh-secret.key"))?;
        let iroh_auth = rho_iroh_auth::IrohAuth::new(db.clone(), secret.public());
        Some((secret, iroh_auth))
    } else {
        None
    };

    let iroh_auth = iroh.as_ref().map(|(_, auth)| auth.clone());
    let agents =
        Arc::new(AgentRegistry::new(db, auth, path_overrides, platform_secrets, iroh_auth).await);
    agents.resume_platform_integrations();

    if let Some((secret, iroh_auth)) = iroh {
        let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
            .secret_key(secret)
            .alpns(vec![
                rho_ui_proto::IROH_ALPN.to_vec(),
                rho_webui_messages::ALPN.to_vec(),
            ])
            .hooks(iroh_auth)
            .bind()
            .await
            .context("bind iroh endpoint")?;
        eprintln!("rho daemon iroh endpoint: {}", endpoint.id());
        tokio::spawn(run_iroh_listener(agents.clone(), endpoint));
    }

    // Attention watchers: one per loaded agent, daemon-owned (not tied to
    // any connection). Preloaded agents are covered here; later creations
    // ride the pool's `created` broadcast, and late loads the LoadAgent
    // handler.
    for (agent_id, agent) in agents.loaded().await {
        spawn_attention_watcher(
            agents.pool.clone(),
            agents.db.clone(),
            agents.events.clone(),
            agent_id,
            agent,
        );
    }
    {
        let mut created_rx = agents.pool.subscribe_created();
        let pool = agents.pool.clone();
        let db = agents.db.clone();
        let events = agents.events.clone();
        tokio::spawn(async move {
            loop {
                match created_rx.recv().await {
                    Ok(created) => spawn_attention_watcher(
                        pool.clone(),
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
    for (agent_id, agent) in agents.db.read().list_agents() {
        if let AgentDisposition::Snoozed { until } = agent.disposition
            && until > rho_core::UnixMs::now()
        {
            spawn_snooze_timer(
                agents.db.clone(),
                agents.pool.clone(),
                agents.events.clone(),
                agent_id,
                until,
            );
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

/// The daemon's iroh identity, raw 32 secret bytes owner-readable only.
fn load_or_create_iroh_secret(path: &std::path::Path) -> anyhow::Result<iroh::SecretKey> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let bytes: [u8; 32] = bytes
                .as_slice()
                .try_into()
                .map_err(|_| anyhow::anyhow!("iroh secret file {path:?} is not 32 bytes"))?;
            Ok(iroh::SecretKey::from_bytes(&bytes))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            let secret = iroh::SecretKey::generate();
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .context("create iroh secret file")?;
            file.write_all(&secret.to_bytes())
                .context("write iroh secret file")?;
            Ok(secret)
        }
        Err(error) => Err(error).context("read iroh secret file"),
    }
}

/// Accepts enrolled iroh connections ([`rho_iroh_auth::IrohAuth`] gates them
/// before they reach here) and serves one session per bi-stream: the full UI
/// protocol on [`rho_ui_proto::IROH_ALPN`], the web UI JSON protocol on
/// [`rho_webui_messages::ALPN`].
async fn run_iroh_listener(agents: Arc<AgentRegistry>, endpoint: iroh::Endpoint) {
    while let Some(incoming) = endpoint.accept().await {
        let agents = agents.clone();
        tokio::spawn(async move {
            let connection = match incoming.await {
                Ok(connection) => connection,
                Err(error) => {
                    eprintln!("rho daemon iroh accept error: {error:#}");
                    return;
                }
            };
            let webui = connection.alpn() == rho_webui_messages::ALPN;
            while let Ok((send, recv)) = connection.accept_bi().await {
                let agents = agents.clone();
                tokio::spawn(async move {
                    let result = if webui {
                        webui::serve_json_session(agents, recv, send).await
                    } else {
                        let counters = rho_ui_proto::IoCounters::default();
                        serve_connection_io(agents, recv, send, counters, None).await
                    };
                    if let Err(error) = result {
                        eprintln!("rho daemon iroh connection error: {error:#}");
                    }
                });
            }
        });
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
    land_statuses: Mutex<HashMap<Utf8PathBuf, (Option<AgentId>, LandStatus)>>,
    /// At most one realtime voice session per daemon (see [`voice`]).
    voice_active: Arc<std::sync::atomic::AtomicBool>,
    /// In-process Slack connection and its thread sessions
    /// (see [`rho_slack::SlackManager`]).
    slack: Arc<rho_slack::SlackManager>,
    /// Shared sealed platform secret store used by Slack and Octo.
    platform_secrets: PlatformSecrets,
    /// Daemon-wide fanout for messages every client must hear regardless of
    /// which connection caused them (attention changes); each connection
    /// forwards this onto its own outgoing channel.
    events: broadcast::Sender<ServerMessage>,
    /// Enrollment/trust for iroh clients; `None` unless `--iroh` is set.
    iroh_auth: Option<rho_iroh_auth::IrohAuth>,
}

impl AgentRegistry {
    async fn new(
        db: RhoDb,
        auth: InferenceAuth,
        path_overrides: PathOverrides,
        platform_secrets: PlatformSecrets,
        iroh_auth: Option<rho_iroh_auth::IrohAuth>,
    ) -> Self {
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
        let slack = rho_slack::SlackManager::new(pool.clone(), db.clone()).await;
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
            slack,
            platform_secrets,
            events: broadcast::channel(1024).0,
            iroh_auth,
        };
        registry.pool.load_non_hidden_agents().await;
        registry
    }

    fn resume_platform_integrations(self: &Arc<Self>) {
        let Some(store) = self.platform_secrets.current_store() else {
            return;
        };
        if store
            .read()
            .map(|secrets| {
                secrets.contains_key("SLACK_BOT_TOKEN") && secrets.contains_key("SLACK_APP_TOKEN")
            })
            .unwrap_or(false)
            && let Err(error) = self.slack.start_from_store(store)
        {
            tracing::error!(%error, "resuming slack from platform secrets");
        }
    }

    /// Live state kinds of every loaded agent, for attention derivation.
    /// Blocked/working are read off the running agent, never persisted; only
    /// the disposition (the user's verdict) lives in the database.
    async fn agent_state_kinds(&self) -> HashMap<AgentId, AgentStateKind> {
        self.pool
            .loaded()
            .await
            .into_iter()
            .map(|(agent_id, agent)| (agent_id, agent.state().kind))
            .collect()
    }

    /// Applies the user's verdict and tells every client the new level; for
    /// snoozes, arms the wake-up timer.
    async fn set_disposition(&self, agent_id: AgentId, disposition: AgentDisposition) {
        let mut write = self.db.write().await;
        write.set_agent_disposition(agent_id, disposition);
        write.commit();
        if let AgentDisposition::Snoozed { until } = disposition {
            spawn_snooze_timer(
                self.db.clone(),
                self.pool.clone(),
                self.events.clone(),
                agent_id,
                until,
            );
        }
        let kind = self.get(agent_id).await.map(|agent| agent.state().kind);
        let _ = self.events.send(ServerMessage::AgentAttention {
            agent_id,
            attention: attention_level(kind.as_ref(), disposition),
        });
    }

    fn topics(&self, kinds: &HashMap<AgentId, AgentStateKind>) -> Vec<UiTopic> {
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
                            parent_agent: agent.parent_agent,
                            role: agent.config(),
                            display_name: agent.display_name,
                            created_at: agent.created_at,
                            updated_at: agent.updated_at,
                            workspace: agent.workspace,
                            status: agent.status,
                            attention: attention_level(kinds.get(&agent_id), agent.disposition),
                            last_active: agent.last_user_message.max(agent.created_at),
                            hidden: agent.disposition == AgentDisposition::Hidden,
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
            topics: self.topics(&self.agent_state_kinds().await),
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

    async fn set_land_status(
        &self,
        repo: Utf8PathBuf,
        agent_id: Option<AgentId>,
        status: LandStatus,
    ) {
        self.land_statuses
            .lock()
            .await
            .insert(repo, (agent_id, status));
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
        role: AgentRole,
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
        let (agent_id, agent) = self.pool.create(topic_id, role, None, start).await?;
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
        if matches!(
            self.db.read().get_agent(self_agent_id).role,
            AgentRole::Oracle { .. }
        ) {
            anyhow::bail!("Oracle agents do not have multi-agent tools");
        }
        match request {
            McpAgentToolRequest::SpawnAgent {
                task_name,
                prompt,
                workspace,
                repo,
                role,
            } => {
                if prompt.trim().is_empty() {
                    anyhow::bail!("prompt must not be empty");
                }
                let workspace = match (workspace, repo) {
                    (McpSpawnWorkspace::Join, None) => SpawnWorkspace::Join,
                    (McpSpawnWorkspace::Fork, None) => SpawnWorkspace::Fork,
                    (McpSpawnWorkspace::New { revset }, repo) => {
                        SpawnWorkspace::New { revset, repo }
                    }
                    (_, Some(_)) => anyhow::bail!("repo is only supported with workspace=new"),
                };
                let child_id = self
                    .pool
                    .spawn_child(
                        self_agent_id,
                        task_name.clone(),
                        prompt,
                        workspace,
                        rho_agent::multi_agent_tools::parse_spawn_role(&role)?,
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
    serve_connection_io(agents, reader, writer, counters, land_holder).await
}

/// One UI protocol session over any framed byte stream (Unix socket or an
/// iroh bi-stream from an enrolled remote client).
async fn serve_connection_io<R, W>(
    agents: Arc<AgentRegistry>,
    reader: R,
    writer: W,
    counters: rho_ui_proto::IoCounters,
    land_holder: Option<LandLeaseHolder>,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
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
    // Daemon-wide events fan out to every client, not just the connection
    // whose action produced them; aborted on disconnect so the writer channel
    // can close.
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

/// Stuck rather than finished: the agent cannot proceed without the user.
fn is_blocked(kind: &AgentStateKind) -> bool {
    matches!(
        kind,
        AgentStateKind::Error(_) | AgentStateKind::UnfinishedTurn { .. }
    )
}

/// Attention = f(live state, disposition). The live half (working, blocked)
/// is read off the running agent — `None` for unloaded agents, which render
/// as idle. The persisted half is the user's verdict on the last turn end;
/// sub-agent turn ends never set it to Pending (see the watcher), so
/// children stay quiet by construction.
fn attention_level(kind: Option<&AgentStateKind>, disposition: AgentDisposition) -> UiAttention {
    if kind.is_some_and(is_working) {
        return UiAttention::Working;
    }
    let pending = match disposition {
        AgentDisposition::Pending => true,
        AgentDisposition::Done | AgentDisposition::Hidden => false,
        // An expired snooze is pending again; the timer only exists to
        // broadcast that moment.
        AgentDisposition::Snoozed { until } => until <= rho_core::UnixMs::now(),
    };
    match (pending, kind.is_some_and(is_blocked)) {
        (false, _) => UiAttention::Quiet,
        (true, true) => UiAttention::NeedsInput,
        (true, false) => UiAttention::Pending,
    }
}

/// Watches one running agent for the daemon itself (not any particular
/// connection): records turn ends and broadcasts attention level changes to
/// every client. Spawned exactly once per loaded agent.
///
/// Sub-agents (a parent spawned them) get Working broadcasts but no turn-end
/// records: their finished turns are the parent's court, not the user's.
fn spawn_attention_watcher(
    pool: Arc<AgentPool>,
    db: RhoDb,
    events: broadcast::Sender<ServerMessage>,
    agent_id: AgentId,
    agent: RunningAgent,
) {
    tokio::spawn(async move {
        let is_child = db.read().get_agent(agent_id).parent_agent.is_some();
        let changes = agent.subscribe();
        futures::pin_mut!(changes);
        let initial_state = agent.state();
        let mut was_working = is_working(&initial_state.kind);
        let mut last_reported_response_count = inference_response_count(&initial_state);
        let mut last_sent = None;
        while let Some(state) = changes.next().await {
            let working = is_working(&state.kind);
            if !working && was_working && !is_child {
                let mut write = db.write().await;
                write.record_agent_turn_end(agent_id);
                write.commit();
            }
            if !working
                && was_working
                && let Some((response_count, final_answer)) = latest_final_response(&state)
                && response_count > last_reported_response_count
            {
                last_reported_response_count = response_count;
                pool.publish_completed_turn(AgentTurnCompleted {
                    agent_id,
                    final_answer,
                });
            }
            was_working = working;
            let disposition = db.read().get_agent(agent_id).disposition;
            let attention = attention_level(Some(&state.kind), disposition);
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

fn inference_response_count(state: &AgentState) -> usize {
    state
        .blocks
        .iter()
        .filter(|block| matches!(block.as_ref(), ContextBlock::InferenceResponse { .. }))
        .count()
}

fn latest_final_response(state: &AgentState) -> Option<(usize, String)> {
    let response_count = inference_response_count(state);
    if response_count == 0 {
        return None;
    }
    state.blocks.iter().rev().find_map(|block| {
        if let ContextBlock::InferenceResponse { items, .. } = block.as_ref() {
            Some((response_count, rho_agent::final_answer_text(items)))
        } else {
            None
        }
    })
}

/// Wakes a snoozed agent: at `until`, rebroadcasts its (by then pending)
/// level. Harmless if the disposition changed meanwhile — it just sends the
/// then-current level.
fn spawn_snooze_timer(
    db: RhoDb,
    pool: Arc<AgentPool>,
    events: broadcast::Sender<ServerMessage>,
    agent_id: AgentId,
    until: rho_core::UnixMs,
) {
    tokio::spawn(async move {
        let delay = until.saturating_duration_since(rho_core::UnixMs::now());
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        let kind = pool.get(agent_id).await.map(|agent| agent.state().kind);
        let disposition = db.read().get_agent(agent_id).disposition;
        let _ = events.send(ServerMessage::AgentAttention {
            agent_id,
            attention: attention_level(kind.as_ref(), disposition),
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
        ClientMessage::PlatformSecretsSet {
            secrets,
            coordinator_repo,
        } => {
            let wants_slack = secrets
                .iter()
                .any(|(key, _)| key == "SLACK_BOT_TOKEN" || key == "SLACK_APP_TOKEN");
            let wants_octo = secrets.iter().any(|(key, _)| key == "GITHUB_TOKEN");
            let (running, detail) = match agents.platform_secrets.install_merge(secrets) {
                Ok((store, stashed)) => {
                    let persistence = if stashed {
                        " and stashed in the systemd fd store"
                    } else {
                        " (no systemd notify socket: they will not survive a daemon restart)"
                    };
                    if wants_slack {
                        match coordinator_repo
                            .ok_or_else(|| anyhow::anyhow!("Slack coordinator repo is required"))
                            .and_then(validate_repo_root)
                        {
                            Ok(coordinator_repo) => match agents
                                .slack
                                .configure_and_start_from_store(store.clone(), coordinator_repo)
                                .await
                            {
                                Ok(()) => (true, format!("slack secrets installed{persistence}")),
                                Err(error) => (false, format!("{error:#}")),
                            },
                            Err(error) => (false, format!("{error:#}")),
                        }
                    } else if wants_octo && store.read()?.contains_key("GITHUB_TOKEN") {
                        (true, format!("octo secrets installed{persistence}"))
                    } else {
                        (true, format!("platform secrets installed{persistence}"))
                    }
                }
                Err(error) => (false, format!("{error:#}")),
            };
            let _ = outgoing_tx.send(ServerMessage::PlatformStatus { running, detail });
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
            role,
            start,
            content,
        } => {
            // Subscription and the AgentCreated announcement ride the pool's
            // creation broadcast (all connections, including this one).
            let (_topic_id, agent_id, agent) = agents.create(topic_id, role, start).await?;
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
        ClientMessage::AcquireLandLease { repo, agent_id } => {
            let lock = agents.land_lock(repo.clone()).await;
            let lease = match lock.clone().try_lock_owned() {
                Ok(lease) => lease,
                Err(_) => {
                    agents
                        .set_land_status(repo.clone(), agent_id, LandStatus::Queued)
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
        ClientMessage::LandStatus {
            repo,
            agent_id,
            status,
        } => {
            agents
                .set_land_status(repo.clone(), agent_id, status.clone())
                .await;
            let _ = agents.events.send(ServerMessage::LandStatus {
                repo,
                agent_id,
                status,
            });
            Ok(Refresh::None)
        }
        ClientMessage::ReleaseLandLease { repo, agent_id: _ } => {
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
                    agents.pool.clone(),
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
            {
                let mut write = agents.db.write().await;
                write.record_agent_user_message(rho_core::UnixMs::now(), agent_id);
                write.commit();
            }
            // Replying cleared the disposition; say so even when the turn
            // doesn't start immediately (queued delivery), or the pending
            // lamp would linger until the watcher's next state change.
            let _ = agents.events.send(ServerMessage::AgentAttention {
                agent_id,
                attention: attention_level(Some(&agent.state().kind), AgentDisposition::Done),
            });
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
            // Hidden changes what the rail folds, which clients read off
            // summaries; attention alone travels on its own broadcast.
            if disposition == AgentDisposition::Hidden {
                Ok(Refresh::Ready)
            } else {
                Ok(Refresh::None)
            }
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
        ClientMessage::IrohApprove { code } => {
            let auth = agents
                .iroh_auth
                .as_ref()
                .context("daemon is not listening over iroh (start it with --iroh)")?;
            let code = code
                .parse::<rho_iroh_auth::EnrollmentCode>()
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            let endpoint_id = auth
                .approve_code(&code)
                .await
                .map_err(|_| anyhow::anyhow!("no pending enrollment has this code"))?;
            let _ = outgoing_tx.send(ServerMessage::IrohApproved {
                endpoint_id: endpoint_id.to_string(),
            });
            Ok(Refresh::None)
        }
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rho_agent::{AgentState, AgentStateKind, InputQueues};
    use rho_core::{
        ContentPart, ContextBlock, InferenceResponseItem, MessagePhase, ToolSpec,
        UnknownProviderSpecificData,
    };

    use super::{inference_response_count, latest_final_response};

    fn state_with_responses(texts: &[&str]) -> AgentState {
        AgentState {
            blocks: texts
                .iter()
                .map(|text| {
                    Arc::new(ContextBlock::InferenceResponse {
                        items: vec![InferenceResponseItem::AssistantMessage {
                            provider_specific: Box::new(UnknownProviderSpecificData {
                                tag: "test".to_owned(),
                            }),
                            content: vec![ContentPart::Text {
                                text: (*text).to_owned(),
                            }],
                            phase: Some(MessagePhase::FinalAnswer),
                        }],
                        provider_response_id: None,
                    })
                })
                .collect(),
            tool_specs: Vec::<ToolSpec>::new().into(),
            system_prompt: "".into(),
            queued_inputs: InputQueues::default(),
            kind: AgentStateKind::Idle,
            context_used: None,
        }
    }

    #[test]
    fn latest_final_response_reports_newest_response_and_count() {
        let state = state_with_responses(&["first", "second"]);
        assert_eq!(inference_response_count(&state), 2);
        assert_eq!(
            latest_final_response(&state),
            Some((2, "second".to_owned()))
        );
    }
}
