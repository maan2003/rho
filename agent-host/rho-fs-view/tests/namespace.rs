//! Live-namespace behaviour: bounded reads below `/src`, cloning through
//! the mirror store from inside the namespace, and the Claude home mount
//! stack. Runs without the libtest harness because the identity user
//! namespace must be created while the process is still single-threaded.

use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use camino::Utf8Path;
use rho_fs_view::{MAX_BOUNDED_READ, Mode, read_file_bounded};

mod common;
use common::{GitDaemon, only_store, open_worksets, setup_remote};

fn main() {
    // These namespace-first binaries cannot use libtest, but nextest still
    // needs a libtest-compatible listing to run and time each binary.
    if std::env::args().any(|arg| arg == "--list") {
        if !std::env::args().any(|arg| arg == "--ignored") {
            println!("e2e: test");
        }
        return;
    }
    let args = std::env::args_os().collect::<Vec<_>>();
    if args.get(1).is_some_and(|arg| arg == "--inside") {
        let bytes = std::fs::read(&args[2]).unwrap();
        let layout: rho_fs_view::WorksetLayout =
            senax_encoder::decode(&mut bytes.as_slice()).unwrap();
        unsafe {
            layout.build().unwrap();
            layout.enter().unwrap();
        }
        std::env::set_current_dir(&args[3]).unwrap();
        panic!(
            "exec workset command: {}",
            Command::new(&args[4]).args(&args[5..]).exec()
        );
    }
    let unshare = Command::new("unshare").args(["-U", "true"]).status();
    if !unshare.map(|status| status.success()).unwrap_or(false) {
        eprintln!("skipping namespace test: kernel forbids unshare(CLONE_NEWUSER)");
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(run());
}

fn host_binary(name: &str) -> PathBuf {
    let path = std::env::var_os("PATH").unwrap();
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .and_then(|candidate| std::fs::canonicalize(candidate).ok())
        .unwrap_or_else(|| panic!("{name} not on PATH"))
}

async fn run() {
    let temp = tempfile::tempdir().unwrap();
    let (_source, _remote) = setup_remote(temp.path());
    let daemon = GitDaemon::start(temp.path());
    let remote = daemon.url("remote.git");
    let root = open_worksets(temp.path()).await;
    let workset = root.create().await.unwrap();
    let checkout = workset.clone_repo(&remote, Some("project")).await.unwrap();
    let outside = temp.path().join("outside.txt");
    std::fs::write(&outside, "secret\n").unwrap();
    std::os::unix::fs::symlink(&outside, checkout.join("escape")).unwrap();
    std::fs::create_dir(checkout.join("sub")).unwrap();
    std::os::unix::fs::symlink("../file.txt", checkout.join("sub/inside")).unwrap();

    let skeleton = temp.path().join("skeleton");
    std::fs::create_dir(&skeleton).unwrap();
    let mount_root = temp.path().join("view-root");
    std::fs::create_dir(&mount_root).unwrap();
    let layout_path = temp.path().join("layout");
    let layout = rho_fs_view::WorksetLayout::new(
        &workset,
        Mode::View {
            home_skeleton: Some(temp.path().join("skeleton")),
        },
        mount_root.try_into().unwrap(),
    )
    .unwrap();
    std::fs::write(&layout_path, senax_encoder::encode(&layout).unwrap()).unwrap();
    assert_eq!(
        workset.host_path(Utf8Path::new("/src")).unwrap(),
        workset.root()
    );
    assert!(
        !workset
            .host_path(Utf8Path::new("/src/missing"))
            .unwrap()
            .is_dir()
    );
    assert!(workset.host_path(Utf8Path::new("/src/../outside")).is_err());

    // Bounded reads use paths in the current filesystem (the host here,
    // /src in a workset process), without a host-visible path translation.
    let source = workset.root();
    let file = source.join("project/file.txt");
    let inside = source.join("project/sub/inside");
    assert_eq!(
        read_file_bounded(source, file.as_std_path(), 1024)
            .await
            .unwrap(),
        b"one\n"
    );
    assert_eq!(
        read_file_bounded(source, inside.as_std_path(), 4)
            .await
            .unwrap(),
        b"one\n"
    );
    assert!(
        read_file_bounded(source, file.as_std_path(), 3)
            .await
            .is_err()
    );
    assert!(
        read_file_bounded(source, file.as_std_path(), MAX_BOUNDED_READ + 1)
            .await
            .is_err()
    );
    assert!(
        read_file_bounded(source, source.join("project/escape").as_std_path(), 1024)
            .await
            .is_err()
    );
    assert!(
        read_file_bounded(source, Path::new("project/file.txt"), 1024)
            .await
            .is_err()
    );
    assert!(
        read_file_bounded(source, source.join("../outside.txt").as_std_path(), 1024)
            .await
            .is_err()
    );
    assert!(
        read_file_bounded(
            source,
            source.join("project/../../etc/passwd").as_std_path(),
            1024
        )
        .await
        .is_err()
    );
    assert!(
        read_file_bounded(source, Path::new("/etc/passwd"), 1024)
            .await
            .is_err()
    );
    assert!(
        read_file_bounded(source, source.join("project/sub").as_std_path(), 1024)
            .await
            .is_err()
    );

    // Notes created after the namespace exists use the existing state mount.
    let notes = workset.state_dir().unwrap().join("notes");
    std::fs::create_dir_all(&notes).unwrap();
    std::fs::write(notes.join("progress.md"), "before rotation").unwrap();

    // Inside the namespace `git` is Rho's git: the agent clones through
    // the keeper, works in the clone, fetches from the mirror, and cannot
    // write the store or the git directory. The command starts in /src.
    let sh = host_binary("sh");
    let store = only_store(temp.path());
    let with_store = format!(
        r#"
test "$(command -v git)" = {base}/bin/git
test "$(command -v env)" = {base}/bin/env
/bin/sh -c true
/usr/bin/env true
test -f /etc/ssl/certs/ca-certificates.crt
test -f /etc/nix/registry.json
test "$XDG_STATE_HOME" = /home/agent/.local/state
test "$GIT_CONFIG_SYSTEM" = /etc/gitconfig
test "$(git config --get core.pager)" = cat
test -n "$RHO_DEVSHELL_BUILDER"
test -n "$RHO_DEVSHELL_DIR"
case "$RHO_DEVSHELL_PATH_PREFIX" in *:/home/agent/.cache/cargo/bin|/home/agent/.cache/cargo/bin) ;; *) exit 1 ;; esac
test "$INSIDE_AGENT" = 1
test "$CARGO_HOME" = /home/agent/.cache/cargo
touch /home/agent/.cache/from-view
touch {state}/from-view {cache}/rho-devshell/from-view
git clone -q -- {remote} second
test "$(cat /src/second/.git/objects/info/alternates)" = {store}/git/objects
git -C /src/second fetch -q
if touch {base}/bin/x 2>/dev/null; then echo "base is writable"; exit 1; fi
git -C /src/second commit -q --allow-empty -m identity
test "$(git -C /src/second log -1 --format=%an)" = "Test Agent"
test "$(bash -c 'cat <(echo substituted)')" = substituted
test "$(echo piped | cat /dev/stdin)" = piped
"#,
        base = rho_fs_view::AGENT_BASE,
        state = workset.state_dir().unwrap(),
        cache = root.cache_dir(),
        store = store.display(),
    );
    let script = format!(
        r#"
set -eu
test "$PWD" = /src
test -d /src/project
test "$(cat {notes}/progress.md)" = "before rotation"
printf 'after rotation' > {notes}/progress.md
test "$RHO_GIT_STORE_SOCKET" = {socket}
test -S "$RHO_GIT_STORE_SOCKET"
{with_store}
test -f /src/second/file.txt
git -C /src/second log -1 --format=%H origin/main >/dev/null
touch /src/second/written
if touch {store}/mirror 2>/dev/null; then echo "store is writable"; exit 1; fi
if mkdir {stores}/x 2>/dev/null; then echo "store root is writable"; exit 1; fi
test ! -e /src/.stores
"#,
        notes = notes,
        stores = root.store_root(),
        socket = root.store_socket().unwrap(),
        store = store.display(),
    );
    let mut command = tokio::process::Command::new(&sh);
    command.arg("-c").arg(&script);
    prepare(&layout_path, &mut command, "/src").unwrap();
    let output = command.output().await.unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(notes.join("progress.md")).unwrap(),
        "after rotation"
    );
    // A command can choose a different cwd within its already entered layout.
    let mut command = tokio::process::Command::new(&sh);
    command.arg("-c").arg(format!(
        "test \"$(cat {notes}/progress.md)\" = \"after rotation\" && printf child > {notes}/child.md"
    ));
    prepare(&layout_path, &mut command, "/src/project").unwrap();
    assert!(command.status().await.unwrap().success());
    assert_eq!(
        std::fs::read_to_string(notes.join("child.md")).unwrap(),
        "child"
    );
    assert!(workset.root().join("second/written").exists());
    assert!(root.cache_dir().join("from-view").exists());
    assert!(workset.state_dir().unwrap().join("from-view").exists());
    assert!(root.devshell_cache_dir().join("from-view").exists());
    assert_eq!(workset.repos().unwrap(), vec!["project", "second"]);
    assert_eq!(only_store(temp.path()), store);

    // Commands inherit the workset environment and can start in a requested cwd.
    let mut command = tokio::process::Command::new(&sh);
    command.arg("-c").arg("pwd");
    prepare(&layout_path, &mut command, "/src/second").unwrap();
    let output = command.output().await.unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "/src/second\n");

    println!("namespace test passed");
}

// The child installs mounts and environment once before executing the command.
fn prepare(layout: &Path, command: &mut tokio::process::Command, cwd: &str) -> anyhow::Result<()> {
    let mut inside = tokio::process::Command::new(std::env::current_exe()?);
    inside
        .arg("--inside")
        .arg(layout)
        .arg(cwd)
        .arg(command.as_std().get_program())
        .args(command.as_std().get_args());
    *command = inside;
    Ok(())
}
