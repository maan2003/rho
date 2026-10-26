use std::collections::BTreeSet;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;
use common::git;

fn run(mut command: Command) -> Output {
    let debug = format!("{command:?}");
    let output = command
        .output()
        .unwrap_or_else(|error| panic!("run {debug}: {error}"));
    assert!(
        output.status.success(),
        "{debug} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    output
}

fn namespace_setup_available() -> bool {
    let status = Command::new("unshare").args(["-U", "true"]).status();
    if !status.is_ok_and(|status| status.success()) {
        eprintln!("skipping workset namespace test: kernel forbids unshare(CLONE_NEWUSER)");
        return false;
    }
    true
}

/// Creates a bare remote, a state root whose `stores/` holds one (fake)
/// store directory, and a workset directory holding a "project" clone.
/// Returns (state root, workset dir, store dir).
fn build_fixture(temp: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let (_source, remote) = common::setup_remote(temp);
    let state = temp.join("state");
    let stores = state.join("stores");
    let src = temp.join("src");
    let store = stores.join("remote-0");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("mirror"), "").unwrap();
    std::fs::create_dir_all(&src).unwrap();
    git(&src, &["clone", "-q", remote.to_str().unwrap(), "project"]);
    (state, src, store)
}

#[test]
fn independent_worksets_preserve_the_host_without_a_src_stub() {
    if !namespace_setup_available() {
        return;
    }

    // Do not create a host /src, even when the caller could do so.
    let host_src = || {
        std::fs::symlink_metadata("/src").ok().map(|metadata| {
            (
                metadata.dev(),
                metadata.ino(),
                metadata.mode(),
                metadata.uid(),
                metadata.gid(),
                metadata.mtime(),
                metadata.mtime_nsec(),
                std::fs::read_dir("/src").ok().map(|entries| {
                    entries
                        .map(|entry| entry.unwrap().file_name())
                        .collect::<BTreeSet<_>>()
                }),
            )
        })
    };
    let before = host_src();
    let symlinks = std::fs::read_dir("/")
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path != Path::new("/src") && path.is_symlink())
        .map(|path| {
            let target = std::fs::read_link(&path).unwrap();
            (path, target)
        })
        .collect::<Vec<_>>();
    let temp = tempfile::tempdir().unwrap();
    let home = std::env::var_os("HOME").unwrap();
    let home_metadata = std::fs::metadata(&home).unwrap();
    let fixtures = ["first", "second"].map(|name| {
        let dir = temp.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        let (state, src, store) = build_fixture(&dir);
        std::fs::write(src.join(format!("workset-{name}")), name).unwrap();
        std::fs::write(store.join("mirror"), "unchanged").unwrap();
        (name, state, src, store)
    });

    std::thread::scope(|scope| {
        for (name, state, src, store) in &fixtures {
            let other = if *name == "first" { "second" } else { "first" };
            let script = format!(
                r#"
set -eu
export LC_ALL=C
test "$PWD" = /src
test "$(id -u)" = {uid}
test "$(id -g)" = {gid}
test "$HOME" = "$RHO_FS_VIEW_TEST_HOME"
test -d "$HOME"
test "$(stat -Lc '%d:%i' "$HOME")" = {home_dev}:{home_ino}
test "$RHO_FS_VIEW_TEST_ENV" = kept
printf usable >/dev/null
test -c /dev/null
test -z "$(cat /dev/null)"
while test "$#" -gt 0; do
    test -L "$1"
    test "$(readlink "$1")" = "$2"
    shift 2
done
git -C /src/project status --short
touch /src/project/writable
printf ready >"$RHO_FS_VIEW_TEST_TEMP/ready-{name}"
attempts=0
while ! test -f "$RHO_FS_VIEW_TEST_TEMP/ready-{other}"; do
    attempts=$((attempts + 1))
    test "$attempts" -lt 100
    sleep 0.1
done
# Both views are live: the second build must not replace the first one's /src.
test "$(ls -A /src)" = "$(printf 'project\nworkset-{name}')"
test "$(cat /src/workset-{name})" = {name}
test ! -e /src/workset-{other}
# Capture errors on a known-writable host path, not /dev/null: a nodev
# regression must not make the redirection fail before the store write runs.
if (printf changed >"$RHO_FS_VIEW_TEST_STORE/mirror") 2>"$RHO_FS_VIEW_TEST_TEMP/error-{name}"; then
    echo "store is writable"
    exit 1
fi
grep -q 'Read-only file system' "$RHO_FS_VIEW_TEST_TEMP/error-{name}"
"#,
                uid = unsafe { libc::getuid() },
                gid = unsafe { libc::getgid() },
                home_dev = home_metadata.dev(),
                home_ino = home_metadata.ino(),
            );
            let mut view = Command::new(env!("CARGO_BIN_EXE_rho-fs-view-dev"));
            view.env("RHO_FS_VIEW_TEST_ENV", "kept")
                .env("RHO_FS_VIEW_TEST_HOME", &home)
                .env("RHO_FS_VIEW_TEST_TEMP", temp.path())
                .env("RHO_FS_VIEW_TEST_STORE", store)
                .arg("--src")
                .arg(src)
                .arg("--state")
                .arg(state)
                .args(["--", "/bin/sh", "-c", &script, "fs-view-test"]);
            for (path, target) in &symlinks {
                view.arg(path).arg(target);
            }
            scope.spawn(move || run(view));
        }
    });
    for (_, _, src, store) in &fixtures {
        assert!(src.join("project/writable").exists());
        assert_eq!(
            std::fs::read_to_string(store.join("mirror")).unwrap(),
            "unchanged"
        );
    }
    assert_eq!(host_src(), before, "host /src changed");
}
