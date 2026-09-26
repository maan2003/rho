use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use rho_agent::db::AgentReadTxnExt as _;
use rho_agent::pool::{AgentPool, RunningAgent};
use rho_agent_hosts::protocol::GitProviderFrame;
use rho_agent_types::{AgentId, AgentRole, ContentPart, Place, WorksetMode, WorkspaceInfo};
use rho_agents_client::protocol::{AuthState, JoinTarget, StartMode};
use rho_db::RhoDb;
use rho_inference::Inference;
use rho_rpc::protocol::server::{Server, ServerConnection};
use rho_rpc::protocol::{Open, Opened, Protocol, read_frame, write_frame};
use tokio::sync::{Mutex as TokioMutex, mpsc, oneshot};

mod agents;
mod agents2;
pub mod debug;
mod desktop;
mod host;
mod live;
mod realtime;
mod secret_store;
mod transcript;
mod usage;
pub mod workspace_channel;

/// FDNAME under which messaging-platform secrets live in the systemd fd store.
const PLATFORM_SECRETS_FD_STORE_NAME: &str = "platform-secrets";
pub fn default_socket_path() -> anyhow::Result<PathBuf> {
    rho_rpc::protocol::socket_path()
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

/// Puts the directories this agent host was told to use into the environment
/// its agents are spawned with.
///
/// `login_environment` starts from a cleared environment and a login shell,
/// so what comes back is whatever that shell chose and none of what this
/// agent host was named. An agent would then work in the XDG defaults under
/// HOME and read a Claude config home nobody named, agreeing with the agent
/// host only by luck. What the agent host resolved wins here; the two
/// directories it has no say over are passed on as it was started with them, or
/// left to the login shell when it was started without them.
fn apply_host_directories(
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
    paths: &rho_rpc::protocol::RuntimePaths,
) -> anyhow::Result<std::fs::File> {
    let path = paths.host_lock();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open agent host runtime lock {}", path.display()))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        anyhow::bail!(
            "refusing to start: runtime directory {} is owned by another agent host (lock file {})",
            paths.directory().display(),
            path.display()
        );
    }
    Ok(lock)
}

struct RuntimeSockets {
    paths: rho_rpc::protocol::RuntimePaths,
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
    let paths = rho_rpc::protocol::RuntimePaths::new(socket_path)?;
    std::fs::create_dir_all(paths.directory()).context("create runtime directory")?;
    let lock = lock_runtime_directory(&paths)?;
    let octo_socket = paths.octo_socket();
    prepare_socket_path(paths.socket(), "rho-agent-host")?;
    prepare_socket_path(&octo_socket, "octo")?;
    let octo_listener = tokio::net::UnixListener::bind(&octo_socket)
        .with_context(|| format!("bind octo socket {}", octo_socket.display()))?;
    let listener = tokio::net::UnixListener::bind(paths.socket())
        .with_context(|| format!("bind rho-agent-host socket {}", paths.socket().display()))?;
    spawn_octo_server(octo_listener, secrets)?;
    Ok(RuntimeSockets {
        paths,
        server: Server::from_listener(listener),
        _lock: lock,
    })
}

pub use rho_fs_view::PathOverrides;

const FIND_DENY_ROOTS_ENV: &str = "FIND_DENY_ROOTS";

fn find_deny_roots() -> OsString {
    let home = dirs::home_dir().expect("home directory must be available");
    std::env::join_paths([PathBuf::from("/"), PathBuf::from("/nix/store"), home])
        .expect("protected root paths must not contain a path separator")
}

/// This must run before the Tokio runtime starts, because mutating the process
/// environment is not thread-safe.
pub fn configure_embedded_environment() {
    // SAFETY: called by rho-agent-host's main before it creates the Tokio runtime.
    unsafe { std::env::set_var(FIND_DENY_ROOTS_ENV, find_deny_roots()) };
}

#[derive(Clone, Debug, clap::Args)]
pub struct HostArgs {
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
    /// its own; without it the agent host uses the user's, `$CLAUDE_CONFIG_DIR`
    /// or `~/.claude`. Deliberately not an `env =` argument: an agent host
    /// pointed at a rig's state directory picked the user's transcripts up
    /// out of the environment and rebuilt agent rows from them.
    #[arg(long, value_name = "DIR")]
    pub claude_config_dir: Option<camino::Utf8PathBuf>,
}

