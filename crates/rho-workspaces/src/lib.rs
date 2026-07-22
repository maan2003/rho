//! Per-agent working sets of stable jj-managed workspaces.
//!
//! An agent's filesystem is a [`View`]: a working set of workdir entries,
//! fixed at spawn, each binding an origin path to a materialized
//! [`Workspace`] (a jj-managed checkout, the user's live checkout, or a plain
//! directory). Each managed checkout has a repository-local `ws-` prefix id,
//! a stable bcachefs subvolume selected by jj, and a shared GC-inhibitor lease
//! held by Rho. With namespaces available, each agent's commands run in
//! a private per-view mount namespace where every entry's checkout is
//! mounted *over its origin repo path*, so the agent sees the real paths:
//! informative context, working `../` relative references, and
//! absolute-path-keyed caches (cargo) stay valid.
//!
//! Requires the bundled jj fork on PATH. jj owns managed-id allocation,
//! materialization, recovery, and garbage collection; Rho owns live use.

use std::collections::HashMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::os::fd::{BorrowedFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use prefix_id::{PrefixId, PrefixIdDomain};
use rustix::fs::FlockOperation;
use senax_encoder::{Decode, Encode, Pack, Unpack};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use tokio::sync::Mutex;

mod ns;
mod sandbox;

pub use ns::init_daemon_namespace;

pub type WorkspaceId = PrefixId<WorkspaceIdDomain>;

fn workspace_handle(id: WorkspaceId) -> String {
    format!("ws-{}", id.encoded())
}

#[derive(Debug)]
struct ManagedWorkspace {
    id: String,
    root: Utf8PathBuf,
    lock: Utf8PathBuf,
    materialized: bool,
}

impl ManagedWorkspace {
    fn id(&self) -> anyhow::Result<WorkspaceId> {
        let encoded = self
            .id
            .strip_prefix("ws-")
            .context("jj managed workspace id is missing ws- prefix")?;
        WorkspaceId::from_encoded(encoded).context("jj returned an invalid managed workspace id")
    }
}

#[derive(Deserialize)]
struct ManagedWorkspaceWire {
    id: String,
    root: String,
    lock: String,
    materialized: bool,
}

#[derive(Debug)]
struct WorkspaceLease {
    file: File,
    path: Utf8PathBuf,
}

impl WorkspaceLease {
    fn acquire(path: &Utf8Path) -> anyhow::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .with_context(|| format!("open workspace lease {path}"))?;
        match rustix::fs::flock(&file, FlockOperation::NonBlockingLockShared) {
            Ok(()) => {}
            Err(rustix::io::Errno::WOULDBLOCK) => {
                anyhow::bail!("workspace is being garbage-collected; retry: {path}")
            }
            Err(error) => return Err(error).with_context(|| format!("lock workspace {path}")),
        }
        let lease = Self {
            file,
            path: path.to_owned(),
        };
        lease.touch()?;
        Ok(lease)
    }

    fn touch(&self) -> anyhow::Result<()> {
        self.file
            .set_modified(std::time::SystemTime::now())
            .with_context(|| format!("touch workspace lease {}", self.path))
    }
}

impl Drop for WorkspaceLease {
    fn drop(&mut self) {
        if let Err(error) = self.touch() {
            eprintln!(
                "rho-workspaces: touch lease {} on release: {error}",
                self.path
            );
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathOverrides {
    pub before: Vec<PathBuf>,
    pub after: Vec<PathBuf>,
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

    fn path(&self) -> Option<&OsStr> {
        self.get("PATH")
    }

    pub fn get(&self, name: &str) -> Option<&OsStr> {
        self.0
            .iter()
            .find_map(|(key, value)| (key == name).then_some(value.as_os_str()))
    }
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

/// Prefix-id family for repository-local jj-managed workspace ids.
///
/// jj owns the actual per-repository seed and counter. Rho persists the
/// resulting encoded id and does not allocate production ids itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceIdDomain(pub u64);

impl PrefixIdDomain for WorkspaceIdDomain {
    const KIND: &'static str = "managed-workspace-id";

    fn machine_seed(&self) -> u64 {
        self.0
    }
}

/// Where an agent works, stored inline on the agent record. Self-contained:
/// there is no separate workspace table.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode, Pack, Unpack)]
pub enum WorkspaceInfo {
    /// The user's own checkout: the agent works directly at the repo path,
    /// no separate checkout and no namespace.
    UserCheckout { repo: Utf8PathBuf },
    /// A stable jj-managed workspace. jj selects and persists its checkout
    /// path; Rho stores only the repository-local id.
    Workspace {
        repo: Utf8PathBuf,
        #[senax(rename = "name")]
        id: WorkspaceId,
    },
    /// A jj-managed workspace whose original VCS metadata is masked from
    /// child commands and replaced by a synthetic Git baseline.
    Sandbox { repo: Utf8PathBuf, id: WorkspaceId },
}

impl WorkspaceInfo {
    pub fn repo(&self) -> &Utf8Path {
        match self {
            Self::UserCheckout { repo }
            | Self::Workspace { repo, .. }
            | Self::Sandbox { repo, .. } => repo,
        }
    }

    pub fn is_user_checkout(&self) -> bool {
        matches!(self, Self::UserCheckout { .. })
    }

    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        match self {
            Self::UserCheckout { .. } => None,
            Self::Workspace { id, .. } | Self::Sandbox { id, .. } => Some(*id),
        }
    }

    pub fn workspace_handle(&self) -> Option<String> {
        match self {
            Self::Workspace { id, .. } => Some(workspace_handle(*id)),
            Self::UserCheckout { .. } | Self::Sandbox { .. } => None,
        }
    }

    pub fn is_sandbox(&self) -> bool {
        matches!(self, Self::Sandbox { .. })
    }
}

/// One jj repo the daemon works with — the crate's entry object.
/// Everything repo-scoped lives here: jj invocation serialization, weak live
/// workspace indexing, one opportunistic managed-workspace GC, and ws-parent
/// pointer plumbing.
///
/// Hold exactly one instance per repo root (the daemon keeps a dedup map):
/// agents joining a workspace share one live [`Workspace`] — one checkout —
/// only within one `Repo` instance.
#[derive(Debug)]
pub struct Repo {
    /// Canonicalized origin root; UTF-8 validated at open.
    root: Utf8PathBuf,
    /// False for a plain directory workdir with no jj repo: only the live
    /// [`Workspace`] exists for it, and snapshotting is a no-op.
    is_jj: bool,
    /// PATH entries applied to commands prepared for this repo's workspaces.
    path_overrides: PathOverrides,
    user_environment: Option<UserEnvironment>,
    /// Serializes jj invocations. jj's op log makes concurrent commands safe
    /// on its own; this is simplicity insurance while the fork's
    /// descendant-workspace snapshot path is young.
    jj_lock: Mutex<()>,
    /// Weakly indexes live workspaces, so agents and views remain their strong
    /// owners while agents joining a workspace share its checkout, mount
    /// namespace, and lazy setup.
    workspaces: Mutex<HashMap<WorkspaceInfo, Weak<Workspace>>>,
    gc_started: AtomicBool,
}

impl Repo {
    /// Opens the repo containing `path` (resolving through workspace
    /// pointers to the origin root).
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_with_path_overrides(path, PathOverrides::default())
    }

    /// Opens the repo containing `path` and records command PATH overrides
    /// for workspaces materialized from this handle.
    pub fn open_with_path_overrides(
        path: &Path,
        path_overrides: PathOverrides,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            root: resolve_repo_root(path)?,
            is_jj: true,
            path_overrides,
            user_environment: None,
            jj_lock: Mutex::new(()),
            workspaces: Mutex::new(HashMap::new()),
            gc_started: AtomicBool::new(false),
        })
    }

