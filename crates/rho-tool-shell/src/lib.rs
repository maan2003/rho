//! Concrete shell tool building block.
//!
//! The process outcome shape is adapted from Tau's `tau-ext-shell`: commands
//! that start successfully return structured results even when they exit
//! non-zero or time out. Only malformed arguments and start/configuration
//! failures are surfaced as tool errors.

mod apply_patch;
#[cfg(test)]
mod truncate;

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use camino::{Utf8Path, Utf8PathBuf};
use rho_core::{
    ApplyPatchMetadata, ToolCall, ToolFormat, ToolGrammarSyntax, ToolName, ToolOutput,
    ToolOutputStatus, ToolResultMetadata, ToolSpec, ToolType,
};
use rho_workspaces::{PathOverrides, View};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};
use tokio::time;

pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub const EXEC_COMMAND_TOOL_NAME: &str = "exec_command";
pub const WRITE_STDIN_TOOL_NAME: &str = "write_stdin";
pub const APPLY_PATCH_TOOL_NAME: &str = "apply_patch";
const MAX_OUTPUT_TOKENS: usize = 10_000;
const MAX_OUTPUT_BYTES: usize = MAX_OUTPUT_TOKENS * APPROX_BYTES_PER_TOKEN as usize;
const APPROX_BYTES_PER_TOKEN: u64 = 4;
static NEXT_CHUNK_ID: AtomicI32 = AtomicI32::new(1);
/// `exec_command` waits for the command to finish by default. A result that
/// says "still running" costs the model a full round trip to poll, while
/// waiting server-side costs nothing, so the default is the ceiling.
const DEFAULT_EXEC_YIELD_MS: u64 = 300_000;
const MIN_YIELD_MS: u64 = 250;
const MAX_YIELD_MS: u64 = 300_000;
/// An empty `write_stdin` is a poll: it blocks until the process prints more
/// or exits, so a model that polls anyway pays one trip per event rather than
/// one per second.
const MIN_POLL_YIELD_MS: u64 = 30_000;
const DEFAULT_POLL_YIELD_MS: u64 = 300_000;
/// A real stdin write yields soon, so interactive sessions stay responsive.
const DEFAULT_STDIN_WRITE_YIELD_MS: u64 = 10_000;
const MAX_STDIN_WRITE_YIELD_MS: u64 = 30_000;
/// After a poll sees output, gather the rest of the burst before returning.
const POLL_SETTLE: Duration = Duration::from_millis(200);

#[derive(Clone, Debug)]
pub struct ShellTools {
    exec: ExecContext,
    env: Vec<(String, String)>,
    processes: Arc<ProcessManager>,
}

/// Collects output until the process exits or `yield_time` passes. With
/// `until_output`, a poll returns as soon as the process has said something
/// (after a short settle so a burst arrives whole).
async fn collect_process_output(
    session: &mut ProcessSession,
    yield_time: Duration,
    until_output: bool,
) -> Result<(Option<std::process::ExitStatus>, Vec<u8>)> {
    let deadline = time::Instant::now() + yield_time.min(Duration::from_millis(MAX_YIELD_MS));
    let mut output = BoundedOutput::new(MAX_OUTPUT_BYTES);
    let mut settle: Option<time::Instant> = None;
    let status = loop {
        if let Some(status) = session.status {
            break Some(status);
        }
        let settle_at = settle.unwrap_or(deadline);
        tokio::select! {
            biased;
            status = &mut session.wait_task => {
                let status = status.map_err(|error| anyhow!("shell wait task failed: {error}"))??;
                session.status = Some(status);
                break Some(status);
            }
            chunk = session.output_rx.recv() => {
                if let Some(chunk) = chunk {
                    output.push(&chunk);
                    if until_output && settle.is_none() {
                        settle = Some(time::Instant::now() + POLL_SETTLE);
                    }
                }
            }
            _ = time::sleep_until(deadline) => break None,
            _ = time::sleep_until(settle_at), if settle.is_some() => break None,
            _ = time::sleep(Duration::from_millis(10)) => {}
        }
    };
    if status.is_some() {
        while let Some(chunk) = session.output_rx.recv().await {
            output.push(&chunk);
        }
    }
    Ok((status, output.finish().bytes))
}

