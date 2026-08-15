//! Daemon-owned clone-store storage root and per-workset workspace collections.
//!
//! Every workspace has the same relative layout in the host frame and in a
//! workset's `/src` view, so jj and Git pointers need no namespace-specific
//! rewriting.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::fs::symlink;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use anyhow::Context as _;
use camino::{Utf8Path, Utf8PathBuf};
use redb::TableDefinition;
use rho_db::{RhoDb, Sen, SenValue};
use senax_encoder::{Decode, Encode};
use tokio::sync::Mutex;

mod ns;

pub mod layout;

pub use layout::*;
pub use ns::{Mode, Namespace};
pub use rho_workset_types::{
    WorkspaceDiffBaseContent, WorkspaceDiffContent, WorkspaceDiffFile, WorkspaceDiffSnapshot,
    WorkspaceDiffStatus, WorkspaceDiffTarget, WorkspaceInfo,
};

const WORKSET_RECORDS: TableDefinition<Sen<String>, Sen<WorksetRecord>> =
    TableDefinition::new("worksets");

/// Durable Workset ordering. `primary` is stored explicitly rather than
/// inferred from map or directory iteration; `checkouts` records creation
/// order and is append-only.
#[derive(Clone, Debug, Encode, Decode)]
struct WorksetRecord {
    primary: Option<String>,
    checkouts: Vec<String>,
}

impl Default for WorksetRecord {
    fn default() -> Self {
        Self {
            primary: None,
            checkouts: Vec::new(),
        }
    }
}

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

    fn values(&self) -> Vec<(OsString, OsString)> {
        self.0.iter().cloned().collect()
    }
}

/// The daemon-wide owner of one clone-store storage root.
#[derive(Debug)]
pub struct Worksets {
    root: Utf8PathBuf,
    db: RhoDb,
    environment: UserEnvironment,
    path_overrides: PathOverrides,
    stores: Mutex<BTreeMap<String, Weak<Store>>>,
    worksets: Mutex<BTreeMap<String, Weak<WorksetInner>>>,
}

impl Worksets {
    pub fn open(
        root: impl AsRef<Path>,
        db: RhoDb,
        environment: UserEnvironment,
        path_overrides: PathOverrides,
    ) -> anyhow::Result<Arc<Self>> {
        let root = absolute_utf8(root.as_ref())?;
        std::fs::create_dir_all(root.join("stores"))
            .with_context(|| format!("create clone-store storage root at {root}"))?;
        std::fs::create_dir_all(root.join("worksets"))
            .with_context(|| format!("create workset storage root at {root}"))?;
        Ok(Arc::new(Self {
            root,
            db,
            environment,
            path_overrides,
            stores: Mutex::new(BTreeMap::new()),
            worksets: Mutex::new(BTreeMap::new()),
        }))
    }

    pub fn open_default(
        db: RhoDb,
        environment: UserEnvironment,
        path_overrides: PathOverrides,
    ) -> anyhow::Result<Arc<Self>> {
        let home = dirs::home_dir().context("HOME directory is unavailable")?;
        Self::open(home.join("src/.rho"), db, environment, path_overrides)
    }

    fn load_record(&self, id: &str) -> anyhow::Result<WorksetRecord> {
        let read = self.db.read();
        anyhow::ensure!(read.has_table("worksets"), "workset does not exist: {id}");
        let table = read.open_table(WORKSET_RECORDS);
        let key = id.to_owned();
        table
            .get(SenValue::borrowed(&key))
            .map(|record| record.value().into_owned())
            .with_context(|| format!("workset does not exist: {id}"))
    }

    async fn save_record(&self, id: &str, record: &WorksetRecord) {
        let mut write = self.db.write().await;
        let key = id.to_owned();
        write
            .open_table(WORKSET_RECORDS)
            .insert(SenValue::borrowed(&key), SenValue::borrowed(record));
        write.commit();
    }

