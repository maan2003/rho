//! The `git` agents run. Three commands go through the mirror store when
//! one is reachable; everything else, and everything when it is not, is
//! `exec` of the real git with the arguments untouched:
//!
//! - `git clone <url> [dir]` asks the keeper for the mirror and births the
//!   clone from it (`clone_from_mirror`). Options that shape the clone
//!   (`--depth`, `--branch`, `--bare`, `--mirror`, `--reference`, ...) send it
//!   to the real git unchanged: the store serves the common case, not every
//!   case.
//! - `git fetch ...` and `git pull ...` work out which URL git would read (the
//!   named remote's, a literal URL or path, else the branch's upstream remote
//!   or `origin`), ask the keeper to refresh that mirror, add the mirror to the
//!   clone's alternates, then run the real git with
//!   `url.<mirror>.insteadOf=<url>`, so the fetch reads the mirror and never
//!   the network. `--all`/`--multiple` go to the real git as is.
//! - `git subtree add|pull --prefix=<dir> <repository> <ref>` is routed the
//!   same way, from outside: `git subtree` is a script whose nested `git fetch`
//!   is the real git, and the `insteadOf` reaches it through the environment.
//!
//! The wrapper is exec-transparent: git's own exit status, output and
//! signals pass straight through, and git's global options before the
//! subcommand are honoured.

use std::ffi::OsString;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};

use anyhow::Context as _;

use crate::{FetchTarget, Git, Store, clone_from_mirror, ensure_alternate, fetch_urls};

/// A git command line split at its subcommand.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Invocation {
    /// Git's global options, in order, exactly as given.
    pub globals: Vec<OsString>,
    /// The subcommand, when the command line has one.
    pub subcommand: Option<String>,
    /// Everything after the subcommand.
    pub rest: Vec<OsString>,
}

impl Invocation {
    /// Splits `args` (without argv[0]) the way `git.c` does: options with
    /// a value in the next word or after `=`, flags, then the first bare
    /// word is the subcommand. An unknown option ends parsing so the real
    /// git can report it.
    pub fn parse(args: &[OsString]) -> Self {
        const WITH_VALUE: &[&str] = &[
            "-C",
            "-c",
            "--config-env",
            "--exec-path",
            "--git-dir",
            "--namespace",
            "--work-tree",
            "--super-prefix",
            "--attr-source",
            "--list-cmds",
        ];
        const FLAGS: &[&str] = &[
            "--version",
            "--help",
            "--html-path",
            "--man-path",
            "--info-path",
            "-p",
            "--paginate",
            "-P",
            "--no-pager",
            "--no-replace-objects",
            "--no-lazy-fetch",
            "--no-optional-locks",
            "--no-advice",
            "--bare",
            "--literal-pathspecs",
            "--glob-pathspecs",
            "--noglob-pathspecs",
            "--icase-pathspecs",
        ];
        let mut invocation = Self::default();
        let mut index = 0;
        while index < args.len() {
            let arg = &args[index];
            let Some(text) = arg.to_str() else {
                break;
            };
            if !text.starts_with('-') || text == "-" || text == "--" {
                invocation.subcommand = Some(text.to_owned());
                invocation.rest = args[index + 1..].to_vec();
                return invocation;
            }
            if WITH_VALUE.contains(&text) {
                if index + 1 < args.len() {
                    invocation.globals.push(arg.clone());
                    invocation.globals.push(args[index + 1].clone());
                    index += 2;
                    continue;
                }
                break;
            }
            let long_with_equals = text
                .split_once('=')
                .is_some_and(|(name, _)| WITH_VALUE.contains(&name) && name.starts_with("--"));
            if long_with_equals || FLAGS.contains(&text) {
                invocation.globals.push(arg.clone());
                index += 1;
                continue;
            }
            break;
        }
        // No subcommand, or an option we do not know: hand everything to
        // the real git untouched.
        invocation.globals = args.to_vec();
        invocation.subcommand = None;
        invocation.rest.clear();
        invocation
    }

    /// `-C` directories in order, resolved against each other as git does.
    pub fn working_directory(&self) -> Option<PathBuf> {
        let mut dir: Option<PathBuf> = None;
        let mut globals = self.globals.iter();
        while let Some(arg) = globals.next() {
            if arg == "-C"
                && let Some(next) = globals.next()
            {
                dir = Some(match dir {
                    Some(current) => current.join(next),
                    None => PathBuf::from(next),
                });
            }
        }
        dir
    }

    fn touches_git_dir(&self) -> bool {
        self.globals.iter().any(|arg| {
            arg.to_str().is_some_and(|text| {
                text == "--bare"
                    || text == "--git-dir"
                    || text.starts_with("--git-dir=")
                    || text == "--work-tree"
                    || text.starts_with("--work-tree=")
            })
        })
    }
}

