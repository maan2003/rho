//! Rho's patched git against a live keeper: clones are born from the
//! mirror, fetches read the refreshed mirror of whatever they name, and
//! nothing is copied into a pack of the clone's own. Needs `RHO_GIT`.

mod common;

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use common::{GitDaemon, git, patched_git, push_commit, setup_remote};
use rho_git_proto::SOCKET_ENV;
use rho_git_server::{MirrorStore, Refresh};

fn rho_git(cwd: &Path, socket: Option<&Path>, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(patched_git().unwrap());
    command
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .env_remove(SOCKET_ENV);
    if let Some(socket) = socket {
        command.env(SOCKET_ENV, socket);
    }
    command.output().unwrap()
}

fn ok(output: std::process::Output) -> String {
    assert!(
        output.status.success(),
        "git failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

struct Keeper {
    store: Arc<MirrorStore>,
    socket: std::path::PathBuf,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Drop for Keeper {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn keeper(temp: &Path) -> Keeper {
    let store = MirrorStore::new(
        temp.join("stores"),
        "git",
        Vec::new(),
        Refresh {
            debounce: Duration::ZERO,
        },
    );
    let socket = temp.join("store.sock");
    let listener = MirrorStore::bind(&socket).unwrap();
    let task = tokio::spawn(Arc::clone(&store).serve(listener));
    Keeper {
        store,
        socket,
        task,
    }
}

fn no_own_pack(clone: &Path) -> bool {
    clone
        .join(".git/objects/pack")
        .read_dir()
        .unwrap()
        .next()
        .is_none()
}

fn alternates(clone: &Path) -> Vec<String> {
    std::fs::read_to_string(clone.join(".git/objects/info/alternates"))
        .unwrap_or_default()
        .lines()
        .map(ToOwned::to_owned)
        .collect()
}

macro_rules! needs_patched_git {
    () => {
        if patched_git().is_none() {
            eprintln!("RHO_GIT is not set: skipping");
            return;
        }
    };
}

#[tokio::test(flavor = "multi_thread")]
async fn clone_and_fetch_go_through_the_store() {
    needs_patched_git!();
    let temp = tempfile::tempdir().unwrap();
    let (source, remote) = setup_remote(temp.path());
    let daemon = GitDaemon::start(temp.path());
    let url = daemon.url("remote.git");
    let keeper = keeper(temp.path());
    let work = temp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    ok(rho_git(&work, Some(&keeper.socket), &["clone", "-q", &url]));
    let clone = work.join("remote");
    assert_eq!(
        std::fs::read_to_string(clone.join("file.txt")).unwrap(),
        "one\n"
    );
    let mirror = keeper.store.mirror_dir(&url);
    assert_eq!(
        alternates(&clone),
        vec![mirror.join("objects").to_str().unwrap()]
    );
    assert_eq!(
        git(&clone, &["config", "--get", "remote.origin.url"]).trim(),
        url
    );
    assert_eq!(git(&clone, &["branch", "--show-current"]).trim(), "main");
    assert_eq!(
        git(&clone, &["rev-parse", "--abbrev-ref", "@{upstream}"]).trim(),
        "origin/main"
    );
    assert!(
        no_own_pack(&clone),
        "a clone is born borrowing, not copying"
    );

    // A named directory, reached through -C.
    ok(rho_git(
        temp.path(),
        Some(&keeper.socket),
        &["-C", "work", "clone", "-q", &url, "named"],
    ));
    assert!(work.join("named/file.txt").is_file());

    // The remote moves; a fetch sees it through the refreshed mirror.
    let second = push_commit(&source, "two\n");
    ok(rho_git(&clone, Some(&keeper.socket), &["fetch", "-q"]));
    assert_eq!(git(&clone, &["rev-parse", "origin/main"]).trim(), second);
    assert_eq!(
        git(&mirror, &["rev-parse", "refs/heads/main"]).trim(),
        second
    );
    assert!(no_own_pack(&clone));

    // pull too, and the checkout follows.
    let third = push_commit(&source, "three\n");
    ok(rho_git(
        &clone,
        Some(&keeper.socket),
        &["pull", "-q", "--ff-only"],
    ));
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]).trim(), third);
    assert!(no_own_pack(&clone));

    // Pushing is untouched: straight to the remote.
    std::fs::write(clone.join("file.txt"), "four\n").unwrap();
    ok(rho_git(
        &clone,
        Some(&keeper.socket),
        &["commit", "-q", "-am", "four"],
    ));
    ok(rho_git(&clone, Some(&keeper.socket), &["push", "-q"]));
    assert_eq!(
        git(&remote, &["rev-parse", "main"]).trim(),
        git(&clone, &["rev-parse", "HEAD"]).trim()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn every_remote_gets_its_own_mirror() {
    needs_patched_git!();
    let temp = tempfile::tempdir().unwrap();
    let (source, _remote) = setup_remote(temp.path());
    // A second remote: a fork with a branch of its own.
    let fork = temp.path().join("fork.git");
    git(temp.path(), &["init", "-q", "--bare", "fork.git"]);
    git(&source, &["remote", "add", "fork", fork.to_str().unwrap()]);
    git(&source, &["push", "-q", "fork", "main:main"]);
    std::fs::write(source.join("fork.txt"), "fork\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-q", "-m", "fork"]);
    let fork_tip = git(&source, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&source, &["push", "-q", "fork", "HEAD:refs/heads/feature"]);
    let daemon = GitDaemon::start(temp.path());
    let url = daemon.url("remote.git");
    let fork_url = daemon.url("fork.git");
    let keeper = keeper(temp.path());
    let work = temp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    ok(rho_git(&work, Some(&keeper.socket), &["clone", "-q", &url]));
    let clone = work.join("remote");
    ok(rho_git(
        &clone,
        Some(&keeper.socket),
        &["remote", "add", "fork", &fork_url],
    ));

    // A named remote: its mirror is made, joins the alternates, and the
    // objects are borrowed rather than packed.
    ok(rho_git(
        &clone,
        Some(&keeper.socket),
        &["fetch", "-q", "fork"],
    ));
    assert_eq!(git(&clone, &["rev-parse", "fork/feature"]).trim(), fork_tip);
    let fork_mirror = keeper.store.mirror_dir(&fork_url);
    assert!(fork_mirror.join("HEAD").is_file());
    assert_eq!(
        alternates(&clone),
        vec![
            keeper
                .store
                .mirror_dir(&url)
                .join("objects")
                .to_str()
                .unwrap(),
            fork_mirror.join("objects").to_str().unwrap(),
        ]
    );
    assert!(no_own_pack(&clone));
    assert_eq!(keeper.store.list().unwrap().len(), 2);

    // A literal URL with a refspec, after the fork moved.
    std::fs::write(source.join("fork.txt"), "more\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-q", "-m", "more"]);
    let fork_next = git(&source, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&source, &["push", "-q", "fork", "HEAD:refs/heads/feature"]);
    ok(rho_git(
        &clone,
        Some(&keeper.socket),
        &["fetch", "-q", &fork_url, "feature"],
    ));
    assert_eq!(git(&clone, &["rev-parse", "FETCH_HEAD"]).trim(), fork_next);
    assert_eq!(alternates(&clone).len(), 2, "a known mirror is listed once");
    assert!(no_own_pack(&clone));

    // No argument: the branch's upstream remote.
    ok(rho_git(
        &clone,
        Some(&keeper.socket),
        &["checkout", "-q", "-b", "feature", "--track", "fork/feature"],
    ));
    std::fs::write(source.join("fork.txt"), "again\n").unwrap();
    git(&source, &["commit", "-q", "-am", "again"]);
    let fork_last = git(&source, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&source, &["push", "-q", "fork", "HEAD:refs/heads/feature"]);
    ok(rho_git(
        &clone,
        Some(&keeper.socket),
        &["pull", "-q", "--ff-only"],
    ));
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]).trim(), fork_last);
    assert!(no_own_pack(&clone));

    // --all and --multiple run one child per remote; each child asks the
    // keeper itself.
    let origin_next = push_commit(&source, "origin moves\n");
    std::fs::write(source.join("fork.txt"), "fork moves\n").unwrap();
    git(&source, &["commit", "-q", "-am", "fork moves"]);
    let fork_moved = git(&source, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&source, &["push", "-q", "fork", "HEAD:refs/heads/feature"]);
    ok(rho_git(
        &clone,
        Some(&keeper.socket),
        &["fetch", "-q", "--all"],
    ));
    assert_eq!(
        git(&clone, &["rev-parse", "origin/main"]).trim(),
        origin_next
    );
    assert_eq!(
        git(&clone, &["rev-parse", "fork/feature"]).trim(),
        fork_moved
    );
    assert!(no_own_pack(&clone));
    let origin_last = push_commit(&source, "origin again\n");
    ok(rho_git(
        &clone,
        Some(&keeper.socket),
        &["fetch", "-q", "--multiple", "origin", "fork"],
    ));
    assert_eq!(
        git(&clone, &["rev-parse", "origin/main"]).trim(),
        origin_last
    );
    assert!(no_own_pack(&clone));
}

