//! Live-namespace behaviour: bounded reads below `/src`, cloning through
//! the mirror store from inside the namespace, and the Claude home mount
//! stack. Runs without the libtest harness because the identity user
//! namespace must be created while the process is still single-threaded.

use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use camino::Utf8Path;
use rho_fs_view::{MAX_BOUNDED_READ, Mode};

mod common;
use common::{GitDaemon, only_store, open_worksets, setup_remote};

fn main() {
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
    let ns = workset
        .enter(
            Mode::View {
                home_skeleton: Some(skeleton),
            },
            Utf8Path::new("/src"),
        )
        .unwrap();
    let mount_root = temp.path().join("view-root");
    std::fs::create_dir(&mount_root).unwrap();
    let layout_path = temp.path().join("layout");
    let layout = rho_fs_view::WorksetLayout::new(
        &workset,
        ns.mode().clone(),
        mount_root.try_into().unwrap(),
    )
    .unwrap();
    std::fs::write(&layout_path, senax_encoder::encode(&layout).unwrap()).unwrap();
    assert_eq!(ns.visible_root(), "/src");
    assert_eq!(ns.cwd(), "/src");
    assert!(
        workset
            .enter(
                Mode::View {
                    home_skeleton: None
                },
                Utf8Path::new("/src/missing")
            )
            .is_err()
    );

    // Bounded reads: visible and relative paths, limits, and escapes.
    assert_eq!(
        ns.read_file_bounded(Path::new("/src/project/file.txt"), 1024)
            .await
            .unwrap(),
        b"one\n"
    );
    assert_eq!(
        ns.read_file_bounded(Path::new("project/file.txt"), 4)
            .await
            .unwrap(),
        b"one\n"
    );
    assert_eq!(
        ns.read_file_bounded(Path::new("project/sub/inside"), 1024)
            .await
            .unwrap(),
        b"one\n",
        "symlinks staying inside the workset resolve"
    );
    assert!(
        ns.read_file_bounded(Path::new("project/file.txt"), 3)
            .await
            .is_err()
    );
    assert!(
        ns.read_file_bounded(Path::new("project/file.txt"), MAX_BOUNDED_READ + 1)
            .await
            .is_err()
    );
    assert!(
        ns.read_file_bounded(Path::new("project/escape"), 1024)
            .await
            .is_err()
    );
    assert!(
        ns.read_file_bounded(Path::new("../outside.txt"), 1024)
            .await
            .is_err()
    );
    assert!(
        ns.read_file_bounded(Path::new("/src/project/../../etc/passwd"), 1024)
            .await
            .is_err()
    );
    assert!(
        ns.read_file_bounded(Path::new("/etc/passwd"), 1024)
            .await
            .is_err()
    );
    assert!(
        ns.read_file_bounded(Path::new("project/sub"), 1024)
            .await
            .is_err()
    );
    assert_eq!(
        ns.resolve_host_path_checked(Path::new("/src/project/x"))
            .unwrap(),
        checkout.join("x")
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
test "$DIRENV_CONFIG" = /etc/rho/direnv
test "$RHO_DIRENV_LAYOUT_DIR" = {state}/direnv
test "$INSIDE_AGENT" = 1
test "$CARGO_HOME" = /home/agent/.cache/cargo
touch /home/agent/.cache/from-view
mkdir -p "$RHO_DIRENV_LAYOUT_DIR" && touch "$RHO_DIRENV_LAYOUT_DIR/from-view"
git clone -q -- {remote} second
test "$(cat /src/second/.git/objects/info/alternates)" = {store}/git/objects
git -C /src/second fetch -q
if touch {base}/bin/x 2>/dev/null; then echo "base is writable"; exit 1; fi
git -C /src/second commit -q --allow-empty -m identity
test "$(git -C /src/second log -1 --format=%an)" = "Test Agent"
printf 'export FOO=bar\n' > /src/second/.envrc
test "$(direnv exec /src/second sh -c 'echo $FOO' 2>/dev/null)" = bar
test "$(bash -c 'cat <(echo substituted)')" = substituted
test "$(echo piped | cat /dev/stdin)" = piped
"#,
        base = rho_fs_view::AGENT_BASE,
        state = workset.state_dir().unwrap(),
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
    prepare(&ns, &layout_path, &mut command, None)
        .await
        .unwrap();
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
    let child = workset
        .enter(
            Mode::View {
                home_skeleton: None,
            },
            Utf8Path::new("/src/project"),
        )
        .unwrap();
    let mut command = tokio::process::Command::new(&sh);
    command.arg("-c").arg(format!(
        "test \"$(cat {notes}/progress.md)\" = \"after rotation\" && printf child > {notes}/child.md"
    ));
    prepare(&child, &layout_path, &mut command, None)
        .await
        .unwrap();
    assert!(command.status().await.unwrap().success());
    assert_eq!(
        std::fs::read_to_string(notes.join("child.md")).unwrap(),
        "child"
    );
    assert!(workset.root().join("second/written").exists());
    assert!(root.cache_dir().join("from-view").exists());
    assert!(
        workset
            .state_dir()
            .unwrap()
            .join("direnv/from-view")
            .exists()
    );
    assert_eq!(workset.repos().unwrap(), vec!["project", "second"]);
    assert_eq!(only_store(temp.path()), store);

    // cwd is a visible path below /src.
    let mut command = tokio::process::Command::new(&sh);
    command.arg("-c").arg("pwd");
    prepare(
        &ns,
        &layout_path,
        &mut command,
        Some(Utf8Path::new("second")),
    )
    .await
    .unwrap();
    let output = command.output().await.unwrap();
    assert_eq!(String::from_utf8_lossy(&output.stdout), "/src/second\n");
    let mut command = tokio::process::Command::new(&sh);
    assert!(
        prepare(&ns, &layout_path, &mut command, Some(Utf8Path::new("/tmp")))
            .await
            .is_err()
    );

    println!("namespace test passed");
}

// Exercise command inheritance in a fresh, single-threaded execution process.
// Mount roots remain owned and cleaned in this test's parent frame.
async fn prepare(
    view: &rho_fs_view::Namespace,
    layout: &Path,
    command: &mut tokio::process::Command,
    cwd: Option<&Utf8Path>,
) -> anyhow::Result<()> {
    view.prepare_command(command, cwd).await?;
    let mut inside = tokio::process::Command::new(std::env::current_exe()?);
    inside
        .arg("--inside")
        .arg(layout)
        .arg(command.as_std().get_current_dir().unwrap())
        .arg(command.as_std().get_program())
        .args(command.as_std().get_args());
    for (key, value) in command.as_std().get_envs() {
        match value {
            Some(value) => {
                inside.env(key, value);
            }
            None => {
                inside.env_remove(key);
            }
        }
    }
    *command = inside;
    Ok(())
}
