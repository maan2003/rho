//! Daemon-owned workset storage.
//!
//! A workset is one plain directory an agent uses as its working place,
//! presented at `/src` inside the agent's namespace. The daemon does not
//! interpret its contents: the agent clones what it needs with ordinary
//! `jj git clone`, which is instant because every clone is served from the
//! daemon's clone-store root (`CLONES.md`). The store root is owned by a
//! `jj store serve` process the daemon runs; agents and the daemon alike
//! reach it through one unix socket and never write a store themselves.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path};
use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use tokio::sync::Mutex;

mod diff;
mod ns;

pub mod layout;

pub use layout::*;
pub use ns::{ClaudeHome, MAX_BOUNDED_READ, Mode, Namespace};
pub use rho_workspaces_types::{
    WorksetMode, WorkspaceDiffBaseContent, WorkspaceDiffContent, WorkspaceDiffFile,
    WorkspaceDiffSnapshot, WorkspaceDiffStatus, WorkspaceDiffTarget, WorkspaceInfo,
};

/// Establishes the identity user namespace required before workset mount
/// namespaces are created from runtime worker threads.
///
/// # Safety
/// The caller must invoke this before starting any threads.
pub unsafe fn init_daemon_namespace() -> anyhow::Result<()> {
    layout::unshare_identity_user_namespace()
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathOverrides {
    pub before: Vec<std::path::PathBuf>,
    pub after: Vec<std::path::PathBuf>,
}

impl PathOverrides {
    pub fn add_to(&self, from_env: &OsStr) -> OsString {
        let mut path = OsString::new();
        for entry in self
            .before
            .iter()
            .cloned()
            .chain(std::env::split_paths(from_env))
            .chain(self.after.iter().cloned())
        {
            if !path.is_empty() {
                path.push(if cfg!(windows) { ";" } else { ":" });
            }
            path.push(entry);
        }
        path
    }
}

/// Environment explicitly supplied to subprocesses owned by the daemon.
#[derive(Clone, Debug, Default)]
pub struct UserEnvironment(Arc<[(OsString, OsString)]>);

impl UserEnvironment {
    pub fn new(values: Vec<(OsString, OsString)>) -> Self {
        Self(values.into())
    }

    pub fn apply(&self, command: &mut tokio::process::Command) {
        let overrides = command
            .as_std()
            .get_envs()
            .map(|(name, value)| (name.to_owned(), value.map(OsStr::to_owned)))
            .collect::<Vec<_>>();
        command.env_clear();
        command.envs(self.0.iter().map(|(name, value)| (name, value)));
        for (name, value) in overrides {
            match value {
                Some(value) => {
                    command.env(name, value);
                }
                None => {
                    command.env_remove(name);
                }
            }
        }
    }

    pub fn get(&self, name: &str) -> Option<&OsStr> {
        self.0
            .iter()
            .find_map(|(key, value)| (key == name).then_some(value.as_os_str()))
    }
}

/// How the store server keeps stores fresh: every store is refetched each
/// `interval`, and a store fetched within `debounce` is served as is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreRefresh {
    pub interval: Duration,
    pub debounce: Duration,
}

impl Default for StoreRefresh {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60),
            debounce: Duration::from_secs(30),
        }
    }
}

/// Whether a state root runs the store server. Without one, jj clients
/// initialize and fetch stores themselves (`JJ_STORE` only) and nothing
/// keeps them fresh in the background.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreService {
    Serve(StoreRefresh),
    None,
}

/// The daemon-wide owner of one state root: the clone-store root, its
/// server, and every workset directory.
#[derive(Debug)]
pub struct Worksets {
    root: Utf8PathBuf,
    environment: UserEnvironment,
    path_overrides: PathOverrides,
    server: Mutex<Option<tokio::process::Child>>,
    worksets: Mutex<BTreeMap<String, Weak<WorksetInner>>>,
    /// Host directories adopted as worksets for this process's lifetime.
    adopted: std::sync::Mutex<BTreeMap<String, Utf8PathBuf>>,
}

