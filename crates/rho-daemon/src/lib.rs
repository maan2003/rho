use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use futures::{FutureExt as _, StreamExt as _};
use rho_agent::db::{
    AgentDisposition, AgentId, AgentReadTxnExt as _, AgentRole, AgentRuntime, AgentUsageModel,
    AgentWriteTxnExt as _, PendingPresentationWorkstream, QuotaModel, QuotaObservationRecord,
    QuotaProvider, WorkstreamId,
};
use rho_agent::pool::{AgentPool, RunningAgent};
use rho_agent::{AgentStateKind, MessageDelivery};
use rho_core::{ContentPart, text_content};
use rho_db::RhoDb;
use rho_inference::{Inference, InferenceAuth};
use rho_ui_proto::remote::AgentRemoteEncoder;
use rho_ui_proto::server::{Server, ServerConnection};
use rho_ui_proto::{
    AgentUsageBucket as UiAgentUsageBucket, AgentUsageSeries, AuthState, ClientMessage, JoinTarget,
    LandLeaseHolder, LandStatus, McpAgentToolRequest, McpAgentToolResponse, QuotaPoint,
    QuotaSeries, QuotaSummary, ServerMessage, StartMode, UiAgentSummary, UiAttention, UiProject,
    UiTurnReport, UiWorkstream, WorkspaceInfo, WorkstreamTarget, read_frame, write_frame,
};
use tokio::sync::{Mutex, Mutex as TokioMutex, OwnedMutexGuard, broadcast, mpsc, oneshot, watch};

mod agent_ui;
pub mod debug;
mod iris;
mod realtime;
mod shell;
mod terminal;
mod workspace_channel;

/// FDNAME under which messaging-platform secrets live in the systemd fd store.
const PLATFORM_SECRETS_FD_STORE_NAME: &str = "platform-secrets";
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
    let router = octo_server::router(token_provider, github_api_url);
    tokio::spawn(async move {
        if let Err(error) = octo_server::serve(listener, router).await {
            tracing::error!(%error, "octo server stopped");
        }
    });
    Ok(())
}

/// Re-exported so daemon entry points can set up the user+mount namespace
/// before the async runtime starts (see
/// [`rho_workspaces::init_daemon_namespace`]).
pub use rho_workspaces::{PathOverrides, init_daemon_namespace};

const EMBEDDED_DIRENV_PATH_BEFORE: Option<&str> = option_env!("RHO_DIRENV_PATH_BEFORE");
const FIND_DENY_ROOTS_ENV: &str = "FIND_DENY_ROOTS";

fn find_deny_roots() -> OsString {
    let home = dirs::home_dir().expect("home directory must be available");
    std::env::join_paths([PathBuf::from("/"), PathBuf::from("/nix/store"), home])
        .expect("protected root paths must not contain a path separator")
}

/// Nix packages can embed a directory for direnv's post-`use_flake` PATH hook.
/// This must run before the Tokio runtime starts, because mutating the process
/// environment is not thread-safe.
pub fn configure_embedded_environment() {
    if let Some(path) = EMBEDDED_DIRENV_PATH_BEFORE {
        // SAFETY: called by rho-daemon's main before it creates the Tokio runtime.
        unsafe { std::env::set_var("RHO_DIRENV_PATH_BEFORE", path) };
    }
    // SAFETY: called by rho-daemon's main before it creates the Tokio runtime.
    unsafe { std::env::set_var(FIND_DENY_ROOTS_ENV, find_deny_roots()) };
}

#[derive(Clone, Debug, clap::Args)]
pub struct DaemonArgs {
    #[arg(long = "socket-path")]
    pub socket_path: Option<PathBuf>,
    /// Also listen for UI clients (including the web UI) over iroh
    /// (relay-backed). Remote clients must be enrolled once via
    /// `rho iroh approve <code>` on this machine.
    #[arg(long = "iroh")]
    pub iroh: bool,
    #[arg(long = "extra-before-path", env = "RHO_EXTRA_BEFORE_PATH")]
    pub extra_before_path: Option<OsString>,
    #[arg(long = "extra-after-path", env = "RHO_EXTRA_AFTER_PATH")]
    pub extra_after_path: Option<OsString>,
    /// Write a Dial9 CPU trace on shutdown (requires a frame-pointer build).
    #[arg(long, value_name = "FILE")]
    pub cpu_profile: Option<PathBuf>,
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
    // The daemon's own cwd must never matter: agents each carry their own
    // working directory. Park the process somewhere empty and read-only so
    // any code still depending on process cwd fails loudly.
    let _ = std::env::set_current_dir("/var/empty").or_else(|_| std::env::set_current_dir("/"));

    let default_socket_path = default_socket_path()?;
    let socket_path = args
        .socket_path
        .unwrap_or_else(|| default_socket_path.clone());
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).context("create socket directory")?;
    }
    let _ = std::fs::remove_file(&socket_path);
    let server = Server::bind(&socket_path).context("bind rho daemon socket")?;
    let platform_secrets = PlatformSecrets::from_fd_store();
    // Octo's fixed socket belongs to the default daemon. Test and other
    // alternate-socket daemons must not unlink it out from under that daemon.
    if socket_path == default_socket_path {
        let octo_socket_path = octo_types::socket_path()?;
        spawn_octo_server(&octo_socket_path, platform_secrets.clone())?;
    }
    let mut user_environment = login_environment()?;
    if let Some(path) = EMBEDDED_DIRENV_PATH_BEFORE {
        user_environment.push(("RHO_DIRENV_PATH_BEFORE".into(), path.into()));
    }
    user_environment.push((FIND_DENY_ROOTS_ENV.into(), find_deny_roots()));
    configure_octo_git_transport(&mut user_environment)?;
    let user_environment = rho_workspaces::UserEnvironment::new(user_environment);

    let db = RhoDb::open(default_db_path()?);
    let stored_auth_name = db.read().default_auth_namespace();
    let default_auth_name = match stored_auth_name {
        Some(name) => name,
        None => {
            let name = "default".to_owned();
            let mut write = db.write().await;
            write.set_default_auth_namespace(name.clone());
            write.commit();
            name
        }
    };
    let active_auth_name = default_auth_name;
    let inference = Inference::new(InferenceAuth::named(&active_auth_name)?);
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
    let quota_path_overrides = path_overrides.clone();
    let iroh = if args.iroh {
        let (listener, auth) =
            rho_rpc::AuthenticatedIrohListener::bind(db.clone(), rho_ui_proto::IROH_ALPN).await?;
        eprintln!("rho daemon iroh endpoint: {}", listener.endpoint_id());
        Some((listener, auth))
    } else {
        None
    };

    let iroh_auth = iroh.as_ref().map(|(_, auth)| auth.clone());
    let agents = Arc::new(
        AgentRegistry::new(
            db,
            inference,
            path_overrides,
            user_environment,
            platform_secrets,
            active_auth_name,
        )
        .await?,
    );
    agents.install_iris_tool_host();
    spawn_presentation_projection(Arc::clone(&agents));
    spawn_turn_report_projection(Arc::clone(&agents));
    spawn_chatgpt_quota_poller(Arc::clone(&agents));
    let quota_environment = agents.user_environment.clone();
    spawn_claude_quota_recorder(
        rho_claude_usage::spawn_poller(
            move || {
                let mut command = tokio::process::Command::new("claude");
                quota_environment.apply(&mut command);
                let path = quota_environment
                    .get("PATH")
                    .context("user environment has no PATH")?;
                command.env("PATH", quota_path_overrides.add_to(path));
                Ok(command)
            },
            default_db_path()?.with_file_name("claude-quota-probe"),
        ),
        agents.db.clone(),
        agents.events.clone(),
    );

    let iroh_listener = iroh.map(|(listener, _)| listener);

    // Attention watchers are daemon-owned and pre-armed synchronously by the
    // pool before any activation caller can start work on the returned agent.
    // The weak pool reference avoids a pool -> observer -> pool cycle.
    let watched_agents = Arc::new(std::sync::Mutex::new(HashSet::new()));
    let activation_observer: Arc<rho_agent::pool::ActivationObserver> = {
        let pool = Arc::downgrade(&agents.pool);
        let db = agents.db.clone();
        let events = agents.events.clone();
        let watched_agents = watched_agents.clone();
        Arc::new(move |agent_id, agent| {
            let pool = pool.clone();
            let db = db.clone();
            let events = events.clone();
            let watched_agents = watched_agents.clone();
            async move {
                if !watched_agents.lock().expect("poison").insert(agent_id) {
                    return;
                }
                let Some(pool) = pool.upgrade() else {
                    return;
                };
                spawn_attention_watcher(pool, db, events, agent_id, agent).await;
            }
            .boxed()
        })
    };
    agents
        .pool
        .set_activation_observer(activation_observer.clone());
    for (agent_id, agent) in agents.loaded().await {
        activation_observer(agent_id, agent).await;
    }
    agents.resume_platform_integrations();
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

    if let Some(listener) = iroh_listener {
        tokio::spawn(run_iroh_listener(
            agents.clone(),
            listener,
            iroh_auth.clone(),
        ));
    }
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            result = &mut shutdown => {
                result?;
                agents.pool.flush_agent_usage(None).await;
                return Ok(());
            }
            connection = server.accept() => {
                let connection = connection?;
                let agents = agents.clone();
                let iroh_auth = iroh_auth.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve_connection(agents, iroh_auth, connection).await {
                        eprintln!("rho daemon connection error: {error:#}");
                    }
                });
            }
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