/// A `git clone` the store can serve: a URL, an optional directory, and
/// only options that do not change what a clone contains.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CloneRequest {
    pub url: String,
    pub directory: Option<PathBuf>,
    pub quiet: bool,
}

impl CloneRequest {
    pub fn parse(rest: &[OsString]) -> Option<Self> {
        const WITH_VALUE: &[&str] = &[
            "-o",
            "--origin",
            "-b",
            "--branch",
            "-u",
            "--upload-pack",
            "--template",
            "--reference",
            "--reference-if-able",
            "--separate-git-dir",
            "--depth",
            "--shallow-since",
            "--shallow-exclude",
            "-c",
            "--config",
            "-j",
            "--jobs",
            "--filter",
            "--bundle-uri",
            "--server-option",
            "--ref-format",
        ];
        let mut quiet = false;
        let mut positionals = Vec::new();
        let mut index = 0;
        let mut options_done = false;
        while index < rest.len() {
            let text = rest[index].to_str()?;
            index += 1;
            if options_done || !text.starts_with('-') {
                positionals.push(text.to_owned());
                continue;
            }
            match text {
                "--" => options_done = true,
                "-q" | "--quiet" => quiet = true,
                "-v"
                | "--verbose"
                | "--progress"
                | "--no-progress"
                | "--no-tags"
                | "--no-recurse-submodules"
                | "--no-shallow-submodules"
                | "--no-remote-submodules" => {}
                _ if WITH_VALUE.contains(&text) => return None,
                _ => return None,
            }
        }
        let (url, directory) = match positionals.as_slice() {
            [url] => (url.clone(), None),
            [url, directory] => (url.clone(), Some(PathBuf::from(directory))),
            _ => return None,
        };
        if url.is_empty() || url.starts_with('-') {
            return None;
        }
        Some(Self {
            url,
            directory,
            quiet,
        })
    }

    pub fn destination(&self) -> Option<PathBuf> {
        self.directory
            .clone()
            .or_else(|| rho_git_proto::repo_name(&self.url).map(PathBuf::from))
    }
}

/// Runs the wrapper for `args` (argv without the program name) and
/// returns the exit code when the command was served in-process. Commands
/// handed to the real git are `exec`ed and never return.
pub fn run(args: Vec<OsString>) -> anyhow::Result<i32> {
    let git = real_git()?;
    let invocation = Invocation::parse(&args);
    let store = Store::from_env();
    match (invocation.subcommand.as_deref(), store) {
        (Some("clone"), Some(store)) if !invocation.touches_git_dir() => {
            if let Some(request) = CloneRequest::parse(&invocation.rest) {
                match serve_clone(&git, &store, &invocation, &request) {
                    Ok(code) => return Ok(code),
                    Err(StoreError::Unavailable(error)) => {
                        eprintln!("rho-git: git store unavailable, cloning directly ({error:#})");
                    }
                    Err(StoreError::Clone(error)) => return Err(error),
                }
            }
        }
        (Some(subcommand @ ("fetch" | "pull" | "subtree")), Some(store)) => {
            let target = if subcommand == "subtree" {
                FetchTarget::parse_subtree(&invocation.rest)
            } else {
                FetchTarget::parse(&invocation.rest)
            };
            let urls = fetch_urls(&git, &invocation.globals, target);
            if !urls.is_empty() {
                match route_fetch(&git, &store, &invocation.globals, &urls) {
                    Ok(rewrites) => {
                        let mut args = invocation.globals.clone();
                        args.extend(rewrites);
                        args.push(invocation.subcommand.clone().unwrap().into());
                        args.extend(invocation.rest.iter().cloned());
                        return Err(exec(&git, args));
                    }
                    Err(error) => {
                        eprintln!("rho-git: git store unavailable, fetching directly ({error:#})");
                    }
                }
            }
        }
        _ => {}
    }
    Err(exec(&git, args))
}

enum StoreError {
    /// The keeper could not serve the clone; the real git should try.
    Unavailable(anyhow::Error),
    /// The mirror was fine and the clone itself failed.
    Clone(anyhow::Error),
}

fn serve_clone(
    git: &Git,
    store: &Store,
    invocation: &Invocation,
    request: &CloneRequest,
) -> Result<i32, StoreError> {
    let mirror = store
        .ensure(&request.url)
        .map_err(StoreError::Unavailable)?;
    let dest = request.destination().ok_or_else(|| {
        StoreError::Clone(anyhow::anyhow!(
            "cannot derive a directory from {:?}",
            request.url
        ))
    })?;
    let dest = match invocation.working_directory() {
        Some(dir) => dir.join(dest),
        None => dest,
    };
    if !request.quiet {
        eprintln!("Cloning into '{}'...", dest.display());
    }
    clone_from_mirror(git, &mirror, &request.url, &dest).map_err(StoreError::Clone)?;
    Ok(0)
}

