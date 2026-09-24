//! The mirror keeper: one bare git mirror per remote URL under a root,
//! fetched on request, served over a unix socket. The daemon runs it in-process
//! and is the root's only writer; everything else — the daemon's own
//! clones and the `git` wrapper agents run — is a client that reads a
//! mirror and never touches the network (`CLONES.md`).
//!
//! Layout under the root, one directory per remote URL (`store_key`):
//!
//! ```text
//! <root>/<key>/git    bare mirror, laid out like `git clone --mirror`:
//!                     the remote's branches in refs/heads/*, its tags in
//!                     refs/tags/*, HEAD naming its default branch. So a
//!                     fetch pointed at the mirror with git's default
//!                     refspecs behaves exactly like one from the remote.
//! <root>/<key>/url    the remote URL; written last, so its presence means
//!                     the mirror is complete
//! ```
//!
//! **The mirror never prunes objects.** Clones borrow them through
//! alternates, so a pruned object would corrupt every clone. Ref pruning is
//! fine; object bytes stay.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use rho_git_proto::{Request, Response, store_key};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixListener;

/// How the keeper serves a mirror: one fetched within `debounce` is
/// served as is, so concurrent and back-to-back requests share a fetch.
/// There is no background fetching: every request fetches what it needs,
/// and a mirror nobody asks for costs nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refresh {
    pub debounce: Duration,
}

impl Default for Refresh {
    fn default() -> Self {
        Self {
            debounce: Duration::from_secs(30),
        }
    }
}

/// Owns a store root: serializes work per mirror and remembers when each
/// was last fetched.
pub struct MirrorStore {
    root: PathBuf,
    git: PathBuf,
    environment: Vec<(OsString, OsString)>,
    refresh: Refresh,
    entries: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Option<Instant>>>>>,
}

impl std::fmt::Debug for MirrorStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirrorStore")
            .field("root", &self.root)
            .field("git", &self.git)
            .finish_non_exhaustive()
    }
}

impl MirrorStore {
    /// A keeper for `root`, running `git` with `environment` (the user's,
    /// so credential helpers and ssh work as they do for them; empty
    /// inherits the process environment). `refresh` says how recently a
    /// mirror must have been fetched for a request to skip fetching it,
    /// and how the background loop behaves.
    pub fn new(
        root: impl Into<PathBuf>,
        git: impl Into<PathBuf>,
        environment: Vec<(OsString, OsString)>,
        refresh: Refresh,
    ) -> Arc<Self> {
        Arc::new(Self {
            root: root.into(),
            git: git.into(),
            environment,
            refresh,
            entries: Mutex::new(HashMap::new()),
        })
    }

    pub fn refresh(&self) -> Refresh {
        self.refresh
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where `url`'s store lives, whether or not it exists yet.
    pub fn store_dir(&self, url: &str) -> PathBuf {
        self.root.join(store_key(url))
    }

    /// `url`'s bare mirror: what clones borrow objects from and fetch from.
    pub fn mirror_dir(&self, url: &str) -> PathBuf {
        self.store_dir(url).join("git")
    }

    fn entry(&self, url: &str) -> Arc<tokio::sync::Mutex<Option<Instant>>> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(entries.entry(store_key(url)).or_default())
    }

    /// The mirror for `url`, initialized if missing and fetched unless it
    /// was fetched within the debounce window. Concurrent requests for one
    /// URL wait on the same lock and share one fetch.
    pub async fn ensure(&self, url: &str) -> anyhow::Result<PathBuf> {
        let url = url.trim();
        anyhow::ensure!(!url.is_empty(), "empty remote URL");
        let entry = self.entry(url);
        let mut last = entry.lock().await;
        let store = self.store_dir(url);
        if store.join("url").is_file() {
            if last.is_none_or(|at| at.elapsed() >= self.refresh.debounce) {
                self.fetch_mirror(&store.join("git")).await?;
                *last = Some(Instant::now());
            }
        } else {
            self.init(url, &store).await?;
            *last = Some(Instant::now());
        }
        Ok(store.join("git"))
    }