impl Worksets {
    /// Opens (creating if needed) the state root and, when asked, starts
    /// its store server, returning once the server accepts connections.
    pub async fn open(
        root: impl AsRef<Path>,
        environment: UserEnvironment,
        path_overrides: PathOverrides,
        service: StoreService,
    ) -> anyhow::Result<Arc<Self>> {
        let root = absolute_utf8(root.as_ref())?;
        std::fs::create_dir_all(root.join("stores"))
            .with_context(|| format!("create clone-store root at {root}"))?;
        std::fs::create_dir_all(root.join("worksets"))
            .with_context(|| format!("create workset root at {root}"))?;
        let server = match service {
            StoreService::Serve(refresh) => {
                Some(spawn_store_server(&root, &environment, &path_overrides, refresh).await?)
            }
            StoreService::None => None,
        };
        Ok(Arc::new(Self {
            root,
            environment,
            path_overrides,
            server: Mutex::new(server),
            worksets: Mutex::new(BTreeMap::new()),
            adopted: std::sync::Mutex::new(BTreeMap::new()),
        }))
    }

    /// The default state root: `$XDG_STATE_HOME/rho` (`~/.local/state/rho`).
    pub fn default_root() -> anyhow::Result<std::path::PathBuf> {
        let state = dirs::state_dir()
            .or_else(|| dirs::home_dir().map(|home| home.join(".local/state")))
            .context("state directory is unavailable")?;
        Ok(state.join("rho"))
    }

    /// Opens the default root with a store server.
    pub async fn open_default(
        environment: UserEnvironment,
        path_overrides: PathOverrides,
    ) -> anyhow::Result<Arc<Self>> {
        Self::open(
            Self::default_root()?,
            environment,
            path_overrides,
            StoreService::Serve(StoreRefresh::default()),
        )
        .await
    }

    pub fn root(&self) -> &Utf8Path {
        &self.root
    }

    /// The clone-store root (`git.clone-store`); read-only for everyone but
    /// the server.
    pub fn store_root(&self) -> Utf8PathBuf {
        self.root.join("stores")
    }

    /// The store server's socket (`git.clone-store-socket`), when one runs.
    pub fn store_socket(&self) -> Option<Utf8PathBuf> {
        self.server
            .try_lock()
            .map(|server| server.is_some())
            .unwrap_or(true)
            .then(|| self.root.join("store.sock"))
    }

    /// Environment that points jj at the store root and its server.
    pub fn store_environment(&self) -> Vec<(OsString, OsString)> {
        let mut environment = vec![("JJ_STORE".into(), self.store_root().into_os_string())];
        if let Some(socket) = self.store_socket() {
            environment.push(("JJ_STORE_SOCKET".into(), socket.into_os_string()));
        }
        environment
    }

    /// Whether the store server is still running.
    pub async fn server_alive(&self) -> bool {
        matches!(
            self.server
                .lock()
                .await
                .as_mut()
                .map(|server| server.try_wait()),
            Some(Ok(None))
        )
    }

    /// Adopts an existing host directory as a workset for the lifetime of
    /// this process: for evaluations, renderings and tests that work on a
    /// directory the user already has. Nothing is written to the state root.
    pub fn adopt(self: &Arc<Self>, directory: impl AsRef<Path>) -> anyhow::Result<Workset> {
        let root = absolute_utf8(directory.as_ref())?;
        anyhow::ensure!(root.is_dir(), "not a directory: {root}");
        let id = format!("adopted-{}", random_workset_id()?);
        self.adopted
            .lock()
            .unwrap()
            .insert(id.clone(), root.clone());
        let workset = Arc::new(WorksetInner {
            id: id.clone(),
            root,
            owner: Arc::downgrade(self),
            operation_lock: Mutex::new(()),
        });
        // Not memoized weakly: an adopted workset is looked up by its
        // recorded directory instead.
        Ok(Workset(workset))
    }

