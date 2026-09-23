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
use rho_agent_host_proto::control::{
    ClientFrame as ControlClientFrame, ServerFrame as ControlFrame,
};
use rho_agent_host_proto::server::{Server, ServerConnection};
use rho_agent_host_proto::{
    AgentCommand, AgentCostSeries, AgentUsageBucket as UiAgentUsageBucket, AgentUsageSeries,
    AuthState, ContentPart, GitProvided, JoinTarget, Open, Opened, Place, QuotaPoint, QuotaSeries,
    QuotaSummary, Reply, Request, StartMode, WorksetMode, WorkspaceInfo, read_frame, write_frame,
};
use rho_db::RhoDb;
use rho_inference::Inference;
use tokio::sync::{Mutex as TokioMutex, broadcast, mpsc, oneshot};

pub mod debug;
mod realtime;
mod secret_store;
pub mod workspace_channel;

/// FDNAME under which messaging-platform secrets live in the systemd fd store.
const PLATFORM_SECRETS_FD_STORE_NAME: &str = "platform-secrets";
pub fn default_socket_path() -> anyhow::Result<PathBuf> {
    rho_agent_host_proto::socket_path()
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

/// Puts the directories this daemon was told to use into the environment its
/// agents are spawned with.
///
/// `login_environment` starts from a cleared environment and a login shell,
/// so what comes back is whatever that shell chose and none of what this
/// daemon was named. An agent would then work in the XDG defaults under HOME
/// and read a Claude config home nobody named, agreeing with the daemon only
/// by luck. What the daemon resolved wins here; the two directories it has no
/// say over are passed on as it was started with them, or left to the login
/// shell when it was started without them.
fn apply_daemon_directories(
    environment: &mut Vec<(OsString, OsString)>,
    state_dir: &camino::Utf8Path,
    claude: &rho_claude::accounts::ClaudePaths,
) {
    // The state directory is `<XDG_STATE_HOME>/rho`.
    if let Some(state_home) = state_dir.parent() {
        set_environment_value(environment, "XDG_STATE_HOME", state_home.as_str());
    }
    set_environment_value(
        environment,
        "CLAUDE_CONFIG_DIR",
        claude.config_home().as_str(),
    );
    for name in ["XDG_CONFIG_HOME", "XDG_DATA_HOME"] {
        if let Some(value) = std::env::var_os(name) {
            set_environment_value(environment, name, value);
        }
    }
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

fn lock_runtime_directory(
    paths: &rho_agent_host_proto::RuntimePaths,
) -> anyhow::Result<std::fs::File> {
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
    paths: rho_agent_host_proto::RuntimePaths,
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
    let paths = rho_agent_host_proto::RuntimePaths::new(socket_path)?;
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

pub use rho_fs_view::PathOverrides;

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
    /// The Claude config directory to run against, with accounts beside it
    /// as `<dir>-accounts`. Named outright by a rig, whose Claude state is
    /// its own; without it the daemon uses the user's, `$CLAUDE_CONFIG_DIR`
    /// or `~/.claude`. Deliberately not an `env =` argument: a daemon
    /// pointed at a rig's state directory picked the user's transcripts up
    /// out of the environment and rebuilt agent rows from them.
    #[arg(long, value_name = "DIR")]
    pub claude_config_dir: Option<camino::Utf8PathBuf>,
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
    // Resolved once, here, and passed down from this point: nothing below
    // reads `$HOME` or `$CLAUDE_CONFIG_DIR` for itself.
    let claude = match args.claude_config_dir.clone() {
        Some(dir) => rho_claude::accounts::ClaudePaths::at(dir),
        None => rho_claude::accounts::ClaudePaths::from_env()?,
    };
    eprintln!("rho daemon: Claude configuration {}", claude.config_home());

    let mut user_environment = login_environment()?;
    if let Some(path) = EMBEDDED_DIRENV_PATH_BEFORE {
        user_environment.push(("RHO_DIRENV_PATH_BEFORE".into(), path.into()));
    }
    user_environment.push((FIND_DENY_ROOTS_ENV.into(), find_deny_roots()));
    user_environment.push((
        rho_agent_host_proto::RuntimePaths::SOCKET_ENV.into(),
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
    apply_daemon_directories(&mut user_environment, &state_dir, &claude);
    let user_environment = rho_fs_view::UserEnvironment::new(user_environment);

    let db = RhoDb::open(db_path);
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
    // The mirror keeper keeps every mirror fetched so new agents start on
    // the remote's latest state; without it (no git on PATH, say) agents
    // get plain git and nothing is shared.
    let worksets = match rho_fs_view::Worksets::open(
        &state_dir,
        user_environment.clone(),
        path_overrides.clone(),
        rho_fs_view::StoreService::Serve(rho_fs_view::StoreRefresh::default()),
    )
    .await
    {
        Ok(worksets) => worksets,
        Err(error) => {
            eprintln!(
                "rho daemon: clone store server unavailable, clones fetch for themselves: {error:#}"
            );
            rho_fs_view::Worksets::open(
                &state_dir,
                user_environment.clone(),
                path_overrides,
                rho_fs_view::StoreService::None,
            )
            .await?
        }
    };
    let iroh = if args.iroh {
        let (listener, auth) =
            rho_rpc::AuthenticatedIrohListener::bind(db.clone(), rho_agent_host_proto::IROH_ALPN)
                .await?;
        eprintln!("rho daemon iroh endpoint: {}", listener.endpoint_id());
        Some((listener, auth))
    } else {
        None
    };

    let iroh_auth = iroh.as_ref().map(|(_, auth)| auth.clone());
    let pool = AgentPool::new(db.clone(), inference.clone(), worksets, claude.clone()).await;
    let services = Arc::new(
        Services::new(
            db,
            inference,
            pool,
            claude.clone(),
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
    for account in claude.list()? {
        let quota_environment = services.user_environment.clone();
        let quota_path_overrides = quota_path_overrides.clone();
        let account_dir = claude.account_dir(&account)?;
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
            let media = rho_rpc::media::Mux::new(connection.clone());
            let uni = media.clone();
            let uni_task = tokio::spawn(async move { uni.receive_uni().await });
            // One control stream per iroh connection; every other stream
            // is opened for one thing of its own.
            let control_claimed = Arc::new(AtomicBool::new(false));
            while let Ok((send, recv)) = connection.accept_bi().await {
                let services = services.clone();
                let control_claimed = control_claimed.clone();
                let media = media.clone();
                let iroh_auth = iroh_auth.clone();
                tokio::spawn(async move {
                    let result = async {
                        let Some((mut recv, send)) =
                            rho_rpc::accept_iroh_stream(&media, send, recv).await?
                        else {
                            return Ok(());
                        };
                        let open = tokio::time::timeout(
                            std::time::Duration::from_secs(10),
                            read_frame::<_, Open>(&mut recv),
                        )
                        .await
                        .map_err(|_| anyhow::anyhow!("iroh stream first frame timed out"))??;
                        if let Open::Wayland {
                            media_id,
                            agent,
                            session,
                        } = open
                        {
                            let transport = media.session(media_id)?;
                            send.set_priority(100)?;
                            let mut writer = rho_rpc::Writer::new(send);
                            write_frame(&mut writer, &Opened::Ready).await?;
                            return serve_wayland(
                                services, transport, recv, writer, agent, session,
                            )
                            .await;
                        }
                        let control = matches!(open, Open::Control);
                        if control {
                            anyhow::ensure!(
                                control_claimed
                                    .compare_exchange(
                                        false,
                                        true,
                                        Ordering::AcqRel,
                                        Ordering::Relaxed
                                    )
                                    .is_ok(),
                                "iroh connection already has a control stream"
                            );
                            send.set_priority(1)
                                .context("set iroh control stream priority")?;
                        }
                        if matches!(
                            open,
                            Open::Terminal { .. } | Open::Shell { .. } | Open::Realtime { .. }
                        ) {
                            send.set_priority(50)
                                .context("set iroh interactive stream priority")?;
                        }
                        let writer = rho_rpc::Writer::new(send);
                        let result = serve_stream(services, iroh_auth, open, recv, writer).await;
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
            uni_task.abort();
        });
    }
    listener.close().await;
}

trait GitStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T> GitStream for T where T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
type BoxGitStream = Box<dyn GitStream>;

#[derive(Default)]
struct GitTransportState {
    providers: HashMap<u64, mpsc::UnboundedSender<ControlFrame>>,
    pending: HashMap<u64, PendingGitTransport>,
}

struct PendingGitTransport {
    response: oneshot::Sender<Result<BoxGitStream, String>>,
    recipients: HashMap<u64, mpsc::UnboundedSender<ControlFrame>>,
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
    async fn register(&self, provider: mpsc::UnboundedSender<ControlFrame>) {
        let provider_id = self.next_provider_id.fetch_add(1, Ordering::Relaxed);
        let mut state = self.state.lock().await;
        state.providers.retain(|_, provider| !provider.is_closed());
        state.providers.insert(provider_id, provider);
    }

    async fn request(
        &self,
        request: rho_agent_host_proto::GitTransportRequest,
    ) -> anyhow::Result<BoxGitStream> {
        self.request_with_timeout(request, std::time::Duration::from_secs(60))
            .await
    }

    async fn request_with_timeout(
        &self,
        request: rho_agent_host_proto::GitTransportRequest,
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
                    .send(ControlFrame::GitTransportRequested {
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
        recipients: &HashMap<u64, mpsc::UnboundedSender<ControlFrame>>,
        except: Option<u64>,
    ) {
        for (&provider_id, provider) in recipients {
            if Some(provider_id) != except {
                let _ = provider.send(ControlFrame::GitTransportDone { request_id });
            }
        }
    }
}

/// Everything the daemon owns that a connection may need: the agent pool,
/// the database, the stores, the locks and the brokers. It is not a
/// registry of agents — the pool is that — but the one bundle a connection
/// is handed so it does not carry a dozen handles of its own.
struct Services {
    pool: Arc<AgentPool>,
    db: RhoDb,
    /// The host's copy of the desk, which serves every desk stream.
    desk: rho_desk_server::DeskServer,
    visualizations: rho_visualizations::VisualizationStore,
    inference: Inference,
    /// The database's machine seed, announced in `Ready` so clients can
    /// encode agent IDs.
    machine_seed: u64,
    /// Stateless PR, CI, review, and comment operations.
    pr_monitor: Arc<rho_pr_monitor::PrMonitor>,
    /// Sealed platform secret store used by Octo.
    platform_secrets: PlatformSecrets,
    /// Daemon-wide fanout for messages every client must hear regardless of
    /// which connection caused them (attention changes); each connection
    /// forwards this onto its own outgoing channel.
    events: broadcast::Sender<ControlFrame>,
    /// The snapshotted login environment, for terminal shells.
    user_environment: rho_fs_view::UserEnvironment,
    /// The Claude configuration this daemon runs against, resolved in `run`.
    claude: rho_claude::accounts::ClaudePaths,
    git_transport: GitTransportBroker,
    /// At most one GUI owns the voice session's microphone and playback.
    voice_lease: Arc<TokioMutex<()>>,
}

impl Services {
    #[allow(clippy::too_many_arguments)]
    async fn new(
        db: RhoDb,
        inference: Inference,
        pool: Arc<AgentPool>,
        claude: rho_claude::accounts::ClaudePaths,
        user_environment: rho_fs_view::UserEnvironment,
        platform_secrets: PlatformSecrets,
        octo_socket: PathBuf,
    ) -> anyhow::Result<Self> {
        let machine_seed = db.read().machine_seed();
        let pr_monitor =
            rho_pr_monitor::PrMonitor::new(pool.clone(), db.clone(), octo_socket).await?;
        let visualizations = rho_visualizations::VisualizationStore::new(db.clone()).await;
        let desk = rho_desk_server::DeskServer::open(db.clone()).await?;
        let registry = Self {
            pool,
            db,
            claude,
            desk,
            visualizations,
            inference,
            machine_seed,
            pr_monitor,
            platform_secrets,
            events: broadcast::channel(1024).0,
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

    async fn ready_message(&self) -> ControlFrame {
        let read = self.db.read();
        ControlFrame::Ready {
            auth: self.auth_state(),
            machine_seed: self.machine_seed,
            agent_counter: read.last_agent_counter(),
        }
    }

    /// `mode` is the agent's own: how it sees the filesystem around the
    /// workset, whether that workset is fresh or one it joins.
    async fn create(
        &self,
        role: AgentRole,
        start: StartMode,
        mode: WorksetMode,
    ) -> anyhow::Result<(AgentId, RunningAgent)> {
        let start = match start {
            StartMode::NewOn { repo, revset } => {
                // The agent exists at once; its workset is placed (cloned
                // from the mirror store, checked out, entered) by its first
                // command, and again by the next one if that failed. The
                // checkout's name is known before the clone, so the record
                // is complete from the start.
                let origin = expand_home(&repo).unwrap_or(repo);
                let name = rho_fs_view::repo_name(origin.as_str())
                    .with_context(|| format!("no repository name in {origin}"))?;
                let worksets = self.pool.worksets();
                let workset = worksets.create().await?;
                let cwd = visible_path(&workset, &workset.root().join(&name))?;
                let place = Place {
                    workset: workset.id().to_owned(),
                    cwd: cwd.clone(),
                    mode,
                    origin: Some(origin.clone()),
                };
                let mode = rho_fs_view::Mode::from_workset_mode(mode);
                rho_agent::StartPlace::pending(place, move || {
                    let workset = workset.clone();
                    let origin = origin.clone();
                    let name = name.clone();
                    let revset = revset.clone();
                    let cwd = cwd.clone();
                    let mode = mode.clone();
                    async move {
                        let checkout = workset.clone_repo(origin.as_str(), Some(&name)).await?;
                        workset.checkout(&checkout, &revset).await?;
                        workset.enter(mode, &cwd)
                    }
                })
                .owning_workset()
            }
            StartMode::Join(JoinTarget::Workspace(info)) => {
                let mut place = info
                    .place()
                    .context("agents no longer work in the user's own checkout")?
                    .clone();
                // The same directory as the agent joined, seen the way this
                // agent asked to see it.
                place.mode = mode;
                let view = self.pool.materialize_view(&place).await?;
                rho_agent::StartPlace::new(view, place.origin.clone())
            }
            StartMode::Join(JoinTarget::User { .. }) => {
                anyhow::bail!(
                    "agents no longer work in the user's own checkout: start on the \
                     repository's URL or path instead"
                );
            }
        };
        let (agent_id, agent) = self.pool.create(role, None, start).await?;
        Ok((agent_id, agent))
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

    async fn load(&self, agent_id: AgentId) -> anyhow::Result<(AgentId, RunningAgent, bool)> {
        self.pool.load(agent_id).await
    }
}

async fn serve_connection(
    services: Arc<Services>,
    iroh_auth: Option<rho_iroh_auth::IrohAuth>,
    connection: ServerConnection,
) -> anyhow::Result<()> {
    let stream = connection.into_stream();
    let (mut reader, writer) = stream.into_split();
    let open = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        read_frame::<_, Open>(&mut reader),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Unix stream first frame timed out"))??;
    serve_stream(services, iroh_auth, open, reader, writer).await
}

/// One stream over any framed byte stream (a Unix socket connection or an
/// iroh bi-stream from an enrolled remote client), serving what its first
/// frame opened.
async fn serve_stream<R, W>(
    services: Arc<Services>,
    iroh_auth: Option<rho_iroh_auth::IrohAuth>,
    open: Open,
    reader: R,
    mut writer: W,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    match open {
        Open::Control => serve_control(services, reader, writer).await,
        Open::Agents => serve_agents(services, reader, writer).await,
        Open::Desk => services.desk.serve(reader, writer).await,
        Open::Workspace { workspace } => {
            serve_workspace_channel(services, reader, writer, workspace).await
        }
        Open::Realtime { offer_sdp } => realtime::serve(services, reader, writer, offer_sdp).await,
        Open::Terminal {
            agent,
            terminal_id,
            open,
            cols,
            rows,
        } => {
            serve_terminal(
                services,
                reader,
                writer,
                agent,
                terminal_id,
                open,
                cols,
                rows,
            )
            .await
        }
        Open::Shell { agent } => serve_shell(services, reader, writer, agent).await,
        Open::Wayland { .. } => {
            write_frame(
                &mut writer,
                &Opened::Refused {
                    reason: "Wayland streams need an iroh connection".to_owned(),
                },
            )
            .await
        }
        Open::GitTransport { request } => {
            serve_git_transport_request(services, reader, writer, request).await
        }
        Open::GitProvide {
            request_id,
            provider_id,
            claim,
        } => {
            serve_git_transport_provider(services, reader, writer, request_id, provider_id, claim)
                .await
        }
        Open::Request(request) => {
            let reply = match handle_request(&services, iroh_auth.as_ref(), request).await {
                Ok((reply, refresh)) => {
                    if let Refresh::Ready = refresh {
                        // Registry changes show on every client (GUI rails
                        // and a waiting CLI), so the refreshed snapshot goes
                        // through the daemon-wide event fanout.
                        let _ = services.events.send(services.ready_message().await);
                    }
                    reply
                }
                // The whole chain, not just the outermost context: a new
                // agent that failed said "create managed workspace" and
                // kept the reason to itself, which is not something a
                // reader can act on.
                Err(error) => Reply::Failed {
                    reason: format!("{error:#}"),
                },
            };
            write_frame(&mut writer, &reply).await
        }
    }
}

/// The control stream: host-wide state pushed to one client, and its
/// offer to carry SSH Git transport.
async fn serve_control<R, W>(
    services: Arc<Services>,
    mut reader: R,
    writer: W,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<ControlFrame>();
    tokio::spawn(async move {
        let mut writer = writer;
        while let Some(frame) = outgoing_rx.recv().await {
            if write_frame(&mut writer, &frame).await.is_err() {
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

    // Announce every agent created in the pool — by clients or by other
    // agents spawning children — so it shows up on this connection.
    let created_task = {
        let services = Arc::clone(&services);
        let outgoing_tx = outgoing_tx.clone();
        tokio::spawn(async move {
            loop {
                match created_rx.recv().await {
                    Ok(created) => {
                        if outgoing_tx
                            .send(ControlFrame::AgentCreated {
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
        })
    };

    // Daemon-wide events fan out to every client, not just the connection
    // whose action produced them.
    let events_task = {
        let services = Arc::clone(&services);
        let outgoing_tx = outgoing_tx.clone();
        tokio::spawn(async move {
            loop {
                match events_rx.recv().await {
                    Ok(frame) => {
                        if outgoing_tx.send(frame).is_err() {
                            break;
                        }
                    }
                    // Most of what fans out is a piece of `Ready`; the whole
                    // of it stands in for whatever was missed.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        if outgoing_tx.send(services.ready_message().await).is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    };

    // Reconcile ephemeral advertisements in workset namespaces. Only changes
    // cross the authenticated GUI stream; discovery never starts an encoder.
    let desktop_task = {
        let services = services.clone();
        let outgoing = outgoing_tx.clone();
        tokio::spawn(async move {
            let mut previous = Vec::new();
            let mut timer = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = outgoing.closed() => break,
                    _ = timer.tick() => {}
                }
                let mut sessions = Vec::new();
                for process in services.pool.executions().await {
                    match process.action(rho_agent::WorksetAction::DesktopList).await {
                        Ok(rho_agent::WorksetReply::DesktopSessions(entries)) => {
                            sessions.extend(entries)
                        }
                        Ok(_) => tracing::warn!("unexpected desktop discovery reply"),
                        Err(error) => tracing::debug!(%error, "desktop discovery unavailable"),
                    }
                }
                sessions.sort();
                sessions.dedup();
                if sessions != previous {
                    previous = sessions.clone();
                    if outgoing
                        .send(ControlFrame::DesktopSessions { sessions })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        })
    };

    let result = loop {
        match rho_agent_host_proto::read_frame_optional::<_, ControlClientFrame>(&mut reader).await
        {
            Ok(Some(ControlClientFrame::ProvideGitTransport)) => {
                services.git_transport.register(outgoing_tx.clone()).await;
            }
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
        }
    };
    created_task.abort();
    events_task.abort();
    desktop_task.abort();
    result
}

static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

async fn serve_git_transport_request<R, W>(
    services: Arc<Services>,
    reader: R,
    mut writer: W,
    request: rho_agent_host_proto::GitTransportRequest,
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
                &Opened::Refused {
                    reason: error.to_string(),
                },
            )
            .await?;
            return Ok(());
        }
    };
    write_frame(&mut writer, &Opened::Ready).await?;
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
            write_frame(&mut writer, &GitProvided::Done).await?;
        }
        GitProviderClaim::Selected(response) => {
            if let Err(error) = write_frame(&mut writer, &GitProvided::Ready).await {
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
            let _ = services.events.send(ControlFrame::AuthState {
                auth: services.auth_state(),
            });
            let _ = services.events.send(ControlFrame::QuotaUsage {
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
    let now = rho_agent_host_proto::UnixMs::now().0;
    let since = rho_agent_host_proto::UnixMs(now.saturating_sub(3 * 24 * 60 * 60 * 1_000));
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
    since: rho_agent_host_proto::UnixMs,
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
    let since = rho_agent_host_proto::UnixMs(
        rho_agent_host_proto::UnixMs::now()
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
                .map(|point| rho_agent_host_proto::QuotaPoint {
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
    let now = rho_agent_host_proto::UnixMs::now().0;
    let since = rho_agent_host_proto::UnixMs(now.saturating_sub(30 * 24 * 60 * 60 * 1_000));
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
    since: rho_agent_host_proto::UnixMs,
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

fn claude_accounts_message(
    db: &RhoDb,
    claude: &rho_claude::accounts::ClaudePaths,
) -> anyhow::Result<Reply> {
    Ok(Reply::ClaudeAccounts {
        accounts: claude.list()?,
        current: db.read().claude_account(),
    })
}

fn spawn_claude_quota_recorder(
    mut updates: tokio::sync::mpsc::Receiver<anyhow::Result<rho_claude_usage::ClaudeUsage>>,
    account: String,
    db: RhoDb,
    inference: Inference,
    events: broadcast::Sender<ControlFrame>,
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
            let observed_at = rho_agent_host_proto::UnixMs::now();
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
                let _ = events.send(ControlFrame::QuotaUsage {
                    summaries: combined_quota_summaries(&db, &inference),
                });
            }
        }
    });
}

/// Wakes a snoozed agent: at `until`, rebroadcasts its (by then pending)
/// level. Harmless if the disposition changed meanwhile — it just sends the
/// then-current level.
/// How many journal entries travel in one `ServerFrame::Log` while a
/// client is catching up. A cold client's first copy is a whole history,
/// so it goes in pages the connection can interleave.
const LOG_PAGE: usize = 512;

/// Sends this connection every journal entry after `since`, then follows
/// the feed: each new row as it is appended, on any agent, and every live
/// delta any loop tells, in the order they happened.
///
/// A connection's agents stream: whose journal this is and how far it
/// runs, then the rows past wherever the client's copy stops, every append
/// after them and the live tails, until the client goes. A stream of its
/// own so that a catch-up of thousands of pages queues behind nothing and
/// holds nothing up. A second `Follow` starts the follow again from its
/// `since`.
async fn serve_agents<R, W>(services: Arc<Services>, mut reader: R, writer: W) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use rho_agent_host_proto::agents::{ClientFrame, ServerFrame};
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<ServerFrame>();
    let writer_task = tokio::spawn(async move {
        let mut writer = writer;
        while let Some(frame) = outgoing_rx.recv().await {
            if write_frame(&mut writer, &frame).await.is_err() {
                break;
            }
        }
    });
    let journal_head = services.db.read().journal_head();
    let _ = outgoing_tx.send(ServerFrame::JournalHead {
        machine_seed: services.machine_seed,
        journal_head,
    });
    // Names this stream's focus in the pool's live set, so it leaves with
    // the stream.
    let stream_id = NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed);
    let mut follow: Option<tokio::task::JoinHandle<()>> = None;
    let result = loop {
        let frame =
            match rho_agent_host_proto::read_frame_optional::<_, ClientFrame>(&mut reader).await {
                Ok(Some(frame)) => frame,
                Ok(None) => break Ok(()),
                Err(error) => break Err(error),
            };
        match frame {
            ClientFrame::Follow { since } => {
                if let Some(previous) = follow.take() {
                    previous.abort();
                }
                follow = Some(spawn_log_follow(
                    Arc::clone(&services),
                    outgoing_tx.clone(),
                    since,
                ));
            }
            ClientFrame::Focus { agent_ids } => {
                if agent_ids.len() > 64 {
                    break Err(anyhow::anyhow!("too many focused agents"));
                }
                // Focus is what this client is looking at, nothing more: it
                // never loads an agent. The pool unions it across streams
                // into the live set; a loaded agent in it tells its tail.
                services
                    .pool
                    .set_live_wants(stream_id, agent_ids.into_iter().collect())
                    .await;
            }
            ClientFrame::Detail {
                agent_id,
                pos,
                more,
            } => {
                // One answer per position, each naming its own `pos`. A chunk
                // asks once and is answered as many times as it asked for.
                for pos in std::iter::once(pos).chain(more) {
                    let body = agent_detail(&services.db, agent_id, pos);
                    let _ = outgoing_tx.send(ServerFrame::Detail {
                        agent_id,
                        pos,
                        body,
                    });
                }
            }
        }
    };
    if let Some(follow) = follow {
        follow.abort();
    }
    services
        .pool
        .set_live_wants(stream_id, HashSet::new())
        .await;
    writer_task.abort();
    result
}

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
    outgoing_tx: mpsc::UnboundedSender<rho_agent_host_proto::agents::ServerFrame>,
    since: rho_agent_host_proto::transcript::Seq,
) -> tokio::task::JoinHandle<()> {
    use rho_agent::transcript::Feed;
    tokio::spawn(async move {
        // Subscribed before the catch-up read, so a row appended during it
        // is queued rather than lost; the seq drops the duplicates.
        let mut feed = rho_agent::transcript::feed(&services.db);
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
                            rho_agent_host_proto::transcript::Live::Item { .. }
                                | rho_agent_host_proto::transcript::Live::Appended { .. }
                        );
                        if !told {
                            continue;
                        }
                    }
                    if outgoing_tx
                        .send(rho_agent_host_proto::agents::ServerFrame::Live { agent_id, live })
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
                            .send(rho_agent_host_proto::agents::ServerFrame::Log {
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
    outgoing_tx: &mpsc::UnboundedSender<rho_agent_host_proto::agents::ServerFrame>,
    sent: &mut rho_agent_host_proto::transcript::Seq,
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
                Some(rho_agent_host_proto::transcript::LogEntry {
                    seq,
                    agent_id,
                    pos: pos.into(),
                    event: rho_agent::transcript::strip(&event)?,
                })
            })
            .collect::<Vec<_>>();
        if !entries.is_empty()
            && outgoing_tx
                .send(rho_agent_host_proto::agents::ServerFrame::Log { entries })
                .is_err()
        {
            return false;
        }
        // Catching up must never starve the connection's own traffic.
        tokio::task::yield_now().await;
    }
}

/// Whether a handled request changed registry state that clients see through
/// `Ready` (agents and workdirs); `Ready` refreshes every control stream,
/// so all clients converge on the change at once.
enum Refresh {
    Ready,
    None,
}

/// One request stream's request. `Err` becomes a [`Reply::Failed`].
async fn handle_request(
    services: &Arc<Services>,
    iroh_auth: Option<&rho_iroh_auth::IrohAuth>,
    request: Request,
) -> anyhow::Result<(Reply, Refresh)> {
    let reply = match request {
        Request::Agent(command) => return handle_agent_command(services, command).await,
        Request::ClaudeAccounts => claude_accounts_message(&services.db, &services.claude)?,
        Request::SetClaudeAccount { name } => {
            // The account has to be there before an agent tries to mount it;
            // a switch to a name with no directory would fail at the next
            // turn of every agent at once.
            services.claude.bootstrap(&name)?;
            let mut write = services.db.write().await;
            write.set_claude_account(&name);
            write.commit();
            claude_accounts_message(&services.db, &services.claude)?
        }
        Request::SetAuthAccountEnabled { name, enabled } => {
            services.set_auth_account_enabled(&name, enabled).await;
            Reply::Done
        }
        Request::Visualization { id } => {
            let visualization = services
                .visualizations
                .get(&id)
                .with_context(|| format!("visualization {id} does not exist"))?;
            Reply::Visualization {
                id,
                mime_type: visualization.mime_type,
                content: visualization.content,
            }
        }
        Request::RecordVisualization { mime_type, content } => {
            let id = services.visualizations.record(mime_type, content).await?;
            Reply::VisualizationRecorded { id }
        }
        Request::QuotaUsage => Reply::QuotaUsage {
            summaries: combined_quota_summaries(&services.db, &services.inference),
        },
        Request::QuotaHistory => Reply::QuotaHistory {
            series: quota_history(&services.db, &services.inference),
        },
        Request::GlobalUsage { since_ms } => {
            services.pool.flush_agent_usage(None).await;
            let usage = services
                .db
                .read()
                .global_agent_usage(rho_agent_host_proto::UnixMs(since_ms));
            Reply::GlobalUsage {
                series: hourly_global_usage_series(usage),
            }
        }
        Request::AgentCostDistribution { since_ms } => {
            const DAY_MS: u64 = 24 * 60 * 60 * 1_000;
            const MAX_HISTORY_DAYS: u64 = 30 + 14 + rho_agent_host_proto::AGENT_COST_WINDOW_DAYS;

            services.pool.flush_agent_usage(None).await;
            let now = rho_agent_host_proto::UnixMs::now().0;
            let earliest = since_ms
                .saturating_sub(rho_agent_host_proto::AGENT_COST_WINDOW_DAYS * DAY_MS)
                .max(now.saturating_sub(MAX_HISTORY_DAYS * DAY_MS));
            Reply::AgentCostDistribution {
                series: hourly_agent_cost_series(
                    &services.db,
                    rho_agent_host_proto::UnixMs(earliest),
                )?,
            }
        }
        Request::TerminalList { agent } => Reply::TerminalList {
            terminals: terminal_list(services, agent.as_deref()).await?,
        },
        Request::ShellStart { agent } => {
            shell_start(services, &agent).await?;
            Reply::Done
        }
        Request::ShellList { agent } => Reply::ShellList {
            shells: shell_list(services, agent.as_deref()).await?,
        },
        Request::ShellClose { agent } => {
            shell_close(services, &agent).await?;
            Reply::Done
        }
        Request::GitTransportPolicy { host } => Reply::GitTransportPolicy {
            pat_available: host == "github.com"
                && services.platform_secrets.contains_nonempty("GITHUB_TOKEN"),
        },
        Request::GuiTelemetryUpload { snapshot } => Reply::GuiTelemetryStored {
            path: store_gui_telemetry(snapshot).await?,
        },
        Request::PlatformSecretsSet { secrets } => {
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
            Reply::PlatformStatus { running, detail }
        }
        Request::Pr {
            agent_id: _,
            command,
        } => {
            let result = async {
                match command {
                    rho_agent_host_proto::PrCommand::Create {
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
                    rho_agent_host_proto::PrCommand::Subscribe { .. } => Ok((
                        "persistent PR subscriptions were removed; poll `rho pr status` instead"
                            .to_owned(),
                        Vec::new(),
                    )),
                    rho_agent_host_proto::PrCommand::Status { url } => services
                        .pr_monitor
                        .status(&url)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_agent_host_proto::PrCommand::List => Ok(("[]".to_owned(), Vec::new())),
                    rho_agent_host_proto::PrCommand::Stop { .. } => Ok((
                        "persistent PR subscriptions were removed".to_owned(),
                        Vec::new(),
                    )),
                    rho_agent_host_proto::PrCommand::Comment {
                        url,
                        reply_comment,
                        body,
                    } => services
                        .pr_monitor
                        .comment(&url, reply_comment, &body)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_agent_host_proto::PrCommand::Comments { url } => services
                        .pr_monitor
                        .comments(&url)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_agent_host_proto::PrCommand::Checks { url } => services
                        .pr_monitor
                        .checks(&url)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_agent_host_proto::PrCommand::Edit {
                        url,
                        base,
                        title,
                        body,
                    } => services
                        .pr_monitor
                        .edit(&url, base, title, body)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_agent_host_proto::PrCommand::Rerun { url, run_id } => services
                        .pr_monitor
                        .rerun(&url, run_id)
                        .await
                        .map(|output| (output, Vec::new())),
                    rho_agent_host_proto::PrCommand::Logs { url, run_id } => {
                        services.pr_monitor.logs(&url, run_id).await.map(|data| {
                            (format!("downloaded logs for run {run_id}"), data.to_vec())
                        })
                    }
                }
            }
            .await;
            match result {
                Ok((output, data)) => Reply::Pr {
                    output,
                    data,
                    is_error: false,
                },
                Err(error) => Reply::Pr {
                    output: format!("{error:#}"),
                    data: Vec::new(),
                    is_error: true,
                },
            }
        }
        Request::Snapshot => Reply::Snapshotted {
            path: debug::daemon_snapshot(&services.db).await?,
        },
        Request::IrohApprove { code } => {
            let auth =
                iroh_auth.context("daemon is not listening over iroh (start it with --iroh)")?;
            let code = code
                .parse::<rho_iroh_auth::EnrollmentCode>()
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            let endpoint_id = auth
                .approve_code(&code)
                .await
                .map_err(|_| anyhow::anyhow!("no pending enrollment has this code"))?;
            Reply::IrohApproved {
                endpoint_id: endpoint_id.to_string(),
            }
        }
        Request::IrohTrustInMemory { endpoint_id } => {
            let auth =
                iroh_auth.context("daemon is not listening over iroh (start it with --iroh)")?;
            let endpoint_id = endpoint_id
                .parse::<iroh::EndpointId>()
                .context("invalid iroh client endpoint id")?;
            auth.trust_in_memory(endpoint_id).await;
            Reply::IrohApproved {
                endpoint_id: endpoint_id.to_string(),
            }
        }
        Request::IrohRevoke { endpoint_id } => {
            let auth =
                iroh_auth.context("daemon is not listening over iroh (start it with --iroh)")?;
            let endpoint_id = endpoint_id
                .parse::<iroh::EndpointId>()
                .context("invalid iroh client endpoint id")?;
            anyhow::ensure!(
                auth.revoke(endpoint_id).await,
                "iroh client is not enrolled"
            );
            Reply::IrohRevoked {
                endpoint_id: endpoint_id.to_string(),
            }
        }
    };
    Ok((reply, Refresh::None))
}

async fn handle_agent_command(
    services: &Arc<Services>,
    command: AgentCommand,
) -> anyhow::Result<(Reply, Refresh)> {
    match command {
        AgentCommand::New {
            role,
            start,
            mode,
            mut content,
        } => {
            if let Some(content) = content.as_mut() {
                prepare_image_content(content).await?;
            }
            // Control streams hear of the agent from the pool's creation
            // broadcast; the reply tells this client which one is its own.
            let (agent_id, agent) = services.create(role, start, mode).await?;
            if let Some(content) = content {
                // The agent is fresh, so the lanes are equivalent here.
                agent
                    .send_user_content_accepted(content, MessageDelivery::NextRequest)
                    .await?;
            }
            Ok((Reply::AgentCreated { agent_id }, Refresh::Ready))
        }
        AgentCommand::Send {
            agent_id,
            mut content,
            delivery,
        } => {
            prepare_image_content(&mut content).await?;
            let (_, agent, _) = services.load(agent_id).await?;
            // What Rho has to tell the agent goes ahead of the person's
            // words, once: the loop's head forgets it as soon as the
            // message is accepted, the log when the message's row lands.
            let notice = agent.head().pending_notice;
            if let Some(text) = notice.clone() {
                content.insert(0, rho_agent_host_proto::ContentPart::Text { text });
            }
            agent.send_user_content_accepted(content, delivery).await?;
            if notice.is_some() {
                agent.notice_carried();
            }
            Ok((Reply::Done, Refresh::None))
        }
        // A compaction rides the next request whichever lane the client
        // named; the lane is not a thing the runtime reads for it.
        AgentCommand::Compact {
            agent_id,
            delivery: _,
        } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.compact();
            Ok((Reply::Done, Refresh::None))
        }
        AgentCommand::ChangeRole { agent_id, role } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.change_role(role).await?;
            Ok((Reply::Done, Refresh::Ready))
        }
        AgentCommand::ChangeMode { agent_id, mode } => {
            let changed = services.pool.change_mode(agent_id, mode).await?;
            for id in changed {
                if id != agent_id && services.pool.is_live(id) {
                    services.load(id).await?;
                }
            }
            // Back at once, in the new view, for whoever is looking.
            services.load(agent_id).await?;
            Ok((Reply::Done, Refresh::Ready))
        }
        AgentCommand::ChangePromptCacheKey { agent_id } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.change_prompt_cache_key()?;
            Ok((Reply::Done, Refresh::None))
        }
        AgentCommand::Cancel { agent_id } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.cancel();
            Ok((Reply::Done, Refresh::None))
        }
        AgentCommand::Rewind { agent_id, turns } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.rewind(turns).await?;
            Ok((Reply::Done, Refresh::Ready))
        }
        AgentCommand::Continue { agent_id } => {
            let (_, agent, _) = services.load(agent_id).await?;
            agent.retry();
            Ok((Reply::Done, Refresh::None))
        }
    }
}

/// Attaches a dedicated Comint-style shell stream. The daemon retains the
/// process when this client detaches.
async fn serve_shell<R, W>(
    services: Arc<Services>,
    reader: R,
    mut writer: W,
    agent: String,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let client = match shell_attach(&services, &agent).await {
        Ok(client) => client,
        Err(error) => {
            let _ = write_frame(
                &mut writer,
                &Opened::Refused {
                    reason: format!("{error:#}"),
                },
            )
            .await;
            return Err(error);
        }
    };
    write_frame(&mut writer, &Opened::Ready).await?;
    client.relay::<_, _, rho_agent_host_proto::shell::ShellClientFrame, rho_agent_host_proto::shell::ShellServerFrame>(reader, writer).await
}

async fn shell_start(services: &Arc<Services>, agent: &str) -> anyhow::Result<()> {
    let agent = services.resolve_display_agent_id(agent).await?;
    let process = services.pool.execution(agent).await?;
    let cwd = services.db.read().get_agent(agent).config.place.cwd;
    process
        .action(rho_agent::WorksetAction::ShellStart {
            agent,
            cwd,
            program: rho_shell_program().into(),
            pager: rho_pager_program().into(),
        })
        .await?;
    Ok(())
}

async fn shell_attach(
    services: &Arc<Services>,
    agent: &str,
) -> anyhow::Result<rho_agent::WorksetClient> {
    let agent = services.resolve_display_agent_id(agent).await?;
    services
        .pool
        .execution(agent)
        .await?
        .attach(rho_agent::WorksetAttach::Shell { agent })
        .await
}

async fn shell_list(
    services: &Arc<Services>,
    agent: Option<&str>,
) -> anyhow::Result<Vec<rho_agent_host_proto::shell::ShellInfo>> {
    let filter = match agent {
        Some(agent) => Some(services.resolve_display_agent_id(agent).await?.encoded()),
        None => None,
    };
    let mut shells = Vec::new();
    for process in services.pool.executions().await {
        if let rho_agent::WorksetReply::Shells(entries) =
            process.action(rho_agent::WorksetAction::ShellList).await?
        {
            shells.extend(
                entries
                    .into_iter()
                    .filter(|entry| filter.as_ref().is_none_or(|agent| &entry.agent == agent)),
            );
        }
    }
    Ok(shells)
}

async fn shell_close(services: &Arc<Services>, agent: &str) -> anyhow::Result<()> {
    let agent = services.resolve_display_agent_id(agent).await?;
    services
        .pool
        .execution(agent)
        .await?
        .action(rho_agent::WorksetAction::ShellClose { agent })
        .await?;
    Ok(())
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

async fn store_gui_telemetry(snapshot: Vec<u8>) -> anyhow::Result<String> {
    anyhow::ensure!(
        snapshot.len() <= rho_agent_host_proto::MAX_GUI_TELEMETRY_BYTES,
        "GUI telemetry snapshot is too large ({} bytes; limit is {} bytes)",
        snapshot.len(),
        rho_agent_host_proto::MAX_GUI_TELEMETRY_BYTES
    );
    let path = tokio::task::spawn_blocking(move || {
        let state = dirs::state_dir().context("state directory not available")?;
        persist_gui_telemetry(&state.join("rho"), &snapshot)
    })
    .await
    .context("GUI telemetry storage task failed")?
    .context("failed to store GUI telemetry")?;
    Ok(path.display().to_string())
}

fn persist_gui_telemetry(state_root: &std::path::Path, snapshot: &[u8]) -> anyhow::Result<PathBuf> {
    use std::io::Write as _;

    anyhow::ensure!(
        snapshot.len() <= rho_agent_host_proto::MAX_GUI_TELEMETRY_BYTES,
        "GUI telemetry snapshot exceeds the {} byte limit",
        rho_agent_host_proto::MAX_GUI_TELEMETRY_BYTES
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

/// Serves a stream dedicated to one daemon-owned terminal: spawns or attaches
/// (per [`TerminalOpen`](rho_agent_host_proto::term::TerminalOpen)), replies
/// `Opened::Ready`, then pumps
/// [`rho_agent_host_proto::term`] frames until either side closes. Closing only
/// detaches; the terminal keeps running. A headless create replies and
/// returns without attaching.
#[expect(clippy::too_many_arguments)]
async fn serve_terminal<R, W>(
    services: Arc<Services>,
    reader: R,
    mut writer: W,
    agent: String,
    terminal_id: u64,
    open: rho_agent_host_proto::term::TerminalOpen,
    cols: u16,
    rows: u16,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let create = matches!(
        open,
        rho_agent_host_proto::term::TerminalOpen::Create { .. }
    );
    let attached = terminal_attach(&services, &agent, terminal_id, create, cols, rows).await;
    let client = match attached {
        Ok(attached) => attached,
        Err(error) => {
            let _ = write_frame(
                &mut writer,
                &Opened::Refused {
                    reason: format!("{error:#}"),
                },
            )
            .await;
            return Err(error);
        }
    };
    write_frame(&mut writer, &Opened::Ready).await?;
    if matches!(
        open,
        rho_agent_host_proto::term::TerminalOpen::Create { attach: false }
    ) {
        // Headless create: the terminal keeps running with no clients.
        return Ok(());
    }

    client
        .relay::<_, _, rho_agent_host_proto::term::TermClientFrame, rho_agent_host_proto::term::TermServerFrame>(
            reader, writer,
        )
        .await
}

/// Resolve metadata and attach to workset-owned execution; no agent activation.
async fn terminal_attach(
    services: &Arc<Services>,
    agent: &str,
    terminal_id: u64,
    create: bool,
    cols: u16,
    rows: u16,
) -> anyhow::Result<rho_agent::WorksetClient> {
    let agent = services.resolve_display_agent_id(agent).await?;
    let process = services.pool.execution(agent).await?;
    let cwd = services.db.read().get_agent(agent).config.place.cwd;
    let shell = services
        .user_environment
        .get("SHELL")
        .and_then(|shell| shell.to_str())
        .unwrap_or("bash");
    let shell = std::fs::canonicalize(shell)
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok())
        .unwrap_or_else(|| shell.to_owned());
    process
        .attach(rho_agent::WorksetAttach::Terminal {
            agent,
            terminal: terminal_id,
            create,
            cols,
            rows,
            cwd,
            shell,
        })
        .await
}

/// The daemon's terminals, or one agent's.
async fn terminal_list(
    services: &Arc<Services>,
    agent: Option<&str>,
) -> anyhow::Result<Vec<rho_agent_host_proto::term::TerminalInfo>> {
    let filter = match agent {
        Some(agent) => Some(services.resolve_display_agent_id(agent).await?.encoded()),
        None => None,
    };
    let mut terminals = Vec::new();
    for process in services.pool.executions().await {
        if let rho_agent::WorksetReply::Terminals(entries) = process
            .action(rho_agent::WorksetAction::TerminalList)
            .await?
        {
            terminals.extend(
                entries
                    .into_iter()
                    .filter(|entry| filter.as_ref().is_none_or(|agent| &entry.agent == agent)),
            );
        }
    }
    Ok(terminals)
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
    let checkout = match open_checkout(&services, &workspace).await {
        Ok((_, checkout)) => checkout,
        Err(error) => {
            let _ = write_frame(
                &mut writer,
                &Opened::Refused {
                    reason: format!("{error:#}"),
                },
            )
            .await;
            return Err(error);
        }
    };
    let files = match workspace_channel::WorkspaceFiles::open(checkout) {
        Ok(files) => Arc::new(files),
        Err(error) => {
            let _ = write_frame(
                &mut writer,
                &Opened::Refused {
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
                &Opened::Refused {
                    reason: format!("watch workspace: {error:#}"),
                },
            )
            .await;
            return Err(error);
        }
    };
    write_frame(&mut writer, &Opened::Ready).await?;

    use rho_agent_host_proto::workspace::{WorkspaceClientFrame, WorkspaceServerFrame};
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
                // scheduling a fresh semantic barrier.
                rho_agent_host_proto::write_frame_limited(
                    &mut writer,
                    &WorkspaceServerFrame::Changed {
                        paths: Vec::new(),
                        rescan: true,
                    },
                    rho_agent_host_proto::workspace::MAX_WORKSPACE_FRAME_LEN,
                )
                .await?;
            }
            frame = rho_agent_host_proto::read_frame_limited::<_, WorkspaceClientFrame>(
                &mut reader,
                rho_agent_host_proto::workspace::MAX_WORKSPACE_FRAME_LEN,
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
                rho_agent_host_proto::write_frame_limited(
                    &mut writer,
                    &response,
                    rho_agent_host_proto::workspace::MAX_WORKSPACE_FRAME_LEN,
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
                rho_agent_host_proto::write_frame_limited(
                    &mut writer,
                    &WorkspaceServerFrame::Changed { paths, rescan },
                    rho_agent_host_proto::workspace::MAX_WORKSPACE_FRAME_LEN,
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
    pos: rho_agent_host_proto::transcript::AgentPos,
) -> rho_agent_host_proto::transcript::DetailBody {
    use rho_agent_host_proto::transcript::DetailBody;
    let event = db.read().agent_event(agent_id, pos.into());
    if let Some(native) = event.as_ref().and_then(rho_agent::AgentEvent::native_event) {
        use rho_agent::native::NativeEvent;
        return match native {
            NativeEvent::RequestStarted { input, .. } => DetailBody::Results(
                input
                    .iter()
                    .flat_map(|item| match item {
                        rho_inference::types::ContextBlock::ToolResults { results } => {
                            results.iter().map(detail_result).collect::<Vec<_>>()
                        }
                        rho_inference::types::ContextBlock::ToolUpdate(update) => {
                            vec![detail_update(&update)]
                        }
                        _ => Vec::new(),
                    })
                    .collect(),
            ),
            NativeEvent::ResponseFinished { output, .. } => DetailBody::Response(
                output
                    .iter()
                    .filter_map(|entry| match entry {
                        rho_inference::types::ContextBlock::InferenceResponse { items, .. } => {
                            Some(items)
                        }
                        _ => None,
                    })
                    .flatten()
                    .filter_map(rho_agent::transcript::item)
                    .collect(),
            ),
            NativeEvent::RequestFailed { partial, .. } => DetailBody::Response(
                partial
                    .items
                    .iter()
                    .filter_map(|slot| match slot {
                        rho_inference::types::StreamingContextItemState::Pending(item)
                        | rho_inference::types::StreamingContextItemState::Finished(item) => item
                            .to_context_item()
                            .ok()
                            .and_then(|item| rho_agent::transcript::item(&item)),
                        _ => None,
                    })
                    .collect(),
            ),
        };
    }
    match event {
        Some(rho_agent::AgentEvent::Transcript { line, .. }) => match line {
            rho_agent::TranscriptLine::Assistant { text, calls, .. } => DetailBody::Response(
                (!text.is_empty())
                    .then_some(rho_agent_host_proto::transcript::Item::Text { text, phase: None })
                    .into_iter()
                    .chain(calls.into_iter().map(|call| {
                        rho_agent_host_proto::transcript::Item::ToolCall {
                            id: call.id,
                            name: call.name,
                            arguments: call.arguments,
                            format: rho_agent_host_proto::transcript::ArgumentsFormat::Json,
                        }
                    }))
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
                    rho_inference::types::StreamingContextItemState::Pending(item)
                    | rho_inference::types::StreamingContextItemState::Finished(item) => {
                        rho_agent::live::to_item(item)
                    }
                    rho_inference::types::StreamingContextItemState::Empty => None,
                })
                .collect(),
        ),
        _ => DetailBody::Nothing,
    }
}

fn detail_result(
    result: &rho_inference::types::ToolResult,
) -> rho_agent_host_proto::transcript::DetailResult {
    use rho_agent_host_proto::transcript::ToolStatus;
    rho_agent_host_proto::transcript::DetailResult {
        id: result.call_id.as_str().to_owned(),
        status: match result.body.status {
            rho_agent_host_proto::ToolOutputStatus::Success => ToolStatus::Success,
            rho_agent_host_proto::ToolOutputStatus::Error => ToolStatus::Error,
            rho_agent_host_proto::ToolOutputStatus::Cancelled => ToolStatus::Cancelled,
        },
        output: result.body.recorded_output().to_owned(),
        error: None,
    }
}

fn detail_update(
    update: &rho_inference::types::ToolUpdate,
) -> rho_agent_host_proto::transcript::DetailResult {
    rho_agent_host_proto::transcript::DetailResult {
        id: update.call_id.as_str().to_owned(),
        status: rho_agent_host_proto::transcript::ToolStatus::Success,
        output: update.recorded_output().to_owned(),
        error: None,
    }
}

/// Repo roots must be absolute (the daemon's cwd is meaningless by design):
/// agents start on daemon-made clones, so both workdir registration and
/// agent creation take repos. A leading `~` expands
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
    if encoded_total > rho_agent_host_proto::MAX_FRAME_LEN.saturating_sub(1024 * 1024) {
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

/// The workset behind an agent's place and the root of the git checkout
/// (or plain directory) its working directory is in, on the host.
async fn open_checkout(
    services: &Services,
    workspace: &WorkspaceInfo,
) -> anyhow::Result<(rho_fs_view::Workset, Utf8PathBuf)> {
    let place = workspace
        .place()
        .context("agents no longer work in the user's own checkout")?;
    let (workset, _, host_cwd) = services.pool.open_workset(place).await?;
    let (root, _) = rho_fs_view::resolve_workdir_root(host_cwd.as_std_path())?;
    Ok((workset, root))
}

/// Where a host directory inside `workset` appears to its agents.
fn visible_path(
    workset: &rho_fs_view::Workset,
    host_path: &Utf8Path,
) -> anyhow::Result<Utf8PathBuf> {
    let relative = host_path
        .strip_prefix(workset.root())
        .with_context(|| format!("{host_path} is outside workset {}", workset.id()))?;
    Ok(Utf8Path::new(rho_fs_view::MOUNT_ROOT).join(relative))
}

fn expand_home(path: &Utf8Path) -> Option<Utf8PathBuf> {
    let rest = path.strip_prefix("~").ok()?;
    let home = Utf8PathBuf::try_from(dirs::home_dir()?).ok()?;
    Some(home.join(rest))
}

#[cfg(test)]
mod daemon_directory_tests {
    use std::ffi::OsString;

    use super::apply_daemon_directories;

    /// The environment an agent gets says where this daemon works, not where
    /// a login shell would have gone. A rig daemon's agent otherwise writes
    /// under the rig's HOME by XDG default and reads a Claude config home
    /// nobody named; the capture carries neither, and a stale value from the
    /// login shell has to lose to the daemon's.
    #[test]
    fn the_daemon_names_the_directories_its_agents_work_in() {
        let root = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(root.path()).unwrap();
        let state_dir = root.join("state").join("rho");
        let claude = rho_claude::accounts::ClaudePaths::at(root.join("config").join("claude"));

        let mut environment: Vec<(OsString, OsString)> = vec![
            ("PATH".into(), "/usr/bin".into()),
            // What a login shell left behind: the user's, not this daemon's.
            ("XDG_STATE_HOME".into(), "/home/someone/.local/state".into()),
        ];
        apply_daemon_directories(&mut environment, &state_dir, &claude);

        let value = |name: &str| {
            environment
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.to_string_lossy().into_owned())
        };
        assert_eq!(
            value("XDG_STATE_HOME").as_deref(),
            Some(root.join("state").as_str())
        );
        assert_eq!(
            value("CLAUDE_CONFIG_DIR").as_deref(),
            Some(root.join("config").join("claude").as_str())
        );
        assert_eq!(
            value("PATH").as_deref(),
            Some("/usr/bin"),
            "the rest is left alone"
        );
        // The two the daemon has no say over: passed on as it was started
        // with them, absent when it was started without them.
        for name in ["XDG_CONFIG_HOME", "XDG_DATA_HOME"] {
            assert_eq!(
                value(name),
                std::env::var_os(name).map(|value| value.to_string_lossy().into_owned()),
                "{name} is this process's own"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, OsString};
    use std::os::fd::AsRawFd as _;
    use std::sync::Arc;

    use rho_agent::db::{AgentWriteTxnExt, QuotaModel, QuotaObservationRecord, QuotaProvider};
    use rho_agent_host_proto::ContentPart;
    use rho_agent_host_proto::control::ServerFrame as ControlFrame;
    use rho_db::RhoDb;

    use super::{
        AgentUsageModel, GitProviderClaim, GitTransportBroker, MAX_IMAGE_BASE64_BYTES,
        MAX_INPUT_IMAGES, PlatformSecrets, claude_quota_history, configure_octo_git_transport,
        hourly_global_usage_series, merge_hourly_agent_cost_bucket, persist_gui_telemetry,
        prepare_image_content, quota_burn, quota_summaries, start_runtime_sockets,
        validate_image_content,
    };

    #[test]
    fn tool_detail_reads_the_complete_host_record() {
        let result = rho_inference::types::ToolResult {
            call_id: rho_inference::types::ToolCallId::try_from("call-1").unwrap(),
            tool_type: rho_inference::types::ToolType::Custom,
            body: rho_inference::types::ToolOutput {
                output: Arc::new("bounded model view".to_owned()),
                full_output: Some(Arc::new("complete host record".to_owned())),
                images: Arc::new(Vec::new()),
                status: rho_agent_host_proto::ToolOutputStatus::Success,
            },
            started_at: rho_agent_host_proto::UnixMs(1),
            finished_at: rho_agent_host_proto::UnixMs(2),
            metadata: None,
        };

        assert_eq!(super::detail_result(&result).output, "complete host record");

        let update = rho_inference::types::ToolUpdate {
            status: None,
            images: Default::default(),
            call_id: rho_inference::types::ToolCallId::try_from("call-1").unwrap(),
            tool_type: rho_inference::types::ToolType::Custom,
            output: Arc::new("bounded update".to_owned()),
            full_output: Some(Arc::new("complete update".to_owned())),
            at: rho_agent_host_proto::UnixMs(3),
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
        let paths =
            rho_agent_host_proto::RuntimePaths::new(Some(runtime.path().join("rho.sock"))).unwrap();
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
        let paths =
            rho_agent_host_proto::RuntimePaths::new(Some(runtime.path().join("rho.sock"))).unwrap();
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
        let paths =
            rho_agent_host_proto::RuntimePaths::new(Some(runtime.path().join("rho.sock"))).unwrap();
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

        assert_eq!(series.len(), 6);
        assert_eq!(series[0].model, "fable");
        assert_eq!(series[0].buckets.len(), 1);
        assert_eq!(series[0].buckets[0].bucket_start_ms, 0);
        assert_eq!(series[0].buckets[0].input_tokens, 30);
        assert_eq!(series[0].buckets[0].requests, 2);
        assert_eq!(series[1].model, "gpt");
        assert_eq!(series[1].buckets[0].bucket_start_ms, 60 * 60 * 1_000);
        assert_eq!(series[5].model, "astra");
        assert!(series[5].buckets.is_empty());
    }

    #[test]
    fn agent_cost_history_rejects_more_than_its_hourly_bucket_limit() {
        let agent_id =
            rho_agent::db::AgentId::from_counter(1, &rho_agent_host_proto::AgentIdDomain(0))
                .unwrap();
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
            observed_at: rho_agent_host_proto::UnixMs(at),
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
            observed_at: rho_agent_host_proto::UnixMs(at),
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
            observed_at: rho_agent_host_proto::UnixMs(at),
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
        let now = rho_agent_host_proto::UnixMs::now().0;
        let mut write = db.write().await;
        for index in 0..5 {
            assert!(write.record_quota_observation(QuotaObservationRecord {
                provider: QuotaProvider::Claude,
                model: QuotaModel::OPUS,
                auth_namespace: Some("default".to_owned()),
                observed_at: rho_agent_host_proto::UnixMs(now - (4 - index) * 1_000),
                used_percent: index as u8,
                reset_at_unix: Some(123),
            }));
        }
        assert!(write.record_quota_observation(QuotaObservationRecord {
            provider: QuotaProvider::Claude,
            model: QuotaModel::FABLE,
            auth_namespace: None,
            observed_at: rho_agent_host_proto::UnixMs(now),
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
    async fn quota_summary_expires_stale_provider_window() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        let now = rho_agent_host_proto::UnixMs::now();
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
        let request = rho_agent_host_proto::GitTransportRequest {
            host: "git.example".to_owned(),
            port: 22,
            user: "git".to_owned(),
            repository: "team/repo.git".to_owned(),
            service: rho_agent_host_proto::GitService::ReceivePack,
            planned_refs: Some(vec!["refs/heads/main".to_owned()]),
        };
        let waiting = {
            let broker = broker.clone();
            tokio::spawn(async move { broker.request(request).await })
        };
        let (request_id, first_provider) = match first_rx.recv().await.unwrap() {
            ControlFrame::GitTransportRequested {
                request_id,
                provider_id,
                ..
            } => (request_id, provider_id),
            message => panic!("unexpected provider message: {message:?}"),
        };
        let second_provider = match second_rx.recv().await.unwrap() {
            ControlFrame::GitTransportRequested {
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
            Some(ControlFrame::GitTransportDone {
                request_id: done_request
            }) if done_request == request_id
        ));
    }

    #[tokio::test]
    async fn git_transport_broker_rejects_without_registered_clients() {
        let result = GitTransportBroker::default()
            .request(rho_agent_host_proto::GitTransportRequest {
                host: "git.example".to_owned(),
                port: 22,
                user: "git".to_owned(),
                repository: "team/repo.git".to_owned(),
                service: rho_agent_host_proto::GitService::UploadPack,
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
                        rho_agent_host_proto::GitTransportRequest {
                            host: "git.example".to_owned(),
                            port: 22,
                            user: "git".to_owned(),
                            repository: "team/repo.git".to_owned(),
                            service: rho_agent_host_proto::GitService::UploadPack,
                            planned_refs: None,
                        },
                        std::time::Duration::from_millis(10),
                    )
                    .await
            })
        };
        let request_id = match provider_rx.recv().await.unwrap() {
            ControlFrame::GitTransportRequested { request_id, .. } => request_id,
            message => panic!("unexpected provider message: {message:?}"),
        };
        let error = match waiting.await.unwrap() {
            Ok(_) => panic!("request unexpectedly received a provider"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("within 60 seconds"));
        assert!(matches!(
            provider_rx.recv().await,
            Some(ControlFrame::GitTransportDone {
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
                &vec![0; rho_agent_host_proto::MAX_GUI_TELEMETRY_BYTES + 1]
            )
            .unwrap_err()
            .to_string()
            .contains("exceeds")
        );
    }
}

/// Authenticate before reading even the media-open request. No video capture
/// exists until the authenticated desktop-open requests the fixed video track.
async fn serve_wayland<R, W>(
    services: Arc<Services>,
    transport: rho_rpc::media::Session,
    mut reader: R,
    mut writer: W,
    agent: String,
    name: String,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use rho_desktop_proto::{Input, Packet, Request, Response};
    let result = async {
        let agent = services.resolve_display_agent_id(&agent).await?;
        let process = services.pool.execution(agent).await?;
        #[cfg(not(target_os = "linux"))]
        anyhow::bail!("agent desktops require a Linux daemon");
        #[cfg(target_os = "linux")]
        {
            let rho_agent::WorksetReply::Desktop { socket } = process
                .action(rho_agent::WorksetAction::Desktop {
                    agent,
                    session: name,
                })
                .await?
            else {
                anyhow::bail!("unexpected desktop discovery reply");
            };
            let desktop = rho_desktop_proto::local::Desktop::open(&socket).await?;
            let mut control = desktop.control;
            // Desktop-open itself requests a fresh keyframe, including late joins
            // to a static desktop whose cached keyframe has expired.
            anyhow::ensure!(
                matches!(
                    rho_desktop_proto::local::request(
                        &mut control,
                        Request::Input {
                            input: Input::Quality {
                                bitrate: 2_000_000,
                                keyframe: true
                            },
                        },
                    )
                    .await?,
                    Response::Done
                ),
                "unexpected desktop quality reply"
            );
            let from_gui = async {
                loop {
                    let (input, _) =
                        rho_rpc::read_frame::<_, Input>(&mut reader, 64 * 1024).await?;
                    anyhow::ensure!(
                        matches!(
                            rho_desktop_proto::local::request(
                                &mut control,
                                Request::Input { input }
                            )
                            .await?,
                            Response::Done
                        ),
                        "unexpected desktop input reply"
                    );
                }
                #[allow(unreachable_code)]
                Ok::<(), anyhow::Error>(())
            };
            let media = async {
                let origin = rho_desktop_media::media::origin();
                let local = rho_desktop_media::media::SessionGuard(
                    rho_desktop_media::media::local_client(desktop.media, origin.clone()).await?,
                );
                let remote = rho_desktop_media::media::publish(transport, &origin).await?;
                tokio::select! {
                    error=local.0.closed()=>anyhow::bail!("desktop media closed: {error}"),
                    _=remote.closed()=>Ok::<(),anyhow::Error>(()),
                }
            };
            tokio::select! {result=from_gui=>result,result=media=>result}
        }
    }
    .await;
    if let Err(error) = &result {
        let _ = rho_rpc::write_frame(&mut writer, &Packet::Error(format!("{error:#}")), 64 * 1024)
            .await;
    }
    result
}