/// Serves application streams from connections already approved by
/// [`rho_rpc::AuthenticatedIrohListener`].
async fn run_iroh_listener(
    agents: Arc<AgentRegistry>,
    mut listener: rho_rpc::AuthenticatedIrohListener,
    iroh_auth: Option<rho_iroh_auth::IrohAuth>,
) {
    while let Some(approved) = listener.accept().await {
        let connection = match approved {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("rho daemon iroh authentication error: {error:#}");
                continue;
            }
        };
        let agents = agents.clone();
        let iroh_auth = iroh_auth.clone();
        tokio::spawn(async move {
            let agent_streams = Some(IrohAgentStreams::new(connection.clone()));
            while let Ok((send, recv)) = connection.accept_bi().await {
                let agents = agents.clone();
                let agent_streams = agent_streams.clone();
                let iroh_auth = iroh_auth.clone();
                tokio::spawn(async move {
                    let result = async {
                        let mut recv = rho_rpc::Reader::new(recv);
                        let first = tokio::time::timeout(
                            std::time::Duration::from_secs(10),
                            read_frame::<_, ClientMessage>(&mut recv),
                        )
                        .await
                        .map_err(|_| anyhow::anyhow!("iroh stream first frame timed out"))??;
                        // Dedicated streams (workspace files, shells,
                        // terminals, one-shot queries) are not the UI control
                        // session and must not claim it.
                        let dedicated = matches!(
                            &first,
                            ClientMessage::ChannelOpen { .. }
                                | ClientMessage::RealtimeOpen { .. }
                                | ClientMessage::DiffSnapshot { .. }
                                | ClientMessage::VisualizationGet { .. }
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
                                | ClientMessage::RealtimeOpen { .. }
                        ) {
                            send.set_priority(50)
                                .context("set iroh interactive stream priority")?;
                        }
                        let send = rho_rpc::Writer::new(send);
                        let result = serve_connection_io(
                            agents,
                            iroh_auth,
                            recv,
                            send,
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
                    .await;
                    if let Err(error) = result {
                        eprintln!("rho daemon iroh connection error: {error:#}");
                    }
                });
            }
        });
    }
    listener.close().await;
}

const FOCUSED_AGENT_STREAM_WEIGHT: NonZeroU16 = NonZeroU16::new(200).unwrap();
const MAX_IROH_AGENT_STREAMS: usize = 1024;

/// Per-iroh-connection agent streams. Agent state is sent on daemon-opened
/// unidirectional streams so QUIC can schedule agents independently while the
/// bidirectional UI session remains a low-volume control channel.
#[derive(Clone)]
struct IrohAgentStreams {
    connection: iroh::endpoint::Connection,
    opened: Arc<Mutex<HashMap<AgentId, watch::Sender<bool>>>>,
    control_claimed: Arc<AtomicBool>,
    focus: watch::Sender<Option<AgentId>>,
}

impl IrohAgentStreams {
    fn new(connection: iroh::endpoint::Connection) -> Self {
        let (focus, _) = watch::channel(None);
        Self {
            connection,
            opened: Arc::new(Mutex::new(HashMap::new())),
            control_claimed: Arc::new(AtomicBool::new(false)),
            focus,
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
        self.connection
            .close(0u32.into(), b"UI control session closed");
    }

    async fn remove(&self, agent_id: AgentId) {
        if let Some(cancel) = self.opened.lock().await.remove(&agent_id) {
            cancel.send_replace(true);
        }
    }

    async fn ensure(&self, agent_id: AgentId, agent: RunningAgent) -> anyhow::Result<()> {
        {
            let mut opened = self.opened.lock().await;
            if opened.contains_key(&agent_id) {
                return Ok(());
            }
            if opened.len() >= MAX_IROH_AGENT_STREAMS {
                self.connection
                    .close(2u32.into(), b"too many subscribed agents");
                anyhow::bail!(
                    "iroh agent stream limit ({MAX_IROH_AGENT_STREAMS}) reached; \
                     hide agents before reconnecting"
                );
            }
            let (cancel, _) = watch::channel(false);
            opened.insert(agent_id, cancel.clone());
        }
        let connection = self.connection.clone();
        let focus_sender = self.focus.clone();
        let cancel_sender = self.opened.lock().await[&agent_id].clone();
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
            let mut opened = opened.lock().await;
            if opened
                .get(&agent_id)
                .is_some_and(|current| current.same_channel(&cancel_sender))
            {
                opened.remove(&agent_id);
            }
            drop(opened);
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
    send: iroh::endpoint::SendStream,
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

    let mut send = rho_rpc::Writer::new(send);
    let result: anyhow::Result<()> = async {
        write_frame(&mut send, &ServerMessage::AgentStreamOpened { agent_id }).await?;
        let changes = agent.subscribe();
        let mut encoder = AgentRemoteEncoder::new();
        write_frame(
            &mut send,
            &ServerMessage::Agent {
                agent_id,
                frame: encoder.encode(agent_ui::project_agent_state(&agent.state())),
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
                            frame: encoder.encode(agent_ui::project_agent_state(&state)),
                        },
                    )
                    .await?;
                }
            }
        }
    }
    .await;
    focus_task.abort();
    result?;
    tokio::io::AsyncWriteExt::shutdown(&mut send)
        .await
        .context("finish compressed iroh agent stream")
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
    visualizations: rho_visualizations::VisualizationStore,
    inference: Inference,
    active_auth_name: RwLock<String>,
    quota_refresh: tokio::sync::Notify,
    /// The database's machine seed, announced in `Ready` so clients can
    /// encode agent IDs.
    machine_seed: u64,
    land_locks: Mutex<HashMap<Utf8PathBuf, Arc<TokioMutex<()>>>>,
    land_holders: Mutex<HashMap<Utf8PathBuf, LandLeaseHolder>>,
    land_statuses: Mutex<HashMap<Utf8PathBuf, (Option<AgentId>, LandStatus)>>,
    /// In-process Slack connection and its thread sessions
    /// (see [`rho_slack::SlackManager`]).
    slack: Arc<rho_slack::SlackManager>,
    /// Stateless PR, CI, review, and comment operations.
    pr_monitor: Arc<rho_pr_monitor::PrMonitor>,
    /// Shared sealed platform secret store used by Slack and Octo.
    platform_secrets: PlatformSecrets,
    /// Daemon-wide fanout for messages every client must hear regardless of
    /// which connection caused them (attention changes); each connection
    /// forwards this onto its own outgoing channel.
    events: broadcast::Sender<ServerMessage>,
    /// Daemon-owned Comint-style shell sessions, one per agent.
    shells: Arc<shell::ShellRegistry>,
    /// Daemon-owned terminal sessions, keyed per agent.
    terminals: Arc<terminal::TerminalRegistry>,
    /// The snapshotted login environment, for terminal shells.
    user_environment: rho_workspaces::UserEnvironment,
    git_transport: GitTransportBroker,
    /// The hidden persisted coordinator backing the single global Iris
    /// surface. It is loaded lazily on the first delegated voice request.
    iris_agent: Mutex<Option<AgentId>>,
    /// At most one GUI owns microphone/playback for Iris at a time.
    iris_voice_lease: Arc<TokioMutex<()>>,
}

