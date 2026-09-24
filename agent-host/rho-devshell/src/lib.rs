//! Flake dev shells for rho's commands: which shell a directory gets, and
//! how an evaluated shell is found again.
//!
//! One [`Resolver`] per workset process serves every agent's commands,
//! terminals and sidecars. It runs `rho-devshell-builder shell`, which does
//! everything Nix-shaped in one process with libnix: finds a valid shell in
//! the daemon's cache ([`Client`]) under a key every valid entry for a flake
//! shares (see [`Flake::key`]), checks entries by observing what their
//! evaluation observed of local inputs again ([`Observation`]), pins them,
//! evaluates on a miss, and writes the activation script. It runs in the
//! caller's filesystem namespace, where those inputs mean what they meant to
//! the evaluator. With a watcher, a resolved shell is kept until something
//! it was built from changes.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::io;
use std::os::unix::ffi::OsStringExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt as _;

pub mod protocol;
pub use protocol::Client;

/// Changes whenever evaluation semantics change (evaluator, Nix fork
/// patches), so shells of an older evaluator are never found.
pub const EVALUATOR: &str = concat!("rho-devshell/", env!("CARGO_PKG_VERSION"), " nix-2.35-rho4");

macro_rules! after_shell {
    () => {
        r#"
if [ -n "${RHO_DEVSHELL_CARGO-}" ]; then
    __rho_cargo=$(command -v cargo || true)
    case $__rho_cargo in
        *-cargo-deluxe-*/bin/cargo)
            __rho_cargo=${__rho_cargo%/cargo}
            PATH=${PATH/"$__rho_cargo"/"$__rho_cargo:$RHO_DEVSHELL_CARGO"} ;;
        *) PATH="$RHO_DEVSHELL_CARGO:$PATH" ;;
    esac
    unset __rho_cargo
fi
if [ -n "${RHO_DEVSHELL_PATH_PREFIX-}" ]; then
    PATH="$RHO_DEVSHELL_PATH_PREFIX:$PATH"
fi
export PATH
"#
    };
}

/// Bash to run after an activation script: the caller's tools ahead of the
/// shell's own. `RHO_DEVSHELL_CARGO`, a directory with the `cargo` to use,
/// goes first, except that a shell whose `cargo` is cargo-deluxe keeps it:
/// deluxe intercepts and runs the next `cargo` on `PATH`, so this one goes
/// right after it. Then `RHO_DEVSHELL_PATH_PREFIX` goes before everything.
/// Not part of the script, so that scripts written before a change to it
/// still get it.
pub const AFTER_SHELL: &str = after_shell!();

/// Bash running a program after the activation script in `$1` and
/// [`AFTER_SHELL`]. `shellHook` output goes to stderr so that stdout is the
/// program's alone.
pub const EXEC_SCRIPT: &str = concat!(
    r#"script=$1; shift; . "$script" >&2; unset script"#,
    after_shell!(),
    r#"exec "$@""#
);

/// The nearest directory with a `flake.nix`, looking no further up than the
/// enclosing git checkout, as Nix finds `.`.
pub fn find_flake(cwd: &Path) -> Option<PathBuf> {
    let physical = cwd.canonicalize().ok()?;
    for dir in physical.ancestors() {
        if dir.join("flake.nix").is_file() {
            return Some(dir.to_path_buf());
        }
        if dir.join(".git").exists() {
            break;
        }
    }
    None
}

/// How Nix fetches a local flake's source tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    /// `git+file`: only files in the git index are visible.
    Git,
    /// `path`: every file is visible.
    Path,
}

impl Scheme {
    pub fn as_str(self) -> &'static str {
        match self {
            Scheme::Git => "git",
            Scheme::Path => "path",
        }
    }
}

/// Where a flake's source tree is fetched from, as Nix does for a local
/// flake: the enclosing git repository (with the flake in a subdirectory of
/// it), or the flake directory itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub scheme: Scheme,
    pub root: PathBuf,
    /// The flake directory relative to `root`.
    pub subdir: PathBuf,
}

