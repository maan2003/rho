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
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::time;

pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub const SHELL_COMMAND_TOOL_NAME: &str = "shell_command";
pub const APPLY_PATCH_TOOL_NAME: &str = "apply_patch";
const MAX_OUTPUT_TOKENS: usize = 10_000;
const MAX_OUTPUT_BYTES: usize = MAX_OUTPUT_TOKENS * APPROX_BYTES_PER_TOKEN as usize;
const APPROX_BYTES_PER_TOKEN: u64 = 4;

#[derive(Clone, Debug)]
pub struct ShellTools {
    default_timeout: Duration,
    exec: ExecContext,
    env: Vec<(String, String)>,
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
    #[serde(alias = "cmd")]
    command: String,
    cwd: Option<String>,
    timeout: Option<u64>,
}

impl ShellTools {
    /// Tools for an agent's workspace view. Namespace setup and cache warming
    /// run lazily on the first shell command, hiding their latency behind the
    /// model's first response.
    pub fn new(timeout: Duration, view: Arc<View>) -> Self {
        Self {
            default_timeout: timeout,
            exec: ExecContext::View(view),
            env: Vec::new(),
        }
    }

    /// Tools running directly in a directory, without a workspace.
    pub fn in_directory(
        timeout: Duration,
        working_directory: Utf8PathBuf,
        path_overrides: PathOverrides,
    ) -> Self {
        Self {
            default_timeout: timeout,
            exec: ExecContext::Directory {
                working_directory,
                path_overrides,
            },
            env: Vec::new(),
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
    fn resolve_patch_path(&self, path: &Path) -> std::path::PathBuf {
        match &self.exec {
            ExecContext::Directory {
                working_directory, ..
            } => {
                if path.is_absolute() {
                    path.to_owned()
                } else {
                    working_directory.as_std_path().join(path)
                }
            }
            ExecContext::View(view) => {
                if path.is_absolute() {
                    view.resolve_host_path(path)
                } else {
                    view.primary().slot().as_std_path().join(path)
                }
            }
        }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        vec![self.shell_command_spec(), self.apply_patch_spec()]
    }

    pub fn shell_command_spec(&self) -> ToolSpec {
        ToolSpec {
            name: ToolName::try_from(SHELL_COMMAND_TOOL_NAME).expect("valid tool name"),
            tool_type: ToolType::Function,
            description: "Run a shell command and return structured process output.".to_owned(),
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["command"],
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Command to run with sh -c"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Optional working directory; defaults to the agent's working directory"
                    },
                    "timeout": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Optional command timeout in seconds"
                    }
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
        matches!(name, SHELL_COMMAND_TOOL_NAME | APPLY_PATCH_TOOL_NAME)
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

    pub async fn call_with_metadata(&self, call: ToolCall) -> ShellToolOutput {
        match self.call_inner(&call).await {
            Ok((output, metadata)) => ShellToolOutput {
                body: ToolOutput {
                    output: Arc::from(output),
                    status: ToolOutputStatus::Success,
                },
                metadata,
            },
            Err(error) => ShellToolOutput {
                body: ToolOutput {
                    output: Arc::from(error.to_string()),
                    status: ToolOutputStatus::Error,
                },
                metadata: None,
            },
        }
    }

    async fn call_inner(&self, call: &ToolCall) -> Result<(String, Option<ToolResultMetadata>)> {
        match call.name.as_str() {
            SHELL_COMMAND_TOOL_NAME => self.call_shell(call).await,
            APPLY_PATCH_TOOL_NAME => self.call_apply_patch(call),
            _ => Err(anyhow!("unsupported tool call: {}", call.name.as_str())),
        }
    }

    async fn call_shell(&self, call: &ToolCall) -> Result<(String, Option<ToolResultMetadata>)> {
        if call.tool_type != ToolType::Function {
            return Err(anyhow!("shell_command expects a function tool call"));
        }

        let args: ShellArgs = serde_json::from_str(&call.arguments)?;
        let timeout = args
            .timeout
            .map(Duration::from_secs)
            .unwrap_or(self.default_timeout);
        if timeout.is_zero() {
            return Err(anyhow!("timeout must be greater than zero"));
        }

        let mut command = Command::new("sh");
        command.arg("-c").arg(&args.command);
        for (name, value) in &self.env {
            command.env(name, value);
        }
        command.kill_on_drop(true);
        let cwd = args.cwd.as_deref().map(Utf8Path::new);
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
                view.prepare_command(&mut command, cwd, Vec::new()).await?;
            }
        }

        let started = Instant::now();
        command.stdin(std::process::Stdio::null());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to capture command stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("failed to capture command stderr"))?;
        let (output_tx, mut output_rx) = mpsc::unbounded_channel();
        let stdout_task = tokio::spawn(read_output_chunks(stdout, output_tx.clone()));
        let stderr_task = tokio::spawn(read_output_chunks(stderr, output_tx));

