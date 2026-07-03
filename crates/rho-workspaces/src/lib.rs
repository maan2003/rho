//! Per-agent jj workspaces backed by the fork's workspace pool.
//!
//! Each agent works in a jj pool slot (`<repo>/.jj/ws-pool/N`) claimed with
//! `jj workspace add --pool`. The jj workspace *name* (the creating agent's
//! id) is the durable handle — the repo view's `wc_commit_ids[name]` follows
//! the agent's change across every operation — while the slot directory is
//! droppable cache that jj rebinds on attach. With namespaces available,
//! each agent's commands run in a private mount namespace where the slot is
//! mounted *over the origin repo path*, so the agent sees the real path:
//! informative context, working `../` relative references, and
//! absolute-path-keyed caches (cargo) stay valid.
//!
//! Requires the user's jj fork on PATH: every jj command snapshots all
//! workspaces in one transaction, the same change can be checked out in
//! multiple workspaces, and the workspace pool commands exist.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use senax_encoder::{Decode, Encode, Pack, Unpack};
use tokio::sync::{Mutex, OnceCell};

mod ns;

pub use ns::init_daemon_namespace;

/// Where an agent works, stored inline on the agent record. Self-contained:
/// there is no separate workspace table.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Encode, Decode, Pack, Unpack)]
pub enum WorkspaceInfo {
    /// The user's own checkout: the agent works directly at the repo path,
    /// no separate checkout and no namespace.
    UserCheckout { repo: String },
    /// A jj workspace named after the creating agent. It is checked out in
    /// a pool slot, but that is an implementation detail: the slot directory
    /// is queried from jj, never stored — it changes across detach/attach
    /// cycles.
    Workspace { repo: String, name: String },
}

impl WorkspaceInfo {
    pub fn repo(&self) -> &str {
        match self {
            Self::UserCheckout { repo } | Self::Workspace { repo, .. } => repo,
        }
    }

    pub fn is_user_checkout(&self) -> bool {
        matches!(self, Self::UserCheckout { .. })
    }
}