    pub fn open_with_environment(
        path: &Path,
        path_overrides: PathOverrides,
        user_environment: UserEnvironment,
    ) -> anyhow::Result<Self> {
        let mut repo = Self::open_with_path_overrides(path, path_overrides)?;
        repo.user_environment = Some(user_environment);
        Ok(repo)
    }

    /// Opens a plain directory (no jj repo) as a live-only workdir: agents
    /// work directly at the path, and no separate workspaces can be created.
    pub fn open_plain_with_path_overrides(
        path: &Path,
        path_overrides: PathOverrides,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            path.is_absolute(),
            "workdir path must be absolute: {}",
            path.display()
        );
        let path = path
            .canonicalize()
            .with_context(|| format!("workdir does not exist: {}", path.display()))?;
        let root = Utf8PathBuf::try_from(path).context("workdir path is not valid UTF-8")?;
        anyhow::ensure!(root.is_dir(), "workdir is not a directory: {root}");
        Ok(Self {
            root,
            is_jj: false,
            path_overrides,
            user_environment: None,
            jj_lock: Mutex::new(()),
            workspaces: Mutex::new(HashMap::new()),
            gc_started: AtomicBool::new(false),
        })
    }

    pub fn open_plain_with_environment(
        path: &Path,
        path_overrides: PathOverrides,
        user_environment: UserEnvironment,
    ) -> anyhow::Result<Self> {
        let mut repo = Self::open_plain_with_path_overrides(path, path_overrides)?;
        repo.user_environment = Some(user_environment);
        Ok(repo)
    }

    /// Whether this workdir is a jj repo (as opposed to a plain live
    /// directory).
    pub fn is_jj(&self) -> bool {
        self.is_jj
    }

    /// Allocates and creates a stable jj-managed workspace on
    /// `parent_revset`.
    pub async fn create_workspace(
        self: &Arc<Self>,
        parent_revset: &str,
    ) -> anyhow::Result<Arc<Workspace>> {
        anyhow::ensure!(self.is_jj, "not a jj repository: {}", self.root);
        let (managed, lease) = self.create_managed(parent_revset).await?;
        let info = self.workspace_info(managed.id()?);
        let workspace = self
            .cache_workspace(info, managed.root, Some(lease))
            .await?;
        self.collect_stale_workspaces();
        Ok(workspace)
    }

    /// Creates a managed workspace, masks its original VCS metadata from child
    /// commands, and creates a synthetic Git baseline for them.
    pub async fn create_sandbox(
        self: &Arc<Self>,
        parent_revset: &str,
    ) -> anyhow::Result<Arc<Workspace>> {
        anyhow::ensure!(
            self.is_jj,
            "sandbox source is not a jj repository: {}",
            self.root
        );
        let (managed, lease) = self.create_managed(parent_revset).await?;
        let id = managed.id()?;
        let info = WorkspaceInfo::Sandbox {
            repo: self.root.clone(),
            id,
        };
        let base = sandbox_base(&self.root, id)?;
        anyhow::ensure!(!base.exists(), "sandbox already exists: {base}");
        let checkout = managed.root;
        let result = async {
            for dir in [
                base.join("home"),
                base.join("tmp"),
                base.join("run"),
                base.join("masks/.jj"),
            ] {
                std::fs::create_dir_all(&dir)
                    .with_context(|| format!("create sandbox directory {dir}"))?;
            }
            std::fs::write(base.join("masks/.git"), "")
                .context("create sandbox Git metadata mask")?;
            self.initialize_sandbox_git(&checkout, &base.join("git"))
                .await
        }
        .await;
        if let Err(error) = result {
            let _ = std::fs::remove_dir_all(&base);
            return Err(error);
        }
        let workspace = self.cache_workspace(info, checkout, Some(lease)).await?;
        self.collect_stale_workspaces();
        Ok(workspace)
    }

    /// Opens an existing workspace, returning the live shared instance when
    /// one exists. Agents may share this checkout while retaining separate
    /// View mount namespaces. Missing managed subvolumes are rematerialized by
    /// jj at their stable paths.
    pub async fn open_workspace(
        self: &Arc<Self>,
        id: WorkspaceId,
    ) -> anyhow::Result<Arc<Workspace>> {
        anyhow::ensure!(self.is_jj, "not a jj repository: {}", self.root);
        let info = self.workspace_info(id);
        let mut workspaces = self.workspaces.lock().await;
        if let Some(workspace) = workspaces.get(&info).and_then(Weak::upgrade) {
            return Ok(workspace);
        }
        let (managed, lease) = self.open_managed(id).await?;
        let workspace = Arc::new(self.workspace(info.clone(), managed.root, Some(lease)));
        workspaces.insert(info, Arc::downgrade(&workspace));
        self.collect_stale_workspaces();
        Ok(workspace)
    }

    pub async fn open_sandbox(self: &Arc<Self>, id: WorkspaceId) -> anyhow::Result<Arc<Workspace>> {
        let info = WorkspaceInfo::Sandbox {
            repo: self.root.clone(),
            id,
        };
        let mut workspaces = self.workspaces.lock().await;
        if let Some(workspace) = workspaces.get(&info).and_then(Weak::upgrade) {
            return Ok(workspace);
        }
        let (managed, lease) = self.open_managed(id).await?;
        let base = sandbox_base(&self.root, id)?;
        let checkout = managed.root;
        anyhow::ensure!(
            base.join("git/HEAD").is_file(),
            "sandbox Git baseline is missing: {base}"
        );
        let workspace = Arc::new(self.workspace(info.clone(), checkout, Some(lease)));
        workspaces.insert(info, Arc::downgrade(&workspace));
        self.collect_stale_workspaces();
        Ok(workspace)
    }

    /// A workspace standing for the user's own checkout: the agent works
    /// directly at the repo path with no separate checkout and no namespace.
    pub async fn user_checkout(self: &Arc<Self>) -> anyhow::Result<Arc<Workspace>> {
        let info = WorkspaceInfo::UserCheckout {
            repo: self.root.clone(),
        };
        let mut workspaces = self.workspaces.lock().await;
        if let Some(workspace) = workspaces.get(&info).and_then(Weak::upgrade) {
            return Ok(workspace);
        }
        let workspace = Arc::new(self.workspace(info.clone(), self.root.clone(), None));
        workspaces.insert(info, Arc::downgrade(&workspace));
        Ok(workspace)
    }

    /// The origin repo root (canonicalized).
    pub fn root(&self) -> &Utf8Path {
        &self.root
    }

    pub fn path_overrides(&self) -> &PathOverrides {
        &self.path_overrides
    }

    fn workspace_info(&self, id: WorkspaceId) -> WorkspaceInfo {
        WorkspaceInfo::Workspace {
            repo: self.root.clone(),
            id,
        }
    }

    async fn create_managed(
        &self,
        parent_revset: &str,
    ) -> anyhow::Result<(ManagedWorkspace, WorkspaceLease)> {
        let _guard = self.jj_lock.lock().await;
        let mut command = self.jj();
        command
            .args(["workspace", "managed", "create", "--revision"])
            .arg(parent_revset);
        let managed = run_managed_jj(command)
            .await
            .context("create managed jj workspace")?;
        let lease = WorkspaceLease::acquire(&managed.lock)?;
        self.prepare_managed_checkout(&managed)?;
        Ok((managed, lease))
    }

    /// Opens a managed workspace.
    async fn open_managed(
        &self,
        id: WorkspaceId,
    ) -> anyhow::Result<(ManagedWorkspace, WorkspaceLease)> {
        let _guard = self.jj_lock.lock().await;
        let handle = workspace_handle(id);
        let mut resolve = self.jj();
        resolve.args(["workspace", "managed", "resolve", &handle]);
        let resolved = run_managed_jj(resolve)
            .await
            .context("resolve managed jj workspace")?;
        let lease = WorkspaceLease::acquire(&resolved.lock)?;
        let managed = if resolved.materialized && resolved.root.is_dir() {
            resolved
        } else {
            let mut attach = self.jj();
            attach.args([
                "workspace",
                "managed",
                "attach",
                "--external-lease",
                &resolved.id,
            ]);
            let attached = run_managed_jj(attach)
                .await
                .context("attach managed jj workspace")?;
            anyhow::ensure!(
                attached.root == resolved.root && attached.lock == resolved.lock,
                "jj changed a managed workspace's stable paths"
            );
            attached
        };
        self.prepare_managed_checkout(&managed)?;
        Ok((managed, lease))
    }

    fn prepare_managed_checkout(&self, managed: &ManagedWorkspace) -> anyhow::Result<()> {
        anyhow::ensure!(
            managed.materialized && managed.root.is_absolute() && managed.root.is_dir(),
            "jj reported an invalid managed workspace root: {}",
            managed.root
        );
        anyhow::ensure!(
            managed.lock.is_absolute(),
            "jj reported a relative lease path"
        );
        self.rewrite_pointers(&managed.root)?;
        for filename in ["flake.nix", "flake.lock", ".envrc"] {
            let _ = copy_mtime_if_same(self.root().join(filename), managed.root.join(filename));
        }
        Ok(())
    }

    async fn cache_workspace(
        self: &Arc<Self>,
        info: WorkspaceInfo,
        checkout: Utf8PathBuf,
        lease: Option<WorkspaceLease>,
    ) -> anyhow::Result<Arc<Workspace>> {
        let mut workspaces = self.workspaces.lock().await;
        if let Some(workspace) = workspaces.get(&info).and_then(Weak::upgrade) {
            return Ok(workspace);
        }
        let workspace = Arc::new(self.workspace(info.clone(), checkout, lease));
        workspaces.insert(info, Arc::downgrade(&workspace));
        Ok(workspace)
    }

    fn workspace(
        self: &Arc<Self>,
        info: WorkspaceInfo,
        checkout: Utf8PathBuf,
        lease: Option<WorkspaceLease>,
    ) -> Workspace {
        Workspace {
            info: info.clone(),
            repo: Arc::clone(self),
            checkout,
            _lease: lease,
            context_config: OnceLock::new(),
        }
    }

    /// A jj command running against this repo.
    fn jj(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("jj");
        if let Some(environment) = &self.user_environment {
            environment.apply(&mut command);
        }
        command.arg("--repository").arg(&self.root);
        command
    }

    async fn initialize_sandbox_git(
        &self,
        checkout: &Utf8Path,
        git_dir: &Utf8Path,
    ) -> anyhow::Result<()> {
        for args in [
            &["init", "-b", "main"][..],
            &["add", "-A"][..],
            &[
                "-c",
                "user.name=rho sandbox",
                "-c",
                "user.email=sandbox@rho.invalid",
                "commit",
                "-m",
                "sandbox baseline",
            ][..],
        ] {
            let mut command = tokio::process::Command::new("git");
            if let Some(environment) = &self.user_environment {
                environment.apply(&mut command);
            }
            command
                .env("GIT_DIR", git_dir)
                .env("GIT_WORK_TREE", checkout)
                .current_dir(checkout)
                .args(args);
            let output = command.output().await.context("spawn sandbox git")?;
            anyhow::ensure!(
                output.status.success(),
                "sandbox git failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            if args.first() == Some(&"init") {
                std::fs::write(git_dir.join("info/exclude"), "/.git\n/.jj\n")
                    .context("exclude hidden workspace metadata from sandbox Git")?;
            }
        }
        Ok(())
    }

    /// Starts one repository-local heartbeat and GC loop after this daemon
    /// first uses a managed workspace.
    fn collect_stale_workspaces(self: &Arc<Self>) {
        if self.gc_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let repo = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let live = repo
                    .workspaces
                    .lock()
                    .await
                    .values()
                    .filter_map(Weak::upgrade)
                    .collect::<Vec<_>>();
                for workspace in live {
                    if let Some(lease) = &workspace._lease
                        && let Err(error) = lease.touch()
                    {
                        eprintln!(
                            "rho-workspaces: heartbeat workspace {}: {error:#}",
                            workspace.repo()
                        );
                    }
                }
                {
                    let _guard = repo.jj_lock.lock().await;
                    let mut command = repo.jj();
                    command.args([
                        "workspace",
                        "managed",
                        "gc",
                        "--older-than-seconds",
                        "86400",
                    ]);
                    if let Err(error) = run_jj(command).await {
                        eprintln!(
                            "rho-workspaces: managed workspace GC for {} failed: {error:#}",
                            repo.root
                        );
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(60 * 60)).await;
            }
        });
    }

    /// Redirects the checkout's back-references to the origin repo through
    /// `<origin>/.jj/ws-parent`, which resolves in every namespace (the
    /// origin path itself is covered by the checkout mount inside the agent's
    /// namespace). Idempotent; runs after every attach since jj recreates
    /// the `.git` worktree pointer each time. Also ensures the origin's
    /// ws-parent symlink, which must exist before the rewritten pointer is
    /// ever read.
    fn rewrite_pointers(&self, checkout: &Utf8Path) -> anyhow::Result<()> {
        ensure_ws_parent_symlink(self.root())?;
        let parent = self.root().join(".jj").join(ns::WS_PARENT);
        // The checkout-side directory the agent namespace binds the origin onto.
        match std::fs::create_dir(checkout.join(".jj").join(ns::WS_PARENT)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("create checkout ws-parent dir"),
        }

        let jj_pointer = checkout.join(".jj").join("repo");
        let target = parent.join(".jj").join("repo");
        std::fs::write(&jj_pointer, target.as_str())
            .with_context(|| format!("rewrite {jj_pointer}"))?;

        // Git worktree checkouts get a `gitdir: <origin>/...` file; same
        // treatment. Replacement is a no-op when the pointer was already
        // rewritten.
        let git_pointer = checkout.join(".git");
        if git_pointer.is_file() {
            let content =
                std::fs::read_to_string(&git_pointer).context("read git worktree pointer")?;
            let rewritten_content = content.replace(self.root.as_str(), parent.as_str());
            std::fs::write(&git_pointer, &rewritten_content)
                .context("rewrite git worktree pointer")?;
            if let Some(gitdir) = gitdir_path(&rewritten_content) {
                let back_pointer = gitdir.join("gitdir");
                if back_pointer.is_file() {
                    let content = std::fs::read_to_string(&back_pointer)
                        .context("read git worktree back-pointer")?;
                    std::fs::write(
                        &back_pointer,
                        content.replace(self.root.as_str(), parent.as_str()),
                    )
                    .context("rewrite git worktree back-pointer")?;
                }
            }
        }
        Ok(())
    }
}

