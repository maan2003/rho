#![allow(dead_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use rho_workset::{PathOverrides, StoreRefresh, UserEnvironment, Worksets};

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

/// Runs jj in `dir` through the daemon's command builder, so the store
/// server is wired in exactly as for an agent.
pub async fn jj(root: &Worksets, dir: &Path, args: &[&str]) -> String {
    let mut command = root.command("jj");
    command.current_dir(dir).args(args);
    let output = command.output().await.unwrap();
    assert!(
        output.status.success(),
        "jj {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

pub fn jj_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("JJ_BIN") {
        return path.into();
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vendor/jj/Cargo.toml");
    let output = Command::new("cargo")
        .args([
            "build",
            "-p",
            "jj-cli",
            "--message-format=json-render-diagnostics",
            "--manifest-path",
        ])
        .arg(manifest)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "build jj: {}",
        String::from_utf8_lossy(&output.stderr)
    );
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
        .expect("cargo did not report jj executable")
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

pub fn environment(jj_bin: &Path) -> UserEnvironment {
    let mut environment = std::env::vars_os()
        .filter(|(name, _)| name != "JJ_STORE" && name != "JJ_STORE_SOCKET")
        .collect::<Vec<_>>();
    environment.push((OsString::from("RHO_JJ"), jj_bin.to_owned().into_os_string()));
    UserEnvironment::new(environment)
}

/// Opens a state root under `temp` with a store server that never
/// debounces, so every clone sees the remote's current state.
pub async fn open_worksets(temp: &Path, jj_bin: &Path) -> Arc<Worksets> {
    Worksets::open(
        temp.join("root"),
        environment(jj_bin),
        PathOverrides::default(),
        StoreRefresh {
            interval: Duration::from_secs(3600),
            debounce: Duration::ZERO,
        },
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
