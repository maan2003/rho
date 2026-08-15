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
//! - **Index**: clones hardlink the immutable, content-addressed segment files
//!   of a template jj repo's index. A jj index may be a superset of any
//!   operation's visible set, and reindexing only unlinks your own links, so
//!   sharing is safe and blast radius stays per-clone.
//!
//! Everything else — refs, config, remotes, op store, keep refs — is
//! per-clone private state. A clone is a completely stock jj repo cloned
//! from `origin` (the real remote) that happens to borrow bytes; no jj
//! behavior is modified, no command is banned, and colocated workspaces
//! (real git worktrees) are fine because their git repo is private.
//!
//! The store is a plain bare git mirror plus a lazily built template. Its
//! one invariant: **the store never prunes** — clones borrow its objects,
//! so it is append-only (auto-gc is disabled at init). Store fetch is
//! plain `git fetch`; the template refreshes at clone creation, where its
//! freshness is consumed, so the first clone after a store init pays the
//! one full index build and later clones pay a usually-empty delta.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use crate::backend::CommitId;
use crate::commit::Commit;
use crate::git;
use crate::git::GitImportOptions;
use crate::git::GitSettings;
use crate::git_backend::GitBackend;
use crate::object_id::ObjectId as _;
use crate::op_store::RefTarget;
use crate::ref_name::RefName;
use crate::ref_name::RemoteName;
use crate::ref_name::RemoteRefSymbol;
use crate::ref_name::WorkspaceNameBuf;
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
    fn msg(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }
}

type Result<T, E = CloneStoreError> = std::result::Result<T, E>;

