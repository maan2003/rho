//! Flake dev shells for rho's commands: which shell a directory gets, and
//! how an evaluated shell is found again.
//!
//! One [`Resolver`] per workset process serves every agent's commands,
//! terminals and sidecars; `rho-devshell-builder shell` runs one for a
//! `nix develop` in a view. Shells are cached by the daemon ([`Client`])
//! under a key that every valid entry for a flake shares (see
//! [`Flake::key`]); entries record what evaluation read, and are checked
//! here, in the caller's filesystem namespace, where those paths mean what
//! they meant to the evaluator. A miss runs `rho-devshell-builder eval`,
//! which evaluates with libnix in its own process. With a watcher, a
//! resolved shell is kept until something it was built from changes.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context as _, Result, bail, ensure};
use devenv_core::ObservedKind;
use devenv_core::build_environment::BuildEnvironment;
use devenv_eval_cache::eval_inputs::Uncached;
use devenv_eval_cache::{Checkout, FlakeScheme, Input};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt as _;

pub mod protocol;
pub use protocol::Client;

/// Changes whenever evaluation semantics change (evaluator, Nix fork
/// patches), so shells of an older evaluator are never found.
pub const EVALUATOR: &str = concat!("rho-devshell/", env!("CARGO_PKG_VERSION"), " nix-2.35-rho3");

/// Bash running a program after the activation script in `$1`. `shellHook`
/// output goes to stderr so that stdout is the program's alone.
pub const EXEC_SCRIPT: &str = r#"script=$1; shift; . "$script" >&2; unset script; exec "$@""#;

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

/// Where a flake's source tree is fetched from, as Nix does for a local
/// flake: the enclosing git repository (with the flake in a subdirectory of
/// it), or the flake directory itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub scheme: FlakeScheme,
    pub root: PathBuf,
    /// The flake directory relative to `root`.
    pub subdir: PathBuf,
}

