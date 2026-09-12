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

//! Clone-store acceptance. The oracle: a clone behaves like a standard jj
//! repo cloned from the remote on its own machine. The constraints:
//! storage O(repo + per-clone work) — object bytes shared via alternates,
//! index bytes via reflinks or hardlinks, nothing else shared at all.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use jj_lib::clone_store::CloneStore;
use jj_lib::clone_store::StoreRoot;
use jj_lib::clone_store::store_key;
use jj_lib::clone_store::trunk_of;
use jj_lib::git;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::ReadonlyRepo;
use jj_lib::repo::Repo as _;
use jj_lib::repo::RepoLoader;
use jj_lib::repo::StoreFactories;
use pollster::FutureExt as _;

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Test", "-c", "user.email=test@localhost"])
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Objects this git dir physically owns (loose + packed), excluding
/// anything borrowed through alternates.
fn owned_objects(git_dir: &Path) -> u64 {
    let out = git_stdout(git_dir, &["count-objects", "-v"]);
    let field = |name: &str| -> u64 {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("{name}: ")))
            .unwrap()
            .parse()
            .unwrap()
    };
    field("count") + field("in-pack")
}

fn has_object(git_dir: &Path, sha: &str) -> bool {
    Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["cat-file", "-e", sha])
        .status()
        .unwrap()
        .success()
}

