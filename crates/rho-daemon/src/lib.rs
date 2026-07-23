use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use futures::StreamExt as _;
use rho_agent::db::{
    AgentDisposition, AgentId, AgentReadTxnExt as _, AgentRole, AgentWriteTxnExt as _, WorkstreamId,
};
use rho_agent::pool::{AgentPool, AgentTurnCompleted, RunningAgent};
use rho_agent::{AgentState, AgentStateKind, MessageDelivery};
use rho_core::{ContentPart, ContextBlock, text_content};
use rho_db::RhoDb;
use rho_inference::InferenceAuth;
use rho_ui_proto::remote::AgentRemoteEncoder;
use rho_ui_proto::server::{Server, ServerConnection};
use rho_ui_proto::{
    ClientMessage, JoinTarget, LandLeaseHolder, LandStatus, McpAgentToolRequest,
    McpAgentToolResponse, ServerMessage, StartMode, UiAgentSummary, UiAttention, UiProject,
    UiWorkstream, WorkspaceInfo, WorkstreamTarget, read_frame, read_frame_counted, write_frame,
    write_frame_counted,
};
use tokio::sync::{
    Mutex, Mutex as TokioMutex, Notify, OwnedMutexGuard, broadcast, mpsc, oneshot, watch,
};

pub mod debug;
mod shell;
mod terminal;
mod webui;

/// FDNAME under which messaging-platform secrets live in the systemd fd store.
const PLATFORM_SECRETS_FD_STORE_NAME: &str = "platform-secrets";
const IROH_SECRET: redb::TableDefinition<(), &[u8; 32]> =
    redb::TableDefinition::new("rho_daemon_iroh_secret_v1");

pub fn default_socket_path() -> anyhow::Result<PathBuf> {
    rho_ui_proto::socket_path()
}

pub fn default_db_path() -> anyhow::Result<PathBuf> {
    let base = dirs::state_dir().ok_or_else(|| anyhow::anyhow!("state directory not available"))?;
    Ok(base.join("rho").join("rho.redb"))
}