    pub async fn create(self: &Arc<Self>) -> anyhow::Result<Workset> {
        for _ in 0..64 {
            let workset_id = random_workset_id()?;
            let base = self.root.join("worksets").join(&workset_id);
            match std::fs::create_dir(&base) {
                Ok(()) => {
                    std::fs::create_dir(base.join("src"))
                        .with_context(|| format!("create workset {workset_id}"))?;
                    return self.open_workset(&workset_id).await;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("allocate workset directory"),
            }
        }
        anyhow::bail!("could not allocate a unique workset id")
    }

    pub async fn open_workset(self: &Arc<Self>, workset_id: &str) -> anyhow::Result<Workset> {
        validate_name(workset_id)?;
        let mut worksets = self.worksets.lock().await;
        if let Some(workset) = worksets.get(workset_id).and_then(Weak::upgrade) {
            return Ok(Workset(workset));
        }
        let adopted = self.adopted.lock().unwrap().get(workset_id).cloned();
        let root = match adopted {
            Some(root) => root,
            None => self.root.join("worksets").join(workset_id).join("src"),
        };
        anyhow::ensure!(root.is_dir(), "workset does not exist: {workset_id}");
        let workset = Arc::new(WorksetInner {
            id: workset_id.to_owned(),
            root,
            owner: Arc::downgrade(self),
            operation_lock: Mutex::new(()),
        });
        worksets.insert(workset_id.to_owned(), Arc::downgrade(&workset));
        Ok(Workset(workset))
    }

    /// Ids of every workset directory under the root.
    pub fn list(&self) -> anyhow::Result<Vec<String>> {
        let dir = self.root.join("worksets");
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(&dir).with_context(|| format!("list worksets in {dir}"))? {
            let entry = entry?;
            if entry.path().join("src").is_dir()
                && let Ok(name) = entry.file_name().into_string()
            {
                ids.push(name);
            }
        }
        ids.sort();
        Ok(ids)
    }

    /// Removes a workset directory and everything the agent put in it.
    /// Stores are untouched: clones only borrow from them. Idempotent.
    pub async fn discard_workset(self: &Arc<Self>, workset_id: &str) -> anyhow::Result<()> {
        validate_name(workset_id)?;
        let live = self
            .worksets
            .lock()
            .await
            .remove(workset_id)
            .and_then(|workset| workset.upgrade());
        let _guard = match &live {
            Some(workset) => Some(workset.operation_lock.lock().await),
            None => None,
        };
        let base = self.root.join("worksets").join(workset_id);
        match std::fs::remove_dir_all(&base) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("remove workset {base}")),
        }
    }

    /// A daemon-side command with the user's environment, the jj override,
    /// and the store server wired in.
    pub fn command(&self, program: &str) -> tokio::process::Command {
        let mut command = base_command(program, &self.environment, &self.path_overrides);
        command.envs(self.store_environment());
        command
    }
}

fn base_command(
    program: &str,
    environment: &UserEnvironment,
    path_overrides: &PathOverrides,
) -> tokio::process::Command {
    let executable = if program == "jj" {
        environment
            .get("RHO_JJ")
            .unwrap_or_else(|| OsStr::new(program))
    } else {
        OsStr::new(program)
    };
    let mut command = tokio::process::Command::new(executable);
    if program == "jj" {
        command.args(["--config", "git.write-change-id-header=true"]);
    }
    environment.apply(&mut command);
    if let Some(path) = environment.get("PATH") {
        command.env("PATH", path_overrides.add_to(path));
    }
    command
}