impl Source {
    pub fn find(flake_dir: &Path) -> Self {
        for dir in flake_dir.ancestors() {
            if dir.join(".git").exists() {
                return Self {
                    scheme: Scheme::Git,
                    root: dir.to_path_buf(),
                    subdir: flake_dir.strip_prefix(dir).unwrap_or(Path::new("")).to_path_buf(),
                };
            }
        }
        Self {
            scheme: Scheme::Path,
            root: flake_dir.to_path_buf(),
            subdir: PathBuf::new(),
        }
    }
}
/// One dev shell of a local flake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flake {
    /// Physical path of the directory holding `flake.nix`.
    pub dir: PathBuf,
    pub source: Source,
    /// The `devShells.<system>` attribute.
    pub shell: String,
}

impl Flake {
    pub fn new(dir: PathBuf, shell: impl Into<String>) -> Self {
        Self {
            source: Source::find(&dir),
            dir,
            shell: shell.into(),
        }
    }

    pub fn attr_path(&self) -> String {
        format!("devShells.{}-{}.{}", std::env::consts::ARCH, std::env::consts::OS, self.shell)
    }

    /// What every valid cached shell of this flake has in common: the
    /// question (evaluator, flake location within its source, attribute)
    /// and the files every evaluation reads, `flake.nix` and `flake.lock`.
    /// An entry under another key cannot be valid here, so grouping by it
    /// hides nothing; clones and worktrees of one flake share it.
    pub fn key(&self) -> io::Result<String> {
        let read = |name: &str| match std::fs::read(self.dir.join(name)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        };
        let mut hasher = blake3::Hasher::new();
        let mut part = |bytes: Option<&[u8]>| {
            match bytes {
                Some(bytes) => {
                    hasher.update(&(bytes.len() as u64).to_le_bytes());
                    hasher.update(bytes);
                }
                None => {
                    hasher.update(&u64::MAX.to_le_bytes());
                }
            };
        };
        part(Some(EVALUATOR.as_bytes()));
        part(Some(self.attr_path().as_bytes()));
        part(Some(self.source.scheme.as_str().as_bytes()));
        part(Some(self.source.subdir.as_os_str().as_encoded_bytes()));
        part(read("flake.nix")?.as_deref());
        part(read("flake.lock")?.as_deref());
        Ok(hasher.finalize().to_hex().to_string())
    }
}

/// One thing evaluation observed of a local input, as the Nix fork records
/// it, with the input named so that every checkout of the flake can check
/// the same record (see [`InputUrl`]). It still holds while observing it
/// again gives the same value.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Observation {
    /// `stat`, `file`, `dir`, `link` or `attr`.
    pub kind: String,
    pub input: InputUrl,
    /// Relative to the input's root, empty for the root; for `attr`, the
    /// source-info attribute's name.
    pub path: String,
    /// What was observed; empty if the read failed.
    pub value: String,
}

/// A local input's URL (`git+file:///repo?...`, `path:/dir`), with a
/// directory inside the flake's source tree relative to it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct InputUrl {
    /// Up to the path: `git+file://`, `path:`.
    pub prefix: String,
    /// Relative to the flake's source root if inside it, else absolute.
    pub dir: PathBuf,
    /// The query and fragment, if any, from their `?` or `#` on.
    pub rest: String,
}