/// One materialized workspace checkout: the unit agents share and [`View`]s
/// compose. Callers share it via `Arc`.
#[derive(Debug)]
pub struct Workspace {
    info: WorkspaceInfo,
    repo: Arc<Repo>,
    checkout: Utf8PathBuf,
    _lease: Option<WorkspaceLease>,
    context_config: OnceLock<Arc<rho_context_config::DiscoveredContext>>,
}

impl Workspace {
    pub fn info(&self) -> &WorkspaceInfo {
        &self.info
    }

    pub fn is_user_checkout(&self) -> bool {
        self.info.is_user_checkout()
    }

    pub fn is_sandbox(&self) -> bool {
        self.info.is_sandbox()
    }

    fn sandbox_base(&self) -> Option<Utf8PathBuf> {
        match self.info() {
            WorkspaceInfo::Sandbox { id, .. } => sandbox_base(self.repo(), *id).ok(),
            WorkspaceInfo::UserCheckout { .. } | WorkspaceInfo::Workspace { .. } => None,
        }
    }

    /// The origin repo root — the path agents see and the system prompt
    /// reports (the checkout is mounted over it in the agent's namespace).
    pub fn repo(&self) -> &Utf8Path {
        self.repo.root()
    }

    pub fn discovered_context(&self) -> Arc<rho_context_config::DiscoveredContext> {
        Arc::clone(self.context_config.get_or_init(|| {
            Arc::new(rho_context_config::DiscoveredContext::discover(
                self.repo(),
                self.checkout(),
            ))
        }))
    }