    pub fn root(&self) -> &Utf8Path {
        &self.root
    }

    /// Gets or crash-safely initializes a named clone store.
    async fn store(self: &Arc<Self>, name: &str, remote_url: &str) -> anyhow::Result<Arc<Store>> {
        validate_name(name)?;
        let mut stores = self.stores.lock().await;
        if let Some(store) = stores.get(name).and_then(Weak::upgrade) {
            anyhow::ensure!(
                store.remote_url == remote_url,
                "store {name} already uses a different remote"
            );
            return Ok(store);
        }
        let root = self.root.join("stores").join(name);
        if !root.join("clone-store").is_file() {
            let mut command = self.command("jj");
            command.args(["store", "init"]).arg(&root).arg(remote_url);
            run(command, "initialize clone store").await?;
        }
        anyhow::ensure!(
            root.join("clone-store").is_file(),
            "clone store is incomplete: {root}"
        );
        let store = Arc::new(Store {
            name: name.to_owned(),
            root,
            remote_url: remote_url.to_owned(),
            operation_lock: Mutex::new(()),
        });
        stores.insert(name.to_owned(), Arc::downgrade(&store));
        Ok(store)
    }

    pub async fn create(self: &Arc<Self>) -> anyhow::Result<Workset> {
        for _ in 0..64 {
            let workset_id = random_workset_id()?;
            let base = self.root.join("worksets").join(&workset_id);
            match std::fs::create_dir(&base) {
                Ok(()) => {
                    std::fs::create_dir_all(base.join("src/.stores"))
                        .with_context(|| format!("create workset {workset_id}"))?;
                    self.save_record(&workset_id, &WorksetRecord::default())
                        .await;
                    return self.load_workset(&workset_id, true).await;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("allocate workset directory"),
            }
        }
        anyhow::bail!("could not allocate a unique workset id")
    }

    pub async fn open_workset(self: &Arc<Self>, workset_id: &str) -> anyhow::Result<Workset> {
        self.load_workset(workset_id, false).await
    }

    async fn load_workset(
        self: &Arc<Self>,
        workset_id: &str,
        newly_created: bool,
    ) -> anyhow::Result<Workset> {
        validate_name(workset_id)?;
        if let Some(workset) = self
            .worksets
            .lock()
            .await
            .get(workset_id)
            .and_then(Weak::upgrade)
        {
            return Ok(Workset(workset));
        }
        let root = self.root.join("worksets").join(workset_id).join("src");
        anyhow::ensure!(
            newly_created || root.join(".stores").is_dir(),
            "workset does not exist: {workset_id}"
        );
        let operation_lock = Arc::new(Mutex::new(()));
        let record = self.load_record(workset_id)?;
        let workset = Arc::new(WorksetInner {
            id: workset_id.to_owned(),
            root: root.clone(),
            owner: Arc::downgrade(self),
            stores: Mutex::new(BTreeMap::new()),
            workspaces: Mutex::new(BTreeMap::new()),
            record: Mutex::new(record),
            operation_lock: Arc::clone(&operation_lock),
        });
        if !newly_created {
            let mut granted = BTreeMap::new();
            for entry in std::fs::read_dir(root.join(".stores"))? {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("store name is not UTF-8"))?;
                granted.insert(name.clone(), self.existing_store(&name).await?);
            }
            let mut workspaces = BTreeMap::new();
            let checkout_names = workset.record.lock().await.checkouts.clone();
            for name in checkout_names {
                let checkout_path = root.join(&name);
                if !checkout_path.is_dir() {
                    continue;
                }
                let pointer = std::fs::read_to_string(checkout_path.join(".jj/repo"))
                    .with_context(|| format!("read workspace pointer for {name}"))?;
                let store = granted
                    .values()
                    .find(|store| pointer.contains(&format!(".stores/{}/", store.name)))
                    .with_context(|| format!("workspace {name} references an ungranted store"))?;
                workspaces.insert(
                    name.clone(),
                    Arc::new(self.checkout(
                        workset_id,
                        Arc::clone(store),
                        name,
                        checkout_path,
                        Arc::clone(&operation_lock),
                    )),
                );
            }
            *workset.stores.lock().await = granted;
            *workset.workspaces.lock().await = workspaces;
        }
        self.worksets
            .lock()
            .await
            .insert(workset_id.to_owned(), Arc::downgrade(&workset));
        Ok(Workset(workset))
    }