impl InputUrl {
    /// `url` with its directory relative to `root` if inside it; `None` for
    /// a URL that is not of a local directory.
    pub fn parse(url: &str, root: &Path) -> Option<Self> {
        let colon = url.find(':')?;
        let after = &url[colon + 1..];
        let slashes = if after.starts_with("//") { 2 } else { 0 };
        let path_start = colon + 1 + slashes;
        let path_end = url[path_start..].find(['?', '#']).map_or(url.len(), |i| path_start + i);
        let dir = PathBuf::from(OsString::from_vec(percent_decode(&url[path_start..path_end])?));
        if !dir.is_absolute() {
            return None;
        }
        let dir = match dir.strip_prefix(root) {
            Ok(rel) => rel.to_path_buf(),
            Err(_) => dir,
        };
        Some(Self {
            prefix: url[..path_start].to_owned(),
            dir,
            rest: url[path_end..].to_owned(),
        })
    }

    /// The input's directory in the checkout at `root`.
    pub fn dir(&self, root: &Path) -> PathBuf {
        join(root, &self.dir)
    }

    /// The input's URL in the checkout at `root`.
    pub fn url(&self, root: &Path) -> String {
        format!("{}{}{}", self.prefix, percent_encode(self.dir(root).as_os_str().as_encoded_bytes()), self.rest)
    }

    fn is_git(&self) -> bool {
        self.prefix.starts_with("git+")
    }
}

/// `base` joined with `rel`, without a trailing slash for an empty `rel`;
/// an absolute `rel` replaces `base`.
fn join(base: &Path, rel: &Path) -> PathBuf {
    if rel.as_os_str().is_empty() { base.to_path_buf() } else { base.join(rel) }
}

fn percent_decode(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

fn percent_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_alphanumeric() || b"-._~/!$&'()*+,;=:@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A cached shell as the builder stores it in the daemon, which does not
/// interpret it: the shell, and what its evaluation observed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Evaluated {
    pub drv_path: String,
    pub env_store_path: String,
    pub observations: Vec<Observation>,
}

/// What `rho-devshell-builder shell` prints.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Shell {
    /// The cache entry, if the shell could be cached.
    pub id: Option<u64>,
    pub env_store_path: String,
    /// The activation script, if the builder had a cache directory to
    /// write it to.
    pub activation: Option<PathBuf>,
    pub watch: Watch,
}

/// A cached shell as the daemon holds it: `data` is an [`Evaluated`] the
/// daemon does not interpret.
#[derive(Clone, Debug, PartialEq, Eq, senax_encoder::Encode, senax_encoder::Decode)]
pub struct Candidate {
    pub id: u64,
    pub env_store_path: String,
    pub data: Vec<u8>,
}

/// The GC root pinning `env_store_path` in the directory of roots. One per
/// environment, whichever entries share it.
pub fn gc_root(roots: &Path, env_store_path: &str) -> PathBuf {
    roots.join(store_basename(env_store_path))
}

/// The directory of GC roots below the shared cache directory.
pub fn roots_dir(dir: &Path) -> PathBuf {
    dir.join("roots")
}

/// The directory of `env_store_path`'s activation scripts below the shared
/// cache directory, removed with the environment.
pub fn activations_dir(dir: &Path, env_store_path: &str) -> PathBuf {
    dir.join("activations").join(store_basename(env_store_path))
}

/// Where the activation script of `env_store_path` is written: one per
/// [`EVALUATOR`], since the script is Nix's. Next to it, with extension
/// `d`, is the directory it keeps structured attributes and redirects the
/// environment's outputs to.
pub fn activation_path(dir: &Path, env_store_path: &str) -> PathBuf {
    let evaluator = blake3::hash(EVALUATOR.as_bytes()).to_hex();
    activations_dir(dir, env_store_path).join(format!("{}.sh", &evaluator[..16]))
}

fn store_basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A resolved shell.
#[derive(Clone, Debug)]
pub struct Resolved {
    /// The cache entry, if the shell could be cached.
    pub id: Option<u64>,
    pub env_store_path: String,
    pub watch: Watch,
}

/// What to watch to know a shell may be stale.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watch {
    /// Paths whose contents matter: files read, directories listed,
    /// `flake.lock`, and the git state deciding which files are visible
    /// and, if the shell used it, the revision.
    pub contents: BTreeSet<PathBuf>,
    /// Paths only stat'd: replacing or deleting the entry matters, and so do
    /// its attributes, but not what a directory contains.
    pub names: BTreeSet<PathBuf>,
}

