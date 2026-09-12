//! The mirror store's client side: talking to the keeper, birthing a
//! clone from a mirror, and the `git` wrapper agents run (`wrapper`).
//!
//! A clone born here is an ordinary git repository whose `origin` is the
//! real remote URL, so the agent's `git remote -v`, `git push` and any
//! tooling see exactly what a plain clone would. What differs is only
//! where the bytes come from: objects are borrowed from the mirror through
//! `objects/info/alternates` (`CLONES.md`), and the wrapper routes
//! `fetch`/`pull` to the mirror after asking the keeper to refresh it.

use std::ffi::{OsStr, OsString};
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context as _;
pub use rho_git_proto as proto;
use rho_git_proto::{GIT_ENV, Request, Response, SOCKET_ENV};

pub mod wrapper;

/// A connection recipe for the keeper's socket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Store {
    socket: PathBuf,
}

impl Store {
    /// The store the environment points at, if any.
    pub fn from_env() -> Option<Self> {
        std::env::var_os(SOCKET_ENV)
            .filter(|path| !path.is_empty())
            .map(|path| Self::at(PathBuf::from(path)))
    }

    pub fn at(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The mirror for `url`, made or refreshed as needed.
    pub fn ensure(&self, url: &str) -> anyhow::Result<PathBuf> {
        self.request(Request::Ensure {
            url: url.to_owned(),
        })
    }

    /// The mirror for `url`, fetched now (within the keeper's debounce).
    pub fn refresh(&self, url: &str) -> anyhow::Result<PathBuf> {
        self.request(Request::Refresh {
            url: url.to_owned(),
        })
    }

    fn request(&self, request: Request) -> anyhow::Result<PathBuf> {
        let line = request.encode()?;
        let mut stream = UnixStream::connect(&self.socket)
            .with_context(|| format!("connect to git store at {}", self.socket.display()))?;
        stream
            .write_all(line.as_bytes())
            .context("send request to git store")?;
        stream.shutdown(std::net::Shutdown::Write).ok();
        let mut reply = String::new();
        BufReader::new(stream)
            .read_line(&mut reply)
            .context("read git store reply")?;
        match Response::decode(&reply)? {
            Response::Ok { mirror } => Ok(mirror),
            Response::Error { message } => anyhow::bail!("git store: {message}"),
        }
    }
}

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

    /// The git `RHO_GIT` names, if set.
    pub fn from_env() -> Option<Self> {
        std::env::var_os(GIT_ENV)
            .filter(|path| !path.is_empty())
            .map(Self::new)
    }

    /// The first `git` on `path` that is not `exclude` (the wrapper
    /// itself, when it is installed under that name).
    pub fn find_on_path(path: Option<&OsStr>, exclude: Option<&Path>) -> Option<Self> {
        let path = path
            .map(ToOwned::to_owned)
            .or_else(|| std::env::var_os("PATH"))?;
        let exclude = exclude.and_then(|path| std::fs::canonicalize(path).ok());
        std::env::split_paths(&path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| dir.join("git"))
            .find(|candidate| {
                use std::os::unix::fs::PermissionsExt as _;
                let Ok(metadata) = std::fs::metadata(candidate) else {
                    return false;
                };
                if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
                    return false;
                }
                match (&exclude, std::fs::canonicalize(candidate)) {
                    (Some(excluded), Ok(resolved)) => &resolved != excluded,
                    _ => true,
                }
            })
            .map(Self::new)
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
    let objects = dest.join(".git").join("objects");
    let objects = if objects.is_dir() {
        objects
    } else {
        // `git init` honoured a template or GIT_DIR; ask where it put things.
        PathBuf::from(
            git.output(Some(dest), ["rev-parse", "--git-path", "objects"])?
                .trim(),
        )
    };
    std::fs::create_dir_all(objects.join("info"))?;
    std::fs::write(
        objects.join("info").join("alternates"),
        format!("{}\n", mirror.join("objects").display()),
    )
    .context("write alternates")?;
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

/// `remote.origin.url` of the repository git would operate on with
/// `globals` (`-C`, `--git-dir`, ...), or `None` when there is no origin
/// or no repository.
pub fn origin_url(git: &Git, globals: &[OsString]) -> Option<String> {
    let mut args: Vec<OsString> = globals.to_vec();
    args.extend(["config", "--get", "remote.origin.url"].map(OsString::from));
    let url = git.output(None, args).ok()?;
    let url = url.trim();
    (!url.is_empty()).then(|| url.to_owned())
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
        let git = Git::find_on_path(None, None).unwrap();
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
        assert_eq!(
            origin_url(&git, &["-C".into(), dest.clone().into()]).as_deref(),
            Some(remote.to_str().unwrap())
        );
        assert_eq!(origin_url(&git, &["-C".into(), temp.path().into()]), None);
    }

    #[test]
    fn clone_refuses_a_populated_destination() {
        let temp = tempfile::tempdir().unwrap();
        let (remote, mirror) = fixture(temp.path());
        let git = Git::find_on_path(None, None).unwrap();
        let dest = temp.path().join("busy");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("x"), "").unwrap();
        let error = clone_from_mirror(&git, &mirror, remote.to_str().unwrap(), &dest).unwrap_err();
        assert!(error.to_string().contains("already exists"), "{error:#}");
    }

    #[test]
    fn find_on_path_skips_the_excluded_binary() {
        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path().join("bin");
        std::fs::create_dir_all(&fake).unwrap();
        let wrapper = fake.join("git");
        std::fs::write(&wrapper, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        let real = Git::find_on_path(None, None).unwrap();
        let mut path = std::env::join_paths([fake.clone()]).unwrap();
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap());
        assert_eq!(
            Git::find_on_path(Some(&path), None).unwrap().executable(),
            wrapper
        );
        assert_eq!(
            Git::find_on_path(Some(&path), Some(&wrapper))
                .unwrap()
                .executable(),
            real.executable()
        );
    }
}
