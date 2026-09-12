use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
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
fn workset_and_generated_root_work_in_the_view() {
    if !namespace_setup_available() {
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let (state, src, store) = build_fixture(temp.path());
    let skeleton = temp.path().join("skeleton");
    std::fs::create_dir(&skeleton).unwrap();
    std::fs::write(skeleton.join("seeded"), "seed\n").unwrap();
    std::os::unix::fs::symlink("/nix/store", skeleton.join("store-link")).unwrap();

    let shell = Path::new("/bin/sh").canonicalize().unwrap();
    let git_bin = Path::new("/run/current-system/sw/bin/git")
        .canonicalize()
        .unwrap_or_else(|_| Path::new("/usr/bin/git").canonicalize().unwrap());
    let unshare = Path::new("/run/current-system/sw/bin/unshare")
        .canonicalize()
        .unwrap_or_else(|_| Path::new("/usr/bin/unshare").canonicalize().unwrap());
    let script = format!(
        r#"
set -eu
test "$PWD" = /src
test -d /src/project
test -d {stores}
test -f "$HOME/seeded"
test -L "$HOME/store-link"
touch "$HOME/writable"
test -z "${{RHO_FS_VIEW_TEST_SECRET-}}"
test ! -e /proc/self/fd/77
grep -q '^root:' /etc/passwd
grep -q '^agent:' /etc/passwd
grep -q '^hosts: files dns$' /etc/nsswitch.conf
grep -q localhost /etc/hosts
test -s /etc/resolv.conf
test -L /etc/localtime
test -L /etc/ssl/certs/ca-certificates.crt
grep -q ' / / .* - tmpfs ' /proc/self/mountinfo
grep ' /nix/store ' /proc/self/mountinfo | grep -q ' ro[, ]'
{git} -C /src/project status --short
{unshare} -Ur true
test ! -w {store}/mirror
touch /src/project/writable
test ! -e {temp}/source
"#,
        stores = state.join("stores").display(),
        store = store.display(),
        temp = temp.path().display(),
        git = git_bin.display(),
        unshare = unshare.display(),
    );
    let mut view = Command::new(env!("CARGO_BIN_EXE_rho-workset-dev"));
    let inherited = std::fs::File::open(temp.path().join("remote.git")).unwrap();
    // SAFETY: dup2 is async-signal-safe and the captured fd remains open.
    unsafe {
        view.pre_exec(move || {
            libc::umask(0o077);
            if libc::dup2(inherited.as_raw_fd(), 77) < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    view.env("RHO_FS_VIEW_TEST_SECRET", "must-not-leak")
        .arg("--src")
        .arg(&src)
        .arg("--state")
        .arg(&state)
        .args(["--skeleton", skeleton.to_str().unwrap(), "--"])
        .arg(shell)
        .args(["-c", &script]);
    run(view);
    assert!(src.join("project/writable").exists());
    git(&src.join("project"), &["status", "--short"]);
}

#[test]
fn exposed_mode_mounts_the_workset_over_the_host_src_stub() {
    if !namespace_setup_available() {
        return;
    }
    if !Path::new("/src").is_dir() {
        eprintln!("skipping exposed-mode test: host has no /src mount stub");
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
    let mut view = Command::new(env!("CARGO_BIN_EXE_rho-workset-dev"));
    view.env("RHO_FS_VIEW_TEST_ENV", "kept")
        .args(["--exposed"])
        .arg("--src")
        .arg(&src)
        .arg("--state")
        .arg(&state)
        .args(["--", "/bin/sh", "-c", &script]);
    run(view);
    assert!(src.join("project/writable").exists());
}