impl Watch {
    /// What to watch for a shell of `flake` built from `observations`.
    pub fn new(flake: &Flake, observations: &[Observation]) -> Self {
        let root = &flake.source.root;
        let mut watch = Self::default();
        watch.contents.insert(flake.dir.join("flake.nix"));
        watch.contents.insert(flake.dir.join("flake.lock"));
        // Whether the flake is fetched from git.
        watch.names.insert(root.join(".git"));
        let mut git_inputs = BTreeSet::new();
        for observation in observations {
            let dir = observation.input.dir(root);
            if observation.input.is_git() {
                git_inputs.insert(dir.clone());
            }
            match observation.kind.as_str() {
                "stat" => {
                    watch.names.insert(join(&dir, Path::new(&observation.path)));
                }
                "attr" => {
                    if let Some(git) = GitDirs::find(&dir) {
                        watch.contents.insert(git.dir.join("HEAD"));
                        watch.contents.insert(git.common.join("packed-refs"));
                        if let Some(head) = std::fs::read_to_string(git.dir.join("HEAD"))
                            .ok()
                            .and_then(|head| head.strip_prefix("ref: ").map(|r| r.trim().to_owned()))
                        {
                            watch.contents.insert(git.common.join(head));
                        }
                    }
                }
                _ => {
                    watch.contents.insert(join(&dir, Path::new(&observation.path)));
                }
            }
        }
        // Which files git inputs show.
        for dir in git_inputs {
            if let Some(git) = GitDirs::find(&dir) {
                watch.contents.insert(git.dir.join("index"));
            }
        }
        watch.names.retain(|path| !watch.contents.contains(path));
        watch
    }

    fn subscribe(&self, watcher: &rho_watch::Watcher) -> io::Result<rho_watch::Subscription> {
        watcher.watch(
            self.contents.iter().map(PathBuf::as_path),
            self.names.iter().map(PathBuf::as_path),
        )
    }

    /// Whether a subscription to `other` sees every change this watch would.
    fn within(&self, other: &Watch) -> bool {
        self.contents.is_subset(&other.contents)
            && self.names.iter().all(|name| other.names.contains(name) || other.contents.contains(name))
    }
}

/// A checkout's git directory, and the directory shared by its worktrees.
struct GitDirs {
    dir: PathBuf,
    common: PathBuf,
}

impl GitDirs {
    fn find(root: &Path) -> Option<Self> {
        let dot_git = root.join(".git");
        let dir = if dot_git.is_dir() {
            dot_git
        } else {
            let file = std::fs::read_to_string(&dot_git).ok()?;
            root.join(file.strip_prefix("gitdir:")?.trim())
        };
        let common = match std::fs::read_to_string(dir.join("commondir")) {
            Ok(common) => dir.join(common.trim()),
            Err(_) => dir.clone(),
        };
        Some(Self { dir, common })
    }
}

/// Finds, pins or evaluates dev shells for one process.
pub struct Resolver {
    cache: Option<Arc<Client>>,
    /// The shared cache directory: GC roots and activation scripts. Bound
    /// at its host path in views, where the Nix daemon resolves the roots.
    dir: PathBuf,
    builder: PathBuf,
    /// The builder's environment: the workset's command environment.
    environment: Vec<(OsString, OsString)>,
    /// One resolution per key at a time; the rest find its result.
    keys: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    hot: Option<Hot>,
}

type Slot = (PathBuf, String);

/// Shells resolved before, each until something it was built from changes.
struct Hot {
    watcher: rho_watch::Watcher,
    shells: Mutex<HashMap<Slot, Kept>>,
    /// What each flake's last shell watched, to watch before resolving it
    /// again.
    watches: Mutex<HashMap<Slot, Watch>>,
}

