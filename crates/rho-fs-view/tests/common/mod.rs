#![allow(dead_code)]

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rho_fs_view::{PathOverrides, StoreRefresh, StoreService, UserEnvironment, Worksets};

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

/// A `source` checkout with one commit on `main`, cloned bare to
/// `remote.git`. Returns `(source, remote)`.
pub fn setup_remote(temp: &Path) -> (PathBuf, PathBuf) {
    let source = temp.join("source");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, &["init", "-q", "-b", "main"]);
    std::fs::write(source.join("file.txt"), "one\n").unwrap();
    git(&source, &["add", "."]);
    git(&source, &["commit", "-q", "-m", "one"]);
    let remote = temp.join("remote.git");
    git(temp, &["clone", "-q", "--bare", "source", "remote.git"]);
    (source, remote)
}

/// Runs Rho's git in `dir` with the store wired in exactly as for an
/// agent.
pub async fn store_git(root: &Worksets, dir: &Path, args: &[&str]) -> String {
    let mut command = root.command(rho_fs_view::GIT);
    command.current_dir(dir).args(args);
    let output = command.output().await.unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

/// The user environment the tests hand the daemon: the process's, without
/// the store variables a surrounding view may have set.
pub fn environment() -> UserEnvironment {
    UserEnvironment::new(
        std::env::vars_os()
            .filter(|(name, _)| {
                name != "RHO_GIT_STORE_SOCKET"
                    && name != "GIT_AUTHOR_NAME"
                    && name != "GIT_AUTHOR_EMAIL"
            })
            .chain([
                ("GIT_AUTHOR_NAME".into(), "Test Agent".into()),
                ("GIT_AUTHOR_EMAIL".into(), "agent@example.test".into()),
            ])
            .collect(),
    )
}

/// A worksets root at `temp/root` with the keeper running, fetching on
/// every request.
pub async fn open_worksets(temp: &Path) -> Arc<Worksets> {
    Worksets::open(
        temp.join("root"),
        environment(),
        PathOverrides::default(),
        StoreService::Serve(StoreRefresh {
            debounce: Duration::ZERO,
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

/// A `git daemon` serving every repository under `base` over `git://`,
/// pushes included, so URLs are remote ones and the store is consulted:
/// the patched git leaves local paths alone.
pub struct GitDaemon {
    child: Child,
    pub port: u16,
}

impl GitDaemon {
    pub fn start(base: &Path) -> Self {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let child = Command::new("git")
            .args([
                "daemon",
                "--reuseaddr",
                "--listen=127.0.0.1",
                &format!("--port={port}"),
                "--export-all",
                "--enable=receive-pack",
                &format!("--base-path={}", base.display()),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "git daemon did not come up");
            std::thread::sleep(Duration::from_millis(50));
        }
        Self { child, port }
    }

    /// The URL of `<base>/<name>`.
    pub fn url(&self, name: &str) -> String {
        format!("git://127.0.0.1:{}/{name}", self.port)
    }
}

impl Drop for GitDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
