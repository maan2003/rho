//! The mirror store's client side: talking to the keeper, birthing a
//! clone from a mirror, and the `git` wrapper agents run (`wrapper`).
//!
//! A clone born here is an ordinary git repository whose `origin` is the
//! real remote URL, so the agent's `git remote -v`, `git push` and any
//! tooling see exactly what a plain clone would. What differs is only
//! where the bytes come from: objects are borrowed from the mirror through
//! `objects/info/alternates` (`CLONES.md`), and the wrapper routes every
//! `fetch`/`pull` to the mirror of whatever URL it names after asking the
//! keeper to refresh it, adding that mirror to the alternates on the way.

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

/// What a `git fetch`/`git pull` command line names as its source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchTarget {
    /// No repository argument: the current branch's upstream remote,
    /// else `origin`, as git does.
    Default,
    /// The first positional: a remote name, a URL or a path.
    Named(String),
    /// `--all`, `--multiple`, or arguments the wrapper does not read.
    Unrouted,
}

impl FetchTarget {
    /// The repository a `git subtree add|pull --prefix=<dir> <repository>
    /// <ref>` reads from. `git subtree` is a script whose nested `git
    /// fetch` runs the real git, so the wrapper routes it from outside:
    /// the `insteadOf` it passes reaches the script through the
    /// environment (`GIT_CONFIG_PARAMETERS`). Other subtree commands are
    /// unrouted.
    pub fn parse_subtree(rest: &[OsString]) -> Self {
        const WITH_VALUE: &[&str] = &[
            "-P",
            "--prefix",
            "-m",
            "--message",
            "-b",
            "--branch",
            "--onto",
            "--annotate",
        ];
        let positionals = match positionals(rest, WITH_VALUE) {
            Some(positionals) => positionals,
            None => return Self::Unrouted,
        };
        match positionals.as_slice() {
            [command, repository, _reference] if *command == "add" || *command == "pull" => {
                Self::Named((*repository).to_owned())
            }
            _ => Self::Unrouted,
        }
    }

    /// Reads `rest` (the words after `fetch`/`pull`), skipping options and
    /// the values of those that take one.
    pub fn parse(rest: &[OsString]) -> Self {
        const WITH_VALUE: &[&str] = &[
            "--depth",
            "--deepen",
            "--shallow-since",
            "--shallow-exclude",
            "--negotiation-tip",
            "--refmap",
            "--submodule-prefix",
            "-j",
            "--jobs",
            "--upload-pack",
            "-o",
            "--server-option",
            "--filter",
            // pull's merge options
            "-s",
            "--strategy",
            "-X",
            "--strategy-option",
            "--cleanup",
        ];
        if rest
            .iter()
            .any(|word| word == "--all" || word == "--multiple")
        {
            return Self::Unrouted;
        }
        match positionals(rest, WITH_VALUE).as_deref() {
            None => Self::Unrouted,
            Some([]) => Self::Default,
            Some([name, ..]) => Self::Named((*name).to_owned()),
        }
    }
}

/// The non-option words of `rest`, skipping the values of options in
/// `with_value`; `None` when a word is not UTF-8.
fn positionals<'a>(rest: &'a [OsString], with_value: &[&str]) -> Option<Vec<&'a str>> {
    let mut positionals = Vec::new();
    let mut index = 0;
    let mut options_done = false;
    while index < rest.len() {
        let text = rest[index].to_str()?;
        index += 1;
        if options_done || !text.starts_with('-') || text == "-" {
            positionals.push(text);
            continue;
        }
        if text == "--" {
            options_done = true;
        } else if with_value.contains(&text) {
            index += 1;
        }
    }
    Some(positionals)
}

/// The URL the `git fetch`/`git pull` given by `globals` and `rest` would
/// read from: a named remote's URL, a literal URL or path, or the current
/// branch's upstream remote (else `origin`). `None` when the command does
/// not read one place (`--all`), names nothing git could resolve, or
/// there is no repository.
pub fn fetch_url(git: &Git, globals: &[OsString], rest: &[OsString]) -> Option<String> {
    resolve_fetch_target(git, globals, FetchTarget::parse(rest))
}

/// The URL `target` stands for in the repository `globals` select.
pub fn resolve_fetch_target(
    git: &Git,
    globals: &[OsString],
    target: FetchTarget,
) -> Option<String> {
    let name = match target {
        FetchTarget::Unrouted => return None,
        FetchTarget::Named(name) => name,
        FetchTarget::Default => {
            git_line(git, globals, ["symbolic-ref", "--quiet", "--short", "HEAD"])
                .and_then(|branch| {
                    git_line(
                        git,
                        globals,
                        ["config", "--get", &format!("branch.{branch}.remote")],
                    )
                })
                .filter(|remote| remote != ".")
                .unwrap_or_else(|| "origin".to_owned())
        }
    };
    if let Some(url) = git_line(git, globals, ["remote", "get-url", &name]) {
        return Some(url);
    }
    looks_like_url(&name).then_some(name)
}

