// Copyright 2026 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The clone store: a primitive for instant, cheap jj clones.
//!
//! Derived from two constraints: storage must be O(repo + per-clone work),
//! never O(repo x clones), and clone creation must stay well under 100ms.
//! Only two things are O(repo)-sized — git object bytes and the jj commit
//! index — so only those two are shared, each through a mechanism that
//! natively understands borrowing:
//!
//! - **Objects**: every clone has its own private git repo whose
//!   `objects/info/alternates` points at the store's object database. Git
//!   treats alternate objects as present-but-not-mine: fetches negotiate
//!   against them, pushes read them, and `git gc` in a clone prunes only its
//!   own odb (repack `-l` even dedups a clone against the store).
//! - **Index**: clones reflink (or hardlink) the immutable, content-addressed
//!   segment files of a template jj repo's index. A jj index may be a
//!   superset of any operation's visible set, and reindexing only unlinks
//!   your own links, so sharing is safe and blast radius stays per-clone.
//!
//! Everything else — refs, config, remotes, op store, working copy — is
//! per-clone private state. A clone is a completely stock jj repo cloned
//! from `origin` (the real remote) that happens to borrow bytes; no jj
//! behavior is modified, no command is banned, and colocated clones are
//! fine because their git repo is private.
//!
//! Stores are a cache keyed by remote URL, living under one root directory
//! (`git.clone-store`, or `JJ_STORE`). `jj git clone` consults it
//! transparently: ensure the URL's store exists and is fresh, then
//! materialize the clone at the destination. `jj git fetch` in a clone
//! whose remote has a store refreshes the store and fetches from its
//! mirror instead of the network. A store server (`git.clone-store-socket`,
//! or `JJ_STORE_SOCKET`; see [`crate::clone_store_server`]) can own the
//! root: it keeps every store fetched in the background and is the only
//! writer, so clients can run against a read-only store.
//!
//! The store is a plain bare git mirror plus a lazily built template. Its
//! one invariant: **the store never prunes** — clones borrow its objects,
//! so it is append-only (auto-gc is disabled at init). Store fetch is
//! plain `git fetch`; the template refreshes there too, where freshness is
//! produced, so clone creation only reads.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use blake2::Blake2b512;
use blake2::Digest as _;

use crate::backend::BackendInitError;
use crate::backend::CommitId;
use crate::config::ConfigGetError;
use crate::config::ConfigGetResultExt as _;
use crate::git;
use crate::git::GitImportOptions;
use crate::git::GitSettings;
use crate::git_backend::GitBackend;
use crate::object_id::ObjectId as _;
use crate::op_store::RefTarget;
use crate::ref_name::RefName;
use crate::ref_name::RefNameBuf;
use crate::ref_name::RemoteName;
use crate::ref_name::RemoteRefSymbol;
use crate::ref_name::WorkspaceName;
use crate::repo::ReadonlyRepo;
use crate::repo::Repo as _;
use crate::repo::RepoLoader;
use crate::repo::StoreFactories;
use crate::settings::UserSettings;
use crate::signing::Signer;
use crate::workspace::Workspace;
use crate::workspace::default_working_copy_factory;
use crate::workspace_store::SimpleWorkspaceStore;
use crate::workspace_store::WorkspaceStore as _;

/// The remote name every store mirrors under. Clones are born with the
/// same name, so a store only ever serves a clone's `origin`.
pub const STORE_REMOTE: &RemoteName = RemoteName::new("origin");

/// Error from a clone store operation: a context message wrapping whatever
/// lower-level failure caused it.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct CloneStoreError {
    message: String,
    #[source]
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl CloneStoreError {
    pub(crate) fn msg(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }
}

type Result<T, E = CloneStoreError> = std::result::Result<T, E>;

pub(crate) trait Context<T> {
    fn ctx<S: Into<String>>(self, message: impl FnOnce() -> S) -> Result<T>;
}

impl<T, E> Context<T> for Result<T, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn ctx<S: Into<String>>(self, message: impl FnOnce() -> S) -> Result<T> {
        self.map_err(|err| CloneStoreError {
            message: message().into(),
            source: Some(Box::new(err)),
        })
    }
}

/// How `jj` reaches clone stores, from settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CloneStoreConfig {
    /// `git.clone-store`: the directory holding one store per remote URL.
    pub root: Option<PathBuf>,
    /// `git.clone-store-socket`: a store server that owns that directory.
    /// When set, clients never write to the store themselves.
    pub socket: Option<PathBuf>,
}

impl CloneStoreConfig {
    /// Reads `git.clone-store` and `git.clone-store-socket`.
    pub fn from_settings(settings: &UserSettings) -> Result<Self, ConfigGetError> {
        Ok(Self {
            root: settings.get::<PathBuf>("git.clone-store").optional()?,
            socket: settings
                .get::<PathBuf>("git.clone-store-socket")
                .optional()?,
        })
    }