/// A shell [`Resolver::hot`] uses without asking the builder.
struct Kept {
    resolved: Resolved,
    subscription: rho_watch::Subscription,
    /// When the daemon last heard the entry was used.
    reported: Instant,
}

/// How often a kept shell in use is reported used, to stay among the
/// pinned. Reporting every use would cost each command a round trip and a
/// database commit.
const REPORT_USE_EVERY: Duration = Duration::from_secs(60);

static RESOLVER: OnceLock<Arc<Resolver>> = OnceLock::new();

/// Make `resolver` the one [`resolver`] returns. The first call wins.
pub fn install(resolver: Resolver) {
    let _ = RESOLVER.set(Arc::new(resolver));
}

/// This process's resolver: the installed one, else one for a process in a
/// view, with the daemon's cache at `$RHO_DEVSHELL_DIR` if that is set.
pub fn resolver() -> Arc<Resolver> {
    RESOLVER
        .get_or_init(|| {
            let dir = std::env::var_os("RHO_DEVSHELL_DIR").map(PathBuf::from);
            Arc::new(Resolver::new(
                dir.as_deref().map(Client::new),
                dir.unwrap_or_else(|| std::env::temp_dir().join("rho-devshell")),
                rho_fs_view::devshell_builder(),
                std::env::vars_os().collect(),
            ))
        })
        .clone()
}

/// `program` in the dev shell of the flake `cwd` is in, through
/// [`resolver`]: Bash running [`EXEC_SCRIPT`], or `program` itself outside
/// flakes and when the shell cannot be had. Further arguments go to the
/// program.
pub async fn command(cwd: &Path, program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    let flake = {
        let cwd = cwd.to_owned();
        tokio::task::spawn_blocking(move || find_flake(&cwd)).await.ok().flatten()
    };
    let activation = match flake {
        None => None,
        Some(dir) => {
            let resolver = resolver();
            let activation = async {
                let (resolved, _) = resolver.resolve(&Flake::new(dir, "default")).await?;
                resolver.activation(&resolved.env_store_path).await
            };
            match activation.await {
                Ok(activation) => Some(activation),
                Err(e) => {
                    eprintln!("rho: dev shell unavailable, running without it: {e:#}");
                    None
                }
            }
        }
    };
    match activation {
        None => tokio::process::Command::new(program),
        Some(activation) => {
            let mut command = tokio::process::Command::new("bash");
            command
                .args(["--noprofile", "--norc", "-c", EXEC_SCRIPT, "bash"])
                .arg(activation)
                .arg(program);
            command
        }
    }
}

impl Resolver {
    /// Without `cache`, every shell is evaluated, and `dir` is only where
    /// activation scripts go.
    pub fn new(cache: Option<Client>, dir: PathBuf, builder: PathBuf, environment: Vec<(OsString, OsString)>) -> Self {
        Self {
            cache: cache.map(Arc::new),
            dir,
            builder,
            environment,
            keys: Mutex::default(),
            hot: None,
        }
    }

    /// Keep resolved shells while `watcher` sees nothing they were built
    /// from change; using one again needs no checks.
    pub fn with_watcher(mut self, watcher: rho_watch::Watcher) -> Self {
        self.hot = Some(Hot {
            watcher,
            shells: Mutex::default(),
            watches: Mutex::default(),
        });
        self
    }

    /// `flake`'s shell: a valid cached one, pinned, or a new evaluation.
    /// Also returns the builder's diagnostics.
    pub async fn resolve(&self, flake: &Flake) -> Result<(Resolved, Vec<u8>)> {
        if let Some(resolved) = self.hot(flake).await {
            return Ok((resolved, Vec::new()));
        }
        let key = {
            let flake = flake.clone();
            tokio::task::spawn_blocking(move || flake.key()).await??
        };
        let lock = self.keys.lock().unwrap().entry(key.clone()).or_default().clone();
        let result = {
            let _guard = lock.lock().await;
            match self.hot(flake).await {
                Some(resolved) => Ok((resolved, Vec::new())),
                None => self.resolve_now(flake).await,
            }
        };
        drop(lock);
        let mut keys = self.keys.lock().unwrap();
        if keys.get(&key).is_some_and(|lock| Arc::strong_count(lock) == 1) {
            keys.remove(&key);
        }
        result
    }

