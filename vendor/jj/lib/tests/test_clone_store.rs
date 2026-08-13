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
//! index bytes via hardlinks, nothing else shared at all.

use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use jj_lib::clone_store::CloneStore;
use jj_lib::clone_store::trunk_of;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::Repo as _;
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

async fn new_commit_in(
    store: &CloneStore,
    id: &str,
) -> (Arc<jj_lib::repo::ReadonlyRepo>, jj_lib::commit::Commit) {
    let repo = store.open_clone(id).await.unwrap();
    let trunk = trunk_of(&repo).unwrap();
    let trunk_commit = repo.store().get_commit_async(&trunk).await.unwrap();
    let mut tx = repo.start_transaction();
    let commit = tx
        .repo_mut()
        .new_commit(vec![trunk], trunk_commit.tree())
        .write()
        .await
        .unwrap();
    let repo = tx.commit(format!("work in {id}")).await.unwrap();
    (repo, commit)
}

#[test]
fn clones_are_independent_and_born_current() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (source, remote) = setup_remote(root);
        let store = store_from(root, &remote).await;
        store.create_clone("a").await.unwrap();
        store.create_clone("b").await.unwrap();

        let (_, commit) = new_commit_in(&store, "a").await;

        // a's work is a's alone until pushed, like separate machines.
        let repo_b = store.open_clone("b").await.unwrap();
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
        store.create_clone("a").await.unwrap();
        store.create_clone("b").await.unwrap();
        let a_git = root.join("store/clones/a/git");
        let b_git = root.join("store/clones/b/git");

        // The O(repo) bytes exist exactly once: newborn clones own zero
        // objects, borrowing everything through alternates.
        assert_eq!(owned_objects(&a_git), 0, "clone a owns objects at birth");
        assert_eq!(owned_objects(&b_git), 0, "clone b owns objects at birth");
        // But they resolve everything.
        let trunk = git_stdout(&store_git, &["rev-parse", "refs/remotes/origin/main"]);
        assert!(has_object(&a_git, &trunk));

        // New work lands in the author's own odb — not the store, not
        // siblings.
        let (_, commit) = new_commit_in(&store, "a").await;
        let sha = commit.id().hex();
        assert!(has_object(&a_git, &sha));
        assert!(owned_objects(&a_git) > 0);
        assert!(
            !has_object(&store_git, &sha),
            "clone wrote into the store odb"
        );
        assert!(!has_object(&b_git, &sha), "clone wrote into a sibling odb");
        assert_eq!(owned_objects(&b_git), 0);
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
        store.create_clone("a").await.unwrap();
        store.create_clone("b").await.unwrap();

        let (_, commit_b) = new_commit_in(&store, "b").await;

        // gc in a — both jj's backend gc and aggressive raw git gc —
        // touches only a's private odb. This used to be the data-loss
        // scenario; with private git dirs it's just stock behavior.
        let repo_a = store.open_clone("a").await.unwrap();
        repo_a
            .store()
            .gc(repo_a.index(), std::time::SystemTime::now())
            .unwrap();
        git(
            &root.join("store/clones/a/git"),
            &["gc", "--aggressive", "--prune=now"],
        );

        let repo_b = store.open_clone("b").await.unwrap();
        assert!(repo_b.index().has_id(commit_b.id()).unwrap());
        assert!(has_object(
            &root.join("store/clones/b/git"),
            &commit_b.id().hex()
        ));
        let trunk = trunk_of(&repo_b).unwrap();
        assert!(has_object(&root.join("store/git"), &trunk.hex()));
        // And a still resolves shared history through its alternates.
        assert!(has_object(&root.join("store/clones/a/git"), &trunk.hex()));
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

        // First clone creation builds it; the clone seeds from it.
        store.create_clone("first").await.unwrap();
        assert!(template.join("store").is_dir());

        // The remote moves; the store prefetches (plain git fetch, no
        // template work); the next creation refreshes the template as a
        // delta and the clone is born at the new trunk.
        std::fs::write(source.join("file.txt"), "three\n").unwrap();
        git(&source, &["commit", "-am", "three"]);
        git(&source, &["push", remote.to_str().unwrap(), "main"]);
        let new_sha = git_stdout(&source, &["rev-parse", "main"]);
        store.fetch().await.unwrap();
        store.create_clone("second").await.unwrap();
        let repo = store.open_clone("second").await.unwrap();
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
        store.create_clone("a").await.unwrap();

        // Every template segment is present in the clone with identical
        // bytes: shared by reflink where the filesystem supports it (a new
        // inode), by hardlink otherwise (the same inode). Either way the
        // clone did not rebuild them, which the second check proves: the
        // clone owns no segment the template does not have, so nothing was
        // indexed from scratch (the root-only segment every fresh repo
        // writes identically is in both sets).
        let template_segments = root.join("store/template/repo/index/segments");
        let clone_segments = root.join("store/clones/a/repo/index/segments");
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
        for entry in std::fs::read_dir(&clone_segments).unwrap() {
            let name = entry.unwrap().file_name();
            assert!(
                template_names.contains(&name),
                "clone rebuilt a segment the template lacks: {name:?}"
            );
        }
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
        store.create_clone("a").await.unwrap();
        store.create_clone("b").await.unwrap();

        // Destroy a's index wholesale (stock `jj debug reindex` semantics:
        // unlink segments and op links, rebuild from own op history on
        // load).
        let index_store =
            jj_lib::default_index::DefaultIndexStore::load(&root.join("store/clones/a/repo/index"));
        index_store.reinit().unwrap();

        // Hardlink semantics: unlinking a's files never touches b's.
        let repo_b = store.open_clone("b").await.unwrap();
        let trunk = trunk_of(&repo_b).unwrap();
        assert!(repo_b.index().has_id(&trunk).unwrap());

        // And a rebuilds itself from its op log, stock jj, no store
        // involvement.
        let repo_a = store.open_clone("a").await.unwrap();
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
        store.create_clone("a").await.unwrap();
        store.create_clone("b").await.unwrap();

        // a commits and pushes a branch to the remote from its own git —
        // objects stream through the alternates.
        let (_, commit) = new_commit_in(&store, "a").await;
        let sha = commit.id().hex();
        let a_git = root.join("store/clones/a/git");
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

        // b fetches (into its own git; the store's buffer is irrelevant
        // here) and sees feat@origin, like any coworker.
        let b_git = root.join("store/clones/b/git");
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
        let repo_b = store.open_clone("b").await.unwrap();
        let mut tx = repo_b.start_transaction();
        let options = jj_lib::git::GitImportOptions {
            abandon_unreachable_commits: false,
            record_synthetic_predecessors: false,
            remote_auto_track_bookmarks: std::collections::HashMap::new(),
        };
        jj_lib::git::import_refs(tx.repo_mut(), &options)
            .await
            .unwrap();
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
fn workspaces_are_real_colocated_git_checkouts() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (source, remote) = setup_remote(root);
        let store = store_from(root, &remote).await;
        store.create_clone("a").await.unwrap();
        let ws = root.join("store/ws");
        store
            .create_workspace("a", &ws, "default", None)
            .await
            .unwrap();

        let trunk_sha = git_stdout(&source, &["rev-parse", "main"]);
        // A real git checkout: everything works, because it is one.
        assert_eq!(git_stdout(&ws, &["rev-parse", "HEAD"]), trunk_sha);
        assert_eq!(
            git_stdout(&ws, &["rev-parse", "--show-toplevel"]),
            ws.to_str().unwrap()
        );
        assert_eq!(git_stdout(&ws, &["status", "--porcelain"]), "");
        assert_eq!(git_stdout(&ws, &["rev-parse", "origin/main"]), trunk_sha);
        assert_eq!(
            git_stdout(&ws, &["describe", "--tags"]),
            format!("v1-1-g{}", &trunk_sha[..7])
        );
        assert_eq!(git_stdout(&ws, &["ls-files"]), "file.txt");
        assert_eq!(git_stdout(&ws, &["log", "-1", "--format=%s"]), "two");

        // jj-side edits appear as unstaged changes.
        std::fs::write(ws.join("file.txt"), "three\n").unwrap();
        assert_eq!(git_stdout(&ws, &["status", "--porcelain"]), "M file.txt");

        // Colocated for real: the worktree's common dir is the clone's git.
        let common = git_stdout(&ws, &["rev-parse", "--git-common-dir"]);
        assert_eq!(
            std::fs::canonicalize(ws.join(&common)).unwrap(),
            std::fs::canonicalize(root.join("store/clones/a/git")).unwrap()
        );
    }
    .block_on();
}