pub struct HostProfiler(Option<rho_profiling::CpuProfiler>);

impl HostProfiler {
    /// Start profiling before the async runtime creates worker threads.
    pub fn start(args: &mut HostArgs) -> anyhow::Result<Self> {
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
                Ok(path) => eprintln!("rho-agent-host: wrote CPU profile to {}", path.display()),
                Err(error) if result.is_err() => {
                    eprintln!("rho-agent-host: failed to write CPU profile: {error:#}");
                }
                Err(error) => return Err(error.context("write agent host CPU profile")),
            }
        }
        result
    }
}

pub async fn run(args: HostArgs) -> anyhow::Result<()> {
    let platform_secrets = PlatformSecrets::from_fd_store();
    let runtime = start_runtime_sockets(args.socket_path, platform_secrets.clone())?;

    // The agent host's own cwd must never matter: agents each carry their own
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
    eprintln!(
        "rho-agent-host: Claude configuration {}",
        claude.config_home()
    );

    let mut user_environment = login_environment()?;
    user_environment.push((FIND_DENY_ROOTS_ENV.into(), find_deny_roots()));
    user_environment.push((
        rho_rpc::protocol::RuntimePaths::SOCKET_ENV.into(),
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
    apply_host_directories(&mut user_environment, &state_dir, &claude);
    let user_environment = rho_fs_view::UserEnvironment::new(user_environment);

    let db = RhoDb::open(db_path);
    let agent2_base_url = args
        .openai_base_url
        .clone()
        .unwrap_or_else(|| rho_inference2::openai::CHATGPT_BASE_URL.to_owned());
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
                "rho-agent-host: clone store server unavailable, clones fetch for themselves: {error:#}"
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
            rho_rpc::AuthenticatedIrohListener::bind(db.clone(), rho_rpc::protocol::IROH_ALPN)
                .await?;
        eprintln!("rho-agent-host iroh endpoint: {}", listener.endpoint_id());
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
            state_dir.join("agent2"),
            agent2_base_url,
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
        usage::spawn_claude_quota_recorder(
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
            services.quota.clone(),
        );
    }

    let mut iroh_listener = iroh.map(|(listener, _)| {
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(run_iroh_listener(
            services.clone(),
            listener,
            iroh_auth.clone(),
            stopped,
        ));
        (shutdown, task)
    });
    let resume_path = state_dir.join(RESUME_AFTER_RESTART);
    tokio::spawn(resume_after_restart(services.clone(), resume_path.clone()));
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            result = &mut shutdown => {
                result?;
                services.stopping.store(true, Ordering::Relaxed);
                stop_executions(&services, &resume_path).await;
                if let Some((shutdown, listener)) = iroh_listener.take() {
                    let _ = shutdown.send(());
                    listener.await.context("close iroh listener")?;
                }
                services.pool.flush_agent_usage(None).await;
                return Ok(());
            }
            connection = runtime.server.accept() => {
                let connection = connection?;
                let services = services.clone();
                let iroh_auth = iroh_auth.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve_connection(services, iroh_auth, connection).await {
                        eprintln!("rho-agent-host connection error: {error:#}");
                    }
                });
            }
        }
    }
}

/// Agents that were working when this agent host was told to stop, one id
/// per line, for the next one to wake.
const RESUME_AFTER_RESTART: &str = "resume-after-restart";

/// A stop someone asked for (a deploy, say) is a chosen restart: the agents
/// it interrupted carry on after it. A crash writes nothing, so a crash is
/// still never a reason to send
/// (`DECISION-a-restart-does-not-resume-by-itself`).
async fn record_resume_after_restart(services: &Services, path: &Utf8Path) {
    let agents = services.pool.unsettled().await;
    let text: String = agents.iter().map(|id| id.encoded() + "\n").collect();
    if let Err(error) = std::fs::write(path, text) {
        eprintln!("rho-agent-host: could not record agents to resume in {path}: {error}");
    }
}

