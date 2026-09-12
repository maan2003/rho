//! The wrapper binary against a live keeper: clones are born from the
//! mirror, fetches read the refreshed mirror, and everything else is the
//! real git.

mod common;

use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use common::{git, push_commit, setup_remote};
use rho_git_proto::SOCKET_ENV;
use rho_git_server::{MirrorStore, Refresh};

const WRAPPER: &str = env!("CARGO_BIN_EXE_rho-git");

fn wrapper(cwd: &Path, socket: Option<&Path>, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(WRAPPER);
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
        "wrapper failed: {}{}",
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
            ..Refresh::default()
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

#[tokio::test(flavor = "multi_thread")]
async fn clone_and_fetch_go_through_the_store() {
    let temp = tempfile::tempdir().unwrap();
    let (source, remote) = setup_remote(temp.path());
    let url = remote.to_str().unwrap();
    let keeper = keeper(temp.path());
    let work = temp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    let output = wrapper(&work, Some(&keeper.socket), &["clone", url]);
    ok(output);
    let clone = work.join("remote");
    assert_eq!(
        std::fs::read_to_string(clone.join("file.txt")).unwrap(),
        "one\n"
    );
    let mirror = keeper.store.mirror_dir(url);
    assert_eq!(
        std::fs::read_to_string(clone.join(".git/objects/info/alternates"))
            .unwrap()
            .trim(),
        mirror.join("objects").to_str().unwrap()
    );
    assert_eq!(
        git(&clone, &["config", "--get", "remote.origin.url"]).trim(),
        url
    );
    assert_eq!(git(&clone, &["branch", "--show-current"]).trim(), "main");

    // A named directory, reached through -C, quietly.
    ok(wrapper(
        temp.path(),
        Some(&keeper.socket),
        &["-C", "work", "clone", "-q", url, "named"],
    ));
    assert!(work.join("named/file.txt").is_file());

    // The remote moves; a wrapped fetch sees it through the mirror.
    let second = push_commit(&source, "two\n");
    ok(wrapper(&clone, Some(&keeper.socket), &["fetch", "-q"]));
    assert_eq!(git(&clone, &["rev-parse", "origin/main"]).trim(), second);
    assert_eq!(
        git(&mirror, &["rev-parse", "refs/heads/main"]).trim(),
        second
    );
    assert!(
        clone
            .join(".git/objects/pack")
            .read_dir()
            .unwrap()
            .next()
            .is_none(),
        "fetched objects come from the mirror, not a new pack"
    );

    // pull too, and the checkout follows.
    let third = push_commit(&source, "three\n");
    ok(wrapper(
        &clone,
        Some(&keeper.socket),
        &["pull", "-q", "--ff-only"],
    ));
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]).trim(), third);
    assert_eq!(
        std::fs::read_to_string(clone.join("file.txt")).unwrap(),
        "three\n"
    );

    // Pushing is the real git, straight to the remote.
    std::fs::write(clone.join("file.txt"), "four\n").unwrap();
    ok(wrapper(
        &clone,
        Some(&keeper.socket),
        &["commit", "-q", "-am", "four"],
    ));
    ok(wrapper(&clone, Some(&keeper.socket), &["push", "-q"]));
    assert_eq!(
        git(&remote, &["rev-parse", "main"]).trim(),
        git(&clone, &["rev-parse", "HEAD"]).trim()
    );
}

fn no_own_pack(clone: &Path) -> bool {
    clone
        .join(".git/objects/pack")
        .read_dir()
        .unwrap()
        .next()
        .is_none()
}