#[tokio::test(flavor = "multi_thread")]
async fn subtree_add_and_pull_read_the_mirror() {
    needs_patched_git!();
    let temp = tempfile::tempdir().unwrap();
    let (source, _remote) = setup_remote(temp.path());
    let daemon = GitDaemon::start(temp.path());
    let url = daemon.url("remote.git");
    let keeper = keeper(temp.path());
    let app = temp.path().join("app");
    std::fs::create_dir_all(&app).unwrap();
    git(&app, &["init", "-q", "-b", "main"]);
    std::fs::write(app.join("README"), "app\n").unwrap();
    git(&app, &["add", "."]);
    git(&app, &["commit", "-q", "-m", "app"]);

    ok(rho_git(
        &app,
        Some(&keeper.socket),
        &[
            "subtree",
            "add",
            "--prefix=vendor/lib",
            &url,
            "main",
            "--squash",
        ],
    ));
    assert_eq!(
        std::fs::read_to_string(app.join("vendor/lib/file.txt")).unwrap(),
        "one\n"
    );
    let mirror = keeper.store.mirror_dir(&url);
    assert!(
        mirror.join("HEAD").is_file(),
        "the store served the subtree"
    );
    assert_eq!(
        alternates(&app),
        vec![mirror.join("objects").to_str().unwrap()]
    );
    assert!(no_own_pack(&app));

    let second = push_commit(&source, "two\n");
    ok(rho_git(
        &app,
        Some(&keeper.socket),
        &[
            "subtree",
            "pull",
            "--prefix=vendor/lib",
            &url,
            "main",
            "--squash",
            "-m",
            "sync lib",
        ],
    ));
    assert_eq!(
        std::fs::read_to_string(app.join("vendor/lib/file.txt")).unwrap(),
        "two\n"
    );
    let squash = git(&app, &["log", "-1", "--format=%B", "HEAD^2"]);
    assert!(
        squash.contains(&format!("git-subtree-split: {second}")),
        "{squash}"
    );
    assert!(no_own_pack(&app), "the pulled objects are borrowed");
}