/// A working repo (two commits on main, an annotated tag on the first)
/// plus a bare repo standing in for the hosted remote.
fn setup_remote(root: &Path) -> (PathBuf, PathBuf) {
    let source = root.join("source");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    std::fs::write(source.join("file.txt"), "one\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "one"]);
    git(&source, &["tag", "-a", "v1", "-m", "v1"]);
    std::fs::write(source.join("file.txt"), "two\n").unwrap();
    git(&source, &["commit", "-am", "two"]);
    let remote = root.join("remote.git");
    git(
        root,
        &[
            "clone",
            "--bare",
            source.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    (source, remote)
}

async fn store_from(root: &Path, remote: &Path) -> CloneStore {
    CloneStore::init_from_remote(
        &root.join("store"),
        remote.to_str().unwrap(),
        &testutils::user_settings(),
    )
    .await
    .unwrap()
}

/// Materializes a clone at `path` and returns its repo.
async fn clone_at(store: &CloneStore, path: &Path, colocate: bool) -> Arc<ReadonlyRepo> {
    std::fs::create_dir_all(path).unwrap();
    let (_workspace, repo) = store.materialize(path, colocate).await.unwrap();
    repo
}

/// The private git dir of a clone made by [`clone_at`].
fn git_dir_of(workspace: &Path, colocate: bool) -> PathBuf {
    if colocate {
        workspace.join(".git")
    } else {
        workspace.join(".jj/repo/store/git")
    }
}

async fn reload(workspace: &Path) -> Arc<ReadonlyRepo> {
    RepoLoader::init_from_file_system(
        &testutils::user_settings(),
        &workspace.join(".jj/repo"),
        &StoreFactories::default(),
    )
    .unwrap()
    .load_at_head()
    .await
    .unwrap()
}

async fn new_commit_in(
    repo: &Arc<ReadonlyRepo>,
    what: &str,
) -> (Arc<ReadonlyRepo>, jj_lib::commit::Commit) {
    let trunk = trunk_of(repo).unwrap();
    let trunk_commit = repo.store().get_commit_async(&trunk).await.unwrap();
    let mut tx = repo.start_transaction();
    let commit = tx
        .repo_mut()
        .new_commit(vec![trunk], trunk_commit.tree())
        .write()
        .await
        .unwrap();
    let repo = tx.commit(format!("work in {what}")).await.unwrap();
    (repo, commit)
}

struct NullCallback;

impl git::GitSubprocessCallback for NullCallback {
    fn needs_progress(&self) -> bool {
        false
    }

    fn progress(&mut self, _progress: &git::GitProgress) -> std::io::Result<()> {
        Ok(())
    }

    fn local_sideband(
        &mut self,
        _message: &[u8],
        _term: Option<git::GitSidebandLineTerminator>,
    ) -> std::io::Result<()> {
        Ok(())
    }

    fn remote_sideband(
        &mut self,
        _message: &[u8],
        _term: Option<git::GitSidebandLineTerminator>,
    ) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn clones_are_independent_and_born_current() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (source, remote) = setup_remote(root);
        let store = store_from(root, &remote).await;
        let repo_a = clone_at(&store, &root.join("a"), false).await;
        clone_at(&store, &root.join("b"), false).await;

        let (_, commit) = new_commit_in(&repo_a, "a").await;

        // a's work is a's alone until pushed, like separate machines.
        let repo_b = reload(&root.join("b")).await;
        assert!(!repo_b.index().has_id(commit.id()).unwrap());
        assert!(!repo_b.view().heads().contains(commit.id()));
        // Shared history and tags came along at birth.
        let trunk = trunk_of(&repo_b).unwrap();
        assert_eq!(trunk.hex(), git_stdout(&source, &["rev-parse", "main"]));
        assert!(
            repo_b
                .view()
                .get_local_tag(jj_lib::ref_name::RefName::new("v1"))
                .is_present()
        );
        // And origin is the real remote, so pushes go to the right place.
        assert_eq!(
            git_stdout(
                &git_dir_of(&root.join("b"), false),
                &["config", "remote.origin.url"]
            ),
            remote.to_str().unwrap()
        );
        assert_eq!(
            store.default_branch().unwrap().unwrap().as_str(),
            "main"
        );
    }
    .block_on();
}

#[test]
fn object_storage_is_borrowed_not_copied() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (_source, remote) = setup_remote(root);
        let store = store_from(root, &remote).await;
        let store_git = root.join("store/git");
        let repo_a = clone_at(&store, &root.join("a"), false).await;
        clone_at(&store, &root.join("b"), false).await;
        let a_git = git_dir_of(&root.join("a"), false);
        let b_git = git_dir_of(&root.join("b"), false);

        // The O(repo) bytes exist exactly once: a newborn clone owns only
        // its working-copy commit (and that commit's empty tree), borrowing
        // everything else through alternates.
        assert_eq!(owned_objects(&a_git), 2, "clone a owns objects at birth");
        assert_eq!(owned_objects(&b_git), 2, "clone b owns objects at birth");
        // But they resolve everything.
        let trunk = git_stdout(&store_git, &["rev-parse", "refs/remotes/origin/main"]);
        assert!(has_object(&a_git, &trunk));

        // New work lands in the author's own odb — not the store, not
        // siblings.
        let (_, commit) = new_commit_in(&repo_a, "a").await;
        let sha = commit.id().hex();
        assert!(has_object(&a_git, &sha));
        assert!(owned_objects(&a_git) > 2);
        assert!(
            !has_object(&store_git, &sha),
            "clone wrote into the store odb"
        );
        assert!(!has_object(&b_git, &sha), "clone wrote into a sibling odb");
        assert_eq!(owned_objects(&b_git), 2);
    }
    .block_on();
}

#[test]
fn per_clone_gc_is_safe_for_siblings_and_store() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (_source, remote) = setup_remote(root);
        let store = store_from(root, &remote).await;
        let repo_a = clone_at(&store, &root.join("a"), false).await;
        let repo_b = clone_at(&store, &root.join("b"), false).await;

        let (_, commit_b) = new_commit_in(&repo_b, "b").await;

        // gc in a — both jj's backend gc and aggressive raw git gc —
        // touches only a's private odb.
        repo_a
            .store()
            .gc(repo_a.index(), std::time::SystemTime::now())
            .unwrap();
        git(
            &git_dir_of(&root.join("a"), false),
            &["gc", "--aggressive", "--prune=now"],
        );

        let repo_b = reload(&root.join("b")).await;
        assert!(repo_b.index().has_id(commit_b.id()).unwrap());
        assert!(has_object(
            &git_dir_of(&root.join("b"), false),
            &commit_b.id().hex()
        ));
        let trunk = trunk_of(&repo_b).unwrap();
        assert!(has_object(&root.join("store/git"), &trunk.hex()));
        // And a still resolves shared history through its alternates.
        assert!(has_object(
            &git_dir_of(&root.join("a"), false),
            &trunk.hex()
        ));
    }
    .block_on();
}