#[tokio::test(flavor = "multi_thread")]
async fn fetches_from_any_remote_go_through_that_remotes_mirror() {
    let temp = tempfile::tempdir().unwrap();
    let (source, remote) = setup_remote(temp.path());
    let url = remote.to_str().unwrap();
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
    let fork_url = fork.to_str().unwrap();
    let keeper = keeper(temp.path());
    let work = temp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    ok(wrapper(&work, Some(&keeper.socket), &["clone", "-q", url]));
    let clone = work.join("remote");
    ok(wrapper(
        &clone,
        Some(&keeper.socket),
        &["remote", "add", "fork", fork_url],
    ));

    // Fetching a named remote: its mirror is made, joins the alternates,
    // and the objects are borrowed rather than packed.
    ok(wrapper(
        &clone,
        Some(&keeper.socket),
        &["fetch", "-q", "fork"],
    ));
    assert_eq!(git(&clone, &["rev-parse", "fork/feature"]).trim(), fork_tip);
    let fork_mirror = keeper.store.mirror_dir(fork_url);
    assert!(fork_mirror.join("HEAD").is_file());
    let alternates = std::fs::read_to_string(clone.join(".git/objects/info/alternates")).unwrap();
    assert_eq!(
        alternates.lines().collect::<Vec<_>>(),
        vec![
            keeper
                .store
                .mirror_dir(url)
                .join("objects")
                .to_str()
                .unwrap(),
            fork_mirror.join("objects").to_str().unwrap(),
        ]
    );
    assert!(no_own_pack(&clone));
    assert_eq!(keeper.store.list().unwrap().len(), 2);

    // A literal URL, with a refspec, after the fork moved.
    std::fs::write(source.join("fork.txt"), "more\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-q", "-m", "more"]);
    let fork_next = git(&source, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&source, &["push", "-q", "fork", "HEAD:refs/heads/feature"]);
    ok(wrapper(
        &clone,
        Some(&keeper.socket),
        &["fetch", "-q", fork_url, "feature"],
    ));
    assert_eq!(git(&clone, &["rev-parse", "FETCH_HEAD"]).trim(), fork_next);
    assert_eq!(
        git(&fork_mirror, &["rev-parse", "refs/heads/feature"]).trim(),
        fork_next
    );
    assert_eq!(
        std::fs::read_to_string(clone.join(".git/objects/info/alternates"))
            .unwrap()
            .lines()
            .count(),
        2,
        "a known mirror is not listed twice"
    );
    assert!(no_own_pack(&clone));

    // No argument: the branch's upstream remote, not origin.
    ok(wrapper(
        &clone,
        Some(&keeper.socket),
        &["checkout", "-q", "-b", "feature", "--track", "fork/feature"],
    ));
    std::fs::write(source.join("fork.txt"), "again\n").unwrap();
    git(&source, &["commit", "-q", "-am", "again"]);
    let fork_last = git(&source, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&source, &["push", "-q", "fork", "HEAD:refs/heads/feature"]);
    ok(wrapper(
        &clone,
        Some(&keeper.socket),
        &["pull", "-q", "--ff-only"],
    ));
    assert_eq!(git(&clone, &["rev-parse", "HEAD"]).trim(), fork_last);
    assert!(no_own_pack(&clone));
    assert_eq!(
        git(&clone, &["rev-parse", "origin/main"]).trim(),
        git(&remote, &["rev-parse", "main"]).trim()
    );

    // --all and --multiple: every remote through its mirror, still no pack.
    let origin_next = push_commit(&source, "origin moves\n");
    std::fs::write(source.join("fork.txt"), "fork moves\n").unwrap();
    git(&source, &["commit", "-q", "-am", "fork moves"]);
    let fork_next = git(&source, &["rev-parse", "HEAD"]).trim().to_owned();
    git(&source, &["push", "-q", "fork", "HEAD:refs/heads/feature"]);
    ok(wrapper(
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
        fork_next
    );
    assert!(no_own_pack(&clone));
    let origin_last = push_commit(&source, "origin again\n");
    ok(wrapper(
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
    let temp = tempfile::tempdir().unwrap();
    let (source, remote) = setup_remote(temp.path());
    let url = remote.to_str().unwrap();
    let keeper = keeper(temp.path());
    let app = temp.path().join("app");
    std::fs::create_dir_all(&app).unwrap();
    git(&app, &["init", "-q", "-b", "main"]);
    std::fs::write(app.join("README"), "app\n").unwrap();
    git(&app, &["add", "."]);
    git(&app, &["commit", "-q", "-m", "app"]);

    ok(wrapper(
        &app,
        Some(&keeper.socket),
        &[
            "subtree",
            "add",
            "--prefix=vendor/lib",
            url,
            "main",
            "--squash",
        ],
    ));
    assert_eq!(
        std::fs::read_to_string(app.join("vendor/lib/file.txt")).unwrap(),
        "one\n"
    );
    let mirror = keeper.store.mirror_dir(url);
    assert!(
        mirror.join("HEAD").is_file(),
        "the store served the subtree"
    );
    assert_eq!(
        std::fs::read_to_string(app.join(".git/objects/info/alternates"))
            .unwrap()
            .trim(),
        mirror.join("objects").to_str().unwrap()
    );
    assert!(no_own_pack(&app));

    let second = push_commit(&source, "two\n");
    ok(wrapper(
        &app,
        Some(&keeper.socket),
        &[
            "subtree",
            "pull",
            "--prefix=vendor/lib",
            url,
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
async fn shaped_clones_and_plain_commands_are_the_real_git() {
    let temp = tempfile::tempdir().unwrap();
    let (_source, remote) = setup_remote(temp.path());
    let url = remote.to_str().unwrap();
    let keeper = keeper(temp.path());
    let work = temp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    // git only honours --depth over a real transport, hence file://.
    let file_url = format!("file://{url}");
    ok(wrapper(
        &work,
        Some(&keeper.socket),
        &["clone", "-q", "--depth", "1", &file_url, "shallow"],
    ));
    assert!(work.join("shallow/.git/shallow").is_file());
    assert!(!work.join("shallow/.git/objects/info/alternates").exists());
    assert!(
        keeper.store.list().unwrap().is_empty(),
        "the store was not consulted"
    );

    let version = ok(wrapper(&work, Some(&keeper.socket), &["--version"]));
    assert!(version.starts_with("git version"), "{version}");
    let output = wrapper(&work, Some(&keeper.socket), &["status"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not a git repository"));
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_store_the_wrapper_is_plain_git() {
    let temp = tempfile::tempdir().unwrap();
    let (source, remote) = setup_remote(temp.path());
    let url = remote.to_str().unwrap();
    let work = temp.path().join("work");
    std::fs::create_dir_all(&work).unwrap();

    ok(wrapper(&work, None, &["clone", "-q", url]));
    let clone = work.join("remote");
    assert!(!clone.join(".git/objects/info/alternates").exists());

    // A socket nobody listens on: fall back, loudly but successfully.
    let dead = temp.path().join("dead.sock");
    let output = wrapper(&work, Some(&dead), &["clone", "-q", url, "direct"]);
    ok(output.clone());
    assert!(String::from_utf8_lossy(&output.stderr).contains("store unavailable"));
    assert!(work.join("direct/file.txt").is_file());
    let second = push_commit(&source, "two\n");
    let output = wrapper(&clone, Some(&dead), &["fetch", "-q"]);
    ok(output.clone());
    assert!(String::from_utf8_lossy(&output.stderr).contains("store unavailable"));
    assert_eq!(git(&clone, &["rev-parse", "origin/main"]).trim(), second);
}
