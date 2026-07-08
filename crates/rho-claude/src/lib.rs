//! Typed Claude Code protocol support for Rho.
//!
//! This crate deliberately stays at the Claude Code boundary: process
//! spawning, stream-json messages, and transcript loading. Rho-specific
//! projection into `rho_core` lives in `rho-agent`.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use camino::Utf8PathBuf;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

pub mod protocol;
mod transcript;

pub use protocol::{ClaudeEvent, Effort, Model, Session};
pub use transcript::*;

const DEFAULT_COMMAND: &str = "claude";
#[allow(dead_code)]
const CLAUDE_AGENT_SDK_VERSION: &str = "0.3.201";
const CLAUDE_CODE_AUTO_COMPACT_WINDOW: &str = "320000";
const GRACEFUL_EXIT_TIMEOUT: Duration = Duration::from_secs(2);
const KILL_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub struct ClaudeCodeOptions {
    pub command: Utf8PathBuf,
    pub cwd: Utf8PathBuf,
    pub model: Model,
    pub effort: Effort,
    pub session: Session,
}

impl ClaudeCodeOptions {
    pub fn new(
        cwd: impl Into<Utf8PathBuf>,
        model: Model,
        effort: Effort,
        session_id: uuid::Uuid,
    ) -> Self {
        Self {
            command: DEFAULT_COMMAND.into(),
            cwd: cwd.into(),
            model,
            effort,
            session: Session::New { session_id },
        }
    }

    fn args(&self) -> Vec<String> {
        let mut args = vec![
            "--output-format".to_owned(),
            "stream-json".to_owned(),
            "--verbose".to_owned(),
            "--input-format".to_owned(),
            "stream-json".to_owned(),
            "--include-partial-messages".to_owned(),
            "--replay-user-messages".to_owned(),
            "--permission-mode".to_owned(),
            "bypassPermissions".to_owned(),
            "--allow-dangerously-skip-permissions".to_owned(),
            "--model".to_owned(),
            self.model.as_arg().to_owned(),
            "--effort".to_owned(),
            self.effort.as_arg().to_owned(),
        ];
        match &self.session {
            Session::New { session_id } => {
                args.push("--session-id".to_owned());
                args.push(session_id.to_string());
            }
            Session::Resume { session_id } => {
                args.push("--resume".to_owned());
                args.push(session_id.to_string());
            }
        }
        args
    }

    pub async fn spawn(&self) -> Result<ClaudeCode> {
        ClaudeCode::spawn_command(self.command()).await
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(self.command.as_std_path());
        command.args(self.args());
        command.current_dir(self.cwd.as_std_path());
        command.env("CLAUDE_CODE_ENTRYPOINT", "sdk-ts");
        command.env("CLAUDE_AGENT_SDK_VERSION", CLAUDE_AGENT_SDK_VERSION);
        command.env(
            "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
            CLAUDE_CODE_AUTO_COMPACT_WINDOW,
        );
        command.env("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1");
        command.env("CLAUDE_CODE_DISABLE_BUNDLED_SKILLS", "1");
        command.env_remove("NODE_OPTIONS");
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::null());
        command.kill_on_drop(true);
        command
    }
}

impl ClaudeCode {
    pub async fn spawn_command(mut command: Command) -> Result<Self> {
        let mut child = command.spawn().context("spawn Claude Code")?;
        let stdin = child
            .stdin
            .take()
            .context("Claude Code stdin was not piped")?;
        let stdout = child
            .stdout
            .take()
            .context("Claude Code stdout was not piped")?;
        Ok(ClaudeCode {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout).lines(),
        })
    }
}

pub struct ClaudeCode {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl ClaudeCode {
    pub async fn send_user_message(&mut self, text: impl Into<String>) -> Result<()> {
        self.write_message(&protocol::InputMessage::user(text))
            .await
    }

    pub async fn apply_effort(&mut self, effort: Effort) -> Result<String> {
        let request_id = uuid::Uuid::new_v4().to_string();
        self.write_json(&serde_json::json!({
            "type": "control_request",
            "request_id": request_id,
            "request": {
                "subtype": "apply_flag_settings",
                "settings": {
                    "effortLevel": effort.as_arg(),
                },
            },
        }))
        .await?;
        Ok(request_id)
    }

