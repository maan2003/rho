//! Per-agent environment generations. A command's environment is the dev
//! shell of the nearest flake, from the process's `rho_devshell` resolver,
//! or the base environment outside flakes. Only cold/invalidated generations
//! ask for the shell; command admission checks the process's watches
//! (`rho_watch`) over what the shell was built from before reusing a
//! snapshot. Paths are absolute; resolver tasks inherit the workset process
//! namespace.
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use futures::future::BoxFuture;
use rho_watch::Subscription;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

pub(super) type Environment = BTreeMap<OsString, OsString>;
const MAX_ENV_BYTES: usize = 4 * 1024 * 1024;
const CACHE_SIZE: usize = 32;
// A cold shell may build a toolchain.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Where a flake's shell comes from: the process's dev shell resolver, or a
/// stand-in in tests.
type Shells = Arc<dyn Fn(PathBuf) -> BoxFuture<'static, Result<(Built, Vec<u8>)>> + Send + Sync>;

pub(super) struct Worker {
    sender: tokio::sync::OnceCell<(mpsc::Sender<Request>, mpsc::UnboundedSender<Key>)>,
    permits: Arc<tokio::sync::Semaphore>,
    shells: Shells,
}

impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker").finish_non_exhaustive()
    }
}

impl Default for Worker {
    fn default() -> Self {
        Self::with_shells(Arc::new(|flake| Box::pin(resolved_shell(flake))))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    cwd: PathBuf,
    base: Arc<Environment>,
}

struct Request {
    key: Key,
    reply: oneshot::Sender<Result<Resolved>>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

pub(super) struct Resolved {
    pub environment: Arc<Environment>,
    pub diagnostics: Vec<u8>,
}

struct Snapshot {
    environment: Arc<Environment>,
    watches: Subscription,
    /// Where the environment came from, to reuse it when a change turns out
    /// not to affect the shell.
    source: Option<Source>,
}

/// A cached shell an environment was activated from.
struct Source {
    flake: PathBuf,
    eval_id: u64,
    contents: HashSet<PathBuf>,
    names: HashSet<PathBuf>,
}

impl Worker {
    fn with_shells(shells: Shells) -> Self {
        Self {
            sender: tokio::sync::OnceCell::new(),
            permits: Arc::new(tokio::sync::Semaphore::new(64)),
            shells,
        }
    }