    /// The checkout directory in the daemon/host namespace. In-process file
    /// operations (patches) and namespace-less fallback execution use this.
    pub fn checkout(&self) -> &Utf8Path {
        &self.checkout
    }

    /// Whether commands for this workspace need a cover mount (the checkout is
    /// not already the origin path).
    fn needs_mount(&self) -> bool {
        !self.is_user_checkout()
    }

    /// Commits the checkout's current file state and its descendant workspace
    /// states into the repo. Called at turn boundaries so the user's own jj
    /// view follows the agent's work.
    /// A no-op for plain (non-jj) directory workdirs.
    pub async fn snapshot(&self) -> anyhow::Result<()> {
        if !self.repo.is_jj {
            return Ok(());
        }
        let _guard = self.repo.jj_lock.lock().await;
        let mut command = tokio::process::Command::new("jj");
        if let Some(environment) = &self.repo.user_environment {
            environment.apply(&mut command);
        }
        command
            .current_dir(&self.checkout)
            .args(["util", "snapshot"]);
        run_jj(command).await.context("jj snapshot")?;
        Ok(())
    }
}

/// One agent's view of the filesystem: a working set of workdir entries
/// fixed at construction, each binding an origin path to a materialized
/// [`Workspace`], realized as a private mount namespace with each entry's
/// checkout mounted over its origin path. Entry 0 is the primary workdir: the
/// default cwd and the path the system prompt reports.
///
/// The namespace fd is created lazily on the first prepared command that
/// needs a cover mount (a daemon restart needs no recovery pass — the first
/// spawned command pays the ~100µs setup once).
#[derive(Debug)]
pub struct View {
    entries: WorkingSet,
    /// Present once a command needed a cover mount. Holding the fd keeps the
    /// namespace alive; commands enter it with `setns` from `pre_exec`.
    /// The lock serializes namespace creation, so the namespace is built
    /// exactly once.
    mnt_ns: Mutex<Option<OwnedFd>>,
}

