//! Rho's Claude Code integration.
//!
//! The Claude Code process protocol is intentionally private to this crate.
//! Public APIs should expose Rho-facing vocabulary and typed metadata rather
//! than raw stream-json messages.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use camino::Utf8PathBuf;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

#[allow(dead_code)]
mod protocol;
mod transcript;

pub use protocol::{Model, Session};
pub use transcript::*;

const DEFAULT_COMMAND: &str = "claude";
#[allow(dead_code)]
const CLAUDE_AGENT_SDK_VERSION: &str = "0.3.201";
const GRACEFUL_EXIT_TIMEOUT: Duration = Duration::from_secs(2);
const KILL_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct ClaudeCodeOptions {
    pub command: Utf8PathBuf,
    pub cwd: Utf8PathBuf,
    pub model: Model,
    pub session: Session,
}

#[allow(dead_code)]
impl ClaudeCodeOptions {
    pub fn new(cwd: impl Into<Utf8PathBuf>, model: Model) -> Self {
        Self {
            command: DEFAULT_COMMAND.into(),
            cwd: cwd.into(),
            model,
            session: Session::New,
        }
    }

    pub(crate) fn args(&self) -> Vec<String> {
        let mut args = vec![
            "--output-format".to_owned(),
            "stream-json".to_owned(),
            "--verbose".to_owned(),
            "--input-format".to_owned(),
            "stream-json".to_owned(),
            "--include-partial-messages".to_owned(),
            "--permission-mode".to_owned(),
            "bypassPermissions".to_owned(),
            "--allow-dangerously-skip-permissions".to_owned(),
            "--model".to_owned(),
            self.model.as_arg().to_owned(),
        ];
        match &self.session {
            Session::New => {}
            Session::Id(session_id) => {
                args.push("--session-id".to_owned());
                args.push(session_id.to_string());
            }
            Session::Resume(session_id) => {
                args.push("--resume".to_owned());
                args.push(session_id.to_string());
            }
        }
        args
    }

    pub(crate) async fn spawn(&self) -> Result<ClaudeSession> {
        let mut command = Command::new(self.command.as_std_path());
        command.args(self.args());
        command.current_dir(self.cwd.as_std_path());
        command.env("CLAUDE_CODE_ENTRYPOINT", "sdk-ts");
        command.env("CLAUDE_AGENT_SDK_VERSION", CLAUDE_AGENT_SDK_VERSION);
        command.env_remove("NODE_OPTIONS");
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::null());
        command.kill_on_drop(true);

        let mut child = command.spawn().context("spawn Claude Code")?;
        let stdin = child
            .stdin
            .take()
            .context("Claude Code stdin was not piped")?;
        let stdout = child
            .stdout
            .take()
            .context("Claude Code stdout was not piped")?;
        Ok(ClaudeSession {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout).lines(),
        })
    }
}

#[allow(dead_code)]
pub struct ClaudeSession {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Lines<BufReader<ChildStdout>>,
}

#[allow(dead_code)]
impl ClaudeSession {
    pub(crate) async fn send_user_message(&mut self, text: impl Into<String>) -> Result<()> {
        self.write_message(&protocol::InputMessage::user(text))
            .await
    }

    async fn write_message(&mut self, message: &protocol::InputMessage) -> Result<()> {
        let Some(stdin) = &mut self.stdin else {
            bail!("Claude Code stdin is closed");
        };
        let mut bytes = serde_json::to_vec(message)?;
        bytes.push(b'\n');
        stdin.write_all(&bytes).await?;
        stdin.flush().await?;
        Ok(())
    }

    pub(crate) async fn next_event(&mut self) -> Result<Option<protocol::ClaudeEvent>> {
        loop {
            let Some(line) = self.stdout.next_line().await? else {
                return Ok(None);
            };
            if line.trim().is_empty() {
                continue;
            }
            let message = serde_json::from_str(&line)
                .with_context(|| format!("parse Claude Code stdout line: {line}"))?;
            return Ok(Some(message));
        }
    }

    pub async fn end_input(&mut self) -> Result<()> {
        if let Some(mut stdin) = self.stdin.take() {
            stdin.shutdown().await?;
        }
        Ok(())
    }

    pub async fn close(mut self) -> Result<()> {
        // Ignore write-side errors: the child may already have exited, and we
        // still want to reach the wait/kill path below.
        let _ = self.end_input().await;
        if tokio::time::timeout(GRACEFUL_EXIT_TIMEOUT, self.wait())
            .await
            .is_ok()
        {
            return Ok(());
        }

        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(KILL_EXIT_TIMEOUT, self.wait()).await;
        Ok(())
    }

    pub async fn wait(&mut self) -> Result<std::process::ExitStatus> {
        self.child.wait().await.context("wait for Claude Code")
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn builds_stream_json_args() {
        let mut options = ClaudeCodeOptions::new("/tmp/project", Model::Sonnet);
        options.session = Session::Id(uuid::uuid!("00000000-0000-4000-8000-000000000000"));

        assert_eq!(
            options.args(),
            [
                "--output-format",
                "stream-json",
                "--verbose",
                "--input-format",
                "stream-json",
                "--include-partial-messages",
                "--permission-mode",
                "bypassPermissions",
                "--allow-dangerously-skip-permissions",
                "--model",
                "sonnet",
                "--session-id",
                "00000000-0000-4000-8000-000000000000",
            ]
        );
    }

    #[test]
    fn builds_user_message() {
        assert_eq!(
            serde_json::to_value(protocol::InputMessage::user("hello")).unwrap(),
            json!({
                "type": "user",
                "session_id": "",
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": "hello"}],
                },
                "parent_tool_use_id": null,
            })
        );
    }

    #[test]
    fn parses_assistant_event() {
        let event: protocol::ClaudeEvent = serde_json::from_value(json!({
            "type": "assistant",
            "session_id": "00000000-0000-4000-8000-000000000001",
            "message": {
                "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "pwd"}},
                    {"type": "text", "text": " world"}
                ]
            }
        }))
        .unwrap();

        let protocol::ClaudeEvent::Assistant(message) = event else {
            panic!("expected assistant event");
        };
        assert_eq!(
            message.session_id,
            Some(uuid::uuid!("00000000-0000-4000-8000-000000000001"))
        );
        assert_eq!(message.text(), "hello world");
        assert!(matches!(
            &message.message.content[1],
            protocol::AssistantContent::ToolUse {
                id,
                name,
                input,
            } if id == "toolu_1"
                && name == "Bash"
                && input["command"] == "pwd"
        ));
    }

    #[test]
    fn parses_result_event() {
        let event: protocol::ClaudeEvent = serde_json::from_value(json!({
            "type": "result",
            "subtype": "error_max_turns",
            "is_error": true,
            "errors": ["too many turns"],
        }))
        .unwrap();

        let protocol::ClaudeEvent::Result(message) = event else {
            panic!("expected result event");
        };
        assert_eq!(message.subtype, protocol::ResultSubtype::ErrorMaxTurns);
        assert!(message.is_error);
        assert_eq!(message.errors, ["too many turns"]);
    }
}