#[test]
fn interrupted_workspace_creation_leaves_path_usable() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (_source, remote) = setup_remote(root);
        let store = store_from(root, &remote).await;
        store.create_clone("a").await.unwrap();
        let clone_git = root.join("store/clones/a/git");
        let sha = git_stdout(&clone_git, &["rev-parse", "refs/remotes/origin/main"]);

        // Wreckage of a creation that died before its rename: a staging
        // sibling of the target, and a worktree registration whose target
        // path no longer exists (the back-pointer is written for the final
        // path before the rename, so a crash leaves it dangling).
        let ws = root.join("store/ws");
        let staging = root.join("store/.incoming-ws-1234-5678");
        std::fs::create_dir_all(staging.join(".jj")).unwrap();
        let gone = root.join("store/gone");
        git(
            root,
            &[
                "--git-dir",
                clone_git.to_str().unwrap(),
                "worktree",
                "add",
                "--no-checkout",
                "--detach",
                gone.to_str().unwrap(),
                &sha,
            ],
        );
        std::fs::remove_dir_all(&gone).unwrap();

        // The target path stays usable: creation prunes the dangling
        // registration and builds in its own staging dir.
        store
            .create_workspace("a", &ws, "default", None)
            .await
            .unwrap();
        assert_eq!(git_stdout(&ws, &["status", "--porcelain"]), "");
        assert_eq!(git_stdout(&ws, &["rev-parse", "HEAD"]), sha);
        assert!(
            staging.exists(),
            "stale staging is left for gc, not adopted"
        );

        // A complete workspace refuses to be overwritten.
        let err = store
            .create_workspace("a", &ws, "other", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "got: {err}");
        assert_eq!(git_stdout(&ws, &["status", "--porcelain"]), "");

        // Creation onto a pre-created *empty* dir works (rename replaces
        // it), matching store-init semantics.
        let ws2 = root.join("store/ws2");
        std::fs::create_dir(&ws2).unwrap();
        store
            .create_workspace("a", &ws2, "second", None)
            .await
            .unwrap();
        assert_eq!(git_stdout(&ws2, &["rev-parse", "HEAD"]), sha);
    }
    .block_on();
}

