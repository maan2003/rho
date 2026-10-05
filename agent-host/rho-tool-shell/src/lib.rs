//! Runs agent shell commands through rho-bash, in the devshell environment
//! of the directory they start in.

mod environment;
mod fork_server;

use std::io;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use rho_fs_view::PathOverrides;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};
use tokio::time;

const MAX_OUTPUT_TOKENS: usize = 10_000;
const APPROX_BYTES_PER_TOKEN: u64 = 4;

#[derive(Debug)]
struct BashEnvironment {
    environment: Arc<environment::Environment>,
    server: Arc<fork_server::Server>,
}

#[derive(Clone, Debug)]
pub struct ShellTools {
    working_directory: Utf8PathBuf,
    path_overrides: PathOverrides,
    env: Vec<(String, String)>,
    environments: Arc<environment::Worker>,
    executor: Arc<Mutex<Option<BashEnvironment>>>,
}

#[derive(Debug)]
struct ProcessSession {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    output_tasks: Vec<tokio::task::JoinHandle<io::Result<()>>>,
    stdin: Option<tokio::net::unix::pipe::Sender>,
    wait_task: tokio::task::JoinHandle<io::Result<std::process::ExitStatus>>,
    status: Option<std::process::ExitStatus>,
    output_rx: mpsc::UnboundedReceiver<Vec<u8>>,
}

/// A command started by [`ShellTools::spawn`], for a caller that streams its
/// output itself instead of collecting it inside one tool call.
///
/// Dropping it kills a live command.
#[derive(Debug)]
pub struct SpawnedProcess {
    session: ProcessSession,
    /// After exit, how long to keep reading for output that is still in the
    /// pipes before calling the process closed. A grandchild that inherited
    /// the pipes could otherwise hold the reader open forever.
    drain_deadline: Option<time::Instant>,
}

/// One thing a [`SpawnedProcess`] did.
#[derive(Debug)]
pub enum ProcessEvent {
    /// Some stdout or stderr, in read order.
    Output(Vec<u8>),
    /// The process exited. Output may still follow, then `Closed`.
    Exited(std::process::ExitStatus),
    /// Nothing more will come.
    Closed,
    /// The process could not be waited on; treat as ended.
    Failed(String),
}

const POST_EXIT_DRAIN: Duration = Duration::from_millis(200);

impl SpawnedProcess {
    /// Stop and reap this owned child while the executor is still alive.
    /// This does not contain or terminate arbitrary descendants.
    pub async fn terminate(&mut self) -> io::Result<()> {
        if let Some(stop) = self.session.stop.take() {
            let _ = stop.send(());
        }
        let result = if self.session.status.is_none() {
            match (&mut self.session.wait_task).await {
                Ok(Ok(status)) => {
                    self.session.status = Some(status);
                    Ok(())
                }
                Ok(Err(error)) => Err(error),
                Err(error) => Err(io::Error::other(error)),
            }
        } else {
            Ok(())
        };
        for task in &self.session.output_tasks {
            task.abort();
        }
        for task in self.session.output_tasks.drain(..) {
            let _ = task.await;
        }
        result
    }

    /// The next thing that happens. After `Closed` or `Failed`, keeps
    /// returning `Closed`.
    pub async fn next(&mut self) -> ProcessEvent {
        let session = &mut self.session;
        if session.status.is_some() {
            let deadline = self
                .drain_deadline
                .get_or_insert_with(|| time::Instant::now() + POST_EXIT_DRAIN);
            return tokio::select! {
                biased;
                chunk = session.output_rx.recv() => match chunk {
                    Some(chunk) => ProcessEvent::Output(chunk),
                    None => ProcessEvent::Closed,
                },
                _ = time::sleep_until(*deadline) => ProcessEvent::Closed,
            };
        }
        tokio::select! {
            biased;
            status = &mut session.wait_task => match status {
                Ok(Ok(status)) => {
                    session.status = Some(status);
                    ProcessEvent::Exited(status)
                }
                Ok(Err(error)) => {
                    session.status = Some(std::process::ExitStatus::default());
                    ProcessEvent::Failed(error.to_string())
                }
                Err(error) => {
                    session.status = Some(std::process::ExitStatus::default());
                    ProcessEvent::Failed(format!("shell wait task failed: {error}"))
                }
            },
            chunk = session.output_rx.recv() => match chunk {
                Some(chunk) => ProcessEvent::Output(chunk),
                // Both pipes closed before the process exited: only the exit
                // is left to wait for.
                None => match (&mut session.wait_task).await {
                    Ok(Ok(status)) => {
                        session.status = Some(status);
                        ProcessEvent::Exited(status)
                    }
                    Ok(Err(error)) => {
                        session.status = Some(std::process::ExitStatus::default());
                        ProcessEvent::Failed(error.to_string())
                    }
                    Err(error) => {
                        session.status = Some(std::process::ExitStatus::default());
                        ProcessEvent::Failed(format!("shell wait task failed: {error}"))
                    }
                },
            },
        }
    }