        let status = match time::timeout(timeout, child.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                let _ = stdout_task.await;
                let _ = stderr_task.await;
                return Ok((timeout_output_text(timeout), None));
            }
        };
        stdout_task
            .await
            .map_err(|error| anyhow!("stdout task failed: {error}"))??;
        stderr_task
            .await
            .map_err(|error| anyhow!("stderr task failed: {error}"))??;
        let mut output = BoundedOutput::new(MAX_OUTPUT_BYTES);
        while let Some(chunk) = output_rx.recv().await {
            output.push(&chunk);
        }

        let elapsed = started.elapsed();
        let status_code = status.code();
        #[cfg(unix)]
        let signal = std::os::unix::process::ExitStatusExt::signal(&status);
        #[cfg(not(unix))]
        let signal = None;

        let output = output.finish();
        let (mut content, valid_utf8) = decode_output(output.bytes);
        if let Some(stats) = output.truncated {
            content = format!(
                "Warning: truncated output (original token count: {})\nTotal output lines: {}\n\n{}",
                approx_tokens_from_byte_count(stats.total_bytes),
                stats.total_lines,
                content,
            );
        }

        Ok((
            command_output_text(CommandOutputText {
                status: status_code,
                signal,
                elapsed,
                output: content,
                valid_utf8,
            }),
            None,
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

fn decode_output(bytes: Vec<u8>) -> (String, bool) {
    match String::from_utf8(bytes) {
        Ok(output) => (output, true),
        Err(error) => (
            String::from_utf8_lossy(error.as_bytes()).into_owned(),
            false,
        ),
    }
}

struct BoundedOutput {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    head_limit: usize,
    tail_limit: usize,
    total_bytes: u64,
    newline_count: u64,
    last_byte: Option<u8>,
}

struct FinishedOutput {
    bytes: Vec<u8>,
    truncated: Option<OutputStats>,
}

struct OutputStats {
    total_bytes: u64,
    total_lines: u64,
}

impl BoundedOutput {
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

    fn push(&mut self, chunk: &[u8]) {
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

struct CommandOutputText {
    status: Option<i32>,
    signal: Option<i32>,
    elapsed: Duration,
    output: String,
    valid_utf8: bool,
}

fn timeout_output_text(timeout: Duration) -> String {
    format!(
        "Command timed out after {} seconds\nOutput:\n",
        timeout.as_secs()
    )
}

fn command_output_text(details: CommandOutputText) -> String {
    let mut sections = Vec::new();
    match (details.status, details.signal) {
        (Some(status), _) => sections.push(format!("Exit code: {status}")),
        (None, Some(signal)) => sections.push(format!("Signal: {signal}")),
        (None, None) => sections.push("Exit status: unknown".to_owned()),
    }
    sections.push(format!(
        "Wall time: {:.3} seconds",
        details.elapsed.as_secs_f64()
    ));
    if !details.valid_utf8 {
        sections.push("Output contained invalid UTF-8 and was decoded lossily".to_owned());
    }
    sections.push(format!("Output:\n{}", details.output));
    sections.join("\n")
}

#[cfg(test)]
mod tests;