    async fn write_message(&mut self, message: &protocol::InputMessage) -> Result<()> {
        self.write_json(message).await
    }

    async fn write_json(&mut self, message: &impl serde::Serialize) -> Result<()> {
        let Some(stdin) = &mut self.stdin else {
            bail!("Claude Code stdin is closed");
        };
        let mut bytes = serde_json::to_vec(message)?;
        bytes.push(b'\n');
        stdin.write_all(&bytes).await?;
        stdin.flush().await?;
        Ok(())
    }

    pub async fn next_event(&mut self) -> Result<Option<ClaudeEvent>> {
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
        let options = ClaudeCodeOptions::new(
            "/tmp/project",
            Model::Sonnet,
            Effort::Medium,
            uuid::uuid!("00000000-0000-4000-8000-000000000000"),
        );

        assert_eq!(
            options.args(),
            [
                "--output-format",
                "stream-json",
                "--verbose",
                "--input-format",
                "stream-json",
                "--include-partial-messages",
                "--replay-user-messages",
                "--permission-mode",
                "bypassPermissions",
                "--allow-dangerously-skip-permissions",
                "--model",
                "sonnet",
                "--effort",
                "medium",
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
    fn parses_assistant_thinking_event() {
        let event: protocol::ClaudeEvent = serde_json::from_value(json!({
            "type": "assistant",
            "message": {
                "model": "claude-fable-5",
                "id": "msg_012GScG8H33PDS5vpbdZ11kY",
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "thinking",
                    "thinking": "",
                    "signature": "CAISvwMKYggPGAIq"
                }],
                "stop_reason": null,
                "usage": {"input_tokens": 2, "output_tokens": 4}
            },
            "parent_tool_use_id": null,
            "session_id": "b1dcda9c-a439-4dd5-b76b-10bec779996c",
            "uuid": "83f8bc79-9c3f-4271-b834-e81d82fbc319",
            "request_id": "req_011CcgmqLKAkdDsusvzJDNFY"
        }))
        .unwrap();

        let protocol::ClaudeEvent::Assistant(message) = event else {
            panic!("expected assistant event");
        };
        assert!(matches!(
            &message.message.content[0],
            protocol::AssistantContent::Thinking { thinking, signature }
                if thinking.is_empty() && signature.as_deref() == Some("CAISvwMKYggPGAIq")
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

    #[test]
    fn parses_control_response_event() {
        let event: protocol::ClaudeEvent = serde_json::from_value(json!({
            "type": "control_response",
            "response": {
                "request_id": "request-1",
                "subtype": "success",
            },
        }))
        .unwrap();

        let protocol::ClaudeEvent::ControlResponse(message) = event else {
            panic!("expected control response event");
        };
        assert_eq!(message.response.request_id, "request-1");
        assert_eq!(message.response.subtype, "success");
        assert_eq!(message.response.error, None);
    }

    #[test]
    fn parses_rate_limit_event_without_body() {
        let event: protocol::ClaudeEvent = serde_json::from_value(json!({
            "type": "rate_limit_event",
            "rate_limit_info": {
                "status": "allowed",
                "resetsAt": 1783173000,
                "rateLimitType": "five_hour",
                "overageStatus": "rejected",
                "overageDisabledReason": "org_level_disabled",
                "isUsingOverage": false
            },
            "uuid": "7850a2a7-37f3-4b5d-9bb6-4aebf33231d5",
            "session_id": "b1dcda9c-a439-4dd5-b76b-10bec779996c"
        }))
        .unwrap();

        assert!(matches!(event, protocol::ClaudeEvent::RateLimitEvent));
    }

    #[test]
    fn parses_system_compact_boundary_event() {
        let event: protocol::ClaudeEvent = serde_json::from_value(json!({
            "type": "system",
            "subtype": "compact_boundary",
            "content": "Conversation compacted",
            "compactMetadata": {
                "trigger": "manual",
                "preTokens": 411842,
                "postTokens": 7797
            },
            "uuid": "1d31a2a9-8361-4561-afac-fdd5f9216d1c",
            "session_id": "00000000-0000-4000-8000-000000000001"
        }))
        .unwrap();

        let protocol::ClaudeEvent::System(protocol::SystemMessage::CompactBoundary {
            compact_metadata,
            ..
        }) = event
        else {
            panic!("expected system event");
        };
        let metadata = compact_metadata.expect("compact metadata");
        assert_eq!(metadata.trigger.as_deref(), Some("manual"));
        assert_eq!(metadata.pre_tokens, Some(411842));
        assert_eq!(metadata.post_tokens, Some(7797));
    }

    #[test]
    fn parses_observed_system_subtypes() {
        let event: protocol::ClaudeEvent = serde_json::from_value(json!({
            "type": "system",
            "subtype": "turn_duration",
            "durationMs": 362188,
            "messageCount": 4,
            "session_id": "00000000-0000-4000-8000-000000000001",
            "uuid": "00000000-0000-4000-8000-000000000002"
        }))
        .unwrap();
        assert!(matches!(
            event,
            protocol::ClaudeEvent::System(protocol::SystemMessage::TurnDuration {
                duration_ms: Some(362188),
                message_count: Some(4),
                ..
            })
        ));

        let event: protocol::ClaudeEvent = serde_json::from_value(json!({
            "type": "system",
            "subtype": "away_summary",
            "content": "summary",
            "sessionId": "00000000-0000-4000-8000-000000000001"
        }))
        .unwrap();
        assert!(matches!(
            event,
            protocol::ClaudeEvent::System(protocol::SystemMessage::AwaySummary {
                content: Some(content),
                ..
            }) if content == "summary"
        ));

        let event: protocol::ClaudeEvent = serde_json::from_value(json!({
            "type": "system",
            "subtype": "local_command",
            "content": "<command-name>/resume</command-name>",
            "level": "info"
        }))
        .unwrap();
        assert!(matches!(
            event,
            protocol::ClaudeEvent::System(protocol::SystemMessage::LocalCommand {
                level: Some(level),
                ..
            }) if level == "info"
        ));

        let event: protocol::ClaudeEvent = serde_json::from_value(json!({
            "type": "system",
            "subtype": "api_error",
            "level": "error",
            "error": {"message": "rate limited"},
            "retryAttempt": 2,
            "retryInMs": 1000,
            "maxRetries": 5
        }))
        .unwrap();
        assert!(matches!(
            event,
            protocol::ClaudeEvent::System(protocol::SystemMessage::ApiError {
                retry_attempt: Some(2),
                retry_in_ms: Some(1000),
                max_retries: Some(5),
                ..
            })
        ));

        let event: protocol::ClaudeEvent = serde_json::from_value(json!({
            "type": "system",
            "subtype": "informational",
            "content": "Unknown command",
            "level": "warning",
            "sessionKind": "primary"
        }))
        .unwrap();
        assert!(matches!(
            event,
            protocol::ClaudeEvent::System(protocol::SystemMessage::Informational {
                level: Some(level),
                session_kind: Some(kind),
                ..
            }) if level == "warning" && kind == "primary"
        ));
    }

    #[test]
    fn parses_synthetic_user_replay_with_string_content() {
        let event: protocol::ClaudeEvent = serde_json::from_value(json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": "This session is being continued from a previous conversation."
            },
            "session_id": "5050cd3e-a4d6-4d2f-bf2c-1e0982515411",
            "parent_tool_use_id": null,
            "uuid": "95c8ed9e-05b1-4cf8-95fd-99c8f9e4934b",
            "timestamp": "2026-07-04T19:01:59.790Z",
            "isReplay": false,
            "isSynthetic": true
        }))
        .unwrap();

        let protocol::ClaudeEvent::User(message) = event else {
            panic!("expected user event");
        };
        assert_eq!(message.is_synthetic, Some(true));
        let content = &message.message.expect("user message").content;
        assert_eq!(
            content,
            &[protocol::OutputContent::Text {
                text: "This session is being continued from a previous conversation.".to_owned()
            }]
        );
    }
}
