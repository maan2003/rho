//! Daemon-owned workset storage.
//!
//! A workset is one plain directory an agent uses as its working place,
//! presented at `/src` inside the agent's namespace. The daemon does not
//! interpret its contents: the agent clones what it needs with ordinary
//! `git clone`, which is instant because every clone is born from the
//! daemon's mirror store (`CLONES.md`). The store root is owned by the
//! keeper (`rho-git-server`) running inside the daemon; the `git` agents
//! see is Rho's patched git, which asks the keeper itself on every fetch
//! and clone, and the daemon's own clones go through the same keeper
//! in-process. Nothing but the keeper writes a mirror.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Weak};

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use rho_git_server::MirrorStore;
use tokio::sync::Mutex;

mod ns;

pub mod layout;

pub use layout::*;
pub use ns::{ClaudeHome, MAX_BOUNDED_READ, Mode, Namespace};
pub use rho_git_proto::{SOCKET_ENV, repo_name};

/// The agent's base userland (`VIEW.md`): a nix `buildEnv` fixed at build
/// time whose `bin/` is the agent's PATH, after the agent's own nix
/// profile. It holds Rho's patched git, the CA bundle and the pinned
/// flake registry.
pub const AGENT_BASE: &str = env!(
    "RHO_AGENT_BASE",
    "RHO_AGENT_BASE must name the agent base (flake.nix agentBase) at build time"
);

/// Rho's patched git (`nix/patches/git-rho-store.patch`): the keeper
/// fetches with it, the daemon clones with it, and agents see it as `git`.
/// Without the store socket in its environment it is plain git.
pub const GIT: &str = concat!(env!("RHO_AGENT_BASE"), "/bin/git");

/// The agent's home inside the view.
pub const AGENT_HOME: &str = "/home/agent";

/// The directory Rho's git really lives in, for exposed mode, whose PATH
/// is the user's own with this first.
pub fn git_dir() -> PathBuf {
    std::fs::canonicalize(GIT)
        .ok()
        .and_then(|git| git.parent().map(Path::to_owned))
        .unwrap_or_else(|| Path::new(AGENT_BASE).join("bin"))
}
pub use rho_git_server::Refresh as StoreRefresh;
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

/// Whether a state root runs the mirror keeper. Without one, the `git`
/// agents see is the plain one: clones and fetches go to the network and
/// nothing is shared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreService {
    Serve(StoreRefresh),
    None,
}

/// The daemon-wide owner of one state root: the mirror store, its keeper,
/// and every workset directory.
#[derive(Debug)]
pub struct Worksets {
    root: Utf8PathBuf,
    environment: UserEnvironment,
    path_overrides: PathOverrides,
    store: Option<StoreHandle>,
    /// `GIT_AUTHOR_*` and `GIT_COMMITTER_*` for agents: the user's, from
    /// their environment or their git config. Empty when unknown.
    identity: Vec<(OsString, OsString)>,
    worksets: Mutex<BTreeMap<String, Weak<WorksetInner>>>,
    /// Host directories adopted as worksets for this process's lifetime.
    adopted: std::sync::Mutex<BTreeMap<String, Utf8PathBuf>>,
}