    async fn existing_store(self: &Arc<Self>, name: &str) -> anyhow::Result<Arc<Store>> {
        validate_name(name)?;
        if let Some(store) = self.stores.lock().await.get(name).and_then(Weak::upgrade) {
            return Ok(store);
        }
        let root = self.root.join("stores").join(name);
        anyhow::ensure!(
            root.join("clone-store").is_file(),
            "store does not exist: {name}"
        );
        let mut command = self.command("git");
        command.arg("--git-dir").arg(root.join("git")).args([
            "config",
            "--get",
            "remote.origin.url",
        ]);
        let remote_url = String::from_utf8(output(command, "read store remote").await?.stdout)?
            .trim()
            .to_owned();
        let store = Arc::new(Store {
            name: name.to_owned(),
            root,
            remote_url,
            operation_lock: Mutex::new(()),
        });
        self.stores
            .lock()
            .await
            .insert(name.to_owned(), Arc::downgrade(&store));
        Ok(store)
    }

    fn checkout(
        &self,
        workset_id: &str,
        store: Arc<Store>,
        name: String,
        checkout: Utf8PathBuf,
        command_lock: Arc<Mutex<()>>,
    ) -> Checkout {
        Checkout {
            info: WorkspaceInfo::Checkout {
                workset: workset_id.to_owned(),
                repo: store.name.clone(),
                name: name.clone(),
            },
            name,
            checkout,
            store,
            environment: self.environment.clone(),
            path_overrides: self.path_overrides.clone(),
            command_lock,
            context_config: OnceLock::new(),
        }
    }

    pub fn delete_workset(&self, workset_id: &str) -> anyhow::Result<()> {
        validate_name(workset_id)?;
        let root = self.root.join("worksets").join(workset_id);
        if root.exists() {
            std::fs::remove_dir_all(&root).with_context(|| format!("delete workset {root}"))?;
        }
        Ok(())
    }

    fn command(&self, program: &str) -> tokio::process::Command {
        let executable = if program == "jj" {
            self.environment
                .get("RHO_JJ")
                .unwrap_or_else(|| OsStr::new(program))
        } else {
            OsStr::new(program)
        };
        let mut command = tokio::process::Command::new(executable);
        if program == "jj" {
            command.args(["--config", "git.write-change-id-header=true"]);
        }
        self.environment.apply(&mut command);
        if let Some(path) = self.environment.get("PATH") {
            command.env("PATH", self.path_overrides.add_to(path));
        }
        command
    }
}

#[derive(Debug)]
struct Store {
    name: String,
    root: Utf8PathBuf,
    remote_url: String,
    operation_lock: Mutex<()>,
}

impl Store {
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// A handle to one workset's host-frame `src` directory.
#[derive(Debug)]
pub struct Workset(Arc<WorksetInner>);

impl Workset {
    pub fn id(&self) -> &str {
        self.0.id()
    }

    pub fn root(&self) -> &Utf8Path {
        self.0.root()
    }

    pub async fn enter(&self, mode: Mode) -> anyhow::Result<Arc<Namespace>> {
        Namespace::create(Self(Arc::clone(&self.0)), mode).await
    }

    pub async fn clone(
        &self,
        repo: &str,
        remote_url: &str,
        name: Option<&str>,
        at: Option<&str>,
    ) -> anyhow::Result<Arc<Checkout>> {
        self.0.as_ref().clone(repo, remote_url, name, at).await
    }