#[test]
fn template_is_lazy_and_amortized() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (source, remote) = setup_remote(root);
        let store = store_from(root, &remote).await;
        let template = root.join("store/template/repo");

        // Store init builds no template — it's a plain git mirror.
        assert!(!template.exists(), "store init built a template eagerly");

        // First clone builds it and seeds from it.
        clone_at(&store, &root.join("first"), false).await;
        assert!(template.join("store").is_dir());

        // The remote moves; the store fetches (refreshing the template as a
        // delta where freshness is produced); the next clone is born at
        // the new trunk without touching the store.
        std::fs::write(source.join("file.txt"), "three\n").unwrap();
        git(&source, &["commit", "-am", "three"]);
        git(&source, &["push", remote.to_str().unwrap(), "main"]);
        let new_sha = git_stdout(&source, &["rev-parse", "main"]);
        store.fetch().await.unwrap();
        let repo = clone_at(&store, &root.join("second"), false).await;
        let new_commit = jj_lib::backend::CommitId::try_from_hex(&new_sha).unwrap();
        assert!(repo.index().has_id(&new_commit).unwrap());
        assert_eq!(trunk_of(&repo).unwrap(), new_commit);
    }
    .block_on();
}

#[test]
fn clone_index_is_seeded_from_template_not_rebuilt() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (_source, remote) = setup_remote(root);
        let store = store_from(root, &remote).await;
        clone_at(&store, &root.join("a"), false).await;

        // Every template segment is present in the clone with identical
        // bytes (reflinked or hardlinked), and the only segment the clone
        // has beyond them is the one for its working-copy commit, so
        // nothing was indexed from scratch.
        let template_segments = root.join("store/template/repo/index/segments");
        let clone_segments = root.join("a/.jj/repo/index/segments");
        let mut template_names = std::collections::BTreeSet::new();
        for entry in std::fs::read_dir(&template_segments).unwrap() {
            let entry = entry.unwrap();
            template_names.insert(entry.file_name());
            let clone_file = clone_segments.join(entry.file_name());
            assert_eq!(
                std::fs::read(entry.path()).unwrap(),
                std::fs::read(&clone_file)
                    .unwrap_or_else(|_| panic!("segment {clone_file:?} missing from clone")),
                "segment {clone_file:?} differs from template"
            );
        }
        assert!(!template_names.is_empty(), "template has no index segments");
        let extra = std::fs::read_dir(&clone_segments)
            .unwrap()
            .filter(|entry| !template_names.contains(&entry.as_ref().unwrap().file_name()))
            .count();
        assert!(
            extra <= 1,
            "clone rebuilt {extra} segments the template lacks"
        );
    }
    .block_on();
}

#[test]
fn reindex_in_one_clone_leaves_others_intact() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (_source, remote) = setup_remote(root);
        let store = store_from(root, &remote).await;
        clone_at(&store, &root.join("a"), false).await;
        clone_at(&store, &root.join("b"), false).await;

        // Destroy a's index wholesale (stock `jj debug reindex` semantics).
        let index_store =
            jj_lib::default_index::DefaultIndexStore::load(&root.join("a/.jj/repo/index"));
        index_store.reinit().unwrap();

        // Unlinking a's files never touches b's.
        let repo_b = reload(&root.join("b")).await;
        let trunk = trunk_of(&repo_b).unwrap();
        assert!(repo_b.index().has_id(&trunk).unwrap());

        // And a rebuilds itself from its op log, stock jj, no store
        // involvement.
        let repo_a = reload(&root.join("a")).await;
        assert!(repo_a.index().has_id(&trunk).unwrap());
    }
    .block_on();
}