/// Something git would take as a repository rather than a remote name.
fn looks_like_url(name: &str) -> bool {
    !name.starts_with('-')
        && (name.contains("://")
            || name.contains(':')
            || name.starts_with('/')
            || name.starts_with("./")
            || name.starts_with("../")
            || name.starts_with('~')
            || Path::new(name).join("HEAD").is_file())
}

/// One non-empty line of git's stdout, or `None` on failure.
fn git_line<const N: usize>(git: &Git, globals: &[OsString], args: [&str; N]) -> Option<String> {
    let mut all: Vec<OsString> = globals.to_vec();
    all.extend(args.map(OsString::from));
    let out = git.output(None, all).ok()?;
    let line = out.lines().next()?.trim();
    (!line.is_empty()).then(|| line.to_owned())
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
        let globals: Vec<OsString> = vec!["-C".into(), dest.clone().into()];
        assert_eq!(
            fetch_url(&git, &globals, &[]).as_deref(),
            Some(remote.to_str().unwrap())
        );
        assert_eq!(
            fetch_url(&git, &["-C".into(), temp.path().into()], &[]),
            None
        );

        // A second mirror joins the alternates once, whatever the order.
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

        // Naming a remote, a path or the upstream remote of the branch.
        run(
            &dest,
            &["remote", "add", "upstream", other.to_str().unwrap()],
        );
        assert_eq!(
            fetch_url(
                &git,
                &globals,
                &["-q".into(), "upstream".into(), "main".into()]
            )
            .as_deref(),
            Some(other.to_str().unwrap())
        );
        assert_eq!(
            fetch_url(
                &git,
                &globals,
                &["--depth".into(), "1".into(), mirror.clone().into()]
            )
            .as_deref(),
            Some(mirror.to_str().unwrap())
        );
        assert_eq!(fetch_url(&git, &globals, &["nosuch".into()]), None);
        assert_eq!(fetch_url(&git, &globals, &["--all".into()]), None);
        run(&dest, &["config", "branch.main.remote", "upstream"]);
        assert_eq!(
            fetch_url(&git, &globals, &[]).as_deref(),
            Some(other.to_str().unwrap())
        );
    }

    #[test]
    fn fetch_targets_skip_option_values() {
        let words = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(FetchTarget::parse(&[]), FetchTarget::Default);
        assert_eq!(
            FetchTarget::parse(&words(&["-q", "--prune", "-j", "4", "--depth=1"])),
            FetchTarget::Default
        );
        assert_eq!(
            FetchTarget::parse(&words(&["--depth", "1", "up", "main"])),
            FetchTarget::Named("up".into())
        );
        assert_eq!(
            FetchTarget::parse(&words(&[
                "--rebase",
                "-Xtheirs",
                "-s",
                "ort",
                "https://x/y",
                "main"
            ])),
            FetchTarget::Named("https://x/y".into())
        );
        assert_eq!(
            FetchTarget::parse(&words(&["--", "-odd"])),
            FetchTarget::Named("-odd".into())
        );
        assert_eq!(
            FetchTarget::parse(&words(&["--all", "-q"])),
            FetchTarget::Unrouted
        );
        assert_eq!(
            FetchTarget::parse(&words(&["--multiple", "a", "b"])),
            FetchTarget::Unrouted
        );

        assert_eq!(
            FetchTarget::parse_subtree(&words(&[
                "pull",
                "--prefix=vendor/x",
                "https://x/y",
                "main",
                "--squash",
                "-m",
                "msg"
            ])),
            FetchTarget::Named("https://x/y".into())
        );
        assert_eq!(
            FetchTarget::parse_subtree(&words(&["add", "-P", "vendor/x", "up", "v1", "--squash"])),
            FetchTarget::Named("up".into())
        );
        for unrouted in [
            &["add", "--prefix=vendor/x", "abc123"][..],
            &["merge", "--prefix=vendor/x", "abc123", "https://x/y"],
            &["split", "--prefix=vendor/x"],
            &["push", "--prefix=vendor/x", "https://x/y", "main"],
        ] {
            assert_eq!(
                FetchTarget::parse_subtree(&words(unrouted)),
                FetchTarget::Unrouted,
                "{unrouted:?}"
            );
        }
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