    pub async fn fork_from(
        &self,
        parent: &Checkout,
        name: Option<&str>,
    ) -> anyhow::Result<Arc<Checkout>> {
        self.0.fork_from(parent, name).await
    }

    pub async fn checkout(&self, name: &str) -> Option<Arc<Checkout>> {
        self.0.checkout(name).await
    }

    pub(crate) fn owner(&self) -> anyhow::Result<Arc<Worksets>> {
        self.0
            .owner
            .upgrade()
            .context("worksets manager was dropped")
    }

    pub(crate) async fn checkouts(&self) -> Vec<Arc<Checkout>> {
        self.0.ordered_checkouts().await
    }

    pub async fn primary_name(&self) -> anyhow::Result<String> {
        self.0
            .record
            .lock()
            .await
            .primary
            .clone()
            .context("workset has no primary checkout")
    }

    pub async fn checkout_names(&self) -> Vec<String> {
        self.0.record.lock().await.checkouts.clone()
    }

    pub(crate) async fn mounts(&self) -> layout::Mounts {
        self.0.mounts().await
    }
}

#[derive(Debug)]
struct WorksetInner {
    id: String,
    root: Utf8PathBuf,
    pub(crate) owner: Weak<Worksets>,
    stores: Mutex<BTreeMap<String, Arc<Store>>>,
    pub(crate) workspaces: Mutex<BTreeMap<String, Arc<Checkout>>>,
    record: Mutex<WorksetRecord>,
    operation_lock: Arc<Mutex<()>>,
}

impl WorksetInner {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn root(&self) -> &Utf8Path {
        &self.root
    }

    async fn grant(&self, store: Arc<Store>) -> anyhow::Result<()> {
        let _agent_guard = self.operation_lock.lock().await;
        let _store_guard = store.operation_lock.lock().await;
        let owner = self
            .owner
            .upgrade()
            .context("worksets manager was dropped")?;
        let clone = store.root.join("clones").join(&self.id);
        if !clone.join("repo").is_dir() {
            let mut command = owner.command("jj");
            command
                .args(["store", "clone"])
                .arg(&store.root)
                .arg(&self.id);
            run(command, "create workset clone").await?;
        }
        let link = self.root.join(".stores").join(&store.name);
        if !link.exists() {
            let incoming = self.root.join(".stores").join(format!(
                ".incoming-{}-{}",
                store.name,
                std::process::id()
            ));
            let _ = std::fs::remove_file(&incoming);
            symlink(Path::new("../../../../stores").join(&store.name), &incoming)
                .with_context(|| format!("create store link {incoming}"))?;
            match std::fs::rename(&incoming, &link) {
                Ok(()) => {}
                Err(error) if link.exists() => {
                    let _ = std::fs::remove_file(&incoming);
                    let _ = error;
                }
                Err(error) => return Err(error).context("install workset store link"),
            }
        }
        let target =
            std::fs::read_link(&link).with_context(|| format!("read store link {link}"))?;
        anyhow::ensure!(
            target == Path::new("../../../../stores").join(&store.name),
            "workset store link points somewhere unexpected: {link}"
        );
        self.stores
            .lock()
            .await
            .insert(store.name.clone(), Arc::clone(&store));
        Ok(())
    }