struct ExecOutput {
    chunk_id: String,
    wall_time_seconds: f64,
    exit_code: Option<i32>,
    session_id: Option<i32>,
    output: String,
}

impl ExecOutput {
    fn render(&self) -> String {
        let mut sections = vec![
            format!("Chunk ID: {}", self.chunk_id),
            format!("Wall time: {:.4} seconds", self.wall_time_seconds),
        ];
        if let Some(exit_code) = self.exit_code {
            sections.push(format!("Process exited with code {exit_code}"));
        }
        if let Some(session_id) = self.session_id {
            sections.push(format!("Process running with session ID {session_id}"));
        }
        sections.push("Output:".to_owned());
        sections.push(self.output.clone());
        sections.join("\n")
    }

    fn json(self) -> Value {
        let mut value = serde_json::Map::from_iter([
            ("chunk_id".to_owned(), Value::String(self.chunk_id)),
            (
                "wall_time_seconds".to_owned(),
                json!(self.wall_time_seconds),
            ),
            ("output".to_owned(), Value::String(self.output)),
        ]);
        if let Some(exit_code) = self.exit_code {
            value.insert("exit_code".to_owned(), json!(exit_code));
        }
        if let Some(session_id) = self.session_id {
            value.insert("session_id".to_owned(), json!(session_id));
        }
        Value::Object(value)
    }
}

fn exec_output(
    wall_time: Duration,
    exit_code: Option<i32>,
    process_id: Option<i32>,
    bytes: Vec<u8>,
    max_output_tokens: Option<usize>,
) -> ExecOutput {
    let max_bytes = max_output_tokens
        .unwrap_or(MAX_OUTPUT_TOKENS)
        .min(MAX_OUTPUT_TOKENS)
        .saturating_mul(APPROX_BYTES_PER_TOKEN as usize);
    let mut output = BoundedOutput::new(max_bytes);
    output.push(&bytes);
    let output = output.finish();
    let (content, _) = decode_output(output.bytes);
    ExecOutput {
        chunk_id: format!("{:x}", NEXT_CHUNK_ID.fetch_add(1, Ordering::Relaxed)),
        wall_time_seconds: wall_time.as_secs_f64(),
        exit_code,
        session_id: process_id,
        output: content,
    }
}

#[derive(Clone, Debug)]
enum ExecContext {
    Directory {
        working_directory: Utf8PathBuf,
        path_overrides: PathOverrides,
    },
    View(Arc<View>),
}

#[derive(Clone, Debug)]
pub struct ShellToolOutput {
    pub body: ToolOutput,
    pub metadata: Option<ToolResultMetadata>,
}

#[derive(Debug, Deserialize)]
struct ShellArgs {
    #[serde(alias = "command")]
    cmd: String,
    #[serde(alias = "cwd")]
    workdir: Option<String>,
    yield_time_ms: Option<u64>,
    max_output_tokens: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct WriteStdinArgs {
    session_id: i32,
    #[serde(default)]
    chars: String,
    yield_time_ms: Option<u64>,
    max_output_tokens: Option<usize>,
}

#[derive(Debug)]
struct ProcessManager {
    next_id: AtomicI32,
    sessions: Mutex<std::collections::HashMap<i32, ProcessSession>>,
}

#[derive(Debug)]
struct ProcessSession {
    stdin: Option<tokio::process::ChildStdin>,
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
    pub fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.session.stdin.take()
    }
}

impl Drop for ProcessSession {
    fn drop(&mut self) {
        // Cancelling the waiter drops its `Child`; `kill_on_drop` terminates a
        // live command and Tokio's orphan queue takes responsibility for reap.
        self.wait_task.abort();
    }
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self {
            next_id: AtomicI32::new(1),
            sessions: Mutex::new(std::collections::HashMap::new()),
        }
    }
}