/// The running keeper and its socket.
#[derive(Debug)]
struct StoreHandle {
    keeper: Arc<MirrorStore>,
    socket: Utf8PathBuf,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for StoreHandle {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Worksets {
    /// Opens (creating if needed) the state root and, when asked, starts
    /// the mirror keeper on its socket.
    pub async fn open(
        root: impl AsRef<Path>,
        environment: UserEnvironment,
        path_overrides: PathOverrides,
        service: StoreService,
    ) -> anyhow::Result<Arc<Self>> {
        let root = absolute_utf8(root.as_ref())?;
        std::fs::create_dir_all(root.join("stores"))
            .with_context(|| format!("create mirror store root at {root}"))?;
        std::fs::create_dir_all(root.join("worksets"))
            .with_context(|| format!("create workset root at {root}"))?;
        std::fs::create_dir_all(root.join("cache"))
            .with_context(|| format!("create shared cache at {root}"))?;
        let store = match service {
            StoreService::Serve(refresh) => {
                Some(start_store(&root, &environment, &path_overrides, refresh)?)
            }
            StoreService::None => None,
        };
        let identity = git_identity(&environment, &path_overrides).await;
        Ok(Arc::new(Self {
            root,
            environment,
            path_overrides,
            store,
            identity,
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

    /// Opens the default root with the keeper running.
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

    /// The mirror store root; read-only for everyone but the keeper.
    pub fn store_root(&self) -> Utf8PathBuf {
        self.root.join("stores")
    }

    /// The cache every agent shares as `~/.cache` (VIEW.md): nix
    /// evaluation and fetcher caches, cargo, uv, npm. Persistent.
    pub fn cache_dir(&self) -> Utf8PathBuf {
        self.root.join("cache")
    }

    /// The user's git identity as `GIT_AUTHOR_*`/`GIT_COMMITTER_*`, when
    /// known.
    pub fn identity_environment(&self) -> &[(OsString, OsString)] {
        &self.identity
    }

    /// The keeper's socket, when one runs.
    pub fn store_socket(&self) -> Option<Utf8PathBuf> {
        self.store.as_ref().map(|store| store.socket.clone())
    }

    /// Environment that points the agent's git at the keeper.
    pub fn store_environment(&self) -> Vec<(OsString, OsString)> {
        match &self.store {
            Some(store) => vec![(SOCKET_ENV.into(), store.socket.clone().into_os_string())],
            None => Vec::new(),
        }
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
    /// Mirrors are untouched: clones only borrow from them. Idempotent.
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

    /// A daemon-side command with the user's environment and the store
    /// wired in.
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
    let mut command = tokio::process::Command::new(program);
    environment.apply(&mut command);
    if let Some(path) = environment.get("PATH") {
        command.env("PATH", path_overrides.add_to(path));
    }
    command
}

/// The user's name and email for commits: `GIT_AUTHOR_*` from their
/// environment when set, else their git config, read with Rho's git in the
/// user's environment. Warns and leaves agents without an identity when
/// neither says.
async fn git_identity(
    environment: &UserEnvironment,
    path_overrides: &PathOverrides,
) -> Vec<(OsString, OsString)> {
    let mut identity = Vec::new();
    for (author, committer, key) in [
        ("GIT_AUTHOR_NAME", "GIT_COMMITTER_NAME", "user.name"),
        ("GIT_AUTHOR_EMAIL", "GIT_COMMITTER_EMAIL", "user.email"),
    ] {
        let value = match environment.get(author) {
            Some(value) => Some(value.to_owned()),
            None => {
                let mut command = base_command(GIT, environment, path_overrides);
                command.args(["config", "--get", key]);
                match command.output().await {
                    Ok(output) if output.status.success() => {
                        let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                        (!value.is_empty()).then(|| OsString::from(value))
                    }
                    _ => None,
                }
            }
        };
        match value {
            Some(value) => {
                identity.push((author.into(), value.clone()));
                identity.push((committer.into(), value));
            }
            None => {
                eprintln!("git identity: {key} is not set; agents' commits will lack it");
            }
        }
    }
    identity
}

/// The environment the keeper runs git with: the user's, with the PATH
/// overrides applied and no store socket, so a patched git never asks the
/// keeper for the mirror it is fetching.
fn keeper_environment(
    environment: &UserEnvironment,
    path_overrides: &PathOverrides,
) -> Vec<(OsString, OsString)> {
    environment
        .0
        .iter()
        .filter(|(name, _)| name != SOCKET_ENV)
        .map(|(name, value)| {
            if name == "PATH" {
                (name.clone(), path_overrides.add_to(value))
            } else {
                (name.clone(), value.clone())
            }
        })
        .collect()
}

/// Starts the keeper: binds the socket and serves it.
fn start_store(
    root: &Utf8Path,
    environment: &UserEnvironment,
    path_overrides: &PathOverrides,
    refresh: StoreRefresh,
) -> anyhow::Result<StoreHandle> {
    let keeper = MirrorStore::new(
        root.join("stores").into_std_path_buf(),
        PathBuf::from(GIT),
        keeper_environment(environment, path_overrides),
        refresh,
    );
    let socket = root.join("store.sock");
    let listener = MirrorStore::bind(socket.as_std_path())
        .with_context(|| format!("bind mirror store socket {socket}"))?;
    let serve = tokio::spawn({
        let keeper = Arc::clone(&keeper);
        async move {
            if let Err(error) = keeper.serve(listener).await {
                eprintln!("git store: socket server stopped: {error:#}");
            }
        }
    });
    Ok(StoreHandle {
        keeper,
        socket,
        tasks: vec![serve],
    })
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

    /// The workset's state directory, `<state>/worksets/<id>/state`,
    /// bound into the view at this same path: direnv's layout and the nix
    /// GC roots it registers live here, so they resolve on the host and die
    /// with the workset.
    pub fn state_dir(&self) -> anyhow::Result<Utf8PathBuf> {
        Ok(self
            .owner()?
            .root
            .join("worksets")
            .join(&self.0.id)
            .join("state"))
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

    /// Checks out `rev` (a branch, tag or commit, as `git checkout` takes
    /// it) in the repository at `checkout`, a host directory inside the
    /// workset, always detached: an agent starts on a commit, and any
    /// branch is one it makes itself. An empty `rev` detaches where the
    /// clone was born, the remote's default branch.
    pub async fn checkout(&self, checkout: &Utf8Path, rev: &str) -> anyhow::Result<()> {
        let rev = rev.trim();
        anyhow::ensure!(!rev.starts_with('-'), "not a revision: {rev}");
        let owner = self.owner()?;
        let _guard = self.0.operation_lock.lock().await;
        let mut command = owner.command("git");
        command
            .current_dir(checkout)
            .args(["checkout", "--quiet", "--detach"]);
        if !rev.is_empty() {
            command.arg(rev);
        }
        run(command, &format!("check out {rev}")).await
    }

    /// A live diff of the checkout containing `checkout` against its base.
    ///
    /// TODO: not ported to git yet.
    pub async fn diff_snapshot(
        &self,
        checkout: &Utf8Path,
        _known_commit_id: Option<&str>,
        _include_paths: &[Utf8PathBuf],
    ) -> anyhow::Result<Option<WorkspaceDiffSnapshot>> {
        let (checkout, is_git) = resolve_workdir_root(checkout.as_std_path())?;
        anyhow::ensure!(is_git, "diff view requires a git repository: {checkout}");
        anyhow::bail!("the diff view is not available yet for git checkouts")
    }

    /// Base-side contents for paths of an earlier diff snapshot.
    ///
    /// TODO: not ported to git yet.
    pub async fn diff_base_contents(
        &self,
        checkout: &Utf8Path,
        _operation_id: &str,
        _commit_id: &str,
        _paths: &[Utf8PathBuf],
    ) -> anyhow::Result<Vec<WorkspaceDiffBaseContent>> {
        let (checkout, is_git) = resolve_workdir_root(checkout.as_std_path())?;
        anyhow::ensure!(is_git, "diff view requires a git repository: {checkout}");
        anyhow::bail!("the diff view is not available yet for git checkouts")
    }

    /// Clones `remote_url` into `<root>/<name>` from the mirror store,
    /// exactly as the agent's `git clone` would (or with plain git when no
    /// keeper runs). `name` defaults to the repository name in the URL. An
    /// existing clone of that name is returned as is.
    pub async fn clone_repo(
        &self,
        remote_url: &str,
        name: Option<&str>,
    ) -> anyhow::Result<Utf8PathBuf> {
        let name = match name {
            Some(name) => name.to_owned(),
            None => rho_git_proto::repo_name(remote_url)
                .with_context(|| format!("remote URL has no repository name: {remote_url:?}"))?,
        };
        validate_name(&name)?;
        let owner = self.owner()?;
        let _guard = self.0.operation_lock.lock().await;
        let target = self.0.root.join(&name);
        if is_git_checkout(target.as_std_path()) {
            return Ok(target);
        }
        anyhow::ensure!(
            !target.exists(),
            "{target} exists and is not a git repository"
        );
        let cloned = match &owner.store {
            Some(store) => {
                let mirror = store.keeper.ensure(remote_url).await?;
                let git = rho_git_client::Git::new(GIT);
                let url = remote_url.to_owned();
                let dest = target.clone();
                tokio::task::spawn_blocking(move || {
                    rho_git_client::clone_from_mirror(&git, &mirror, &url, dest.as_std_path())
                })
                .await
                .context("clone worker panicked")?
            }
            None => {
                let mut command = owner.command("git");
                command
                    .current_dir(&self.0.root)
                    .args(["clone", "--quiet", "--", remote_url, &name]);
                run(command, "clone repository").await
            }
        };
        if let Err(error) = cloned {
            let _ = std::fs::remove_dir_all(&target);
            return Err(error);
        }
        Ok(target)
    }

    /// Names of the top-level git checkouts in the workset.
    pub fn repos(&self) -> anyhow::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(&self.0.root)
            .with_context(|| format!("list workset {}", self.0.root))?
        {
            let entry = entry?;
            if is_git_checkout(&entry.path())
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

/// Whether `path` is the root of a git checkout: a `.git` directory, or the
/// `.git` file of a worktree.
fn is_git_checkout(path: &Path) -> bool {
    path.join(".git").exists()
}

/// Walks up from `path` to the containing git checkout root when there is
/// one, otherwise canonicalizes the plain directory. Returns the root and
/// whether it is a git checkout.
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
        if is_git_checkout(cursor.as_std_path()) {
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