/// One jj repo the daemon works with — the crate's entry object.
/// Everything repo-scoped lives here: the jj invocation lock, the live
/// workspace instances, the once-per-daemon reap of stale attachments, and
/// the ws-parent pointer plumbing.
///
/// Hold exactly one instance per repo root (the daemon keeps a dedup map):
/// agents joining a workspace share one live [`Workspace`] — one checkout,
/// one mount namespace — only within one `Repo` instance.
#[derive(Debug)]
pub struct Repo {
    /// Canonicalized origin root; UTF-8 validated at open.
    root: String,
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
        let root = resolve_repo_root(path)?;
        let root = root
            .into_os_string()
            .into_string()
            .map_err(|root| anyhow::anyhow!("repo path is not valid UTF-8: {root:?}"))?;
        Ok(Self {
            root,
            jj_lock: Mutex::new(()),
            workspaces: Mutex::new(HashMap::new()),
            reaped: OnceCell::new(),
        })
    }

    /// Creates the jj workspace named `name` for a new agent in a pool slot
    /// with a new change on top of `parent_revset` (resolved against this
    /// repo, so `@` is the user's checkout and `<name>@` another
    /// workspace's). A background `pool prepare` keeps warm slots ready for
    /// later agents.
    pub async fn create_workspace(
        self: &Arc<Self>,
        name: &str,
        parent_revset: &str,
    ) -> anyhow::Result<Arc<Workspace>> {
        let slot = self.attach(name, Some(parent_revset)).await?;
        self.warm_pool();
        self.cache_workspace(self.workspace_info(name), slot).await
    }

    /// Opens an existing workspace, returning the live shared instance when
    /// one exists — agents in the same workspace share one checkout and one
    /// mount namespace. A pool workspace is (re)attached idempotently, so a
    /// workspace detached earlier is rematerialized into a fresh slot here.
    pub async fn open_workspace(self: &Arc<Self>, name: &str) -> anyhow::Result<Arc<Workspace>> {
        let info = self.workspace_info(name);
        if let Some(workspace) = self.workspaces.lock().await.get(&info) {
            return Ok(Arc::clone(workspace));
        }
        let slot = self.attach(name, None).await?;
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
        self.cache_workspace(info, PathBuf::from(&self.root)).await
    }

    /// The origin repo root (canonicalized).
    pub fn root(&self) -> &Path {
        Path::new(&self.root)
    }

    fn workspace_info(&self, name: &str) -> WorkspaceInfo {
        WorkspaceInfo::Workspace {
            repo: self.root.clone(),
            name: name.to_owned(),
        }
    }

    /// Checks the workspace out into a pool slot (creating the workspace
    /// first when `create_revset` is given) and returns the slot directory,
    /// with its repo pointers rewritten through ws-parent and its flake
    /// files' mtimes restored for direnv/nix fingerprints.
    async fn attach(&self, name: &str, create_revset: Option<&str>) -> anyhow::Result<PathBuf> {
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
        slot: PathBuf,
    ) -> anyhow::Result<Arc<Workspace>> {
        let workspace = Arc::new(Workspace {
            info: info.clone(),
            repo: Arc::clone(self),
            slot,
            mnt_ns: OnceCell::new(),
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
    async fn workspace_root(&self, name: &str) -> anyhow::Result<PathBuf> {
        let mut command = self.jj();
        command.args(["workspace", "root", "--name", name]);
        let stdout = run_jj(command).await.context("jj workspace root")?;
        let path = String::from_utf8(stdout).context("workspace root is not valid UTF-8")?;
        let path = PathBuf::from(path.trim());
        anyhow::ensure!(
            path.is_absolute() && path.is_dir(),
            "jj reported a bad workspace root: {}",
            path.display()
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
    fn rewrite_pointers(&self, slot: &Path) -> anyhow::Result<()> {
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
        std::fs::write(&jj_pointer, target.as_os_str().as_encoded_bytes())
            .with_context(|| format!("rewrite {}", jj_pointer.display()))?;

        // Git worktree slots get a `gitdir: <origin>/...` file; same
        // treatment. Replacement is a no-op when the pointer was already
        // rewritten.
        let git_pointer = slot.join(".git");
        if git_pointer.is_file() {
            let content =
                std::fs::read_to_string(&git_pointer).context("read git worktree pointer")?;
            let parent_str = parent
                .to_str()
                .expect("ws-parent path derives from UTF-8 root");
            std::fs::write(&git_pointer, content.replace(&self.root, parent_str))
                .context("rewrite git worktree pointer")?;
        }
        Ok(())
    }
}

/// One materialized workspace checkout. Callers share it via `Arc`; the
/// mount-namespace fd is created lazily on first use (a daemon restart needs
/// no recovery pass — the first spawned command pays the ~100µs setup once).
#[derive(Debug)]
pub struct Workspace {
    info: WorkspaceInfo,
    repo: Arc<Repo>,
    slot: PathBuf,
    mnt_ns: OnceCell<OwnedFd>,
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
    pub fn repo(&self) -> &Path {
        self.repo.root()
    }

    /// The checkout directory in the daemon/host namespace. In-process file
    /// operations (patches) and namespace-less fallback execution use this.
    pub fn slot(&self) -> &Path {
        &self.slot
    }

    /// The workspace's mount namespace, created on first call. The fd keeps
    /// the namespace alive; commands enter it with `setns` from `pre_exec`.
    ///
    /// Panics when namespace setup fails (including when
    /// [`init_daemon_namespace`] never ran): an agent whose real cwd
    /// contradicts the paths baked into its context would silently corrupt
    /// every later turn, so there is deliberately no degraded mode.
    /// User-checkout workspaces never call this — they have no namespace.
    pub async fn mnt_ns(&self) -> &OwnedFd {
        assert!(
            !self.is_user_checkout(),
            "user checkouts have no namespace"
        );
        self.mnt_ns
            .get_or_init(|| async {
                let repo = self.repo.root().to_owned();
                let slot = self.slot.clone();
                tokio::task::spawn_blocking(move || ns::create_workspace_ns(&repo, &slot))
                    .await
                    .expect("namespace task panicked")
                    .expect("workspace mount namespace setup failed")
            })
            .await
    }

    /// Commits the checkout's current file state into the repo (any jj
    /// command snapshots all workspaces under the fork). Called at turn
    /// boundaries so the user's own jj view follows the agent's work.
    pub async fn snapshot(&self) -> anyhow::Result<()> {
        let _guard = self.repo.jj_lock.lock().await;
        let mut command = self.repo.jj();
        command.args(["workspace", "list"]);
        run_jj(command).await.context("jj snapshot")?;
        Ok(())
    }
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

/// Repo roots must be absolute, existing jj repo roots. A path inside a
/// secondary workspace resolves to its origin repo via the `.jj/repo`
/// pointer.
pub fn resolve_repo_root(path: &Path) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        path.is_absolute(),
        "repo path must be absolute: {}",
        path.display()
    );
    let path = path
        .canonicalize()
        .with_context(|| format!("repo does not exist: {}", path.display()))?;
    anyhow::ensure!(
        path.join(".jj").is_dir(),
        "not a jj repository root: {}",
        path.display()
    );
    let pointer = path.join(".jj").join("repo");
    if pointer.is_file() {
        // A secondary workspace: the pointer names `<origin>/.jj/repo`.
        let target = PathBuf::from(
            std::fs::read_to_string(&pointer)
                .with_context(|| format!("read {}", pointer.display()))?
                .trim(),
        );
        let origin = target
            .parent()
            .and_then(Path::parent)
            .with_context(|| format!("malformed repo pointer in {}", pointer.display()))?
            .to_owned();
        anyhow::ensure!(
            origin.join(".jj").is_dir(),
            "workspace points at a missing repo: {}",
            origin.display()
        );
        return Ok(origin);
    }
    Ok(path)
}

/// The origin's `.jj/ws-parent -> ..` symlink (back to the repo root — the
/// target resolves relative to the `.jj` dir holding the link) that slot
/// pointers route through (see [`ns::WS_PARENT`]); created once per repo,
/// idempotent.
fn ensure_ws_parent_symlink(repo: &Path) -> anyhow::Result<()> {
    let link = repo.join(".jj").join(ns::WS_PARENT);
    match std::os::unix::fs::symlink("..", &link) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error).with_context(|| format!("create {}", link.display())),
    }
}

fn copy_mtime(source: PathBuf, target: PathBuf) -> std::io::Result<()> {
    let meta = std::fs::metadata(source)?;
    let times = std::fs::FileTimes::new()
        .set_accessed(meta.accessed()?)
        .set_modified(meta.modified()?);
    std::fs::File::options()
        .write(true)
        .open(target)?
        .set_times(times)
}