#[test]
fn coworkers_coordinate_through_the_remote() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (_source, remote) = setup_remote(root);
        let store = store_from(root, &remote).await;
        let repo_a = clone_at(&store, &root.join("a"), false).await;
        clone_at(&store, &root.join("b"), false).await;

        // a commits and pushes a branch to the remote from its own git —
        // objects stream through the alternates.
        let (_, commit) = new_commit_in(&repo_a, "a").await;
        let sha = commit.id().hex();
        let a_git = git_dir_of(&root.join("a"), false);
        git(
            root,
            &[
                "--git-dir",
                a_git.to_str().unwrap(),
                "push",
                "origin",
                &format!("{sha}:refs/heads/feat"),
            ],
        );

        // b fetches from the remote directly and sees feat@origin, like any
        // coworker.
        let b_git = git_dir_of(&root.join("b"), false);
        git(
            root,
            &[
                "--git-dir",
                b_git.to_str().unwrap(),
                "fetch",
                "--no-tags",
                "origin",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        );
        let repo_b = reload(&root.join("b")).await;
        let mut tx = repo_b.start_transaction();
        let options = git::GitImportOptions {
            abandon_unreachable_commits: false,
            record_synthetic_predecessors: false,
            remote_auto_track_bookmarks: std::collections::HashMap::new(),
        };
        git::import_refs(tx.repo_mut(), &options).await.unwrap();
        let repo_b = tx.commit("fetch").await.unwrap();
        assert!(repo_b.index().has_id(commit.id()).unwrap());
        let symbol = jj_lib::ref_name::RemoteRefSymbol {
            name: jj_lib::ref_name::RefName::new("feat"),
            remote: jj_lib::ref_name::RemoteName::new("origin"),
        };
        assert_eq!(
            repo_b.view().get_remote_bookmark(symbol).target.as_normal(),
            Some(commit.id())
        );
        // The store never learned any of this: its refs are origin's only.
        assert_eq!(
            git_stdout(
                &root.join("store/git"),
                &["for-each-ref", "--format=%(refname)", "refs/heads"]
            ),
            ""
        );
    }
    .block_on();
}

#[test]
fn colocated_clone_is_a_real_git_repo() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (source, remote) = setup_remote(root);
        let store = store_from(root, &remote).await;
        let ws = root.join("ws");
        clone_at(&store, &ws, true).await;

        let trunk_sha = git_stdout(&source, &["rev-parse", "main"]);
        assert_eq!(git_stdout(&ws, &["rev-parse", "--git-dir"]), ".git");
        assert_eq!(git_stdout(&ws, &["rev-parse", "origin/main"]), trunk_sha);
        assert_eq!(
            git_stdout(&ws, &["describe", "--tags", "origin/main"]),
            format!("v1-1-g{}", &trunk_sha[..7])
        );
        assert_eq!(
            git_stdout(&ws, &["ls-remote", "--get-url", "origin"]),
            remote.to_str().unwrap()
        );
        // Only the working-copy commit is owned; history is borrowed.
        assert_eq!(owned_objects(&ws.join(".git")), 2);
        // jj knows it as colocated: the backend points at the sibling .git.
        assert_eq!(
            std::fs::read_to_string(ws.join(".jj/repo/store/git_target"))
                .unwrap()
                .trim(),
            "../../../.git"
        );
    }
    .block_on();
}