    /// A kept shell nothing has changed under, used as a cache hit is.
    async fn hot(&self, flake: &Flake) -> Option<Resolved> {
        let hot = self.hot.as_ref()?;
        let slot = slot(flake);
        let (resolved, report) = {
            let mut shells = hot.shells.lock().unwrap();
            let kept = shells.get_mut(&slot)?;
            // An environment unpinned while kept may have been collected.
            if !matches!(kept.subscription.changed(), Ok(false))
                || !Path::new(&kept.resolved.env_store_path).exists()
            {
                shells.remove(&slot);
                return None;
            }
            let report = kept.reported.elapsed() >= REPORT_USE_EVERY;
            if report {
                kept.reported = Instant::now();
            }
            (kept.resolved.clone(), report)
        };
        if report && let (Some(cache), Some(id)) = (self.cache.clone(), resolved.id) {
            let pin = self.pin_command(&resolved.env_store_path);
            tokio::spawn(async move {
                if let Ok(false) = cache.used(id).await
                    && let Ok(false) = pinned(pin).await
                {
                    let _ = cache.forget(id).await;
                }
            });
        }
        Some(resolved)
    }

    async fn resolve_now(&self, flake: &Flake) -> Result<(Resolved, Vec<u8>)> {
        // Watching what the flake's last shell watched before the builder
        // checks means a change after the check cannot go unseen.
        let before = match &self.hot {
            Some(hot) => {
                let watch = hot.watches.lock().unwrap().get(&slot(flake)).cloned();
                match watch {
                    Some(watch) => self.subscribe(hot, watch).await,
                    None => None,
                }
            }
            None => None,
        };
        let (shell, diagnostics) = self.build(flake).await?;
        let resolved = Resolved {
            id: shell.id,
            env_store_path: shell.env_store_path,
            watch: shell.watch,
        };
        self.keep(flake, resolved.clone(), before).await;
        Ok((resolved, diagnostics))
    }

    /// Keep `resolved` for [`Self::hot`] if nothing it was built from can
    /// have changed since the builder checked it unseen: watched from
    /// before, or watched now and confirmed by checking again.
    async fn keep(&self, flake: &Flake, mut resolved: Resolved, before: Option<(Watch, rho_watch::Subscription)>) {
        let Some(hot) = &self.hot else {
            return;
        };
        if resolved.id.is_none() {
            return;
        }
        hot.watches.lock().unwrap().insert(slot(flake), resolved.watch.clone());
        let subscription = match before {
            Some((watch, subscription))
                if resolved.watch.within(&watch) && matches!(subscription.changed(), Ok(false)) =>
            {
                subscription
            }
            _ => {
                let Some((_, subscription)) = self.subscribe(hot, resolved.watch.clone()).await else {
                    return;
                };
                let Ok((again, _)) = self.build(flake).await else { return };
                if again.id.is_none()
                    || again.env_store_path != resolved.env_store_path
                    || !again.watch.within(&resolved.watch)
                {
                    return;
                }
                // The entry checked last is the one to report used.
                resolved.id = again.id;
                subscription
            }
        };
        hot.shells.lock().unwrap().insert(
            slot(flake),
            Kept {
                resolved,
                subscription,
                reported: Instant::now(),
            },
        );
    }

    async fn subscribe(&self, hot: &Hot, watch: Watch) -> Option<(Watch, rho_watch::Subscription)> {
        let watcher = hot.watcher.clone();
        tokio::task::spawn_blocking(move || {
            let subscription = watch.subscribe(&watcher).ok()?;
            Some((watch, subscription))
        })
        .await
        .ok()
        .flatten()
    }