#[test]
fn workspace_retry_after_committed_add_operation() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (_source, remote) = setup_remote(root);
        let store = store_from(root, &remote).await;
        store.create_clone("a").await.unwrap();
        let clone_git = root.join("store/clones/a/git");

        // A creation that died after committing its "add workspace"
        // operation leaves the name in the clone's view with no working
        // copy on disk. Simulate exactly that state: build a workspace,
        // then delete its directory and prune the worktree registration.
        let ws = root.join("store/ws");
        store
            .create_workspace("a", &ws, "revived", None)
            .await
            .unwrap();
        std::fs::remove_dir_all(&ws).unwrap();
        git(
            root,
            &[
                "--git-dir",
                clone_git.to_str().unwrap(),
                "worktree",
                "prune",
            ],
        );

        // Retrying the same name and path must succeed (this attaches to
        // the recorded name rather than re-initializing it).
        store
            .create_workspace("a", &ws, "revived", None)
            .await
            .unwrap();
        assert_eq!(git_stdout(&ws, &["status", "--porcelain"]), "");

        // The same name can also be re-pointed at a different path.
        let ws2 = root.join("store/ws2");
        store
            .create_workspace("a", &ws2, "revived", None)
            .await
            .unwrap();
        assert_eq!(git_stdout(&ws2, &["status", "--porcelain"]), "");
    }
    .block_on();
}

#[test]
fn workspace_for_missing_clone_reports_clearly() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (_source, remote) = setup_remote(root);
        let store = store_from(root, &remote).await;

        let err = store
            .create_workspace("nope", &root.join("ws"), "default", None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("does not exist"), "got: {err}");
        assert!(!root.join("ws").exists());
    }
    .block_on();
}

