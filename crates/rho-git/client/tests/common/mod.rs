#![allow(dead_code)]

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub fn git(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} in {}: {}",
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// A `source` checkout with one commit on `main`, pushed to a bare
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
    git(
        &source,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    (source, remote)
}

/// Commits `content` to `file.txt` in `source` and pushes `main`.
pub fn push_commit(source: &Path, content: &str) -> String {
    std::fs::write(source.join("file.txt"), content).unwrap();
    git(source, &["commit", "-q", "-am", content]);
    git(source, &["push", "-q", "origin", "main"]);
    git(source, &["rev-parse", "HEAD"]).trim().to_owned()
}

/// Rho's patched git, from the agent base this build was made with.
pub fn patched_git() -> PathBuf {
    PathBuf::from(concat!(
        env!(
            "RHO_AGENT_BASE",
            "RHO_AGENT_BASE must name the agent base at build time"
        ),
        "/bin/git"
    ))
}

/// A `git daemon` serving every repository under `base` over `git://`,
/// pushes included, so URLs are remote ones and the store is consulted.
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