    /// The process's stdin, once; `None` if already taken.
    pub fn take_stdin(&mut self) -> Option<tokio::net::unix::pipe::Sender> {
        self.session.stdin.take()
    }
}

impl Drop for ProcessSession {
    fn drop(&mut self) {
        // Dropping the waiter's job guard queues cancellation; the native
        // supervisor kills the process group and reaps its leader.
        self.wait_task.abort();
        for task in &self.output_tasks {
            task.abort();
        }
    }
}

impl ShellTools {
    /// Tools running in a directory in the current process namespace.
    pub fn in_directory(working_directory: Utf8PathBuf, path_overrides: PathOverrides) -> Self {
        Self {
            working_directory,
            path_overrides,
            env: Vec::new(),
            environments: Arc::new(environment::Worker::default()),
            executor: Arc::new(Mutex::new(None)),
        }
    }

    pub fn with_env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((name.into(), value.into()));
        self
    }

    /// Initialize ordinary Python filesystem operations in this tool's workdir.
    ///
    /// # Safety
    /// Must run on a dedicated interpreter thread with CLONE_FS unshared.
    /// Poll this future on that same thread throughout.
    pub async unsafe fn enter_interpreter_thread(&self) -> Result<()> {
        rho_fs_view::layout::unshare_fs_attributes()?;
        std::env::set_current_dir(&self.working_directory)
            .with_context(|| format!("enter working directory {}", self.working_directory))
    }

    /// Start `cmd` with `bash -o pipefail -c` in `workdir`, resolved against
    /// the working directory. Without `stdin` the command
    /// reads `/dev/null`: an open pipe nobody writes to would hang tools that
    /// fall back to reading stdin, such as `rg` without a path.
    pub async fn spawn(
        &self,
        cmd: &str,
        workdir: Option<&str>,
        stdin: bool,
    ) -> Result<SpawnedProcess> {
        let session = self.spawn_process(cmd, workdir, stdin).await?;
        Ok(SpawnedProcess {
            session,
            drain_deadline: None,
        })
    }

    async fn spawn_process(
        &self,
        cmd: &str,
        workdir: Option<&str>,
        stdin: bool,
    ) -> Result<ProcessSession> {
        let mut command = Command::new(format!("{}/bin/rho-bash", rho_fs_view::AGENT_BASE));
        // An absolute model-supplied cwd wins; a relative one resolves
        // against the tool's working directory (join handles both).
        let cwd = workdir.map_or_else(
            || self.working_directory.clone(),
            |cwd| self.working_directory.join(Utf8Path::new(cwd)),
        );
        command.current_dir(cwd.as_std_path());

        let directory = command
            .as_std()
            .get_current_dir()
            .expect("configured cwd")
            .to_owned();
        let directory = if directory.is_absolute() {
            directory
        } else {
            std::env::current_dir()?.join(directory)
        };
        let mut base: environment::Environment = std::env::vars_os().collect();
        for (name, value) in &self.env {
            base.insert(name.into(), value.into());
        }
        let path = base
            .get(std::ffi::OsStr::new("PATH"))
            .expect("PATH must be set");
        base.insert("PATH".into(), self.path_overrides.add_to(path));
        let resolved = self.environments.resolve(directory.clone(), base).await?;
        let server = {
            let mut cached = self.executor.lock().await;
            if let Some(BashEnvironment {
                environment,
                server,
            }) = cached.as_ref()
                && environment == &resolved.environment
                && !server.closed()
            {
                server.clone()
            } else {
                // A reused supervisor already owns the resolved environment.
                command.env_clear().envs(resolved.environment.iter());
                let server = fork_server::Server::start(command)
                    .await
                    .context("start native Bash executor")?;
                *cached = Some(BashEnvironment {
                    environment: resolved.environment.clone(),
                    server: server.clone(),
                });
                server
            }
        };
        let (stdin, child_stdin) = if stdin {
            let (stdin, child_stdin) = tokio::net::unix::pipe::pipe()?;
            (Some(stdin), child_stdin.into_blocking_fd()?)
        } else {
            (None, std::fs::File::open("/dev/null")?.into())
        };
        let (child_stdout, stdout) = tokio::net::unix::pipe::pipe()?;
        let (child_stderr, stderr) = tokio::net::unix::pipe::pipe()?;
        let fds = [
            child_stdin,
            child_stdout.into_blocking_fd()?,
            child_stderr.into_blocking_fd()?,
        ];
        let mut job = server
            .spawn(cmd, &directory, &fds)
            .await
            .context("start Bash command")?;
        drop(fds);
        let (output_tx, output_rx) = mpsc::unbounded_channel();
        if !resolved.diagnostics.is_empty() {
            let _ = output_tx.send(resolved.diagnostics);
        }
        let stdout_task = tokio::spawn(read_output_chunks(stdout, output_tx.clone()));
        let stderr_task = tokio::spawn(read_output_chunks(stderr, output_tx));
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let wait_task = tokio::spawn(async move {
            tokio::select! {
                status = job.wait() => status,
                _ = stopped => { job.cancel(); job.wait().await }
            }
        });
        Ok(ProcessSession {
            stop: Some(stop),
            output_tasks: vec![stdout_task, stderr_task],
            stdin,
            wait_task,
            status: None,
            output_rx,
        })
    }
}

