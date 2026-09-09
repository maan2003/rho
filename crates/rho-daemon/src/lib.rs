use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use rho_agent::MessageDelivery;
use rho_agent::db::{
    AgentId, AgentReadTxnExt as _, AgentRole, AgentUsageModel, AgentWriteTxnExt as _, QuotaModel,
    QuotaObservationRecord, QuotaProvider,
};
use rho_agent::pool::{AgentPool, RunningAgent};
use rho_core::ContentPart;
use rho_db::RhoDb;
use rho_inference::Inference;
use rho_ui_proto::server::{Server, ServerConnection};
use rho_ui_proto::{
    AgentCostSeries, AgentUsageBucket as UiAgentUsageBucket, AgentUsageSeries, AuthState,
    ClientMessage, JoinTarget, LandLeaseHolder, LandStatus, McpAgentToolRequest,
    McpAgentToolResponse, QuotaPoint, QuotaSeries, QuotaSummary, ServerMessage, StartMode,
    WorkspaceInfo, read_frame, write_frame,
};
use tokio::sync::{Mutex, Mutex as TokioMutex, Notify, OwnedMutexGuard, broadcast, mpsc, oneshot};

pub mod debug;
mod desk_cells;
mod desk_parent_labels;
mod detail;
mod realtime;
mod secret_store;
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
    store: Arc<std::sync::Mutex<Option<Arc<secret_store::SecretStore>>>>,
}

impl PlatformSecrets {
    fn from_fd_store() -> Self {
        let secrets = Self::default();
        match secret_store::SecretStore::take_from_listen_fds(PLATFORM_SECRETS_FD_STORE_NAME) {
            Ok(Some(store)) => {
                tracing::info!("reclaimed platform secrets from fd store");
                *secrets.store.lock().expect("platform secrets lock") = Some(Arc::new(store));
            }
            Ok(None) => {}
            Err(error) => tracing::error!(%error, "reclaiming platform secrets fd"),
        }
        secrets
    }

    fn current_store(&self) -> Option<Arc<secret_store::SecretStore>> {
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
    ) -> anyhow::Result<(Arc<secret_store::SecretStore>, bool)> {
        let mut merged = self.read().unwrap_or_default();
        for (key, value) in secrets {
            merged.insert(key, value);
        }
        let store = Arc::new(
            secret_store::SecretStore::create(&merged).context("sealing platform secrets")?,
        );
        let stashed = store
            .stash_in_fd_store(PLATFORM_SECRETS_FD_STORE_NAME)
            .context("stashing platform secrets in the systemd fd store")?;
        *self.store.lock().expect("platform secrets lock") = Some(store.clone());
        Ok((store, stashed))
    }
}

fn prepare_socket_path(socket_path: &Path, name: &str) -> anyhow::Result<()> {
    match std::fs::remove_file(socket_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("remove stale {name} socket {}", socket_path.display())),
    }
}

fn lock_runtime_directory(paths: &rho_ui_proto::RuntimePaths) -> anyhow::Result<std::fs::File> {
    let path = paths.daemon_lock();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open daemon runtime lock {}", path.display()))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        anyhow::bail!(
            "refusing to start: runtime directory {} is owned by another daemon (lock file {})",
            paths.directory().display(),
            path.display()
        );
    }
    Ok(lock)
}

struct RuntimeSockets {
    paths: rho_ui_proto::RuntimePaths,
    server: Server,
    _lock: std::fs::File,
}

