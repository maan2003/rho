#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

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