    async fn create_workspace(
        &self,
        store: Arc<Store>,
        name: &str,
        at: Option<&str>,
    ) -> anyhow::Result<Arc<Checkout>> {
        validate_name(name)?;
        self.grant(Arc::clone(&store)).await?;
        let _guard = self.operation_lock.lock().await;
        if let Some(workspace) = self.workspaces.lock().await.get(name).cloned() {
            anyhow::ensure!(
                workspace.store.name == store.name,
                "workspace {name} already belongs to store {}",
                workspace.store.name
            );
            return Ok(workspace);
        }
        let checkout = self.root.join(name);
        let owner = self
            .owner
            .upgrade()
            .context("worksets manager was dropped")?;
        let mut command = owner.command("jj");
        command
            .args(["store", "workspace"])
            .arg(self.root.join(".stores").join(&store.name))
            .arg(&self.id)
            .arg(&checkout)
            .args(["--name", name]);
        if let Err(error) = run(command, "create workset workspace").await {
            if !checkout.exists() {
                return Err(error);
            }
            self.discard_workspace(&owner, &store, &checkout)
                .await
                .with_context(|| format!("{error:#}; clean unrecorded workspace before retry"))?;
            let mut retry = owner.command("jj");
            retry
                .args(["store", "workspace"])
                .arg(self.root.join(".stores").join(&store.name))
                .arg(&self.id)
                .arg(&checkout)
                .args(["--name", name]);
            run(retry, "retry creation of cleaned unrecorded workspace").await?;
        }
        if let Some(at) = at {
            let mut new = owner.command("jj");
            new.current_dir(&checkout).args(["new", at]);
            if let Err(error) = run(new, "start checkout at requested revset").await {
                return match self.discard_workspace(&owner, &store, &checkout).await {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(anyhow::anyhow!(
                        "{error:#}; cleanup after failed checkout initialization also failed: {cleanup:#}"
                    )),
                };
            }
        }
        {
            let mut record = self.record.lock().await;
            let mut updated = record.clone();
            if !updated.checkouts.iter().any(|checkout| checkout == name) {
                updated.checkouts.push(name.to_owned());
                if updated.primary.is_none() {
                    updated.primary = Some(name.to_owned());
                }
                owner.save_record(&self.id, &updated).await;
                *record = updated;
            }
        }
        let workspace = Arc::new(owner.checkout(
            &self.id,
            store,
            name.to_owned(),
            checkout,
            Arc::clone(&self.operation_lock),
        ));
        self.workspaces
            .lock()
            .await
            .insert(name.to_owned(), Arc::clone(&workspace));
        Ok(workspace)
    }

    async fn discard_workspace(
        &self,
        owner: &Worksets,
        store: &Store,
        checkout: &Utf8Path,
    ) -> anyhow::Result<()> {
        let mut forget = owner.command("jj");
        forget.current_dir(checkout).args(["workspace", "forget"]);
        let forget_result = run(forget, "forget failed workset workspace").await;

        let mut remove = owner.command("git");
        remove
            .arg("--git-dir")
            .arg(store.root.join("clones").join(&self.id).join("git"))
            .args(["worktree", "remove", "--force"])
            .arg(checkout);
        let remove_result = run(remove, "remove failed workset Git worktree").await;
        let directory_result = if checkout.exists() {
            std::fs::remove_dir_all(checkout)
                .with_context(|| format!("remove failed workset checkout directory {checkout}"))
        } else {
            Ok(())
        };
        if forget_result.is_ok() {
            directory_result
        } else {
            forget_result.and(remove_result).and(directory_result)
        }
    }

    /// Ensures the shared store and this workset's private clone, then
    /// materializes one checkout. The checkout name defaults to the repo name.
    /// When `at` is supplied, the checkout starts a fresh change whose parent
    /// is that revset; resolution failure is returned without a trunk fallback.
    pub async fn clone(
        &self,
        repo: &str,
        remote_url: &str,
        name: Option<&str>,
        at: Option<&str>,
    ) -> anyhow::Result<Arc<Checkout>> {
        let owner = self
            .owner
            .upgrade()
            .context("worksets manager was dropped")?;
        let store = owner.store(repo, remote_url).await?;
        self.create_workspace(store, name.unwrap_or(repo), at).await
    }

