//! Per-agent environment generations. Only cold/invalidated generations run
//! direnv; command admission drains kernel watches before reusing a snapshot.
//! Paths are absolute; resolver tasks inherit the workset process namespace.
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::io::Read;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use base64::Engine;
use rustix::fs::inotify::{self, ReadFlags, WatchFlags};
use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, oneshot};

pub(super) type Environment = BTreeMap<OsString, OsString>;
const MAX_ENV_BYTES: usize = 4 * 1024 * 1024;
const MAX_INPUTS: usize = 4096;
const CACHE_SIZE: usize = 32;
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub(super) struct Worker {
    sender: tokio::sync::OnceCell<mpsc::Sender<Request>>,
    permits: Arc<tokio::sync::Semaphore>,
}

impl Default for Worker {
    fn default() -> Self {
        Self {
            sender: tokio::sync::OnceCell::new(),
            permits: Arc::new(tokio::sync::Semaphore::new(64)),
        }
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
    watches: Watches,
    inputs: HashSet<PathBuf>,
}

impl Worker {
    pub async fn resolve(&self, cwd: PathBuf, base: Environment) -> Result<Resolved> {
        let permit = Arc::clone(&self.permits).acquire_owned().await?;
        let sender = self
            .sender
            .get_or_try_init(|| async {
                let (sender, receiver) = mpsc::channel(64);
                tokio::spawn(serve(receiver));
                Ok::<_, anyhow::Error>(sender)
            })
            .await?;
        let (reply, result) = oneshot::channel();
        sender
            .send(Request {
                key: Key {
                    cwd,
                    base: Arc::new(base),
                },
                reply,
                _permit: permit,
            })
            .await
            .context("environment worker stopped")?;
        result.await.context("environment worker stopped")?
    }
}

async fn serve(mut requests: mpsc::Receiver<Request>) {
    let mut snapshots: VecDeque<(Key, Snapshot)> = VecDeque::new();
    let mut loading = HashMap::<Key, (tokio::task::AbortHandle, Vec<Request>)>::new();
    let mut tasks = tokio::task::JoinSet::new();
    let mut cancellation = tokio::time::interval(Duration::from_millis(20));
    loop {
        tokio::select! {
            request = requests.recv(), if loading.len() < CACHE_SIZE => {
                let Some(request) = request else { break };
                if request.reply.is_closed() { continue; }
                let mut inputs = HashSet::new();
                if let Some(index) = snapshots.iter().position(|(key, _)| key == &request.key) {
                    let (key, mut snapshot) = snapshots.remove(index).unwrap();
                    if matches!(snapshot.watches.changed(), Ok(false)) {
                        let _ = request.reply.send(Ok(Resolved {
                            environment: Arc::clone(&snapshot.environment),
                            diagnostics: Vec::new(),
                        }));
                        snapshots.push_front((key, snapshot));
                        continue;
                    }
                    inputs = snapshot.inputs;
                }
                if let Some((_, waiters)) = loading.get_mut(&request.key) {
                    waiters.push(request);
                } else {
                    let key = request.key.clone();
                    let task = tasks.spawn(async move {
                        let result = tokio::time::timeout(RESOLVE_TIMEOUT, resolve(&key, inputs)).await
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
            _ = cancellation.tick(), if !loading.is_empty() => {
                loading.retain(|_, (task, waiters)| {
                    waiters.retain(|waiter| !waiter.reply.is_closed());
                    if waiters.is_empty() { task.abort(); false } else { true }
                });
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
) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(bytes.len() <= limit, "direnv output exceeds {limit} bytes");
    Ok(bytes)
}

async fn output(mut command: tokio::process::Command) -> Result<(Vec<u8>, Vec<u8>)> {
    command
        .kill_on_drop(true)
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    rho_fs_view::command_stdio_only(&mut command);
    let mut child = command
        .spawn()
        .context("start direnv environment resolver")?;
    let _group = ResolverGroup(rustix::process::Pid::from_raw(child.id().unwrap() as i32).unwrap());
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (status, environment, diagnostics) = tokio::try_join!(
        async { child.wait().await.map_err(anyhow::Error::from) },
        bounded_output(stdout, MAX_ENV_BYTES),
        bounded_output(stderr, 64 * 1024),
    )?;
    ensure!(
        status.success(),
        "direnv failed: {}",
        String::from_utf8_lossy(&diagnostics)
    );
    Ok((environment, diagnostics))
}

async fn evaluate(key: &Key) -> Result<(Environment, Vec<u8>)> {
    let mut command = tokio::process::Command::new("direnv");
    // direnv routes envrc output to stderr; only `env -0` owns stdout.
    command
        .args(["exec", ".", "env", "-0"])
        .current_dir(&key.cwd)
        .env_clear()
        .envs(key.base.iter());
    let (bytes, diagnostics) = output(command).await?;
    let mut environment = Environment::new();
    for item in bytes.split(|b| *b == 0).filter(|item| !item.is_empty()) {
        let separator = item
            .iter()
            .position(|b| *b == b'=')
            .ok_or_else(|| anyhow!("invalid direnv environment entry"))?;
        ensure!(separator > 0, "empty direnv environment key");
        environment.insert(
            OsString::from_vec(item[..separator].to_vec()),
            OsString::from_vec(item[separator + 1..].to_vec()),
        );
    }
    Ok((environment, diagnostics))
}

#[derive(serde::Deserialize)]
struct Input {
    path: PathBuf,
    modtime: i64,
    exists: bool,
}

fn declared_inputs(environment: &Environment) -> Result<Vec<Input>> {
    let Some(encoded) = environment.get(std::ffi::OsStr::new("DIRENV_WATCHES")) else {
        return Ok(Vec::new());
    };
    let bytes = base64::engine::general_purpose::URL_SAFE.decode(encoded.as_bytes())?;
    let mut bytes_out = Vec::new();
    flate2::read::ZlibDecoder::new(bytes.as_slice())
        .take((MAX_ENV_BYTES + 1) as u64)
        .read_to_end(&mut bytes_out)?;
    ensure!(
        bytes_out.len() <= MAX_ENV_BYTES,
        "direnv watch list exceeds 4 MiB"
    );
    let inputs: Vec<Input> = serde_json::from_slice(&bytes_out)?;
    ensure!(inputs.len() <= MAX_INPUTS, "too many environment inputs");
    ensure!(
        inputs.iter().all(|input| input.path.is_absolute()),
        "relative direnv watch path"
    );
    Ok(inputs)
}

fn inputs(key: &Key, environment: &Environment) -> Result<HashSet<PathBuf>> {
    let mut paths: HashSet<_> = declared_inputs(environment)?
        .into_iter()
        .map(|input| input.path)
        .collect();
    // Discovery is itself an input: a nearer envrc must invalidate a cached
    // parent environment, including the no-envrc case.
    let physical_cwd = key.cwd.canonicalize().context("resolve environment cwd")?;
    for parent in key.cwd.ancestors().chain(physical_cwd.ancestors()) {
        paths.insert(parent.join(".envrc"));
        paths.insert(parent.join(".env"));
    }
    if let Some(home) = key.base.get(std::ffi::OsStr::new("HOME")) {
        paths.insert(Path::new(home).join(".direnvrc"));
        let config = key
            .base
            .get(std::ffi::OsStr::new("DIRENV_CONFIG"))
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                key.base
                    .get(std::ffi::OsStr::new("XDG_CONFIG_HOME"))
                    .map(PathBuf::from)
                    .unwrap_or_else(|| Path::new(home).join(".config"))
                    .join("direnv")
            });
        paths.insert(config.join("lib"));
        paths.insert(config.join("direnvrc"));
        paths.insert(config.join("direnv.toml"));
    }
    ensure!(paths.len() <= MAX_INPUTS, "too many environment inputs");
    Ok(paths)
}

async fn resolve(key: &Key, mut paths: HashSet<PathBuf>) -> Result<(Snapshot, Vec<u8>)> {
    paths.extend(inputs(key, &Environment::new())?);
    for _ in 0..3 {
        let mut known = Watches::new(&paths)?;
        let before = states(&paths)?;
        let (environment, diagnostics) = evaluate(key).await?;
        paths.extend(inputs(key, &environment)?);
        let mut watches = Watches::new(&paths)?;
        // Newly discovered watch_file inputs carry direnv's own observation
        // (existence and second-resolution mtime). Preserve that contract:
        // arbitrary envrc code is not a transactional filesystem reader.
        let declared_valid = declared_inputs(&environment)?.iter().all(|input| {
            let metadata = [
                std::fs::symlink_metadata(&input.path),
                std::fs::metadata(&input.path),
            ]
            .into_iter()
            .filter_map(Result::ok)
            .filter_map(|m| m.modified().ok())
            .max();
            match metadata {
                Some(time) => {
                    input.exists
                        && time
                            .duration_since(std::time::UNIX_EPOCH)
                            .is_ok_and(|time| time.as_secs() as i64 == input.modtime)
                }
                None => !input.exists,
            }
        });
        let known_changed = known.changed()?;
        let known_valid = !known_changed
            || (!known.content_changed
                && before.iter().all(|(path, state)| {
                    states(&HashSet::from([path.clone()]))
                        .is_ok_and(|current| current.get(path) == Some(state))
                }));
        if declared_valid && known_valid && !watches.changed()? {
            return Ok((
                Snapshot {
                    environment: Arc::new(environment),
                    watches,
                    inputs: paths,
                },
                diagnostics,
            ));
        }
    }
    bail!("environment inputs kept changing during resolution; retry the command")
}

#[derive(PartialEq, Eq)]
enum State {
    Missing,
    File {
        identity: (u64, u64, u32),
    },
    Other {
        identity: (u64, u64, u32),
        modified: std::time::SystemTime,
    },
}

fn states(paths: &HashSet<PathBuf>) -> Result<BTreeMap<PathBuf, State>> {
    use std::os::unix::fs::MetadataExt;
    paths
        .iter()
        .map(|path| {
            let state = match std::fs::metadata(path) {
                Ok(metadata) => {
                    let identity = (metadata.dev(), metadata.ino(), metadata.mode());
                    if metadata.is_file() {
                        State::File { identity }
                    } else {
                        State::Other {
                            identity,
                            modified: metadata.modified()?,
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => State::Missing,
                Err(error) => return Err(error.into()),
            };
            Ok((path.clone(), state))
        })
        .collect()
}

struct Watches {
    fd: OwnedFd,
    names: HashMap<i32, HashSet<OsString>>,
    directories: HashSet<i32>,
    files: HashSet<i32>,
    content_changed: bool,
}

impl Watches {
    fn new(paths: &HashSet<PathBuf>) -> Result<Self> {
        let mut watches = Self {
            fd: inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)?,
            names: HashMap::new(),
            directories: HashSet::new(),
            files: HashSet::new(),
            content_changed: false,
        };
        let mut seen = HashSet::new();
        for path in paths {
            watches.path(path, &mut seen, 0)?;
        }
        Ok(watches)
    }

    fn path(&mut self, path: &Path, seen: &mut HashSet<PathBuf>, depth: usize) -> Result<()> {
        ensure!(depth < 64, "environment input symlink chain is too deep");
        if !seen.insert(path.to_owned()) {
            return Ok(());
        }
        ensure!(seen.len() <= MAX_INPUTS, "too many environment watch paths");
        let mut parent = PathBuf::from("/");
        for component in path.components().skip(1) {
            let name = component.as_os_str();
            match self.add(&parent) {
                Ok(wd) => {
                    self.names.entry(wd).or_default().insert(name.to_owned());
                }
                Err(error)
                    if error == rustix::io::Errno::NOENT || error == rustix::io::Errno::NOTDIR =>
                {
                    break;
                }
                Err(error) => return Err(error.into()),
            }
            parent.push(name);
            if let Ok(target) = std::fs::read_link(&parent) {
                let target = if target.is_absolute() {
                    target
                } else {
                    parent.parent().unwrap().join(target)
                };
                self.path(&target, seen, depth + 1)?;
            }
        }
        if path.is_dir() {
            let wd = self.add(path)?;
            self.directories.insert(wd);
        } else if path.is_file() {
            // Parent watches observe replacement; inode watches additionally
            // observe writes through hard-link aliases in other directories.
            let wd = self.add(path)?;
            self.files.insert(wd);
        }
        Ok(())
    }

    fn add(&self, path: &Path) -> rustix::io::Result<i32> {
        inotify::add_watch(
            &self.fd,
            path,
            WatchFlags::MODIFY
                | WatchFlags::ATTRIB
                | WatchFlags::CLOSE_WRITE
                | WatchFlags::CREATE
                | WatchFlags::DELETE
                | WatchFlags::MOVED_FROM
                | WatchFlags::MOVED_TO
                | WatchFlags::DELETE_SELF
                | WatchFlags::MOVE_SELF,
        )
    }

    fn changed(&mut self) -> Result<bool> {
        let mut buffer = [MaybeUninit::uninit(); 4096];
        let mut reader = inotify::Reader::new(&self.fd, &mut buffer);
        let mut changed = false;
        loop {
            match reader.next() {
                Err(error) if error == rustix::io::Errno::AGAIN => return Ok(changed),
                Err(error) => return Err(error.into()),
                Ok(event) => {
                    let flags = event.events();
                    if flags.intersects(
                        ReadFlags::QUEUE_OVERFLOW
                            | ReadFlags::IGNORED
                            | ReadFlags::UNMOUNT
                            | ReadFlags::DELETE_SELF
                            | ReadFlags::MOVE_SELF,
                    ) {
                        self.content_changed = true;
                        changed = true;
                    } else if self.files.contains(&event.wd())
                        || self.directories.contains(&event.wd())
                        || event.file_name().is_some_and(|name| {
                            self.names.get(&event.wd()).is_some_and(|names| {
                                names.contains(std::ffi::OsStr::from_bytes(name.to_bytes()))
                            })
                        })
                    {
                        changed = true;
                        self.content_changed |= flags.intersects(
                            ReadFlags::MODIFY
                                | ReadFlags::CLOSE_WRITE
                                | ReadFlags::CREATE
                                | ReadFlags::DELETE
                                | ReadFlags::MOVED_FROM
                                | ReadFlags::MOVED_TO,
                        );
                    }
                }
            }
        }
    }
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

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let config = root.path().join("config");
            std::fs::create_dir(&config).unwrap();
            std::fs::write(config.join("direnvrc"), "").unwrap();
            std::fs::write(
                config.join("direnv.toml"),
                format!(
                    "[whitelist]\nprefix = [{}]\n",
                    serde_json::to_string(root.path()).unwrap()
                ),
            )
            .unwrap();
            let tools = ShellTools::in_directory(
                Duration::from_secs(5),
                camino::Utf8PathBuf::from_path_buf(root.path().to_owned()).unwrap(),
                PathOverrides::default(),
            )
            .with_env("DIRENV_CONFIG", config.to_str().unwrap())
            .with_env("RHO_CACHE_TEST_BASE", "base");
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
            ".envrc",
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
            ".envrc",
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
    async fn optional_input_atomic_replace_delete_and_nearer_envrc_invalidate() {
        let fixture = Fixture::new();
        fixture.write(
            ".envrc",
            "export RHO_CACHE_TEST_VALUE=parent\nsource_env_if_exists extra\n",
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
        fixture.write("child/.envrc", "export RHO_CACHE_TEST_VALUE=nearer\n");
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
            ".envrc",
            "export RHO_CACHE_TEST_VALUE=\"$RHO_CACHE_TEST_BASE\"\n",
        );
        let other = Fixture {
            root: tempfile::tempdir().unwrap(),
            tools: fixture
                .tools
                .clone()
                .with_env("RHO_CACHE_TEST_BASE", "other"),
        };
        let command = r#"printf '%s' "$RHO_CACHE_TEST_VALUE""#;
        assert!(fixture.run(command, None).await.ends_with("base"));
        assert!(other.run(command, None).await.ends_with("other"));
        assert_eq!(fixture.run(command, None).await, "base");
    }

    #[tokio::test]
    async fn resolver_failure_does_not_serve_the_previous_generation() {
        let fixture = Fixture::new();
        fixture.write(".envrc", "export RHO_CACHE_TEST_VALUE=old\n");
        fixture.run("true", None).await;
        fixture.write(".envrc", "echo expected-failure >&2\nexit 1\n");
        let error = fixture
            .tools
            .spawn("touch must-not-run", None)
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("expected-failure"), "{error:#}");
        assert!(!fixture.root.path().join("must-not-run").exists());
    }

    #[test]
    fn watches_symlink_targets_retargeting_and_ancestor_replacement() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("dir")).unwrap();
        std::fs::write(root.path().join("dir/value"), "one").unwrap();
        symlink("dir/value", root.path().join("link")).unwrap();
        let paths = HashSet::from([root.path().join("link")]);
        let mut watches = Watches::new(&paths).unwrap();
        assert!(!watches.changed().unwrap());
        std::fs::write(root.path().join("dir/value"), "two").unwrap();
        assert!(watches.changed().unwrap());
        let mut watches = Watches::new(&paths).unwrap();
        std::fs::rename(root.path().join("dir"), root.path().join("old")).unwrap();
        assert!(watches.changed().unwrap());
        let mut watches = Watches::new(&paths).unwrap();
        std::fs::remove_file(root.path().join("link")).unwrap();
        symlink("old/value", root.path().join("link")).unwrap();
        assert!(watches.changed().unwrap());
    }

    #[test]
    fn writes_through_hard_links_invalidate() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("other")).unwrap();
        let input = root.path().join("input");
        let alias = root.path().join("other/alias");
        std::fs::write(&input, "one").unwrap();
        std::fs::hard_link(&input, &alias).unwrap();
        let mut watches = Watches::new(&HashSet::from([input])).unwrap();
        assert!(!watches.changed().unwrap());
        std::fs::write(alias, "two").unwrap();
        assert!(watches.changed().unwrap());
    }

    #[tokio::test]
    async fn symlinked_cwd_observes_new_physical_ancestor_envrc() {
        let fixture = Fixture::new();
        fixture.write(".envrc", "export RHO_CACHE_TEST_VALUE=root\n");
        std::fs::create_dir_all(fixture.root.path().join("actual/sub")).unwrap();
        std::os::unix::fs::symlink("actual/sub", fixture.root.path().join("link")).unwrap();
        let command = r#"printf '%s' "$RHO_CACHE_TEST_VALUE""#;
        assert!(fixture.run(command, Some("link")).await.ends_with("root"));
        fixture.write("actual/.envrc", "export RHO_CACHE_TEST_VALUE=nearer\n");
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
        output(command).await.unwrap();
    }

    #[tokio::test]
    async fn pending_callers_are_bounded_and_cancelled_loads_release_permits() {
        let fixture = Fixture::new();
        fixture.write(".envrc", "touch started\nsleep 30\n");
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
        fixture.write(".envrc", "export RHO_CACHE_TEST_VALUE=replacement\n");
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
        fixture.write(".envrc", "export RHO_CACHE_TEST_VALUE=warm\n");
        fixture.run("true", None).await;
        std::fs::create_dir(fixture.root.path().join("slow")).unwrap();
        fixture.write("slow/.envrc", "touch started\nsleep 30\n");
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
        let error = tokio::time::timeout(Duration::from_secs(3), output(command))
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
            let mut exec = Vec::new();
            // Separate phases: nix-direnv touches watched cache metadata on
            // every baseline exec, which would invalidate the warm generation.
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
            for _ in 0..51 {
                let start = std::time::Instant::now();
                let mut command = tokio::process::Command::new("direnv");
                command
                    .args(["exec", ".", "bash", "-o", "pipefail", "-c", source])
                    .current_dir(&cwd);
                for name in ["DIRENV_DIFF", "DIRENV_DIR", "DIRENV_FILE", "DIRENV_WATCHES"] {
                    command.env_remove(name);
                }
                let result = command.output().await.unwrap();
                exec.push(start.elapsed().as_secs_f64() * 1e6);
                assert!(result.status.success());
            }
            native.sort_by(f64::total_cmp);
            exec.sort_by(f64::total_cmp);
            eprintln!(
                "{source:?}: native_median_us={} exec_median_us={} native_p95_us={} exec_p95_us={}",
                native[25], exec[25], native[48], exec[48]
            );
        }
    }
}