impl Source {
    pub fn find(flake_dir: &Path) -> Self {
        for dir in flake_dir.ancestors() {
            if dir.join(".git").exists() {
                return Self {
                    scheme: FlakeScheme::Git,
                    root: dir.to_path_buf(),
                    subdir: flake_dir.strip_prefix(dir).unwrap_or(Path::new("")).to_path_buf(),
                };
            }
        }
        Self {
            scheme: FlakeScheme::Path,
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

/// What `rho-devshell-builder eval` reports: the shell, and what its
/// evaluation read, if that could be recorded.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Evaluated {
    pub drv_path: String,
    pub env_store_path: String,
    /// `None` if the shell cannot be cached.
    pub inputs: Option<Vec<Input>>,
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

/// Where the activation script of `env_store_path` is written below the
/// shared cache directory.
pub fn activation_path(dir: &Path, env_store_path: &str) -> PathBuf {
    dir.join("activations").join(format!("{}.sh", store_basename(env_store_path)))
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
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
    fn subscribe(&self, watcher: &rho_watch::Watcher) -> io::Result<rho_watch::Subscription> {
        watcher.watch(
            self.contents.iter().map(PathBuf::as_path),
            self.names.iter().map(PathBuf::as_path),
        )
    }
}

impl Watch {
    fn new(flake: &Flake, inputs: &[Input]) -> Self {
        let source = &flake.source;
        let mut watch = Self::default();
        watch.contents.insert(flake.dir.join("flake.nix"));
        watch.contents.insert(flake.dir.join("flake.lock"));
        // Whether the flake is fetched from git.
        watch.names.insert(source.root.join(".git"));
        let absolute = |rel: &Path| {
            if rel.as_os_str().is_empty() { source.root.clone() } else { source.root.join(rel) }
        };
        for input in inputs {
            match input {
                Input::Path(p) if p.kind == ObservedKind::Stat => {
                    watch.names.insert(absolute(&p.path));
                }
                Input::Path(p) => {
                    watch.contents.insert(absolute(&p.path));
                }
                Input::FlakeRev(_) => {
                    if let Some(git) = GitDirs::find(&source.root) {
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
            }
        }
        if source.scheme == FlakeScheme::Git
            && let Some(git) = GitDirs::find(&source.root)
        {
            watch.contents.insert(git.dir.join("index"));
        }
        watch.names.retain(|path| !watch.contents.contains(path));
        watch
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

/// The candidates whose recorded inputs all hold in the checkout now, in
/// the order given. Candidates mostly share inputs; each is observed once.
fn valid(source: &Source, candidates: Vec<Candidate>) -> Result<Vec<(Candidate, Evaluated)>> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let checkout = Checkout::new(&source.root, source.scheme)?;
    let mut now = HashMap::new();
    let mut valid = Vec::new();
    for candidate in candidates {
        let Ok(evaluated) = serde_json::from_slice::<Evaluated>(&candidate.data) else {
            continue;
        };
        let Some(inputs) = &evaluated.inputs else { continue };
        if holds(&checkout, &mut now, inputs) {
            valid.push((candidate, evaluated));
        }
    }
    Ok(valid)
}

/// Whether every input observes now as it was recorded, remembering what
/// was observed in `now`.
fn holds(
    checkout: &Checkout,
    now: &mut HashMap<devenv_eval_cache::eval_inputs::InputIdentity, Option<Input>>,
    inputs: &[Input],
) -> bool {
    inputs.iter().all(|input| {
        now.entry(input.identity())
            .or_insert_with(|| input.recapture(checkout, &mut Uncached).ok())
            .as_ref()
            == Some(input)
    })
}

/// Bash applying the environment at `env_store_path` (the JSON Nix's
/// `get-env.sh` writes), then [`AFTER_SHELL`].
fn activation_script(env_store_path: &str) -> Result<String> {
    let json = std::fs::read_to_string(env_store_path)
        .with_context(|| format!("read {env_store_path}"))?;
    let mut script = BuildEnvironment::from_json(&json)
        .with_context(|| format!("parse {env_store_path}"))?
        .to_activation_script();
    script.push_str(AFTER_SHELL);
    Ok(script)
}

/// The caller's tools ahead of the shell's own. `RHO_DEVSHELL_CARGO`, a
/// directory with the `cargo` to use, goes first, except that a shell whose
/// `cargo` is cargo-deluxe keeps it: deluxe intercepts and runs the next
/// `cargo` on `PATH`, so this one goes right after it. Then
/// `RHO_DEVSHELL_PATH_PREFIX` goes before everything.
pub const AFTER_SHELL: &str = r#"
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
"#;

/// Finds, pins or evaluates dev shells for one process.
pub struct Resolver {
    cache: Option<Client>,
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

/// Shells resolved before, each until something it was built from changes.
struct Hot {
    watcher: rho_watch::Watcher,
    shells: Mutex<HashMap<(PathBuf, String), (Resolved, rho_watch::Subscription)>>,
}

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
                dir.unwrap_or_default(),
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
            cache,
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
            self.resolve_key(flake, &key).await
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
        let slot = (flake.dir.clone(), flake.shell.clone());
        let resolved = {
            let mut shells = hot.shells.lock().unwrap();
            let (resolved, subscription) = shells.get(&slot)?;
            if !matches!(subscription.changed(), Ok(false)) {
                shells.remove(&slot);
                return None;
            }
            resolved.clone()
        };
        if let (Some(cache), Some(id)) = (&self.cache, resolved.id) {
            // Without the daemon, the shell is as valid as before.
            if let Ok(false) = cache.used(id).await
                && let Ok(false) = self.pin(&resolved.env_store_path).await
            {
                let _ = cache.forget(id).await;
                hot.shells.lock().unwrap().remove(&slot);
                return None;
            }
        }
        Some(resolved)
    }

    async fn resolve_key(&self, flake: &Flake, key: &str) -> Result<(Resolved, Vec<u8>)> {
        let mut diagnostics = Vec::new();
        if let Some(cache) = &self.cache {
            match self.cached(cache, flake, key).await {
                Ok(Some((resolved, evaluated))) => {
                    self.keep(flake, &resolved, evaluated).await;
                    return Ok((resolved, diagnostics));
                }
                Ok(None) => {}
                Err(e) => diagnostics.extend(format!("rho: dev shell cache unavailable: {e:#}\n").bytes()),
            }
        }
        let (evaluated, builder_output) = self.evaluate(flake).await?;
        diagnostics.extend(builder_output);
        let mut id = None;
        if let (Some(cache), Some(_)) = (&self.cache, &evaluated.inputs) {
            match cache
                .store(key.to_owned(), evaluated.env_store_path.clone(), serde_json::to_vec(&evaluated)?)
                .await
            {
                Ok(stored) => id = Some(stored),
                Err(e) => diagnostics.extend(format!("rho: failed to cache dev shell: {e:#}\n").bytes()),
            }
        }
        let resolved = Self::resolved(flake, id, &evaluated);
        self.keep(flake, &resolved, evaluated).await;
        Ok((resolved, diagnostics))
    }

    /// Keep `resolved` for [`Self::hot`] if its inputs, now watched, still
    /// hold: a change after they were checked or read but before the watch
    /// existed would otherwise go unseen.
    async fn keep(&self, flake: &Flake, resolved: &Resolved, evaluated: Evaluated) {
        let (Some(hot), Some(inputs)) = (&self.hot, evaluated.inputs) else {
            return;
        };
        let watcher = hot.watcher.clone();
        let watch = resolved.watch.clone();
        let source = flake.source.clone();
        let subscription = tokio::task::spawn_blocking(move || {
            let subscription = watch.subscribe(&watcher).ok()?;
            let checkout = Checkout::new(&source.root, source.scheme).ok()?;
            holds(&checkout, &mut HashMap::new(), &inputs).then_some(subscription)
        })
        .await;
        if let Ok(Some(subscription)) = subscription {
            hot.shells
                .lock()
                .unwrap()
                .insert((flake.dir.clone(), flake.shell.clone()), (resolved.clone(), subscription));
        }
    }

    /// The newest valid cached shell whose environment could be pinned.
    async fn cached(&self, cache: &Client, flake: &Flake, key: &str) -> Result<Option<(Resolved, Evaluated)>> {
        let candidates = cache.lookup(key.to_owned()).await?;
        let valid = {
            let source = flake.source.clone();
            tokio::task::spawn_blocking(move || valid(&source, candidates)).await??
        };
        for (candidate, evaluated) in valid {
            if !cache.used(candidate.id).await? && !self.pin(&candidate.env_store_path).await? {
                cache.forget(candidate.id).await?;
                continue;
            }
            return Ok(Some((Self::resolved(flake, Some(candidate.id), &evaluated), evaluated)));
        }
        Ok(None)
    }

    fn resolved(flake: &Flake, id: Option<u64>, evaluated: &Evaluated) -> Resolved {
        Resolved {
            id,
            env_store_path: evaluated.env_store_path.clone(),
            watch: Watch::new(flake, evaluated.inputs.as_deref().unwrap_or_default()),
        }
    }

    /// Bash applying the shell at `env_store_path` to the caller's
    /// environment, `shellHook` included, then rho's `PATH` policy: a
    /// script written once per environment and shared.
    pub async fn activation(&self, env_store_path: &str) -> Result<PathBuf> {
        let path = activation_path(&self.dir, env_store_path);
        let dir = path.parent().unwrap().to_owned();
        let env_store_path = env_store_path.to_owned();
        let target = path.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            if target.exists() {
                return Ok(());
            }
            let script = activation_script(&env_store_path)?;
            std::fs::create_dir_all(&dir)?;
            let mut temporary = tempfile::NamedTempFile::new_in(&dir)?;
            std::io::Write::write_all(&mut temporary, script.as_bytes())?;
            temporary.persist(&target)?;
            Ok(())
        })
        .await??;
        Ok(path)
    }

    /// Pin `env_store_path` with a GC root; `false` if it is gone.
    async fn pin(&self, env_store_path: &str) -> Result<bool> {
        let mut command = self.builder_command();
        command.arg("pin").arg(env_store_path).arg(gc_root(&roots_dir(&self.dir), env_store_path));
        let output = run(command).await?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(PIN_GONE) => Ok(false),
            _ => bail!("pinning {env_store_path} failed: {}", String::from_utf8_lossy(&output.stderr)),
        }
    }

    async fn evaluate(&self, flake: &Flake) -> Result<(Evaluated, Vec<u8>)> {
        let mut command = self.builder_command();
        command.arg("eval").arg(&flake.dir).arg("--shell").arg(&flake.shell);
        if self.cache.is_some() {
            command.arg("--roots").arg(roots_dir(&self.dir));
        }
        command.current_dir(&flake.dir);
        let output = run(command).await?;
        ensure!(
            output.status.success(),
            "rho-devshell-builder failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let evaluated = serde_json::from_slice(&output.stdout).context("parse rho-devshell-builder output")?;
        Ok((evaluated, output.stderr))
    }

    fn builder_command(&self) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(&self.builder);
        command.env_clear().envs(self.environment.iter().map(|(k, v)| (k, v)));
        command
    }
}

/// `rho-devshell-builder pin`'s exit status for a path already collected.
pub const PIN_GONE: i32 = 3;

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
    use devenv_eval_cache::PathInput;

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
    fn only_candidates_whose_inputs_hold_are_valid_in_order() {
        let dir = flake(&[("flake.nix", "{}"), ("shell.nix", "one")]);
        let source = Source::find(dir.path());
        let checkout = Checkout::new(&source.root, source.scheme).unwrap();
        let input = |rel: &str| {
            Input::Path(PathInput {
                kind: ObservedKind::File,
                scheme: FlakeScheme::Path,
                path: rel.into(),
                value: String::new(),
            })
            .recapture(&checkout, &mut Uncached)
            .unwrap()
        };
        let candidate = |id, inputs: Option<Vec<Input>>| Candidate {
            id,
            env_store_path: format!("/nix/store/{id}-env"),
            data: serde_json::to_vec(&Evaluated {
                drv_path: String::new(),
                env_store_path: String::new(),
                inputs,
            })
            .unwrap(),
        };
        let old = candidate(1, Some(vec![input("flake.nix"), input("shell.nix")]));
        std::fs::write(dir.path().join("shell.nix"), "two").unwrap();
        let candidates = vec![
            old,
            candidate(2, None),
            candidate(3, Some(vec![input("flake.nix"), input("shell.nix")])),
            candidate(4, Some(vec![input("flake.nix")])),
        ];
        let ids: Vec<u64> = valid(&source, candidates).unwrap().into_iter().map(|(c, _)| c.id).collect();
        assert_eq!(ids, [3, 4]);
    }
}