async fn spawn_store_server(
    root: &Utf8Path,
    environment: &UserEnvironment,
    path_overrides: &PathOverrides,
    refresh: StoreRefresh,
) -> anyhow::Result<tokio::process::Child> {
    let socket = root.join("store.sock");
    let mut command = base_command("jj", environment, path_overrides);
    command
        .arg("--config")
        .arg(format!(
            "git.clone-store={}",
            toml_string(root.join("stores").as_str())
        ))
        .args(["store", "serve", "--socket"])
        .arg(&socket)
        .arg("--interval")
        .arg(refresh.interval.as_secs().to_string())
        .arg("--debounce")
        .arg(refresh.debounce.as_secs().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().context("start clone store server")?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if tokio::net::UnixStream::connect(&socket).await.is_ok() {
            // The server's stderr is its log; keep it from blocking on a
            // full pipe now that startup is over.
            if let Some(stderr) = child.stderr.take() {
                tokio::spawn(async move {
                    use tokio::io::AsyncBufReadExt as _;
                    let mut lines = tokio::io::BufReader::new(stderr).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        eprintln!("store server: {line}");
                    }
                });
            }
            return Ok(child);
        }
        if let Some(status) = child.try_wait()? {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                use tokio::io::AsyncReadExt as _;
                let _ = pipe.read_to_string(&mut stderr).await;
            }
            anyhow::bail!(
                "clone store server exited during startup ({status}): {}",
                stderr.trim()
            );
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "clone store server did not open {socket}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn toml_string(value: &str) -> String {
    format!("{value:?}")
}

/// A handle to one workset's host-frame `src` directory.
#[derive(Clone, Debug)]
pub struct Workset(Arc<WorksetInner>);

#[derive(Debug)]
struct WorksetInner {
    id: String,
    root: Utf8PathBuf,
    owner: Weak<Worksets>,
    operation_lock: Mutex<()>,
}

impl Workset {
    pub fn id(&self) -> &str {
        &self.0.id
    }

    /// Host path of the directory presented at `/src`.
    pub fn root(&self) -> &Utf8Path {
        &self.0.root
    }

    pub(crate) fn owner(&self) -> anyhow::Result<Arc<Worksets>> {
        self.0
            .owner
            .upgrade()
            .context("worksets manager was dropped")
    }

    /// A namespace over this workset for one agent, whose working
    /// directory is `cwd` as the agent sees it (below the mode's visible
    /// root, or relative to it). The mount namespace itself is built on the
    /// first command.
    pub fn enter(&self, mode: Mode, cwd: &Utf8Path) -> anyhow::Result<Arc<Namespace>> {
        Namespace::new(self.clone(), mode, cwd)
    }

    /// Starts a new change atop `revset` in the repository at `checkout`
    /// (a host directory inside the workset).
    pub async fn new_change(&self, checkout: &Utf8Path, revset: &str) -> anyhow::Result<()> {
        let owner = self.owner()?;
        let _guard = self.0.operation_lock.lock().await;
        let mut command = owner.command("jj");
        command.current_dir(checkout).args(["new", revset]);
        run(command, "start change at requested revset").await
    }

