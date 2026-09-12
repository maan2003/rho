#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use rho_workset::{PathOverrides, StoreRefresh, StoreService, UserEnvironment, Worksets};

pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=Test", "-c", "user.email=test@localhost"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Runs the workset root's `git` wrapper in `dir` with the store wired in
/// exactly as for an agent.
pub async fn store_git(root: &Worksets, dir: &Path, args: &[&str]) -> String {
    let wrapper = root.store_bin().expect("wrapper installed").join("git");
    let mut command = root.command(wrapper.as_str());
    command.current_dir(dir).args(args);
    let output = command.output().await.unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// Builds the `rho-git` wrapper and returns its path.
pub fn wrapper_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("RHO_GIT_WRAPPER") {
        return path.into();
    }
    let output = Command::new("cargo")
        .args([
            "build",
            "-p",
            "rho-git-client",
            "--bin",
            "rho-git",
            "--message-format=json-render-diagnostics",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "build rho-git: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .find_map(|message| {
            if message.get("reason")?.as_str()? == "compiler-artifact"
                && message.get("target")?.get("name")?.as_str()? == "rho-git"
            {
                Some(PathBuf::from(message.get("executable")?.as_str()?))
            } else {
                None
            }
        })
        .expect("cargo did not report the rho-git executable")
}

/// A source repository with one commit (`file.txt` = "one") and a bare
/// clone of it acting as the remote. Returns `(source, remote)`.
pub fn setup_remote(temp: &Path) -> (PathBuf, PathBuf) {
    let source = temp.join("source");
    let remote = temp.join("remote.git");
    std::fs::create_dir(&source).unwrap();
    git(&source, &["init", "-b", "main"]);
    std::fs::write(source.join("file.txt"), "one\n").unwrap();
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
    (source, remote)
}

pub fn environment() -> UserEnvironment {
    UserEnvironment::new(
        std::env::vars_os()
            .filter(|(name, _)| name != rho_workset::SOCKET_ENV && name != rho_workset::GIT_ENV)
            .collect(),
    )
}

/// Opens a state root under `temp` with a keeper that never debounces, so
/// every clone sees the remote's current state. `wrapper` is installed as
/// the root's `git`.
pub async fn open_worksets(temp: &Path, wrapper: &Path) -> Arc<Worksets> {
    // SAFETY: tests using this run on one thread when they call it (a
    // current-thread runtime, or main before the runtime starts).
    unsafe { std::env::set_var("RHO_GIT_WRAPPER", wrapper) };
    Worksets::open(
        temp.join("root"),
        environment(),
        PathOverrides::default(),
        StoreService::Serve(StoreRefresh {
            interval: Duration::from_secs(3600),
            debounce: Duration::ZERO,
            ..StoreRefresh::default()
        }),
    )
    .await
    .unwrap()
}

/// The single store directory under `root/stores`.
pub fn only_store(temp: &Path) -> PathBuf {
    let stores = temp.join("root/stores");
    let mut entries = std::fs::read_dir(&stores)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 1, "{entries:?}");
    entries.pop().unwrap()
}