impl AgentRegistry {
    async fn new(
        db: RhoDb,
        inference: Inference,
        path_overrides: PathOverrides,
        user_environment: rho_workspaces::UserEnvironment,
        platform_secrets: PlatformSecrets,
        active_auth_name: String,
    ) -> anyhow::Result<Self> {
        let pool = AgentPool::new(
            db.clone(),
            inference.clone(),
            path_overrides,
            user_environment.clone(),
        )
        .await;
        let machine_seed = db.read().machine_seed();
        let slack = rho_slack::SlackManager::new(pool.clone(), db.clone()).await;
        let pr_monitor = rho_pr_monitor::PrMonitor::new(pool.clone(), db.clone()).await?;
        let visualizations = rho_visualizations::VisualizationStore::new(db.clone()).await;
        let registry = Self {
            pool,
            db,
            visualizations,
            inference,
            active_auth_name: RwLock::new(active_auth_name),
            quota_refresh: tokio::sync::Notify::new(),
            machine_seed,
            land_locks: Mutex::new(HashMap::new()),
            land_holders: Mutex::new(HashMap::new()),
            land_statuses: Mutex::new(HashMap::new()),
            slack,
            pr_monitor,
            platform_secrets,
            events: broadcast::channel(1024).0,
            shells: Arc::new(shell::ShellRegistry::default()),
            terminals: Arc::new(terminal::TerminalRegistry::default()),
            user_environment,
            git_transport: GitTransportBroker::default(),
            iris_agent: Mutex::new(None),
            iris_voice_lease: Arc::new(TokioMutex::new(())),
        };
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

    fn ui_workstreams(&self) -> Vec<UiWorkstream> {
        let read = self.db.read();
        let iris_workstreams = read
            .list_agents()
            .into_iter()
            .filter(|(_, agent)| {
                agent.role == AgentRole::Iris
                    || agent
                        .labels
                        .iter()
                        .any(|label| label == rho_agent::iris_tools::LABEL)
            })
            .map(|(_, agent)| agent.workstream)
            .collect::<HashSet<_>>();
        let mut records = read.list_workstreams();
        records.sort_by_key(|(_, workstream)| workstream.created_at);
        records
            .into_iter()
            .filter(|(workstream_id, _)| !iris_workstreams.contains(workstream_id))
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
            .filter(|(_, agent)| {
                agent.role != AgentRole::Iris
                    && !agent
                        .labels
                        .iter()
                        .any(|label| label == rho_agent::iris_tools::LABEL)
            })
            .map(|(agent_id, agent)| UiAgentSummary {
                agent_id,
                parent_agent: agent.parent_agent,
                role: agent.config(),
                created_at: agent.created_at,
                updated_at: agent.updated_at,
                workspace: agent.primary_workdir().clone(),
                display_name: agent.display_name.or(agent.generated_title),
                attention: attention_level(kinds.get(&agent_id), agent.disposition),
                last_active: agent.last_user_message.max(agent.created_at),
                hidden: agent.disposition == AgentDisposition::Hidden,
                last_user_message_text: agent.last_user_message_text,
                activity: agent.activity,
                turn_report: agent.turn_report.map(|report| UiTurnReport {
                    needs_you: report.needs_you,
                    summary: report.summary,
                }),
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

    fn auth_state(&self) -> AuthState {
        let active = self.active_auth_name.read().unwrap().clone();
        let default = self
            .db
            .read()
            .default_auth_namespace()
            .unwrap_or_else(|| "default".to_owned());
        let mut namespaces = rho_inference::auth_namespaces().unwrap_or_default();
        namespaces.extend([active.clone(), default.clone()]);
        namespaces.sort();
        namespaces.dedup();
        AuthState {
            active,
            default,
            namespaces,
        }
    }

    async fn validated_auth(name: &str) -> anyhow::Result<InferenceAuth> {
        let name = name.trim();
        let auth = InferenceAuth::named(name)?;
        let candidate = auth.clone();
        tokio::task::spawn_blocking(move || candidate.resolve_oauth())
            .await
            .context("join auth credential validation")??;
        Ok(auth)
    }

    async fn set_auth_namespace(&self, name: &str) -> anyhow::Result<()> {
        let auth = Self::validated_auth(name).await?;
        let name = name.trim().to_owned();
        let mut write = self.db.write().await;
        write.set_default_auth_namespace(name.clone());
        write.commit();
        self.inference.set_auth(auth);
        *self.active_auth_name.write().unwrap() = name;
        let _ = self.events.send(ServerMessage::AuthState {
            auth: self.auth_state(),
        });
        self.quota_refresh.notify_one();
        Ok(())
    }

    async fn ready_message(&self) -> ServerMessage {
        ServerMessage::Ready {
            workstreams: self.ui_workstreams(),
            agents: self.ui_agents(&self.agent_state_kinds().await),
            projects: self.projects(),
            auth: self.auth_state(),
            view_config: self.db.read().view_config(),
            machine_seed: self.machine_seed,
            agent_counter: self.db.read().last_agent_counter(),
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
    iroh_auth: Option<rho_iroh_auth::IrohAuth>,
    connection: ServerConnection,
) -> anyhow::Result<()> {
    let land_holder = connection.peer_cred().ok().map(|cred| LandLeaseHolder {
        pid: cred.pid().and_then(|pid| u32::try_from(pid).ok()),
        uid: cred.uid(),
        gid: cred.gid(),
    });
    let stream = connection.into_stream();
    let (reader, writer) = stream.into_split();
    serve_connection_io(agents, iroh_auth, reader, writer, land_holder, None, None).await
}

/// One UI protocol session over any framed byte stream (Unix socket or an
/// iroh bi-stream from an enrolled remote client).
async fn serve_connection_io<R, W>(
    agents: Arc<AgentRegistry>,
    iroh_auth: Option<rho_iroh_auth::IrohAuth>,
    reader: R,
    writer: W,
    land_holder: Option<LandLeaseHolder>,
    agent_streams: Option<IrohAgentStreams>,
    first: Option<ClientMessage>,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // The first client frame chooses the stream's protocol: `ChannelOpen`
    // dedicates the whole stream to one workspace channel, anything else starts a
    // normal UI session (every UI client speaks first — Subscribe or a
    // command — so waiting here never deadlocks).
    let mut reader = reader;
    let first = match first {
        Some(first) => first,
        None => tokio::time::timeout(
            std::time::Duration::from_secs(10),
            read_frame::<_, ClientMessage>(&mut reader),
        )
        .await
        .map_err(|_| anyhow::anyhow!("Unix stream first frame timed out"))??,
    };
    if let ClientMessage::ChannelOpen { workspace } = first {
        return serve_workspace_channel(agents, reader, writer, workspace).await;
    }
    if let ClientMessage::RealtimeOpen { offer_sdp } = first {
        return realtime::serve(agents, reader, writer, offer_sdp).await;
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
    if let ClientMessage::DiffBaseContents {
        workspace,
        operation_id,
        commit_id,
        paths,
    } = first
    {
        return serve_diff_base_contents(agents, writer, workspace, operation_id, commit_id, paths)
            .await;
    }
    if let ClientMessage::VisualizationGet { id } = first {
        let mut writer = writer;
        let response = match agents.visualizations.get(&id) {
            Some(visualization) => ServerMessage::VisualizationContent {
                id,
                mime_type: visualization.mime_type,
                content: visualization.content,
            },
            None => ServerMessage::VisualizationRefused {
                reason: format!("visualization {id} does not exist"),
            },
        };
        write_frame(&mut writer, &response).await?;
        return Ok(());
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
    tokio::spawn(async move {
        let mut writer = writer;
        while let Some(message) = outgoing_rx.recv().await {
            if write_frame(&mut writer, &message).await.is_err() {
                break;
            }
        }
    });

    // Creations update lightweight registry summaries. Agent-state streams
    // are opened only by this connection's explicit subscriptions. Subscribe
    // before building Ready so a concurrent creation is either in its
    // snapshot or arrives on this receiver (occasionally both, harmlessly).
    let mut created_rx = agents.pool.subscribe_created();
    let mut events_rx = agents.events.subscribe();
    let _ = outgoing_tx.send(agents.ready_message().await);
    let mut local_subscriptions = HashMap::new();
    let mut presentation_watches = HashMap::new();

    // Announce every agent created in the pool — by clients or by other
    // agents spawning children — so it shows up on this connection.
    {
        let agents = Arc::clone(&agents);
        let outgoing_tx = outgoing_tx.clone();
        tokio::spawn(async move {
            loop {
                match created_rx.recv().await {
                    Ok(created) => {
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
            None => match read_frame::<_, ClientMessage>(&mut reader).await {
                Ok(message) => message,
                Err(error) => {
                    for (repo, _) in &land_leases {
                        agents.clear_land_holder(repo).await;
                    }
                    break Err(error);
                }
            },
        };
        match handle_message(
            &agents,
            iroh_auth.as_ref(),
            &outgoing_tx,
            &mut land_leases,
            land_holder.clone(),
            agent_streams.as_ref(),
            &mut local_subscriptions,
            &mut presentation_watches,
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
    for (_, subscription) in local_subscriptions {
        subscription.abort();
    }
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
    let provider = match agents.git_transport.request(request).await {
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
    let requester = tokio::io::join(reader, writer);
    rho_rpc::relay_bidirectional(requester, provider).await?;
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
/// sub-agent turn ends only set it to Pending once the user has personally
/// engaged the agent (see `settle_turn`), so untouched children stay quiet
/// by construction.
fn attention_level(kind: Option<&AgentStateKind>, disposition: AgentDisposition) -> UiAttention {
    if kind.is_some_and(AgentStateKind::is_working) {
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

/// Durable presentation changes refresh the normal snapshot for every
/// connection. Broadcast loss is harmless because `Ready` is reconstructed
/// from the agent cache, including after daemon restart.
fn spawn_presentation_projection(agents: Arc<AgentRegistry>) {
    let mut changes = agents.pool.subscribe_presentation_changes();
    let agents = Arc::downgrade(&agents);
    tokio::spawn(async move {
        while let Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) = changes.recv().await {
            let Some(agents) = agents.upgrade() else {
                break;
            };
            let _ = agents.events.send(agents.ready_message().await);
        }
    });
}

/// Relays turn reports to every client. Classification and persistence
/// happen runtime-side at turn end; broadcast loss is harmless because
/// `Ready` snapshots carry the persisted report.
fn spawn_turn_report_projection(agents: Arc<AgentRegistry>) {
    let mut reports = agents.pool.subscribe_turn_reports();
    let agents = Arc::downgrade(&agents);
    tokio::spawn(async move {
        loop {
            let reported = match reports.recv().await {
                Ok(reported) => reported,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            };
            let Some(agents) = agents.upgrade() else {
                break;
            };
            let _ = agents.events.send(ServerMessage::AgentTurnReport {
                agent_id: reported.agent_id,
                report: UiTurnReport {
                    needs_you: reported.report.needs_you,
                    summary: reported.report.summary,
                },
            });
            // An FYI settled the row Done as it persisted; tell clients the
            // level moved, exactly as a user verdict would.
            if !reported.report.needs_you {
                let kind = agents
                    .get(reported.agent_id)
                    .await
                    .map(|agent| agent.state().kind);
                let _ = agents.events.send(ServerMessage::AgentAttention {
                    agent_id: reported.agent_id,
                    attention: attention_level(kind.as_ref(), AgentDisposition::Done),
                });
            }
        }
    });
}

/// Watches one running agent for the daemon itself (not any particular
/// connection): records turn ends and broadcasts attention level changes to
/// every client. Spawned exactly once per activated agent.
///
/// Sub-agents (a parent spawned them) get Working broadcasts but no turn-end
/// records until the user personally engages them: their finished turns are
/// the parent's court, not the user's.
async fn spawn_attention_watcher(
    pool: Arc<AgentPool>,
    db: RhoDb,
    events: broadcast::Sender<ServerMessage>,
    agent_id: AgentId,
    agent: RunningAgent,
) {
    let (ready_tx, ready_rx) = oneshot::channel();
    tokio::spawn(async move {
        let changes = agent.subscribe();
        futures::pin_mut!(changes);
        let Some(initial_state) = changes.next().await else {
            let _ = ready_tx.send(());
            return;
        };
        // The state subscription is armed. Activation callers may now start
        // work; durable turn completion is published by each runtime rather
        // than inferred from this coalescing snapshot stream.
        let _ = ready_tx.send(());
        let mut was_working = initial_state.kind.is_working();
        let mut last_sent = None;
        let mut last_quota = None;
        let states = futures::stream::once(async move { initial_state }).chain(changes);
        futures::pin_mut!(states);
        while let Some(state) = states.next().await {
            if state.quota_observation != last_quota {
                last_quota = state.quota_observation.clone();
                if let Some(observation) = &state.quota_observation {
                    let model = match observation.model.as_str() {
                        "gpt" => QuotaModel::GPT,
                        "fable" => QuotaModel::FABLE,
                        "opus" => QuotaModel::OPUS,
                        _ => continue,
                    };
                    // ChatGPT quota is persisted by the namespace-aware
                    // poller. A streamed observation does not carry the auth
                    // used by its in-flight request and cannot be attributed
                    // safely after an auth switch.
                    let provider = match observation.provider {
                        rho_agent::QuotaProvider::ChatGpt => continue,
                        rho_agent::QuotaProvider::Claude => QuotaProvider::Claude,
                    };
                    let mut write = db.write().await;
                    let changed = write.record_quota_observation(QuotaObservationRecord {
                        provider,
                        model,
                        auth_namespace: None,
                        observed_at: observation.observed_at,
                        used_percent: observation.used_percent,
                        reset_at_unix: observation.reset_at_unix,
                    });
                    write.commit();
                    if changed {
                        let _ = events.send(ServerMessage::QuotaUsage {
                            summaries: quota_summaries(&db),
                        });
                    }
                }
            }
            let working = state.kind.is_working();
            if !working && was_working {
                pool.flush_agent_usage(Some(agent_id)).await;
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
    let _ = ready_rx.await;
}

fn quota_summaries(db: &RhoDb) -> Vec<QuotaSummary> {
    let now = rho_core::UnixMs::now().0;
    let since = rho_core::UnixMs(now.saturating_sub(3 * 24 * 60 * 60 * 1_000));
    quota_observation_groups(db, since)
        .into_iter()
        .filter_map(|((model, auth_namespace), observations)| {
            let samples = observations.iter().collect::<Vec<_>>();
            let latest = samples.last()?;
            let reset_expired = latest
                .reset_at_unix
                .is_some_and(|reset| reset <= (now / 1_000) as i64);
            let burn = |duration| {
                if reset_expired {
                    0
                } else {
                    quota_burn(&samples, now, duration)
                }
            };
            Some(QuotaSummary {
                model: model.name().to_owned(),
                auth_namespace,
                remaining_percent: if reset_expired {
                    100
                } else {
                    100u8.saturating_sub(latest.used_percent)
                },
                burn_10m: burn(10 * 60 * 1_000),
                burn_2h: burn(2 * 60 * 60 * 1_000),
                burn_1d: burn(24 * 60 * 60 * 1_000),
                burn_3d: burn(3 * 24 * 60 * 60 * 1_000),
                reset_at_unix: if reset_expired {
                    None
                } else {
                    latest.reset_at_unix
                },
            })
        })
        .collect()
}

fn ui_agent_usage_bucket(bucket: rho_agent::db::AgentUsageBucket) -> UiAgentUsageBucket {
    UiAgentUsageBucket {
        bucket_start_ms: bucket.bucket_start_ms,
        input_tokens: bucket.input_tokens,
        cache_read_tokens: bucket.cache_read_tokens,
        cache_write_tokens: bucket.cache_write_tokens,
        cache_write_1h_tokens: bucket.cache_write_1h_tokens,
        output_tokens: bucket.output_tokens,
        requests: bucket.requests,
        approximate: bucket.approximate,
    }
}

/// Reduces the indexed five-minute usage records to the hourly samples the
/// usage-share chart renders. The persisted key begins with time, so the
/// preceding database query is already a bounded range scan.
fn hourly_global_usage_series(
    usage: Vec<(AgentUsageModel, rho_agent::db::AgentUsageBucket)>,
) -> Vec<AgentUsageSeries> {
    const HOUR_MS: u64 = 60 * 60 * 1_000;

    let mut hourly = BTreeMap::<(AgentUsageModel, u64), rho_agent::db::AgentUsageBucket>::new();
    for (model, bucket) in usage {
        let bucket_start_ms = bucket.bucket_start_ms / HOUR_MS * HOUR_MS;
        hourly
            .entry((model, bucket_start_ms))
            .or_insert_with(|| rho_agent::db::AgentUsageBucket {
                bucket_start_ms,
                model,
                ..rho_agent::db::AgentUsageBucket::default()
            })
            .add(&bucket);
    }

    [
        AgentUsageModel::FABLE,
        AgentUsageModel::GPT,
        AgentUsageModel::OPUS,
        AgentUsageModel::TERRA,
        AgentUsageModel::LUNA,
    ]
    .into_iter()
    .map(|model| AgentUsageSeries {
        model: model.name().to_owned(),
        buckets: hourly
            .iter()
            .filter(|((candidate, _), _)| *candidate == model)
            .map(|(_, bucket)| ui_agent_usage_bucket(bucket.clone()))
            .collect(),
    })
    .collect()
}

fn quota_history(db: &RhoDb) -> Vec<QuotaSeries> {
    let now = rho_core::UnixMs::now().0;
    let since = rho_core::UnixMs(now.saturating_sub(30 * 24 * 60 * 60 * 1_000));
    quota_observation_groups(db, since)
        .into_iter()
        .filter_map(|((model, auth_namespace), observations)| {
            let points = observations
                .into_iter()
                .map(|sample| QuotaPoint {
                    observed_at_ms: sample.observed_at.0,
                    remaining_percent: 100u8.saturating_sub(sample.used_percent),
                    reset_at_unix: sample.reset_at_unix,
                })
                .collect::<Vec<_>>();
            (!points.is_empty()).then(|| QuotaSeries {
                model: model.name().to_owned(),
                auth_namespace,
                points,
            })
        })
        .collect()
}

fn quota_observation_groups(
    db: &RhoDb,
    since: rho_core::UnixMs,
) -> BTreeMap<(QuotaModel, Option<String>), Vec<QuotaObservationRecord>> {
    let read = db.read();
    let mut groups = BTreeMap::new();
    for model in [QuotaModel::GPT, QuotaModel::OPUS, QuotaModel::FABLE] {
        for observation in read.quota_observations(model, since) {
            // Pre-namespace GPT history cannot be attributed safely.
            if model == QuotaModel::GPT && observation.auth_namespace.is_none() {
                continue;
            }
            groups
                .entry((model, observation.auth_namespace.clone()))
                .or_insert_with(Vec::new)
                .push(observation);
        }
    }
    groups
}

fn quota_burn(samples: &[&QuotaObservationRecord], now: u64, duration_ms: u64) -> u16 {
    let cutoff = now.saturating_sub(duration_ms);
    let start = samples
        .partition_point(|sample| sample.observed_at.0 < cutoff)
        .saturating_sub(1);
    let Some((first, rest)) = samples
        .get(start..)
        .and_then(|samples| samples.split_first())
    else {
        return 0;
    };

    let mut epoch_start = *first;
    let mut epoch_end = *first;
    let mut burn = 0u16;
    for sample in rest {
        let same_epoch = match (epoch_end.reset_at_unix, sample.reset_at_unix) {
            (Some(old), Some(new)) => old.abs_diff(new) <= 60,
            (None, None) => true,
            _ => false,
        };
        if same_epoch {
            epoch_end = sample;
        } else {
            burn += epoch_end
                .used_percent
                .saturating_sub(epoch_start.used_percent) as u16;
            epoch_start = sample;
            epoch_end = sample;
        }
    }
    burn + epoch_end
        .used_percent
        .saturating_sub(epoch_start.used_percent) as u16
}

fn spawn_chatgpt_quota_poller(agents: Arc<AgentRegistry>) {
    tokio::spawn(async move {
        loop {
            let auth = agents.auth_state();
            let mut namespaces = auth.namespaces;
            namespaces.sort_by_key(|name| (name != &auth.active, name.clone()));
            let mut changed = false;
            for namespace in namespaces {
                let name = namespace.clone();
                let usage =
                    tokio::task::spawn_blocking(move || rho_inference::chatgpt_weekly_usage(name))
                        .await;
                if let Ok(Ok(Some(usage))) = usage {
                    let mut write = agents.db.write().await;
                    changed |= write.record_quota_observation(QuotaObservationRecord {
                        provider: QuotaProvider::ChatGpt,
                        model: QuotaModel::GPT,
                        auth_namespace: Some(namespace),
                        observed_at: rho_core::UnixMs::now(),
                        used_percent: usage.used_percent.clamp(0.0, 100.0).round() as u8,
                        reset_at_unix: Some(usage.reset_at_unix),
                    });
                    write.commit();
                }
            }
            if changed {
                let _ = agents.events.send(ServerMessage::QuotaUsage {
                    summaries: quota_summaries(&agents.db),
                });
            }
            tokio::select! {
                () = tokio::time::sleep(std::time::Duration::from_secs(10 * 60)) => {}
                () = agents.quota_refresh.notified() => {}
            }
        }
    });
}

fn spawn_claude_quota_recorder(
    mut updates: tokio::sync::mpsc::Receiver<anyhow::Result<rho_claude_usage::ClaudeUsage>>,
    db: RhoDb,
    events: broadcast::Sender<ServerMessage>,
) {
    tokio::spawn(async move {
        while let Some(update) = updates.recv().await {
            let usage = match update {
                Ok(usage) => usage,
                Err(error) => {
                    tracing::warn!(%error, "Claude quota probe failed");
                    continue;
                }
            };
            let observed_at = rho_core::UnixMs::now();
            let mut write = db.write().await;
            let mut changed = write.record_quota_observation(QuotaObservationRecord {
                provider: QuotaProvider::Claude,
                model: QuotaModel::OPUS,
                auth_namespace: None,
                observed_at,
                used_percent: usage.all_models.used_percent,
                reset_at_unix: Some(usage.all_models.reset_at_unix),
            });
            changed |= write.record_quota_observation(QuotaObservationRecord {
                provider: QuotaProvider::Claude,
                model: QuotaModel::FABLE,
                auth_namespace: None,
                observed_at,
                used_percent: usage.fable.used_percent,
                reset_at_unix: Some(usage.fable.reset_at_unix),
            });
            write.commit();
            if changed {
                let _ = events.send(ServerMessage::QuotaUsage {
                    summaries: quota_summaries(&db),
                });
            }
        }
    });
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
#[allow(clippy::too_many_arguments)]
async fn handle_message(
    agents: &Arc<AgentRegistry>,
    iroh_auth: Option<&rho_iroh_auth::IrohAuth>,
    outgoing_tx: &mpsc::UnboundedSender<ServerMessage>,
    land_leases: &mut Vec<(Utf8PathBuf, OwnedMutexGuard<()>)>,
    land_holder: Option<LandLeaseHolder>,
    agent_streams: Option<&IrohAgentStreams>,
    local_subscriptions: &mut HashMap<AgentId, tokio::task::JoinHandle<()>>,
    presentation_watches: &mut HashMap<AgentId, rho_agent::presentation::Watch>,
    message: ClientMessage,
) -> anyhow::Result<Refresh> {
    match message {
        ClientMessage::Ping => {
            let _ = outgoing_tx.send(ServerMessage::Pong);
            Ok(Refresh::None)
        }
        ClientMessage::RecordVisualization { mime_type, content } => {
            let id = agents.visualizations.record(mime_type, content).await?;
            let _ = outgoing_tx.send(ServerMessage::VisualizationRecorded { id });
            Ok(Refresh::None)
        }
        ClientMessage::ChatGptUsage => {
            let _ = outgoing_tx.send(ServerMessage::QuotaUsage {
                summaries: quota_summaries(&agents.db),
            });
            Ok(Refresh::None)
        }
        ClientMessage::QuotaHistory => {
            let _ = outgoing_tx.send(ServerMessage::QuotaHistory {
                series: quota_history(&agents.db),
            });
            Ok(Refresh::None)
        }
        ClientMessage::AgentUsage { agent_id, since_ms } => {
            agents.pool.flush_agent_usage(Some(agent_id)).await;
            let read = agents.db.read();
            let model = match read.get_agent(agent_id).runtime {
                AgentRuntime::Rho { .. } => "gpt-5.6-sol",
                AgentRuntime::Claude { .. } => "claude-fable-5",
            };
            let buckets = read
                .agent_usage(agent_id, rho_core::UnixMs(since_ms))
                .into_iter()
                .map(ui_agent_usage_bucket)
                .collect();
            let total = ui_agent_usage_bucket(read.agent_usage_total(agent_id));
            let _ = outgoing_tx.send(ServerMessage::AgentUsage {
                agent_id,
                model: model.to_owned(),
                buckets,
                total,
            });
            Ok(Refresh::None)
        }
        ClientMessage::GlobalUsage { since_ms } => {
            agents.pool.flush_agent_usage(None).await;
            let usage = agents
                .db
                .read()
                .global_agent_usage(rho_core::UnixMs(since_ms));
            let series = hourly_global_usage_series(usage);
            let _ = outgoing_tx.send(ServerMessage::GlobalUsage { series });
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
            agent_id: _,
            command,
        } => {
            let result = async {
                match command {
                    rho_ui_proto::PrCommand::Create {
                        owner,
                        repo,
                        head,
                        base,
                        title,
                        body,
                        review_bots: _,
                    } => agents
                        .pr_monitor
                        .create(rho_pr_monitor::CreatePullRequest {
                            owner,
                            repo,
                            head,
                            base,
                            title,
                            body,
                        })
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_ui_proto::PrCommand::Subscribe { .. } => Ok((
                        "persistent PR subscriptions were removed; poll `rho pr status` instead"
                            .to_owned(),
                        Vec::new(),
                    )),
                    rho_ui_proto::PrCommand::Status { url } => agents
                        .pr_monitor
                        .status(&url)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_ui_proto::PrCommand::List => Ok(("[]".to_owned(), Vec::new())),
                    rho_ui_proto::PrCommand::Stop { .. } => Ok((
                        "persistent PR subscriptions were removed".to_owned(),
                        Vec::new(),
                    )),
                    rho_ui_proto::PrCommand::Comment {
                        url,
                        reply_comment,
                        body,
                    } => agents
                        .pr_monitor
                        .comment(&url, reply_comment, &body)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_ui_proto::PrCommand::Comments { url } => agents
                        .pr_monitor
                        .comments(&url)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_ui_proto::PrCommand::Checks { url } => agents
                        .pr_monitor
                        .checks(&url)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_ui_proto::PrCommand::Edit {
                        url,
                        base,
                        title,
                        body,
                    } => agents
                        .pr_monitor
                        .edit(&url, base, title, body)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_ui_proto::PrCommand::Rerun { url, run_id } => agents
                        .pr_monitor
                        .rerun(&url, run_id)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_ui_proto::PrCommand::Logs { url, run_id } => {
                        agents.pr_monitor.logs(&url, run_id).await.map(|data| {
                            (format!("downloaded logs for run {run_id}"), data.to_vec())
                        })
                    }
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
            if let Some(content) = content.as_deref() {
                validate_image_content(content)?;
            }
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
            if let Some((workstream_id, provisional_name)) = &founded {
                let mut write = agents.db.write().await;
                write.set_pending_presentation_workstream(
                    agent_id,
                    Some(PendingPresentationWorkstream {
                        workstream_id: *workstream_id,
                        provisional_name: provisional_name.clone(),
                    }),
                );
                write.commit();
            }
            if let Some(content) = content {
                // The agent is fresh, so the lanes are equivalent here.
                agent
                    .send_user_content_accepted(content, MessageDelivery::NextRequest, None)
                    .await?;
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
        ClientMessage::SubscribeAgent { agent_id } => {
            subscribe_connection_agents(
                agents,
                outgoing_tx,
                agent_streams,
                local_subscriptions,
                presentation_watches,
                [agent_id],
            )
            .await?;
            Ok(Refresh::None)
        }
        ClientMessage::SubscribeAgents { agent_ids } => {
            anyhow::ensure!(agent_ids.len() <= 1024, "too many agent subscriptions");
            subscribe_connection_agents(
                agents,
                outgoing_tx,
                agent_streams,
                local_subscriptions,
                presentation_watches,
                agent_ids,
            )
            .await?;
            Ok(Refresh::None)
        }
        ClientMessage::UnsubscribeAgents { agent_ids } => {
            anyhow::ensure!(agent_ids.len() <= 1024, "too many agent unsubscriptions");
            for agent_id in agent_ids {
                if let Some(streams) = agent_streams {
                    streams.remove(agent_id).await;
                }
                if let Some(subscription) = local_subscriptions.remove(&agent_id) {
                    subscription.abort();
                    let _ = subscription.await;
                }
                presentation_watches.remove(&agent_id);
                let _ = outgoing_tx.send(ServerMessage::AgentUnloaded {
                    agent_id,
                    reason: rho_ui_proto::AgentUnloadReason::Unsubscribed,
                });
            }
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
            validate_image_content(&content)?;
            let agent = agents
                .get(agent_id)
                .await
                .ok_or_else(|| anyhow::anyhow!("agent is not loaded: {agent_id:?}"))?;
            agent
                .send_user_content_accepted(content, delivery, None)
                .await?;
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
        ClientMessage::ChangeAgentRole { agent_id, role } => {
            let agent = agents
                .get(agent_id)
                .await
                .ok_or_else(|| anyhow::anyhow!("agent is not loaded: {agent_id:?}"))?;
            agent.change_role(role).await?;
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
        ClientMessage::SetAuthNamespace { name } => {
            agents.set_auth_namespace(&name).await?;
            Ok(Refresh::None)
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
            let auth =
                iroh_auth.context("daemon is not listening over iroh (start it with --iroh)")?;
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
            let auth =
                iroh_auth.context("daemon is not listening over iroh (start it with --iroh)")?;
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
            let auth =
                iroh_auth.context("daemon is not listening over iroh (start it with --iroh)")?;
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
        ClientMessage::RealtimeOpen { .. } => {
            anyhow::bail!("RealtimeOpen must be the first frame on a dedicated stream")
        }
        ClientMessage::DiffSnapshot { .. }
        | ClientMessage::DiffBaseContents { .. }
        | ClientMessage::VisualizationGet { .. }
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

async fn shell_start(agents: &Arc<AgentRegistry>, agent: &str) -> anyhow::Result<()> {
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

/// Loads one bounded parent-content batch from an already immutable diff
/// operation. This intentionally does not snapshot the working copy.
async fn serve_diff_base_contents<W>(
    agents: Arc<AgentRegistry>,
    mut writer: W,
    workspace: WorkspaceInfo,
    operation_id: String,
    commit_id: String,
    paths: Vec<Utf8PathBuf>,
) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    static DIFF_LOADS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let _permit = DIFF_LOADS.acquire().await.context("diff loader closed")?;
        let workspace = agents.pool.open_workspace(&workspace).await?;
        workspace
            .diff_base_contents(&operation_id, &commit_id, &paths)
            .await
    })
    .await
    .context("deferred diff content timed out after 30 seconds")
    .and_then(|result| result);
    match result {
        Ok(contents) => {
            write_frame(&mut writer, &ServerMessage::DiffBaseContents { contents }).await
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

/// Serves a bounded typed file channel rooted in one workspace checkout.
async fn serve_workspace_channel<R, W>(
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
    let files = match workspace_channel::WorkspaceFiles::open(workspace.checkout().to_owned()) {
        Ok(files) => Arc::new(files),
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
    let watcher_setup = match files.start_watcher() {
        Ok(watcher) => watcher,
        Err(error) => {
            let _ = write_frame(
                &mut writer,
                &ServerMessage::ChannelClosed {
                    reason: format!("watch workspace: {error:#}"),
                },
            )
            .await;
            return Err(error);
        }
    };
    write_frame(&mut writer, &ServerMessage::ChannelOpened).await?;

    use rho_ui_proto::workspace::{WorkspaceClientFrame, WorkspaceServerFrame};
    let mut changes = watcher_setup.changes;
    let changes_overflowed = watcher_setup.overflowed;
    let mut watcher_ready = Some(watcher_setup.ready);
    // Keep the watcher alive after its asynchronous directory registration
    // completes. The leading underscore documents that ownership is the only
    // purpose of this value.
    let mut _watcher = None;
    let mut pending_watch_directories = std::collections::BTreeSet::<camino::Utf8PathBuf>::new();
    loop {
        tokio::select! {
            result = async { watcher_ready.as_mut().expect("watcher setup is enabled").await }, if watcher_ready.is_some() => {
                // Drop the completed JoinHandle before retaining its watcher.
                watcher_ready.take();
                match result {
                    Ok(Ok(watcher)) => {
                        _watcher = Some(watcher);
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(%error, "workspace watcher registration failed");
                    }
                    Err(error) => {
                        tracing::warn!(%error, "workspace watcher registration task failed");
                    }
                }
                if let Some(watcher) = _watcher.as_mut() {
                    for directory in std::mem::take(&mut pending_watch_directories) {
                        if let Err(error) = files.watch_directory_tree(watcher, &directory) {
                            tracing::warn!(%directory, %error, "watch newly created workspace directory");
                        }
                    }
                }
                // A watcher cannot report changes made before its directory
                // registration completed. Treat that window like overflow; the
                // GUI already reconciles it by reloading open buffers and
                // scheduling a fresh jj semantic barrier.
                rho_ui_proto::write_frame_limited(
                    &mut writer,
                    &WorkspaceServerFrame::Changed {
                        paths: Vec::new(),
                        rescan: true,
                    },
                    rho_ui_proto::workspace::MAX_WORKSPACE_FRAME_LEN,
                )
                .await?;
            }
            frame = rho_ui_proto::read_frame_limited::<_, WorkspaceClientFrame>(
                &mut reader,
                rho_ui_proto::workspace::MAX_WORKSPACE_FRAME_LEN,
            ) => {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(error) if error.chain().any(|cause| {
                        cause.downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::UnexpectedEof)
                    }) => return Ok(()),
                    Err(error) => return Err(error),
                };
                let response = match frame {
                    WorkspaceClientFrame::Open { request_id, path } => {
                        let result = files.read(path.clone()).await;
                        WorkspaceServerFrame::Opened { request_id, path, result }
                    }
                    WorkspaceClientFrame::Reload { request_id, path } => {
                        let result = files.read(path.clone()).await;
                        WorkspaceServerFrame::Reloaded { request_id, path, result }
                    }
                    WorkspaceClientFrame::Save { request_id, path, revision, contents } => {
                        let result = files.save(path.clone(), Some(revision), contents).await;
                        WorkspaceServerFrame::Saved { request_id, path, result }
                    }
                    WorkspaceClientFrame::Overwrite { request_id, path, contents } => {
                        let result = files.save(path.clone(), None, contents).await;
                        WorkspaceServerFrame::Saved { request_id, path, result }
                    }
                };
                rho_ui_proto::write_frame_limited(
                    &mut writer,
                    &response,
                    rho_ui_proto::workspace::MAX_WORKSPACE_FRAME_LEN,
                )
                .await?;
            }
            Some(first) = changes.recv() => {
                let (paths, directories, explicit_rescan) =
                    workspace_channel::drain_changes(first, &mut changes);
                pending_watch_directories.extend(directories);
                if let Some(watcher) = _watcher.as_mut() {
                    for directory in std::mem::take(&mut pending_watch_directories) {
                        if let Err(error) = files.watch_directory_tree(watcher, &directory) {
                            tracing::warn!(%directory, %error, "watch newly created workspace directory");
                        }
                    }
                }
                let overflowed = changes_overflowed.swap(false, Ordering::AcqRel);
                let rescan = explicit_rescan || overflowed;
                rho_ui_proto::write_frame_limited(
                    &mut writer,
                    &WorkspaceServerFrame::Changed { paths, rescan },
                    rho_ui_proto::workspace::MAX_WORKSPACE_FRAME_LEN,
                )
                .await?;
            }
        }
    }
}
async fn subscribe_connection_agents(
    agents: &Arc<AgentRegistry>,
    outgoing_tx: &mpsc::UnboundedSender<ServerMessage>,
    agent_streams: Option<&IrohAgentStreams>,
    local_subscriptions: &mut HashMap<AgentId, tokio::task::JoinHandle<()>>,
    presentation_watches: &mut HashMap<AgentId, rho_agent::presentation::Watch>,
    agent_ids: impl IntoIterator<Item = AgentId>,
) -> anyhow::Result<()> {
    for agent_id in agent_ids {
        let (agent_id, agent, _loaded_now) = agents.load(agent_id).await?;
        if let Some(streams) = agent_streams {
            streams.ensure(agent_id, agent).await?;
        } else {
            local_subscriptions
                .entry(agent_id)
                .or_insert_with(|| subscribe_agent(agent_id, agent, outgoing_tx.clone()));
        }
        // `Ready` always includes the durable presentation cache for every
        // dashboard row. This lease only permits a loaded agent to
        // refresh that cache with Luna while its transcript is observed.
        if !presentation_watches.contains_key(&agent_id)
            && let Some(watch) = agents.pool.watch_presentation(agent_id).await
        {
            presentation_watches.insert(agent_id, watch);
        }
        let _ = outgoing_tx.send(ServerMessage::AgentSubscribed { agent_id });
    }
    Ok(())
}

fn subscribe_agent(
    agent_id: AgentId,
    agent: RunningAgent,
    state_tx: mpsc::UnboundedSender<ServerMessage>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let changes = agent.subscribe();
        let mut encoder = AgentRemoteEncoder::new();
        let _ = state_tx.send(ServerMessage::Agent {
            agent_id,
            frame: encoder.encode(agent_ui::project_agent_state(&agent.state())),
        });
        futures::pin_mut!(changes);
        while let Some(state) = changes.next().await {
            if state_tx
                .send(ServerMessage::Agent {
                    agent_id,
                    frame: encoder.encode(agent_ui::project_agent_state(&state)),
                })
                .is_err()
            {
                break;
            }
        }
    })
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

const MAX_INPUT_IMAGES: usize = 20;
const MAX_IMAGE_BASE64_BYTES: usize = 10 * 1024 * 1024;

/// Validate image inputs before they enter an agent queue or persistent log.
/// The aggregate bound leaves room for content tags and framing inside the
/// protocol's 64 MiB payload cap.
fn validate_image_content(content: &[ContentPart]) -> anyhow::Result<()> {
    let mut count = 0usize;
    let mut encoded_total = 0usize;
    for part in content {
        let ContentPart::Image { media_type, data } = part else {
            continue;
        };
        count += 1;
        if count > MAX_INPUT_IMAGES {
            anyhow::bail!("too many image attachments (maximum {MAX_INPUT_IMAGES})");
        }
        if !matches!(
            media_type.as_str(),
            "image/png" | "image/jpeg" | "image/webp" | "image/gif"
        ) {
            anyhow::bail!("unsupported image format: {media_type}");
        }
        if data.is_empty() {
            anyhow::bail!("image attachment is empty");
        }
        let encoded = data.len().div_ceil(3).saturating_mul(4);
        if encoded > MAX_IMAGE_BASE64_BYTES {
            anyhow::bail!("image attachment exceeds the 10 MiB encoded limit");
        }
        encoded_total = encoded_total.saturating_add(encoded);
    }
    if encoded_total > rho_ui_proto::MAX_FRAME_LEN.saturating_sub(1024 * 1024) {
        anyhow::bail!("image attachments exceed the protocol aggregate size limit");
    }
    Ok(())
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

    use rho_agent::db::{AgentWriteTxnExt, QuotaModel, QuotaObservationRecord, QuotaProvider};
    use rho_core::ContentPart;
    use rho_db::RhoDb;
    use rho_ui_proto::ServerMessage;

    use super::{
        AgentUsageModel, GitProviderClaim, GitTransportBroker, MAX_IMAGE_BASE64_BYTES,
        MAX_INPUT_IMAGES, configure_octo_git_transport, hourly_global_usage_series, quota_burn,
        quota_history, quota_summaries, validate_image_content,
    };

    #[test]
    fn global_usage_response_rolls_five_minute_buckets_up_to_hours() {
        let bucket = |model, bucket_start_ms, input_tokens| rho_agent::db::AgentUsageBucket {
            bucket_start_ms,
            model,
            input_tokens,
            requests: 1,
            ..Default::default()
        };
        let series = hourly_global_usage_series(vec![
            (
                AgentUsageModel::FABLE,
                bucket(AgentUsageModel::FABLE, 5 * 60 * 1_000, 10),
            ),
            (
                AgentUsageModel::FABLE,
                bucket(AgentUsageModel::FABLE, 55 * 60 * 1_000, 20),
            ),
            (
                AgentUsageModel::GPT,
                bucket(AgentUsageModel::GPT, 60 * 60 * 1_000, 30),
            ),
        ]);

        assert_eq!(series.len(), 5);
        assert_eq!(series[0].model, "fable");
        assert_eq!(series[0].buckets.len(), 1);
        assert_eq!(series[0].buckets[0].bucket_start_ms, 0);
        assert_eq!(series[0].buckets[0].input_tokens, 30);
        assert_eq!(series[0].buckets[0].requests, 2);
        assert_eq!(series[1].model, "gpt");
        assert_eq!(series[1].buckets[0].bucket_start_ms, 60 * 60 * 1_000);
    }

    #[test]
    fn quota_burn_uses_net_change_within_each_reset_epoch() {
        let sample = |at, used_percent, reset_at_unix| QuotaObservationRecord {
            provider: QuotaProvider::ChatGpt,
            model: QuotaModel::GPT,
            auth_namespace: None,
            observed_at: rho_core::UnixMs(at),
            used_percent,
            reset_at_unix,
        };
        let records = [
            sample(0, 10, Some(100)),
            sample(100, 15, Some(100)),
            sample(200, 13, Some(100)),
            sample(300, 3, Some(200)),
            sample(400, 6, Some(200)),
        ];
        let samples = records.iter().collect::<Vec<_>>();
        assert_eq!(quota_burn(&samples, 400, 1_000), 6);
        assert_eq!(quota_burn(&samples, 400, 150), 3);
    }

    #[test]
    fn quota_burn_does_not_sum_sample_jitter() {
        let sample = |at, used_percent| QuotaObservationRecord {
            provider: QuotaProvider::ChatGpt,
            model: QuotaModel::GPT,
            auth_namespace: None,
            observed_at: rho_core::UnixMs(at),
            used_percent,
            reset_at_unix: Some(100),
        };
        let records = [
            sample(0, 50),
            sample(100, 48),
            sample(200, 50),
            sample(300, 49),
            sample(400, 50),
        ];
        let samples = records.iter().collect::<Vec<_>>();

        assert_eq!(quota_burn(&samples, 400, 1_000), 0);
    }

    #[test]
    fn quota_burn_tolerates_reset_target_jitter() {
        let sample = |at, used_percent, reset_at_unix| QuotaObservationRecord {
            provider: QuotaProvider::ChatGpt,
            model: QuotaModel::GPT,
            auth_namespace: None,
            observed_at: rho_core::UnixMs(at),
            used_percent,
            reset_at_unix: Some(reset_at_unix),
        };
        let records = [
            sample(0, 17, 1_000),
            sample(100, 15, 1_001),
            sample(200, 17, 999),
            sample(300, 16, 1_000),
            sample(400, 17, 1_002),
        ];
        let samples = records.iter().collect::<Vec<_>>();

        assert_eq!(quota_burn(&samples, 400, 1_000), 0);
    }

    #[tokio::test]
    async fn quota_history_includes_every_stored_point() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        let now = rho_core::UnixMs::now().0;
        let mut write = db.write().await;
        for index in 0..5 {
            assert!(write.record_quota_observation(QuotaObservationRecord {
                provider: QuotaProvider::ChatGpt,
                model: QuotaModel::GPT,
                auth_namespace: Some("work".to_owned()),
                observed_at: rho_core::UnixMs(now - (4 - index) * 1_000),
                used_percent: index as u8,
                reset_at_unix: Some(123),
            }));
        }
        assert!(write.record_quota_observation(QuotaObservationRecord {
            provider: QuotaProvider::ChatGpt,
            model: QuotaModel::GPT,
            auth_namespace: Some("personal".to_owned()),
            observed_at: rho_core::UnixMs(now),
            used_percent: 50,
            reset_at_unix: Some(789),
        }));
        assert!(write.record_quota_observation(QuotaObservationRecord {
            provider: QuotaProvider::Claude,
            model: QuotaModel::OPUS,
            auth_namespace: None,
            observed_at: rho_core::UnixMs(now),
            used_percent: 25,
            reset_at_unix: Some(456),
        }));
        write.commit();

        let history = quota_history(&db);
        let gpt = history
            .iter()
            .find(|series| {
                series.model == "gpt" && series.auth_namespace.as_deref() == Some("work")
            })
            .unwrap();
        assert_eq!(gpt.points.len(), 5);
        assert_eq!(
            gpt.points
                .iter()
                .map(|point| point.remaining_percent)
                .collect::<Vec<_>>(),
            [100, 99, 98, 97, 96]
        );
        let personal = history
            .iter()
            .find(|series| {
                series.model == "gpt" && series.auth_namespace.as_deref() == Some("personal")
            })
            .unwrap();
        assert_eq!(personal.points[0].remaining_percent, 50);
        let opus = history
            .iter()
            .find(|series| series.model == "opus")
            .unwrap();
        assert_eq!(opus.points[0].remaining_percent, 75);
    }

    #[tokio::test]
    async fn quota_summary_expires_stale_provider_window() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        let now = rho_core::UnixMs::now();
        let mut write = db.write().await;
        assert!(write.record_quota_observation(QuotaObservationRecord {
            provider: QuotaProvider::Claude,
            model: QuotaModel::FABLE,
            auth_namespace: None,
            observed_at: now,
            used_percent: 99,
            reset_at_unix: Some(1),
        }));
        write.commit();

        let summary = quota_summaries(&db)
            .into_iter()
            .find(|summary| summary.model == "fable")
            .unwrap();
        assert_eq!(summary.remaining_percent, 100);
        assert_eq!(summary.burn_10m, 0);
        assert_eq!(summary.reset_at_unix, None);
    }

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

    #[test]
    fn image_input_validation_enforces_format_count_and_encoded_size() {
        let image = |media_type: &str, len: usize| ContentPart::Image {
            media_type: media_type.to_owned(),
            data: vec![0; len],
        };
        assert!(validate_image_content(&[image("image/png", 3)]).is_ok());
        assert!(
            validate_image_content(&[image("image/bmp", 3)])
                .unwrap_err()
                .to_string()
                .contains("unsupported")
        );
        assert!(
            validate_image_content(
                &(0..=MAX_INPUT_IMAGES)
                    .map(|_| image("image/png", 1))
                    .collect::<Vec<_>>()
            )
            .is_err()
        );
        let raw_over_limit = (MAX_IMAGE_BASE64_BYTES / 4) * 3 + 1;
        assert!(
            validate_image_content(&[image("image/jpeg", raw_over_limit)])
                .unwrap_err()
                .to_string()
                .contains("10 MiB")
        );
    }
}