#[cfg(unix)]
fn login_environment() -> anyhow::Result<Vec<(OsString, OsString)>> {
    use std::os::unix::ffi::OsStringExt as _;

    let home = dirs::home_dir().context("home directory not available")?;
    let mut command = std::process::Command::new("bash");
    command
        .args(["-lc", "exec env -0"])
        .env_clear()
        .env("HOME", &home)
        .current_dir(&home);
    for name in ["PATH", "USER", "LOGNAME", "SHELL", "XDG_RUNTIME_DIR"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let output = command
        .output()
        .context("capture login-shell environment")?;
    anyhow::ensure!(
        output.status.success(),
        "login shell failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );

    let mut environment = Vec::new();
    for entry in output.stdout.split(|byte| *byte == 0) {
        if entry.is_empty() {
            continue;
        }
        let Some(separator) = entry.iter().position(|byte| *byte == b'=') else {
            anyhow::bail!("login shell emitted malformed environment output");
        };
        let name = &entry[..separator];
        if matches!(name, b"PWD" | b"OLDPWD" | b"SHLVL" | b"_") || name.starts_with(b"DIRENV_") {
            continue;
        }
        environment.push((
            OsString::from_vec(name.to_vec()),
            OsString::from_vec(entry[separator + 1..].to_vec()),
        ));
    }
    Ok(environment)
}

fn configure_octo_git_transport(environment: &mut Vec<(OsString, OsString)>) -> anyhow::Result<()> {
    let count = environment
        .iter()
        .find_map(|(name, value)| (name == "GIT_CONFIG_COUNT").then_some(value))
        .map(|value| {
            value
                .to_str()
                .context("GIT_CONFIG_COUNT is not valid UTF-8")?
                .parse::<usize>()
                .context("GIT_CONFIG_COUNT is not a number")
        })
        .transpose()?
        .unwrap_or(0);
    let rewrites = [
        ("url.octo://github.com/.insteadOf", "git@github.com:"),
        ("url.octo://github.com/.insteadOf", "ssh://git@github.com/"),
        ("url.octo://git@git.sr.ht/.insteadOf", "git@git.sr.ht:"),
        (
            "url.octo://git@git.sr.ht/.insteadOf",
            "ssh://git@git.sr.ht/",
        ),
    ];
    let new_count = count
        .checked_add(rewrites.len())
        .context("too many ambient Git configuration entries")?;
    set_environment_value(environment, "GIT_CONFIG_COUNT", new_count.to_string());
    for (offset, (key, value)) in rewrites.into_iter().enumerate() {
        let index = count + offset;
        set_environment_value(environment, &format!("GIT_CONFIG_KEY_{index}"), key);
        set_environment_value(environment, &format!("GIT_CONFIG_VALUE_{index}"), value);
    }
    Ok(())
}

fn set_environment_value(
    environment: &mut Vec<(OsString, OsString)>,
    name: &str,
    value: impl Into<OsString>,
) {
    let value = value.into();
    if let Some((_, current)) = environment.iter_mut().find(|(key, _)| key == name) {
        *current = value;
    } else {
        environment.push((name.into(), value));
    }
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

    fn contains_nonempty(&self, key: &str) -> bool {
        self.read()
            .ok()
            .and_then(|secrets| secrets.get(key).cloned())
            .is_some_and(|value| !value.trim().is_empty())
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
    let token_provider: octo_server::TokenProvider =
        Arc::new(move || secrets.get("GITHUB_TOKEN").context("reading GITHUB_TOKEN"));
    tokio::spawn(async move {
        if let Err(error) = octo_server::serve(listener, token_provider, github_api_url).await {
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
    /// Use BBR3 instead of CUBIC for daemon-to-client iroh traffic.
    #[arg(long = "iroh-bbr3", env = "RHO_IROH_BBR3")]
    pub iroh_bbr3: bool,
    #[arg(long = "extra-before-path", env = "RHO_EXTRA_BEFORE_PATH")]
    pub extra_before_path: Option<OsString>,
    #[arg(long = "extra-after-path", env = "RHO_EXTRA_AFTER_PATH")]
    pub extra_after_path: Option<OsString>,
    /// Write a Dial9 CPU trace on shutdown (requires a frame-pointer build).
    #[arg(long, value_name = "FILE")]
    pub cpu_profile: Option<PathBuf>,
}

pub fn install_crypto_provider() -> anyhow::Result<()> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .map_err(|_| anyhow::anyhow!("failed to install the AWS-LC rustls crypto provider"))?;
    }
    Ok(())
}

pub struct DaemonProfiler(Option<rho_profiling::CpuProfiler>);

impl DaemonProfiler {
    /// Start profiling before the async runtime creates worker threads.
    pub fn start(args: &mut DaemonArgs) -> anyhow::Result<Self> {
        Ok(Self(
            args.cpu_profile
                .take()
                .map(rho_profiling::CpuProfiler::start)
                .transpose()?,
        ))
    }

    pub fn finish(self, result: anyhow::Result<()>) -> anyhow::Result<()> {
        if let Some(profiler) = self.0 {
            match profiler.finish() {
                Ok(path) => eprintln!("rho daemon: wrote CPU profile to {}", path.display()),
                Err(error) if result.is_err() => {
                    eprintln!("rho daemon: failed to write CPU profile: {error:#}");
                }
                Err(error) => return Err(error.context("write daemon CPU profile")),
            }
        }
        result
    }
}

pub async fn run(args: DaemonArgs) -> anyhow::Result<()> {
    install_crypto_provider()?;
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
    let octo_socket_path = octo_types::socket_path()?;
    spawn_octo_server(&octo_socket_path, platform_secrets.clone())?;
    let mut user_environment = login_environment()?;
    configure_octo_git_transport(&mut user_environment)?;
    let user_environment = rho_workspaces::UserEnvironment::new(user_environment);

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
        let secret = load_or_create_iroh_secret(&db).await?;
        let iroh_auth = rho_iroh_auth::IrohAuth::new(db.clone(), secret.public());
        Some((secret, iroh_auth))
    } else {
        None
    };

    let iroh_auth = iroh.as_ref().map(|(_, auth)| auth.clone());
    let agents = Arc::new(
        AgentRegistry::new(
            db,
            auth,
            path_overrides,
            user_environment,
            platform_secrets,
            iroh_auth,
        )
        .await?,
    );
    agents.resume_platform_integrations();

    if let Some((secret, iroh_auth)) = iroh {
        let mut transport = iroh::endpoint::QuicTransportConfig::builder()
            .max_concurrent_bidi_streams(16u8.into())
            .qlog_from_env("rho-daemon");
        if args.iroh_bbr3 {
            transport = transport.congestion_controller_factory(Arc::new(
                noq_proto::congestion::Bbr3Config::default(),
            ));
        }
        let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
            .secret_key(secret)
            .transport_config(transport.build())
            .alpns(vec![
                rho_ui_proto::IROH_ALPN.to_vec(),
                rho_webui_messages::ALPN.to_vec(),
            ])
            .bind()
            .await
            .context("bind iroh endpoint")?;
        eprintln!("rho daemon iroh endpoint: {}", endpoint.id());
        tokio::spawn(run_iroh_listener(agents.clone(), endpoint, iroh_auth));
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
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        if args.die_on_detached
            && accepted_connection
            && active_connections.load(Ordering::Relaxed) == 0
        {
            return Ok(());
        }

        tokio::select! {
            result = &mut shutdown => {
                result?;
                return Ok(());
            }
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

async fn shutdown_signal() -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .context("register SIGTERM handler")?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result.context("wait for SIGINT"),
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await.context("wait for Ctrl-C")
    }
}

/// Loads the daemon's iroh identity from the local database.
async fn load_or_create_iroh_secret(db: &RhoDb) -> anyhow::Result<iroh::SecretKey> {
    let mut write = db.write().await;
    let mut table = write.open_table(IROH_SECRET);
    if let Some(secret) = table.get(&()) {
        return Ok(iroh::SecretKey::from_bytes(secret.value()));
    }

    let secret = iroh::SecretKey::generate().to_bytes();
    table.insert(&(), &secret);
    drop(table);
    write.commit();
    Ok(iroh::SecretKey::from_bytes(&secret))
}

/// Authenticates every iroh connection on its first bi-stream, then serves one
/// full UI control session plus any number of zed-channel bi-streams on
/// [`rho_ui_proto::IROH_ALPN`], the web UI JSON protocol on
/// [`rho_webui_messages::ALPN`]. Unapproved connections never reach either
/// application handler.
async fn run_iroh_listener(
    agents: Arc<AgentRegistry>,
    endpoint: iroh::Endpoint,
    auth: rho_iroh_auth::IrohAuth,
) {
    const MAX_CONCURRENT_PREAUTH: usize = 64;
    let preauth = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_PREAUTH));
    while let Some(incoming) = endpoint.accept().await {
        let permit = match preauth.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => return,
        };
        let agents = agents.clone();
        let auth = auth.clone();
        tokio::spawn(async move {
            let connection = match incoming.await {
                Ok(connection) => connection,
                Err(error) => {
                    eprintln!("rho daemon iroh accept error: {error:#}");
                    return;
                }
            };
            match rho_iroh_auth::authenticate_server_connection(&auth, &connection).await {
                Ok(rho_iroh_auth::ServerAuthDecision::Approved) => {
                    drop(permit);
                }
                Ok(
                    rho_iroh_auth::ServerAuthDecision::EnrollmentRequired(_)
                    | rho_iroh_auth::ServerAuthDecision::Unavailable,
                ) => {
                    connection.close(0u32.into(), b"iroh authentication required");
                    return;
                }
                Err(error) => {
                    eprintln!("rho daemon iroh authentication error: {error:#}");
                    connection.close(0u32.into(), b"iroh authentication failed");
                    return;
                }
            }
            let webui = connection.alpn() == rho_webui_messages::ALPN;
            let agent_streams = (!webui).then(|| IrohAgentStreams::new(connection.clone()));
            while let Ok((send, recv)) = connection.accept_bi().await {
                let agents = agents.clone();
                let agent_streams = agent_streams.clone();
                tokio::spawn(async move {
                    let result = if webui {
                        webui::serve_json_session(agents, recv, send).await
                    } else {
                        async {
                            let counters = rho_ui_proto::IoCounters::default();
                            let mut recv = recv;
                            let first =
                                read_frame_counted::<_, ClientMessage>(&mut recv, Some(&counters))
                                    .await?;
                            // Dedicated streams (zed channels, shells,
                            // terminals, one-shot queries) are not the UI control
                            // session and must not claim it.
                            let dedicated = matches!(
                                &first,
                                ClientMessage::ChannelOpen { .. }
                                    | ClientMessage::DiffSnapshot { .. }
                                    | ClientMessage::TerminalCreate { .. }
                                    | ClientMessage::TerminalAttach { .. }
                                    | ClientMessage::TerminalList { .. }
                                    | ClientMessage::ShellAttach { .. }
                                    | ClientMessage::GitTransportRequest { .. }
                                    | ClientMessage::GitTransportProvide { .. }
                                    | ClientMessage::GitTransportQuery { .. }
                            );
                            let control = if !dedicated {
                                let streams = agent_streams
                                    .clone()
                                    .context("iroh agent streams missing")?;
                                anyhow::ensure!(
                                    streams.claim_control(),
                                    "iroh connection already has a UI control session"
                                );
                                send.set_priority(1)
                                    .context("set iroh control stream priority")?;
                                Some(streams)
                            } else {
                                None
                            };
                            if matches!(
                                &first,
                                ClientMessage::TerminalCreate { .. }
                                    | ClientMessage::TerminalAttach { .. }
                                    | ClientMessage::ShellAttach { .. }
                            ) {
                                send.set_priority(50)
                                    .context("set iroh interactive stream priority")?;
                            }
                            let result = serve_connection_io(
                                agents,
                                recv,
                                send,
                                counters,
                                None,
                                agent_streams,
                                Some(first),
                            )
                            .await;
                            if let Some(control) = control {
                                control.close();
                            }
                            result
                        }
                        .await
                    };
                    if let Err(error) = result {
                        eprintln!("rho daemon iroh connection error: {error:#}");
                    }
                });
            }
        });
    }
}

const FOCUSED_AGENT_STREAM_WEIGHT: NonZeroU16 = NonZeroU16::new(200).unwrap();
const MAX_IROH_AGENT_STREAMS: usize = 1024;

/// Per-iroh-connection agent streams. Agent state is sent on daemon-opened
/// unidirectional streams so QUIC can schedule agents independently while the
/// bidirectional UI session remains a low-volume control channel.
#[derive(Clone)]
struct IrohAgentStreams {
    connection: iroh::endpoint::Connection,
    opened: Arc<Mutex<HashSet<AgentId>>>,
    control_claimed: Arc<AtomicBool>,
    focus: watch::Sender<Option<AgentId>>,
    cancel: watch::Sender<bool>,
}

impl IrohAgentStreams {
    fn new(connection: iroh::endpoint::Connection) -> Self {
        let (focus, _) = watch::channel(None);
        let (cancel, _) = watch::channel(false);
        Self {
            connection,
            opened: Arc::new(Mutex::new(HashSet::new())),
            control_claimed: Arc::new(AtomicBool::new(false)),
            focus,
            cancel,
        }
    }

    fn set_focus(&self, agent_id: Option<AgentId>) {
        self.focus.send_replace(agent_id);
    }

    fn claim_control(&self) -> bool {
        self.control_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    }

    fn close(&self) {
        self.cancel.send_replace(true);
        self.connection
            .close(0u32.into(), b"UI control session closed");
    }

    async fn ensure(&self, agent_id: AgentId, agent: RunningAgent) -> anyhow::Result<()> {
        {
            let mut opened = self.opened.lock().await;
            if opened.contains(&agent_id) {
                return Ok(());
            }
            if opened.len() >= MAX_IROH_AGENT_STREAMS {
                self.connection
                    .close(2u32.into(), b"too many loaded agents");
                anyhow::bail!(
                    "iroh agent stream limit ({MAX_IROH_AGENT_STREAMS}) reached; \
                     hide agents before reconnecting"
                );
            }
            opened.insert(agent_id);
        }
        let connection = self.connection.clone();
        let focus_sender = self.focus.clone();
        let cancel_sender = self.cancel.clone();
        let opened = self.opened.clone();
        tokio::spawn(async move {
            const RETRIES: usize = 3;
            let mut exhausted = true;
            for attempt in 0..RETRIES {
                if *cancel_sender.borrow() {
                    exhausted = false;
                    break;
                }
                let focus = focus_sender.subscribe();
                let cancel = cancel_sender.subscribe();
                let result = async {
                    let send = connection
                        .open_uni()
                        .await
                        .context("open iroh agent stream")?;
                    serve_iroh_agent_stream(agent_id, agent.clone(), send, focus, cancel).await
                }
                .await;
                match result {
                    Ok(()) => {
                        exhausted = false;
                        break;
                    }
                    Err(error) => {
                        eprintln!("rho daemon iroh agent stream error: {error:#}");
                        if attempt + 1 < RETRIES {
                            let mut retry_cancel = cancel_sender.subscribe();
                            tokio::select! {
                                _ = tokio::time::sleep(std::time::Duration::from_millis(100 << attempt)) => {}
                                _ = retry_cancel.changed() => {
                                    exhausted = false;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            opened.lock().await.remove(&agent_id);
            if exhausted {
                connection.close(1u32.into(), b"agent state stream failed");
            }
        });
        Ok(())
    }
}

async fn serve_iroh_agent_stream(
    agent_id: AgentId,
    agent: RunningAgent,
    mut send: iroh::endpoint::SendStream,
    mut focus: watch::Receiver<Option<AgentId>>,
    mut cancel: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    if *cancel.borrow() {
        return Ok(());
    }
    let priority = send.priority_handle();
    let weight = |focused| {
        if focused {
            FOCUSED_AGENT_STREAM_WEIGHT
        } else {
            NonZeroU16::MIN
        }
    };
    priority
        .set_weight(weight(*focus.borrow() == Some(agent_id)))
        .context("set initial iroh agent stream weight")?;
    let mut focus_cancel = cancel.clone();
    let focus_task = tokio::spawn(async move {
        loop {
            tokio::select! {
                changed = focus.changed() => {
                    changed.context("iroh agent focus channel closed")?;
                    priority
                        .set_weight(weight(*focus.borrow_and_update() == Some(agent_id)))
                        .context("update iroh agent stream weight")?;
                }
                _ = focus_cancel.changed() => return Ok::<(), anyhow::Error>(()),
            }
        }
    });

    let result = async {
        write_frame(&mut send, &ServerMessage::AgentStreamOpened { agent_id }).await?;
        let changes = agent.subscribe();
        let mut encoder = AgentRemoteEncoder::new();
        write_frame(
            &mut send,
            &ServerMessage::Agent {
                agent_id,
                frame: encoder.encode(agent.state()),
            },
        )
        .await?;
        futures::pin_mut!(changes);
        loop {
            tokio::select! {
                _ = cancel.changed() => return Ok(()),
                state = changes.next() => {
                    let Some(state) = state else { return Ok(()) };
                    write_frame(
                        &mut send,
                        &ServerMessage::Agent {
                            agent_id,
                            frame: encoder.encode(state),
                        },
                    )
                    .await?;
                }
            }
        }
    }
    .await;
    focus_task.abort();
    result
}

trait GitStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T> GitStream for T where T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
type BoxGitStream = Box<dyn GitStream>;

#[derive(Default)]
struct GitTransportState {
    providers: HashMap<u64, mpsc::UnboundedSender<ServerMessage>>,
    pending: HashMap<u64, PendingGitTransport>,
}

struct PendingGitTransport {
    response: oneshot::Sender<Result<BoxGitStream, String>>,
    recipients: HashMap<u64, mpsc::UnboundedSender<ServerMessage>>,
    remaining: HashSet<u64>,
}

#[derive(Default)]
struct GitTransportBroker {
    next_request_id: AtomicU64,
    next_provider_id: AtomicU64,
    state: TokioMutex<GitTransportState>,
}

enum GitProviderClaim {
    Selected(oneshot::Sender<Result<BoxGitStream, String>>),
    Done,
}

impl GitTransportBroker {
    async fn register(&self, provider: mpsc::UnboundedSender<ServerMessage>) {
        let provider_id = self.next_provider_id.fetch_add(1, Ordering::Relaxed);
        let mut state = self.state.lock().await;
        state.providers.retain(|_, provider| !provider.is_closed());
        state.providers.insert(provider_id, provider);
    }

    async fn request(
        &self,
        request: rho_ui_proto::GitTransportRequest,
    ) -> anyhow::Result<BoxGitStream> {
        self.request_with_timeout(request, std::time::Duration::from_secs(60))
            .await
    }

    async fn request_with_timeout(
        &self,
        request: rho_ui_proto::GitTransportRequest,
        timeout: std::time::Duration,
    ) -> anyhow::Result<BoxGitStream> {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.state.lock().await;
            state.providers.retain(|_, provider| !provider.is_closed());
            anyhow::ensure!(
                !state.providers.is_empty(),
                "no GUI clients are registered for SSH Git transport"
            );
            anyhow::ensure!(
                state.pending.len() < 8,
                "too many pending GUI Git transport requests"
            );
            let recipients = state.providers.clone();
            let remaining = recipients.keys().copied().collect();
            state.pending.insert(
                request_id,
                PendingGitTransport {
                    response: tx,
                    recipients: recipients.clone(),
                    remaining,
                },
            );
            let mut disconnected = Vec::new();
            for (&provider_id, provider) in &recipients {
                if provider
                    .send(ServerMessage::GitTransportRequested {
                        request_id,
                        provider_id,
                        request: request.clone(),
                    })
                    .is_err()
                {
                    disconnected.push(provider_id);
                }
            }
            for provider_id in disconnected {
                state.providers.remove(&provider_id);
                if let Some(pending) = state.pending.get_mut(&request_id) {
                    pending.remaining.remove(&provider_id);
                }
            }
            if state
                .pending
                .get(&request_id)
                .is_some_and(|pending| pending.remaining.is_empty())
            {
                state.pending.remove(&request_id);
                anyhow::bail!("all registered GUI SSH Git clients disconnected");
            }
        }
        let result = match tokio::time::timeout(timeout, rx).await {
            Ok(result) => result.context("SSH Git provider claim was abandoned")?,
            Err(_) => {
                let pending = self.state.lock().await.pending.remove(&request_id);
                if let Some(pending) = pending {
                    Self::notify_done(request_id, &pending.recipients, None);
                }
                anyhow::bail!(
                    "no registered GUI claimed the SSH Git transport request within 60 seconds"
                );
            }
        };
        result.map_err(anyhow::Error::msg)
    }

    async fn claim(
        &self,
        request_id: u64,
        provider_id: u64,
        claim: bool,
    ) -> anyhow::Result<GitProviderClaim> {
        let mut state = self.state.lock().await;
        let Some(pending) = state.pending.get_mut(&request_id) else {
            return Ok(GitProviderClaim::Done);
        };
        if !pending.remaining.remove(&provider_id) {
            return Ok(GitProviderClaim::Done);
        }
        if claim {
            let pending = state
                .pending
                .remove(&request_id)
                .expect("pending request was just found");
            Self::notify_done(request_id, &pending.recipients, Some(provider_id));
            return Ok(GitProviderClaim::Selected(pending.response));
        }
        if pending.remaining.is_empty() {
            let pending = state
                .pending
                .remove(&request_id)
                .expect("pending request was just found");
            Self::notify_done(request_id, &pending.recipients, None);
            let _ = pending.response.send(Err(
                "all registered GUI clients rejected the SSH Git transport request".to_owned(),
            ));
        }
        Ok(GitProviderClaim::Done)
    }

    fn notify_done(
        request_id: u64,
        recipients: &HashMap<u64, mpsc::UnboundedSender<ServerMessage>>,
        except: Option<u64>,
    ) {
        for (&provider_id, provider) in recipients {
            if Some(provider_id) != except {
                let _ = provider.send(ServerMessage::GitTransportDone { request_id });
            }
        }
    }
}

struct AgentRegistry {
    pool: Arc<AgentPool>,
    db: RhoDb,
    auth: InferenceAuth,
    /// The database's machine seed, announced in `Ready` so clients can
    /// encode agent IDs.
    machine_seed: u64,
    /// Agents with a title generation in flight, so a burst of messages to an
    /// untitled agent starts at most one task.
    title_tasks: Mutex<HashSet<AgentId>>,
    land_locks: Mutex<HashMap<Utf8PathBuf, Arc<TokioMutex<()>>>>,
    land_holders: Mutex<HashMap<Utf8PathBuf, LandLeaseHolder>>,
    land_statuses: Mutex<HashMap<Utf8PathBuf, (Option<AgentId>, LandStatus)>>,
    /// In-process Slack connection and its thread sessions
    /// (see [`rho_slack::SlackManager`]).
    slack: Arc<rho_slack::SlackManager>,
    /// Durable CI and review-feedback watches owned by PR-friendly PMs.
    pr_monitor: Arc<rho_pr_monitor::PrMonitor>,
    /// Shared sealed platform secret store used by Slack and Octo.
    platform_secrets: PlatformSecrets,
    /// Daemon-wide fanout for messages every client must hear regardless of
    /// which connection caused them (attention changes); each connection
    /// forwards this onto its own outgoing channel.
    events: broadcast::Sender<ServerMessage>,
    /// Enrollment/trust for iroh clients; `None` unless `--iroh` is set.
    iroh_auth: Option<rho_iroh_auth::IrohAuth>,
    /// The in-process zed host (headless gpui thread), spawned lazily on the
    /// first channel open so daemons that never serve an editing client
    /// never start it.
    zed_host: std::sync::OnceLock<rho_zed_host::ZedHost>,
    /// Daemon-owned Comint-style shell sessions, one per agent.
    shells: Arc<shell::ShellRegistry>,
    /// Daemon-owned terminal sessions, keyed per agent.
    terminals: Arc<terminal::TerminalRegistry>,
    /// The snapshotted login environment, for terminal shells.
    user_environment: rho_workspaces::UserEnvironment,
    git_transport: GitTransportBroker,
}

impl AgentRegistry {
    async fn new(
        db: RhoDb,
        auth: InferenceAuth,
        path_overrides: PathOverrides,
        user_environment: rho_workspaces::UserEnvironment,
        platform_secrets: PlatformSecrets,
        iroh_auth: Option<rho_iroh_auth::IrohAuth>,
    ) -> anyhow::Result<Self> {
        let pool = AgentPool::new(
            db.clone(),
            auth.clone(),
            path_overrides,
            user_environment.clone(),
        )
        .await;
        let machine_seed = db.read().machine_seed();
        let slack = rho_slack::SlackManager::new(pool.clone(), db.clone()).await;
        let pr_monitor = rho_pr_monitor::PrMonitor::new(pool.clone(), db.clone()).await?;
        let registry = Self {
            pool,
            db,
            auth,
            machine_seed,
            title_tasks: Mutex::new(HashSet::new()),
            land_locks: Mutex::new(HashMap::new()),
            land_holders: Mutex::new(HashMap::new()),
            land_statuses: Mutex::new(HashMap::new()),
            slack,
            pr_monitor,
            platform_secrets,
            events: broadcast::channel(1024).0,
            iroh_auth,
            zed_host: std::sync::OnceLock::new(),
            shells: Arc::new(shell::ShellRegistry::default()),
            terminals: Arc::new(terminal::TerminalRegistry::default()),
            user_environment,
            git_transport: GitTransportBroker::default(),
        };
        registry.pool.load_non_hidden_agents().await;
        Ok(registry)
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

    fn zed_host(&self) -> &rho_zed_host::ZedHost {
        self.zed_host.get_or_init(rho_zed_host::ZedHost::spawn)
    }

    fn ui_workstreams(&self) -> Vec<UiWorkstream> {
        let mut records = self.db.read().list_workstreams();
        records.sort_by_key(|(_, workstream)| workstream.created_at);
        records
            .into_iter()
            .map(|(workstream_id, workstream)| UiWorkstream {
                workstream_id,
                name: workstream.name,
                labels: workstream.labels,
            })
            .collect()
    }

    fn ui_agents(&self, kinds: &HashMap<AgentId, AgentStateKind>) -> Vec<UiAgentSummary> {
        let mut records = self.db.read().list_agents();
        records.sort_by_key(|(_, agent)| agent.created_at);
        records
            .into_iter()
            .map(|(agent_id, agent)| UiAgentSummary {
                agent_id,
                parent_agent: agent.parent_agent,
                role: agent.config(),
                created_at: agent.created_at,
                updated_at: agent.updated_at,
                workspace: agent.primary_workdir().clone(),
                display_name: agent.display_name,
                attention: attention_level(kinds.get(&agent_id), agent.disposition),
                last_active: agent.last_user_message.max(agent.created_at),
                hidden: agent.disposition == AgentDisposition::Hidden,
                last_user_message_text: agent.last_user_message_text,
                workstream: agent.workstream,
                labels: agent.labels,
            })
            .collect()
    }

    fn projects(&self) -> Vec<UiProject> {
        let mut projects = self
            .db
            .read()
            .list_projects()
            .into_iter()
            .map(|(path, record)| UiProject {
                path,
                name: record.name,
                description: record.description,
            })
            .collect::<Vec<_>>();
        projects.sort_by(|left, right| left.name.cmp(&right.name));
        projects
    }

    async fn ready_message(&self) -> ServerMessage {
        ServerMessage::Ready {
            workstreams: self.ui_workstreams(),
            agents: self.ui_agents(&self.agent_state_kinds().await),
            projects: self.projects(),
            view_config: self.db.read().view_config(),
            machine_seed: self.machine_seed,
            agent_counter: self.db.read().last_agent_counter(),
        }
    }

    async fn loaded(&self) -> Vec<(AgentId, RunningAgent)> {
        self.pool.loaded().await
    }

    async fn visible_loaded(&self) -> Vec<(AgentId, RunningAgent)> {
        self.pool
            .loaded()
            .await
            .into_iter()
            .filter(|(agent_id, _)| {
                self.db.read().get_agent(*agent_id).disposition != AgentDisposition::Hidden
            })
            .collect()
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

    async fn create_workstream(&self, name: String) -> UiWorkstream {
        let mut write = self.db.write().await;
        let workstream_id = write.create_workstream(rho_core::UnixMs::now(), name);
        write.commit();
        // Re-read: creation may have uniquified the name.
        let workstream = self.db.read().get_workstream(workstream_id);
        UiWorkstream {
            workstream_id,
            name: workstream.name,
            labels: workstream.labels,
        }
    }

    async fn create(
        &self,
        workstream: WorkstreamId,
        role: AgentRole,
        start: StartMode,
    ) -> anyhow::Result<(AgentId, RunningAgent)> {
        let start = match start {
            StartMode::NewOn { repo, revset } => {
                let repo = validate_repo_root(repo)?;
                vec![rho_agent::StartWorkdir::Create {
                    repo: self.pool.repo(&repo).await?,
                    parent_revset: revset,
                }]
            }
            StartMode::Sandbox { repo, revset } => {
                let repo = validate_repo_root(repo)?;
                vec![rho_agent::StartWorkdir::Sandbox {
                    repo: self.pool.repo(&repo).await?,
                    parent_revset: revset,
                }]
            }
            StartMode::Join(JoinTarget::Workspace(info)) => {
                vec![rho_agent::StartWorkdir::Existing(
                    self.pool.open_workspace(&info).await?,
                )]
            }
            StartMode::Join(JoinTarget::User { repo }) => {
                let repo = validate_repo_root(repo)?;
                vec![rho_agent::StartWorkdir::Existing(
                    self.pool.repo(&repo).await?.user_checkout().await?,
                )]
            }
        };
        let (agent_id, agent) = self.pool.create(workstream, role, None, start).await?;
        Ok((agent_id, agent))
    }

    async fn mcp_agent_tool(
        &self,
        self_agent_id: AgentId,
        request: McpAgentToolRequest,
    ) -> anyhow::Result<String> {
        if !self.pool.agent_exists(self_agent_id) {
            anyhow::bail!("agent is not known: {self_agent_id:?}");
        }
        let role = self.db.read().get_agent(self_agent_id).role;
        if matches!(role, AgentRole::Advisor { .. })
            && !matches!(
                &request,
                McpAgentToolRequest::MessageAgent { .. }
                    | McpAgentToolRequest::FollowupAdvisor { .. }
                    | McpAgentToolRequest::Wait { .. }
            )
        {
            anyhow::bail!("Advisors may only message agents and wait for replies");
        }
        if role.is_pm()
            && matches!(
                &request,
                McpAgentToolRequest::AskAdvisor { .. } | McpAgentToolRequest::Wait { .. }
            )
        {
            anyhow::bail!("Project managers cannot ask Advisors or wait for agent mail");
        }
        match request {
            McpAgentToolRequest::SpawnEngineer {
                task_name,
                prompt,
                workdirs,
            } => {
                if prompt.trim().is_empty() {
                    anyhow::bail!("prompt must not be empty");
                }
                let workdirs = rho_agent::multi_agent_tools::parse_spawn_workdirs(
                    workdirs
                        .into_iter()
                        .map(|entry| rho_agent::multi_agent_tools::SpawnWorkdirArgs {
                            repo: entry.repo,
                            checkout: None,
                            revset: entry.revset,
                        })
                        .collect(),
                )?;
                let child_id = self
                    .pool
                    .spawn_child(
                        self_agent_id,
                        task_name.clone(),
                        prompt,
                        workdirs,
                        AgentRole::default(),
                    )
                    .await?;
                let child_record = self.pool.db().read().get_agent(child_id);
                let workspace_note = match child_record.primary_workdir().workspace_handle() {
                    Some(workspace) => format!(
                        " Its jj workspace is `{workspace}`; inspect its working-copy commit with \
                         `jj diff -r '{workspace}@' --stat`."
                    ),
                    None => " It is running in the shared user checkout workspace; there is no \
                             separate `<workspace>@` handle."
                        .to_owned(),
                };
                Ok(format!(
                    "Spawned Engineer {} for task \"{}\". Its results will arrive as mail.{}",
                    self.display_agent_id(child_id),
                    task_name,
                    workspace_note,
                ))
            }
            McpAgentToolRequest::MessageAgent { agent_id, message } => {
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
            McpAgentToolRequest::InterruptEngineer {
                engineer_id: agent_id,
            } => {
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
            McpAgentToolRequest::AskAdvisor { message } => {
                let workdirs = self
                    .db
                    .read()
                    .get_agent(self_agent_id)
                    .workdirs
                    .into_iter()
                    .map(|info| rho_agent::pool::SpawnWorkdir {
                        repo: info.repo().to_owned(),
                        checkout: rho_agent::pool::SpawnCheckout::Shared,
                    })
                    .collect();
                let advisor = self
                    .pool
                    .spawn_child(
                        self_agent_id,
                        "advisor".to_owned(),
                        message,
                        workdirs,
                        AgentRole::Advisor {
                            intelligence: rho_agent::db::AdvisorIntelligence::Medium,
                        },
                    )
                    .await?;
                Ok(format!(
                    "Advisor {} is considering the question.",
                    self.display_agent_id(advisor)
                ))
            }
            McpAgentToolRequest::FollowupAdvisor {
                advisor_id,
                message,
            } => {
                let advisor = self.resolve_display_agent_id(&advisor_id)?;
                let record = self.db.read().get_agent(advisor);
                anyhow::ensure!(
                    matches!(record.role, AgentRole::Advisor { .. }),
                    "target is not an Advisor"
                );
                anyhow::ensure!(
                    record.parent_agent == Some(self_agent_id),
                    "Advisor belongs to another agent"
                );
                self.pool
                    .deliver_mail(
                        self_agent_id,
                        advisor,
                        message,
                        MessageDelivery::NextRequest,
                    )
                    .await?;
                Ok(format!("Follow-up sent to Advisor {advisor_id}."))
            }
        }
    }

    fn resolve_display_agent_id(&self, agent_id: &str) -> anyhow::Result<AgentId> {
        let text = agent_id.trim();
        let (prefix, raw_agent_id) = match text.split_once('-') {
            Some((prefix, raw)) => (Some(prefix), raw),
            None => (None, text),
        };
        let resolved = match self.pool.resolve_agent_id(raw_agent_id)? {
            prefix_id::PrefixResolution::Unique(agent_id) => agent_id,
            prefix_id::PrefixResolution::Ambiguous { .. } => {
                anyhow::bail!("ambiguous agent id {agent_id}")
            }
            prefix_id::PrefixResolution::NotFound => {
                anyhow::bail!("no agent with id {agent_id}")
            }
        };
        if !self.pool.agent_exists(resolved) {
            anyhow::bail!("no agent with id {agent_id}");
        }
        if let Some(prefix) = prefix {
            let expected = self.db.read().get_agent(resolved).role.handle_prefix();
            anyhow::ensure!(
                prefix == expected,
                "agent handle prefix does not match its role"
            );
        }
        Ok(resolved)
    }

    fn display_agent_id(&self, agent_id: AgentId) -> String {
        self.pool.agent_handle(agent_id)
    }

    /// Moves an agent to another workstream. Its spawn subtree moves with
    /// it, so a subtree never straddles workstreams; a `Named` target that
    /// matches no workstream founds one under that name.
    async fn move_agent(&self, agent_id: AgentId, target: WorkstreamTarget) -> anyhow::Result<()> {
        let now = rho_core::UnixMs::now();
        let mut write = self.db.write().await;
        let read = self.db.read();
        let workstreams = read.list_workstreams();
        let workstream_id = match target {
            WorkstreamTarget::Existing(workstream_id) => {
                if !workstreams.iter().any(|(id, _)| *id == workstream_id) {
                    anyhow::bail!("unknown workstream id: {}", workstream_id.0);
                }
                workstream_id
            }
            WorkstreamTarget::Named(name) => workstreams
                .iter()
                .find(|(_, workstream)| workstream.name == name)
                .map(|(workstream_id, _)| *workstream_id)
                .unwrap_or_else(|| write.create_workstream(now, name)),
        };
        let agents = read.list_agents();
        let Some((_, moved)) = agents.iter().find(|(id, _)| *id == agent_id) else {
            anyhow::bail!("agent is not known: {agent_id:?}");
        };
        let source = moved.workstream;
        let members = spawn_subtree(&agents, agent_id);
        for member in &members {
            write.set_agent_workstream(now, *member, workstream_id);
        }
        // A workstream is only a statement about its agents; when the move
        // empties the source, nothing is being said and the record goes,
        // rather than lingering as a nameless husk (and letting merges be
        // plain moves).
        let source_emptied = source != workstream_id
            && agents
                .iter()
                .filter(|(_, agent)| agent.workstream == source)
                .all(|(id, _)| members.contains(id));
        if source_emptied {
            write.delete_workstream(source);
        }
        write.commit();
        Ok(())
    }

    async fn workstream_label(
        &self,
        workstream_id: WorkstreamId,
        label: String,
        add: bool,
    ) -> anyhow::Result<()> {
        validate_label(&label)?;
        let mut write = self.db.write().await;
        write.workstream_label(rho_core::UnixMs::now(), workstream_id, &label, add);
        write.commit();
        Ok(())
    }

    async fn agent_label(&self, agent_id: AgentId, label: String, add: bool) -> anyhow::Result<()> {
        validate_label(&label)?;
        let mut write = self.db.write().await;
        write.agent_label(rho_core::UnixMs::now(), agent_id, &label, add);
        write.commit();
        Ok(())
    }

    /// Titles an untitled agent from its first user message, in the
    /// background. Policy: only fills an empty `display_name` — a manual
    /// rename, before or during generation, always wins — and at most one
    /// generation runs per agent at a time. Every connection gets a `Ready`
    /// refresh when the title lands. A workstream the agent founded
    /// (`retitle`: its id plus the provisional name it was created under)
    /// takes the same title, unless someone renamed it meanwhile.
    async fn maybe_generate_title(
        self: &Arc<Self>,
        agent_id: AgentId,
        text: String,
        retitle: Option<(WorkstreamId, String)>,
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
                        let now = rho_core::UnixMs::now();
                        write.set_agent_display_name(now, agent_id, title.clone());
                        if let Some((workstream_id, provisional)) = retitle
                            && registry.db.read().get_workstream(workstream_id).name == provisional
                        {
                            write.set_workstream_name(now, workstream_id, title);
                        }
                        write.commit();
                        // Titles show on every client's rail, so the refresh
                        // fans out instead of following the acting connection.
                        let _ = registry.events.send(registry.ready_message().await);
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

    async fn rename_workstream(
        &self,
        workstream_id: WorkstreamId,
        name: String,
    ) -> anyhow::Result<()> {
        if name.trim().is_empty() {
            anyhow::bail!("workstream name cannot be empty");
        }
        let mut write = self.db.write().await;
        write.set_workstream_name(rho_core::UnixMs::now(), workstream_id, name);
        write.commit();
        Ok(())
    }

    async fn set_project(
        &self,
        path: Utf8PathBuf,
        name: Option<String>,
        description: String,
    ) -> anyhow::Result<()> {
        let path = validate_repo_root(path)?;
        let name = match name {
            Some(name) => name,
            None => path
                .file_name()
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("workdir path has no basename: {path}"))?,
        };
        let mut write = self.db.write().await;
        write.upsert_project(rho_core::UnixMs::now(), path.as_str(), name, description);
        write.commit();
        Ok(())
    }

    async fn remove_project(&self, path: Utf8PathBuf) -> anyhow::Result<()> {
        let mut write = self.db.write().await;
        write.remove_project(path.as_str());
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
    serve_connection_io(agents, reader, writer, counters, land_holder, None, None).await
}

/// One UI protocol session over any framed byte stream (Unix socket or an
/// iroh bi-stream from an enrolled remote client).
async fn serve_connection_io<R, W>(
    agents: Arc<AgentRegistry>,
    reader: R,
    writer: W,
    counters: rho_ui_proto::IoCounters,
    land_holder: Option<LandLeaseHolder>,
    agent_streams: Option<IrohAgentStreams>,
    first: Option<ClientMessage>,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // The first client frame chooses the stream's protocol: `ChannelOpen`
    // dedicates the whole stream to one zed channel, anything else starts a
    // normal UI session (every UI client speaks first — Subscribe or a
    // command — so waiting here never deadlocks).
    let mut reader = reader;
    let first = match first {
        Some(first) => first,
        None => read_frame_counted::<_, ClientMessage>(&mut reader, Some(&counters)).await?,
    };
    if let ClientMessage::ChannelOpen { workspace } = first {
        return serve_zed_channel(agents, reader, writer, workspace).await;
    }
    if let ClientMessage::DiffSnapshot {
        workspace,
        known_commit_id,
        include_paths,
    } = first
    {
        return serve_diff_snapshot(agents, writer, workspace, known_commit_id, include_paths)
            .await;
    }
    if let ClientMessage::TerminalCreate {
        agent,
        terminal_id,
        attach,
        cols,
        rows,
    } = first
    {
        let open = TerminalOpenKind::Create { attach };
        return serve_terminal(agents, reader, writer, agent, terminal_id, open, cols, rows).await;
    }
    if let ClientMessage::TerminalAttach {
        agent,
        terminal_id,
        cols,
        rows,
    } = first
    {
        let open = TerminalOpenKind::Attach;
        return serve_terminal(agents, reader, writer, agent, terminal_id, open, cols, rows).await;
    }
    if let ClientMessage::TerminalList { agent } = first {
        return serve_terminal_list(agents, writer, agent).await;
    }
    if let ClientMessage::ShellAttach { agent } = first {
        return serve_shell(agents, reader, writer, agent).await;
    }
    if let ClientMessage::GitTransportRequest { request } = first {
        return serve_git_transport_request(agents, reader, writer, request).await;
    }
    if let ClientMessage::GitTransportProvide {
        request_id,
        provider_id,
        claim,
    } = first
    {
        return serve_git_transport_provider(
            agents,
            reader,
            writer,
            request_id,
            provider_id,
            claim,
        )
        .await;
    }
    if let ClientMessage::GitTransportQuery { host } = first {
        let pat_available =
            host == "github.com" && agents.platform_secrets.contains_nonempty("GITHUB_TOKEN");
        let mut writer = writer;
        write_frame(
            &mut writer,
            &ServerMessage::GitTransportPolicy { pat_available },
        )
        .await?;
        return Ok(());
    }

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
    if let Some(agent_streams) = &agent_streams {
        for (agent_id, agent) in agents.visible_loaded().await {
            if let Err(error) = agent_streams.ensure(agent_id, agent).await {
                let _ = outgoing_tx.send(ServerMessage::Error {
                    message: error.to_string(),
                });
            }
        }
    } else {
        for (agent_id, agent) in agents.loaded().await {
            subscribe_agent(agent_id, agent, outgoing_tx.clone());
        }
    }

    // Announce every agent created in the pool — by clients or by other
    // agents spawning children — so it shows up on this connection.
    {
        let agents = Arc::clone(&agents);
        let outgoing_tx = outgoing_tx.clone();
        let agent_streams = agent_streams.clone();
        tokio::spawn(async move {
            loop {
                match created_rx.recv().await {
                    Ok(created) => {
                        if let Some(agent_streams) = &agent_streams {
                            if let Err(error) =
                                agent_streams.ensure(created.agent_id, created.agent).await
                            {
                                let _ = outgoing_tx.send(ServerMessage::Error {
                                    message: error.to_string(),
                                });
                            }
                        } else {
                            subscribe_agent(created.agent_id, created.agent, outgoing_tx.clone());
                        }
                        if outgoing_tx
                            .send(ServerMessage::AgentCreated {
                                agent_id: created.agent_id,
                                workstream: created.workstream,
                            })
                            .is_err()
                            || outgoing_tx.send(agents.ready_message().await).is_err()
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Rebuild both the stream set and the refreshed list;
                        // otherwise a missed creation would never get an
                        // agent-state stream on this connection.
                        if let Some(agent_streams) = &agent_streams {
                            for (agent_id, agent) in agents.visible_loaded().await {
                                if let Err(error) = agent_streams.ensure(agent_id, agent).await {
                                    let _ = outgoing_tx.send(ServerMessage::Error {
                                        message: error.to_string(),
                                    });
                                    break;
                                }
                            }
                        }
                        if outgoing_tx.send(agents.ready_message().await).is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

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
    let mut first = Some(first);
    let result = loop {
        let message = match first.take() {
            Some(message) => message,
            None => {
                match read_frame_counted::<_, ClientMessage>(&mut reader, Some(&counters)).await {
                    Ok(message) => message,
                    Err(error) => {
                        for (repo, _) in &land_leases {
                            agents.clear_land_holder(repo).await;
                        }
                        break Err(error);
                    }
                }
            }
        };
        match handle_message(
            &agents,
            &outgoing_tx,
            &mut land_leases,
            land_holder.clone(),
            agent_streams.as_ref(),
            message,
        )
        .await
        {
            Ok(Refresh::Ready) => {
                // Registry changes show on every client (GUI rails, the web
                // UI, a waiting CLI), so the refreshed snapshot goes through
                // the daemon-wide event fanout, not just this connection.
                let _ = agents.events.send(agents.ready_message().await);
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

async fn serve_git_transport_request<R, W>(
    agents: Arc<AgentRegistry>,
    reader: R,
    mut writer: W,
    request: rho_ui_proto::GitTransportRequest,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut provider = match agents.git_transport.request(request).await {
        Ok(provider) => provider,
        Err(error) => {
            write_frame(
                &mut writer,
                &ServerMessage::GitTransportRefused {
                    reason: error.to_string(),
                },
            )
            .await?;
            return Ok(());
        }
    };
    write_frame(&mut writer, &ServerMessage::GitTransportReady).await?;
    let mut requester = tokio::io::join(reader, writer);
    tokio::io::copy_bidirectional(&mut requester, &mut provider).await?;
    Ok(())
}

async fn serve_git_transport_provider<R, W>(
    agents: Arc<AgentRegistry>,
    reader: R,
    mut writer: W,
    request_id: u64,
    provider_id: u64,
    claim: bool,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    match agents
        .git_transport
        .claim(request_id, provider_id, claim)
        .await?
    {
        GitProviderClaim::Done => {
            write_frame(&mut writer, &ServerMessage::GitTransportDone { request_id }).await?;
        }
        GitProviderClaim::Selected(response) => {
            if let Err(error) = write_frame(&mut writer, &ServerMessage::GitTransportReady).await {
                let _ = response.send(Err(format!(
                    "selected GUI SSH Git client disconnected: {error}"
                )));
                return Err(error);
            }
            let stream = Box::new(tokio::io::join(reader, writer));
            response
                .send(Ok(stream))
                .map_err(|_| anyhow::anyhow!("Git transport requester disconnected"))?;
        }
    }
    Ok(())
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
/// `Ready` (workstreams, agents, workdirs); `Ready` refreshes every
/// connection, so all clients converge on the change at once.
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
    agent_streams: Option<&IrohAgentStreams>,
    message: ClientMessage,
) -> anyhow::Result<Refresh> {
    match message {
        ClientMessage::Ping => {
            let _ = outgoing_tx.send(ServerMessage::Pong);
            Ok(Refresh::None)
        }
        ClientMessage::ChatGptUsage => {
            let outgoing_tx = outgoing_tx.clone();
            tokio::spawn(async move {
                if let Ok(Ok(Some(usage))) = tokio::task::spawn_blocking(|| {
                    rho_inference::chatgpt_weekly_usage("default")
                })
                .await
                {
                    let _ = outgoing_tx.send(ServerMessage::ChatGptUsage {
                        used_percent: usage.used_percent,
                        reset_at_unix: usage.reset_at_unix,
                    });
                }
            });
            Ok(Refresh::None)
        }
        ClientMessage::ShellStart { request_id, agent } => {
            let agents = Arc::clone(agents);
            let outgoing_tx = outgoing_tx.clone();
            tokio::spawn(async move {
                let response = match shell_start(&agents, &agent).await {
                    Ok(()) => ServerMessage::ShellStarted { request_id },
                    Err(error) => ServerMessage::ShellRequestFailed {
                        request_id,
                        reason: format!("{error:#}"),
                    },
                };
                let _ = outgoing_tx.send(response);
            });
            Ok(Refresh::None)
        }
        ClientMessage::ShellList { request_id, agent } => {
            let response = match shell_list(agents, agent.as_deref()).await {
                Ok(shells) => ServerMessage::ShellList { request_id, shells },
                Err(error) => ServerMessage::ShellRequestFailed {
                    request_id,
                    reason: format!("{error:#}"),
                },
            };
            let _ = outgoing_tx.send(response);
            Ok(Refresh::None)
        }
        ClientMessage::ShellClose { request_id, agent } => {
            let agents = Arc::clone(agents);
            let outgoing_tx = outgoing_tx.clone();
            tokio::spawn(async move {
                let response = match shell_close(&agents, &agent).await {
                    Ok(()) => ServerMessage::ShellClosed { request_id },
                    Err(error) => ServerMessage::ShellRequestFailed {
                        request_id,
                        reason: format!("{error:#}"),
                    },
                };
                let _ = outgoing_tx.send(response);
            });
            Ok(Refresh::None)
        }
        ClientMessage::GitTransportRegister => {
            agents.git_transport.register(outgoing_tx.clone()).await;
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
                        (true, format!("GitHub secrets installed{persistence}"))
                    } else {
                        (true, format!("platform secrets installed{persistence}"))
                    }
                }
                Err(error) => (false, format!("{error:#}")),
            };
            let _ = outgoing_tx.send(ServerMessage::PlatformStatus { running, detail });
            Ok(Refresh::None)
        }
        ClientMessage::PrCommand {
            request_id,
            agent_id,
            command,
        } => {
            let result = async {
                let raw_agent_id =
                    agent_id.ok_or_else(|| anyhow::anyhow!("missing --agent or RHO_AGENT_ID"))?;
                let agent_id = agents.resolve_display_agent_id(&raw_agent_id)?;
                match command {
                    rho_ui_proto::PrCommand::Create {
                        owner,
                        repo,
                        head,
                        base,
                        title,
                        body,
                        review_bots,
                    } => agents
                        .pr_monitor
                        .create_and_subscribe(
                            agent_id,
                            rho_pr_monitor::CreatePullRequest {
                                owner,
                                repo,
                                head,
                                base,
                                title,
                                body,
                                approved_review_bots: review_bots,
                            },
                        )
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_ui_proto::PrCommand::Subscribe {
                        url,
                        replay_existing,
                        review_bots,
                    } => agents
                        .pr_monitor
                        .subscribe(agent_id, &url, replay_existing, review_bots)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_ui_proto::PrCommand::Status { url } => agents
                        .pr_monitor
                        .status(agent_id, &url)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_ui_proto::PrCommand::List => agents
                        .pr_monitor
                        .list(agent_id)
                        .map(|output| (output, Vec::new())),
                    rho_ui_proto::PrCommand::Stop { url } => agents
                        .pr_monitor
                        .stop(agent_id, &url)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_ui_proto::PrCommand::Comment { url, reply, body } => agents
                        .pr_monitor
                        .comment(agent_id, &url, &body, reply.as_deref())
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_ui_proto::PrCommand::Rerun { url, run_id } => agents
                        .pr_monitor
                        .rerun(agent_id, &url, run_id)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_ui_proto::PrCommand::Logs { url, run_id } => agents
                        .pr_monitor
                        .logs(agent_id, &url, run_id)
                        .await
                        .map(|data| (format!("downloaded logs for run {run_id}"), data.to_vec())),
                }
            }
            .await;
            let (output, data, is_error) = match result {
                Ok((output, data)) => (output, data, false),
                Err(error) => (format!("{error:#}"), Vec::new(), true),
            };
            let _ = outgoing_tx.send(ServerMessage::PrCommandResult {
                request_id,
                output,
                data,
                is_error,
            });
            Ok(Refresh::None)
        }
        ClientMessage::Subscribe => Ok(Refresh::None),
        ClientMessage::NewAgent {
            workstream,
            role,
            start,
            content,
        } => {
            // Without a workstream to join, the agent founds its own,
            // provisionally named after its first message until the
            // generated title lands.
            let (workstream, founded) = match workstream {
                Some(workstream_id) => (workstream_id, None),
                None => {
                    let name = provisional_workstream_name(content.as_deref());
                    let workstream = agents.create_workstream(name).await;
                    let _ = outgoing_tx.send(ServerMessage::WorkstreamCreated {
                        workstream: workstream.clone(),
                    });
                    (
                        workstream.workstream_id,
                        Some((workstream.workstream_id, workstream.name)),
                    )
                }
            };
            // Subscription and the AgentCreated announcement ride the pool's
            // creation broadcast (all connections, including this one).
            let (agent_id, agent) = agents.create(workstream, role, start).await?;
            if let Some(content) = content {
                let text = text_content(&content);
                // The agent is fresh, so the lanes are equivalent here.
                agent.send_user_message(text.clone(), MessageDelivery::NextRequest);
                agents
                    .maybe_generate_title(agent_id, text, founded)
                    .await;
            }
            Ok(Refresh::Ready)
        }
        ClientMessage::ProjectSet {
            path,
            name,
            description,
        } => {
            agents.set_project(path, name, description).await?;
            Ok(Refresh::Ready)
        }
        ClientMessage::ProjectRemove { path } => {
            agents.remove_project(path).await?;
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
                if agent_streams.is_none() {
                    subscribe_agent(agent_id, agent.clone(), outgoing_tx.clone());
                }
            }
            if let Some(agent_streams) = agent_streams
                && let Err(error) = agent_streams.ensure(agent_id, agent).await
            {
                let _ = outgoing_tx.send(ServerMessage::Error {
                    message: error.to_string(),
                });
            }
            let _ = outgoing_tx.send(ServerMessage::AgentLoaded { agent_id });
            Ok(Refresh::None)
        }
        ClientMessage::AgentStreamFocus { agent_id } => {
            if let Some(agent_streams) = agent_streams {
                agent_streams.set_focus(agent_id);
            }
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
                write.record_agent_user_message(rho_core::UnixMs::now(), agent_id, &text);
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
                .maybe_generate_title(agent_id, text, None)
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
        ClientMessage::AgentMove { agent_id, target } => {
            agents.move_agent(agent_id, target).await?;
            Ok(Refresh::Ready)
        }
        ClientMessage::AgentLabel {
            agent_id,
            label,
            add,
        } => {
            agents.agent_label(agent_id, label, add).await?;
            Ok(Refresh::Ready)
        }
        ClientMessage::RenameAgent { agent_id, name } => {
            agents.rename_agent(agent_id, name).await?;
            Ok(Refresh::Ready)
        }
        ClientMessage::ChangePromptCacheKey { agent_id } => {
            let agent = agents
                .get(agent_id)
                .await
                .ok_or_else(|| anyhow::anyhow!("agent is not loaded: {agent_id:?}"))?;
            agent.change_prompt_cache_key()?;
            Ok(Refresh::None)
        }
        ClientMessage::WorkstreamRename {
            workstream_id,
            name,
        } => {
            agents.rename_workstream(workstream_id, name).await?;
            Ok(Refresh::Ready)
        }
        ClientMessage::WorkstreamLabel {
            workstream_id,
            label,
            add,
        } => {
            agents.workstream_label(workstream_id, label, add).await?;
            Ok(Refresh::Ready)
        }
        ClientMessage::ViewConfigSet { data } => {
            let mut write = agents.db.write().await;
            write.set_view_config(data);
            write.commit();
            Ok(Refresh::None)
        }
        ClientMessage::SetAgentDisposition {
            agent_id,
            disposition,
        } => {
            agents.set_disposition(agent_id, disposition).await;
            if disposition != AgentDisposition::Hidden
                && let Some(agent_streams) = agent_streams
                && let Some(agent) = agents.get(agent_id).await
                && let Err(error) = agent_streams.ensure(agent_id, agent).await
            {
                let _ = outgoing_tx.send(ServerMessage::Error {
                    message: error.to_string(),
                });
            }
            // Hidden changes what the rail folds, which clients read off
            // summaries; attention alone travels on its own broadcast.
            if disposition == AgentDisposition::Hidden {
                Ok(Refresh::Ready)
            } else {
                Ok(Refresh::None)
            }
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
        ClientMessage::IrohTrustInMemory { endpoint_id } => {
            let auth = agents
                .iroh_auth
                .as_ref()
                .context("daemon is not listening over iroh (start it with --iroh)")?;
            let endpoint_id = endpoint_id
                .parse::<iroh::EndpointId>()
                .context("invalid iroh client endpoint id")?;
            auth.trust_in_memory(endpoint_id).await;
            let _ = outgoing_tx.send(ServerMessage::IrohApproved {
                endpoint_id: endpoint_id.to_string(),
            });
            Ok(Refresh::None)
        }
        ClientMessage::IrohRevoke { endpoint_id } => {
            let auth = agents
                .iroh_auth
                .as_ref()
                .context("daemon is not listening over iroh (start it with --iroh)")?;
            let endpoint_id = endpoint_id
                .parse::<iroh::EndpointId>()
                .context("invalid iroh client endpoint id")?;
            anyhow::ensure!(
                auth.revoke(endpoint_id).await,
                "iroh client is not enrolled"
            );
            let _ = outgoing_tx.send(ServerMessage::IrohRevoked {
                endpoint_id: endpoint_id.to_string(),
            });
            Ok(Refresh::None)
        }
        // Only valid as a stream's first frame (see `serve_connection_io`);
        // inside a UI session it is a protocol error.
        ClientMessage::ChannelOpen { .. } => {
            anyhow::bail!("ChannelOpen must be the first frame on a dedicated stream")
        }
        ClientMessage::DiffSnapshot { .. }
        | ClientMessage::TerminalCreate { .. }
        | ClientMessage::TerminalAttach { .. }
        | ClientMessage::TerminalList { .. }
        | ClientMessage::ShellAttach { .. }
        | ClientMessage::GitTransportRequest { .. }
        | ClientMessage::GitTransportProvide { .. }
        | ClientMessage::GitTransportQuery { .. } => {
            anyhow::bail!("channel messages must be the first frame on a dedicated stream")
        }
    }
}

/// Attaches a dedicated Comint-style shell stream. The daemon retains the
/// process when this client detaches.
async fn serve_shell<R, W>(
    agents: Arc<AgentRegistry>,
    mut reader: R,
    mut writer: W,
    agent: String,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let client = shell_attach(&agents, &agent).await;
    let shell::ShellClient {
        mut frames,
        mut exit,
        submit,
        control,
    } = match client {
        Ok(client) => client,
        Err(error) => {
            let _ = write_frame(
                &mut writer,
                &ServerMessage::ShellAttachRefused {
                    reason: format!("{error:#}"),
                },
            )
            .await;
            return Err(error);
        }
    };
    write_frame(&mut writer, &ServerMessage::ShellOpened).await?;
    let (accepted_tx, mut accepted_rx) = tokio::sync::mpsc::channel(shell::SUBMIT_QUEUE);

    let mut writer_task = tokio::spawn(async move {
        loop {
            while let Ok((submission, execution)) = accepted_rx.try_recv() {
                if write_frame(
                    &mut writer,
                    &rho_ui_proto::shell::ShellServerFrame::Accepted {
                        submission,
                        execution,
                    },
                )
                .await
                .is_err()
                {
                    return;
                }
            }
            let final_state = { exit.borrow_and_update().clone() };
            if let Some(final_state) = final_state {
                let snapshot = rho_ui_proto::shell::ShellServerFrame::Snapshot {
                    state: final_state.state.clone(),
                };
                if write_frame(&mut writer, &snapshot).await.is_ok() {
                    let _ = write_frame(
                        &mut writer,
                        &rho_ui_proto::shell::ShellServerFrame::Exited {
                            status: final_state.status,
                        },
                    )
                    .await;
                }
                break;
            }
            tokio::select! {
                biased;
                changed = exit.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                accepted = accepted_rx.recv() => match accepted {
                    Some((submission, execution)) => {
                        if write_frame(
                            &mut writer,
                            &rho_ui_proto::shell::ShellServerFrame::Accepted {
                                submission,
                                execution,
                            },
                        )
                        .await
                        .is_err()
                        {
                            break;
                        }
                    }
                    None => break,
                },
                frame = frames.recv() => match frame {
                    Some(frame) => {
                        if write_frame(&mut writer, &frame).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
        let _ = tokio::io::AsyncWriteExt::shutdown(&mut writer).await;
    });
    let result = loop {
        tokio::select! {
            _ = &mut writer_task => break Ok(()),
            frame = read_frame::<_, rho_ui_proto::shell::ShellClientFrame>(&mut reader) => {
                use rho_ui_proto::shell::{ShellClientFrame, command_fits};
                match frame {
                    Ok(ShellClientFrame::Submit { submission, command }) => {
                        if !command_fits(&command) {
                            break Err(anyhow::anyhow!("shell command exceeds the input limit"));
                        }
                        match submit.try_send(command) {
                            Ok(execution) => {
                                if accepted_tx.send((submission, execution)).await.is_err() {
                                    break Ok(());
                                }
                            }
                            Err(shell::ShellSubmitError::Full) => {
                                break Err(anyhow::anyhow!("shell command queue is full"));
                            }
                            Err(shell::ShellSubmitError::Closed) => break Ok(()),
                            Err(shell::ShellSubmitError::Exhausted) => {
                                break Err(anyhow::anyhow!("shell execution ids exhausted"));
                            }
                            Err(shell::ShellSubmitError::TooLarge) => {
                                break Err(anyhow::anyhow!("shell command exceeds the input limit"));
                            }
                        }
                    }
                    Ok(ShellClientFrame::Interrupt) => {
                        if control.send(shell::ShellControl::Interrupt).await.is_err() {
                            break Ok(());
                        }
                    }
                    Ok(ShellClientFrame::Eof) => {
                        if control.send(shell::ShellControl::Eof).await.is_err() {
                            break Ok(());
                        }
                    }
                    Ok(ShellClientFrame::PagerAction {
                        execution,
                        pager,
                        page,
                        action,
                    }) => {
                        if control
                            .pager_action(execution, pager, page, action)
                            .await
                            .is_err()
                        {
                            break Ok(());
                        }
                    }
                    Err(_) => break Ok(()),
                }
            }
        }
    };
    if !writer_task.is_finished() {
        writer_task.abort();
    }
    result
}

async fn shell_start(
    agents: &Arc<AgentRegistry>,
    agent: &str,
) -> anyhow::Result<()> {
    let agent_id = agents.resolve_display_agent_id(agent)?;
    let record = agents.db.read().get_agent(agent_id);
    shell::ensure_supported_workdirs(&record.workdirs)?;
    let view = agents
        .pool
        .materialize_view(&record.workdirs)
        .await
        .context("materialize agent view")?;
    agents
        .shells
        .start(
            agent_id,
            shell::ShellSpawn {
                view,
                program: rho_shell_program(),
                args: Vec::new(),
                pager_program: rho_pager_program(),
            },
        )
        .await
}

async fn shell_attach(
    agents: &Arc<AgentRegistry>,
    agent: &str,
) -> anyhow::Result<shell::ShellClient> {
    let agent_id = agents.resolve_display_agent_id(agent)?;
    agents.shells.attach(agent_id).await
}

async fn shell_list(
    agents: &Arc<AgentRegistry>,
    agent: Option<&str>,
) -> anyhow::Result<Vec<rho_ui_proto::shell::ShellInfo>> {
    let filter = agent
        .map(|agent| agents.resolve_display_agent_id(agent))
        .transpose()?;
    Ok(agents
        .shells
        .list()
        .await
        .into_iter()
        .filter(|entry| filter.is_none_or(|agent_id| entry.agent_id == agent_id))
        .map(|entry| rho_ui_proto::shell::ShellInfo {
            agent: entry.agent_id.encoded(),
            clients: entry.clients as u32,
        })
        .collect())
}

async fn shell_close(agents: &Arc<AgentRegistry>, agent: &str) -> anyhow::Result<()> {
    let agent_id = agents.resolve_display_agent_id(agent)?;
    agents.shells.close(agent_id).await
}

fn rho_shell_program() -> std::ffi::OsString {
    if let Some(program) = std::env::var_os("RHO_SHELL") {
        return program;
    }
    if let Ok(current) = std::env::current_exe()
        && let Some(directory) = current.parent()
    {
        let sibling = directory.join("rho-shell");
        if sibling.is_file() {
            return sibling.into_os_string();
        }
    }
    "rho-shell".into()
}

fn rho_pager_program() -> std::ffi::OsString {
    if let Some(program) = std::env::var_os("RHO_PAGER") {
        return program;
    }
    if let Ok(current) = std::env::current_exe()
        && let Some(directory) = current.parent()
    {
        let sibling = directory.join("rho-pager");
        if sibling.is_file() {
            return sibling.into_os_string();
        }
    }
    "rho-pager".into()
}

/// Persists one jj working-copy snapshot and serves its bounded parent-side
/// manifest on a dedicated stream, avoiding control-session head-of-line
/// blocking.
async fn serve_diff_snapshot<W>(
    agents: Arc<AgentRegistry>,
    mut writer: W,
    workspace: WorkspaceInfo,
    known_commit_id: Option<String>,
    include_paths: Vec<Utf8PathBuf>,
) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    static DIFF_LOADS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let _permit = DIFF_LOADS.acquire().await.context("diff loader closed")?;
        let workspace = agents.pool.open_workspace(&workspace).await?;
        workspace
            .diff_snapshot(known_commit_id.as_deref(), &include_paths)
            .await
    })
    .await
    .context("diff snapshot timed out after 30 seconds")
    .and_then(|result| result);
    match result {
        Ok(Some(snapshot)) => {
            write_frame(&mut writer, &ServerMessage::DiffSnapshot { snapshot }).await
        }
        Ok(None) => {
            write_frame(
                &mut writer,
                &ServerMessage::DiffUnchanged {
                    commit_id: known_commit_id.unwrap_or_default(),
                },
            )
            .await
        }
        Err(error) => {
            write_frame(
                &mut writer,
                &ServerMessage::DiffRefused {
                    reason: format!("{error:#}"),
                },
            )
            .await
        }
    }
}

/// How a terminal stream's first frame opens its terminal.
enum TerminalOpenKind {
    Create { attach: bool },
    Attach,
}

/// Serves a stream dedicated to one daemon-owned terminal: spawns or attaches
/// (per [`TerminalOpenKind`]), replies `TerminalOpened`, then pumps
/// [`rho_ui_proto::term`] frames until either side closes. Closing only
/// detaches; the terminal keeps running. A headless create replies and
/// returns without attaching.
#[expect(clippy::too_many_arguments)]
async fn serve_terminal<R, W>(
    agents: Arc<AgentRegistry>,
    mut reader: R,
    mut writer: W,
    agent: String,
    terminal_id: u64,
    open: TerminalOpenKind,
    cols: u16,
    rows: u16,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let create = matches!(open, TerminalOpenKind::Create { .. });
    let attached = terminal_attach(&agents, &agent, terminal_id, create, cols, rows).await;
    let client = match attached {
        Ok(attached) => attached,
        Err(error) => {
            let _ = write_frame(
                &mut writer,
                &ServerMessage::TerminalRefused {
                    reason: format!("{error:#}"),
                },
            )
            .await;
            return Err(error);
        }
    };
    write_frame(&mut writer, &ServerMessage::TerminalOpened { terminal_id }).await?;
    if matches!(open, TerminalOpenKind::Create { attach: false }) {
        // Headless create: the terminal keeps running with no clients.
        return Ok(());
    }

    let terminal::TerminalClient { mut frames, input } = client;
    let writer_task = tokio::spawn(async move {
        while let Some(frame) = frames.recv().await {
            if write_frame(&mut writer, &frame).await.is_err() {
                break;
            }
        }
        // Half-close so a client blocked on reads notices the terminal is
        // gone even if it never sends input.
        let _ = tokio::io::AsyncWriteExt::shutdown(&mut writer).await;
    });
    let result = loop {
        use rho_ui_proto::term::TermClientFrame;
        let client_input = match read_frame::<_, TermClientFrame>(&mut reader).await {
            Ok(TermClientFrame::Input(bytes)) => terminal::ClientInput::Bytes(bytes),
            Ok(TermClientFrame::Resize { cols, rows }) => {
                terminal::ClientInput::Resize { cols, rows }
            }
            Ok(TermClientFrame::Keystroke(keystroke)) => {
                terminal::ClientInput::Keystroke(keystroke)
            }
            Ok(TermClientFrame::Paste(text)) => terminal::ClientInput::Paste(text),
            Ok(TermClientFrame::Scroll {
                lines,
                col,
                row,
                ctrl,
                alt,
                shift,
            }) => terminal::ClientInput::Scroll {
                lines,
                col,
                row,
                ctrl,
                alt,
                shift,
            },
            Err(_) => break Ok(()),
        };
        let _ = input.send(client_input);
    };
    writer_task.abort();
    result
}

/// Resolves the agent, then attaches to a running terminal — or, for
/// `create`, builds the spawn spec for its default shell inside its view and
/// spawns a fresh one.
async fn terminal_attach(
    agents: &Arc<AgentRegistry>,
    agent: &str,
    terminal_id: u64,
    create: bool,
    cols: u16,
    rows: u16,
) -> anyhow::Result<terminal::TerminalClient> {
    let agent_id = agents.resolve_display_agent_id(agent)?;
    if !create {
        return agents
            .terminals
            .attach(agent_id, terminal_id, cols, rows)
            .await;
    }
    let record = agents.db.read().get_agent(agent_id);
    anyhow::ensure!(
        !record
            .workdirs
            .iter()
            .any(|workdir| matches!(workdir, WorkspaceInfo::Sandbox { .. })),
        "sandboxed agents have no terminals yet"
    );
    let view = agents
        .pool
        .materialize_view(&record.workdirs)
        .await
        .context("materialize agent view")?;
    let shell = agents
        .user_environment
        .get("SHELL")
        .and_then(|shell| shell.to_str())
        .unwrap_or("bash")
        .to_owned();
    agents
        .terminals
        .create(
            agent_id,
            terminal_id,
            cols,
            rows,
            terminal::TerminalSpawn { view, shell },
        )
        .await
}

/// Answers a [`ClientMessage::TerminalList`] one-shot stream.
async fn serve_terminal_list<W>(
    agents: Arc<AgentRegistry>,
    mut writer: W,
    agent: Option<String>,
) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let filter = match &agent {
        Some(agent) => match agents.resolve_display_agent_id(agent) {
            Ok(agent_id) => Some(agent_id),
            Err(error) => {
                let _ = write_frame(
                    &mut writer,
                    &ServerMessage::TerminalRefused {
                        reason: format!("{error:#}"),
                    },
                )
                .await;
                return Err(error);
            }
        },
        None => None,
    };
    let terminals = agents
        .terminals
        .list()
        .await
        .into_iter()
        .filter(|entry| filter.is_none_or(|agent_id| entry.agent_id == agent_id))
        .map(|entry| rho_ui_proto::term::TerminalInfo {
            agent: entry.agent_id.encoded(),
            terminal_id: entry.terminal_id,
            title: entry.title.unwrap_or_default(),
            cols: entry.cols,
            rows: entry.rows,
            clients: entry.clients as u32,
        })
        .collect();
    write_frame(&mut writer, &ServerMessage::TerminalList { terminals }).await
}

/// Serves a stream dedicated to one zed channel: binds a headless project
/// session, replies `ChannelOpened { root }`, then pumps raw envelope frames
/// both ways until either side closes. Stream teardown is session teardown.
async fn serve_zed_channel<R, W>(
    agents: Arc<AgentRegistry>,
    mut reader: R,
    mut writer: W,
    workspace: WorkspaceInfo,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let workspace = match agents.pool.open_workspace(&workspace).await {
        Ok(workspace) => workspace,
        Err(error) => {
            let _ = write_frame(
                &mut writer,
                &ServerMessage::ChannelClosed {
                    reason: format!("{error:#}"),
                },
            )
            .await;
            return Err(error);
        }
    };
    let root = workspace.checkout().to_owned();

    let (to_host_tx, to_host_rx) = futures::channel::mpsc::unbounded();
    let (from_host_tx, mut from_host_rx) = futures::channel::mpsc::unbounded();
    let session_id = agents
        .zed_host()
        .open_session(rho_zed_host::SessionStreams {
            incoming: to_host_rx,
            outgoing: from_host_tx,
        });

    write_frame(&mut writer, &ServerMessage::ChannelOpened { root }).await?;

    let writer_task = tokio::spawn(async move {
        while let Some(payload) = from_host_rx.next().await {
            if rho_ui_proto::write_raw_frame(&mut writer, &payload)
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let result = loop {
        match rho_ui_proto::read_raw_frame(&mut reader).await {
            Ok(Some(payload)) => {
                if to_host_tx.unbounded_send(payload).is_err() {
                    break Ok(());
                }
            }
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
        }
    };
    agents.zed_host().close_session(session_id);
    writer_task.abort();
    result
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
/// The name a self-founded workstream starts under: the first line of the
/// agent's first message, truncated. The generated title replaces it
/// (matching by this exact string) once it lands.
fn provisional_workstream_name(content: Option<&[ContentPart]>) -> String {
    let text = content.map(text_content).unwrap_or_default();
    let line = text.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return "new task".to_owned();
    }
    match line.char_indices().nth(48) {
        Some((index, _)) => format!("{}…", &line[..index]),
        None => line.to_owned(),
    }
}

fn validate_label(label: &str) -> anyhow::Result<()> {
    if label.trim().is_empty() {
        anyhow::bail!("label cannot be empty");
    }
    Ok(())
}

/// `agent_id` and every transitive spawn descendant, so workstream moves
/// never leave a subtree straddling workstreams.
fn spawn_subtree(
    agents: &[(AgentId, rho_agent::db::AgentRecord)],
    agent_id: AgentId,
) -> Vec<AgentId> {
    let mut members = vec![agent_id];
    let mut frontier = vec![agent_id];
    while let Some(parent) = frontier.pop() {
        for (child, record) in agents {
            if record.parent_agent == Some(parent) && !members.contains(child) {
                members.push(*child);
                frontier.push(*child);
            }
        }
    }
    members
}

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
    use std::ffi::{OsStr, OsString};
    use std::sync::Arc;

    use rho_agent::{AgentState, AgentStateKind, InputQueues};
    use rho_core::{
        ContentPart, ContextBlock, InferenceResponseItem, MessagePhase, UnknownProviderSpecificData,
    };
    use rho_db::RhoDb;
    use rho_ui_proto::ServerMessage;

    use super::{
        GitProviderClaim, GitTransportBroker, configure_octo_git_transport,
        inference_response_count, latest_final_response, load_or_create_iroh_secret,
    };

    fn environment_value<'a>(
        environment: &'a [(OsString, OsString)],
        name: &str,
    ) -> Option<&'a OsStr> {
        environment
            .iter()
            .find_map(|(key, value)| (key == name).then_some(value.as_os_str()))
    }

    #[test]
    fn ambient_octo_transport_appends_git_config_without_replacing_it() {
        let mut environment = vec![
            ("GIT_CONFIG_COUNT".into(), "1".into()),
            ("GIT_CONFIG_KEY_0".into(), "user.name".into()),
            ("GIT_CONFIG_VALUE_0".into(), "Example".into()),
        ];
        configure_octo_git_transport(&mut environment).unwrap();

        assert_eq!(
            environment_value(&environment, "GIT_CONFIG_COUNT"),
            Some(OsStr::new("5"))
        );
        assert_eq!(
            environment_value(&environment, "GIT_CONFIG_KEY_0"),
            Some(OsStr::new("user.name"))
        );
        assert_eq!(
            environment_value(&environment, "GIT_CONFIG_VALUE_0"),
            Some(OsStr::new("Example"))
        );
        assert_eq!(
            environment_value(&environment, "GIT_CONFIG_KEY_1"),
            Some(OsStr::new("url.octo://github.com/.insteadOf"))
        );
        assert_eq!(
            environment_value(&environment, "GIT_CONFIG_VALUE_1"),
            Some(OsStr::new("git@github.com:"))
        );
        assert_eq!(
            environment_value(&environment, "GIT_CONFIG_VALUE_2"),
            Some(OsStr::new("ssh://git@github.com/"))
        );
        assert_eq!(
            environment_value(&environment, "GIT_CONFIG_KEY_3"),
            Some(OsStr::new("url.octo://git@git.sr.ht/.insteadOf"))
        );
        assert_eq!(
            environment_value(&environment, "GIT_CONFIG_VALUE_3"),
            Some(OsStr::new("git@git.sr.ht:"))
        );
        assert_eq!(
            environment_value(&environment, "GIT_CONFIG_VALUE_4"),
            Some(OsStr::new("ssh://git@git.sr.ht/"))
        );
    }

    #[tokio::test]
    async fn iroh_secret_is_persisted_in_database() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));

        let first = load_or_create_iroh_secret(&db).await.unwrap();
        let second = load_or_create_iroh_secret(&db).await.unwrap();

        assert_eq!(first.public(), second.public());
    }

    #[tokio::test]
    async fn git_transport_broker_first_claim_wins() {
        let broker = Arc::new(GitTransportBroker::default());
        let (first_tx, mut first_rx) = tokio::sync::mpsc::unbounded_channel();
        let (second_tx, mut second_rx) = tokio::sync::mpsc::unbounded_channel();
        broker.register(first_tx).await;
        broker.register(second_tx).await;
        let request = rho_ui_proto::GitTransportRequest {
            host: "git.example".to_owned(),
            port: 22,
            user: "git".to_owned(),
            repository: "team/repo.git".to_owned(),
            service: rho_ui_proto::GitService::ReceivePack,
            planned_refs: Some(vec!["refs/heads/main".to_owned()]),
        };
        let waiting = {
            let broker = broker.clone();
            tokio::spawn(async move { broker.request(request).await })
        };
        let (request_id, first_provider) = match first_rx.recv().await.unwrap() {
            ServerMessage::GitTransportRequested {
                request_id,
                provider_id,
                ..
            } => (request_id, provider_id),
            message => panic!("unexpected provider message: {message:?}"),
        };
        let second_provider = match second_rx.recv().await.unwrap() {
            ServerMessage::GitTransportRequested {
                request_id: second_request,
                provider_id,
                ..
            } => {
                assert_eq!(second_request, request_id);
                provider_id
            }
            message => panic!("unexpected provider message: {message:?}"),
        };
        assert!(matches!(
            broker
                .claim(request_id, first_provider, false)
                .await
                .unwrap(),
            GitProviderClaim::Done
        ));
        let response = match broker
            .claim(request_id, second_provider, true)
            .await
            .unwrap()
        {
            GitProviderClaim::Selected(response) => response,
            GitProviderClaim::Done => panic!("second provider did not win"),
        };
        let (provided, _peer) = tokio::io::duplex(64);
        assert!(response.send(Ok(Box::new(provided))).is_ok());
        waiting.await.unwrap().unwrap();
        assert!(matches!(
            first_rx.recv().await,
            Some(ServerMessage::GitTransportDone {
                request_id: done_request
            }) if done_request == request_id
        ));
    }

    #[tokio::test]
    async fn git_transport_broker_rejects_without_registered_clients() {
        let result = GitTransportBroker::default()
            .request(rho_ui_proto::GitTransportRequest {
                host: "git.example".to_owned(),
                port: 22,
                user: "git".to_owned(),
                repository: "team/repo.git".to_owned(),
                service: rho_ui_proto::GitService::UploadPack,
                planned_refs: None,
            })
            .await;
        let error = match result {
            Ok(_) => panic!("request unexpectedly found a provider"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("no GUI clients are registered"));
    }

    #[tokio::test]
    async fn git_transport_broker_times_out_and_notifies_clients() {
        let broker = Arc::new(GitTransportBroker::default());
        let (provider_tx, mut provider_rx) = tokio::sync::mpsc::unbounded_channel();
        broker.register(provider_tx).await;
        let waiting = {
            let broker = broker.clone();
            tokio::spawn(async move {
                broker
                    .request_with_timeout(
                        rho_ui_proto::GitTransportRequest {
                            host: "git.example".to_owned(),
                            port: 22,
                            user: "git".to_owned(),
                            repository: "team/repo.git".to_owned(),
                            service: rho_ui_proto::GitService::UploadPack,
                            planned_refs: None,
                        },
                        std::time::Duration::from_millis(10),
                    )
                    .await
            })
        };
        let request_id = match provider_rx.recv().await.unwrap() {
            ServerMessage::GitTransportRequested { request_id, .. } => request_id,
            message => panic!("unexpected provider message: {message:?}"),
        };
        let error = match waiting.await.unwrap() {
            Ok(_) => panic!("request unexpectedly received a provider"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("within 60 seconds"));
        assert!(matches!(
            provider_rx.recv().await,
            Some(ServerMessage::GitTransportDone {
                request_id: done_request
            }) if done_request == request_id
        ));
    }

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