#[test]
fn fetch_prunes_and_advances_the_mirror() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (source, remote) = setup_remote(root);
        let store = store_from(root, &remote).await;
        let store_git = root.join("store/git");
        // Build the template so fetch has one to refresh.
        clone_at(&store, &root.join("t"), false).await;

        git(&source, &["push", remote.to_str().unwrap(), "main:doomed"]);
        store.fetch().await.unwrap();
        std::fs::write(source.join("file.txt"), "three\n").unwrap();
        git(&source, &["commit", "-am", "three"]);
        git(&source, &["push", remote.to_str().unwrap(), "main"]);
        git(&remote, &["branch", "-D", "doomed"]);
        store.fetch().await.unwrap();

        let refs = git_stdout(&store_git, &["for-each-ref", "--format=%(refname)"]);
        assert!(!refs.contains("doomed"), "pruned branch survived: {refs}");
        assert_eq!(
            git_stdout(&store_git, &["rev-parse", "refs/remotes/origin/main"]),
            git_stdout(&source, &["rev-parse", "main"])
        );
        // Fetch refreshed the template in the same stroke.
        let ref_state = std::fs::read_to_string(root.join("store/template/ref-state")).unwrap();
        assert!(
            ref_state.contains(&git_stdout(&source, &["rev-parse", "main"])),
            "template ref-state not refreshed by fetch: {ref_state}"
        );
        // The mirror's namespaces stay pure: no refs/heads.
        assert_eq!(
            git_stdout(
                &store_git,
                &["for-each-ref", "--format=%(refname)", "refs/heads"]
            ),
            ""
        );
    }
    .block_on();
}

#[test]
fn clone_fetches_from_the_mirror_without_the_network() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (source, remote) = setup_remote(root);
        let store = store_from(root, &remote).await;
        let ws = root.join("ws");
        clone_at(&store, &ws, false).await;
        let owned_at_birth = owned_objects(&git_dir_of(&ws, false));

        // The remote moves and the store (the server, in practice) fetches.
        std::fs::write(source.join("file.txt"), "three\n").unwrap();
        git(&source, &["commit", "-am", "three"]);
        git(&source, &["push", remote.to_str().unwrap(), "main"]);
        git(&source, &["push", remote.to_str().unwrap(), "main:topic"]);
        let new_sha = git_stdout(&source, &["rev-parse", "main"]);
        store.fetch().await.unwrap();
        // Then the remote vanishes: whatever the clone learns now came
        // from the mirror.
        std::fs::remove_dir_all(&remote).unwrap();

        let repo = reload(&ws).await;
        let mut tx = repo.start_transaction();
        let import_options = git::GitImportOptions {
            abandon_unreachable_commits: false,
            record_synthetic_predecessors: false,
            remote_auto_track_bookmarks: std::collections::HashMap::new(),
        };
        let subprocess = git::GitSubprocessOptions::from_settings(&testutils::user_settings())
            .unwrap();
        let origin = jj_lib::ref_name::RemoteName::new("origin");
        let mut fetch = git::GitFetch::new(tx.repo_mut(), subprocess, &import_options).unwrap();
        let expanded = git::expand_fetch_refspecs(
            origin,
            git::GitFetchRefExpression {
                bookmark: jj_lib::str_util::StringExpression::all(),
                tag: jj_lib::str_util::StringExpression::all(),
            },
        )
        .unwrap();
        fetch
            .fetch_from_mirror(origin, &store.git_dir(), expanded, &mut NullCallback)
            .unwrap();
        let stats = fetch.import_refs().await.unwrap();
        assert!(!stats.changed_remote_bookmarks.is_empty());
        let repo = tx.commit("fetch").await.unwrap();

        let new_commit = jj_lib::backend::CommitId::try_from_hex(&new_sha).unwrap();
        assert_eq!(trunk_of(&repo).unwrap(), new_commit);
        let topic = jj_lib::ref_name::RemoteRefSymbol {
            name: jj_lib::ref_name::RefName::new("topic"),
            remote: origin,
        };
        assert_eq!(
            repo.view().get_remote_bookmark(topic).target.as_normal(),
            Some(&new_commit)
        );
        // Nothing was copied: the new objects are still borrowed.
        assert_eq!(owned_objects(&git_dir_of(&ws, false)), owned_at_birth);
    }
    .block_on();
}

