//! Per-agent working sets of jj workspaces backed by the fork's workspace
//! pool.
//!
//! An agent's filesystem is a [`View`]: a working set of workdir entries,
//! fixed at spawn, each binding an origin path to a materialized
//! [`Workspace`] (a jj pool-slot checkout, the user's live checkout, or a
//! plain directory). Checkouts live in jj pool slots
//! (`<repo>/.jj/ws-pool/N`) claimed with `jj workspace add --pool`. The jj
//! workspace *name* (the workspace id's encoding) is the durable handle —
//! the repo view's `wc_commit_ids[name]` follows the agent's change across
//! every operation — while the slot directory is droppable cache that jj
//! rebinds on attach. With namespaces available, each agent's commands run
//! in a private per-view mount namespace where every entry's slot is
//! mounted *over its origin repo path*, so the agent sees the real paths:
//! informative context, working `../` relative references, and
//! absolute-path-keyed caches (cargo) stay valid.
//!
//! Requires the user's jj fork on PATH: every jj command snapshots all
//! workspaces in one transaction, the same change can be checked out in
//! multiple workspaces, and the workspace pool commands exist.

use std::collections::HashMap;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::os::fd::{BorrowedFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use prefix_id::{PrefixId, PrefixIdDomain};
use senax_encoder::{Decode, Encode, Pack, Unpack};
use tokio::sync::{Mutex, OnceCell};

mod ns;

pub use ns::init_daemon_namespace;

pub type WorkspaceId = PrefixId<WorkspaceIdDomain>;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathOverrides {
    pub before: Vec<PathBuf>,
    pub after: Vec<PathBuf>,
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

/// Keys workspace-id encoding with the daemon database's persisted machine
/// seed. The id is stored as part of [`WorkspaceInfo`] and its encoded form is
/// the jj workspace name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceIdDomain(pub u64);

impl PrefixIdDomain for WorkspaceIdDomain {
    const KIND: &'static str = "workspace-id";

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
    /// A jj workspace named after this workspace id's encoding. It is checked
    /// out in a pool slot, but that is an implementation detail: the slot
    /// directory is queried from jj, never stored — it changes across
    /// detach/attach cycles.
    Workspace {
        repo: Utf8PathBuf,
        #[senax(rename = "name")]
        id: WorkspaceId,
    },
}

impl WorkspaceInfo {
    pub fn repo(&self) -> &Utf8Path {
        match self {
            Self::UserCheckout { repo } | Self::Workspace { repo, .. } => repo,
        }
    }

    pub fn is_user_checkout(&self) -> bool {
        matches!(self, Self::UserCheckout { .. })
    }

    pub fn workspace_id(&self) -> Option<WorkspaceId> {
        match self {
            Self::UserCheckout { .. } => None,
            Self::Workspace { id, .. } => Some(*id),
        }
    }

    pub fn workspace_name(&self) -> Option<String> {
        self.workspace_id().map(|id| id.encoded())
    }
}

/// One jj repo the daemon works with — the crate's entry object.
/// Everything repo-scoped lives here: the jj invocation lock, the live
/// workspace instances, the once-per-daemon reap of stale attachments, and
/// the ws-parent pointer plumbing.
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
    /// Serializes jj invocations. jj's op log makes concurrent commands safe
    /// on its own; this is simplicity insurance while the fork's
    /// all-workspace snapshot path is young.
    jj_lock: Mutex<()>,
    /// One live instance per workspace, so agents joining a workspace share
    /// its checkout, mount namespace, and lazy setup.
    workspaces: Mutex<HashMap<WorkspaceInfo, Arc<Workspace>>>,
    /// Set once the stale pool attachments from a previous daemon have been
    /// released. Attachment is a runtime property: whatever a dead daemon
    /// left attached is detached — snapshotting first, so mid-turn work from
    /// a crash lands in its workspace's commit — before this daemon's first
    /// attach in the repo.
    reaped: OnceCell<()>,
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
            jj_lock: Mutex::new(()),
            workspaces: Mutex::new(HashMap::new()),
            reaped: OnceCell::new(),
        })
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
            jj_lock: Mutex::new(()),
            workspaces: Mutex::new(HashMap::new()),
            reaped: OnceCell::new(),
        })
    }

    /// Whether this workdir is a jj repo (as opposed to a plain live
    /// directory).
    pub fn is_jj(&self) -> bool {
        self.is_jj
    }

    /// Creates the jj workspace named after `id`'s encoding for a new agent
    /// in a pool slot with a new change on top of `parent_revset` (resolved
    /// against this repo, so `@` is the user's checkout and `<id>@` another
    /// workspace's). A background `pool prepare` keeps warm slots ready for
    /// later agents.
    pub async fn create_workspace(
        self: &Arc<Self>,
        id: WorkspaceId,
        parent_revset: &str,
    ) -> anyhow::Result<Arc<Workspace>> {
        anyhow::ensure!(self.is_jj, "not a jj repository: {}", self.root);
        let name = id.encoded();
        let slot = self.attach(&name, Some(parent_revset)).await?;
        self.warm_pool();
        self.cache_workspace(self.workspace_info(id), slot).await
    }

    /// Opens an existing workspace, returning the live shared instance when
    /// one exists — agents in the same workspace share one checkout and one
    /// mount namespace. A pool workspace is (re)attached idempotently, so a
    /// workspace detached earlier is rematerialized into a fresh slot here.
    pub async fn open_workspace(
        self: &Arc<Self>,
        id: WorkspaceId,
    ) -> anyhow::Result<Arc<Workspace>> {
        anyhow::ensure!(self.is_jj, "not a jj repository: {}", self.root);
        let info = self.workspace_info(id);
        if let Some(workspace) = self.workspaces.lock().await.get(&info) {
            return Ok(Arc::clone(workspace));
        }
        let name = id.encoded();
        let slot = self.attach(&name, None).await?;
        self.cache_workspace(info, slot).await
    }

    /// A workspace standing for the user's own checkout: the agent works
    /// directly at the repo path with no separate checkout and no namespace.
    pub async fn user_checkout(self: &Arc<Self>) -> anyhow::Result<Arc<Workspace>> {
        let info = WorkspaceInfo::UserCheckout {
            repo: self.root.clone(),
        };
        if let Some(workspace) = self.workspaces.lock().await.get(&info) {
            return Ok(Arc::clone(workspace));
        }
        self.cache_workspace(info, self.root.clone()).await
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

    /// Checks the workspace out into a pool slot (creating the workspace
    /// first when `create_revset` is given) and returns the slot directory,
    /// with its repo pointers rewritten through ws-parent and its flake
    /// files' mtimes restored for direnv/nix fingerprints.
    async fn attach(&self, name: &str, create_revset: Option<&str>) -> anyhow::Result<Utf8PathBuf> {
        let _guard = self.jj_lock.lock().await;
        self.ensure_reaped().await?;
        let mut command = self.jj();
        match create_revset {
            Some(revset) => {
                command
                    .args(["workspace", "add", "--pool", "--name", name])
                    .args(["--revision", revset]);
            }
            None => {
                command.args(["workspace", "pool", "attach", name]);
            }
        }
        run_jj(command).await.context("jj workspace attach")?;
        let slot = self.workspace_root(name).await?;
        self.rewrite_pointers(&slot)?;
        for filename in ["flake.nix", "flake.lock", ".envrc"] {
            let _ = copy_mtime(self.root().join(filename), slot.join(filename));
        }
        Ok(slot)
    }

    async fn cache_workspace(
        self: &Arc<Self>,
        info: WorkspaceInfo,
        slot: Utf8PathBuf,
    ) -> anyhow::Result<Arc<Workspace>> {
        let workspace = Arc::new(Workspace {
            info: info.clone(),
            repo: Arc::clone(self),
            slot,
            context_config: OnceLock::new(),
        });
        self.workspaces
            .lock()
            .await
            .insert(info, Arc::clone(&workspace));
        Ok(workspace)
    }

    /// A jj command running against this repo.
    fn jj(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("jj");
        command.arg("--repository").arg(&self.root);
        command
    }

    /// Once per daemon lifetime, detaches every pool-attached workspace (one
    /// `jj workspace pool detach --all`) — jj knows which workspaces occupy
    /// pool slots; rho doesn't enumerate anything. Runs under the jj lock,
    /// before this daemon's first attach in the repo — later attaches by
    /// this daemon must not be reaped.
    async fn ensure_reaped(&self) -> anyhow::Result<()> {
        self.reaped
            .get_or_try_init(|| async {
                let mut command = self.jj();
                command.args(["workspace", "pool", "detach", "--all"]);
                run_jj(command)
                    .await
                    .context("jj workspace pool detach --all")?;
                anyhow::Ok(())
            })
            .await?;
        Ok(())
    }

    /// Fire-and-forget `jj workspace pool prepare`: keep a few warm free
    /// slots (seeded with the default workspace's ignored files) ready for
    /// the next agents.
    fn warm_pool(self: &Arc<Self>) {
        let repo = Arc::clone(self);
        tokio::spawn(async move {
            let _guard = repo.jj_lock.lock().await;
            let mut command = repo.jj();
            command.args(["workspace", "pool", "prepare", "--count", "4"]);
            if let Err(error) = run_jj(command).await {
                eprintln!(
                    "rho-workspaces: pool prepare for {} failed: {error:#}",
                    repo.root
                );
            }
        });
    }

    /// The slot directory a workspace is currently attached at, per jj.
    async fn workspace_root(&self, name: &str) -> anyhow::Result<Utf8PathBuf> {
        let mut command = self.jj();
        command.args(["workspace", "root", "--name", name]);
        let stdout = run_jj(command).await.context("jj workspace root")?;
        let path = String::from_utf8(stdout).context("workspace root is not valid UTF-8")?;
        let path = Utf8PathBuf::from(path.trim());
        anyhow::ensure!(
            path.is_absolute() && path.is_dir(),
            "jj reported a bad workspace root: {path}",
        );
        Ok(path)
    }

    /// Redirects the slot's back-references to the origin repo through
    /// `<origin>/.jj/ws-parent`, which resolves in every namespace (the
    /// origin path itself is covered by the slot mount inside the agent's
    /// namespace). Idempotent; runs after every attach since jj recreates
    /// the `.git` worktree pointer each time. Also ensures the origin's
    /// ws-parent symlink, which must exist before the rewritten pointer is
    /// ever read.
    fn rewrite_pointers(&self, slot: &Utf8Path) -> anyhow::Result<()> {
        ensure_ws_parent_symlink(self.root())?;
        let parent = self.root().join(".jj").join(ns::WS_PARENT);
        // The slot-side directory the agent namespace binds the origin onto.
        match std::fs::create_dir(slot.join(".jj").join(ns::WS_PARENT)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("create slot ws-parent dir"),
        }

        let jj_pointer = slot.join(".jj").join("repo");
        let target = parent.join(".jj").join("repo");
        std::fs::write(&jj_pointer, target.as_str())
            .with_context(|| format!("rewrite {jj_pointer}"))?;

        // Git worktree slots get a `gitdir: <origin>/...` file; same
        // treatment. Replacement is a no-op when the pointer was already
        // rewritten.
        let git_pointer = slot.join(".git");
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
    slot: Utf8PathBuf,
    context_config: OnceLock<Arc<rho_context_config::DiscoveredContext>>,
}

impl Workspace {
    pub fn info(&self) -> &WorkspaceInfo {
        &self.info
    }

    pub fn is_user_checkout(&self) -> bool {
        self.info.is_user_checkout()
    }

    /// The origin repo root — the path agents see and the system prompt
    /// reports (the slot is mounted over it in the agent's namespace).
    pub fn repo(&self) -> &Utf8Path {
        self.repo.root()
    }

    pub fn discovered_context(&self) -> Arc<rho_context_config::DiscoveredContext> {
        Arc::clone(self.context_config.get_or_init(|| {
            Arc::new(rho_context_config::DiscoveredContext::discover(
                self.repo(),
                self.slot(),
            ))
        }))
    }

    /// The checkout directory in the daemon/host namespace. In-process file
    /// operations (patches) and namespace-less fallback execution use this.
    pub fn slot(&self) -> &Utf8Path {
        &self.slot
    }

    /// Whether commands for this workspace need a cover mount (the slot is
    /// not already the origin path).
    fn needs_mount(&self) -> bool {
        !self.is_user_checkout()
    }

    /// Commits the checkout's current file state into the repo (any jj
    /// command snapshots all workspaces under the fork). Called at turn
    /// boundaries so the user's own jj view follows the agent's work.
    /// A no-op for plain (non-jj) directory workdirs.
    pub async fn snapshot(&self) -> anyhow::Result<()> {
        if !self.repo.is_jj {
            return Ok(());
        }
        let _guard = self.repo.jj_lock.lock().await;
        let mut command = self.repo.jj();
        command.args(["workspace", "list"]);
        run_jj(command).await.context("jj snapshot")?;
        Ok(())
    }
}

/// One agent's view of the filesystem: a working set of workdir entries
/// fixed at construction, each binding an origin path to a materialized
/// [`Workspace`], realized as a private mount namespace with each entry's
/// slot mounted over its origin path. Entry 0 is the primary workdir: the
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
/// cover mounts (a parent mounted after a child hides the child's slot).
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
        command.env(
            "PATH",
            primary
                .repo
                .path_overrides()
                .add_to(&std::env::var_os("PATH").expect("PATH must be set")),
        );
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
            command.pre_exec(move || enter_workspace_ns(ns_fd, &cwd, &mounts));
        }
        Ok(())
    }

    /// Maps a path in the agent-visible view (origin paths) to the host path
    /// where the bytes actually live (slot checkouts), for in-process file
    /// operations that do not enter the namespace. Paths outside every entry
    /// are returned unchanged.
    pub fn resolve_host_path(&self, path: &Path) -> PathBuf {
        for entry in self.entries() {
            if let Ok(rel) = path.strip_prefix(entry.repo().as_std_path()) {
                return entry.slot().as_std_path().join(rel);
            }
        }
        path.to_owned()
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
            .map(|entry| {
                (
                    entry.repo().as_std_path().to_owned(),
                    entry.slot().as_std_path().to_owned(),
                )
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
/// target resolves relative to the `.jj` dir holding the link) that slot
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

fn copy_mtime(source: impl AsRef<Path>, target: impl AsRef<Path>) -> std::io::Result<()> {
    let meta = std::fs::metadata(source)?;
    let times = std::fs::FileTimes::new()
        .set_accessed(meta.accessed()?)
        .set_modified(meta.modified()?);
    std::fs::File::options()
        .write(true)
        .open(target)?
        .set_times(times)
}