    /// Whether clones and fetches should go through a store at all.
    pub fn is_enabled(&self) -> bool {
        self.root.is_some() || self.socket.is_some()
    }
}

/// Gets `remote_url`'s store, initialized and fresh, through whatever the
/// config names: the server over its socket, or the local root directly.
/// `None` when no store is configured.
pub async fn prepare_store(
    config: &CloneStoreConfig,
    settings: &UserSettings,
    remote_url: &str,
) -> Result<Option<CloneStore>> {
    if let Some(socket) = &config.socket {
        #[cfg(unix)]
        {
            let path = crate::clone_store_server::request(socket, "ensure", remote_url)?;
            return CloneStore::open(&path, settings).map(Some);
        }
        #[cfg(not(unix))]
        {
            return Err(CloneStoreError::msg(format!(
                "clone store sockets are not supported on this platform ({})",
                socket.display()
            )));
        }
    }
    let Some(root) = &config.root else {
        return Ok(None);
    };
    let stores = StoreRoot::new(root.clone(), settings.clone());
    stores.ensure(remote_url).await.map(Some)
}

/// Strips the noise that makes one remote look like two: surrounding
/// whitespace, trailing slashes, and a `.git` suffix.
pub fn normalize_remote_url(url: &str) -> String {
    let mut url = url.trim();
    while let Some(stripped) = url.strip_suffix('/') {
        url = stripped;
    }
    let url = url.strip_suffix(".git").unwrap_or(url);
    url.to_owned()
}