    pub async fn resolve(&self, cwd: PathBuf, base: Environment) -> Result<Resolved> {
        let permit = Arc::clone(&self.permits).acquire_owned().await?;
        let (sender, cancelled) = self
            .sender
            .get_or_try_init(|| async {
                let (sender, receiver) = mpsc::channel(64);
                let (cancelled, cancellations) = mpsc::unbounded_channel();
                tokio::spawn(serve(receiver, cancellations, self.shells.clone()));
                Ok::<_, anyhow::Error>((sender, cancelled))
            })
            .await?;
        let key = Key {
            cwd,
            base: Arc::new(base),
        };
        // Declared before the reply, so that a caller going away closes
        // the reply before this tells the worker to look.
        let mut cancel = Cancel(Some((key.clone(), cancelled.clone())));
        let (reply, result) = oneshot::channel();
        sender
            .send(Request {
                key,
                reply,
                _permit: permit,
            })
            .await
            .context("environment worker stopped")?;
        let result = result.await.context("environment worker stopped")?;
        cancel.0 = None;
        result
    }
}

/// Tells the worker when a caller stops waiting, so that a generation no
/// caller waits for is cancelled.
struct Cancel(Option<(Key, mpsc::UnboundedSender<Key>)>);

impl Drop for Cancel {
    fn drop(&mut self) {
        if let Some((key, cancelled)) = self.0.take() {
            let _ = cancelled.send(key);
        }
    }
}

async fn serve(
    mut requests: mpsc::Receiver<Request>,
    mut cancellations: mpsc::UnboundedReceiver<Key>,
    shells: Shells,
) {
    let mut snapshots: VecDeque<(Key, Snapshot)> = VecDeque::new();
    let mut loading = HashMap::<Key, (tokio::task::AbortHandle, Vec<Request>)>::new();
    let mut tasks = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            request = requests.recv(), if loading.len() < CACHE_SIZE => {
                let Some(request) = request else { break };
                if request.reply.is_closed() { continue; }
                let mut previous = None;
                if let Some(index) = snapshots.iter().position(|(key, _)| key == &request.key) {
                    let (key, snapshot) = snapshots.remove(index).unwrap();
                    if matches!(snapshot.watches.changed(), Ok(false)) {
                        let _ = request.reply.send(Ok(Resolved {
                            environment: Arc::clone(&snapshot.environment),
                            diagnostics: Vec::new(),
                        }));
                        snapshots.push_front((key, snapshot));
                        continue;
                    }
                    previous = Some(snapshot);
                }
                if let Some((_, waiters)) = loading.get_mut(&request.key) {
                    waiters.push(request);
                } else {
                    let key = request.key.clone();
                    let shells = shells.clone();
                    let task = tasks.spawn(async move {
                        let result = tokio::time::timeout(RESOLVE_TIMEOUT, resolve(&key, previous, &shells)).await
                            .map_err(|_| anyhow!("environment resolution timed out"))
                            .and_then(|result| result);
                        (key, result)
                    });
                    loading.insert(request.key.clone(), (task, vec![request]));
                }
            }
            result = tasks.join_next_with_id(), if !tasks.is_empty() => {
                let Some(result) = result else { continue };
                let (id, (key, result)) = match result {
                    Ok(result) => result,
                    Err(error) => {
                        let key = loading.iter().find(|(_, (task, _))| task.id() == error.id())
                            .map(|(key, _)| key.clone());
                        if let Some(key) = key {
                            let (_, waiters) = loading.remove(&key).unwrap();
                            for waiter in waiters {
                                let _ = waiter.reply.send(Err(anyhow!("environment resolver stopped: {error}")));
                            }
                        }
                        continue;
                    }
                };
                if !loading.get(&key).is_some_and(|(task, _)| task.id() == id) {
                    continue; // cancelled generations cannot settle replacement callers
                }
                let (_, waiters) = loading.remove(&key).unwrap();
                match result {
                    Ok((snapshot, diagnostics)) => {
                        let mut diagnostics = Some(diagnostics);
                        for waiter in waiters {
                            if !waiter.reply.is_closed() {
                                let _ = waiter.reply.send(Ok(Resolved {
                                    environment: Arc::clone(&snapshot.environment),
                                    diagnostics: diagnostics.take().unwrap_or_default(),
                                }));
                            }
                        }
                        snapshots.push_front((key, snapshot));
                        snapshots.truncate(CACHE_SIZE);
                    }
                    Err(error) => {
                        let message = format!("{error:#}");
                        for waiter in waiters {
                            let _ = waiter.reply.send(Err(anyhow!(message.clone())));
                        }
                    }
                }
            }
            Some(key) = cancellations.recv() => {
                if let Some((task, waiters)) = loading.get_mut(&key) {
                    waiters.retain(|waiter| !waiter.reply.is_closed());
                    if waiters.is_empty() {
                        task.abort();
                        loading.remove(&key);
                    }
                }
            }
        }
    }
    // JoinSet drops/aborts resolvers; their kill_on_drop children cannot
    // outlive the final ShellTools owner.
}

struct ResolverGroup(rustix::process::Pid);

impl Drop for ResolverGroup {
    fn drop(&mut self) {
        let _ = rustix::process::kill_process_group(self.0, rustix::process::Signal::KILL);
    }
}

async fn bounded_output(
    reader: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
    what: &str,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(bytes.len() <= limit, "{what} output exceeds {limit} bytes");
    Ok(bytes)
}