trait Context<T> {
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

/// A repository's store: the shared object mirror, the lazy index
/// template, and the clones borrowing from both.
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
            store.store_git([
                "fetch",
                "--no-tags",
                "origin",
                "+refs/heads/*:refs/remotes/origin/*",
                "+refs/tags/*:refs/tags/*",
            ])?;
            // Packed refs carry precomputed peeled targets, which keeps ref
            // listing free of per-tag object reads on the clone path.
            store.store_git(["pack-refs", "--all"])
        })
        .await?;
        Ok(store)
    }

    /// Fetches `origin` into the store: plain `git fetch`, nothing else.
    /// An under-the-hood prefetch so clones' own fetches find every object
    /// already local. Serialized per store by a lock file; correctness
    /// never depends on this having run.
    pub async fn fetch(&self) -> Result<()> {
        {
            let _lock = flock_exclusive(&self.root.join("fetch.lock"))?;
            self.store_git([
                "fetch",
                "--prune",
                "--no-tags",
                "origin",
                "+refs/heads/*:refs/remotes/origin/*",
                "+refs/tags/*:refs/tags/*",
            ])?;
            self.store_git(["pack-refs", "--all"])?;
        }
        // Refresh the template (if one has been built) where freshness is
        // produced rather than lazily where it is consumed: clone creation
        // then only reads the template, so store writes — fetch and
        // template alike — stay with whoever runs fetches, and clone
        // creation can run under a principal with no store write access.
        // The lazy refresh at clone creation remains as a fallback.
        if self.template_repo_dir().is_dir() {
            let gix_repo = self.store_gix()?;
            let ref_state = self.store_ref_state(&gix_repo)?;
            drop(self.ensure_template_fresh(&ref_state).await?);
        }
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
            fs::create_dir(staged.clones_dir()).ctx(|| "create clones dir".to_string())?;
            // Completion token: only ever written here, inside a staged
            // init that then renames into place, so open() can use it to
            // reject look-alike and truncated directories.
            fs::write(staging.join("clone-store"), "jj clone store v2\n")
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

    /// The store's bare git mirror — the shared object database every
    /// clone's alternates point at.
    pub fn git_dir(&self) -> PathBuf {
        self.root.join("git")
    }

    fn template_repo_dir(&self) -> PathBuf {
        self.root.join("template").join("repo")
    }

    fn clones_dir(&self) -> PathBuf {
        self.root.join("clones")
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

    fn remote_url(&self, store_git: &gix::Repository) -> Result<String> {
        let url = store_git
            .config_snapshot()
            .string("remote.origin.url")
            .ok_or_else(|| CloneStoreError::msg("store git repo has no remote.origin.url"))?;
        Ok(String::from_utf8_lossy(&url).into_owned())
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
                    // A symbolic ref in a mirror namespace (e.g. a mirrored
                    // HEAD) is nothing a clone should be born with.
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

    /// Resolves a clone id to its repo dir, rejecting anything that isn't a
    /// single normal path component — ids reach this from callers we don't
    /// control, and a traversal id would mutate paths outside the store.
    pub fn clone_repo_path(&self, id: &str) -> Result<PathBuf> {
        if !(!id.is_empty()
            && id.len() <= 80
            && !id.starts_with(['.', '-'])
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        {
            return Err(CloneStoreError::msg(format!(
                "invalid clone id {id:?}: alphanumerics plus '-', '_', '.'; must not start with \
                 '.' or '-'"
            )));
        }
        Ok(self.clones_dir().join(id).join("repo"))
    }

    fn clone_git_dir(&self, id: &str) -> Result<PathBuf> {
        Ok(self.clone_repo_path(id)?.parent().unwrap().join("git"))
    }

    /// The template is an amortized cache: built on first use (the one
    /// full O(repo) index build a store ever pays), refreshed with a delta
    /// import on later clone creations, and skipped entirely when the
    /// store's ref state hasn't moved since the last refresh — jj's ref
    /// import pays a per-ref cost (annotated tags especially), so the
    /// common no-change case must not walk refs at all. Serialized by a
    /// lock file; staleness or a crashed refresh only ever means more work
    /// for the next refresh, never incorrect clones.
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

    /// Creates a clone: a private git repo borrowing the store's objects
    /// through alternates, plus a stock jj repo over it with a seeded
    /// index. Born at the store's last-fetched state — `main@origin`,
    /// tags, the works — exactly like a fresh clone on its own machine.
    /// No workspace is attached; workspaces are separate.
    pub async fn create_clone(&self, id: &str) -> Result<PathBuf> {
        let repo_path = self.clone_repo_path(id)?;
        let clone_dir = self.clones_dir().join(id);
        if clone_dir.exists() {
            return Err(CloneStoreError::msg(format!(
                "clone {id} already exists at {repo_path:?}"
            )));
        }
        let store_git = self.store_gix()?;
        let ref_state = self.store_ref_state(&store_git)?;
        let remote_url = self.remote_url(&store_git)?;
        drop(store_git);
        let template = self.ensure_template_fresh(&ref_state).await?;
        // Build in a staging dir and rename into place: the final path only
        // ever holds complete clones, so an interrupted creation leaves the
        // id free and the wreckage inert. Dot-prefixed so it can't collide
        // with a valid id; same directory depth so relative links need no
        // fixup.
        let staging = self.clones_dir().join(format!(
            ".incoming-{id}-{}-{}",
            std::process::id(),
            std::time::UNIX_EPOCH
                .elapsed()
                .map(|t| t.as_nanos())
                .unwrap_or(0),
        ));
        if let Err(err) = self
            .build_clone_in(&staging, &ref_state, &remote_url, &template)
            .await
        {
            drop(fs::remove_dir_all(&staging));
            return Err(err);
        }
        match fs::rename(&staging, &clone_dir) {
            Ok(()) => Ok(repo_path),
            Err(rename_err) => {
                drop(fs::remove_dir_all(&staging));
                if clone_dir.exists() {
                    return Err(CloneStoreError::msg(format!(
                        "clone {id} already exists at {repo_path:?}"
                    )));
                }
                Err(rename_err).ctx(|| format!("move clone into place at {clone_dir:?}"))
            }
        }
    }

    /// Builds a complete clone (`git/` + `repo/`) under `staging`, which
    /// sits at the same depth as its final location. `ref_state` is the
    /// store's current ref listing (the state `template` was refreshed to).
    async fn build_clone_in(
        &self,
        staging: &Path,
        ref_state: &str,
        remote_url: &str,
        template: &Arc<ReadonlyRepo>,
    ) -> Result<()> {
        let git_dir = staging.join("git");
        let repo_path = staging.join("repo");
        fs::create_dir(staging).ctx(|| format!("create staging dir {staging:?}"))?;
        write_clone_git_dir(&git_dir, remote_url, ref_state)?;

        // The jj repo over it. <clone>/repo/store -> ../../git.
        fs::create_dir(&repo_path).ctx(|| format!("create staging repo dir {repo_path:?}"))?;
        let repo = init_repo_at(&repo_path, Path::new("../../git"), &self.settings).await?;
        let init_op_hex = repo.op_id().hex();
        drop(repo);
        self.seed_index_from_template(&repo_path, &init_op_hex, &template.op_id().hex())?;

        // Reload from disk so the seeded op link is what the next op builds
        // on; the handle from init still holds the unseeded root-only index.
        let repo = open_repo_at(&repo_path, &self.settings).await?;
        seed_view_from_template(&repo, template).await?;
        Ok(())
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
    /// metadata, so nothing the clone's owner does — and no ownership
    /// scheme layered above the store — can couple back to the template's
    /// inodes. Filesystems without reflink (tmpfs, ext4) fall back to
    /// hardlinks; both share the bytes.
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

    /// Loads a clone at its current op heads.
    pub async fn open_clone(&self, id: &str) -> Result<Arc<ReadonlyRepo>> {
        let repo_path = self.clone_repo_path(id)?;
        if !repo_path.is_dir() {
            return Err(CloneStoreError::msg(format!(
                "clone {id} does not exist in store {:?}",
                self.root
            )));
        }
        open_repo_at(&repo_path, &self.settings).await
    }

    /// Attaches a working directory to a clone at `target` (or the clone's
    /// trunk): a real git worktree of the clone's git plus a jj workspace,
    /// colocated — stock jj keeps HEAD/index in sync, and every git tool
    /// works because this *is* a git checkout. jj materializes the files;
    /// the worktree is created without a checkout and its index set after.
    ///
    /// Like store init and clone creation, the workspace is built in a
    /// same-depth staging sibling and renamed into place: the final path
    /// only ever holds complete workspaces, and an interrupted creation
    /// leaves the path free for a clean retry. Every path baked into the
    /// staged workspace is written for the *final* location before the
    /// rename — the worktree pointer and jj repo pointer are relative and
    /// depth-invariant, and the two files that do embed the workspace's
    /// name (git's worktree back-pointer, jj's workspace-store entry) are
    /// pointed at the final path explicitly.
    ///
    /// Relative pointers make a store and its workspaces one relocatable
    /// tree: created side by side (say `<root>/.stores/<repo>` and
    /// `<root>/<name>`), they can be exposed in a mount namespace at any
    /// other root (say `/ws`) — wholly or as selected bind mounts — and
    /// every pointer keeps resolving, as long as the mounts preserve the
    /// workspace's position relative to its store. No pointer escapes the
    /// tree, so the namespace needs nothing else mounted.
    pub async fn create_workspace(
        &self,
        clone_id: &str,
        workspace_root: &Path,
        workspace_name: &str,
        target: Option<CommitId>,
    ) -> Result<()> {
        let repo = self.open_clone(clone_id).await?;
        let target = match target {
            Some(id) => id,
            None => trunk_of(&repo)
                .ok_or_else(|| CloneStoreError::msg("clone has no trunk to check out"))?,
        };
        let target = repo
            .store()
            .get_commit_async(&target)
            .await
            .ctx(|| "load target commit".to_string())?;

        let clone_git = self.clone_git_dir(clone_id)?;
        // Serializes workspace creation per clone: the cleanup prune below
        // must not race another creation's not-yet-renamed registration
        // (whose back-pointer targets a final path that doesn't exist yet).
        let _lock = flock_exclusive(&self.clones_dir().join(clone_id).join("workspace.lock"))?;
        // Registrations left by creations that died before their rename
        // point at never-created final paths; git considers them prunable.
        self.git([
            Path::new("--git-dir"),
            &clone_git,
            Path::new("worktree"),
            Path::new("prune"),
        ])?;

        let parent = workspace_root.parent().ok_or_else(|| {
            CloneStoreError::msg(format!("workspace root {workspace_root:?} has no parent"))
        })?;
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).ctx(|| format!("create workspace parent {parent:?}"))?;
        }
        let file_name = workspace_root.file_name().ok_or_else(|| {
            CloneStoreError::msg(format!(
                "workspace root {workspace_root:?} has no directory name"
            ))
        })?;
        let staging = parent.join(format!(
            ".incoming-ws-{}-{}",
            std::process::id(),
            std::time::UNIX_EPOCH
                .elapsed()
                .map(|t| t.as_nanos())
                .unwrap_or(0),
        ));
        let result = self
            .build_workspace_in(
                &staging,
                file_name,
                clone_id,
                &clone_git,
                &repo,
                workspace_name,
                &target,
            )
            .await;
        if let Err(err) = result {
            drop(fs::remove_dir_all(&staging));
            drop(self.git([
                Path::new("--git-dir"),
                &clone_git,
                Path::new("worktree"),
                Path::new("prune"),
            ]));
            return Err(err);
        }
        match fs::rename(&staging, workspace_root) {
            Ok(()) => Ok(()),
            Err(rename_err) => {
                drop(fs::remove_dir_all(&staging));
                drop(self.git([
                    Path::new("--git-dir"),
                    &clone_git,
                    Path::new("worktree"),
                    Path::new("prune"),
                ]));
                if workspace_root.exists() {
                    return Err(CloneStoreError::msg(format!(
                        "workspace path already exists at {workspace_root:?}"
                    )));
                }
                Err(rename_err).ctx(|| format!("move workspace into place at {workspace_root:?}"))
            }
        }
    }

    /// Builds a complete workspace under `staging`, a same-depth sibling of
    /// its final location `<parent>/<file_name>`.
    #[expect(clippy::too_many_arguments)]
    async fn build_workspace_in(
        &self,
        staging: &Path,
        file_name: &OsStr,
        clone_id: &str,
        clone_git: &Path,
        repo: &Arc<ReadonlyRepo>,
        workspace_name: &str,
        target: &Commit,
    ) -> Result<()> {
        let target_hex = target.id().hex();
        let add_args: Vec<&OsStr> = vec![
            "--git-dir".as_ref(),
            clone_git.as_os_str(),
            "worktree".as_ref(),
            "add".as_ref(),
            "--no-checkout".as_ref(),
            "--detach".as_ref(),
            staging.as_os_str(),
            target_hex.as_ref(),
        ];
        self.git(add_args)?;

        // `git worktree add` writes absolute paths into the `.git` pointer
        // file and the admin dir's back-pointer. Rewrite both to relative:
        // the pointer relative to the workspace (identical from any
        // same-depth sibling, so it survives the rename), the back-pointer
        // relative to the admin dir but naming the *final* workspace path
        // (it embeds the directory name, so it is written for where the
        // workspace will live, not where it is being built). Relative
        // pointers keep the store-plus-workspaces tree relocatable and
        // bind-mountable; see [`Self::create_workspace`].
        let pointer_path = staging.join(".git");
        let content =
            fs::read_to_string(&pointer_path).ctx(|| "read git worktree pointer".to_string())?;
        let gitdir = content
            .strip_prefix("gitdir:")
            .map(str::trim)
            .ok_or_else(|| CloneStoreError::msg("unexpected .git pointer format"))?;
        // Preserve symlinks in the caller's frame. In particular, Rho exposes
        // stores through a sibling `.stores/<name>` symlink; resolving that
        // symlink here would bake the host store location into these relative
        // pointers and make the workspace unusable when the same frame is
        // assembled under `/ws`.
        let staging_abs = std::path::absolute(staging)
            .ctx(|| "make workspace staging path absolute".to_string())?;
        // Git resolves the `--git-dir` symlink before writing `gitdir`, so
        // recover the admin directory under the path by which the caller
        // reached the clone instead of absolutizing Git's already-resolved
        // spelling.
        let admin_name = Path::new(gitdir)
            .file_name()
            .ok_or_else(|| CloneStoreError::msg("worktree admin path has no directory name"))?;
        let admin_abs = std::path::absolute(clone_git.join("worktrees").join(admin_name))
            .ctx(|| "make worktree admin path absolute".to_string())?;
        let final_abs = staging_abs.parent().unwrap().join(file_name);
        let to_admin = relative_path(&staging_abs, &admin_abs);
        fs::write(&pointer_path, format!("gitdir: {}\n", to_admin.display()))
            .ctx(|| "rewrite git worktree pointer".to_string())?;
        let to_pointer = relative_path(&admin_abs, &final_abs.join(".git"));
        fs::write(
            admin_abs.join("gitdir"),
            format!("{}\n", to_pointer.display()),
        )
        .ctx(|| "rewrite git worktree back-pointer".to_string())?;

        let name = WorkspaceNameBuf::from(workspace_name.to_owned());
        let clone_repo_path = self.clone_repo_path(clone_id)?;
        let (mut workspace, repo) = if repo.view().get_wc_commit_id(&name).is_some() {
            // The name is already in the view: a previous creation died
            // after committing its "add workspace" operation, or the
            // caller is deliberately re-pointing the name. Attach the
            // working-copy machinery without minting a working-copy commit
            // (init would check out fresh over the recorded commit and
            // trip the transaction's rebase assertion); the checkout below
            // replaces the recorded commit properly.
            let workspace = Workspace::attach_workspace_with_existing_repo(
                staging,
                &clone_repo_path,
                repo,
                default_working_copy_factory().as_ref(),
                name.clone(),
            )
            .ctx(|| "attach workspace".to_string())?;
            (workspace, repo.clone())
        } else {
            Workspace::init_workspace_with_existing_repo(
                staging,
                &clone_repo_path,
                repo,
                default_working_copy_factory().as_ref(),
                name.clone(),
            )
            .await
            .ctx(|| "init workspace".to_string())?
        };
        // Workspace initialization canonicalizes the repository path before
        // writing `.jj/repo`. Rewrite that pointer in the caller's symlink
        // frame for the same reason as the Git pointers above.
        let clone_repo_abs = std::path::absolute(&clone_repo_path)
            .ctx(|| "make clone repository path absolute".to_string())?;
        let to_repo = relative_path(&staging_abs.join(".jj"), &clone_repo_abs);
        let repo_pointer = crate::file_util::path_to_bytes(&to_repo)
            .ctx(|| "encode jj repository pointer".to_string())?;
        fs::write(staging.join(".jj/repo"), repo_pointer)
            .ctx(|| "rewrite jj repository pointer".to_string())?;

        let mut tx = repo.start_transaction();
        let wc_commit = tx
            .repo_mut()
            .check_out(name.clone(), target)
            .await
            .ctx(|| "check out target commit".to_string())?;
        // check_out abandons the placeholder working-copy commit created at
        // workspace init; settle its (empty) descendants before committing.
        tx.repo_mut()
            .rebase_descendants()
            .await
            .ctx(|| "rebase descendants after checkout".to_string())?;
        let repo = tx
            .commit(format!(
                "check out {} in {workspace_name}",
                target.id().hex()
            ))
            .await
            .ctx(|| "commit checkout".to_string())?;
        workspace
            .check_out(repo.op_id().clone(), None, &wc_commit)
            .await
            .ctx(|| "materialize working copy".to_string())?;
        // jj wrote the files; give the worktree a matching git index so
        // `git status` starts clean and jj-side edits show as unstaged.
        self.git([
            Path::new("-C"),
            &staging_abs,
            Path::new("read-tree"),
            Path::new("HEAD"),
        ])?;
        // Workspace init recorded the staging path in the clone's
        // workspace store; re-point the entry at the final location (the
        // last of the pre-rename fixups).
        SimpleWorkspaceStore::load(&clone_repo_path)
            .ctx(|| "load workspace store".to_string())?
            .add(&name, &final_abs)
            .ctx(|| "record workspace path".to_string())?;
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

/// Picks the clone's trunk: the first of main/master/trunk at `origin`,
/// falling back to any visible head.
pub fn trunk_of(repo: &Arc<ReadonlyRepo>) -> Option<CommitId> {
    let view = repo.view();
    for name in ["main", "master", "trunk"] {
        let symbol = RemoteRefSymbol {
            name: RefName::new(name),
            remote: RemoteName::new("origin"),
        };
        if let Some(id) = view.get_remote_bookmark(symbol).target.as_normal() {
            return Some(id.clone());
        }
    }
    view.heads().iter().next().cloned()
}

/// Writes a clone's bare git dir directly — the same files `git init
/// --bare` plus `git remote add --no-tags origin <url>` would produce,
/// without paying two subprocess spawns per clone. Private refs, private
/// config, origin pointing at the real remote: indistinguishable from the
/// .git of a fresh workstation clone, except that it owns no object bytes
/// (alternates borrow the store's) and its refs come verbatim from the
/// store's last-fetched state.
fn write_clone_git_dir(git_dir: &Path, remote_url: &str, ref_state: &str) -> Result<()> {
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
            "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = true\n[remote \
             \"origin\"]\n\turl = {}\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n\ttagOpt = \
             --no-tags\n",
            git_config_quote(remote_url)
        ),
    )
    .ctx(|| "write clone git config".to_string())?;
    // What colocated `jj git init` writes: keep jj's metadata out of git's
    // view in every worktree of this clone.
    fs::write(git_dir.join("info").join("exclude"), "/.jj/\n")
        .ctx(|| "write clone git exclude".to_string())?;
    // Borrow the store's objects. Relative (resolved against the objects
    // dir holding the file), so the store moving wholesale keeps working:
    // clones/<id>/git/objects -> ../../../../git/objects.
    fs::write(
        git_dir.join("objects").join("info").join("alternates"),
        "../../../../git/objects\n",
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
async fn import_git_refs(repo: &Arc<ReadonlyRepo>) -> Result<()> {
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
        .ctx(|| "commit ref import".to_string())?;
    Ok(())
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
) -> Result<()> {
    let mut tx = repo.start_transaction();
    let mut data = template.view().store_view().clone();
    data.wc_commit_ids.clear();
    data.git_heads.clear();
    data.git_head = RefTarget::absent();
    tx.repo_mut().set_view(data);
    tx.commit("import refs from template")
        .await
        .ctx(|| "commit view seed".to_string())?;
    Ok(())
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

/// Computes the relative path from `from_dir` to `to`. Both must be
/// absolute and canonical (no symlink or `..` components).
fn relative_path(from_dir: &Path, to: &Path) -> PathBuf {
    let from: Vec<_> = from_dir.components().collect();
    let to: Vec<_> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut rel = PathBuf::new();
    for _ in common..from.len() {
        rel.push("..");
    }
    for component in &to[common..] {
        rel.push(component);
    }
    if rel.as_os_str().is_empty() {
        rel.push(".");
    }
    rel
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