fn spawn_octo_server(
    listener: tokio::net::UnixListener,
    secrets: PlatformSecrets,
) -> anyhow::Result<()> {
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

fn start_runtime_sockets(
    socket_path: Option<PathBuf>,
    secrets: PlatformSecrets,
) -> anyhow::Result<RuntimeSockets> {
    let paths = rho_ui_proto::RuntimePaths::new(socket_path)?;
    std::fs::create_dir_all(paths.directory()).context("create runtime directory")?;
    let lock = lock_runtime_directory(&paths)?;
    let octo_socket = paths.octo_socket();
    prepare_socket_path(paths.socket(), "rho daemon")?;
    prepare_socket_path(&octo_socket, "octo")?;
    let octo_listener = tokio::net::UnixListener::bind(&octo_socket)
        .with_context(|| format!("bind octo socket {}", octo_socket.display()))?;
    let listener = tokio::net::UnixListener::bind(paths.socket())
        .with_context(|| format!("bind rho daemon socket {}", paths.socket().display()))?;
    spawn_octo_server(octo_listener, secrets)?;
    Ok(RuntimeSockets {
        paths,
        server: Server::from_listener(listener),
        _lock: lock,
    })
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
    /// Also listen for remote UI clients over iroh (relay-backed).
    /// Remote clients must be enrolled once via
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
    /// Override the OpenAI Responses base URL (isolated QA providers).
    #[arg(long, value_name = "URL")]
    pub openai_base_url: Option<String>,
    /// Set ANTHROPIC_BASE_URL for Claude Code subprocesses.
    #[arg(long, value_name = "URL")]
    pub anthropic_base_url: Option<String>,
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
    let platform_secrets = PlatformSecrets::from_fd_store();
    let runtime = start_runtime_sockets(args.socket_path, platform_secrets.clone())?;

    // The daemon's own cwd must never matter: agents each carry their own
    // working directory. Park the process somewhere empty and read-only so
    // any code still depending on process cwd fails loudly.
    let _ = std::env::set_current_dir("/var/empty").or_else(|_| std::env::set_current_dir("/"));

    let mut user_environment = login_environment()?;
    if let Some(path) = EMBEDDED_DIRENV_PATH_BEFORE {
        user_environment.push(("RHO_DIRENV_PATH_BEFORE".into(), path.into()));
    }
    user_environment.push((FIND_DENY_ROOTS_ENV.into(), find_deny_roots()));
    user_environment.push((
        rho_ui_proto::RuntimePaths::SOCKET_ENV.into(),
        runtime.paths.socket().as_os_str().to_owned(),
    ));
    configure_octo_git_transport(&mut user_environment)?;
    if let Some(endpoint) = &args.anthropic_base_url {
        let parsed = url::Url::parse(endpoint).context("parse Anthropic base URL")?;
        anyhow::ensure!(
            matches!(parsed.scheme(), "http" | "https"),
            "Anthropic base URL must use http or https"
        );
        anyhow::ensure!(
            parsed.host().is_some(),
            "Anthropic base URL must have a host"
        );
        set_environment_value(&mut user_environment, "ANTHROPIC_BASE_URL", endpoint);
    }
    let user_environment = rho_workspaces::UserEnvironment::new(user_environment);

    let db_path = default_db_path()?;
    // The state directory is resolved here, in the binary, and handed down.
    // Nothing below this line asks `dirs` for it: a library that resolves the
    // user's state directory reads the user's own files from a test.
    let state_dir = camino::Utf8PathBuf::try_from(
        db_path
            .parent()
            .context("the database path has no directory")?
            .to_owned(),
    )
    .context("the state directory is not valid UTF-8")?;
    let db = RhoDb::open(db_path);
    // One-off (7 Sep), before any agent loop can append: every Claude
    // log the file copier wrote is rebuilt from its session file.
    let rebuilt = rho_agent::rebuild::rebuild_claude_logs(&db).await;
    eprintln!(
        "rho daemon: rebuilt {} Claude logs from their session files, closed {} queues without one (one-off)",
        rebuilt.rebuilt, rebuilt.closed
    );
    let inference = match args.openai_base_url {
        Some(endpoint) => {
            Inference::new_with_config(
                db.clone(),
                rho_inference::InferenceConfig::with_responses_base_url(endpoint)?,
            )
            .await?
        }
        None => Inference::new(db.clone()).await?,
    };
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
    let services = Arc::new(
        Services::new(
            db,
            inference,
            path_overrides,
            state_dir,
            user_environment,
            platform_secrets,
            runtime.paths.octo_socket(),
        )
        .await?,
    );
    spawn_inference_projection(Arc::clone(&services));
    // One probe per account: a subscription's headroom is the account's, and
    // agents move between accounts. The probe has no view to mount an
    // account into, so it names the account directory outright.
    for account in rho_claude::accounts::list()? {
        let quota_environment = services.user_environment.clone();
        let quota_path_overrides = quota_path_overrides.clone();
        let account_dir = rho_claude::accounts::account_dir(&account)?;
        spawn_claude_quota_recorder(
            rho_claude_usage::spawn_poller(
                move || {
                    let mut command = tokio::process::Command::new("claude");
                    quota_environment.apply(&mut command);
                    let path = quota_environment
                        .get("PATH")
                        .context("user environment has no PATH")?;
                    command.env("PATH", quota_path_overrides.add_to(path));
                    command.env("CLAUDE_CONFIG_DIR", account_dir.as_str());
                    Ok(command)
                },
                default_db_path()?.with_file_name(format!("claude-quota-probe-{account}")),
            ),
            account,
            services.db.clone(),
            services.inference.clone(),
            services.events.clone(),
        );
    }

    let iroh_listener = iroh.map(|(listener, _)| listener);

    if let Some(listener) = iroh_listener {
        tokio::spawn(run_iroh_listener(
            services.clone(),
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
                services.pool.flush_agent_usage(None).await;
                return Ok(());
            }
            connection = runtime.server.accept() => {
                let connection = connection?;
                let services = services.clone();
                let iroh_auth = iroh_auth.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve_connection(services, iroh_auth, connection).await {
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
    services: Arc<Services>,
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
        let services = services.clone();
        let iroh_auth = iroh_auth.clone();
        tokio::spawn(async move {
            // One UI control session per iroh connection; the rest of its
            // streams are dedicated (files, shells, one-shot queries).
            let control_claimed = Arc::new(AtomicBool::new(false));
            while let Ok((send, recv)) = connection.accept_bi().await {
                let services = services.clone();
                let control_claimed = control_claimed.clone();
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
                                | ClientMessage::GuiTelemetryUpload { .. }
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
                            anyhow::ensure!(
                                control_claimed
                                    .compare_exchange(
                                        false,
                                        true,
                                        Ordering::AcqRel,
                                        Ordering::Relaxed
                                    )
                                    .is_ok(),
                                "iroh connection already has a UI control session"
                            );
                            send.set_priority(1)
                                .context("set iroh control stream priority")?;
                            true
                        } else {
                            false
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
                        let result =
                            serve_connection_io(services, iroh_auth, recv, send, None, Some(first))
                                .await;
                        if control {
                            control_claimed.store(false, Ordering::Release);
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

/// One GUI's hold on a desk device.
///
/// A device is one GUI, and the CRDT gives each device its own namespace, so
/// two live writers under one device id would collide on versions. The hold
/// is therefore exclusive — but exclusive to the *newest* connection: a GUI
/// that died without closing leaves its hold behind, and the GUI the user
/// just restarted must not be the one that is refused.
struct DeskBinding {
    /// Which connection holds it, so a connection ending only lets go of a
    /// hold that is still its own.
    connection: u64,
    /// The displaced connection's writer, to tell it why it is going.
    outgoing: mpsc::UnboundedSender<ServerMessage>,
    /// Set when a newer connection takes the device. A displaced connection
    /// may not write: its mutations are refused from this moment, whether or
    /// not its socket has noticed yet.
    displaced: AtomicBool,
    /// Wakes the displaced connection's read loop so it ends rather than
    /// sitting on a socket nobody is reading.
    closed: Notify,
}

/// What a connection holds after `DeskSync`.
struct DeskSession {
    device: rho_desk::cells::DeviceId,
    node_namespace: u16,
    binding: Arc<DeskBinding>,
}

/// Everything the daemon owns that a connection may need: the agent pool,
/// the database, the stores, the locks and the brokers. It is not a
/// registry of agents — the pool is that — but the one bundle a connection
/// is handed so it does not carry a dozen handles of its own.
struct Services {
    pool: Arc<AgentPool>,
    db: RhoDb,
    desk_cells: desk_cells::DeskCellStore,
    desk_devices: Mutex<HashMap<rho_desk::cells::DeviceId, Arc<DeskBinding>>>,
    visualizations: rho_visualizations::VisualizationStore,
    inference: Inference,
    /// The database's machine seed, announced in `Ready` so clients can
    /// encode agent IDs.
    machine_seed: u64,
    land_locks: Mutex<HashMap<Utf8PathBuf, Arc<TokioMutex<()>>>>,
    land_holders: Mutex<HashMap<Utf8PathBuf, LandLeaseHolder>>,
    land_statuses: Mutex<HashMap<Utf8PathBuf, (Option<AgentId>, LandStatus)>>,
    /// Stateless PR, CI, review, and comment operations.
    pr_monitor: Arc<rho_pr_monitor::PrMonitor>,
    /// Sealed platform secret store used by Octo.
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
    /// At most one GUI owns the voice session's microphone and playback.
    voice_lease: Arc<TokioMutex<()>>,
}

impl Services {
    async fn new(
        db: RhoDb,
        inference: Inference,
        path_overrides: PathOverrides,
        state_dir: camino::Utf8PathBuf,
        user_environment: rho_workspaces::UserEnvironment,
        platform_secrets: PlatformSecrets,
        octo_socket: PathBuf,
    ) -> anyhow::Result<Self> {
        let pool = AgentPool::new(
            db.clone(),
            inference.clone(),
            path_overrides,
            state_dir,
            user_environment.clone(),
        )
        .await;
        let machine_seed = db.read().machine_seed();
        let pr_monitor =
            rho_pr_monitor::PrMonitor::new(pool.clone(), db.clone(), octo_socket).await?;
        let visualizations = rho_visualizations::VisualizationStore::new(db.clone()).await;
        let desk_cells = desk_cells::DeskCellStore::new(db.clone())
            .await
            .map_err(anyhow::Error::msg)?;
        let registry = Self {
            pool,
            db,
            desk_cells,
            desk_devices: Mutex::new(HashMap::new()),
            visualizations,
            inference,
            machine_seed,
            land_locks: Mutex::new(HashMap::new()),
            land_holders: Mutex::new(HashMap::new()),
            land_statuses: Mutex::new(HashMap::new()),
            pr_monitor,
            platform_secrets,
            events: broadcast::channel(1024).0,
            shells: Arc::new(shell::ShellRegistry::default()),
            terminals: Arc::new(terminal::TerminalRegistry::default()),
            user_environment,
            git_transport: GitTransportBroker::default(),
            voice_lease: Arc::new(TokioMutex::new(())),
        };
        Ok(registry)
    }

    fn auth_state(&self) -> AuthState {
        let state = self.inference.state();
        AuthState {
            namespaces: state.namespaces,
            disabled_namespaces: state.disabled_namespaces,
            active_namespace: state.active_namespace,
        }
    }

    async fn set_auth_account_enabled(&self, name: &str, enabled: bool) {
        self.inference.set_account_enabled(name, enabled).await;
    }

    async fn ready_message(&self) -> ServerMessage {
        let read = self.db.read();
        ServerMessage::Ready {
            auth: self.auth_state(),
            machine_seed: self.machine_seed,
            agent_counter: read.last_agent_counter(),
            journal_head: read.journal_head(),
        }
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

    async fn create(
        &self,
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
        let (agent_id, agent) = self.pool.create(role, None, start).await?;
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
        let (_, self_agent, _) = self.load(self_agent_id).await?;
        let role = self_agent.head().config.role;
        if matches!(role, AgentRole::Advisor { .. })
            && !matches!(
                &request,
                McpAgentToolRequest::MessageAgent { .. }
                    | McpAgentToolRequest::FollowupAdvisor { .. }
            )
        {
            anyhow::bail!("Advisors may only message agents");
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
                let child_record = self.load(child_id).await?.1.head();
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
                let recipient = self.resolve_display_agent_id(&agent_id).await?;
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
                let target = self.resolve_display_agent_id(&agent_id).await?;
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
            McpAgentToolRequest::AskAdvisor { message } => {
                let workdirs = self_agent
                    .head()
                    .config
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
                let advisor = self.resolve_display_agent_id(&advisor_id).await?;
                let record = self.load(advisor).await?.1.head();
                anyhow::ensure!(
                    matches!(record.config.role, AgentRole::Advisor { .. }),
                    "target is not an Advisor"
                );
                anyhow::ensure!(
                    self.db.read().agent_parent(advisor) == Some(self_agent_id),
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

    async fn resolve_display_agent_id(&self, agent_id: &str) -> anyhow::Result<AgentId> {
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
            let expected = self
                .load(resolved)
                .await?
                .1
                .head()
                .config
                .role
                .handle_prefix();
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

    async fn load(&self, agent_id: AgentId) -> anyhow::Result<(AgentId, RunningAgent, bool)> {
        self.pool.load(agent_id).await
    }
}

async fn serve_connection(
    services: Arc<Services>,
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
    serve_connection_io(services, iroh_auth, reader, writer, land_holder, None).await
}

/// One UI protocol session over any framed byte stream (Unix socket or an
/// iroh bi-stream from an enrolled remote client).
async fn serve_connection_io<R, W>(
    services: Arc<Services>,
    iroh_auth: Option<rho_iroh_auth::IrohAuth>,
    reader: R,
    writer: W,
    land_holder: Option<LandLeaseHolder>,
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
        return serve_workspace_channel(services, reader, writer, workspace).await;
    }
    if let ClientMessage::RealtimeOpen { offer_sdp } = first {
        return realtime::serve(services, reader, writer, offer_sdp).await;
    }
    if let ClientMessage::DiffSnapshot {
        workspace,
        known_commit_id,
        include_paths,
    } = first
    {
        return serve_diff_snapshot(services, writer, workspace, known_commit_id, include_paths)
            .await;
    }
    if let ClientMessage::DiffBaseContents {
        workspace,
        operation_id,
        commit_id,
        paths,
    } = first
    {
        return serve_diff_base_contents(
            services,
            writer,
            workspace,
            operation_id,
            commit_id,
            paths,
        )
        .await;
    }
    if let ClientMessage::GuiTelemetryUpload { snapshot } = first {
        return serve_gui_telemetry_upload(writer, snapshot).await;
    }
    if let ClientMessage::VisualizationGet { id } = first {
        let mut writer = writer;
        let response = match services.visualizations.get(&id) {
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
        return serve_terminal(
            services,
            reader,
            writer,
            agent,
            terminal_id,
            open,
            cols,
            rows,
        )
        .await;
    }
    if let ClientMessage::TerminalAttach {
        agent,
        terminal_id,
        cols,
        rows,
    } = first
    {
        let open = TerminalOpenKind::Attach;
        return serve_terminal(
            services,
            reader,
            writer,
            agent,
            terminal_id,
            open,
            cols,
            rows,
        )
        .await;
    }
    if let ClientMessage::TerminalList { agent } = first {
        return serve_terminal_list(services, writer, agent).await;
    }
    if let ClientMessage::ShellAttach { agent } = first {
        return serve_shell(services, reader, writer, agent).await;
    }
    if let ClientMessage::GitTransportRequest { request } = first {
        return serve_git_transport_request(services, reader, writer, request).await;
    }
    if let ClientMessage::GitTransportProvide {
        request_id,
        provider_id,
        claim,
    } = first
    {
        return serve_git_transport_provider(
            services,
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
            host == "github.com" && services.platform_secrets.contains_nonempty("GITHUB_TOKEN");
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

    // Creations update lightweight registry summaries. Subscribe before
    // building Ready so a concurrent creation is either in its snapshot or
    // arrives on this receiver (occasionally both, harmlessly).
    let mut created_rx = services.pool.subscribe_created();
    let mut events_rx = services.events.subscribe();
    let _ = outgoing_tx.send(services.ready_message().await);
    // Names this connection's wants in the pool's live set, so they leave
    // with it.
    let connection_id = NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
    let mut log_follow: Option<tokio::task::JoinHandle<()>> = None;
    let mut desk_session = None;

    // Announce every agent created in the pool — by clients or by other
    // agents spawning children — so it shows up on this connection.
    {
        let services = Arc::clone(&services);
        let outgoing_tx = outgoing_tx.clone();
        tokio::spawn(async move {
            loop {
                match created_rx.recv().await {
                    Ok(created) => {
                        if outgoing_tx
                            .send(ServerMessage::AgentCreated {
                                agent_id: created.agent_id,
                            })
                            .is_err()
                            || outgoing_tx.send(services.ready_message().await).is_err()
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if outgoing_tx.send(services.ready_message().await).is_err() {
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
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if events_tx.send(ServerMessage::DeskResyncRequired).is_err() {
                        break;
                    }
                }
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
                // A displaced connection stops here rather than sitting on a
                // socket nobody is reading: the GUI that held this device has
                // been told, and the window that took it is the live one.
                let displaced = desk_session
                    .as_ref()
                    .map(|session: &DeskSession| Arc::clone(&session.binding));
                let frame = rho_ui_proto::read_frame_optional::<_, ClientMessage>(&mut reader);
                let read = match displaced {
                    Some(binding) => {
                        tokio::select! {
                            biased;
                            () = binding.closed.notified() => None,
                            read = frame => Some(read),
                        }
                    }
                    None => Some(frame.await),
                };
                match read {
                    None => {
                        for (repo, _) in &land_leases {
                            services.clear_land_holder(repo).await;
                        }
                        break Ok(());
                    }
                    Some(Ok(Some(message))) => message,
                    Some(Ok(None)) => {
                        for (repo, _) in &land_leases {
                            services.clear_land_holder(repo).await;
                        }
                        break Ok(());
                    }
                    Some(Err(error)) => {
                        for (repo, _) in &land_leases {
                            services.clear_land_holder(repo).await;
                        }
                        break Err(error);
                    }
                }
            }
        };
        match handle_message(
            &services,
            iroh_auth.as_ref(),
            &outgoing_tx,
            &mut land_leases,
            land_holder.clone(),
            connection_id,
            &mut log_follow,
            &mut desk_session,
            message,
        )
        .await
        {
            Ok(Refresh::Ready) => {
                // Registry changes show on every client (GUI rails and a
                // waiting CLI), so the refreshed snapshot goes through
                // the daemon-wide event fanout, not just this connection.
                let _ = services.events.send(services.ready_message().await);
            }
            Ok(Refresh::None) => {}
            Err(error) => {
                // The whole chain, not just the outermost context: a new
                // agent that failed said "create managed jj workspace" and
                // kept the reason to itself, which is not something a
                // reader can act on.
                let _ = outgoing_tx.send(ServerMessage::Error {
                    message: format!("{error:#}"),
                });
            }
        }
    };
    // Let go of the device only if the hold is still this connection's: a
    // newer window may have taken it, and ending must not unbind theirs.
    if let Some(session) = desk_session {
        let mut devices = services.desk_devices.lock().await;
        if devices
            .get(&session.device)
            .is_some_and(|held| Arc::ptr_eq(held, &session.binding))
        {
            devices.remove(&session.device);
        }
    }
    events_task.abort();
    if let Some(log_follow) = log_follow {
        log_follow.abort();
    }
    services
        .pool
        .set_live_wants(connection_id, HashSet::new())
        .await;
    result
}

static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

async fn serve_git_transport_request<R, W>(
    services: Arc<Services>,
    reader: R,
    mut writer: W,
    request: rho_ui_proto::GitTransportRequest,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let provider = match services.git_transport.request(request).await {
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
    services: Arc<Services>,
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
    match services
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

/// Durable presentation changes refresh the normal snapshot for every
/// connection. Broadcast loss is harmless because `Ready` is reconstructed
/// from the agent cache, including after daemon restart.
fn spawn_inference_projection(services: Arc<Services>) {
    let mut state = services.inference.subscribe();
    let services = Arc::downgrade(&services);
    tokio::spawn(async move {
        while state.changed().await.is_ok() {
            let Some(services) = services.upgrade() else {
                break;
            };
            let _ = state.borrow_and_update();
            let _ = services.events.send(ServerMessage::AuthState {
                auth: services.auth_state(),
            });
            let _ = services.events.send(ServerMessage::QuotaUsage {
                summaries: combined_quota_summaries(&services.db, &services.inference),
            });
        }
    });
}

fn combined_quota_summaries(db: &RhoDb, inference: &Inference) -> Vec<QuotaSummary> {
    let mut summaries = quota_summaries(db);
    summaries.extend(
        inference
            .state()
            .quotas
            .into_iter()
            .map(|summary| QuotaSummary {
                model: "gpt".to_owned(),
                auth_namespace: Some(summary.auth_namespace),
                remaining_percent: summary.remaining_percent,
                burn_10m: summary.burn_10m,
                burn_2h: summary.burn_2h,
                burn_1d: summary.burn_1d,
                burn_3d: summary.burn_3d,
                reset_at_unix: summary.reset_at_unix,
            }),
    );
    summaries
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
        AgentUsageModel::GEMINI,
        AgentUsageModel::ASTRA,
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

fn hourly_agent_cost_series(
    db: &RhoDb,
    since: rho_core::UnixMs,
) -> anyhow::Result<Vec<AgentCostSeries>> {
    const MAX_HOURLY_AGENT_COST_BUCKETS: usize = 500_000;

    let read = db.read();
    let mut hourly = BTreeMap::new();
    for agent_id in read.list_agent_ids() {
        for bucket in read.agent_usage(agent_id, since) {
            if !matches!(
                bucket.model,
                AgentUsageModel::GPT
                    | AgentUsageModel::ASTRA
                    | AgentUsageModel::TERRA
                    | AgentUsageModel::LUNA
                    | AgentUsageModel::UNKNOWN
            ) {
                continue;
            }
            merge_hourly_agent_cost_bucket(
                &mut hourly,
                agent_id,
                bucket,
                MAX_HOURLY_AGENT_COST_BUCKETS,
            )?;
        }
    }

    let mut series = BTreeMap::<(AgentId, AgentUsageModel), Vec<UiAgentUsageBucket>>::new();
    for ((agent_id, model, _), bucket) in hourly {
        series
            .entry((agent_id, model))
            .or_default()
            .push(ui_agent_usage_bucket(bucket));
    }
    Ok(series
        .into_iter()
        .map(|((agent_id, model), buckets)| AgentCostSeries {
            agent_id,
            model: model.name().to_owned(),
            buckets,
        })
        .collect())
}

fn merge_hourly_agent_cost_bucket(
    hourly: &mut BTreeMap<(AgentId, AgentUsageModel, u64), rho_agent::db::AgentUsageBucket>,
    agent_id: AgentId,
    bucket: rho_agent::db::AgentUsageBucket,
    max_buckets: usize,
) -> anyhow::Result<()> {
    const HOUR_MS: u64 = 60 * 60 * 1_000;

    let bucket_start_ms = bucket.bucket_start_ms / HOUR_MS * HOUR_MS;
    hourly
        .entry((agent_id, bucket.model, bucket_start_ms))
        .or_insert_with(|| rho_agent::db::AgentUsageBucket {
            bucket_start_ms,
            model: bucket.model,
            ..rho_agent::db::AgentUsageBucket::default()
        })
        .add(&bucket);
    anyhow::ensure!(
        hourly.len() <= max_buckets,
        "agent cost history exceeds {max_buckets} hourly buckets"
    );
    Ok(())
}

fn quota_history(db: &RhoDb, inference: &Inference) -> Vec<QuotaSeries> {
    let mut series = claude_quota_history(db);
    let since = rho_core::UnixMs(
        rho_core::UnixMs::now()
            .0
            .saturating_sub(30 * 24 * 60 * 60 * 1_000),
    );
    for history in inference.quota_history(since) {
        series.push(QuotaSeries {
            model: "gpt".to_owned(),
            auth_namespace: Some(history.auth_namespace),
            points: history
                .points
                .into_iter()
                .map(|point| rho_ui_proto::QuotaPoint {
                    observed_at_ms: point.observed_at.0,
                    remaining_percent: point.remaining_percent,
                    reset_at_unix: point.reset_at_unix,
                })
                .collect(),
        });
    }
    series
}

fn claude_quota_history(db: &RhoDb) -> Vec<QuotaSeries> {
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
    for model in [QuotaModel::OPUS, QuotaModel::FABLE] {
        for observation in read.quota_observations(model, since) {
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

fn claude_accounts_message(db: &RhoDb) -> anyhow::Result<ServerMessage> {
    Ok(ServerMessage::ClaudeAccounts {
        accounts: rho_claude::accounts::list()?,
        current: db.read().claude_account(),
    })
}

fn spawn_claude_quota_recorder(
    mut updates: tokio::sync::mpsc::Receiver<anyhow::Result<rho_claude_usage::ClaudeUsage>>,
    account: String,
    db: RhoDb,
    inference: Inference,
    events: broadcast::Sender<ServerMessage>,
) {
    tokio::spawn(async move {
        while let Some(update) = updates.recv().await {
            let usage = match update {
                Ok(usage) => usage,
                Err(error) => {
                    tracing::warn!(%error, %account, "Claude quota probe failed");
                    continue;
                }
            };
            let observed_at = rho_core::UnixMs::now();
            let mut write = db.write().await;
            let mut changed = write.record_quota_observation(QuotaObservationRecord {
                provider: QuotaProvider::Claude,
                model: QuotaModel::OPUS,
                auth_namespace: Some(account.clone()),
                observed_at,
                used_percent: usage.all_models.used_percent,
                reset_at_unix: Some(usage.all_models.reset_at_unix),
            });
            changed |= write.record_quota_observation(QuotaObservationRecord {
                provider: QuotaProvider::Claude,
                model: QuotaModel::FABLE,
                auth_namespace: Some(account.clone()),
                observed_at,
                used_percent: usage.fable.used_percent,
                reset_at_unix: Some(usage.fable.reset_at_unix),
            });
            write.commit();
            if changed {
                let _ = events.send(ServerMessage::QuotaUsage {
                    summaries: combined_quota_summaries(&db, &inference),
                });
            }
        }
    });
}

/// Wakes a snoozed agent: at `until`, rebroadcasts its (by then pending)
/// level. Harmless if the disposition changed meanwhile — it just sends the
/// then-current level.
/// How many journal entries travel in one [`ServerMessage::Log`] while a
/// client is catching up. A cold client's first copy is a whole history,
/// so it goes in pages the connection can interleave.
const LOG_PAGE: usize = 512;

/// Sends this connection every journal entry after `since`, then follows
/// the feed: each new row as it is appended, on any agent, and every live
/// delta any loop tells, in the order they happened.
///
/// Contiguous by seq is the whole contract for rows. The daemon remembers
/// the last seq it sent; an append that is not the next one, or a lagged
/// subscription, sends it back to the journal from there. Rows the
/// mirror leaves behind (`strip` says nothing) advance the seq without a
/// message.
///
/// Live deltas are forwarded only once the loops have been asked to tell
/// their tails whole, which happens after the catch-up: a delta from
/// before that would be an append to a tail the client does not hold.
/// After a lag the same is done again, since deltas were lost.
fn spawn_log_follow(
    services: Arc<Services>,
    outgoing_tx: mpsc::UnboundedSender<ServerMessage>,
    since: rho_ui_proto::mirror::Seq,
) -> tokio::task::JoinHandle<()> {
    use rho_agent::mirror::Feed;
    tokio::spawn(async move {
        // Subscribed before the catch-up read, so a row appended during it
        // is queued rather than lost; the seq drops the duplicates.
        let mut feed = rho_agent::mirror::feed(&services.db);
        let mut sent = since;
        if !send_journal_from(&services.db, &outgoing_tx, &mut sent).await {
            return;
        }
        let mut told = false;
        services.pool.tell_tails().await;
        loop {
            match feed.recv().await {
                Ok(Feed::Live { agent_id, live }) => {
                    // The first whole tell for a loop starts with a phase
                    // (`Requesting`, `Waiting`, `Idle`); anything before
                    // one is from before the ask and is dropped.
                    if !told {
                        told = !matches!(
                            live,
                            rho_ui_proto::mirror::Live::Item { .. }
                                | rho_ui_proto::mirror::Live::Appended { .. }
                        );
                        if !told {
                            continue;
                        }
                    }
                    if outgoing_tx
                        .send(ServerMessage::Live { agent_id, live })
                        .is_err()
                    {
                        return;
                    }
                }
                Ok(Feed::Appended(appended)) => {
                    if appended.seq <= sent {
                        continue;
                    }
                    if appended.seq != sent.next() {
                        if !send_journal_from(&services.db, &outgoing_tx, &mut sent).await {
                            return;
                        }
                        continue;
                    }
                    sent = appended.seq;
                    if let Some(entry) = appended.entry()
                        && outgoing_tx
                            .send(ServerMessage::Log {
                                entries: vec![entry],
                            })
                            .is_err()
                    {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if !send_journal_from(&services.db, &outgoing_tx, &mut sent).await {
                        return;
                    }
                    told = false;
                    services.pool.tell_tails().await;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}

/// Pages the journal out from after `sent`, moving it as it goes. False
/// when the connection is gone.
async fn send_journal_from(
    db: &RhoDb,
    outgoing_tx: &mpsc::UnboundedSender<ServerMessage>,
    sent: &mut rho_ui_proto::mirror::Seq,
) -> bool {
    loop {
        let page = db.read().journal_since(*sent, LOG_PAGE);
        let Some((last, _, _, _)) = page.last() else {
            return true;
        };
        *sent = *last;
        let entries = page
            .into_iter()
            .filter_map(|(seq, agent_id, pos, event)| {
                Some(rho_ui_proto::mirror::LogEntry {
                    seq,
                    agent_id,
                    pos: pos.into(),
                    event: rho_agent::mirror::strip(&event)?,
                })
            })
            .collect::<Vec<_>>();
        if !entries.is_empty() && outgoing_tx.send(ServerMessage::Log { entries }).is_err() {
            return false;
        }
        // Catching up must never starve the connection's own traffic.
        tokio::task::yield_now().await;
    }
}

/// Whether a handled message changed registry state that clients see through
/// `Ready` (agents and workdirs); `Ready` refreshes every
/// connection, so all clients converge on the change at once.
enum Refresh {
    Ready,
    None,
}

/// One client request. `Err` becomes a [`ServerMessage::Error`]; extra replies
/// (creation events, pongs) are sent inline before the caller's `Ready`.
#[allow(clippy::too_many_arguments)]
async fn handle_message(
    services: &Arc<Services>,
    iroh_auth: Option<&rho_iroh_auth::IrohAuth>,
    outgoing_tx: &mpsc::UnboundedSender<ServerMessage>,
    land_leases: &mut Vec<(Utf8PathBuf, OwnedMutexGuard<()>)>,
    land_holder: Option<LandLeaseHolder>,
    connection_id: u64,
    log_follow: &mut Option<tokio::task::JoinHandle<()>>,
    desk_session: &mut Option<DeskSession>,
    message: ClientMessage,
) -> anyhow::Result<Refresh> {
    match message {
        ClientMessage::Ping => {
            let _ = outgoing_tx.send(ServerMessage::Pong);
            Ok(Refresh::None)
        }
        ClientMessage::DeskSync {
            device,
            known,
            store,
            bodies,
        } => {
            if desk_session
                .as_ref()
                .is_some_and(|session| session.device != device)
            {
                anyhow::bail!("Desk connection is already bound to another device");
            }
            let node_namespace = services
                .desk_cells
                .node_namespace(device)
                .await
                .map_err(anyhow::Error::msg)?;
            // The client says which store it counted `known` in. If that
            // is not this store, the answer is the whole of this one: the
            // client's numbers were counted elsewhere, and a difference
            // taken from them would leave it holding a desk made of two
            // stores at once.
            let (store, delta) = services
                .desk_cells
                .sync_for(store, &known)
                .map_err(anyhow::Error::msg)?;
            let binding = match desk_session.take() {
                // This connection already holds the device: syncing again is
                // a resync, not a second writer.
                Some(session) => session.binding,
                None => {
                    let binding = Arc::new(DeskBinding {
                        connection: connection_id,
                        outgoing: outgoing_tx.clone(),
                        displaced: AtomicBool::new(false),
                        closed: Notify::new(),
                    });
                    // Newest wins. A device is one GUI, so a hold that is
                    // still standing belongs to a GUI that has died or is
                    // stale — the user has just restarted theirs, and
                    // refusing it would leave them with no desk until the
                    // transport gave up on the old connection, which over
                    // iroh is the ten minutes of
                    // `rho_iroh_auth::AUTHENTICATED_IDLE_TIMEOUT`.
                    if let Some(held) = services
                        .desk_devices
                        .lock()
                        .await
                        .insert(device, Arc::clone(&binding))
                        && held.connection != connection_id
                    {
                        held.displaced.store(true, Ordering::SeqCst);
                        let _ = held.outgoing.send(ServerMessage::Error {
                            message: "The desk moved to a newer window on this device".into(),
                        });
                        held.closed.notify_one();
                    }
                    binding
                }
            };
            *desk_session = Some(DeskSession {
                device,
                node_namespace,
                binding,
            });
            let _ = outgoing_tx.send(ServerMessage::DeskSynced {
                store,
                node_namespace,
                delta,
                bodies: services.desk_cells.bodies_since(&bodies),
            });
            Ok(Refresh::None)
        }
        ClientMessage::DeskCellsApply { cells } => {
            // The other half of the handshake. The same two conditions as a
            // mutation, and for the same reason: they are about this
            // connection, not about what the cells say.
            let Some(session) = desk_session.as_ref() else {
                anyhow::bail!("Desk connection must sync before writing");
            };
            anyhow::ensure!(
                !session.binding.displaced.load(Ordering::SeqCst),
                "The desk moved to a newer window on this device"
            );
            match services.desk_cells.apply_cells(cells).await {
                Ok(()) => {
                    let frontier = services.desk_cells.frontier().map_err(anyhow::Error::msg)?;
                    let _ = services
                        .events
                        .send(ServerMessage::DeskCellsAvailable { frontier });
                }
                Err(error) => {
                    tracing::warn!(%error, device = ?session.device,
                        "a client's catch-up cells did not merge");
                }
            }
            Ok(Refresh::None)
        }
        ClientMessage::DeskMutationApply { mutation } => {
            let stamp = mutation.stamp;
            // Nothing here is a verdict on what the user wrote: the two
            // conditions below are about this connection, and they break it
            // the same way the text path does. The desk is the client's, and
            // the daemon holds a copy so that clients can sync through it.
            let Some(session) = desk_session.as_ref() else {
                anyhow::bail!("Desk connection must sync before writing");
            };
            // Displaced, so this connection's device id belongs to another
            // window now: writing under it would put two authors in one
            // CRDT namespace.
            anyhow::ensure!(
                !session.binding.displaced.load(Ordering::SeqCst),
                "The desk moved to a newer window on this device"
            );
            let device = session.device;
            match services.desk_cells.apply_mutation(device, mutation).await {
                // No answer goes back. The write was done on the client
                // when the client made it; what the other devices need is
                // the poke that says there is something to sync.
                Ok(()) => {
                    let frontier = services.desk_cells.frontier().map_err(anyhow::Error::msg)?;
                    let _ = services
                        .events
                        .send(ServerMessage::DeskCellsAvailable { frontier });
                }
                // What is left is a mutation that could not be decoded into
                // the store at all. There is no answer for it any more, and
                // the client is not waiting for one; the log is where it
                // goes.
                Err(error) => {
                    tracing::warn!(%error, device = ?device, version = stamp.version,
                        "a desk mutation did not merge");
                }
            }
            Ok(Refresh::None)
        }
        ClientMessage::DeskTextApply {
            id,
            operation,
            transaction,
        } => {
            let Some(session) = desk_session.as_ref() else {
                anyhow::bail!("Desk connection must sync before writing text");
            };
            anyhow::ensure!(
                !session.binding.displaced.load(Ordering::SeqCst),
                "The desk moved to a newer window on this device"
            );
            let namespace = session.node_namespace;
            if services
                .desk_cells
                .apply_body(
                    namespace,
                    id.clone(),
                    operation.clone(),
                    transaction.clone(),
                )
                .await
                .map_err(anyhow::Error::msg)?
            {
                let _ = services.events.send(ServerMessage::DeskTextApplied {
                    id,
                    operation,
                    transaction,
                });
            }
            Ok(Refresh::None)
        }
        ClientMessage::ClaudeAccounts => {
            let _ = outgoing_tx.send(claude_accounts_message(&services.db)?);
            Ok(Refresh::None)
        }
        ClientMessage::SetClaudeAccount { name } => {
            // The account has to be there before an agent tries to mount it;
            // a switch to a name with no directory would fail at the next
            // turn of every agent at once.
            rho_claude::accounts::bootstrap(&name)?;
            let mut write = services.db.write().await;
            write.set_claude_account(&name);
            write.commit();
            let _ = outgoing_tx.send(claude_accounts_message(&services.db)?);
            Ok(Refresh::None)
        }
        ClientMessage::RecordVisualization { mime_type, content } => {
            let id = services.visualizations.record(mime_type, content).await?;
            let _ = outgoing_tx.send(ServerMessage::VisualizationRecorded { id });
            Ok(Refresh::None)
        }
        ClientMessage::ChatGptUsage => {
            let _ = outgoing_tx.send(ServerMessage::QuotaUsage {
                summaries: combined_quota_summaries(&services.db, &services.inference),
            });
            Ok(Refresh::None)
        }
        ClientMessage::QuotaHistory => {
            let _ = outgoing_tx.send(ServerMessage::QuotaHistory {
                series: quota_history(&services.db, &services.inference),
            });
            Ok(Refresh::None)
        }
        ClientMessage::GlobalUsage { since_ms } => {
            services.pool.flush_agent_usage(None).await;
            let usage = services
                .db
                .read()
                .global_agent_usage(rho_core::UnixMs(since_ms));
            let series = hourly_global_usage_series(usage);
            let _ = outgoing_tx.send(ServerMessage::GlobalUsage { series });
            Ok(Refresh::None)
        }
        ClientMessage::AgentCostDistribution { since_ms } => {
            const DAY_MS: u64 = 24 * 60 * 60 * 1_000;
            const MAX_HISTORY_DAYS: u64 = 30 + 14 + rho_ui_proto::AGENT_COST_WINDOW_DAYS;

            services.pool.flush_agent_usage(None).await;
            let now = rho_core::UnixMs::now().0;
            let earliest = since_ms
                .saturating_sub(rho_ui_proto::AGENT_COST_WINDOW_DAYS * DAY_MS)
                .max(now.saturating_sub(MAX_HISTORY_DAYS * DAY_MS));
            let response = match hourly_agent_cost_series(&services.db, rho_core::UnixMs(earliest))
            {
                Ok(series) => ServerMessage::AgentCostDistribution { series },
                Err(error) => ServerMessage::Error {
                    message: error.to_string(),
                },
            };
            let _ = outgoing_tx.send(response);
            Ok(Refresh::None)
        }
        ClientMessage::ShellStart { request_id, agent } => {
            let services = Arc::clone(services);
            let outgoing_tx = outgoing_tx.clone();
            tokio::spawn(async move {
                let response = match shell_start(&services, &agent).await {
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
            let response = match shell_list(services, agent.as_deref()).await {
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
            let services = Arc::clone(services);
            let outgoing_tx = outgoing_tx.clone();
            tokio::spawn(async move {
                let response = match shell_close(&services, &agent).await {
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
            services.git_transport.register(outgoing_tx.clone()).await;
            Ok(Refresh::None)
        }
        ClientMessage::PlatformSecretsSet { secrets } => {
            let wants_octo = secrets.iter().any(|(key, _)| key == "GITHUB_TOKEN");
            let (running, detail) = match services.platform_secrets.install_merge(secrets) {
                Ok((store, stashed)) => {
                    let persistence = if stashed {
                        " and stashed in the systemd fd store"
                    } else {
                        " (no systemd notify socket: they will not survive a daemon restart)"
                    };
                    if wants_octo && store.read()?.contains_key("GITHUB_TOKEN") {
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
                    } => services
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
                    rho_ui_proto::PrCommand::Status { url } => services
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
                    } => services
                        .pr_monitor
                        .comment(&url, reply_comment, &body)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_ui_proto::PrCommand::Comments { url } => services
                        .pr_monitor
                        .comments(&url)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_ui_proto::PrCommand::Checks { url } => services
                        .pr_monitor
                        .checks(&url)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_ui_proto::PrCommand::Edit {
                        url,
                        base,
                        title,
                        body,
                    } => services
                        .pr_monitor
                        .edit(&url, base, title, body)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_ui_proto::PrCommand::Rerun { url, run_id } => services
                        .pr_monitor
                        .rerun(&url, run_id)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_ui_proto::PrCommand::Logs { url, run_id } => {
                        services.pr_monitor.logs(&url, run_id).await.map(|data| {
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
            role,
            start,
            mut content,
        } => {
            if let Some(content) = content.as_mut() {
                prepare_image_content(content).await?;
            }
            // Subscription and the AgentCreated announcement ride the pool's
            // creation broadcast (all connections, including this one).
            let (_, agent) = services.create(role, start).await?;
            if let Some(content) = content {
                // The agent is fresh, so the lanes are equivalent here.
                agent
                    .send_user_content_accepted(content, MessageDelivery::NextRequest)
                    .await?;
            }
            Ok(Refresh::Ready)
        }
        ClientMessage::AcquireLandLease { repo, agent_id } => {
            let lock = services.land_lock(repo.clone()).await;
            let lease = match lock.clone().try_lock_owned() {
                Ok(lease) => lease,
                Err(_) => {
                    services
                        .set_land_status(repo.clone(), agent_id, LandStatus::Queued)
                        .await;
                    let holder = services.land_holder(&repo).await;
                    let _ = outgoing_tx.send(ServerMessage::LandLeaseQueued {
                        repo: repo.clone(),
                        holder,
                    });
                    lock.lock_owned().await
                }
            };
            if let Some(holder) = land_holder {
                services.set_land_holder(repo.clone(), holder).await;
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
            services
                .set_land_status(repo.clone(), agent_id, status.clone())
                .await;
            let _ = services.events.send(ServerMessage::LandStatus {
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
                services.clear_land_holder(&repo).await;
            }
            Ok(Refresh::None)
        }
        ClientMessage::AgentStreamFocus { agent_ids } => {
            anyhow::ensure!(agent_ids.len() <= 64, "too many focused agents");
            // Focus is what this client is looking at, nothing more: it
            // never loads an agent. The pool unions it across connections
            // into the live set; a loaded agent in it tells its tail.
            services
                .pool
                .set_live_wants(connection_id, agent_ids.into_iter().collect())
                .await;
            Ok(Refresh::None)
        }
        ClientMessage::Follow { since } => {
            if let Some(previous) = log_follow.take() {
                previous.abort();
            }
            *log_follow = Some(spawn_log_follow(
                Arc::clone(services),
                outgoing_tx.clone(),
                since,
            ));
            Ok(Refresh::None)
        }
        ClientMessage::Detail {
            agent_id,
            pos,
            more,
        } => {
            // One answer per position, each naming its own `pos`. A chunk
            // asks once and is answered as many times as it asked for.
            for pos in std::iter::once(pos).chain(more) {
                let body = agent_detail(&services.db, agent_id, pos);
                let _ = outgoing_tx.send(ServerMessage::Detail {
                    agent_id,
                    pos,
                    body,
                });
            }
            Ok(Refresh::None)
        }
        ClientMessage::SendUserMessage {
            agent_id,
            mut content,
            delivery,
        } => {
            prepare_image_content(&mut content).await?;
            let (_, agent, _) = services.load(agent_id).await?;
            agent.send_user_content_accepted(content, delivery).await?;
            Ok(Refresh::None)
        }
        // A compaction rides the next request whichever lane the client
        // named; the lane is not a thing the runtime reads for it.
        ClientMessage::CompactAgent {
            agent_id,
            delivery: _,
        } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.compact();
            Ok(Refresh::None)
        }
        ClientMessage::ChangeAgentRole { agent_id, role } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.change_role(role).await?;
            Ok(Refresh::Ready)
        }
        ClientMessage::ChangePromptCacheKey { agent_id } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.change_prompt_cache_key()?;
            Ok(Refresh::None)
        }
        ClientMessage::SetAuthAccountEnabled { name, enabled } => {
            services.set_auth_account_enabled(&name, enabled).await;
            Ok(Refresh::None)
        }
        ClientMessage::CancelTurn { agent_id } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.cancel();
            let _ = outgoing_tx.send(ServerMessage::TurnCancelled { agent_id });
            Ok(Refresh::None)
        }
        ClientMessage::RewindAgent { agent_id, turns } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.rewind(turns).await?;
            Ok(Refresh::Ready)
        }
        ClientMessage::ContinueTurn { agent_id } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.retry();
            Ok(Refresh::None)
        }
        ClientMessage::McpAgentTool {
            request_id,
            self_agent_id,
            request,
        } => {
            let result = services.mcp_agent_tool(self_agent_id, request).await;
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
        | ClientMessage::GuiTelemetryUpload { .. }
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
    services: Arc<Services>,
    mut reader: R,
    mut writer: W,
    agent: String,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let client = shell_attach(&services, &agent).await;
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

async fn shell_start(services: &Arc<Services>, agent: &str) -> anyhow::Result<()> {
    let agent_id = services.resolve_display_agent_id(agent).await?;
    let record = services.load(agent_id).await?.1.head();
    shell::ensure_supported_workdirs(&record.config.workdirs)?;
    let view = services
        .pool
        .materialize_view(&record.config.workdirs)
        .await
        .context("materialize agent view")?;
    services
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

async fn shell_attach(services: &Arc<Services>, agent: &str) -> anyhow::Result<shell::ShellClient> {
    let agent_id = services.resolve_display_agent_id(agent).await?;
    services.shells.attach(agent_id).await
}

async fn shell_list(
    services: &Arc<Services>,
    agent: Option<&str>,
) -> anyhow::Result<Vec<rho_ui_proto::shell::ShellInfo>> {
    let filter = match agent {
        Some(agent) => Some(services.resolve_display_agent_id(agent).await?),
        None => None,
    };
    Ok(services
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

async fn shell_close(services: &Arc<Services>, agent: &str) -> anyhow::Result<()> {
    let agent_id = services.resolve_display_agent_id(agent).await?;
    services.shells.close(agent_id).await
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
    services: Arc<Services>,
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
        let workspace = services.pool.open_workspace(&workspace).await?;
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

async fn serve_gui_telemetry_upload<W>(mut writer: W, snapshot: Vec<u8>) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let response = if snapshot.len() > rho_ui_proto::MAX_GUI_TELEMETRY_BYTES {
        ServerMessage::GuiTelemetryRefused {
            reason: format!(
                "GUI telemetry snapshot is too large ({} bytes; limit is {} bytes)",
                snapshot.len(),
                rho_ui_proto::MAX_GUI_TELEMETRY_BYTES
            ),
        }
    } else {
        let result = tokio::task::spawn_blocking(move || {
            let state = dirs::state_dir().context("state directory not available")?;
            persist_gui_telemetry(&state.join("rho"), &snapshot)
        })
        .await
        .context("GUI telemetry storage task failed")?;
        match result {
            Ok(path) => ServerMessage::GuiTelemetryStored {
                path: path.display().to_string(),
            },
            Err(error) => ServerMessage::GuiTelemetryRefused {
                reason: format!("failed to store GUI telemetry: {error:#}"),
            },
        }
    };
    write_frame(&mut writer, &response).await
}

fn persist_gui_telemetry(state_root: &std::path::Path, snapshot: &[u8]) -> anyhow::Result<PathBuf> {
    use std::io::Write as _;

    anyhow::ensure!(
        snapshot.len() <= rho_ui_proto::MAX_GUI_TELEMETRY_BYTES,
        "GUI telemetry snapshot exceeds the {} byte limit",
        rho_ui_proto::MAX_GUI_TELEMETRY_BYTES
    );
    let directory = state_root.join("gui-telemetry");
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("create {}", directory.display()))?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    for suffix in 0_u16..=u16::MAX {
        let path = directory.join(format!("gui-telemetry-{timestamp}-{suffix}.json"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(snapshot)
                    .with_context(|| format!("write {}", path.display()))?;
                file.sync_all()
                    .with_context(|| format!("sync {}", path.display()))?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).with_context(|| format!("create {}", path.display())),
        }
    }
    anyhow::bail!("could not allocate a unique GUI telemetry filename")
}

/// Persists one jj working-copy snapshot and serves its bounded parent-side
/// manifest on a dedicated stream, avoiding control-session head-of-line
/// blocking.
async fn serve_diff_snapshot<W>(
    services: Arc<Services>,
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
        let workspace = services.pool.open_workspace(&workspace).await?;
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
    services: Arc<Services>,
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
    let attached = terminal_attach(&services, &agent, terminal_id, create, cols, rows).await;
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
    services: &Arc<Services>,
    agent: &str,
    terminal_id: u64,
    create: bool,
    cols: u16,
    rows: u16,
) -> anyhow::Result<terminal::TerminalClient> {
    let agent_id = services.resolve_display_agent_id(agent).await?;
    if !create {
        return services
            .terminals
            .attach(agent_id, terminal_id, cols, rows)
            .await;
    }
    let record = services.load(agent_id).await?.1.head();
    anyhow::ensure!(
        !record
            .config
            .workdirs
            .iter()
            .any(|workdir| matches!(workdir, WorkspaceInfo::Sandbox { .. })),
        "sandboxed agents have no terminals yet"
    );
    let view = services
        .pool
        .materialize_view(&record.config.workdirs)
        .await
        .context("materialize agent view")?;
    let shell = services
        .user_environment
        .get("SHELL")
        .and_then(|shell| shell.to_str())
        .unwrap_or("bash")
        .to_owned();
    services
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
    services: Arc<Services>,
    mut writer: W,
    agent: Option<String>,
) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let filter = match &agent {
        Some(agent) => match services.resolve_display_agent_id(agent).await {
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
    let terminals = services
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
    services: Arc<Services>,
    mut reader: R,
    mut writer: W,
    workspace: WorkspaceInfo,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let workspace = match services.pool.open_workspace(&workspace).await {
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
/// The bodies one raw row carries, for a client that asked by position:
/// a request's tool results whole, or a response as the transcript draws
/// it. Rows a rewind hid still answer; the client asked for one it holds.
fn agent_detail(
    db: &RhoDb,
    agent_id: AgentId,
    pos: rho_ui_proto::mirror::AgentPos,
) -> rho_ui_proto::mirror::DetailBody {
    use rho_ui_proto::mirror::DetailBody;
    match db.read().agent_event(agent_id, pos.into()) {
        Some(rho_agent::AgentEvent::Sent { blocks, .. }) => DetailBody::Results(
            blocks
                .iter()
                .flat_map(|block| match block {
                    rho_core::ContextBlock::ToolResults { results } => {
                        results.iter().map(detail_result).collect::<Vec<_>>()
                    }
                    rho_core::ContextBlock::ToolUpdate(update) => {
                        vec![detail_update(update)]
                    }
                    _ => Vec::new(),
                })
                .collect(),
        ),
        Some(rho_agent::AgentEvent::Replied { blocks, .. }) => DetailBody::Response(
            blocks
                .iter()
                .flat_map(|block| match block {
                    rho_core::ContextBlock::InferenceResponse { items, .. } => {
                        items.iter().filter_map(detail::item).collect::<Vec<_>>()
                    }
                    _ => Vec::new(),
                })
                .collect(),
        ),
        Some(rho_agent::AgentEvent::Transcript { line, .. }) => match line {
            rho_agent::TranscriptLine::Assistant { text, calls, .. } => DetailBody::Response(
                (!text.is_empty())
                    .then_some(rho_ui_proto::mirror::Item::Text { text, phase: None })
                    .into_iter()
                    .chain(
                        calls
                            .into_iter()
                            .map(|call| rho_ui_proto::mirror::Item::ToolCall {
                                id: call.id,
                                name: call.name,
                                arguments: call.arguments,
                            }),
                    )
                    .collect(),
            ),
            rho_agent::TranscriptLine::ToolResults { results } => {
                DetailBody::Results(results.iter().map(detail_result).collect())
            }
            rho_agent::TranscriptLine::User { .. }
            | rho_agent::TranscriptLine::Compacted { .. } => DetailBody::Nothing,
        },
        Some(rho_agent::AgentEvent::Failed { partial, .. }) => DetailBody::Response(
            partial
                .items
                .iter()
                .filter_map(|slot| match slot {
                    rho_core::StreamingContextItemState::Pending(item)
                    | rho_core::StreamingContextItemState::Finished(item) => {
                        rho_agent::live::to_item(item)
                    }
                    rho_core::StreamingContextItemState::Empty => None,
                })
                .collect(),
        ),
        _ => DetailBody::Nothing,
    }
}

fn detail_result(result: &rho_core::ToolResult) -> rho_ui_proto::mirror::DetailResult {
    use rho_ui_proto::mirror::ToolStatus;
    rho_ui_proto::mirror::DetailResult {
        id: result.call_id.as_str().to_owned(),
        status: match result.body.status {
            rho_core::ToolOutputStatus::Success => ToolStatus::Success,
            rho_core::ToolOutputStatus::Error => ToolStatus::Error,
            rho_core::ToolOutputStatus::Cancelled => ToolStatus::Cancelled,
        },
        output: result.body.recorded_output().to_owned(),
        error: None,
    }
}

fn detail_update(update: &rho_core::ToolUpdate) -> rho_ui_proto::mirror::DetailResult {
    rho_ui_proto::mirror::DetailResult {
        id: update.call_id.as_str().to_owned(),
        status: rho_ui_proto::mirror::ToolStatus::Success,
        output: update.recorded_output().to_owned(),
        error: None,
    }
}

/// Repo roots must be absolute (the daemon's cwd is meaningless by design)
/// jj repo roots: agents work in daemon-created jj workspaces, so both
/// workdir registration and agent creation take repos. A leading `~` expands
/// to the daemon's home: clients may run on another machine, so path
/// interpretation belongs here.
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

async fn prepare_image_content(content: &mut [ContentPart]) -> anyhow::Result<()> {
    validate_image_content(content)?;
    for part in content.iter_mut() {
        let ContentPart::Image { media_type, data } = part else {
            continue;
        };
        let source = std::mem::take(data);
        let prepared = rho_image::prepare(source).await?;
        let encoded = prepared.content.data.len().div_ceil(3).saturating_mul(4);
        if encoded > MAX_IMAGE_BASE64_BYTES {
            anyhow::bail!("prepared image exceeds the 10 MiB encoded limit");
        }
        *media_type = prepared.content.media_type;
        *data = prepared.content.data;
    }
    validate_image_content(content)
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
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, OsString};
    use std::os::fd::AsRawFd as _;
    use std::sync::Arc;

    use rho_agent::db::{AgentWriteTxnExt, QuotaModel, QuotaObservationRecord, QuotaProvider};
    use rho_core::ContentPart;
    use rho_db::RhoDb;
    use rho_ui_proto::ServerMessage;

    use super::{
        AgentUsageModel, ClientMessage, DeskSession, GitProviderClaim, GitTransportBroker,
        MAX_IMAGE_BASE64_BYTES, MAX_INPUT_IMAGES, PlatformSecrets, Services, claude_quota_history,
        configure_octo_git_transport, hourly_global_usage_series, merge_hourly_agent_cost_bucket,
        persist_gui_telemetry, prepare_image_content, quota_burn, quota_summaries,
        start_runtime_sockets, validate_image_content,
    };

    #[test]
    fn tool_detail_reads_the_complete_host_record() {
        let result = rho_core::ToolResult {
            call_id: rho_core::ToolCallId::try_from("call-1").unwrap(),
            tool_type: rho_core::ToolType::Custom,
            body: rho_core::ToolOutput {
                output: Arc::new("bounded model view".to_owned()),
                full_output: Some(Arc::new("complete host record".to_owned())),
                images: Arc::new(Vec::new()),
                status: rho_core::ToolOutputStatus::Success,
            },
            started_at: rho_core::UnixMs(1),
            finished_at: rho_core::UnixMs(2),
            metadata: None,
        };

        assert_eq!(super::detail_result(&result).output, "complete host record");

        let update = rho_core::ToolUpdate {
            call_id: rho_core::ToolCallId::try_from("call-1").unwrap(),
            tool_type: rho_core::ToolType::Custom,
            output: Arc::new("bounded update".to_owned()),
            full_output: Some(Arc::new("complete update".to_owned())),
            at: rho_core::UnixMs(3),
        };
        assert_eq!(super::detail_update(&update).output, "complete update");
    }

    #[tokio::test]
    async fn explicit_socket_keeps_runtime_files_beside_it() {
        let runtime = tempfile::tempdir().unwrap();
        let sockets = start_runtime_sockets(
            Some(runtime.path().join("qa/rho.sock")),
            PlatformSecrets::default(),
        )
        .unwrap();
        let paths = &sockets.paths;

        assert!(paths.socket().exists());
        assert!(paths.octo_socket().exists());
        assert!(paths.daemon_lock().exists());
        assert!(!paths.browser_socket().exists());
        assert!(!paths.pr_logs().exists());
        assert_eq!(
            std::fs::read_dir(runtime.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            [std::ffi::OsString::from("qa")]
        );
        drop(sockets);
    }

    #[tokio::test]
    async fn second_daemon_is_refused_while_first_holds_runtime_lock() {
        let runtime = tempfile::tempdir().unwrap();
        let paths = rho_ui_proto::RuntimePaths::new(Some(runtime.path().join("rho.sock"))).unwrap();
        let first =
            start_runtime_sockets(Some(paths.socket().to_owned()), PlatformSecrets::default())
                .unwrap();

        let error = match start_runtime_sockets(
            Some(paths.socket().to_owned()),
            PlatformSecrets::default(),
        ) {
            Ok(_) => panic!("second daemon acquired the runtime directory"),
            Err(error) => error,
        };
        let message = format!("{error:#}");

        assert!(
            message.contains(&paths.directory().display().to_string()),
            "{message}"
        );
        assert!(
            message.contains(&paths.daemon_lock().display().to_string()),
            "{message}"
        );
        drop(first);
    }

    #[tokio::test]
    async fn stale_socket_files_are_removed_and_rebound() {
        let runtime = tempfile::tempdir().unwrap();
        let paths = rho_ui_proto::RuntimePaths::new(Some(runtime.path().join("rho.sock"))).unwrap();
        drop(std::os::unix::net::UnixListener::bind(paths.socket()).unwrap());
        drop(std::os::unix::net::UnixListener::bind(paths.octo_socket()).unwrap());

        let sockets =
            start_runtime_sockets(Some(paths.socket().to_owned()), PlatformSecrets::default())
                .unwrap();

        assert!(std::os::unix::net::UnixStream::connect(paths.socket()).is_ok());
        assert!(std::os::unix::net::UnixStream::connect(paths.octo_socket()).is_ok());
        drop(sockets);
    }

    #[tokio::test]
    async fn runtime_lock_remains_held_after_socket_setup_returns() {
        let runtime = tempfile::tempdir().unwrap();
        let paths = rho_ui_proto::RuntimePaths::new(Some(runtime.path().join("rho.sock"))).unwrap();
        let sockets =
            start_runtime_sockets(Some(paths.socket().to_owned()), PlatformSecrets::default())
                .unwrap();
        let contender = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(paths.daemon_lock())
            .unwrap();

        let result = unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(result, -1);
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(sockets);
    }

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

        assert_eq!(series.len(), 7);
        assert_eq!(series[0].model, "fable");
        assert_eq!(series[0].buckets.len(), 1);
        assert_eq!(series[0].buckets[0].bucket_start_ms, 0);
        assert_eq!(series[0].buckets[0].input_tokens, 30);
        assert_eq!(series[0].buckets[0].requests, 2);
        assert_eq!(series[1].model, "gpt");
        assert_eq!(series[1].buckets[0].bucket_start_ms, 60 * 60 * 1_000);
        assert_eq!(series[5].model, "gemini");
        assert!(series[5].buckets.is_empty());
        assert_eq!(series[6].model, "astra");
        assert!(series[6].buckets.is_empty());
    }

    #[test]
    fn agent_cost_history_rejects_more_than_its_hourly_bucket_limit() {
        let agent_id =
            rho_agent::db::AgentId::from_counter(1, &rho_core::AgentIdDomain(0)).unwrap();
        let bucket = |bucket_start_ms| rho_agent::db::AgentUsageBucket {
            bucket_start_ms,
            model: AgentUsageModel::GPT,
            requests: 1,
            ..Default::default()
        };
        let mut hourly = BTreeMap::new();
        merge_hourly_agent_cost_bucket(&mut hourly, agent_id, bucket(0), 1).unwrap();
        assert!(
            merge_hourly_agent_cost_bucket(&mut hourly, agent_id, bucket(60 * 60 * 1_000), 1,)
                .is_err()
        );
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
    async fn claude_quota_history_includes_every_stored_point() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        let now = rho_core::UnixMs::now().0;
        let mut write = db.write().await;
        for index in 0..5 {
            assert!(write.record_quota_observation(QuotaObservationRecord {
                provider: QuotaProvider::Claude,
                model: QuotaModel::OPUS,
                auth_namespace: Some("default".to_owned()),
                observed_at: rho_core::UnixMs(now - (4 - index) * 1_000),
                used_percent: index as u8,
                reset_at_unix: Some(123),
            }));
        }
        assert!(write.record_quota_observation(QuotaObservationRecord {
            provider: QuotaProvider::Claude,
            model: QuotaModel::FABLE,
            auth_namespace: None,
            observed_at: rho_core::UnixMs(now),
            used_percent: 25,
            reset_at_unix: Some(456),
        }));
        write.commit();

        let history = claude_quota_history(&db);
        let opus = history
            .iter()
            .find(|series| series.model == "opus")
            .unwrap();
        assert_eq!(opus.points.len(), 5);
        assert_eq!(
            opus.points
                .iter()
                .map(|point| point.remaining_percent)
                .collect::<Vec<_>>(),
            [100, 99, 98, 97, 96]
        );
        let fable = history
            .iter()
            .find(|series| series.model == "fable")
            .unwrap();
        assert_eq!(fable.points[0].remaining_percent, 75);
    }

    #[tokio::test]
    async fn inference_migrates_legacy_scoped_gpt_history() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        let mut write = db.write().await;
        write.init_agent_tables();
        assert!(write.record_quota_observation(QuotaObservationRecord {
            provider: QuotaProvider::ChatGpt,
            model: QuotaModel::GPT,
            auth_namespace: Some("work".to_owned()),
            observed_at: rho_core::UnixMs(123),
            used_percent: 42,
            reset_at_unix: Some(456),
        }));
        write.commit();

        let inference = rho_inference::Inference::new(db).await.unwrap();
        let history = inference.quota_history(rho_core::UnixMs(0));

        assert_eq!(history.len(), 1);
        assert_eq!(history[0].auth_namespace, "work");
        assert_eq!(history[0].points[0].remaining_percent, 58);
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
    #[tokio::test]
    async fn image_input_is_reencoded_from_pixels_before_queueing() {
        use std::io::Cursor;

        use image::{DynamicImage, ImageBuffer, Rgba};

        let source =
            DynamicImage::ImageRgba8(ImageBuffer::from_pixel(2, 1, Rgba([10, 20, 30, 255])));
        let mut encoded = Cursor::new(Vec::new());
        source
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let mut content = vec![ContentPart::Image {
            // The actual bytes, rather than this permitted but incorrect label,
            // determine the normalized output.
            media_type: "image/jpeg".to_owned(),
            data: encoded.into_inner(),
        }];

        prepare_image_content(&mut content).await.unwrap();
        let ContentPart::Image { media_type, data } = &content[0] else {
            panic!("expected image")
        };
        assert_eq!(media_type, "image/png");
        assert_eq!(&data[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn gui_telemetry_storage_is_private_unique_and_bounded() {
        let temp = tempfile::tempdir().unwrap();
        let first = persist_gui_telemetry(temp.path(), b"one").unwrap();
        let second = persist_gui_telemetry(temp.path(), b"two").unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(first.as_path()).unwrap(), b"one");
        assert_eq!(std::fs::read(second.as_path()).unwrap(), b"two");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(first).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(
            persist_gui_telemetry(
                temp.path(),
                &vec![0; rho_ui_proto::MAX_GUI_TELEMETRY_BYTES + 1]
            )
            .unwrap_err()
            .to_string()
            .contains("exceeds")
        );
    }

    /// A device is one GUI, and the newest window wins it.
    ///
    /// The user's GUI panicked and restarted; the daemon still held the old
    /// connection's binding, and the restarted GUI was refused with "Desk
    /// device already has an active writer connection" until the transport
    /// gave up on the dead one — over iroh that is the ten minutes of
    /// `rho_iroh_auth::AUTHENTICATED_IDLE_TIMEOUT`. So a second `DeskSync`
    /// displaces the first. The guard itself stays real: the displaced
    /// connection may not write afterwards, because two writers under one
    /// device id would collide in the CRDT's per-device namespace.
    #[tokio::test]
    async fn a_newer_window_takes_the_device_and_the_displaced_one_may_not_write() {
        use rho_desk::cells::{
            CellMutation, CellWrite, DeviceId, Id, Property, Stamp, State, Uuid, Version,
        };

        let temp = tempfile::tempdir().unwrap();
        let services = test_services(temp.path()).await;
        let device = DeviceId([7; 16]);

        let (older_tx, mut older_rx) = tokio::sync::mpsc::unbounded_channel();
        let (newer_tx, _newer_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut older: Option<DeskSession> = None;
        let mut newer: Option<DeskSession> = None;

        let sync = |device| ClientMessage::DeskSync {
            bodies: std::collections::BTreeMap::new(),
            device,
            known: Version::default(),
            store: None,
        };
        desk_message(&services, &older_tx, 1, &mut older, sync(device))
            .await
            .expect("the first window binds the device");
        desk_message(&services, &newer_tx, 2, &mut newer, sync(device))
            .await
            .expect("and the window the user just restarted binds it too");

        // The older connection is told why it is going, in words it can show.
        let told = std::iter::from_fn(|| older_rx.try_recv().ok())
            .filter_map(|message| match message {
                ServerMessage::Error { message } => Some(message),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            told,
            ["The desk moved to a newer window on this device"],
            "the displaced window is told, and not left guessing"
        );

        let mutation = CellMutation {
            stamp: Stamp { device, version: 1 },
            writes: vec![CellWrite {
                id: Id::Note(Uuid([9; 16])),
                property: Property::State(State::Open),
            }],
            verdict: None,
        };
        // The daemon no longer answers a write with a refusal, so the one
        // condition that is about the connection rather than about what the
        // user wrote breaks the connection, the way the text path already
        // did. Two authors in one CRDT namespace is not a thing to carry on
        // through.
        let broken = desk_message(
            &services,
            &older_tx,
            1,
            &mut older,
            ClientMessage::DeskMutationApply {
                mutation: mutation.clone(),
            },
        )
        .await
        .expect_err("it may not write under a device id that is another window's now");
        assert_eq!(
            broken.to_string(),
            "The desk moved to a newer window on this device"
        );

        // The window that took the device writes.
        desk_message(
            &services,
            &newer_tx,
            2,
            &mut newer,
            ClientMessage::DeskMutationApply { mutation },
        )
        .await
        .unwrap();
        // Nothing is sent back for it, so the store is where the answer is.
        assert_eq!(
            services
                .desk_cells
                .frontier()
                .unwrap()
                .get(&device)
                .copied(),
            Some(1),
            "the live window's write lands"
        );
    }

    /// One desk message through the daemon's own handler, with everything a
    /// desk message does not use left empty.
    async fn desk_message(
        services: &Arc<Services>,
        outgoing: &tokio::sync::mpsc::UnboundedSender<ServerMessage>,
        connection: u64,
        session: &mut Option<DeskSession>,
        message: ClientMessage,
    ) -> anyhow::Result<()> {
        let mut log_follow = None;
        super::handle_message(
            services,
            None,
            outgoing,
            &mut Vec::new(),
            None,
            connection,
            &mut log_follow,
            session,
            message,
        )
        .await
        .map(|_| ())
    }

    async fn test_services(root: &std::path::Path) -> Arc<Services> {
        let db = RhoDb::open(root.join("rho.redb"));
        db.write().await.init_agent_tables();
        let inference = rho_inference::Inference::new(db.clone()).await.unwrap();
        Arc::new(
            Services::new(
                db,
                inference,
                Default::default(),
                camino::Utf8PathBuf::from_path_buf(root.join("state")).unwrap(),
                rho_workspaces::UserEnvironment::new(Default::default()),
                PlatformSecrets::default(),
                root.join("octo.sock"),
            )
            .await
            .unwrap(),
        )
    }
}