/// The directory name of `url`'s store under a store root: a readable
/// slug from the URL's last component plus a hash of the normalized URL,
/// so distinct remotes never collide and the same remote spelled two
/// ways lands in one store.
pub fn store_key(url: &str) -> String {
    let normalized = normalize_remote_url(url);
    let last = normalized
        .rsplit(['/', ':', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or("repo");
    let slug: String = last
        .chars()
        .take(40)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let slug = slug.trim_start_matches(['.', '-']).to_owned();
    let slug = if slug.is_empty() { "repo".to_owned() } else { slug };
    let digest = Blake2b512::digest(normalized.as_bytes());
    let mut hex = String::with_capacity(12);
    for byte in &digest[..6] {
        write!(hex, "{byte:02x}").unwrap();
    }
    format!("{slug}-{hex}")
}

/// A directory of stores, one per remote URL.
#[derive(Clone, Debug)]
pub struct StoreRoot {
    root: PathBuf,
    settings: UserSettings,
}

impl StoreRoot {
    /// A root at `root`; nothing is created until a store is.
    pub fn new(root: PathBuf, settings: UserSettings) -> Self {
        Self { root, settings }
    }

    /// The directory holding the stores.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where `url`'s store lives, whether or not it exists yet.
    pub fn store_dir(&self, url: &str) -> PathBuf {
        self.root.join(store_key(url))
    }

    /// Opens `url`'s store if it has been initialized.
    pub fn open(&self, url: &str) -> Result<Option<CloneStore>> {
        let dir = self.store_dir(url);
        if !dir.join("clone-store").is_file() {
            return Ok(None);
        }
        CloneStore::open(&dir, &self.settings).map(Some)
    }

    /// Initializes `url`'s store, tolerating a concurrent init that won
    /// the race.
    pub async fn init(&self, url: &str) -> Result<CloneStore> {
        let dir = self.store_dir(url);
        match CloneStore::init_from_remote(&dir, url, &self.settings).await {
            Ok(store) => Ok(store),
            Err(err) => match self.open(url)? {
                Some(store) => Ok(store),
                None => Err(err),
            },
        }
    }

    /// `url`'s store, initialized if missing and fetched if not: the state
    /// a clone should be born from.
    pub async fn ensure(&self, url: &str) -> Result<CloneStore> {
        match self.open(url)? {
            Some(store) => {
                store.fetch().await?;
                Ok(store)
            }
            None => self.init(url).await,
        }
    }

    /// Every initialized store under the root.
    pub fn list(&self) -> Result<Vec<CloneStore>> {
        let mut stores = Vec::new();
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(stores),
            Err(err) => return Err(err).ctx(|| format!("list store root {:?}", self.root)),
        };
        for entry in entries {
            let entry = entry.ctx(|| "read store root entry".to_string())?;
            let dir = entry.path();
            if dir.join("clone-store").is_file() {
                stores.push(CloneStore::open(&dir, &self.settings)?);
            }
        }
        stores.sort_by(|a, b| a.root.cmp(&b.root));
        Ok(stores)
    }
}

/// A remote's store: the shared object mirror and the lazy index template
/// that clones borrow from.
pub struct CloneStore {
    root: PathBuf,
    settings: UserSettings,
    git_executable: PathBuf,
}

impl CloneStore {
    /// Creates a store for `remote_url`: a bare git mirror every clone
    /// will borrow objects from. Branches land at `refs/remotes/origin/*`
    /// and tags at `refs/tags/*` — the ref state a fresh workstation clone
    /// would hold, ready to be copied into newborn clones. Pruning is
    /// disabled permanently: clones reference these objects.
    pub async fn init_from_remote(
        root: &Path,
        remote_url: &str,
        settings: &UserSettings,
    ) -> Result<Self> {
        let store = Self::init_staged(root, settings, |store| {
            let git_dir = store.git_dir();
            store.git([Path::new("init"), Path::new("--bare"), git_dir.as_path()])?;
            for (key, value) in [
                // Append-only: a pruned store object would corrupt every
                // clone that borrowed it.
                ("gc.auto", "0"),
                ("gc.pruneExpire", "never"),
                ("maintenance.auto", "false"),
            ] {
                store.store_git(["config", key, value])?;
            }
            // --no-tags: git's tag auto-following would race the explicit
            // refspec; tags are fetched exactly once, explicitly.
            store.store_git(["remote", "add", "--no-tags", "origin", remote_url])?;
            store.fetch_mirror()
        })
        .await?;
        Ok(store)
    }

    /// Fetches `origin` into the store and refreshes the template (if one
    /// has been built) in the same stroke, so clone creation only reads.
    /// Serialized per store by a lock file; correctness never depends on
    /// this having run.
    pub async fn fetch(&self) -> Result<()> {
        {
            let _lock = flock_exclusive(&self.root.join("fetch.lock"))?;
            self.fetch_mirror()?;
        }
        if self.template_repo_dir().is_dir() {
            self.prepare_template().await?;
        }
        Ok(())
    }

    /// The network half of a fetch: branches, tags, the remote's default
    /// branch, then packed refs so ref listing stays object-free.
    fn fetch_mirror(&self) -> Result<()> {
        self.store_git([
            "fetch",
            "--prune",
            "--no-tags",
            "origin",
            "+refs/heads/*:refs/remotes/origin/*",
            "+refs/tags/*:refs/tags/*",
        ])?;
        // Records refs/remotes/origin/HEAD, which is what clones check out.
        // Best effort: a remote without HEAD (an empty repo) has no default.
        drop(self.store_git(["remote", "set-head", "origin", "--auto"]));
        self.store_git(["pack-refs", "--all"])
    }

    /// Builds or refreshes the template to the mirror's current ref state.
    /// The store server calls this after init and fetch so that clients
    /// find a fresh template without writing to the store.
    pub async fn prepare_template(&self) -> Result<()> {
        let gix_repo = self.store_gix()?;
        let ref_state = self.store_ref_state(&gix_repo)?;
        drop(gix_repo);
        drop(self.ensure_template_fresh(&ref_state).await?);
        Ok(())
    }

    /// Builds the store in a staging sibling and renames it into place:
    /// the final root only ever holds complete stores, an interrupted init
    /// leaves inert wreckage, and a retry just works. Renaming onto a
    /// pre-created *empty* root succeeds; anything non-empty is "already
    /// exists".
    async fn init_staged(
        root: &Path,
        settings: &UserSettings,
        init_git: impl FnOnce(&Self) -> Result<()>,
    ) -> Result<Self> {
        let parent = root
            .parent()
            .ok_or_else(|| CloneStoreError::msg(format!("store root {root:?} has no parent")))?;
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).ctx(|| format!("create store parent {parent:?}"))?;
        }
        let staging = parent.join(format!(
            ".incoming-store-{}-{}",
            std::process::id(),
            std::time::UNIX_EPOCH
                .elapsed()
                .map(|t| t.as_nanos())
                .unwrap_or(0),
        ));
        let staged = Self {
            root: staging.clone(),
            settings: settings.clone(),
            git_executable: git_executable(settings)?,
        };
        let result = (|| {
            fs::create_dir(&staging).ctx(|| format!("create store staging dir {staging:?}"))?;
            init_git(&staged)?;
            // Completion token: only ever written here, inside a staged
            // init that then renames into place, so open() can use it to
            // reject look-alike and truncated directories.
            fs::write(staging.join("clone-store"), "jj clone store v3\n")
                .ctx(|| "write store marker".to_string())?;
            Ok(())
        })();
        if let Err(err) = result {
            drop(fs::remove_dir_all(&staging));
            return Err(err);
        }
        match fs::rename(&staging, root) {
            Ok(()) => Ok(Self {
                root: root.to_owned(),
                ..staged
            }),
            Err(rename_err) => {
                drop(fs::remove_dir_all(&staging));
                if root.exists() {
                    return Err(CloneStoreError::msg(format!(
                        "store already exists at {root:?}"
                    )));
                }
                Err(rename_err).ctx(|| format!("move store into place at {root:?}"))
            }
        }
    }

    /// Opens an existing store, refusing partial or look-alike directories
    /// (the completion marker is only ever written by a finished init).
    pub fn open(root: &Path, settings: &UserSettings) -> Result<Self> {
        if !(root.join("clone-store").is_file() && root.join("git").is_dir()) {
            return Err(CloneStoreError::msg(format!(
                "no store (or a partial store) at {root:?}"
            )));
        }
        Ok(Self {
            root: root.to_owned(),
            settings: settings.clone(),
            git_executable: git_executable(settings)?,
        })
    }

    /// The store's directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The store's bare git mirror — the shared object database every
    /// clone's alternates point at, and the source clones fetch from.
    pub fn git_dir(&self) -> PathBuf {
        self.root.join("git")
    }

    fn template_repo_dir(&self) -> PathBuf {
        self.root.join("template").join("repo")
    }

    /// Opens the store's git repo with gix, isolated from user and system
    /// git config so behavior never depends on the environment.
    fn store_gix(&self) -> Result<gix::Repository> {
        gix::open_opts(
            self.git_dir(),
            gix::open::Options::isolated().open_path_as_is(true),
        )
        .ctx(|| format!("open store git repo {:?}", self.git_dir()))
    }

    /// The URL this store mirrors.
    pub fn remote_url(&self) -> Result<String> {
        let store_git = self.store_gix()?;
        self.remote_url_of(&store_git)
    }

    fn remote_url_of(&self, store_git: &gix::Repository) -> Result<String> {
        let url = store_git
            .config_snapshot()
            .string("remote.origin.url")
            .ok_or_else(|| CloneStoreError::msg("store git repo has no remote.origin.url"))?;
        Ok(String::from_utf8_lossy(&url).into_owned())
    }

    /// The remote's default branch as recorded at the last fetch
    /// (`refs/remotes/origin/HEAD`), if the remote has one.
    pub fn default_branch(&self) -> Result<Option<RefNameBuf>> {
        let store_git = self.store_gix()?;
        let Ok(reference) = store_git.find_reference("refs/remotes/origin/HEAD") else {
            return Ok(None);
        };
        let target = match reference.target() {
            gix::refs::TargetRef::Symbolic(name) => name.as_bstr().to_string(),
            gix::refs::TargetRef::Object(_) => return Ok(None),
        };
        Ok(target
            .strip_prefix("refs/remotes/origin/")
            .map(|name| RefNameBuf::from(name.to_owned())))
    }

    /// The store's git ref state: one `sha refname [peeled-sha]` line per
    /// ref, sorted by refname. Doubles as the template's freshness
    /// fingerprint and as the source for a newborn clone's packed-refs, so
    /// both always describe the same state. Read in-process: the store
    /// packs refs at fetch time, so peeled tag targets come from packed-ref
    /// hints without a single object read in the common case.
    fn store_ref_state(&self, store_git: &gix::Repository) -> Result<String> {
        let mut entries: Vec<(String, String)> = Vec::new();
        let platform = store_git
            .references()
            .ctx(|| "list store refs".to_string())?;
        for prefix in ["refs/remotes/origin/", "refs/tags/"] {
            let iter = platform
                .prefixed(prefix)
                .ctx(|| format!("list store refs under {prefix}"))?;
            for reference in iter {
                let reference = reference.map_err(|err| CloneStoreError {
                    message: format!("read store ref under {prefix}"),
                    source: Some(err),
                })?;
                let name = reference.inner.name.as_bstr().to_string();
                let Some(target) = reference.inner.target.try_id().map(|id| id.to_owned()) else {
                    // A symbolic ref in a mirror namespace (origin/HEAD) is
                    // nothing a clone should be born with.
                    continue;
                };
                let peeled = match reference.inner.peeled {
                    // Packed-ref peel hint; equal means not an annotated tag.
                    Some(peeled) if peeled != target => Some(peeled),
                    Some(_) => None,
                    // Loose ref (a fetch not yet followed by pack-refs, or
                    // an interrupted one): peel through the object store.
                    None if prefix == "refs/tags/" => {
                        let peeled = reference
                            .into_fully_peeled_id()
                            .ctx(|| format!("peel store ref {name}"))?
                            .detach();
                        (peeled != target).then_some(peeled)
                    }
                    None => None,
                };
                let line = match peeled {
                    Some(peeled) => format!("{target} {name} {peeled}"),
                    None => format!("{target} {name}"),
                };
                entries.push((name, line));
            }
        }
        entries.sort();
        let mut state = String::new();
        for (_, line) in entries {
            state.push_str(&line);
            state.push('\n');
        }
        Ok(state)
    }

    /// The template is an amortized cache: built on first use (the one
    /// full O(repo) index build a store ever pays), refreshed with a delta
    /// import later, and skipped entirely when the store's ref state
    /// hasn't moved since the last refresh — jj's ref import pays a
    /// per-ref cost (annotated tags especially), so the common no-change
    /// case must not walk refs at all. Serialized by a lock file;
    /// staleness or a crashed refresh only ever means more work for the
    /// next refresh, never incorrect clones.
    async fn ensure_template_fresh(&self, ref_state: &str) -> Result<Arc<ReadonlyRepo>> {
        let _lock = flock_exclusive(&self.root.join("template.lock"))?;
        let template = self.template_repo_dir();
        let state_path = self.root.join("template").join("ref-state");
        if !template.join("store").is_dir() {
            let staging = self.root.join(format!(
                ".incoming-template-{}-{}",
                std::process::id(),
                std::time::UNIX_EPOCH
                    .elapsed()
                    .map(|t| t.as_nanos())
                    .unwrap_or(0),
            ));
            let result = async {
                let staged_repo = staging.join("repo");
                fs::create_dir(&staging).ctx(|| format!("create template staging {staging:?}"))?;
                fs::create_dir(&staged_repo).ctx(|| "create template repo dir".to_string())?;
                // <root>/template/repo/store -> ../../../git; the staging
                // sibling sits at the same depth so the link needs no fixup.
                let repo =
                    init_repo_at(&staged_repo, Path::new("../../../git"), &self.settings).await?;
                import_git_refs(&repo).await?;
                fs::write(staging.join("ref-state"), ref_state)
                    .ctx(|| "record template ref state".to_string())?;
                Ok(())
            }
            .await;
            if let Err(err) = result {
                drop(fs::remove_dir_all(&staging));
                return Err(err);
            }
            fs::rename(&staging, template.parent().unwrap())
                .ctx(|| "move template into place".to_string())?;
        } else if fs::read_to_string(&state_path).ok().as_deref() != Some(ref_state) {
            let repo = open_repo_at(&template, &self.settings).await?;
            import_git_refs(&repo).await?;
            fs::write(&state_path, ref_state).ctx(|| "record template ref state".to_string())?;
        }
        let repo = open_repo_at(&template, &self.settings).await?;
        // Force the index (and its op link) to exist for the loaded op —
        // load_at_head may have just merged concurrent op heads.
        repo.index();
        Ok(repo)
    }

    /// The template to seed a clone from, and whether it lags `ref_state`.
    /// A fresh template is used without taking the lock, so a client of a
    /// read-only store (one a server keeps fetched) never writes. When the
    /// template is stale and cannot be refreshed here, the stale one is
    /// still a valid seed: the clone imports the difference itself.
    async fn template_for_clone(&self, ref_state: &str) -> Result<(Arc<ReadonlyRepo>, bool)> {
        let template = self.template_repo_dir();
        let state_path = self.root.join("template").join("ref-state");
        let built = template.join("store").is_dir();
        if built && fs::read_to_string(&state_path).ok().as_deref() == Some(ref_state) {
            return Ok((open_repo_at(&template, &self.settings).await?, false));
        }
        match self.ensure_template_fresh(ref_state).await {
            Ok(repo) => Ok((repo, false)),
            Err(err) if built => {
                tracing::warn!(?err, "clone store template could not be refreshed; seeding stale");
                Ok((open_repo_at(&template, &self.settings).await?, true))
            }
            Err(err) => Err(err),
        }
    }

    /// Creates a clone at `workspace_root`: a stock jj workspace whose
    /// private git repo borrows the store's objects through alternates and
    /// whose index is seeded from the template. Born at the store's
    /// last-fetched state — `main@origin`, tags, the works — exactly like
    /// a fresh clone on its own machine, with `origin` pointing at the real
    /// remote. The directory must exist and be empty; on failure the
    /// caller removes what was written (`.jj`, and `.git` when colocated).
    pub async fn materialize(
        &self,
        workspace_root: &Path,
        colocate: bool,
    ) -> Result<(Workspace, Arc<ReadonlyRepo>)> {
        let store_git = self.store_gix()?;
        let ref_state = self.store_ref_state(&store_git)?;
        let remote_url = self.remote_url_of(&store_git)?;
        drop(store_git);
        let (template, stale) = self.template_for_clone(&ref_state).await?;
        // Absolute: the store's location is configuration, not something a
        // clone's position implies. A server-owned store is mounted at the
        // same path wherever clones run.
        let alternates = dunce::canonicalize(self.git_dir())
            .ctx(|| format!("resolve store git dir {:?}", self.git_dir()))?
            .join("objects");

        let jj_dir = crate::workspace::create_jj_dir(workspace_root)
            .ctx(|| format!("create .jj in {workspace_root:?}"))?;
        let repo_dir = jj_dir.join("repo");
        fs::create_dir(&repo_dir).ctx(|| format!("create repo dir {repo_dir:?}"))?;
        let git_target: PathBuf = if colocate {
            let git_dir = workspace_root.join(".git");
            write_clone_git_dir(&git_dir, &remote_url, &ref_state, &alternates, false)?;
            PathBuf::from("../../../.git")
        } else {
            PathBuf::from("git")
        };
        let repo = ReadonlyRepo::init(
            &self.settings,
            &repo_dir,
            &|settings, store_path| {
                if !colocate {
                    write_clone_git_dir(
                        &store_path.join("git"),
                        &remote_url,
                        &ref_state,
                        &alternates,
                        true,
                    )
                    .map_err(|err| BackendInitError(Box::new(err)))?;
                }
                Ok(Box::new(GitBackend::init_external(
                    settings,
                    store_path,
                    &git_target,
                )?))
            },
            Signer::from_settings(&self.settings).ctx(|| "init signer".to_string())?,
            ReadonlyRepo::default_op_store_initializer(),
            ReadonlyRepo::default_op_heads_store_initializer(),
            ReadonlyRepo::default_index_store_initializer(),
            ReadonlyRepo::default_submodule_store_initializer(),
        )
        .await
        .ctx(|| format!("init repo at {repo_dir:?}"))?;
        let init_op_hex = repo.op_id().hex();
        drop(repo);
        self.seed_index_from_template(&repo_dir, &init_op_hex, &template.op_id().hex())?;

        // Reload from disk so the seeded op link is what the next op builds
        // on; the handle from init still holds the unseeded root-only index.
        let repo = open_repo_at(&repo_dir, &self.settings).await?;
        let repo = seed_view_from_template(&repo, &template).await?;
        let repo = if stale {
            import_git_refs(&repo).await?
        } else {
            repo
        };

        let workspace_store =
            SimpleWorkspaceStore::load(&repo_dir).ctx(|| "load workspace store".to_string())?;
        let (working_copy, repo) = crate::workspace::init_working_copy(
            &repo,
            workspace_root,
            &jj_dir,
            default_working_copy_factory().as_ref(),
            WorkspaceName::DEFAULT.to_owned(),
        )
        .await
        .ctx(|| "init working copy".to_string())?;
        let repo_dir = dunce::canonicalize(&repo_dir).ctx(|| "resolve repo dir".to_string())?;
        let workspace = Workspace::new(workspace_root, repo_dir, working_copy, repo.loader().clone())
            .ctx(|| "open workspace".to_string())?;
        workspace_store
            .add(workspace.workspace_name(), workspace.workspace_root())
            .ctx(|| "record workspace".to_string())?;
        Ok((workspace, repo))
    }

    /// Gives a fresh clone the template's index: share every immutable,
    /// content-addressed segment file, then associate the clone's initial
    /// operation with the template's current index by copying the op link
    /// (the link file names segment ids, not operations, so it is valid
    /// under any operation whose view the index covers — an index may
    /// always be a superset of the visible set).
    ///
    /// Sharing prefers reflink (a new copy-on-write inode) over hardlink:
    /// the clone's segment files then have their own ownership and
    /// metadata, so nothing the clone's owner does can couple back to the
    /// template's inodes. Filesystems without reflink (tmpfs, ext4) fall
    /// back to hardlinks; both share the bytes.
    fn seed_index_from_template(
        &self,
        repo_path: &Path,
        init_op_hex: &str,
        template_op: &str,
    ) -> Result<()> {
        let template_index = self.template_repo_dir().join("index");
        let clone_index = repo_path.join("index");
        for sub in ["segments", "changed_paths"] {
            let src_dir = template_index.join(sub);
            let dst_dir = clone_index.join(sub);
            fs::create_dir_all(&dst_dir).ctx(|| format!("create index dir {dst_dir:?}"))?;
            for entry in
                fs::read_dir(&src_dir).ctx(|| format!("read template index dir {src_dir:?}"))?
            {
                let entry = entry.ctx(|| "read template index entry".to_string())?;
                if !entry
                    .file_type()
                    .ctx(|| "stat template segment".to_string())?
                    .is_file()
                {
                    continue;
                }
                let dst = dst_dir.join(entry.file_name());
                match share_file(&entry.path(), &dst) {
                    Ok(()) => {}
                    // Content-addressed names: an existing file is the same
                    // bytes (e.g. the root-only segment every fresh repo
                    // writes identically).
                    Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(err) => {
                        return Err(err).ctx(|| format!("share segment to {dst:?}"));
                    }
                }
            }
        }
        let src_link = template_index.join("op_links").join(template_op);
        let dst_links = clone_index.join("op_links");
        fs::create_dir_all(&dst_links).ctx(|| "create op_links dir".to_string())?;
        fs::copy(&src_link, dst_links.join(init_op_hex))
            .ctx(|| format!("copy template op link {src_link:?}"))?;
        Ok(())
    }

    /// Runs git (the executable from settings) against the store's git dir.
    fn store_git<const N: usize>(&self, args: [&str; N]) -> Result<()> {
        let git_dir = self.git_dir();
        let mut full: Vec<&Path> = vec![Path::new("--git-dir"), &git_dir];
        full.extend(args.iter().map(|arg| Path::new(*arg)));
        self.git(full)
    }

    fn git(&self, args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>) -> Result<()> {
        run_git(&self.git_executable, args)
    }
}