    /// Snapshots the jj workspace containing `checkout` (a host directory
    /// inside the workset) and reads its working-copy commit against the
    /// merged parent tree through `jj-lib`. `None` when the commit is still
    /// `known_commit_id`.
    pub async fn diff_snapshot(
        &self,
        checkout: &Utf8Path,
        known_commit_id: Option<&str>,
        include_paths: &[Utf8PathBuf],
    ) -> anyhow::Result<Option<WorkspaceDiffSnapshot>> {
        anyhow::ensure!(
            include_paths.len() <= 2_048,
            "too many live diff paths: {}",
            include_paths.len()
        );
        anyhow::ensure!(
            include_paths
                .iter()
                .try_fold(0_usize, |total, path| total
                    .checked_add(path.as_str().len()))
                .is_some_and(|total| total <= 1024 * 1024),
            "live diff paths exceed the 1 MiB path budget"
        );
        let (checkout, is_jj) = resolve_workdir_root(checkout.as_std_path())?;
        anyhow::ensure!(is_jj, "diff view requires a jj repository: {checkout}");
        static DIFF_READERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
        let permit = DIFF_READERS
            .acquire()
            .await
            .context("diff readers closed")?;
        let known_commit_id = known_commit_id.map(str::to_owned);
        let include_paths = include_paths.to_vec();
        let environment = self.jj_environment()?;
        let inner = Arc::clone(&self.0);
        tokio::task::spawn_blocking(move || {
            // jj-lib's repository/index graph is intentionally !Send. Keep
            // every jj value and future on this one blocking worker; only the
            // fully-owned wire DTO crosses back to Tokio.
            let _permit = permit;
            let _guard = inner.operation_lock.blocking_lock();
            futures::executor::block_on(async {
                let epoch = jj_cli::cli_util::snapshot_workspace_descendants_at_with_environment(
                    checkout.as_std_path(),
                    environment,
                )
                .await
                .map_err(|error| anyhow::anyhow!(error.error.to_string()))?;
                let captured = diff::capture(epoch).await?;
                if known_commit_id.as_deref() == Some(captured.commit_id_hex().as_str()) {
                    return Ok(None);
                }
                diff::load(captured, &include_paths).await.map(Some)
            })
        })
        .await
        .context("jj diff reader panicked")?
    }

    /// Materializes bounded parent-side content from a previously returned
    /// immutable jj operation. This never snapshots the live working copy.
    pub async fn diff_base_contents(
        &self,
        checkout: &Utf8Path,
        operation_id: &str,
        commit_id: &str,
        paths: &[Utf8PathBuf],
    ) -> anyhow::Result<Vec<WorkspaceDiffBaseContent>> {
        anyhow::ensure!(
            paths.len() <= 64,
            "too many deferred diff paths: {}",
            paths.len()
        );
        anyhow::ensure!(
            paths
                .iter()
                .try_fold(0_usize, |total, path| total
                    .checked_add(path.as_str().len()))
                .is_some_and(|total| total <= 1024 * 1024),
            "deferred diff paths exceed the 1 MiB path budget"
        );
        let (checkout, is_jj) = resolve_workdir_root(checkout.as_std_path())?;
        anyhow::ensure!(is_jj, "diff view requires a jj repository: {checkout}");
        static DIFF_READERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
        let permit = DIFF_READERS
            .acquire()
            .await
            .context("diff readers closed")?;
        let operation_id = operation_id.to_owned();
        let commit_id = commit_id.to_owned();
        let paths = paths.to_vec();
        let environment = self.jj_environment()?;
        let inner = Arc::clone(&self.0);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _guard = inner.operation_lock.blocking_lock();
            futures::executor::block_on(async {
                let epoch = jj_cli::cli_util::workspace_snapshot_at_operation_with_environment(
                    checkout.as_std_path(),
                    &operation_id,
                    environment,
                )
                .await
                .map_err(|error| anyhow::anyhow!(error.error.to_string()))?;
                let captured = diff::capture(epoch).await?;
                anyhow::ensure!(
                    captured.commit_id_hex() == commit_id,
                    "diff snapshot revision is no longer available"
                );
                diff::load_base_contents(captured, &paths).await
            })
        })
        .await
        .context("jj deferred diff reader panicked")?
    }

    /// The environment in-process jj readers see: the user's, plus the
    /// store variables.
    fn jj_environment(&self) -> anyhow::Result<Vec<(OsString, OsString)>> {
        let owner = self.owner()?;
        let mut environment = owner.environment.0.iter().cloned().collect::<Vec<_>>();
        environment.extend(owner.store_environment());
        Ok(environment)
    }

    /// Clones `remote_url` into `<root>/<name>` through the store server,
    /// exactly as the agent would with `jj git clone`. `name` defaults to
    /// the repository name in the URL. An existing clone of that name is
    /// returned as is.
    pub async fn clone_repo(
        &self,
        remote_url: &str,
        name: Option<&str>,
    ) -> anyhow::Result<Utf8PathBuf> {
        let name = match name {
            Some(name) => name.to_owned(),
            None => repo_name(remote_url)?,
        };
        validate_name(&name)?;
        let owner = self.owner()?;
        let _guard = self.0.operation_lock.lock().await;
        let target = self.0.root.join(&name);
        if target.join(".jj").is_dir() {
            return Ok(target);
        }
        anyhow::ensure!(
            !target.exists(),
            "{target} exists and is not a jj repository"
        );
        let mut command = owner.command("jj");
        command
            .current_dir(&self.0.root)
            .args(["git", "clone", "--", remote_url, &name]);
        if let Err(error) = run(command, "clone repository").await {
            let _ = std::fs::remove_dir_all(&target);
            return Err(error);
        }
        Ok(target)
    }

    /// Names of the top-level jj repositories in the workset.
    pub fn repos(&self) -> anyhow::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.0.root)
            .with_context(|| format!("list workset {}", self.0.root))?
        {
            let entry = entry?;
            if entry.path().join(".jj").is_dir()
                && let Ok(name) = entry.file_name().into_string()
            {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }

    /// Maps a path as the agent sees it (absolute below `/src`, or relative
    /// to it) onto the host directory, refusing `.` and `..` components.
    pub fn host_path(&self, visible: &Utf8Path) -> anyhow::Result<Utf8PathBuf> {
        let relative = visible_relative(layout::MOUNT_ROOT, visible)?;
        Ok(self.0.root.join(relative))
    }

    /// Records pending working-copy changes in every top-level repository.
    pub async fn snapshot(&self) -> anyhow::Result<()> {
        let owner = self.owner()?;
        let _guard = self.0.operation_lock.lock().await;
        for name in self.repos()? {
            let mut command = owner.command("jj");
            command
                .current_dir(self.0.root.join(&name))
                .args(["util", "snapshot"]);
            run(command, &format!("snapshot {name}")).await?;
        }
        Ok(())
    }
}