    /// Bash applying the shell at `env_store_path` to the caller's
    /// environment as `nix develop` does, `shellHook` included: a script
    /// written once per environment and shared. Run [`AFTER_SHELL`] after
    /// it.
    pub async fn activation(&self, env_store_path: &str) -> Result<PathBuf> {
        let path = activation_path(&self.dir, env_store_path);
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            return Ok(path);
        }
        let mut command = self.builder_command();
        command.arg("activate").arg(env_store_path).arg("--dir").arg(&self.dir);
        let output = run(command).await?;
        ensure!(
            output.status.success(),
            "writing the activation script failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(path)
    }

    /// The builder pinning `env_store_path` with a GC root, for [`pinned`].
    fn pin_command(&self, env_store_path: &str) -> tokio::process::Command {
        let mut command = self.builder_command();
        command.arg("pin").arg(env_store_path).arg(gc_root(&roots_dir(&self.dir), env_store_path));
        command
    }

    /// Run `rho-devshell-builder shell` for `flake`.
    async fn build(&self, flake: &Flake) -> Result<(Shell, Vec<u8>)> {
        let mut command = self.builder_command();
        command
            .arg("shell")
            .arg(&flake.dir)
            .arg("--shell")
            .arg(&flake.shell)
            .arg("--dir")
            .arg(&self.dir);
        if self.cache.is_none() {
            command.arg("--no-cache");
        }
        command.current_dir(&flake.dir);
        let output = run(command).await?;
        ensure!(
            output.status.success(),
            "rho-devshell-builder failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let shell = serde_json::from_slice(&output.stdout).context("parse rho-devshell-builder output")?;
        Ok((shell, output.stderr))
    }

    fn builder_command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.builder);
        command.env_clear().envs(self.environment.iter().map(|(k, v)| (k, v)));
        command
    }
}

fn slot(flake: &Flake) -> Slot {
    (flake.dir.clone(), flake.shell.clone())
}

/// `rho-devshell-builder pin`'s exit status for a path already collected.
pub const PIN_GONE: i32 = 3;

/// Run a [`Resolver::pin_command`]: whether the environment is pinned,
/// `false` if it is gone.
async fn pinned(command: tokio::process::Command) -> Result<bool> {
    let output = run(command).await?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(PIN_GONE) => Ok(false),
        _ => bail!("pinning failed: {}", String::from_utf8_lossy(&output.stderr)),
    }
}

const MAX_OUTPUT: u64 = 4 * 1024 * 1024;