/// Run `command` with `stdin`, returning its stdout and stderr.
async fn output(
    mut command: tokio::process::Command,
    stdin: &[u8],
    what: &str,
) -> Result<(Vec<u8>, Vec<u8>)> {
    command
        .kill_on_drop(true)
        .process_group(0)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    rho_fs_view::command_stdio_only(&mut command);
    let mut child = command.spawn().with_context(|| format!("start {what}"))?;
    let _group = ResolverGroup(rustix::process::Pid::from_raw(child.id().unwrap() as i32).unwrap());
    let mut input = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (status, (), output, diagnostics) = tokio::try_join!(
        async { child.wait().await.map_err(anyhow::Error::from) },
        async {
            // A child that exits without reading reports through its status.
            match input.write_all(stdin).await {
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
                result => result?,
            }
            drop(input);
            Ok(())
        },
        bounded_output(stdout, MAX_ENV_BYTES, what),
        bounded_output(stderr, 64 * 1024, what),
    )?;
    ensure!(
        status.success(),
        "{what} failed: {}",
        String::from_utf8_lossy(&diagnostics)
    );
    Ok((output, diagnostics))
}

/// Where commands in a directory get their environment from.
#[derive(PartialEq, Eq)]
struct Discovery {
    /// The nearest flake, found as Nix finds `.`: upward from the physical
    /// directory, stopping at the repository root.
    flake: Option<PathBuf>,
    /// Entries whose appearance or removal changes the answer.
    names: HashSet<PathBuf>,
}

fn discover(cwd: &Path) -> Result<Discovery> {
    let physical = cwd.canonicalize().context("resolve environment cwd")?;
    let mut names = HashSet::new();
    let mut flake = None;
    for dir in physical.ancestors() {
        names.insert(dir.join("flake.nix"));
        if dir.join("flake.nix").is_file() {
            flake = Some(dir.to_path_buf());
            break;
        }
        names.insert(dir.join(".git"));
        if dir.join(".git").exists() {
            break;
        }
    }
    // Retargeting a symlinked cwd changes the physical ancestors.
    names.insert(cwd.to_path_buf());
    Ok(Discovery { flake, names })
}

/// A flake's shell, for one generation.
struct Built {
    /// The cache entry, if the shell could be cached.
    eval_id: Option<u64>,
    /// Bash applying the shell to the caller's environment, `shellHook`
    /// included, then rho's `PATH` policy.
    activation: String,
    /// Paths whose contents the shell depends on.
    watch: Vec<PathBuf>,
    /// Paths the shell depends on existing as they are.
    watch_names: Vec<PathBuf>,
}

async fn resolved_shell(flake: PathBuf) -> Result<(Built, Vec<u8>)> {
    let resolver = rho_devshell::resolver();
    let (resolved, diagnostics) = resolver
        .resolve(&rho_devshell::Flake::new(flake, "default"))
        .await?;
    let path = resolver.activation(&resolved.env_store_path).await?;
    let mut activation = tokio::fs::read_to_string(&path)
        .await
        .with_context(|| format!("read {}", path.display()))?;
    activation.push_str(rho_devshell::AFTER_SHELL);
    Ok((
        Built {
            eval_id: resolved.id,
            activation,
            watch: resolved.watch.contents.into_iter().collect(),
            watch_names: resolved.watch.names.into_iter().collect(),
        },
        diagnostics,
    ))
}

async fn build(shells: &Shells, flake: &Path) -> Result<(Built, Vec<u8>)> {
    let (built, diagnostics) = shells(flake.to_owned()).await?;
    ensure!(
        built.watch.iter().chain(&built.watch_names).all(|path| path.is_absolute()),
        "relative shell watch path"
    );
    Ok((built, diagnostics))
}

/// Apply `activation` to the base environment as `nix develop` would, in Bash.
async fn activate(key: &Key, activation: &str) -> Result<(Environment, Vec<u8>)> {
    let mut command = tokio::process::Command::new("bash");
    // The script arrives on stdin; the shell hook's output goes to stderr so
    // that only `env -0` owns stdout.
    command
        .args([
            "--noprofile",
            "--norc",
            "-c",
            r#"script=$(cat) && exec 3>&1 1>&2 || exit; eval "$script"; exec env -0 >&3"#,
        ])
        .current_dir(&key.cwd)
        .env_clear()
        .envs(key.base.iter());
    let (bytes, diagnostics) = output(command, activation.as_bytes(), "shell activation").await?;
    let mut environment = Environment::new();
    for item in bytes.split(|b| *b == 0).filter(|item| !item.is_empty()) {
        let separator = item
            .iter()
            .position(|b| *b == b'=')
            .ok_or_else(|| anyhow!("invalid shell environment entry"))?;
        ensure!(separator > 0, "empty shell environment key");
        environment.insert(
            OsString::from_vec(item[..separator].to_vec()),
            OsString::from_vec(item[separator + 1..].to_vec()),
        );
    }
    Ok((environment, diagnostics))
}

