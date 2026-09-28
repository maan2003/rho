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
fn workset_mounts_over_the_host_src_stub() {
    if !namespace_setup_available() {
        return;
    }
    if !Path::new("/src").is_dir() {
        eprintln!("skipping workset namespace test: host has no /src mount stub");
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let (state, src, store) = build_fixture(temp.path());

    let script = format!(
        r#"
set -eu
test "$PWD" = /src
test -d /src/project
test "$HOME" = {home}
test -d {temp}
test "$RHO_FS_VIEW_TEST_ENV" = kept
git -C /src/project status --short
test ! -w {store}/mirror
touch /src/project/writable
"#,
        home = std::env::var("HOME").unwrap(),
        temp = temp.path().display(),
        store = store.display(),
    );
    let mut view = Command::new(env!("CARGO_BIN_EXE_rho-fs-view-dev"));
    view.env("RHO_FS_VIEW_TEST_ENV", "kept")
        .arg("--src")
        .arg(&src)
        .arg("--state")
        .arg(&state)
        .args(["--", "/bin/sh", "-c", &script]);
    run(view);
    assert!(src.join("project/writable").exists());
}