#[test]
fn stale_template_is_still_a_valid_seed() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (source, remote) = setup_remote(root);
        let store = store_from(root, &remote).await;
        clone_at(&store, &root.join("first"), false).await;

        // The mirror advances without the template following (a fetch by
        // hand), and the template cannot be refreshed here: its lock is
        // not writable, as for a client of a read-only store.
        std::fs::write(source.join("file.txt"), "three\n").unwrap();
        git(&source, &["commit", "-am", "three"]);
        git(&source, &["push", remote.to_str().unwrap(), "main"]);
        let new_sha = git_stdout(&source, &["rev-parse", "main"]);
        git(
            &root.join("store/git"),
            &[
                "fetch",
                "--no-tags",
                "origin",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        );
        let lock = root.join("store/template.lock");
        let original = std::fs::metadata(&lock).unwrap().permissions();
        let mut permissions = original.clone();
        permissions.set_readonly(true);
        std::fs::set_permissions(&lock, permissions).unwrap();

        let repo = clone_at(&store, &root.join("second"), false).await;
        let new_commit = jj_lib::backend::CommitId::try_from_hex(&new_sha).unwrap();
        assert_eq!(trunk_of(&repo).unwrap(), new_commit);
        assert!(repo.index().has_id(&new_commit).unwrap());
        // The template itself was left alone.
        let ref_state = std::fs::read_to_string(root.join("store/template/ref-state")).unwrap();
        assert!(!ref_state.contains(&new_sha));

        std::fs::set_permissions(&lock, original).unwrap();
    }
    .block_on();
}

#[test]
fn store_root_keys_urls_and_ensures_freshness() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (source, remote) = setup_remote(root);
        let url = remote.to_str().unwrap();
        let stores = StoreRoot::new(root.join("stores"), testutils::user_settings());
        assert!(stores.open(url).unwrap().is_none());
        assert!(stores.list().unwrap().is_empty());

        let store = stores.ensure(url).await.unwrap();
        assert_eq!(store.root(), root.join("stores").join(store_key(url)));
        assert_eq!(store.remote_url().unwrap(), url);
        assert_eq!(stores.list().unwrap().len(), 1);
        assert!(stores.open(&format!("{url}/")).unwrap().is_some());
        assert!(stores.open("https://example.com/other").unwrap().is_none());

        // ensure on an existing store fetches it.
        std::fs::write(source.join("file.txt"), "three\n").unwrap();
        git(&source, &["commit", "-am", "three"]);
        git(&source, &["push", url, "main"]);
        let new_sha = git_stdout(&source, &["rev-parse", "main"]);
        let store = stores.ensure(url).await.unwrap();
        assert_eq!(
            git_stdout(&store.git_dir(), &["rev-parse", "refs/remotes/origin/main"]),
            new_sha
        );
    }
    .block_on();
}

#[test]
fn interrupted_store_init_leaves_root_usable() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (_source, remote) = setup_remote(root);
        let store_root = root.join("store");

        // Wreckage of an init that died mid-build: a staging sibling with a
        // partial git dir. The root must stay free and the wreckage inert.
        let staging = root.join(".incoming-store-1234-5678");
        std::fs::create_dir_all(staging.join("git")).unwrap();

        let store = CloneStore::init_from_remote(
            &store_root,
            remote.to_str().unwrap(),
            &testutils::user_settings(),
        )
        .await
        .unwrap();
        clone_at(&store, &root.join("a"), false).await;
        assert!(
            staging.exists(),
            "stale staging is left for gc, not adopted"
        );

        // A partial store (git dir, no marker) is refused by open, and init
        // onto a non-empty root reports "already exists".
        let partial = root.join("partial");
        std::fs::create_dir_all(partial.join("git")).unwrap();
        assert!(CloneStore::open(&partial, &testutils::user_settings()).is_err());
        let Err(err) = CloneStore::init_from_remote(
            &partial,
            remote.to_str().unwrap(),
            &testutils::user_settings(),
        )
        .await
        else {
            panic!("init onto non-empty root must fail");
        };
        assert!(err.to_string().contains("already exists"), "got: {err}");

        // Init onto a pre-created *empty* dir works (rename replaces it).
        let empty = root.join("empty");
        std::fs::create_dir(&empty).unwrap();
        CloneStore::init_from_remote(
            &empty,
            remote.to_str().unwrap(),
            &testutils::user_settings(),
        )
        .await
        .unwrap();
        CloneStore::open(&empty, &testutils::user_settings()).unwrap();

        // Wreckage of a template build that died mid-way is inert too.
        let template_staging = root.join("store/.incoming-template-1234-5678");
        std::fs::create_dir_all(template_staging.join("repo")).unwrap();
        clone_at(&store, &root.join("b"), false).await;
        assert!(template_staging.exists());
    }
    .block_on();
}