async fn resolve(key: &Key, previous: Option<Snapshot>, shells: &Shells) -> Result<(Snapshot, Vec<u8>)> {
    if let Some(previous) = previous
        && let Some(reused) = reuse(key, previous, shells).await?
    {
        return Ok(reused);
    }
    for _ in 0..3 {
        let discovery = discover(&key.cwd)?;
        let mut contents = HashSet::new();
        let mut names = discovery.names.clone();
        let (environment, diagnostics, source) = match &discovery.flake {
            None => ((*key.base).clone(), Vec::new(), None),
            Some(flake) => {
                let (built, mut diagnostics) = build(shells, flake).await?;
                let (environment, hook_output) = activate(key, &built.activation).await?;
                diagnostics.extend(hook_output);
                contents.extend(built.watch.iter().cloned());
                names.extend(built.watch_names.iter().cloned());
                let source = built.eval_id.map(|eval_id| Source {
                    flake: flake.clone(),
                    eval_id,
                    contents: built.watch.into_iter().collect(),
                    names: built.watch_names.into_iter().collect(),
                });
                (environment, diagnostics, source)
            }
        };
        let watches = watch(&contents, &names)?;
        // A change after the builder observed an input but before its watch
        // existed is visible to neither; confirm the shell still holds now.
        // An uncacheable shell cannot be confirmed and is kept as built.
        let still_valid = discover(&key.cwd)? == discovery
            && match &source {
                Some(source) => build(shells, &source.flake).await?.0.eval_id == Some(source.eval_id),
                None => true,
            };
        if still_valid && !watches.changed()? {
            return Ok((
                Snapshot {
                    environment: Arc::new(environment),
                    watches,
                    source,
                },
                diagnostics,
            ));
        }
    }
    bail!("environment inputs kept changing during resolution; retry the command")
}

/// Keep `previous`'s environment if the change its watches reported left the
/// shell as it was, e.g. git rewriting its index or an editor saving
/// unchanged contents. Watching the same inputs before asking the builder
/// makes its answer hold until the next command drains the watches.
async fn reuse(key: &Key, previous: Snapshot, shells: &Shells) -> Result<Option<(Snapshot, Vec<u8>)>> {
    let Some(source) = previous.source else {
        return Ok(None);
    };
    let discovery = discover(&key.cwd)?;
    if discovery.flake.as_ref() != Some(&source.flake) {
        return Ok(None);
    }
    let names = source.names.union(&discovery.names).cloned().collect();
    let watches = watch(&source.contents, &names)?;
    let (built, diagnostics) = build(shells, &source.flake).await?;
    if built.eval_id != Some(source.eval_id) || discover(&key.cwd)? != discovery || watches.changed()? {
        return Ok(None);
    }
    Ok(Some((
        Snapshot {
            environment: previous.environment,
            watches,
            source: Some(source),
        },
        diagnostics,
    )))
}

/// Watch what an environment was derived from, with the process's shared
/// watches; checking drains them, so a change before admission is seen.
fn watch(contents: &HashSet<PathBuf>, names: &HashSet<PathBuf>) -> Result<Subscription> {
    Ok(rho_watch::Watcher::global()?.watch(
        contents.iter().map(PathBuf::as_path),
        names.iter().map(PathBuf::as_path),
    )?)
}

#[cfg(test)]
mod tests {
    use rho_fs_view::PathOverrides;
    use tempfile::TempDir;

    use super::*;
    use crate::ShellTools;

    struct Fixture {
        root: TempDir,
        tools: ShellTools,
    }