    /// Forks the parent's exact current working-copy commit into this workset's
    /// private clone and creates a workspace with the same files/change ids.
    pub async fn fork_from(
        &self,
        parent: &Checkout,
        name: Option<&str>,
    ) -> anyhow::Result<Arc<Checkout>> {
        let name = name.unwrap_or(parent.name());
        parent.snapshot().await?;
        let (sha, source_change_id) = parent.current_identity().await?;
        let store = Arc::clone(&parent.store);
        self.grant(Arc::clone(&store)).await?;
        let owner = self
            .owner
            .upgrade()
            .context("worksets manager was dropped")?;
        let child_git = store.root.join("clones").join(&self.id).join("git");
        let parent_pointer = std::fs::read_to_string(parent.checkout.join(".git"))
            .context("read parent Git worktree pointer")?;
        let parent_admin = parent_pointer
            .strip_prefix("gitdir:")
            .map(str::trim)
            .context("parent workspace has an invalid .git pointer")?;
        let parent_git = std::path::absolute(parent.checkout.join(parent_admin))?
            .parent()
            .and_then(Path::parent)
            .context("parent Git worktree pointer has no clone Git directory")?
            .to_owned();

        let mut fetch = owner.command("git");
        let transfer_ref = format!("refs/heads/rho-fork-{}", &sha[..12]);
        fetch
            .arg("--git-dir")
            .arg(&child_git)
            .args(["fetch", "--no-tags"])
            .arg(&parent_git)
            .arg(format!("{sha}:{transfer_ref}"));
        run(fetch, "transfer parent private commit").await?;

        // `jj git import` needs a workspace (the clone's bare repo directory
        // is not itself a CLI workspace), so materialize the target name at
        // trunk, import through it, then move that workspace to the exact
        // transferred commit. The transferred snapshot retains its change id;
        // the child starts a fresh change above it so two clones never co-edit
        // one logical change and later diverge.
        let workspace = self.create_workspace(store, name, None).await?;
        let mut import = owner.command("jj");
        import
            .current_dir(&workspace.checkout)
            .args(["git", "import"]);
        run(import, "import forked commit").await?;
        let mut new = owner.command("jj");
        new.current_dir(&workspace.checkout).args(["new", &sha]);
        run(new, "start child change from forked commit").await?;
        let imported_change_id = workspace.change_id("@-").await?;
        anyhow::ensure!(
            imported_change_id == source_change_id,
            "fork import changed the source change id"
        );
        let mut cleanup = owner.command("git");
        cleanup
            .arg("--git-dir")
            .arg(&child_git)
            .args(["update-ref", "-d", &transfer_ref]);
        run(cleanup, "remove fork transfer ref").await?;
        Ok(workspace)
    }

    pub async fn checkout(&self, name: &str) -> Option<Arc<Checkout>> {
        self.workspaces.lock().await.get(name).cloned()
    }

    async fn ordered_checkouts(&self) -> Vec<Arc<Checkout>> {
        let order = self.record.lock().await.checkouts.clone();
        let workspaces = self.workspaces.lock().await;
        order
            .iter()
            .filter_map(|name| workspaces.get(name).cloned())
            .collect()
    }