/// A nonempty working set of workdirs with pairwise-disjoint origin roots,
/// primary first. The constructor enforces the invariants, so a held
/// `WorkingSet` is always valid: overlapping roots would shadow each other's
/// cover mounts (a parent mounted after a child hides the child's checkout).
#[derive(Debug, Clone)]
pub struct WorkingSet(Vec<Arc<Workspace>>);

impl WorkingSet {
    pub fn new(entries: Vec<Arc<Workspace>>) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !entries.is_empty(),
            "a working set needs at least one workdir"
        );
        for (i, entry) in entries.iter().enumerate() {
            Self::ensure_disjoint(&entries[..i], entry.repo())?;
        }
        Ok(Self(entries))
    }

    /// The primary workdir (entry 0): default cwd, prompt header.
    pub fn primary(&self) -> &Arc<Workspace> {
        &self.0[0]
    }

    fn ensure_disjoint(existing: &[Arc<Workspace>], candidate: &Utf8Path) -> anyhow::Result<()> {
        for entry in existing {
            anyhow::ensure!(
                !candidate.starts_with(entry.repo()) && !entry.repo().starts_with(candidate),
                "workdir root {candidate} overlaps {} already in the working set",
                entry.repo()
            );
        }
        Ok(())
    }
}

impl std::ops::Deref for WorkingSet {
    type Target = [Arc<Workspace>];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'a> IntoIterator for &'a WorkingSet {
    type Item = &'a Arc<Workspace>;
    type IntoIter = std::slice::Iter<'a, Arc<Workspace>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl View {
    /// A view over `entries` (nonempty; entry 0 is the primary workdir).
    /// Entries must have disjoint origin roots.
    pub fn new(entries: Vec<Arc<Workspace>>) -> anyhow::Result<Arc<Self>> {
        let sandboxed = entries.iter().filter(|entry| entry.is_sandbox()).count();
        anyhow::ensure!(
            sandboxed == 0 || sandboxed == entries.len(),
            "sandbox and ordinary workdirs cannot be mixed in one view"
        );
        Ok(Arc::new(Self {
            entries: WorkingSet::new(entries)?,
            mnt_ns: Mutex::new(None),
        }))
    }

    /// The primary workdir (entry 0): default cwd, prompt header.
    pub fn primary(&self) -> Arc<Workspace> {
        Arc::clone(self.entries.primary())
    }

    /// The working set, primary first.
    pub fn entries(&self) -> &WorkingSet {
        &self.entries
    }

    /// Configure a child process to run in this view: enter the view's
    /// namespace, set PATH from the primary workdir's repo, and set the cwd
    /// (relative to the primary workdir; absolute used as-is; `None` for the
    /// primary itself).
    ///
    /// `file_mounts` are `(source, target)` absolute file paths to bind
    /// inside the namespace; targets must already exist. A view whose
    /// entries are all live checkouts has no private namespace, so file
    /// mounts are rejected rather than leaking mounts into the daemon
    /// namespace.
    pub async fn prepare_command(
        &self,
        command: &mut tokio::process::Command,
        cwd: Option<&Utf8Path>,
        file_mounts: Vec<(Utf8PathBuf, Utf8PathBuf)>,
    ) -> anyhow::Result<()> {
        let mut ns_guard = self.mnt_ns.lock().await;
        let entries = self.entries();
        let primary = &entries[0];
        let base_path = if let Some(environment) = &primary.repo.user_environment {
            environment.apply(command);
            environment
                .path()
                .context("user environment has no PATH")?
                .to_owned()
        } else {
            std::env::var_os("PATH").context("PATH must be set")?
        };
        command.env("PATH", primary.repo.path_overrides().add_to(&base_path));
        let landlock = if primary.is_sandbox() {
            let base = primary.sandbox_base().expect("sandbox has base");
            command
                .env("HOME", base.join("home"))
                .env("TMPDIR", base.join("tmp"))
                .env("XDG_RUNTIME_DIR", base.join("run"))
                .env("XDG_CACHE_HOME", base.join("home/.cache"))
                .env("XDG_CONFIG_HOME", base.join("home/.config"))
                .env("GIT_DIR", base.join("git"))
                .env("GIT_WORK_TREE", primary.repo());
            let mut writable = Vec::new();
            for entry in entries {
                writable.push(entry.checkout().as_std_path().to_owned());
                let base = entry.sandbox_base().expect("sandbox has base");
                writable.extend(
                    ["home", "tmp", "run", "git"]
                        .into_iter()
                        .map(|name| base.join(name).into_std_path_buf()),
                );
            }
            Some(sandbox::Policy::new(&writable, &base_path)?)
        } else {
            None
        };
        let cwd = cwd.map_or_else(|| primary.repo().to_owned(), |cwd| primary.repo().join(cwd));
        if !entries.iter().any(|entry| entry.needs_mount()) {
            anyhow::ensure!(
                file_mounts.is_empty(),
                "live-checkout views have no private mount namespace for file mounts"
            );
            command.current_dir(cwd.as_std_path());
            return Ok(());
        }
        let ns_fd = std::os::fd::AsRawFd::as_raw_fd(ensure_ns(&mut ns_guard, entries).await);
        let cwd = CString::new(cwd.as_str().as_bytes())
            .map_err(|_| anyhow::anyhow!("workspace path contains a NUL byte"))?;
        let mut mounts = Vec::with_capacity(file_mounts.len());
        for (source, target) in file_mounts {
            mounts.push((
                CString::new(source.as_str().as_bytes())
                    .map_err(|_| anyhow::anyhow!("file mount source contains a NUL byte"))?,
                CString::new(target.as_str().as_bytes())
                    .map_err(|_| anyhow::anyhow!("file mount target contains a NUL byte"))?,
            ));
        }
        unsafe {
            command.pre_exec(move || {
                enter_workspace_ns(ns_fd, &cwd, &mounts)?;
                if let Some(policy) = &landlock {
                    policy.restrict_self()?;
                }
                Ok(())
            });
        }
        Ok(())
    }

    /// Maps a path in the agent-visible view (origin paths) to the host path
    /// where the bytes actually live (managed checkouts), for in-process file
    /// operations that do not enter the namespace. Paths outside every entry
    /// are returned unchanged.
    pub fn resolve_host_path(&self, path: &Path) -> PathBuf {
        for entry in self.entries() {
            if let Ok(rel) = path.strip_prefix(entry.repo().as_std_path()) {
                return entry.checkout().as_std_path().join(rel);
            }
        }
        path.to_owned()
    }

    pub fn resolve_host_path_checked(&self, path: &Path) -> anyhow::Result<PathBuf> {
        if !self.entries[0].is_sandbox() {
            return Ok(self.resolve_host_path(path));
        }
        if path.is_absolute() {
            for entry in self.entries() {
                if let Ok(rel) = path.strip_prefix(entry.repo().as_std_path()) {
                    return sandbox_join(entry.checkout().as_std_path(), rel);
                }
            }
            anyhow::bail!(
                "sandbox patch path is outside every workdir: {}",
                path.display()
            );
        }
        sandbox_join(self.entries[0].checkout().as_std_path(), path)
    }

    /// Commits every jj entry's checkout state into its repo. Called at turn
    /// boundaries so the user's jj view follows the agent's work in each
    /// workdir.
    pub async fn snapshot(&self) -> anyhow::Result<()> {
        let mut failures = Vec::new();
        for entry in self.entries() {
            if let Err(error) = entry.snapshot().await {
                failures.push(format!("{}: {error:#}", entry.repo()));
            }
        }
        anyhow::ensure!(
            failures.is_empty(),
            "snapshot failed: {}",
            failures.join("; ")
        );
        Ok(())
    }
}

/// The view's mount namespace, created on first need under the view's
/// namespace lock.
///
/// Panics when namespace setup fails (including when
/// [`init_daemon_namespace`] never ran): an agent whose real cwd contradicts
/// the paths baked into its context would silently corrupt every later turn,
/// so there is deliberately no degraded mode.
async fn ensure_ns<'ns>(
    ns_guard: &'ns mut Option<OwnedFd>,
    entries: &[Arc<Workspace>],
) -> &'ns OwnedFd {
    if ns_guard.is_none() {
        let mounts = entries
            .iter()
            .filter(|entry| entry.needs_mount())
            .map(|entry| ns::ViewMount {
                repo: entry.repo().as_std_path().to_owned(),
                checkout: entry.checkout().as_std_path().to_owned(),
                metadata_masks: entry.sandbox_base().map(|base| {
                    (
                        base.join("masks/.jj").into_std_path_buf(),
                        base.join("masks/.git").into_std_path_buf(),
                    )
                }),
            })
            .collect::<Vec<_>>();
        let ns = tokio::task::spawn_blocking(move || ns::create_view_ns(mounts))
            .await
            .expect("namespace task panicked")
            .expect("view mount namespace setup failed");
        *ns_guard = Some(ns);
    }
    ns_guard.as_ref().expect("namespace just ensured")
}

