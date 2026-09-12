//! The mirror store's client side: birthing a clone from a mirror, for
//! the daemon's own clones (the keeper itself is called in-process). Agents
//! need no client: the `git` in their view is Rho's patched git
//! (`nix/patches/git-rho-store.patch`), which asks the keeper itself on every
//! fetch and clone of a remote URL when `RHO_GIT_STORE_SOCKET` is set. The
//! end-to-end tests in `tests/` drive that git (the build's `RHO_GIT`)
//! against a live keeper.
//!
//! A clone born here is an ordinary git repository whose `origin` is the
//! real remote URL, so `git remote -v`, `git push` and any tooling see
//! exactly what a plain clone would. What differs is only where the bytes
//! come from: objects are borrowed from the mirror through
//! `objects/info/alternates` (`CLONES.md`).

use std::ffi::{OsStr, OsString};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context as _;
pub use rho_git_proto as proto;

/// The real git executable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Git {
    executable: PathBuf,
}

impl Git {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    pub fn executable(&self) -> &Path {
        &self.executable
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command.stdin(std::process::Stdio::null());
        command
    }

    /// Runs git with `args` (global options first) and returns its stdout.
    pub fn output<I, S>(&self, cwd: Option<&Path>, args: I) -> anyhow::Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = self.command();
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let args: Vec<OsString> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect();
        command.args(&args);
        let output = command
            .output()
            .with_context(|| format!("run {}", self.executable.display()))?;
        anyhow::ensure!(
            output.status.success(),
            "git {} failed: {}",
            args.iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// Births `dest` as a clone of `url` from `mirror`: an empty repository
/// borrowing the mirror's objects, `origin` pointing at the real remote,
/// the remote's branches and tags copied over locally, and the remote's
/// default branch checked out. Nothing here touches the network.
pub fn clone_from_mirror(git: &Git, mirror: &Path, url: &str, dest: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(
        mirror.join("HEAD").is_file(),
        "mirror {} is not a git directory",
        mirror.display()
    );
    if dest.exists() {
        anyhow::ensure!(
            dest.is_dir() && dest.read_dir()?.next().is_none(),
            "destination path '{}' already exists and is not an empty directory",
            dest.display()
        );
    }
    std::fs::create_dir_all(dest).with_context(|| format!("create {}", dest.display()))?;
    git.output(Some(dest), ["init", "--quiet"])?;
    ensure_alternate(git, &[OsString::from("-C"), dest.into()], mirror)?;
    git.output(Some(dest), ["remote", "add", "origin", url])?;
    let mirror_arg = mirror.as_os_str();
    git.output(
        Some(dest),
        [
            OsStr::new("fetch"),
            OsStr::new("--quiet"),
            OsStr::new("--no-tags"),
            mirror_arg,
            OsStr::new("+refs/heads/*:refs/remotes/origin/*"),
            OsStr::new("+refs/tags/*:refs/tags/*"),
        ],
    )?;
    // The mirror's HEAD is the remote's default branch; an unborn HEAD
    // (empty remote) leaves the clone with nothing checked out, as git does.
    let head = git
        .output(
            None,
            [
                OsStr::new("--git-dir"),
                mirror_arg,
                OsStr::new("symbolic-ref"),
                OsStr::new("--quiet"),
                OsStr::new("HEAD"),
            ],
        )
        .ok()
        .and_then(|head| {
            head.trim()
                .strip_prefix("refs/heads/")
                .map(ToOwned::to_owned)
        })
        .filter(|branch| {
            git.output(
                None,
                [
                    OsStr::new("--git-dir"),
                    mirror_arg,
                    OsStr::new("rev-parse"),
                    OsStr::new("--verify"),
                    OsStr::new("--quiet"),
                    OsStr::new(&format!("refs/heads/{branch}")),
                ],
            )
            .is_ok()
        });
    if let Some(branch) = head {
        git.output(
            Some(dest),
            [
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                &format!("refs/remotes/origin/{branch}"),
            ],
        )?;
        git.output(
            Some(dest),
            [
                "checkout",
                "--quiet",
                "-b",
                &branch,
                "--track",
                &format!("origin/{branch}"),
            ],
        )?;
    }
    Ok(())
}

/// Adds `mirror`'s objects to the alternates of the repository `globals`
/// select (`-C`, `--git-dir`, ...), so a fetch from that mirror borrows
/// its objects instead of copying them into a pack of the clone's own.
/// Idempotent; a clone fetching from several mirrors lists them all.
pub fn ensure_alternate(git: &Git, globals: &[OsString], mirror: &Path) -> anyhow::Result<()> {
    let mut args = globals.to_vec();
    args.extend(
        [
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "objects/info/alternates",
        ]
        .map(OsString::from),
    );
    let path = PathBuf::from(git.output(None, args)?.trim());
    let objects = mirror.join("objects");
    let existing = match std::fs::read_to_string(&path) {
        Ok(existing) => existing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    if existing.lines().any(|line| Path::new(line) == objects) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = String::new();
    if !existing.is_empty() && !existing.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("{}\n", objects.display()));
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(cwd: &Path, args: &[&str]) -> String {
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
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// A remote with one commit on `main` and a mirror of it laid out as
    /// the keeper would.
    fn fixture(temp: &Path) -> (PathBuf, PathBuf) {
        let source = temp.join("source");
        std::fs::create_dir_all(&source).unwrap();
        run(&source, &["init", "-q", "-b", "main"]);
        std::fs::write(source.join("file.txt"), "one\n").unwrap();
        run(&source, &["add", "."]);
        run(&source, &["commit", "-q", "-m", "one"]);
        run(&source, &["tag", "v1"]);
        let remote = temp.join("remote.git");
        run(temp, &["clone", "-q", "--bare", "source", "remote.git"]);
        let mirror = temp.join("mirror");
        std::fs::create_dir_all(&mirror).unwrap();
        run(&mirror, &["init", "-q", "--bare"]);
        run(
            &mirror,
            &[
                "remote",
                "add",
                "--no-tags",
                "origin",
                remote.to_str().unwrap(),
            ],
        );
        run(
            &mirror,
            &[
                "fetch",
                "-q",
                "--no-tags",
                "origin",
                "+refs/heads/*:refs/heads/*",
                "+refs/tags/*:refs/tags/*",
            ],
        );
        run(&mirror, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        (remote, mirror)
    }

    #[test]
    fn clone_borrows_the_mirror_and_tracks_the_remote() {
        let temp = tempfile::tempdir().unwrap();
        let (remote, mirror) = fixture(temp.path());
        let git = Git::new("git");
        let dest = temp.path().join("clone");
        clone_from_mirror(&git, &mirror, remote.to_str().unwrap(), &dest).unwrap();

        assert_eq!(
            std::fs::read_to_string(dest.join("file.txt")).unwrap(),
            "one\n"
        );
        let alternates =
            std::fs::read_to_string(dest.join(".git/objects/info/alternates")).unwrap();
        assert_eq!(alternates.trim(), mirror.join("objects").to_str().unwrap());
        assert_eq!(
            run(&dest, &["config", "--get", "remote.origin.url"]).trim(),
            remote.to_str().unwrap()
        );
        assert_eq!(run(&dest, &["branch", "--show-current"]).trim(), "main");
        assert_eq!(
            run(&dest, &["rev-parse", "--abbrev-ref", "@{upstream}"]).trim(),
            "origin/main"
        );
        assert_eq!(run(&dest, &["tag"]).trim(), "v1");
        assert!(
            dest.join(".git/objects/pack")
                .read_dir()
                .unwrap()
                .next()
                .is_none()
        );
        // A second mirror joins the alternates once, whatever the order.
        let globals: Vec<OsString> = vec!["-C".into(), dest.clone().into()];
        let other = temp.path().join("other-mirror");
        std::fs::create_dir_all(&other).unwrap();
        run(&other, &["init", "-q", "--bare"]);
        ensure_alternate(&git, &globals, &other).unwrap();
        ensure_alternate(&git, &globals, &other).unwrap();
        ensure_alternate(&git, &globals, &mirror).unwrap();
        let alternates =
            std::fs::read_to_string(dest.join(".git/objects/info/alternates")).unwrap();
        assert_eq!(
            alternates.lines().collect::<Vec<_>>(),
            vec![
                mirror.join("objects").to_str().unwrap(),
                other.join("objects").to_str().unwrap()
            ]
        );
    }

    #[test]
    fn clone_refuses_a_populated_destination() {
        let temp = tempfile::tempdir().unwrap();
        let (remote, mirror) = fixture(temp.path());
        let git = Git::new("git");
        let dest = temp.path().join("busy");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("x"), "").unwrap();
        let error = clone_from_mirror(&git, &mirror, remote.to_str().unwrap(), &dest).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error:#}");
    }
}