#[test]
fn family_workspaces_share_one_clone_view() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (_source, remote) = setup_remote(root);
        let store = store_from(root, &remote).await;
        store.create_clone("family").await.unwrap();

        let ws_parent = root.join("store/ws-parent");
        let ws_child = root.join("store/ws-child");
        store
            .create_workspace("family", &ws_parent, "parent", None)
            .await
            .unwrap();
        store
            .create_workspace("family", &ws_child, "child", None)
            .await
            .unwrap();

        let repo = store.open_clone("family").await.unwrap();
        assert_eq!(repo.view().wc_commit_ids().len(), 2);
        assert_eq!(
            std::fs::read_to_string(ws_child.join("file.txt")).unwrap(),
            "two\n"
        );
        assert_eq!(
            git_stdout(&ws_parent, &["rev-parse", "HEAD"]),
            git_stdout(&ws_child, &["rev-parse", "HEAD"]),
        );
    }
    .block_on();
}

#[test]
fn store_and_contents_survive_moving_wholesale() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (source, remote) = setup_remote(root);
        let site = root.join("site");
        std::fs::create_dir(&site).unwrap();
        let store = CloneStore::init_from_remote(
            &site.join("store"),
            remote.to_str().unwrap(),
            &testutils::user_settings(),
        )
        .await
        .unwrap();
        store.create_clone("a").await.unwrap();
        let ws = site.join("store/ws");
        store
            .create_workspace("a", &ws, "default", None)
            .await
            .unwrap();

        // Alternates and worktree pointers are all layout-relative: the
        // store moving wholesale (workspace inside) keeps everything
        // resolving. The same property lets a namespace expose the tree at
        // any mount root, as long as relative positions are preserved.
        let moved = root.join("moved");
        std::fs::rename(&site, &moved).unwrap();
        let ws = moved.join("store/ws");
        assert_eq!(git_stdout(&ws, &["status", "--porcelain"]), "");
        assert_eq!(
            git_stdout(&ws, &["rev-parse", "HEAD"]),
            git_stdout(&source, &["rev-parse", "main"])
        );
        let store = CloneStore::open(&moved.join("store"), &testutils::user_settings()).unwrap();
        let repo = store.open_clone("a").await.unwrap();
        assert!(trunk_of(&repo).is_some());
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
        store.create_clone("t").await.unwrap();

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
        // Fetch refreshed the template in the same stroke: freshness is
        // produced where refs change, so clone creation only reads.
        let ref_state = std::fs::read_to_string(root.join("store/template/ref-state")).unwrap();
        assert!(
            ref_state.contains(&git_stdout(&source, &["rev-parse", "main"])),
            "template ref-state not refreshed by fetch: {ref_state}"
        );
        // The mirror's namespaces stay pure: no refs/heads, tags only under
        // refs/tags (plus remote-tracking); nothing is anybody's local
        // state.
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
        store.create_clone("a").await.unwrap();
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
    }
    .block_on();
}

#[test]
fn interrupted_creation_leaves_id_usable() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (_source, remote) = setup_remote(root);
        let store = store_from(root, &remote).await;

        // Wreckage of a creation that died mid-build, and of a template
        // build that died mid-way: both inert, both ids/paths stay usable.
        let clone_staging = root.join("store/clones/.incoming-crashseed-1234-5678");
        std::fs::create_dir_all(clone_staging.join("repo")).unwrap();
        let template_staging = root.join("store/.incoming-template-1234-5678");
        std::fs::create_dir_all(template_staging.join("repo")).unwrap();

        let repo_path = store.create_clone("crashseed").await.unwrap();
        assert!(
            repo_path.join("store/git_target").exists(),
            "clone is complete"
        );
        let repo = store.open_clone("crashseed").await.unwrap();
        assert!(trunk_of(&repo).is_some());
        assert!(
            clone_staging.exists(),
            "stale staging is left for gc, not adopted"
        );
        assert!(template_staging.exists());
    }
    .block_on();
}

#[test]
fn traversal_clone_ids_are_rejected_without_side_effects() {
    async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let (_source, remote) = setup_remote(root);
        let store = store_from(root, &remote).await;
        let store_root = root.join("store");

        for id in [
            "../../outside",
            "../escaped",
            "",
            ".",
            "..",
            ".hidden",
            "a/b",
            "-flag",
        ] {
            let before: Vec<_> = std::fs::read_dir(root)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert!(
                store.create_clone(id).await.is_err(),
                "id {id:?} should be rejected"
            );
            let after: Vec<_> = std::fs::read_dir(root)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(before, after, "id {id:?} left filesystem debris");
            assert!(!root.join("outside").exists());
            assert!(!store_root.join("escaped").exists());
        }
    }
    .block_on();
}