/// Runs between fork and exec: enter the workspace's mount namespace, move
/// to the working directory (whose path only resolves inside it), and shed
/// the daemon's in-namespace privileges. Must not allocate — the forked
/// child could deadlock on the allocator lock.
fn enter_workspace_ns(
    ns_fd: RawFd,
    cwd: &CStr,
    file_mounts: &[(CString, CString)],
) -> std::io::Result<()> {
    use rustix::thread::{CapabilitySet, CapabilitySets, LinkNameSpaceType};

    // SAFETY: the fd is kept alive by the `View` held by the spawning
    // agent/tool for the duration of spawn (a view's namespace fd is never
    // dropped once created).
    let fd = unsafe { BorrowedFd::borrow_raw(ns_fd) };
    rustix::thread::move_into_link_name_space(fd, Some(LinkNameSpaceType::Mount))?;
    for (source, target) in file_mounts {
        rustix::mount::mount_bind(source, target)?;
    }
    rustix::process::chdir(cwd)?;
    rustix::thread::set_no_new_privs(true)?;
    let empty = CapabilitySet::empty();
    rustix::thread::set_capabilities(
        None,
        CapabilitySets {
            effective: empty,
            permitted: empty,
            inheritable: empty,
        },
    )?;
    Ok(())
}

async fn run_jj(mut command: tokio::process::Command) -> anyhow::Result<Vec<u8>> {
    let output = command.output().await.context("spawn jj")?;
    if !output.status.success() {
        anyhow::bail!(
            "jj failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

async fn run_managed_jj(command: tokio::process::Command) -> anyhow::Result<ManagedWorkspace> {
    let stdout = run_jj(command).await?;
    let wire: ManagedWorkspaceWire =
        serde_json::from_slice(&stdout).context("parse managed workspace JSON")?;
    let root = Utf8PathBuf::from(wire.root);
    let lock = Utf8PathBuf::from(wire.lock);
    Ok(ManagedWorkspace {
        id: wire.id,
        root,
        lock,
        materialized: wire.materialized,
    })
}

/// Repo roots must be absolute, existing, UTF-8 jj repo roots. A path inside
/// a secondary workspace resolves to its origin repo via the `.jj/repo`
/// pointer.
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
        let origin = target
            .parent()
            .and_then(Utf8Path::parent)
            .with_context(|| format!("malformed repo pointer in {pointer}"))?
            .to_owned();
        anyhow::ensure!(
            origin.join(".jj").is_dir(),
            "workspace points at a missing repo: {origin}",
        );
        return Ok(origin);
    }
    Ok(path)
}

/// Resolves a workdir path for an agent working set: walks up to the
/// containing jj repo root (through workspace pointers) when there is one,
/// otherwise canonicalizes the plain directory. Returns the root and whether
/// it is a jj repo.
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
            return Ok((resolve_repo_root(cursor.as_std_path())?, true));
        }
        match cursor.parent() {
            Some(parent) => cursor = parent,
            None => return Ok((canonical, false)),
        }
    }
}