/// Lossy UTF-8, for callers that render process output themselves.
pub fn decode_output_lossy(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

/// Keeps the first and last halves of a byte budget and notes what fell out
/// of the middle, so a long log still shows how it started and how it ended.
pub struct BoundedOutput {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    head_limit: usize,
    tail_limit: usize,
    total_bytes: u64,
}

impl BoundedOutput {
    /// Bytes for `tokens` tokens at the shell tools' usual estimate, capped at
    /// their own ceiling.
    pub fn for_tokens(tokens: Option<usize>) -> Self {
        Self::new(
            tokens
                .unwrap_or(MAX_OUTPUT_TOKENS)
                .min(MAX_OUTPUT_TOKENS)
                .saturating_mul(APPROX_BYTES_PER_TOKEN as usize),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.total_bytes == 0
    }

    /// Whether the middle has been dropped, so the kept bytes are a sample of
    /// what was pushed rather than all of it.
    pub fn is_truncated(&self) -> bool {
        self.total_bytes as usize > self.head_limit + self.tail_limit
    }

    /// The kept bytes, with a marker where the middle was dropped.
    pub fn into_bytes(self) -> Vec<u8> {
        let dropped = self
            .total_bytes
            .saturating_sub((self.head_limit + self.tail_limit) as u64);
        let mut bytes = self.head;
        if dropped > 0 {
            bytes.extend_from_slice(
                format!(
                    "\n…{} tokens truncated…\n",
                    approx_tokens_from_byte_count(dropped)
                )
                .as_bytes(),
            );
        }
        bytes.extend(self.tail);
        bytes
    }

    fn new(limit: usize) -> Self {
        let head_limit = limit / 2;
        let tail_limit = limit - head_limit;
        Self {
            head: Vec::with_capacity(head_limit),
            tail: std::collections::VecDeque::with_capacity(tail_limit),
            head_limit,
            tail_limit,
            total_bytes: 0,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.total_bytes = self.total_bytes.saturating_add(chunk.len() as u64);

        let mut rest = chunk;
        let head_remaining = self.head_limit.saturating_sub(self.head.len());
        if head_remaining > 0 {
            let keep = head_remaining.min(rest.len());
            self.head.extend_from_slice(&rest[..keep]);
            rest = &rest[keep..];
        }
        for byte in rest {
            if self.tail.len() == self.tail_limit {
                self.tail.pop_front();
            }
            self.tail.push_back(*byte);
        }
    }
}

fn approx_tokens_from_byte_count(bytes: u64) -> u64 {
    bytes.saturating_add(APPROX_BYTES_PER_TOKEN.saturating_sub(1)) / APPROX_BYTES_PER_TOKEN
}

async fn read_output_chunks<R>(
    mut reader: R,
    output: mpsc::UnboundedSender<Vec<u8>>,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut buffer = [0_u8; 8192];
    loop {
        let len = reader.read(&mut buffer).await?;
        if len == 0 {
            return Ok(());
        }
        if output.send(buffer[..len].to_vec()).is_err() {
            return Ok(());
        }
    }
}

#[cfg(test)]
mod tests;