/// Picks a repo's trunk: the first of main/master/trunk at `origin`,
/// falling back to any visible head.
pub fn trunk_of(repo: &Arc<ReadonlyRepo>) -> Option<CommitId> {
    let view = repo.view();
    for name in ["main", "master", "trunk"] {
        let symbol = RemoteRefSymbol {
            name: RefName::new(name),
            remote: STORE_REMOTE,
        };
        if let Some(id) = view.get_remote_bookmark(symbol).target.as_normal() {
            return Some(id.clone());
        }
    }
    view.heads().iter().next().cloned()
}

/// Writes a clone's git dir directly — the same files `git init` plus
/// `git remote add --no-tags origin <url>` would produce, without paying
/// two subprocess spawns per clone. Private refs, private config, origin
/// pointing at the real remote: indistinguishable from the git dir of a
/// fresh workstation clone, except that it owns no object bytes
/// (alternates borrow the store's) and its refs come verbatim from the
/// store's last-fetched state.
fn write_clone_git_dir(
    git_dir: &Path,
    remote_url: &str,
    ref_state: &str,
    alternates: &Path,
    bare: bool,
) -> Result<()> {
    for dir in [
        "objects/info",
        "objects/pack",
        "refs/heads",
        "refs/tags",
        "info",
    ] {
        fs::create_dir_all(git_dir.join(dir)).ctx(|| format!("create clone git dir {dir:?}"))?;
    }
    fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n")
        .ctx(|| "write clone git HEAD".to_string())?;
    fs::write(
        git_dir.join("config"),
        format!(
            "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = {bare}\n\
             \tlogallrefupdates = {logs}\n[remote \"origin\"]\n\turl = {url}\n\tfetch = \
             +refs/heads/*:refs/remotes/origin/*\n\ttagOpt = --no-tags\n",
            logs = !bare,
            url = git_config_quote(remote_url)
        ),
    )
    .ctx(|| "write clone git config".to_string())?;
    // What colocated `jj git init` writes: keep jj's metadata out of git's
    // view.
    fs::write(git_dir.join("info").join("exclude"), "/.jj/\n")
        .ctx(|| "write clone git exclude".to_string())?;
    // Borrow the store's objects.
    let alternates_line = crate::file_util::path_to_bytes(alternates)
        .ctx(|| "encode alternates path".to_string())?;
    let mut alternates_file = alternates_line.to_vec();
    alternates_file.push(b'\n');
    fs::write(
        git_dir.join("objects").join("info").join("alternates"),
        alternates_file,
    )
    .ctx(|| "write alternates".to_string())?;
    // Born at the store's last-fetched ref state, verbatim: the same
    // refs/remotes/origin/* and refs/tags/* a real `git clone` leaves.
    // `ref_state` is sorted by refname and refs/remotes orders before
    // refs/tags, so the packed-refs file stays sorted.
    let mut packed = String::from("# pack-refs with: peeled fully-peeled sorted \n");
    for line in ref_state.lines() {
        let mut parts = line.split(' ');
        let (Some(sha), Some(refname)) = (parts.next(), parts.next()) else {
            continue;
        };
        writeln!(packed, "{sha} {refname}").unwrap();
        if let Some(peeled) = parts.next().filter(|peeled| !peeled.is_empty()) {
            writeln!(packed, "^{peeled}").unwrap();
        }
    }
    fs::write(git_dir.join("packed-refs"), packed).ctx(|| "write clone packed-refs".to_string())?;
    Ok(())
}