impl ShellTools {
    /// Tools for an agent's workspace view. Namespace setup and cache warming
    /// run lazily on the first shell command, hiding their latency behind the
    /// model's first response.
    pub fn new(_timeout: Duration, view: Arc<View>) -> Self {
        Self {
            exec: ExecContext::View(view),
            env: Vec::new(),
            processes: Arc::new(ProcessManager::default()),
        }
    }

    /// Tools running directly in a directory, without a workspace.
    pub fn in_directory(
        _timeout: Duration,
        working_directory: Utf8PathBuf,
        path_overrides: PathOverrides,
    ) -> Self {
        Self {
            exec: ExecContext::Directory {
                working_directory,
                path_overrides,
            },
            env: Vec::new(),
            processes: Arc::new(ProcessManager::default()),
        }
    }

    pub fn with_env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((name.into(), value.into()));
        self
    }

    /// Resolve a patch path to where in-process file operations apply: the
    /// real checkout files, never a namespace-relative path. Relative paths
    /// land in the primary workdir's checkout; absolute paths inside any
    /// workdir are translated to that workdir's checkout.
    fn resolve_patch_path(&self, path: &Path) -> Result<std::path::PathBuf, String> {
        match &self.exec {
            ExecContext::Directory {
                working_directory, ..
            } => Ok(if path.is_absolute() {
                path.to_owned()
            } else {
                working_directory.as_std_path().join(path)
            }),
            ExecContext::View(view) => view
                .resolve_host_path_checked(path)
                .map_err(|error| error.to_string()),
        }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        vec![
            self.exec_command_spec(),
            self.write_stdin_spec(),
            self.apply_patch_spec(),
        ]
    }

    pub fn exec_command_spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName::try_from(EXEC_COMMAND_TOOL_NAME).expect("valid tool name"),
            tool_type: ToolType::Function,
            description:
                "Runs a command, returning output or a session ID for ongoing interaction."
                    .to_owned(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["cmd"],
                "properties": {
                    "cmd": {
                        "type": "string",
                        "description": "Command to run with bash -o pipefail -c"
                    },
                    "workdir": {
                        "type": "string",
                        "description": "Optional working directory; defaults to the agent's working directory"
                    },
                    "yield_time_ms": {
                        "type": "integer",
                        "description": "How long to wait for the command to finish before yielding with a session ID. Defaults to 300000 ms, so the call normally returns the finished result; lower it only for an interactive command that needs stdin."
                    },
                    "max_output_tokens": {
                        "type": "integer",
                        "description": "Output token budget. Defaults to 10000 tokens."
                    }
                }
            }),
            format: None,
        }
    }

    pub fn write_stdin_spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName::try_from(WRITE_STDIN_TOOL_NAME).expect("valid tool name"),
            tool_type: ToolType::Function,
            description:
                "Writes characters to an existing unified exec session and returns recent output."
                    .to_owned(),
            input_schema: json!({
                "type": "object", "additionalProperties": false, "required": ["session_id"],
                "properties": {
                    "session_id": {"type": "integer", "description": "Identifier of the running unified exec session."},
                    "chars": {"type": "string", "description": "Bytes to write to stdin. Defaults to empty, which waits for the process to print more or exit instead of writing."},
                    "yield_time_ms": {"type": "integer", "description": "With empty chars: how long to wait for more output or exit, 30000 to 300000 ms; returns as soon as something arrives. After a write: how long to wait for a response, at most 30000 ms."},
                    "max_output_tokens": {"type": "integer", "description": "Output token budget. Defaults to 10000 tokens."}
                }
            }),
            format: None,
        }
    }

    pub fn apply_patch_spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName::try_from(APPLY_PATCH_TOOL_NAME).expect("valid tool name"),
            tool_type: ToolType::Custom,
            description: "Apply a Codex-style patch to local files.".to_owned(),
            input_schema: Value::Null,
            format: Some(ToolFormat::Grammar {
                syntax: ToolGrammarSyntax::Lark,
                definition: apply_patch::APPLY_PATCH_LARK_GRAMMAR.to_owned(),
            }),
        }
    }

    pub fn supports(&self, name: &str) -> bool {
        matches!(
            name,
            EXEC_COMMAND_TOOL_NAME | WRITE_STDIN_TOOL_NAME | APPLY_PATCH_TOOL_NAME
        )
    }

    pub fn preview_metadata(&self, call: &ToolCall) -> Option<ToolResultMetadata> {
        match call.name.as_str() {
            APPLY_PATCH_TOOL_NAME if call.tool_type == ToolType::Custom => {
                apply_patch::preview_metadata(&call.arguments)
                    .ok()
                    .map(ToolResultMetadata::ApplyPatch)
            }
            _ => None,
        }
    }

    pub async fn call(&self, call: ToolCall) -> ToolOutput {
        self.call_with_metadata(call).await.body
    }

    /// Structured result used by JavaScript code mode. Direct tool calls use
    /// the Codex-style rendered text from `call_with_metadata` instead.
    pub async fn call_code_mode(&self, call: ToolCall) -> Result<Value> {
        match call.name.as_str() {
            EXEC_COMMAND_TOOL_NAME => Ok(self.exec_command(&call).await?.json()),
            WRITE_STDIN_TOOL_NAME => Ok(self.write_stdin(&call).await?.json()),
            APPLY_PATCH_TOOL_NAME => Ok(Value::String(self.call_apply_patch(&call)?.0)),
            _ => Err(anyhow!("unsupported tool call: {}", call.name.as_str())),
        }
    }

    pub fn code_mode_output_schema(name: &str) -> Option<Value> {
        matches!(name, EXEC_COMMAND_TOOL_NAME | WRITE_STDIN_TOOL_NAME).then(|| {
            json!({
                "type": "object",
                "required": ["chunk_id", "wall_time_seconds", "output"],
                "properties": {
                "chunk_id": {"type": "string", "description": "Identifier for this output chunk."},
                "wall_time_seconds": {"type": "number", "description": "Time spent waiting during this call."},
                "exit_code": {"type": "integer", "description": "Exit code when the process completed; otherwise absent."},
                "session_id": {"type": "integer", "description": "Session to pass to write_stdin while the process remains live; otherwise absent."},
                "output": {"type": "string", "description": "Output produced during this call."}
                }
            })
        })
    }

    pub async fn call_with_metadata(&self, call: ToolCall) -> ShellToolOutput {
        match self.call_inner(&call).await {
            Ok((output, metadata)) => ShellToolOutput {
                body: ToolOutput {
                    full_output: None,
                    images: std::sync::Arc::new(Vec::new()),
                    output: Arc::from(output),
                    status: ToolOutputStatus::Success,
                },
                metadata,
            },
            Err(error) => ShellToolOutput {
                body: ToolOutput {
                    full_output: None,
                    images: std::sync::Arc::new(Vec::new()),
                    output: Arc::from(error.to_string()),
                    status: ToolOutputStatus::Error,
                },
                metadata: None,
            },
        }
    }

    async fn call_inner(&self, call: &ToolCall) -> Result<(String, Option<ToolResultMetadata>)> {
        match call.name.as_str() {
            EXEC_COMMAND_TOOL_NAME => Ok((self.exec_command(call).await?.render(), None)),
            WRITE_STDIN_TOOL_NAME => Ok((self.write_stdin(call).await?.render(), None)),
            APPLY_PATCH_TOOL_NAME => self.call_apply_patch(call),
            _ => Err(anyhow!("unsupported tool call: {}", call.name.as_str())),
        }
    }

    /// Initialize ordinary Python filesystem operations in this tool's workdir.
    ///
    /// # Safety
    /// Must run on a dedicated interpreter thread with CLONE_FS unshared.
    /// Poll this future on that same thread throughout.
    pub async unsafe fn enter_interpreter_thread(&self) -> Result<()> {
        match &self.exec {
            ExecContext::Directory {
                working_directory, ..
            } => {
                std::env::set_current_dir(working_directory)?;
                Ok(())
            }
            ExecContext::View(view) => unsafe { view.enter_interpreter_thread().await },
        }
    }

    /// Start `cmd` the way `exec_command` would, and hand the running process
    /// over instead of collecting its output.
    pub async fn spawn(&self, cmd: &str, workdir: Option<&str>) -> Result<SpawnedProcess> {
        let session = self.spawn_process(cmd, workdir).await?;
        Ok(SpawnedProcess {
            session,
            drain_deadline: None,
        })
    }

    async fn exec_command(&self, call: &ToolCall) -> Result<ExecOutput> {
        if call.tool_type != ToolType::Function {
            return Err(anyhow!("exec_command expects a function tool call"));
        }

        let args: ShellArgs = serde_json::from_str(&call.arguments)?;
        let started = Instant::now();
        let mut session = self
            .spawn_process(&args.cmd, args.workdir.as_deref())
            .await?;
        let yield_ms = args
            .yield_time_ms
            .unwrap_or(DEFAULT_EXEC_YIELD_MS)
            .clamp(MIN_YIELD_MS, MAX_YIELD_MS);
        let (status, output) =
            collect_process_output(&mut session, Duration::from_millis(yield_ms), false).await?;
        let process_id = status
            .is_none()
            .then(|| self.processes.next_id.fetch_add(1, Ordering::Relaxed));
        if let Some(id) = process_id {
            self.processes.sessions.lock().await.insert(id, session);
        }
        Ok(exec_output(
            started.elapsed(),
            status.and_then(|s| s.code()),
            process_id,
            output,
            args.max_output_tokens,
        ))
    }

    async fn spawn_process(&self, cmd: &str, workdir: Option<&str>) -> Result<ProcessSession> {
        let mut command = Command::new("direnv");
        command.args(["exec", "."]);
        command.env_remove("DIRENV_DIFF");
        command.env_remove("DIRENV_DIR");
        command.env_remove("DIRENV_FILE");
        command.env_remove("DIRENV_WATCHES");
        for (name, value) in &self.env {
            command.env(name, value);
        }
        // A pipeline reports its last failing stage, so `cargo test | tail`
        // fails when the tests do rather than when `tail` does.
        command.args(["bash", "-o", "pipefail", "-c"]).arg(cmd);
        command.kill_on_drop(true);
        let cwd = workdir.map(Utf8Path::new);
        match &self.exec {
            ExecContext::Directory {
                working_directory,
                path_overrides,
            } => {
                command.env(
                    "PATH",
                    path_overrides.add_to(&std::env::var_os("PATH").expect("PATH must be set")),
                );
                // An absolute model-supplied cwd wins; a relative one resolves
                // against the tool's working directory (join handles both).
                let cwd = cwd.map_or_else(
                    || working_directory.clone(),
                    |cwd| working_directory.join(cwd),
                );
                command.current_dir(cwd.as_std_path());
            }
            ExecContext::View(view) => {
                view.prepare_command(&mut command, cwd).await?;
            }
        }

        command.stdin(std::process::Stdio::piped());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("failed to capture command stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to capture command stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("failed to capture command stderr"))?;
        let (output_tx, output_rx) = mpsc::unbounded_channel();
        let stdout_task = tokio::spawn(read_output_chunks(stdout, output_tx.clone()));
        let stderr_task = tokio::spawn(read_output_chunks(stderr, output_tx));

        drop(stdout_task);
        drop(stderr_task);
        let wait_task = tokio::spawn(async move { child.wait().await });
        Ok(ProcessSession {
            stdin: Some(stdin),
            wait_task,
            status: None,
            output_rx,
        })
    }

    async fn write_stdin(&self, call: &ToolCall) -> Result<ExecOutput> {
        let args: WriteStdinArgs = serde_json::from_str(&call.arguments)?;
        let started = Instant::now();
        let mut session = self
            .processes
            .sessions
            .lock()
            .await
            .remove(&args.session_id)
            .ok_or_else(|| anyhow!("unknown session ID {}", args.session_id))?;
        let poll = args.chars.is_empty();
        if !poll {
            let stdin = session
                .stdin
                .as_mut()
                .ok_or_else(|| anyhow!("session stdin is closed"))?;
            stdin.write_all(args.chars.as_bytes()).await?;
            stdin.flush().await?;
        }
        let yield_ms = if poll {
            args.yield_time_ms
                .unwrap_or(DEFAULT_POLL_YIELD_MS)
                .clamp(MIN_POLL_YIELD_MS, MAX_YIELD_MS)
        } else {
            args.yield_time_ms
                .unwrap_or(DEFAULT_STDIN_WRITE_YIELD_MS)
                .clamp(MIN_YIELD_MS, MAX_STDIN_WRITE_YIELD_MS)
        };
        let (status, output) =
            collect_process_output(&mut session, Duration::from_millis(yield_ms), poll).await?;
        let process_id = status.is_none().then_some(args.session_id);
        if process_id.is_some() {
            self.processes
                .sessions
                .lock()
                .await
                .insert(args.session_id, session);
        }
        Ok(exec_output(
            started.elapsed(),
            status.and_then(|s| s.code()),
            process_id,
            output,
            args.max_output_tokens,
        ))
    }

    fn call_apply_patch(&self, call: &ToolCall) -> Result<(String, Option<ToolResultMetadata>)> {
        if call.tool_type != ToolType::Custom {
            return Err(anyhow!("apply_patch expects a custom tool call"));
        }
        let (output, metadata) =
            apply_patch::apply_patch_with_metadata(&call.arguments, &|path| {
                self.resolve_patch_path(path)
            })?;
        Ok((
            output,
            Some(ToolResultMetadata::ApplyPatch(ApplyPatchMetadata {
                changes: metadata.changes,
            })),
        ))
    }
}

