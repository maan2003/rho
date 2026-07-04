//! Concrete shell tool building block.
//!
//! The process outcome shape is adapted from Tau's `tau-ext-shell`: commands
//! that start successfully return structured results even when they exit
//! non-zero or time out. Only malformed arguments and start/configuration
//! failures are surfaced as tool errors.

mod apply_patch;
mod truncate;

use std::ffi::{CStr, CString};
use std::os::fd::{BorrowedFd, RawFd};
use std::path::Path;

use camino::Utf8PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use rho_workspaces::Workspace;
use rho_core::{
    ApplyPatchMetadata, ToolCall, ToolFormat, ToolGrammarSyntax, ToolName, ToolOutput,
    ToolOutputStatus, ToolResultMetadata, ToolSpec, ToolType,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time;

pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub const SHELL_COMMAND_TOOL_NAME: &str = "shell_command";
pub const APPLY_PATCH_TOOL_NAME: &str = "apply_patch";

#[derive(Clone, Debug)]
pub struct ShellTools {
    default_timeout: Duration,
    exec: ExecContext,
}

/// Where and how tool commands execute; the owning process's cwd is never
/// consulted.
#[derive(Clone, Debug)]
enum ExecContext {
    /// Run directly in a directory (tests, workspace-less fallbacks).
    Directory(Utf8PathBuf),
    /// Run in the workspace: inside its mount namespace at the origin repo
    /// path.
    Workspace(Arc<Workspace>),
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
    /// Tools for an agent's workspace. Namespace setup and cache warming run
    /// lazily on the first shell command, hiding their latency behind the
    /// model's first response.
    pub fn new(timeout: Duration, workspace: Arc<Workspace>) -> Self {
        Self {
            default_timeout: timeout,
            exec: ExecContext::Workspace(workspace),
        }
    }

    /// Tools running directly in a directory, without a workspace.
    pub fn in_directory(timeout: Duration, working_directory: Utf8PathBuf) -> Self {
        Self {
            default_timeout: timeout,
            exec: ExecContext::Directory(working_directory),
        }
    }

    /// Where in-process file operations (patches) apply: the real checkout
    /// files, never a namespace-relative path.
    fn patch_directory(&self) -> &Path {
        match &self.exec {
            ExecContext::Directory(dir) => dir.as_std_path(),
            ExecContext::Workspace(workspace) => workspace.slot().as_std_path(),
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

        let (base_dir, mnt_ns) = match &self.exec {
            ExecContext::Directory(dir) => (dir.clone(), None),
            // A user-checkout workspace IS the repo path: no namespace.
            ExecContext::Workspace(workspace)
                if workspace.is_user_checkout() =>
            {
                (workspace.repo().to_owned(), None)
            }
            ExecContext::Workspace(workspace) => {
                // The raw fd stays valid: the workspace (and its namespace
                // fd) lives in `self.exec` past the spawn.
                let ns_fd = std::os::fd::AsRawFd::as_raw_fd(workspace.mnt_ns().await);
                (workspace.repo().to_owned(), Some(ns_fd))
            }
        };

        let mut command = Command::new("sh");
        command.arg("-c").arg(&args.command);
        command.kill_on_drop(true);
        // An absolute model-supplied cwd wins; a relative one resolves
        // against the agent's working directory (join handles both).
        let cwd = match args.cwd {
            Some(cwd) => base_dir.join(cwd),
            None => base_dir,
        };
        match mnt_ns {
            Some(ns_fd) => {
                // The namespaced cwd only exists after setns, so entering the
                // namespace and chdir both happen post-fork. Everything the
                // closure needs is prepared here: pre_exec runs between fork
                // and exec where allocation is off-limits.
                let cwd = CString::new(cwd.into_string().into_bytes())
                    .map_err(|_| anyhow!("working directory contains a NUL byte"))?;
                unsafe {
                    command.pre_exec(move || enter_workspace_ns(ns_fd, &cwd));
                }
            }
            None => {
                command.current_dir(cwd);
            }
        }

        let started = Instant::now();
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        let mut child = command.spawn()?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("failed to capture command stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("failed to capture command stderr"))?;
        let stdout_task = tokio::spawn(async move {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).await.map(|_| bytes)
        });
        let stderr_task = tokio::spawn(async move {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).await.map(|_| bytes)
        });

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
        let stdout = stdout_task
            .await
            .map_err(|error| anyhow!("stdout task failed: {error}"))??;
        let stderr = stderr_task
            .await
            .map_err(|error| anyhow!("stderr task failed: {error}"))??;

        let elapsed = started.elapsed();
        let status_code = status.code();
        #[cfg(unix)]
        let signal = std::os::unix::process::ExitStatusExt::signal(&status);
        #[cfg(not(unix))]
        let signal = None;

        let (stdout, stdout_valid_utf8) = decode_output(stdout);
        let (stderr, stderr_valid_utf8) = decode_output(stderr);
        let output = combine_output(&stdout, &stderr);
        let valid_utf8 = stdout_valid_utf8 && stderr_valid_utf8;

        let truncated = truncate::formatted_truncate_text(&output);

        Ok((
            command_output_text(CommandOutputText {
                status: status_code,
                signal,
                elapsed,
                output: truncated.content,
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
            apply_patch::apply_patch_with_metadata(&call.arguments, self.patch_directory())?;
        Ok((
            output,
            Some(ToolResultMetadata::ApplyPatch(ApplyPatchMetadata {
                changes: metadata.changes,
            })),
        ))
    }
}

/// Runs between fork and exec: enter the workspace's mount namespace, move
/// to the working directory (whose path only resolves inside it), and shed
/// the daemon's in-namespace privileges. Must not allocate — the forked
/// child could deadlock on the allocator lock.
fn enter_workspace_ns(ns_fd: RawFd, cwd: &CStr) -> std::io::Result<()> {
    use rustix::thread::{CapabilitySet, CapabilitySets, LinkNameSpaceType};

    // SAFETY: the fd is kept alive by the `Arc<Workspace>` held in the
    // spawning `ShellTools` for the duration of the spawn.
    let fd = unsafe { BorrowedFd::borrow_raw(ns_fd) };
    rustix::thread::move_into_link_name_space(fd, Some(LinkNameSpaceType::Mount))?;
    rustix::process::chdir(cwd)?;
    rustix::thread::set_no_new_privs(true)?;
    let empty = CapabilitySet::empty();
    rustix::thread::set_capabilities(
        None,
        CapabilitySets {
            effective: empty,
            permitted: empty,
            inheritable: empty,
        },
    )?;
    Ok(())
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

fn combine_output(stdout: &str, stderr: &str) -> String {
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout.to_owned(),
        (true, false) => stderr.to_owned(),
        (false, false) => format!("{stdout}{stderr}"),
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