/// Quotes a value for a git config file: git's parser understands double
/// quotes with backslash escapes.
fn git_config_quote(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push('"');
    for c in value.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

/// Initializes a stock jj repo at `repo_path` backed by the git repo at
/// `git_target` (relative to the repo's `store/` dir). Everything — op
/// store, op heads, index, working-copy machinery — is jj's default.
async fn init_repo_at(
    repo_path: &Path,
    git_target: &'static Path,
    settings: &UserSettings,
) -> Result<Arc<ReadonlyRepo>> {
    ReadonlyRepo::init(
        settings,
        repo_path,
        &|settings, store_path| {
            Ok(Box::new(GitBackend::init_external(
                settings, store_path, git_target,
            )?))
        },
        Signer::from_settings(settings).ctx(|| "init signer".to_string())?,
        ReadonlyRepo::default_op_store_initializer(),
        ReadonlyRepo::default_op_heads_store_initializer(),
        ReadonlyRepo::default_index_store_initializer(),
        ReadonlyRepo::default_submodule_store_initializer(),
    )
    .await
    .ctx(|| format!("init repo at {repo_path:?}"))
}

/// Loads a jj repo at its current op heads.
async fn open_repo_at(repo_path: &Path, settings: &UserSettings) -> Result<Arc<ReadonlyRepo>> {
    let loader = RepoLoader::init_from_file_system(settings, repo_path, &StoreFactories::default())
        .ctx(|| format!("load repo at {repo_path:?}"))?;
    loader
        .load_at_head()
        .await
        .ctx(|| format!("load repo at head at {repo_path:?}"))
}

/// Imports the backing git repo's refs into the repo's view — stock jj
/// import: `refs/remotes/origin/*` become `@origin` bookmarks, `refs/tags`
/// become tags — indexing any commits the index doesn't cover yet.
async fn import_git_refs(repo: &Arc<ReadonlyRepo>) -> Result<Arc<ReadonlyRepo>> {
    let mut tx = repo.start_transaction();
    let options = GitImportOptions {
        abandon_unreachable_commits: false,
        record_synthetic_predecessors: false,
        remote_auto_track_bookmarks: HashMap::new(),
    };
    git::import_refs(tx.repo_mut(), &options)
        .await
        .ctx(|| "import refs".to_string())?;
    tx.commit("import git refs")
        .await
        .ctx(|| "commit ref import".to_string())
}

/// Gives a newborn clone the template's view without walking git refs or
/// loading a single object: the view (heads, remote bookmarks, tags,
/// git_refs) is plain data, and the seeded index already covers every
/// commit it names, so a wholesale copy of the template's store view is
/// both correct and O(refs) cheap — the same trick the index seeding
/// plays with index bytes. Importing from git instead would cost a git
/// object read per ref (annotated tags are peeled), which blows the
/// clone-time budget on tag-heavy repos. The copied git_refs match the
/// clone's packed-refs exactly (both come from the same store ref state),
/// so the clone's own later fetches import incrementally on top.
/// Working-copy and git-HEAD state is per-workspace, never shared, and
/// cleared explicitly.
async fn seed_view_from_template(
    repo: &Arc<ReadonlyRepo>,
    template: &Arc<ReadonlyRepo>,
) -> Result<Arc<ReadonlyRepo>> {
    let mut tx = repo.start_transaction();
    let mut data = template.view().store_view().clone();
    data.wc_commit_ids.clear();
    data.git_heads.clear();
    data.git_head = RefTarget::absent();
    tx.repo_mut().set_view(data);
    tx.commit("import refs from template")
        .await
        .ctx(|| "commit view seed".to_string())
}

/// Shares `src`'s bytes at `dst` without copying them: reflink where the
/// filesystem supports it, hardlink otherwise. Fails with `AlreadyExists`
/// if `dst` exists.
fn share_file(src: &Path, dst: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let src_file = fs::File::open(src)?;
        let dst_file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dst)?;
        match rustix::fs::ioctl_ficlone(&dst_file, &src_file) {
            Ok(()) => return Ok(()),
            Err(_) => {
                // Not reflinkable here (filesystem, kernel, or cross-device);
                // remove the empty file and share via hardlink instead.
                drop(dst_file);
                fs::remove_file(dst)?;
            }
        }
    }
    fs::hard_link(src, dst)
}