/// The path below `visible_root` that `path` denotes; a relative `path` is
/// taken relative to the root.
pub(crate) fn visible_relative<'a>(
    visible_root: &str,
    path: &'a Utf8Path,
) -> anyhow::Result<&'a Utf8Path> {
    anyhow::ensure!(
        !path.components().any(|component| matches!(
            component,
            camino::Utf8Component::CurDir | camino::Utf8Component::ParentDir
        )),
        "path must not contain . or .. components: {path}"
    );
    if path.is_absolute() {
        path.strip_prefix(visible_root)
            .with_context(|| format!("path is outside {visible_root}: {path}"))
    } else {
        Ok(path)
    }
}

/// The repository name jj would derive for a clone of `url`.
fn repo_name(url: &str) -> anyhow::Result<String> {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let name = trimmed
        .rsplit(['/', ':'])
        .find(|part| !part.is_empty())
        .context("remote URL has no repository name")?;
    Ok(name.to_owned())
}

/// The root of the jj repository whose workspace `path` is (following a
/// secondary workspace's pointer to its origin).
pub fn resolve_repo_root(path: &Path) -> anyhow::Result<Utf8PathBuf> {
    anyhow::ensure!(
        path.is_absolute(),
        "repo path must be absolute: {}",
        path.display()
    );
    let path = path
        .canonicalize()
        .with_context(|| format!("repo does not exist: {}", path.display()))?;
    let path = Utf8PathBuf::try_from(path).context("repo path is not valid UTF-8")?;
    anyhow::ensure!(
        path.join(".jj").is_dir(),
        "not a jj repository root: {path}"
    );
    let pointer = path.join(".jj").join("repo");
    if pointer.is_file() {
        // A secondary workspace: the pointer names `<origin>/.jj/repo`.
        let target = Utf8PathBuf::from(
            std::fs::read_to_string(&pointer)
                .with_context(|| format!("read {pointer}"))?
                .trim(),
        );
        let target = if target.is_absolute() {
            target
        } else {
            path.join(".jj").join(target)
        };
        let origin = target
            .parent()
            .and_then(Utf8Path::parent)
            .with_context(|| format!("malformed repo pointer in {pointer}"))?
            .to_owned();
        anyhow::ensure!(
            origin.join(".jj").is_dir(),
            "workspace points at a missing repo: {origin}",
        );
        let origin = origin
            .canonicalize_utf8()
            .with_context(|| format!("canonicalize repo root {origin}"))?;
        return Ok(origin);
    }
    Ok(path)
}