#[tokio::test(flavor = "multi_thread")]
async fn shaped_clones_and_local_paths() {
    needs_patched_git!();
    let temp = tempfile::tempdir().unwrap();
    let (_source, remote) = setup_remote(temp.path());
    let daemon = GitDaemon::start(temp.path());
    let url = daemon.url("remote.git");
    let keeper = keeper(temp.path());
    let work = temp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    // A shallow clone is served from the mirror too: the shape is git's
    // business, the bytes are the store's.
    ok(rho_git(
        &work,
        Some(&keeper.socket),
        &["clone", "-q", "--depth", "1", &url, "shallow"],
    ));
    let shallow = work.join("shallow");
    assert!(shallow.join(".git/shallow").is_file());
    assert_eq!(alternates(&shallow).len(), 1);
    // A shallow cut is packed by git's own rules; the store only spared
    // the network.

    // Local paths never touch the store.
    ok(rho_git(
        &work,
        Some(&keeper.socket),
        &["clone", "-q", remote.to_str().unwrap(), "local"],
    ));
    assert!(alternates(&work.join("local")).is_empty());
    assert_eq!(keeper.store.list().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_store_git_is_plain() {
    needs_patched_git!();
    let temp = tempfile::tempdir().unwrap();
    let (source, _remote) = setup_remote(temp.path());
    let daemon = GitDaemon::start(temp.path());
    let url = daemon.url("remote.git");
    let work = temp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    ok(rho_git(&work, None, &["clone", "-q", &url]));
    let clone = work.join("remote");
    assert!(alternates(&clone).is_empty());

    // A socket nobody listens on: a warning, then the network.
    let dead = temp.path().join("dead.sock");
    let output = rho_git(&work, Some(&dead), &["clone", "-q", &url, "direct"]);
    ok(output.clone());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("rho git store"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(work.join("direct/file.txt").is_file());
    assert!(alternates(&work.join("direct")).is_empty());
    let second = push_commit(&source, "two\n");
    let output = rho_git(&clone, Some(&dead), &["fetch", "-q"]);
    ok(output.clone());
    assert!(String::from_utf8_lossy(&output.stderr).contains("rho git store"));
    assert_eq!(git(&clone, &["rev-parse", "origin/main"]).trim(), second);
}