    pub async fn mounts(&self) -> layout::Mounts {
        let stores = self.stores.lock().await;
        let workspaces = self.ordered_checkouts().await;
        layout::Mounts {
            stores: stores
                .values()
                .map(|store| layout::StoreMount {
                    name: store.name.clone(),
                    source: store.root.as_std_path().to_owned(),
                    writable_clone: self.id.clone(),
                })
                .collect(),
            workspaces: workspaces
                .iter()
                .map(|workspace| layout::WorkspaceMount {
                    name: workspace.name.clone(),
                    source: workspace.checkout.as_std_path().to_owned(),
                })
                .collect(),
        }
    }
}

#[derive(Debug)]
pub struct Checkout {
    info: WorkspaceInfo,
    name: String,
    checkout: Utf8PathBuf,
    store: Arc<Store>,
    environment: UserEnvironment,
    path_overrides: PathOverrides,
    command_lock: Arc<Mutex<()>>,
    context_config: OnceLock<Arc<rho_context_config::DiscoveredContext>>,
}

impl Checkout {
    pub fn info(&self) -> &WorkspaceInfo {
        &self.info
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn repo(&self) -> &str {
        self.store.name()
    }

    pub fn checkout(&self) -> &Utf8Path {
        &self.checkout
    }

    /// The stable workset-visible path.
    pub fn visible_path(&self) -> Utf8PathBuf {
        Utf8PathBuf::from("/src").join(&self.name)
    }

    pub fn is_user_checkout(&self) -> bool {
        false
    }

    pub fn is_sandbox(&self) -> bool {
        self.info.is_sandbox()
    }

    pub fn discovered_context(&self) -> Arc<rho_context_config::DiscoveredContext> {
        Arc::clone(self.context_config.get_or_init(|| {
            Arc::new(rho_context_config::DiscoveredContext::discover(
                &self.checkout,
                &self.checkout,
            ))
        }))
    }

    fn command(&self, program: &str) -> tokio::process::Command {
        let executable = if program == "jj" {
            self.environment
                .get("RHO_JJ")
                .unwrap_or_else(|| OsStr::new(program))
        } else {
            OsStr::new(program)
        };
        let mut command = tokio::process::Command::new(executable);
        if program == "jj" {
            command.args(["--config", "git.write-change-id-header=true"]);
        }
        self.environment.apply(&mut command);
        if let Some(path) = self.environment.get("PATH") {
            command.env("PATH", self.path_overrides.add_to(path));
        }
        command.current_dir(&self.checkout);
        command
    }

    async fn current_identity(&self) -> anyhow::Result<(String, String)> {
        let _guard = self.command_lock.lock().await;
        let mut command = self.command("jj");
        command.args([
            "--ignore-working-copy",
            "log",
            "-r",
            "@",
            "--no-graph",
            "-T",
            r#"commit_id ++ " " ++ change_id ++ "\n""#,
        ]);
        let output = output(command, "resolve parent working-copy commit").await?;
        let identity =
            String::from_utf8(output.stdout).context("jj returned non-UTF-8 identity")?;
        let (commit_id, change_id) = identity
            .trim()
            .split_once(' ')
            .context("jj returned an invalid commit identity")?;
        Ok((commit_id.to_owned(), change_id.to_owned()))
    }

    async fn change_id(&self, revision: &str) -> anyhow::Result<String> {
        let _guard = self.command_lock.lock().await;
        let mut command = self.command("jj");
        command.args([
            "--ignore-working-copy",
            "log",
            "-r",
            revision,
            "--no-graph",
            "-T",
            "change_id",
        ]);
        Ok(
            String::from_utf8(output(command, "resolve change id").await?.stdout)?
                .trim()
                .to_owned(),
        )
    }

    pub async fn snapshot(&self) -> anyhow::Result<()> {
        let _guard = self.command_lock.lock().await;
        let mut command = self.command("jj");
        command.args(["util", "snapshot"]);
        run(command, "jj snapshot").await
    }

    /// Internal snapshot inputs used by rho-agent's turn-diff tracker.
    #[doc(hidden)]
    pub fn diff_inputs(&self) -> (Utf8PathBuf, Arc<Mutex<()>>, Vec<(OsString, OsString)>) {
        (
            self.checkout.clone(),
            Arc::clone(&self.command_lock),
            self.environment.values(),
        )
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

async fn run(command: tokio::process::Command, action: &str) -> anyhow::Result<()> {
    output(command, action).await.map(|_| ())
}

async fn output(
    mut command: tokio::process::Command,
    action: &str,
) -> anyhow::Result<std::process::Output> {
    let result = command
        .output()
        .await
        .with_context(|| format!("spawn {action}"))?;
    anyhow::ensure!(
        result.status.success(),
        "{action} failed: {}",
        String::from_utf8_lossy(&result.stderr).trim()
    );
    Ok(result)
}
