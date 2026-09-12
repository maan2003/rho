use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

mod common;
use common::{git, jj_binary};

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

/// Creates a bare remote, a store root, and a workset directory holding a
/// "project" clone made through the store. Returns (store root, workset
/// dir, store dir).
fn build_fixture(temp: &Path, jj: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let (_source, remote) = common::setup_remote(temp);
    let stores = temp.join("stores");
    let src = temp.join("src");
    std::fs::create_dir_all(&stores).unwrap();
    std::fs::create_dir_all(&src).unwrap();
    let mut clone = Command::new(jj);
    clone
        .env("JJ_STORE", &stores)
        .current_dir(&src)
        .args(["git", "clone", "--"])
        .arg(&remote)
        .arg("project");
    run(clone);
    let store = std::fs::read_dir(&stores)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap();
    (stores, src, store)
}

#[test]
fn workset_and_generated_root_work_in_the_view() {
    if !namespace_setup_available() {
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let jj = jj_binary();
    let (stores, src, store) = build_fixture(temp.path(), &jj);
    let skeleton = temp.path().join("skeleton");
    std::fs::create_dir(&skeleton).unwrap();
    std::fs::write(skeleton.join("seeded"), "seed\n").unwrap();
    std::os::unix::fs::symlink("/nix/store", skeleton.join("store-link")).unwrap();
    std::fs::copy(&jj, skeleton.join("jj")).unwrap();

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
{jj} -R /src/project st >/dev/null
{unshare} -Ur true
test ! -w {store}/clone-store
touch /src/project/writable
test ! -e {temp}/source
"#,
        stores = stores.display(),
        store = store.display(),
        temp = temp.path().display(),
        git = git_bin.display(),
        jj = "/home/agent/jj",
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
        .arg("--stores")
        .arg(&stores)
        .args(["--skeleton", skeleton.to_str().unwrap(), "--"])
        .arg(shell)
        .args(["-c", &script]);
    run(view);
    assert!(src.join("project/writable").exists());
    git(&src.join("project"), &["status", "--short"]);
}

#[test]
fn exposed_mode_mounts_the_workset_over_the_host_ws_stub() {
    if !namespace_setup_available() {
        return;
    }
    if !Path::new("/ws").is_dir() {
        eprintln!("skipping exposed-mode test: host has no /ws mount stub");
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let jj = jj_binary();
    let (stores, src, store) = build_fixture(temp.path(), &jj);

    let script = format!(
        r#"
set -eu
test "$PWD" = /ws
test -d /ws/project
test "$HOME" = {home}
test -d {temp}
test "$RHO_FS_VIEW_TEST_ENV" = kept
git -C /ws/project status --short
{jj} -R /ws/project st >/dev/null
test ! -w {store}/clone-store
touch /ws/project/writable
"#,
        home = std::env::var("HOME").unwrap(),
        temp = temp.path().display(),
        store = store.display(),
        jj = jj.display(),
    );
    let mut view = Command::new(env!("CARGO_BIN_EXE_rho-workset-dev"));
    view.env("RHO_FS_VIEW_TEST_ENV", "kept")
        .args(["--exposed"])
        .arg("--src")
        .arg(&src)
        .arg("--stores")
        .arg(&stores)
        .args(["--", "/bin/sh", "-c", &script]);
    run(view);
    assert!(src.join("project/writable").exists());
}