/// Walks up from `path` to the containing jj workspace root when there is
/// one, otherwise canonicalizes the plain directory. Returns the root and
/// whether it is a jj workspace.
pub fn resolve_workdir_root(path: &Path) -> anyhow::Result<(Utf8PathBuf, bool)> {
    anyhow::ensure!(
        path.is_absolute(),
        "workdir path must be absolute: {}",
        path.display()
    );
    let canonical = path
        .canonicalize()
        .with_context(|| format!("workdir does not exist: {}", path.display()))?;
    let canonical = Utf8PathBuf::try_from(canonical).context("workdir path is not valid UTF-8")?;
    anyhow::ensure!(
        canonical.is_dir(),
        "workdir is not a directory: {canonical}"
    );
    let mut cursor: &Utf8Path = &canonical;
    loop {
        if cursor.join(".jj").is_dir() {
            return Ok((cursor.to_owned(), true));
        }
        match cursor.parent() {
            Some(parent) => cursor = parent,
            None => return Ok((canonical, false)),
        }
    }
}

fn validate_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!name.is_empty(), "name is empty");
    anyhow::ensure!(
        Path::new(name)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
            && !name.contains('/'),
        "name is not one path component: {name}"
    );
    anyhow::ensure!(
        !name.starts_with('.'),
        "name may not start with '.': {name}"
    );
    Ok(())
}

fn random_workset_id() -> anyhow::Result<String> {
    let mut bytes = [0_u8; 6];
    let read = unsafe { libc::syscall(libc::SYS_getrandom, bytes.as_mut_ptr(), bytes.len(), 0) };
    if read != bytes.len() as libc::c_long {
        return Err(std::io::Error::last_os_error()).context("generate workset id");
    }
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn absolute_utf8(path: &Path) -> anyhow::Result<Utf8PathBuf> {
    let path = std::path::absolute(path)
        .with_context(|| format!("make path absolute: {}", path.display()))?;
    Utf8PathBuf::try_from(path).context("path is not valid UTF-8")
}

async fn run(mut command: tokio::process::Command, action: &str) -> anyhow::Result<()> {
    let result = command
        .output()
        .await
        .with_context(|| format!("spawn {action}"))?;
    anyhow::ensure!(
        result.status.success(),
        "{action} failed: {}",
        String::from_utf8_lossy(&result.stderr).trim()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_names_follow_the_url() {
        assert_eq!(
            repo_name("https://github.com/org/repo.git").unwrap(),
            "repo"
        );
        assert_eq!(repo_name("git@github.com:org/repo").unwrap(), "repo");
        assert_eq!(repo_name("/tmp/remote.git/").unwrap(), "remote");
        assert!(repo_name("").is_err());
    }

    #[test]
    fn visible_paths_stay_below_the_root() {
        assert_eq!(
            visible_relative("/src", Utf8Path::new("/src/a/b")).unwrap(),
            "a/b"
        );
        assert_eq!(visible_relative("/src", Utf8Path::new("a")).unwrap(), "a");
        assert!(visible_relative("/src", Utf8Path::new("/src/../x")).is_err());
        assert!(visible_relative("/src", Utf8Path::new("/srcx/a")).is_err());
        assert!(visible_relative("/src", Utf8Path::new("./a")).is_err());
    }
}