#[cfg(unix)]
#[test]
fn server_owns_the_root_and_clients_share_one_store() {
    use std::time::Duration;

    use jj_lib::clone_store_server::StoreServer;
    use jj_lib::clone_store_server::request;

    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (source, remote) = setup_remote(root);
        let url = remote.to_str().unwrap().to_owned();
        let stores = StoreRoot::new(root.join("stores"), testutils::user_settings());
        let server = StoreServer::new(stores, Duration::from_secs(3600));
        let socket = root.join("store.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        {
            let server = Arc::clone(&server);
            std::thread::spawn(move || server.serve(listener).unwrap());
        }

        // Two clients ask for the same URL at once: one store, initialized
        // once, with its template built by the server.
        let clients: Vec<_> = (0..2)
            .map(|_| {
                let socket = socket.clone();
                let url = url.clone();
                std::thread::spawn(move || request(&socket, "ensure", &url))
            })
            .collect();
        let paths: Vec<PathBuf> = clients
            .into_iter()
            .map(|client| client.join().unwrap().unwrap())
            .collect();
        assert_eq!(paths[0], paths[1]);
        assert_eq!(paths[0], root.join("stores").join(store_key(&url)));
        assert!(paths[0].join("template/repo/store").is_dir());
        assert_eq!(
            std::fs::read_dir(root.join("stores"))
                .unwrap()
                .filter(|entry| !entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with('.'))
                .count(),
            1
        );

        // A client clones from the served store without writing to it.
        let store = CloneStore::open(&paths[0], &testutils::user_settings()).unwrap();
        let repo = clone_at(&store, &root.join("ws"), false).await;
        assert_eq!(
            trunk_of(&repo).unwrap().hex(),
            git_stdout(&source, &["rev-parse", "main"])
        );

        // Within the debounce window a request does not fetch; a refresh
        // after the window does. (The server's own clock, so expire it by
        // asking through a zero-debounce server on the same root.)
        std::fs::write(source.join("file.txt"), "three\n").unwrap();
        git(&source, &["commit", "-am", "three"]);
        git(&source, &["push", &url, "main"]);
        let new_sha = git_stdout(&source, &["rev-parse", "main"]);
        request(&socket, "refresh", &url).unwrap();
        assert_ne!(
            git_stdout(&store.git_dir(), &["rev-parse", "refs/remotes/origin/main"]),
            new_sha,
            "debounced request must not fetch"
        );
        let eager = StoreServer::new(
            StoreRoot::new(root.join("stores"), testutils::user_settings()),
            Duration::ZERO,
        );
        eager.ensure(&url).unwrap();
        assert_eq!(
            git_stdout(&store.git_dir(), &["rev-parse", "refs/remotes/origin/main"]),
            new_sha
        );
        let ref_state = std::fs::read_to_string(paths[0].join("template/ref-state")).unwrap();
        assert!(ref_state.contains(&new_sha), "server did not refresh the template");

        // Malformed and failing requests are reported, not dropped.
        assert!(server.handle_request("bogus\n").starts_with("error "));
        let err = request(&socket, "ensure", root.join("nowhere").to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("clone store server"), "{err}");
    }
    .block_on();
}