/// How long stopping may take: draining the agents (each workset gives
/// up on a request at 50s) and then the workset processes. systemd's default
/// stop timeout is 90s.
const STOP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Drains every agent, so requests in flight end and each log is flushed,
/// then stops every workset process, as idle eviction does, rather than
/// leaving them to die with this one. Under `KillMode=mixed` only this
/// process is signalled, so the workers are still there to drain.
async fn stop_executions(services: &Services, resume_path: &Utf8Path) {
    let stop = async {
        services.pool.drain().await;
        // After the drain: an agent whose request ended in a final answer
        // is done, and only those still at work are woken.
        record_resume_after_restart(services, resume_path).await;
        let stops = services
            .pool
            .executions()
            .await
            .into_iter()
            .map(|process| async move { process.shutdown().await });
        futures::future::join_all(stops).await;
    };
    if tokio::time::timeout(STOP_TIMEOUT, stop).await.is_err() {
        eprintln!("rho-agent-host: still stopping after {STOP_TIMEOUT:?}; exiting anyway");
    }
}

/// Wakes the agents the last agent host recorded on its way down. The record
/// is removed first, so an agent host that dies waking them does not wake
/// them again.
async fn resume_after_restart(services: Arc<Services>, path: Utf8PathBuf) {
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    if let Err(error) = std::fs::remove_file(&path) {
        eprintln!("rho-agent-host: not resuming agents, {path} stays: {error}");
        return;
    }
    for id in text.lines().filter(|line| !line.is_empty()) {
        let Ok(agent_id) = AgentId::from_encoded(id) else {
            eprintln!("rho-agent-host: not resuming unknown agent id {id}");
            continue;
        };
        let command = rho_agents_client::protocol::AgentCommand::Send {
            agent_id,
            content: vec![ContentPart::Text {
                text: "The agent host was stopped on purpose (for example to deploy a new rho) \
                       while you were working, and has started again. Continue your task."
                    .to_owned(),
            }],
            delivery: rho_agent_types::MessageDelivery::Immediate,
        };
        if let Err(error) = agents::handle_agent_command(&services, command).await {
            eprintln!("rho-agent-host: could not resume {id}: {error:#}");
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
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    while let Some(approved) = tokio::select! {
        approved = listener.accept() => approved,
        _ = &mut shutdown => None,
    } {
        let connection = match approved {
            Ok(connection) => connection,
            Err(error) => {
                eprintln!("rho-agent-host iroh authentication error: {error:#}");
                continue;
            }
        };
        let services = services.clone();
        let iroh_auth = iroh_auth.clone();
        tokio::spawn(async move {
            let media = rho_rpc::media::Mux::new(connection.clone());
            let uni = media.clone();
            let uni_task = tokio::spawn(async move { uni.receive_uni().await });
            while let Ok((send, recv)) = connection.accept_bi().await {
                let services = services.clone();
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
                        let desktop_open = (open.protocol == Protocol::Desktop)
                            .then(|| open.unpack::<rho_desktop_client::protocol::Open>())
                            .transpose()?;
                        if let Some(rho_desktop_client::protocol::Open::Wayland {
                            media_id,
                            agent,
                            session,
                        }) = desktop_open
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
                        if matches!(
                            open.protocol,
                            Protocol::Terminal | Protocol::Shell | Protocol::Voice
                        ) {
                            send.set_priority(50)
                                .context("set iroh interactive stream priority")?;
                        }
                        let writer = rho_rpc::Writer::new(send);
                        serve_stream(services, iroh_auth, open, recv, writer).await
                    }
                    .await;
                    if let Err(error) = result {
                        eprintln!("rho-agent-host iroh connection error: {error:#}");
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
    providers: HashMap<u64, mpsc::UnboundedSender<GitProviderFrame>>,
    pending: HashMap<u64, PendingGitTransport>,
}

struct PendingGitTransport {
    response: oneshot::Sender<Result<BoxGitStream, String>>,
    recipients: HashMap<u64, mpsc::UnboundedSender<GitProviderFrame>>,
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
    async fn register(&self, provider: mpsc::UnboundedSender<GitProviderFrame>) {
        let provider_id = self.next_provider_id.fetch_add(1, Ordering::Relaxed);
        let mut state = self.state.lock().await;
        state.providers.retain(|_, provider| !provider.is_closed());
        state.providers.insert(provider_id, provider);
    }

    async fn request(
        &self,
        request: rho_agent_hosts::protocol::GitTransportRequest,
    ) -> anyhow::Result<BoxGitStream> {
        self.request_with_timeout(request, std::time::Duration::from_secs(60))
            .await
    }

    async fn request_with_timeout(
        &self,
        request: rho_agent_hosts::protocol::GitTransportRequest,
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
                    .send(GitProviderFrame::Requested {
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
        recipients: &HashMap<u64, mpsc::UnboundedSender<GitProviderFrame>>,
        except: Option<u64>,
    ) {
        for (&provider_id, provider) in recipients {
            if Some(provider_id) != except {
                let _ = provider.send(GitProviderFrame::Done { request_id });
            }
        }
    }
}

/// Everything the agent host owns that a connection may need: the agent pool,
/// the database, the stores, the locks and the brokers. It is not a
/// registry of agents — the pool is that — but the one bundle a connection
/// is handed so it does not carry a dozen handles of its own.
struct Services {
    pool: Arc<AgentPool>,
    agents2: Arc<agents2::Agents2>,
    db: RhoDb,
    /// Every device's sealed ledger, kept and passed between them.
    ledger: rho_ledger_server::LedgerServer,
    visualizations: rho_visualizations::VisualizationStore,
    inference: Inference,
    /// The database's machine seed, announced in `Ready` so clients can
    /// encode agent IDs.
    machine_seed: u64,
    /// Stateless PR, CI, review, and comment operations.
    pr_monitor: Arc<rho_pr_monitor::PrMonitor>,
    /// Sealed platform secret store used by Octo.
    platform_secrets: PlatformSecrets,
    /// Marked whenever a quota observation lands; every agents session
    /// tells its client the quota again.
    quota: tokio::sync::watch::Sender<()>,
    /// The snapshotted login environment, for terminal shells.
    user_environment: rho_fs_view::UserEnvironment,
    /// The Claude configuration this agent host runs against, resolved in
    /// `run`.
    claude: rho_claude::accounts::ClaudePaths,
    git_transport: GitTransportBroker,
    /// At most one GUI owns the voice session's microphone and playback.
    voice_lease: Arc<TokioMutex<()>>,
    /// Set once this agent host starts stopping: agents are draining, and
    /// none is created or loaded or told anything new.
    stopping: std::sync::atomic::AtomicBool,
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
        agent2_dir: Utf8PathBuf,
        agent2_base_url: String,
    ) -> anyhow::Result<Self> {
        let machine_seed = db.read().machine_seed();
        let agents2 = agents2::Agents2::live(
            agent2_dir,
            agent2_base_url,
            db.read().machine_seed(),
            db.read().last_agent_counter(),
        )?;
        let pr_monitor =
            rho_pr_monitor::PrMonitor::new(pool.clone(), db.clone(), octo_socket).await?;
        let visualizations = rho_visualizations::VisualizationStore::new(db.clone()).await;
        let ledger = rho_ledger_server::LedgerServer::open(db.clone()).await;
        let registry = Self {
            pool,
            agents2,
            db,
            claude,
            ledger,
            visualizations,
            inference,
            machine_seed,
            pr_monitor,
            platform_secrets,
            quota: tokio::sync::watch::channel(()).0,
            user_environment,
            git_transport: GitTransportBroker::default(),
            voice_lease: Arc::new(TokioMutex::new(())),
            stopping: std::sync::atomic::AtomicBool::new(false),
        };
        Ok(registry)
    }

    fn refuse_while_stopping(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.stopping.load(Ordering::Relaxed),
            "the agent host is stopping; try again once it is back"
        );
        Ok(())
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

    /// `mode` is the agent's own: how it sees the filesystem around the
    /// workset, whether that workset is fresh or one it joins.
    async fn create(
        &self,
        role: AgentRole,
        start: StartMode,
        mode: WorksetMode,
    ) -> anyhow::Result<(AgentId, RunningAgent)> {
        self.refuse_while_stopping()?;
        let start = self.resolve_start_place(start, mode).await?;
        let (agent_id, agent) = self.pool.create(role, None, start).await?;
        Ok((agent_id, agent))
    }

    /// Resolve the user's new-on or join choice to the exact workset place
    /// and filesystem view shared by agents and their workset resources.
    async fn resolve_start_place(
        &self,
        start: StartMode,
        mode: WorksetMode,
    ) -> anyhow::Result<rho_agent::StartPlace> {
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
        Ok(start)
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
        self.refuse_while_stopping()?;
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
    writer: W,
) -> anyhow::Result<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    match open.protocol {
        Protocol::Agents => agents::serve(services, open.unpack()?, reader, writer).await,
        Protocol::Agents2 => agents2::serve(services, open.unpack()?, reader, writer).await,
        Protocol::Ledger => anyhow::bail!("the old ledger protocol is no longer supported"),
        Protocol::LedgerLog => services.ledger.serve(reader, writer).await,
        Protocol::Desktop => desktop::serve(services, open.unpack()?, reader, writer).await,
        Protocol::Host => host::serve(services, iroh_auth, open.unpack()?, reader, writer).await,
        Protocol::Shell => agents::serve_shells(services, open.unpack()?, reader, writer).await,
        Protocol::Terminal => {
            agents::serve_terminals(services, open.unpack()?, reader, writer).await
        }
        Protocol::Voice => {
            let rho_rtc::protocol::Open { offer_sdp } = open.unpack()?;
            realtime::serve(services, reader, writer, offer_sdp).await
        }
        Protocol::Workspace => {
            let rho_files::protocol::Open { workspace } = open.unpack()?;
            agents::serve_workspace_channel(services, reader, writer, workspace).await
        }
    }
}

static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

/// An inference state change moves the quota every session shows.
fn spawn_inference_projection(services: Arc<Services>) {
    let mut state = services.inference.subscribe();
    let services = Arc::downgrade(&services);
    tokio::spawn(async move {
        while state.changed().await.is_ok() {
            let Some(services) = services.upgrade() else {
                break;
            };
            let _ = state.borrow_and_update();
            services.quota.send_replace(());
        }
    });
}

/// Repo roots must be absolute (the agent host's cwd is meaningless by design):
/// agents start on host-made clones, so both workdir registration and
/// agent creation take repos. A leading `~` expands
/// to the agent host's home: clients may run on another machine, so path
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
    if encoded_total > rho_rpc::protocol::MAX_FRAME_LEN.saturating_sub(1024 * 1024) {
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
mod host_directory_tests {
    use std::ffi::OsString;

    use super::apply_host_directories;

    /// The environment an agent gets says where this agent host works, not
    /// where a login shell would have gone. A rig agent host's agent
    /// otherwise writes under the rig's HOME by XDG default and reads a
    /// Claude config home nobody named; the capture carries neither, and a
    /// stale value from the login shell has to lose to the agent host's.
    #[test]
    fn the_host_names_the_directories_its_agents_work_in() {
        let root = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(root.path()).unwrap();
        let state_dir = root.join("state").join("rho");
        let claude = rho_claude::accounts::ClaudePaths::at(root.join("config").join("claude"));

        let mut environment: Vec<(OsString, OsString)> = vec![
            ("PATH".into(), "/usr/bin".into()),
            // What a login shell left behind: the user's, not this agent host's.
            ("XDG_STATE_HOME".into(), "/home/someone/.local/state".into()),
        ];
        apply_host_directories(&mut environment, &state_dir, &claude);

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
        // The two the agent host has no say over: passed on as it was started
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
    use std::ffi::{OsStr, OsString};
    use std::os::fd::AsRawFd as _;
    use std::sync::Arc;

    use rho_agent_hosts::protocol::GitProviderFrame;
    use rho_agent_types::ContentPart;

    use super::{
        GitProviderClaim, GitTransportBroker, MAX_IMAGE_BASE64_BYTES, MAX_INPUT_IMAGES,
        PlatformSecrets, configure_octo_git_transport, prepare_image_content,
        start_runtime_sockets, validate_image_content,
    };

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
        assert!(paths.host_lock().exists());
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
    async fn second_host_is_refused_while_first_holds_runtime_lock() {
        let runtime = tempfile::tempdir().unwrap();
        let paths =
            rho_rpc::protocol::RuntimePaths::new(Some(runtime.path().join("rho.sock"))).unwrap();
        let first =
            start_runtime_sockets(Some(paths.socket().to_owned()), PlatformSecrets::default())
                .unwrap();

        let error = match start_runtime_sockets(
            Some(paths.socket().to_owned()),
            PlatformSecrets::default(),
        ) {
            Ok(_) => panic!("second agent host acquired the runtime directory"),
            Err(error) => error,
        };
        let message = format!("{error:#}");

        assert!(
            message.contains(&paths.directory().display().to_string()),
            "{message}"
        );
        assert!(
            message.contains(&paths.host_lock().display().to_string()),
            "{message}"
        );
        drop(first);
    }

    #[tokio::test]
    async fn stale_socket_files_are_removed_and_rebound() {
        let runtime = tempfile::tempdir().unwrap();
        let paths =
            rho_rpc::protocol::RuntimePaths::new(Some(runtime.path().join("rho.sock"))).unwrap();
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
            rho_rpc::protocol::RuntimePaths::new(Some(runtime.path().join("rho.sock"))).unwrap();
        let sockets =
            start_runtime_sockets(Some(paths.socket().to_owned()), PlatformSecrets::default())
                .unwrap();
        let contender = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(paths.host_lock())
            .unwrap();

        let result = unsafe { libc::flock(contender.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(result, -1);
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(sockets);
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
        let request = rho_agent_hosts::protocol::GitTransportRequest {
            host: "git.example".to_owned(),
            port: 22,
            user: "git".to_owned(),
            repository: "team/repo.git".to_owned(),
            service: rho_agent_hosts::protocol::GitService::ReceivePack,
            planned_refs: Some(vec!["refs/heads/main".to_owned()]),
        };
        let waiting = {
            let broker = broker.clone();
            tokio::spawn(async move { broker.request(request).await })
        };
        let (request_id, first_provider) = match first_rx.recv().await.unwrap() {
            GitProviderFrame::Requested {
                request_id,
                provider_id,
                ..
            } => (request_id, provider_id),
            message => panic!("unexpected provider message: {message:?}"),
        };
        let second_provider = match second_rx.recv().await.unwrap() {
            GitProviderFrame::Requested {
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
            Some(GitProviderFrame::Done {
                request_id: done_request
            }) if done_request == request_id
        ));
    }

    #[tokio::test]
    async fn git_transport_broker_rejects_without_registered_clients() {
        let result = GitTransportBroker::default()
            .request(rho_agent_hosts::protocol::GitTransportRequest {
                host: "git.example".to_owned(),
                port: 22,
                user: "git".to_owned(),
                repository: "team/repo.git".to_owned(),
                service: rho_agent_hosts::protocol::GitService::UploadPack,
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
                        rho_agent_hosts::protocol::GitTransportRequest {
                            host: "git.example".to_owned(),
                            port: 22,
                            user: "git".to_owned(),
                            repository: "team/repo.git".to_owned(),
                            service: rho_agent_hosts::protocol::GitService::UploadPack,
                            planned_refs: None,
                        },
                        std::time::Duration::from_millis(10),
                    )
                    .await
            })
        };
        let request_id = match provider_rx.recv().await.unwrap() {
            GitProviderFrame::Requested { request_id, .. } => request_id,
            message => panic!("unexpected provider message: {message:?}"),
        };
        let error = match waiting.await.unwrap() {
            Ok(_) => panic!("request unexpectedly received a provider"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("within 60 seconds"));
        assert!(matches!(
            provider_rx.recv().await,
            Some(GitProviderFrame::Done {
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
        anyhow::bail!("agent desktops require a Linux agent host");
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