    /// A stand-in for the dev shell resolver: the shell sources the flake's
    /// `env.sh` and depends on `env.sh` and an optional `extra`, and is
    /// cached by their contents.
    fn shells() -> Shells {
        Arc::new(|flake: PathBuf| {
            Box::pin(async move {
                let mut hasher = std::hash::DefaultHasher::new();
                for name in ["env.sh", "extra"] {
                    std::hash::Hash::hash(&std::fs::read(flake.join(name)).ok(), &mut hasher);
                }
                let built = Built {
                    eval_id: Some(std::hash::Hasher::finish(&hasher)),
                    activation: format!(". {}/env.sh", flake.display()),
                    watch: vec![flake.join("env.sh"), flake.join("extra")],
                    watch_names: Vec::new(),
                };
                Ok((built, Vec::new()))
            })
        })
    }

    impl Fixture {
        /// A flake at the root, with [`shells`].
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("flake.nix"), "").unwrap();
            let mut tools = ShellTools::in_directory(
                Duration::from_secs(5),
                camino::Utf8PathBuf::from_path_buf(root.path().to_owned()).unwrap(),
                PathOverrides::default(),
            )
            .with_env("RHO_CACHE_TEST_BASE", "base");
            tools.environments = Arc::new(Worker::with_shells(shells()));
            Self { root, tools }
        }

        fn write(&self, name: &str, text: &str) {
            std::fs::write(self.root.path().join(name), text).unwrap();
        }

        async fn run(&self, cmd: &str, cwd: Option<&str>) -> String {
            let mut process = self.tools.spawn(cmd, cwd).await.unwrap();
            let mut text = Vec::new();
            loop {
                let event = process.next().await;
                match event {
                    crate::ProcessEvent::Output(bytes) => text.extend(bytes),
                    crate::ProcessEvent::Exited(status) => assert!(status.success()),
                    crate::ProcessEvent::Closed => break,
                    crate::ProcessEvent::Failed(error) => panic!("{error}"),
                }
            }
            String::from_utf8(text).unwrap()
        }
    }

    #[tokio::test]
    async fn warm_commands_do_not_evaluate_again_and_preserve_unsets() {
        let fixture = Fixture::new();
        fixture.write(
            "env.sh",
            "printf x >> evaluations\nexport RHO_CACHE_TEST_VALUE=one\nunset RHO_CACHE_TEST_BASE\n",
        );
        let command =
            r#"printf 'value=%s base=%s' "$RHO_CACHE_TEST_VALUE" "${RHO_CACHE_TEST_BASE-unset}""#;
        assert!(
            fixture
                .run(command, None)
                .await
                .ends_with("value=one base=unset")
        );
        assert_eq!(fixture.run(command, None).await, "value=one base=unset");
        assert_eq!(
            std::fs::read(fixture.root.path().join("evaluations")).unwrap(),
            b"x"
        );
        fixture.write(
            "env.sh",
            "printf x >> evaluations\nexport RHO_CACHE_TEST_VALUE=two\n",
        );
        assert!(
            fixture
                .run(command, None)
                .await
                .ends_with("value=two base=base")
        );
        assert_eq!(
            std::fs::read(fixture.root.path().join("evaluations")).unwrap(),
            b"xx"
        );
    }

    #[tokio::test]
    async fn unwatched_files_do_not_invalidate() {
        let fixture = Fixture::new();
        fixture.write("env.sh", "printf x >> evaluations\n");
        fixture.run("true", None).await;
        fixture.write("unrelated", "x");
        std::fs::create_dir(fixture.root.path().join("dir")).unwrap();
        fixture.run("true", None).await;
        assert_eq!(
            std::fs::read(fixture.root.path().join("evaluations")).unwrap(),
            b"x"
        );
    }

    #[tokio::test]
    async fn changes_that_leave_the_shell_as_it_was_keep_the_environment() {
        let fixture = Fixture::new();
        fixture.write("env.sh", "printf x >> evaluations\n");
        fixture.run("true", None).await;
        // Same contents, new inode: the watch fires, the builder hits the
        // same entry, and the shell hook does not run again.
        fixture.write("replacement", "printf x >> evaluations\n");
        std::fs::rename(
            fixture.root.path().join("replacement"),
            fixture.root.path().join("env.sh"),
        )
        .unwrap();
        fixture.run("true", None).await;
        assert_eq!(
            std::fs::read(fixture.root.path().join("evaluations")).unwrap(),
            b"x"
        );
    }

    #[tokio::test]
    async fn outside_a_flake_commands_get_the_base_environment() {
        let fixture = Fixture::new();
        std::fs::remove_file(fixture.root.path().join("flake.nix")).unwrap();
        // Nix stops looking for a flake at the repository root.
        std::fs::create_dir(fixture.root.path().join("repo")).unwrap();
        std::fs::create_dir(fixture.root.path().join("repo/.git")).unwrap();
        fixture.write("env.sh", "export RHO_CACHE_TEST_VALUE=flake\n");
        let command = r#"printf '%s %s' "${RHO_CACHE_TEST_VALUE-unset}" "$RHO_CACHE_TEST_BASE""#;
        assert!(fixture.run(command, None).await.ends_with("unset base"));
        fixture.write("flake.nix", "");
        assert!(fixture.run(command, None).await.ends_with("flake base"));
        assert!(fixture.run(command, Some("repo")).await.ends_with("unset base"));
    }

    #[tokio::test]
    async fn optional_input_atomic_replace_delete_and_nearer_flake_invalidate() {
        let fixture = Fixture::new();
        fixture.write(
            "env.sh",
            "export RHO_CACHE_TEST_VALUE=parent\n[ -e \"$(dirname \"$BASH_SOURCE\")/extra\" ] && . \"$(dirname \"$BASH_SOURCE\")/extra\"\n",
        );
        std::fs::create_dir(fixture.root.path().join("child")).unwrap();
        let command = r#"printf '%s' "$RHO_CACHE_TEST_VALUE""#;
        assert!(
            fixture
                .run(command, Some("child"))
                .await
                .ends_with("parent")
        );
        fixture.write("extra", "export RHO_CACHE_TEST_VALUE=created\n");
        assert!(
            fixture
                .run(command, Some("child"))
                .await
                .ends_with("created")
        );
        fixture.write("replacement", "export RHO_CACHE_TEST_VALUE=renamed\n");
        std::fs::rename(
            fixture.root.path().join("replacement"),
            fixture.root.path().join("extra"),
        )
        .unwrap();
        assert!(
            fixture
                .run(command, Some("child"))
                .await
                .ends_with("renamed")
        );
        std::fs::remove_file(fixture.root.path().join("extra")).unwrap();
        assert!(
            fixture
                .run(command, Some("child"))
                .await
                .ends_with("parent")
        );
        fixture.write("child/env.sh", "export RHO_CACHE_TEST_VALUE=nearer\n");
        fixture.write("child/flake.nix", "");
        assert!(
            fixture
                .run(command, Some("child"))
                .await
                .ends_with("nearer")
        );
        assert!(fixture.run(command, None).await.ends_with("parent"));
    }

    #[tokio::test]
    async fn clones_with_different_overrides_do_not_share_environment_values() {
        let fixture = Fixture::new();
        fixture.write(
            "env.sh",
            "export RHO_CACHE_TEST_VALUE=\"$RHO_CACHE_TEST_BASE\"\n",
        );
        let other = fixture
            .tools
            .clone()
            .with_env("RHO_CACHE_TEST_BASE", "other");
        let command = r#"printf '%s' "$RHO_CACHE_TEST_VALUE""#;
        assert!(fixture.run(command, None).await.ends_with("base"));
        let mut process = other.spawn(command, None).await.unwrap();
        let mut text = Vec::new();
        loop {
            match process.next().await {
                crate::ProcessEvent::Output(bytes) => text.extend(bytes),
                crate::ProcessEvent::Closed => break,
                _ => {}
            }
        }
        assert!(String::from_utf8(text).unwrap().ends_with("other"));
        assert_eq!(fixture.run(command, None).await, "base");
    }

    #[tokio::test]
    async fn resolver_failure_does_not_serve_the_previous_generation() {
        let fixture = Fixture::new();
        fixture.write("env.sh", "export RHO_CACHE_TEST_VALUE=old\n");
        fixture.run("true", None).await;
        fixture.write("env.sh", "echo expected-failure >&2\nexit 1\n");
        let error = fixture
            .tools
            .spawn("touch must-not-run", None)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("expected-failure"), "{error:#}");
        assert!(!fixture.root.path().join("must-not-run").exists());
    }

    #[tokio::test]
    async fn shell_hook_output_and_status_do_not_break_activation() {
        let fixture = Fixture::new();
        fixture.write("env.sh", "echo noise\nexport RHO_CACHE_TEST_VALUE=ok\nfalse\n");
        let output = fixture.run(r#"printf '%s' "$RHO_CACHE_TEST_VALUE""#, None).await;
        // The hook's output is a diagnostic, not the environment.
        assert!(output.contains("noise") && output.ends_with("ok"), "{output}");
    }

    #[tokio::test]
    async fn symlinked_cwd_observes_new_physical_ancestor_flake() {
        let fixture = Fixture::new();
        fixture.write("env.sh", "export RHO_CACHE_TEST_VALUE=root\n");
        std::fs::create_dir_all(fixture.root.path().join("actual/sub")).unwrap();
        std::os::unix::fs::symlink("actual/sub", fixture.root.path().join("link")).unwrap();
        let command = r#"printf '%s' "$RHO_CACHE_TEST_VALUE""#;
        assert!(fixture.run(command, Some("link")).await.ends_with("root"));
        fixture.write("actual/env.sh", "export RHO_CACHE_TEST_VALUE=nearer\n");
        fixture.write("actual/flake.nix", "");
        assert!(fixture.run(command, Some("link")).await.ends_with("nearer"));
    }

    #[tokio::test]
    async fn resolver_does_not_inherit_host_descriptors() {
        use std::os::fd::AsRawFd;
        let root = tempfile::tempdir().unwrap();
        let sentinel = root.path().join("sentinel");
        let file = std::fs::File::create(&sentinel).unwrap();
        let descriptor = rustix::io::fcntl_dupfd_cloexec(&file, 100).unwrap();
        rustix::io::fcntl_setfd(&descriptor, rustix::io::FdFlags::empty()).unwrap();
        let mut command = tokio::process::Command::new("bash");
        command
            .args([
                "-c",
                &format!(
                    "test \"$(readlink /proc/self/fd/{})\" != \"$SENTINEL\"",
                    descriptor.as_raw_fd()
                ),
            ])
            .env("SENTINEL", &sentinel);
        output(command, &[], "test").await.unwrap();
    }

    #[tokio::test]
    async fn pending_callers_are_bounded_and_cancelled_loads_release_permits() {
        let fixture = Fixture::new();
        fixture.write("env.sh", "touch started\nsleep 30\n");
        let mut callers = tokio::task::JoinSet::new();
        for _ in 0..100 {
            let tools = fixture.tools.clone();
            callers.spawn(async move { tools.spawn("touch must-not-run", None).await });
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            while !fixture.root.path().join("started").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            while fixture.tools.environments.permits.available_permits() != 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        callers.abort_all();
        while callers.join_next().await.is_some() {}
        tokio::time::timeout(Duration::from_secs(3), async {
            while fixture.tools.environments.permits.available_permits() != 64 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(!fixture.root.path().join("must-not-run").exists());
        fixture.write("env.sh", "export RHO_CACHE_TEST_VALUE=replacement\n");
        assert!(
            fixture
                .run(r#"printf '%s' "$RHO_CACHE_TEST_VALUE""#, None)
                .await
                .ends_with("replacement")
        );
    }

    #[tokio::test]
    async fn slow_resolution_does_not_block_another_warm_cwd() {
        let fixture = Fixture::new();
        fixture.write("env.sh", "export RHO_CACHE_TEST_VALUE=warm\n");
        fixture.run("true", None).await;
        std::fs::create_dir(fixture.root.path().join("slow")).unwrap();
        fixture.write("slow/env.sh", "touch started\nsleep 30\n");
        fixture.write("slow/flake.nix", "");
        let tools = fixture.tools.clone();
        let pending = tokio::spawn(async move { tools.spawn("true", Some("slow")).await });
        tokio::time::timeout(Duration::from_secs(3), async {
            while !fixture.root.path().join("slow/started").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(1),
                fixture.run(r#"printf '%s' "$RHO_CACHE_TEST_VALUE""#, None)
            )
            .await
            .unwrap(),
            "warm"
        );
        pending.abort();
        let _ = pending.await;
    }

    #[tokio::test]
    async fn bounded_resolver_output_fails_without_waiting_for_exit() {
        let mut command = tokio::process::Command::new("bash");
        command.args(["-c", "while :; do printf '%010000d' 0 >&2; done"]);
        let error = tokio::time::timeout(Duration::from_secs(3), output(command, &[], "test"))
            .await
            .expect("oversized stderr must not hang")
            .err()
            .unwrap();
        assert!(error.to_string().contains("exceeds"), "{error:#}");
    }
}

#[cfg(test)]
mod latency {
    use rho_fs_view::PathOverrides;

    use super::*;
    use crate::{ProcessEvent, ShellTools};

    #[tokio::test]
    #[ignore = "manual release-mode end-to-end launch benchmark"]
    async fn warm_admission() {
        let cwd = std::env::current_dir().unwrap();
        let tools = ShellTools::in_directory(
            Duration::from_secs(30),
            camino::Utf8PathBuf::try_from(cwd.clone()).unwrap(),
            PathOverrides::default(),
        );
        for source in [
            "true",
            "printf hello",
            "rg --files src | sort",
            "git status --short",
        ] {
            let mut native = Vec::new();
            for _ in 0..51 {
                let start = std::time::Instant::now();
                let mut process = tools.spawn(source, None).await.unwrap();
                loop {
                    match process.next().await {
                        ProcessEvent::Output(_) => {}
                        ProcessEvent::Exited(status) => assert!(status.success()),
                        ProcessEvent::Closed => break,
                        ProcessEvent::Failed(error) => panic!("{error}"),
                    }
                }
                native.push(start.elapsed().as_secs_f64() * 1e6);
            }
            native.sort_by(f64::total_cmp);
            eprintln!(
                "{source:?}: median_us={} p95_us={}",
                native[25], native[48]
            );
        }
    }

    #[tokio::test]
    #[ignore = "manual: needs rho-devshell-builder and a flake checkout in RHO_TEST_FLAKE"]
    async fn real_flake_admission() {
        let flake = std::env::var("RHO_TEST_FLAKE").unwrap();
        let tools = ShellTools::in_directory(
            Duration::from_secs(600),
            camino::Utf8PathBuf::from(flake.clone()),
            PathOverrides::default(),
        );
        let run = |source: &'static str| {
            let tools = tools.clone();
            async move {
                let start = std::time::Instant::now();
                let mut process = tools.spawn(source, None).await.unwrap();
                let mut text = Vec::new();
                loop {
                    match process.next().await {
                        ProcessEvent::Output(bytes) => text.extend(bytes),
                        ProcessEvent::Exited(status) => assert!(status.success()),
                        ProcessEvent::Closed => break,
                        ProcessEvent::Failed(error) => panic!("{error}"),
                    }
                }
                let text = String::from_utf8_lossy(&text).into_owned();
                eprintln!("{source:?} {:?}: {}", start.elapsed(), text.lines().last().unwrap_or(""));
                text
            }
        };
        assert!(run("command -v cargo").await.trim_end().ends_with("/bin/cargo"));
        run("command -v cargo").await;
        // Git rewrites its index after noticing the new mtime; the shell is
        // unchanged, so the environment is kept after one builder check.
        run("touch flake.nix && git status --short >/dev/null").await;
        run("command -v cargo").await;
        run("command -v cargo").await;
        std::fs::write(Path::new(&flake).join("untracked-probe"), "").unwrap();
        run("command -v cargo").await;
        std::fs::remove_file(Path::new(&flake).join("untracked-probe")).unwrap();
    }
}
