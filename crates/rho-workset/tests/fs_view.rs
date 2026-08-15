use std::os::fd::AsRawFd as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

fn git(dir: &Path, args: &[&str]) {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Test", "-c", "user.email=test@localhost"])
        .args(args);
    run(command);
}

fn jj_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("JJ_BIN") {
        return path.into();
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/jj/Cargo.toml");
    let mut command = Command::new("cargo");
    command.args([
        "build",
        "--release",
        "-p",
        "jj-cli",
        "--message-format=json-render-diagnostics",
        "--manifest-path",
    ]);
    command.arg(manifest);
    let output = run(command);
    output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .find_map(|message| {
            if message.get("reason")?.as_str()? == "compiler-artifact"
                && message.get("target")?.get("name")?.as_str()? == "jj"
            {
                Some(PathBuf::from(message.get("executable")?.as_str()?))
            } else {
                None
            }
        })
        .expect("cargo did not report the jj executable")
}

fn namespace_setup_available() -> bool {
    let status = Command::new("unshare").args(["-U", "true"]).status();
    if !status.is_ok_and(|status| status.success()) {
        eprintln!("skipping workset namespace test: kernel forbids unshare(CLONE_NEWUSER)");
        return false;
    }
    true
}

/// Creates a bare remote, a clone store, an "agent" clone, and a "project"
/// workspace under `temp`, returning (remote, store, workspace).
fn build_clone_store_fixture(temp: &Path, jj: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let source = temp.join("source");
    let remote = temp.join("remote.git");
    let tree = temp.join("tree");
    let store = tree.join(".stores/repo");
    let workspace = tree.join("project");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&tree).unwrap();
    git(&source, &["init", "-b", "main"]);
    std::fs::write(source.join("file.txt"), "content\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-m", "initial"]);
    git(
        temp,
        &[
            "clone",
            "--bare",
            source.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    let mut init = Command::new(jj);
    init.args(["store", "init"]).arg(&store).arg(&remote);
    run(init);
    let mut clone = Command::new(jj);
    clone.args(["store", "clone"]).arg(&store).arg("agent");
    run(clone);
    let mut create_workspace = Command::new(jj);
    create_workspace
        .args(["store", "workspace"])
        .arg(&store)
        .arg("agent")
        .arg(&workspace)
        .args(["--name", "project"]);
    run(create_workspace);
    (remote, store, workspace)
}

#[test]
fn clone_store_workspace_and_generated_root_work_in_the_view() {
    if !namespace_setup_available() {
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let jj = jj_binary();
    let (remote, store, workspace) = build_clone_store_fixture(temp.path(), &jj);
    let skeleton = temp.path().join("skeleton");
    std::fs::create_dir(&skeleton).unwrap();
    std::fs::write(skeleton.join("seeded"), "seed\n").unwrap();
    std::os::unix::fs::symlink("/nix/store", skeleton.join("store-link")).unwrap();
    std::fs::copy(&jj, skeleton.join("jj")).unwrap();

    let shell = Path::new("/bin/sh").canonicalize().unwrap();
    let git = Path::new("/run/current-system/sw/bin/git")
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
test -d /src/.stores/repo
test -f "$HOME/seeded"
test -L "$HOME/store-link"
touch "$HOME/writable"
test ! -e /home/maan2003
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
test ! -w /src/.stores/repo/clone-store
touch /src/.stores/repo/clones/agent/writable
"#,
        git = git.display(),
        jj = "/home/agent/jj",
        unshare = unshare.display(),
    );
    let mut view = Command::new(env!("CARGO_BIN_EXE_rho-workset-dev"));
    let inherited = std::fs::File::open(&remote).unwrap();
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
        .args(["--workspace", &format!("project={}", workspace.display())])
        .args(["--store", &format!("repo={}={}", store.display(), "agent")])
        .args(["--skeleton", skeleton.to_str().unwrap(), "--"])
        .arg(shell)
        .args(["-c", &script]);
    run(view);
}

#[test]
fn exposed_mode_mounts_the_working_set_over_the_host_ws_stub() {
    if !namespace_setup_available() {
        return;
    }
    if !Path::new("/ws").is_dir() {
        eprintln!("skipping exposed-mode test: host has no /ws mount stub");
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let jj = jj_binary();
    let (_remote, store, workspace) = build_clone_store_fixture(temp.path(), &jj);

    let script = format!(
        r#"
set -eu
test "$PWD" = /ws
test -d /ws/project
test -d /ws/.stores/repo
test "$HOME" = {home}
test -d {temp}
test "$RHO_FS_VIEW_TEST_ENV" = kept
git -C /ws/project status --short
{jj} -R /ws/project st >/dev/null
test ! -w /ws/.stores/repo/clone-store
touch /ws/.stores/repo/clones/agent/writable
"#,
        home = std::env::var("HOME").unwrap(),
        temp = temp.path().display(),
        jj = jj.display(),
    );
    let mut view = Command::new(env!("CARGO_BIN_EXE_rho-workset-dev"));
    view.env("RHO_FS_VIEW_TEST_ENV", "kept")
        .args(["--exposed"])
        .args(["--workspace", &format!("project={}", workspace.display())])
        .args(["--store", &format!("repo={}={}", store.display(), "agent")])
        .args(["--", "/bin/sh", "-c", &script]);
    run(view);
}
