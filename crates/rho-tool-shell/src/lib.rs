//! Concrete shell tool building block.
//!
//! The process outcome shape is adapted from Tau's `tau-ext-shell`: commands
//! that start successfully return structured results even when they exit
//! non-zero or time out. Only malformed arguments and start/configuration
//! failures are surfaced as tool errors.

mod apply_patch;
mod truncate;

use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use rho_core::{ToolCall, ToolFormat, ToolGrammarSyntax, ToolResult, ToolSpec, ToolType};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time;

pub const DEFAULT_TIMEOUT_SECS: u64 = 120;
pub const SLOW_COMMAND_EXEC_TIME_THRESHOLD_SECS: u64 = 5;
pub const SHELL_COMMAND_TOOL_NAME: &str = "shell_command";
pub const APPLY_PATCH_TOOL_NAME: &str = "apply_patch";

#[derive(Clone, Debug)]
pub struct ShellTools {
    default_timeout: Duration,
}

#[derive(Debug, Deserialize)]
struct ShellArgs {
    #[serde(alias = "cmd")]
    command: String,
    cwd: Option<String>,
    timeout: Option<u64>,
}

#[derive(Debug)]
struct CommandDetails {
    status: Option<i32>,
    signal: Option<i32>,
    timed_out: bool,
    duration_seconds: Option<u64>,
    termination_reason: &'static str,
    total_lines: Option<usize>,
    total_bytes: Option<usize>,
    output: String,
    stdout: String,
    stderr: String,
    truncated: bool,
    valid_utf8: bool,
}

impl ShellTools {
    pub fn new(timeout: Duration) -> Self {
        Self {
            default_timeout: timeout,
        }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        vec![self.shell_command_spec(), self.apply_patch_spec()]
    }

    pub fn shell_command_spec(&self) -> ToolSpec {
        ToolSpec {
            name: SHELL_COMMAND_TOOL_NAME.to_owned(),
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
                        "description": "Optional working directory"
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
            name: APPLY_PATCH_TOOL_NAME.to_owned(),
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

    pub async fn call(&self, call: ToolCall) -> ToolResult {
        let call_id = call.id.clone();
        let mut result = match self.call_inner(&call).await {
            Ok(content) => ToolResult::success(call_id, content),
            Err(error) => ToolResult::error(call_id, error.to_string()),
        };
        result.tool_type = call.tool_type;
        result
    }

    async fn call_inner(&self, call: &ToolCall) -> Result<String> {
        match call.name.as_str() {
            SHELL_COMMAND_TOOL_NAME => self.call_shell(call).await,
            APPLY_PATCH_TOOL_NAME => self.call_apply_patch(call),
            _ => Err(anyhow!("unsupported tool call: {}", call.name)),
        }
    }

    async fn call_shell(&self, call: &ToolCall) -> Result<String> {
        if call.tool_type != ToolType::Function {
            return Err(anyhow!("shell_command expects a function tool call"));
        }

        let args: ShellArgs = serde_json::from_value(call.arguments.clone())?;
        let timeout = args
            .timeout
            .map(Duration::from_secs)
            .unwrap_or(self.default_timeout);
        if timeout.is_zero() {
            return Err(anyhow!("timeout must be greater than zero"));
        }

        let mut command = Command::new("sh");
        command.arg("-c").arg(&args.command);
        command.kill_on_drop(true);
        if let Some(cwd) = args.cwd {
            command.current_dir(cwd);
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
                return Ok(details_json(CommandDetails {
                    status: None,
                    signal: None,
                    timed_out: true,
                    duration_seconds: Some(timeout.as_secs()),
                    termination_reason: "timeout",
                    total_lines: None,
                    total_bytes: None,
                    output: String::new(),
                    stdout: String::new(),
                    stderr: String::new(),
                    truncated: false,
                    valid_utf8: true,
                }));
            }
        };
        let stdout = stdout_task
            .await
            .map_err(|error| anyhow!("stdout task failed: {error}"))??;
        let stderr = stderr_task
            .await
            .map_err(|error| anyhow!("stderr task failed: {error}"))??;

        let elapsed = started.elapsed();
        let duration_seconds = (Duration::from_secs(SLOW_COMMAND_EXEC_TIME_THRESHOLD_SECS)
            < elapsed)
            .then(|| elapsed.as_secs_f64().ceil() as u64);
        let status_code = status.code();
        #[cfg(unix)]
        let signal = std::os::unix::process::ExitStatusExt::signal(&status);
        #[cfg(not(unix))]
        let signal = None;

        let (stdout, stdout_valid_utf8) = decode_output(stdout);
        let (stderr, stderr_valid_utf8) = decode_output(stderr);
        let output = combine_output(&stdout, &stderr);

        let truncated = truncate::truncate_line_oriented(&output);

        Ok(details_json(CommandDetails {
            status: status_code,
            signal,
            timed_out: false,
            duration_seconds,
            termination_reason: if output_status_success(status_code, signal) {
                "exit"
            } else if signal.is_some() {
                "signal"
            } else {
                "exit"
            },
            total_lines: truncated.was_truncated.then_some(truncated.total_lines),
            total_bytes: truncated.was_truncated.then_some(truncated.total_bytes),
            output: truncated.content,
            stdout,
            stderr,
            truncated: truncated.was_truncated,
            valid_utf8: stdout_valid_utf8 && stderr_valid_utf8,
        }))
    }

    fn call_apply_patch(&self, call: &ToolCall) -> Result<String> {
        if call.tool_type != ToolType::Custom {
            return Err(anyhow!("apply_patch expects a custom tool call"));
        }
        let patch = call
            .arguments
            .as_str()
            .ok_or_else(|| anyhow!("apply_patch expects freeform patch text"))?;
        apply_patch::apply_patch(patch)
    }
}

fn output_status_success(status: Option<i32>, signal: Option<i32>) -> bool {
    status == Some(0) && signal.is_none()
}

fn details_json(details: CommandDetails) -> String {
    let mut value = json!({
        "status": details.status,
        "signal": details.signal,
        "timed_out": details.timed_out,
        "termination_reason": details.termination_reason,
        "output": details.output,
        "stdout": details.stdout,
        "stderr": details.stderr,
        "truncated": details.truncated,
        "valid_utf8": details.valid_utf8,
    });

    insert_optional(&mut value, "duration_seconds", details.duration_seconds);
    insert_optional(&mut value, "total_lines", details.total_lines);
    insert_optional(&mut value, "total_bytes", details.total_bytes);
    value.to_string()
}

fn insert_optional<T: serde::Serialize>(value: &mut Value, key: &str, item: Option<T>) {
    let Some(item) = item else {
        return;
    };
    let Value::Object(map) = value else {
        return;
    };
    map.insert(
        key.to_owned(),
        serde_json::to_value(item).expect("optional field serializes"),
    );
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
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

#[cfg(test)]
mod tests;