/// Lossy UTF-8, for callers that render process output themselves.
pub fn decode_output_lossy(bytes: Vec<u8>) -> String {
    decode_output(bytes).0
}

fn decode_output(bytes: Vec<u8>) -> (String, bool) {
    match String::from_utf8(bytes) {
        Ok(output) => (output, true),
        Err(error) => (
            String::from_utf8_lossy(error.as_bytes()).into_owned(),
            false,
        ),
    }
}

/// Keeps the first and last halves of a byte budget and notes what fell out
/// of the middle, so a long log still shows how it started and how it ended.
pub struct BoundedOutput {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    head_limit: usize,
    tail_limit: usize,
    total_bytes: u64,
    newline_count: u64,
    last_byte: Option<u8>,
}

#[allow(dead_code)]
struct FinishedOutput {
    bytes: Vec<u8>,
    truncated: Option<OutputStats>,
}

#[allow(dead_code)]
struct OutputStats {
    total_bytes: u64,
    total_lines: u64,
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

    /// The kept bytes, with a marker where the middle was dropped.
    pub fn into_bytes(self) -> Vec<u8> {
        self.finish().bytes
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
            newline_count: 0,
            last_byte: None,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        self.total_bytes = self.total_bytes.saturating_add(chunk.len() as u64);
        self.newline_count = self
            .newline_count
            .saturating_add(chunk.iter().filter(|byte| **byte == b'\n').count() as u64);
        self.last_byte = chunk.last().copied().or(self.last_byte);

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

    fn finish(self) -> FinishedOutput {
        let limit = self.head_limit + self.tail_limit;
        let truncated = (self.total_bytes as usize > limit).then(|| OutputStats {
            total_bytes: self.total_bytes,
            total_lines: self.total_lines(),
        });
        let mut bytes = self.head;
        if let Some(stats) = &truncated {
            bytes.extend_from_slice(
                format!(
                    "\n…{} tokens truncated…\n",
                    approx_tokens_from_byte_count(stats.total_bytes.saturating_sub(limit as u64))
                )
                .as_bytes(),
            );
        }
        bytes.extend(self.tail);
        FinishedOutput { bytes, truncated }
    }

    fn total_lines(&self) -> u64 {
        self.newline_count + u64::from(self.total_bytes > 0 && self.last_byte != Some(b'\n'))
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