/// The origin's `.jj/ws-parent -> ..` symlink (back to the repo root — the
/// target resolves relative to the `.jj` dir holding the link) that checkout
/// pointers route through (see [`ns::WS_PARENT`]); created once per repo,
/// idempotent.
fn ensure_ws_parent_symlink(repo: &Utf8Path) -> anyhow::Result<()> {
    let link = repo.join(".jj").join(ns::WS_PARENT);
    match std::os::unix::fs::symlink("..", &link) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).with_context(|| format!("create {link}")),
    }
}

fn gitdir_path(pointer: &str) -> Option<Utf8PathBuf> {
    pointer
        .strip_prefix("gitdir: ")
        .map(str::trim)
        .map(Utf8PathBuf::from)
}

fn sandbox_base(repo: &Utf8Path, id: WorkspaceId) -> anyhow::Result<Utf8PathBuf> {
    let state = dirs::state_dir().context("state directory not available")?;
    let state = Utf8PathBuf::try_from(state).context("state directory is not valid UTF-8")?;
    let digest = Sha256::digest(repo.as_str().as_bytes());
    Ok(state.join("rho/sandboxes").join(format!(
        "{}-{}",
        id.encoded(),
        &format!("{digest:x}")[..16]
    )))
}

fn sandbox_join(root: &Path, path: &Path) -> anyhow::Result<PathBuf> {
    let mut joined = root.to_owned();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(component) => joined.push(component),
            std::path::Component::ParentDir => {
                anyhow::ensure!(joined != root, "sandbox patch path escapes its workdir");
                joined.pop();
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                anyhow::bail!("sandbox patch path is not relative")
            }
        }
    }
    Ok(joined)
}