/// Refreshes the mirror of every URL, borrows each into the repository's
/// alternates, and returns the `-c url.<mirror>.insteadOf=<url>` pairs.
fn route_fetch(
    git: &Git,
    store: &Store,
    globals: &[OsString],
    urls: &[String],
) -> anyhow::Result<Vec<OsString>> {
    let mut rewrites = Vec::new();
    for url in urls {
        let mirror = store.refresh(url)?;
        if let Err(error) = ensure_alternate(git, globals, &mirror) {
            eprintln!("rho-git: cannot borrow the mirror's objects, fetching copies ({error:#})");
        }
        rewrites.push("-c".into());
        rewrites.push(insteadof(&mirror, url));
    }
    Ok(rewrites)
}

fn insteadof(mirror: &Path, url: &str) -> OsString {
    let mut arg = OsString::from("url.");
    arg.push(mirror);
    arg.push(".insteadOf=");
    arg.push(url);
    arg
}

fn real_git() -> anyhow::Result<Git> {
    if let Some(git) = Git::from_env() {
        return Ok(git);
    }
    let me = std::env::current_exe().ok();
    Git::find_on_path(None, me.as_deref()).with_context(|| {
        format!(
            "no git found: set {} or put one on PATH",
            rho_git_proto::GIT_ENV
        )
    })
}

fn exec(git: &Git, args: Vec<OsString>) -> anyhow::Error {
    let error = std::process::Command::new(git.executable())
        .args(args)
        .exec();
    anyhow::Error::from(error).context(format!("exec {}", git.executable().display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn splits_globals_from_the_subcommand() {
        let parsed = Invocation::parse(&args(&[
            "-C",
            "dir",
            "-c",
            "a.b=c",
            "--git-dir=g",
            "--no-pager",
            "fetch",
            "origin",
            "-p",
        ]));
        assert_eq!(
            parsed.globals,
            args(&["-C", "dir", "-c", "a.b=c", "--git-dir=g", "--no-pager"])
        );
        assert_eq!(parsed.subcommand.as_deref(), Some("fetch"));
        assert_eq!(parsed.rest, args(&["origin", "-p"]));
        assert!(parsed.touches_git_dir());

        let bare = Invocation::parse(&args(&["--version"]));
        assert_eq!(bare.subcommand, None);
        assert_eq!(bare.globals, args(&["--version"]));

        let unknown = Invocation::parse(&args(&["--bogus", "clone", "x"]));
        assert_eq!(unknown.subcommand, None);
        assert_eq!(unknown.globals, args(&["--bogus", "clone", "x"]));

        let nested = Invocation::parse(&args(&["-C", "a", "-C", "b", "status"]));
        assert_eq!(nested.working_directory(), Some(PathBuf::from("a/b")));
        assert_eq!(
            Invocation::parse(&args(&["status"])).working_directory(),
            None
        );
    }

    #[test]
    fn clone_requests_only_cover_plain_clones() {
        let plain = CloneRequest::parse(&args(&["https://example.com/o/r.git"])).unwrap();
        assert_eq!(plain.url, "https://example.com/o/r.git");
        assert_eq!(plain.destination(), Some(PathBuf::from("r")));
        assert!(!plain.quiet);

        let named = CloneRequest::parse(&args(&["-q", "--progress", "u", "dir"])).unwrap();
        assert_eq!(named.destination(), Some(PathBuf::from("dir")));
        assert!(named.quiet);

        let dashed = CloneRequest::parse(&args(&["--", "u", "dir"])).unwrap();
        assert_eq!(dashed.url, "u");

        for narrowing in [
            &["--depth", "1", "u"][..],
            &["-b", "x", "u"],
            &["--bare", "u"],
            &["--mirror", "u"],
            &["--single-branch", "u"],
            &["--reference", "r", "u"],
            &["--filter=blob:none", "u"],
            &["u", "dir", "extra"],
            &[],
        ] {
            assert!(
                CloneRequest::parse(&args(narrowing)).is_none(),
                "{narrowing:?}"
            );
        }
    }

    #[test]
    fn insteadof_rewrites_the_fetched_url() {
        assert_eq!(
            insteadof(Path::new("/s/r/git"), "https://x/y"),
            OsString::from("url./s/r/git.insteadOf=https://x/y")
        );
    }
}