struct Output {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Run `command` to completion with bounded output, killing its process
/// group if the caller goes away.
async fn run(mut command: tokio::process::Command) -> Result<Output> {
    command
        .kill_on_drop(true)
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    rho_fs_view::command_stdio_only(&mut command);
    let mut child = command.spawn().context("start rho-devshell-builder")?;
    let _group = Group(child.id().and_then(|id| rustix::process::Pid::from_raw(id as i32)));
    let mut stdout = child.stdout.take().unwrap().take(MAX_OUTPUT + 1);
    let mut stderr = child.stderr.take().unwrap().take(64 * 1024);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let (status, _, _) = tokio::try_join!(child.wait(), stdout.read_to_end(&mut out), stderr.read_to_end(&mut err))?;
    ensure!(out.len() as u64 <= MAX_OUTPUT, "rho-devshell-builder output too large");
    Ok(Output {
        status,
        stdout: out,
        stderr: err,
    })
}

/// Kills a builder's process group when dropped.
struct Group(Option<rustix::process::Pid>);

impl Drop for Group {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flake(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            let path = dir.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    fn key(dir: &Path, shell: &str) -> String {
        Flake::new(dir.to_owned(), shell).key().unwrap()
    }

    #[test]
    fn keys_are_shared_by_copies_and_split_by_everything_every_entry_read() {
        let files = [("flake.nix", "{}"), ("flake.lock", "lock"), ("other", "x")];
        let (one, two) = (flake(&files), flake(&files));
        assert_eq!(key(one.path(), "default"), key(two.path(), "default"));
        std::fs::write(two.path().join("other"), "y").unwrap();
        assert_eq!(key(one.path(), "default"), key(two.path(), "default"));
        assert_ne!(key(one.path(), "default"), key(one.path(), "ci"));
        std::fs::write(two.path().join("flake.lock"), "newer").unwrap();
        assert_ne!(key(one.path(), "default"), key(two.path(), "default"));
        std::fs::remove_file(two.path().join("flake.lock")).unwrap();
        std::fs::write(two.path().join("flake.nix"), "{ }").unwrap();
        assert_ne!(key(one.path(), "default"), key(two.path(), "default"));
    }

    #[test]
    fn a_flake_in_a_subdirectory_has_its_own_key() {
        let repo = flake(&[("flake.nix", "{}"), ("sub/flake.nix", "{}")]);
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        let sub = Flake::new(repo.path().join("sub"), "default");
        assert_eq!(sub.source.subdir, Path::new("sub"));
        assert_ne!(sub.key().unwrap(), key(repo.path(), "default"));
    }

    #[test]
    fn input_urls_inside_the_source_are_relative_to_it() {
        let root = Path::new("/work/a b");
        let url = InputUrl::parse("git+file:///work/a%20b?dir=sub", root).unwrap();
        assert_eq!(url.dir, Path::new(""));
        assert_eq!(url.url(Path::new("/other/ch%eckout")), "git+file:///other/ch%25eckout?dir=sub");
        let nested = InputUrl::parse("path:/work/a%20b/vendored", root).unwrap();
        assert_eq!(nested.dir, Path::new("vendored"));
        assert_eq!(nested.url(root), "path:/work/a%20b/vendored");
        let outside = InputUrl::parse("path:/elsewhere#x", root).unwrap();
        assert_eq!(outside.url(Path::new("/other")), "path:/elsewhere#x");
        assert_eq!(InputUrl::parse("github:NixOS/nixpkgs", root), None);
    }

    #[test]
    fn watches_what_was_observed_in_this_checkout() {
        let repo = flake(&[("flake.nix", "{}"), ("sub/flake.nix", "{}")]);
        std::fs::create_dir_all(repo.path().join(".git/refs/heads")).unwrap();
        std::fs::write(repo.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        let root = repo.path().canonicalize().unwrap();
        let flake = Flake::new(root.join("sub"), "default");
        let observation = |kind: &str, url: &str, path: &str| Observation {
            kind: kind.into(),
            input: InputUrl::parse(url, Path::new("/elsewhere")).unwrap(),
            path: path.into(),
            value: String::new(),
        };
        let watch = Watch::new(
            &flake,
            &[
                observation("file", "git+file:///elsewhere", "sub/flake.nix"),
                observation("dir", "git+file:///elsewhere", ""),
                observation("stat", "git+file:///elsewhere", "sub/missing"),
                observation("attr", "git+file:///elsewhere", "rev"),
                observation("file", "path:/outside", "x.nix"),
            ],
        );
        let git = root.join(".git");
        let contents: BTreeSet<PathBuf> = [
            root.join("sub/flake.nix"),
            root.join("sub/flake.lock"),
            root.clone(),
            git.join("HEAD"),
            git.join("packed-refs"),
            git.join("refs/heads/main"),
            git.join("index"),
            PathBuf::from("/outside/x.nix"),
        ]
        .into();
        assert_eq!(watch.contents, contents);
        assert_eq!(watch.names, [git, root.join("sub/missing")].into());
        assert!(Watch::new(&flake, &[]).within(&watch));
        assert!(!watch.within(&Watch::new(&flake, &[])));
    }
}