/// Reuses a donor file's timestamps only when the checkout has identical
/// bytes. Retained managed checkouts may contain direnv/nix caches keyed by
/// these mtimes; preserving a timestamp across different contents would make a
/// stale cache look current.
fn copy_mtime_if_same(source: impl AsRef<Path>, target: impl AsRef<Path>) -> std::io::Result<()> {
    let source = source.as_ref();
    let target = target.as_ref();
    if std::fs::read(source)? != std::fs::read(target)? {
        return Ok(());
    }
    let meta = std::fs::metadata(source)?;
    let times = std::fs::FileTimes::new()
        .set_accessed(meta.accessed()?)
        .set_modified(meta.modified()?);
    std::fs::File::options()
        .write(true)
        .open(target)?
        .set_times(times)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use super::{Repo, copy_mtime_if_same, sandbox_join};

    #[tokio::test]
    async fn repo_cache_weakly_deduplicates_workspaces() {
        let temp = tempfile::tempdir().unwrap();
        let repo = Arc::new(
            Repo::open_plain_with_path_overrides(temp.path(), Default::default()).unwrap(),
        );

        let (first, second) = tokio::join!(repo.user_checkout(), repo.user_checkout());
        let first = first.unwrap();
        let second = second.unwrap();
        assert!(Arc::ptr_eq(&first, &second));

        let weak = Arc::downgrade(&first);
        drop(first);
        drop(second);
        assert!(weak.upgrade().is_none());

        let (first, second) = tokio::join!(repo.user_checkout(), repo.user_checkout());
        assert!(Arc::ptr_eq(&first.unwrap(), &second.unwrap()));
    }

    #[tokio::test]
    async fn creates_provenance_free_git_sandbox() {
        // Managed workspaces deliberately require the repository filesystem
        // to be bcachefs; this checkout is the test's bcachefs fixture.
        let temp = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        assert!(
            std::process::Command::new("jj")
                .current_dir(&repo)
                .args(["git", "init", "--no-colocate"])
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(repo.join("tracked"), "baseline").unwrap();
        assert!(
            std::process::Command::new("jj")
                .current_dir(&repo)
                .args(["commit", "-m", "source"])
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(repo.join("tracked"), "working copy").unwrap();
        std::fs::write(repo.join(".gitignore"), "ignored-secret\n").unwrap();
        std::fs::write(repo.join("ignored-secret"), "secret").unwrap();

        let repo = Arc::new(Repo::open(&repo).unwrap());
        let workspace = repo.create_sandbox("@").await.unwrap();
        assert_eq!(
            std::fs::read_to_string(workspace.checkout().join("tracked")).unwrap(),
            "working copy"
        );
        assert!(!workspace.checkout().join("ignored-secret").exists());
        assert!(workspace.checkout().join(".jj").is_dir());
        let sandbox_git = workspace.sandbox_base().unwrap().join("git");
        let commits = std::process::Command::new("git")
            .env("GIT_DIR", &sandbox_git)
            .env("GIT_WORK_TREE", workspace.checkout())
            .current_dir(workspace.checkout())
            .args(["rev-list", "--count", "HEAD"])
            .output()
            .unwrap();
        assert!(commits.status.success());
        assert_eq!(String::from_utf8(commits.stdout).unwrap().trim(), "1");
        assert!(
            std::process::Command::new("git")
                .env("GIT_DIR", &sandbox_git)
                .env("GIT_WORK_TREE", workspace.checkout())
                .current_dir(workspace.checkout())
                .args(["status", "--porcelain"])
                .output()
                .unwrap()
                .stdout
                .is_empty()
        );
        std::fs::remove_dir_all(workspace.sandbox_base().unwrap()).unwrap();
    }

    #[test]
    fn sandbox_paths_cannot_escape() {
        let root = std::path::Path::new("/sandbox");
        assert_eq!(
            sandbox_join(root, std::path::Path::new("src/../file")).unwrap(),
            std::path::Path::new("/sandbox/file")
        );
        assert!(sandbox_join(root, std::path::Path::new("../secret")).is_err());
    }

    fn set_mtime(path: &std::path::Path, time: SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(time))
            .unwrap();
    }

    #[test]
    fn copies_mtime_for_identical_files() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        std::fs::write(&source, "same").unwrap();
        std::fs::write(&target, "same").unwrap();
        let source_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let target_time = source_time + Duration::from_secs(100);
        set_mtime(&source, source_time);
        set_mtime(&target, target_time);

        copy_mtime_if_same(&source, &target).unwrap();

        assert_eq!(
            std::fs::metadata(target).unwrap().modified().unwrap(),
            source_time
        );
    }

    #[test]
    fn keeps_mtime_for_different_files() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        std::fs::write(&source, "source").unwrap();
        std::fs::write(&target, "target").unwrap();
        let source_time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let target_time = source_time + Duration::from_secs(100);
        set_mtime(&source, source_time);
        set_mtime(&target, target_time);

        copy_mtime_if_same(&source, &target).unwrap();

        assert_eq!(
            std::fs::metadata(target).unwrap().modified().unwrap(),
            target_time
        );
    }
}