fn flock_exclusive(path: &Path) -> Result<fs::File> {
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .ctx(|| format!("open lock file {path:?}"))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)
        .ctx(|| format!("lock {path:?}"))?;
    Ok(lock)
}

fn git_executable(settings: &UserSettings) -> Result<PathBuf> {
    Ok(GitSettings::from_settings(settings)
        .ctx(|| "load git settings".to_string())?
        .executable_path)
}

fn run_git(
    git_executable: &Path,
    args: impl IntoIterator<Item = impl AsRef<std::ffi::OsStr>>,
) -> Result<()> {
    let mut command = Command::new(git_executable);
    command.args(args);
    let output = command.output().ctx(|| format!("run {git_executable:?}"))?;
    if !output.status.success() {
        return Err(CloneStoreError::msg(format!(
            "git {:?} failed: {}",
            command.get_args().collect::<Vec<_>>(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_keys_are_stable_and_normalized() {
        let a = store_key("https://github.com/example/repo.git");
        assert_eq!(a, store_key("https://github.com/example/repo"));
        assert_eq!(a, store_key(" https://github.com/example/repo/ "));
        assert!(a.starts_with("repo-"), "{a}");
        assert_ne!(a, store_key("https://github.com/other/repo"));
        assert_ne!(a, store_key("git@github.com:example/repo.git"));
        let weird = store_key("/tmp/some dir/x y.git");
        assert!(weird.starts_with("x_y-"), "{weird}");
        assert!(store_key("").starts_with("repo-"));
    }
}