    /// Initializes `url`'s store in a staging directory and renames it into
    /// place once its first fetch succeeded, so a half-made store is never
    /// mistaken for a mirror.
    async fn init(&self, url: &str, store: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create store root {}", self.root.display()))?;
        let staging = store.with_file_name(format!(
            "{}.staging",
            store.file_name().unwrap_or_default().to_string_lossy()
        ));
        for stale in [store, staging.as_path()] {
            if stale.exists() {
                std::fs::remove_dir_all(stale)
                    .with_context(|| format!("remove incomplete store {}", stale.display()))?;
            }
        }
        let mirror = staging.join("git");
        std::fs::create_dir_all(&mirror)
            .with_context(|| format!("create mirror {}", mirror.display()))?;
        self.git(&mirror, ["init", "--bare", "--quiet"]).await?;
        for (key, value) in [
            // Append-only: a pruned mirror object would corrupt every clone
            // that borrowed it.
            ("gc.auto", "0"),
            ("gc.pruneExpire", "never"),
            ("maintenance.auto", "false"),
        ] {
            self.git(&mirror, ["config", key, value]).await?;
        }
        // --no-tags: tags come through the explicit refspec, not
        // auto-following. The refspecs make this a mirror of the remote's
        // branches and tags, and only those.
        self.git(&mirror, ["remote", "add", "--no-tags", "origin", url])
            .await?;
        self.git(
            &mirror,
            [
                "config",
                "--replace-all",
                "remote.origin.fetch",
                "+refs/heads/*:refs/heads/*",
            ],
        )
        .await?;
        self.git(
            &mirror,
            [
                "config",
                "--add",
                "remote.origin.fetch",
                "+refs/tags/*:refs/tags/*",
            ],
        )
        .await?;
        self.fetch_mirror(&mirror).await?;
        std::fs::write(staging.join("url"), format!("{url}\n"))
            .with_context(|| format!("record remote URL in {}", staging.display()))?;
        std::fs::rename(&staging, store)
            .with_context(|| format!("install store {}", store.display()))?;
        Ok(())
    }

    /// The network half: branches, tags, the remote's default branch, then
    /// packed refs so listing them never reads an object.
    async fn fetch_mirror(&self, mirror: &Path) -> anyhow::Result<()> {
        self.git(
            mirror,
            ["fetch", "--quiet", "--prune", "--no-tags", "origin"],
        )
        .await?;
        // HEAD follows the remote's default branch, which is what clones
        // check out. Best effort: an empty remote has none.
        if let Ok(listing) = self
            .git_output(mirror, ["ls-remote", "--symref", "origin", "HEAD"])
            .await
        {
            let head = listing.lines().find_map(|line| {
                line.strip_prefix("ref: ")
                    .and_then(|rest| rest.split_once('\t'))
                    .map(|(target, _)| target.to_owned())
            });
            if let Some(target) = head.filter(|target| target.starts_with("refs/heads/")) {
                self.git(mirror, ["symbolic-ref", "HEAD", &target]).await?;
            }
        }
        self.git(mirror, ["pack-refs", "--all"]).await
    }

    async fn git<const N: usize>(&self, mirror: &Path, args: [&str; N]) -> anyhow::Result<()> {
        self.git_output(mirror, args).await.map(drop)
    }

    async fn git_output<const N: usize>(
        &self,
        mirror: &Path,
        args: [&str; N],
    ) -> anyhow::Result<String> {
        let mut command = tokio::process::Command::new(&self.git);
        if !self.environment.is_empty() {
            command.env_clear();
            command.envs(self.environment.iter().map(|(name, value)| (name, value)));
        }
        command
            .arg("--git-dir")
            .arg(mirror)
            .args(args)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = command
            .output()
            .await
            .with_context(|| format!("run {}", self.git.display()))?;
        anyhow::ensure!(
            output.status.success(),
            "git {} failed in {}: {}",
            args.join(" "),
            mirror.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Every complete store under the root, as `(store dir, remote URL)`.
    pub fn list(&self) -> io::Result<Vec<(PathBuf, String)>> {
        let mut stores = Vec::new();
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(stores),
            Err(error) => return Err(error),
        };
        for entry in entries {
            let dir = entry?.path();
            if let Ok(url) = std::fs::read_to_string(dir.join("url")) {
                stores.push((dir, url.trim().to_owned()));
            }
        }
        stores.sort();
        Ok(stores)
    }

    /// Answers one protocol line.
    pub async fn handle_request(&self, line: &str) -> String {
        let response = match Request::decode(line) {
            Ok(request) => match self.ensure(request.url()).await {
                Ok(mirror) => Response::Ok { mirror },
                Err(error) => Response::Error {
                    message: format!("{error:#}"),
                },
            },
            Err(error) => Response::Error {
                message: error.to_string(),
            },
        };
        response.encode()
    }

    /// Binds the keeper's socket, replacing a stale one.
    pub fn bind(socket: &Path) -> io::Result<UnixListener> {
        match std::fs::remove_file(socket) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        UnixListener::bind(socket)
    }

    /// Serves connections on `listener` until accept fails, one task per
    /// connection.
    pub async fn serve(self: Arc<Self>, listener: UnixListener) -> io::Result<()> {
        loop {
            let (stream, _) = listener.accept().await?;
            let store = Arc::clone(&self);
            tokio::spawn(async move {
                let (reader, mut writer) = stream.into_split();
                let mut line = String::new();
                if tokio::io::BufReader::new(reader)
                    .read_line(&mut line)
                    .await
                    .is_err()
                {
                    return;
                }
                let response = store.handle_request(&line).await;
                let _ = writer.write_all(response.as_bytes()).await;
                let _ = writer.shutdown().await;
            });
        }
    }
}
