//! Claude Code agent support.
//!
//! `rho-claude` owns the Claude Code protocol. This module owns the projection
//! from Claude protocol/transcript messages into Rho agent vocabulary.

use std::num::NonZeroU64;
use std::sync::Arc;

use anyhow::Context as _;
use async_stream::stream;
use futures::Stream;
use rho_claude::{ClaudeCode, ClaudeCodeOptions, Model, Session};
use rho_core::{
    ContentPart, ContextBlock, InferenceResponseItem, PendingInferenceResponse, ToolCallId,
    ToolName, ToolOutput, ToolOutputStatus, ToolResult, ToolType, UnixMs,
};
use rho_db::RhoDb;
use serde_json::Value;
use tokio::sync::{Notify, mpsc};
use uuid::Uuid;

use crate::db::{AgentId, AgentKind, AgentReadTxnExt, AgentWriteTxnExt, TopicId, UnixMillis};
use crate::{AgentState, AgentStateKind, FailedInferenceResponse, StartWorkspace, system_prompt};

#[derive(Clone)]
pub struct ClaudeAgent {
    state: Arc<std::sync::RwLock<AgentState>>,
    control: mpsc::UnboundedSender<ClaudeControl>,
    notify: Arc<Notify>,
}

impl ClaudeAgent {
    pub async fn create(
        db: RhoDb,
        topic_id: TopicId,
        display_name: Option<String>,
        start: StartWorkspace,
        model: Model,
    ) -> anyhow::Result<(AgentId, Self)> {
        let mut write = db.write().await;
        let agent_id = write.alloc_agent_id();
        let workspace = match start {
            StartWorkspace::Create {
                repo,
                parent_revset,
            } => {
                let workspace_id = write.alloc_workspace_id();
                repo.create_workspace(&workspace_id.encoded(), &parent_revset)
                    .await?
            }
            StartWorkspace::Existing(workspace) => workspace,
        };
        let session_id = Uuid::new_v4();
        write.create_agent(
            UnixMillis::now(),
            agent_id,
            topic_id,
            display_name,
            workspace.info().clone(),
            AgentKind::Claude {
                model,
                session_id,
                transcript_path: None,
            },
        );
        write.commit();

        let state = AgentState {
            blocks: Vec::new(),
            tool_specs: Arc::from([]),
            system_prompt: system_prompt::prompt(workspace.repo()),
            kind: AgentStateKind::Idle,
        };
        Ok((
            agent_id,
            Self::new(
                db,
                agent_id,
                workspace,
                model,
                session_id,
                None,
                state,
                ClaudeStartMode::New,
            ),
        ))
    }

    pub async fn load(
        db: RhoDb,
        agent_id: AgentId,
        workspace: Arc<rho_workspaces::Workspace>,
    ) -> anyhow::Result<Self> {
        let record = db.read().get_agent(agent_id);
        let AgentKind::Claude {
            model,
            session_id,
            transcript_path,
        } = record.kind
        else {
            anyhow::bail!("cannot load Rho agent with the Claude agent runtime");
        };
        let blocks = match &transcript_path {
            Some(path) => {
                let messages = rho_claude::read_session_messages(
                    path,
                    rho_claude::SessionMessagesOptions::default(),
                )
                .await?;
                transcript_messages_to_context(&messages)?
            }
            None => Vec::new(),
        };
        let state = AgentState {
            blocks,
            tool_specs: Arc::from([]),
            system_prompt: system_prompt::prompt(workspace.repo()),
            kind: AgentStateKind::Idle,
        };
        Ok(Self::new(
            db,
            agent_id,
            workspace,
            model,
            session_id,
            transcript_path,
            state,
            ClaudeStartMode::Resume,
        ))
    }

    fn new(
        db: RhoDb,
        agent_id: AgentId,
        workspace: Arc<rho_workspaces::Workspace>,
        model: Model,
        session_id: Uuid,
        transcript_path: Option<camino::Utf8PathBuf>,
        state: AgentState,
        start_mode: ClaudeStartMode,
    ) -> Self {
        let state = Arc::new(std::sync::RwLock::new(state));
        let notify = Arc::new(Notify::new());
        let (control, control_rx) = mpsc::unbounded_channel();
        let loop_state = ClaudeLoop {
            db,
            agent_id,
            workspace,
            model,
            session_id,
            transcript_path,
            start_mode,
            process: None,
            state: Arc::clone(&state),
            notify: Arc::clone(&notify),
            control_rx,
        };
        tokio::spawn(loop_state.run());
        Self {
            state,
            control,
            notify,
        }
    }

    pub fn state(&self) -> AgentState {
        self.state.read().expect("poison").clone()
    }

    pub fn send_user_message(&self, text: impl Into<String>) {
        let _ = self.control.send(ClaudeControl::UserMessage(text.into()));
    }

    pub fn cancel(&self) {
        let _ = self.control.send(ClaudeControl::Cancel);
    }

    pub fn subscribe(&self) -> impl Stream<Item = AgentState> + use<> {
        let state = Arc::clone(&self.state);
        let notify = Arc::clone(&self.notify);
        stream! {
            loop {
                let notified = notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();

                let snapshot = state.read().expect("poison").clone();
                yield snapshot;

                notified.await;
            }
        }
    }
}

#[derive(Clone, Copy)]
enum ClaudeStartMode {
    New,
    Resume,
}

enum ClaudeControl {
    UserMessage(String),
    Cancel,
}

struct ClaudeLoop {
    db: RhoDb,
    agent_id: AgentId,
    workspace: Arc<rho_workspaces::Workspace>,
    model: Model,
    session_id: Uuid,
    transcript_path: Option<camino::Utf8PathBuf>,
    start_mode: ClaudeStartMode,
    process: Option<ClaudeCode>,
    state: Arc<std::sync::RwLock<AgentState>>,
    notify: Arc<Notify>,
    control_rx: mpsc::UnboundedReceiver<ClaudeControl>,
}

impl ClaudeLoop {
    async fn run(mut self) {
        loop {
            if self.process.is_some() {
                let event = {
                    let process = self.process.as_mut().expect("checked above");
                    let control_rx = &mut self.control_rx;
                    tokio::select! {
                        biased;
                        control = control_rx.recv() => ClaudeLoopEvent::Control(control),
                        event = process.next_event() => ClaudeLoopEvent::Protocol(event),
                    }
                };
                match event {
                    ClaudeLoopEvent::Control(Some(control)) => self.handle_control(control).await,
                    ClaudeLoopEvent::Control(None) => return,
                    ClaudeLoopEvent::Protocol(Ok(Some(event))) => self.handle_event(event).await,
                    ClaudeLoopEvent::Protocol(Ok(None)) => self.process = None,
                    ClaudeLoopEvent::Protocol(Err(error)) => {
                        self.process = None;
                        self.fail(error);
                    }
                }
            } else {
                let Some(control) = self.control_rx.recv().await else {
                    return;
                };
                self.handle_control(control).await;
            }
        }
    }

    async fn handle_control(&mut self, control: ClaudeControl) {
        match control {
            ClaudeControl::UserMessage(text) => {
                if let Err(error) = self.ensure_process().await {
                    self.fail(error);
                    return;
                }
                let content = vec![ContentPart::Text { text: text.clone() }];
                self.push_block(Arc::new(ContextBlock::UserMessage {
                    content: content.clone(),
                }));
                self.set_kind(AgentStateKind::ApiStreaming {
                    pending_response: PendingInferenceResponse::default(),
                    previous_attempt: None,
                });
                if let Err(error) = self.process.as_mut().unwrap().send_user_message(text).await {
                    self.fail(error);
                }
            }
            ClaudeControl::Cancel => {
                if let Some(process) = self.process.take() {
                    tokio::spawn(async move {
                        let _ = process.close().await;
                    });
                }
                self.set_kind(AgentStateKind::Idle);
            }
        }
    }

    async fn ensure_process(&mut self) -> anyhow::Result<()> {
        if self.process.is_some() {
            return Ok(());
        }
        let session = match self.start_mode {
            ClaudeStartMode::New => Session::New {
                session_id: self.session_id,
            },
            ClaudeStartMode::Resume => Session::Resume {
                session_id: self.session_id,
            },
        };
        let mut options = ClaudeCodeOptions::new(
            self.workspace.repo().to_owned(),
            self.model,
            self.session_id,
        );
        options.session = session;
        let mut command = options.command();
        self.workspace.prepare_command(&mut command).await?;
        self.process = Some(ClaudeCode::spawn_command(command).await?);
        self.start_mode = ClaudeStartMode::Resume;
        Ok(())
    }

    async fn handle_event(&mut self, event: rho_claude::ClaudeEvent) {
        match event {
            rho_claude::ClaudeEvent::System(message) => {
                if message.subtype.as_deref() == Some("init")
                    && let Some(path) = message.transcript_path
                {
                    let path = camino::Utf8PathBuf::from(path);
                    self.transcript_path = Some(path.clone());
                    let mut write = self.db.write().await;
                    write.set_claude_transcript_path(UnixMillis::now(), self.agent_id, path);
                    write.commit();
                }
            }
            rho_claude::ClaudeEvent::Assistant(message) => {
                match assistant_message_to_block(message) {
                    Ok(block) => {
                        self.persist_inference_block(&block).await;
                        self.push_block(block);
                    }
                    Err(error) => self.fail(error),
                }
            }
            rho_claude::ClaudeEvent::User(message) => match user_output_to_block(message) {
                Ok(Some(block)) => self.push_block(block),
                Ok(None) => {}
                Err(error) => self.fail(error),
            },
            rho_claude::ClaudeEvent::Result(message) => {
                if message.is_error {
                    self.fail(anyhow::anyhow!("{}", message.errors.join("\n")));
                } else {
                    self.set_kind(AgentStateKind::Idle);
                    let workspace = Arc::clone(&self.workspace);
                    tokio::spawn(async move {
                        if let Err(error) = workspace.snapshot().await {
                            eprintln!("rho-agent Claude snapshot failed: {error:#}");
                        }
                    });
                }
            }
            rho_claude::ClaudeEvent::StreamEvent(_) => {}
        }
    }

    async fn persist_inference_block(&self, _block: &Arc<ContextBlock>) {}

    fn push_block(&self, block: Arc<ContextBlock>) {
        self.state.write().expect("poison").blocks.push(block);
        self.notify.notify_waiters();
    }

    fn set_kind(&self, kind: AgentStateKind) {
        self.state.write().expect("poison").kind = kind;
        self.notify.notify_waiters();
    }

    fn fail(&self, error: anyhow::Error) {
        self.set_kind(AgentStateKind::Error(FailedInferenceResponse {
            partial_response: PendingInferenceResponse::default(),
            attempt_count: NonZeroU64::MIN,
            error: Arc::new(error.to_string()),
        }));
    }
}

enum ClaudeLoopEvent {
    Control(Option<ClaudeControl>),
    Protocol(anyhow::Result<Option<rho_claude::ClaudeEvent>>),
}

pub fn transcript_messages_to_context(
    messages: &[rho_claude::SessionMessage],
) -> anyhow::Result<Vec<Arc<ContextBlock>>> {
    messages
        .iter()
        .filter_map(transcript_message_to_context)
        .collect()
}

fn assistant_message_to_block(
    message: rho_claude::protocol::AssistantMessage,
) -> anyhow::Result<Arc<ContextBlock>> {
    let message = rho_claude::SessionMessage {
        kind: rho_claude::SessionMessageKind::Assistant,
        uuid: message
            .uuid
            .and_then(|uuid| Uuid::parse_str(&uuid).ok())
            .unwrap_or_else(Uuid::new_v4),
        session_id: message.session_id.unwrap_or_else(Uuid::new_v4),
        message: serde_json::to_value(message.message)?,
        parent_tool_use_id: message.parent_tool_use_id,
        timestamp: None,
    };
    let mut blocks = transcript_messages_to_context(&[message])?;
    blocks
        .pop()
        .context("assistant message projected no blocks")
}

fn user_output_to_block(
    message: rho_claude::protocol::UserOutputMessage,
) -> anyhow::Result<Option<Arc<ContextBlock>>> {
    let Some(output) = message.message else {
        return Ok(None);
    };
    let message = rho_claude::SessionMessage {
        kind: rho_claude::SessionMessageKind::User,
        uuid: message
            .uuid
            .and_then(|uuid| Uuid::parse_str(&uuid).ok())
            .unwrap_or_else(Uuid::new_v4),
        session_id: message.session_id.unwrap_or_else(Uuid::new_v4),
        message: serde_json::to_value(output)?,
        parent_tool_use_id: message.parent_tool_use_id,
        timestamp: None,
    };
    Ok(transcript_messages_to_context(&[message])?.pop())
}

fn transcript_message_to_context(
    message: &rho_claude::SessionMessage,
) -> Option<anyhow::Result<Arc<ContextBlock>>> {
    match message.kind {
        rho_claude::SessionMessageKind::User => Some(project_user_message(message)),
        rho_claude::SessionMessageKind::Assistant => Some(project_assistant_message(message)),
        rho_claude::SessionMessageKind::System => None,
    }
}

fn project_user_message(message: &rho_claude::SessionMessage) -> anyhow::Result<Arc<ContextBlock>> {
    let mut text = String::new();
    let mut results = Vec::new();
    for content in message_content(&message.message) {
        match content.get("type").and_then(Value::as_str) {
            Some("text") => push_text(&mut text, content),
            Some("tool_result") => {
                if let Some(result) = project_tool_result(content)? {
                    results.push(result);
                }
            }
            _ => {}
        }
    }
    if !results.is_empty() {
        return Ok(Arc::new(ContextBlock::ToolResults { results }));
    }
    Ok(Arc::new(ContextBlock::UserMessage {
        content: vec![ContentPart::Text { text }],
    }))
}

fn project_assistant_message(
    message: &rho_claude::SessionMessage,
) -> anyhow::Result<Arc<ContextBlock>> {
    let mut items = Vec::new();
    let mut text = String::new();
    for content in message_content(&message.message) {
        match content.get("type").and_then(Value::as_str) {
            Some("text") => push_text(&mut text, content),
            Some("thinking") => {
                flush_text(&mut text, &mut items);
                if let Some(thinking) = content.get("thinking").and_then(Value::as_str) {
                    items.push(InferenceResponseItem::RawReasoning {
                        content: thinking.to_owned(),
                        summary: Vec::new(),
                    });
                }
            }
            Some("tool_use") => {
                flush_text(&mut text, &mut items);
                items.push(project_tool_call(content)?);
            }
            _ => {}
        }
    }
    flush_text(&mut text, &mut items);
    Ok(Arc::new(ContextBlock::InferenceResponse {
        items,
        provider_response_id: None,
    }))
}

fn message_content(message: &Value) -> Vec<&Value> {
    match message.get("content") {
        Some(Value::Array(content)) => content.iter().collect(),
        Some(Value::String(_)) => vec![message],
        _ => Vec::new(),
    }
}

fn push_text(output: &mut String, content: &Value) {
    if let Some(text) = content
        .get("text")
        .or_else(|| content.get("content"))
        .and_then(Value::as_str)
    {
        output.push_str(text);
    }
}

fn flush_text(text: &mut String, items: &mut Vec<InferenceResponseItem>) {
    if text.is_empty() {
        return;
    }
    items.push(InferenceResponseItem::AssistantMessage {
        content: vec![ContentPart::Text {
            text: std::mem::take(text),
        }],
        phase: None,
    });
}

fn project_tool_call(content: &Value) -> anyhow::Result<InferenceResponseItem> {
    let id = content
        .get("id")
        .and_then(Value::as_str)
        .context("Claude tool_use missing id")?;
    let name = content
        .get("name")
        .and_then(Value::as_str)
        .context("Claude tool_use missing name")?;
    let input = content.get("input").cloned().unwrap_or(Value::Null);
    Ok(InferenceResponseItem::ToolCall {
        id: ToolCallId::try_from(id)?,
        name: ToolName::try_from(name)?,
        tool_type: ToolType::Function,
        arguments: serde_json::to_string(&input)?,
    })
}

fn project_tool_result(content: &Value) -> anyhow::Result<Option<ToolResult>> {
    let Some(tool_use_id) = content.get("tool_use_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    let output = match content.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        Some(other) => serde_json::to_string(other)?,
        None => String::new(),
    };
    let status = if content
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        ToolOutputStatus::Error
    } else {
        ToolOutputStatus::Success
    };
    Ok(Some(ToolResult {
        call_id: ToolCallId::try_from(tool_use_id)?,
        tool_type: ToolType::Function,
        body: ToolOutput {
            output: Arc::new(output),
            status,
        },
        started_at: UnixMs(0),
        finished_at: UnixMs(0),
        metadata: None,
    }))
}

#[cfg(test)]
mod tests {
    use rho_core::text_content;
    use serde_json::json;

    use super::*;

    fn session_message(
        kind: rho_claude::SessionMessageKind,
        message: Value,
    ) -> rho_claude::SessionMessage {
        rho_claude::SessionMessage {
            kind,
            uuid: uuid::uuid!("00000000-0000-4000-8000-000000000001"),
            session_id: uuid::uuid!("00000000-0000-4000-8000-000000000002"),
            message,
            parent_tool_use_id: None,
            timestamp: None,
        }
    }

    #[test]
    fn projects_user_text() {
        let blocks = transcript_messages_to_context(&[session_message(
            rho_claude::SessionMessageKind::User,
            json!({"role": "user", "content": [{"type": "text", "text": "hello"}]}),
        )])
        .unwrap();

        let ContextBlock::UserMessage { content } = blocks[0].as_ref() else {
            panic!("expected user message");
        };
        assert_eq!(text_content(content), "hello");
    }

    #[test]
    fn projects_assistant_text_and_tool_call() {
        let blocks = transcript_messages_to_context(&[session_message(
            rho_claude::SessionMessageKind::Assistant,
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "I'll check."},
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "pwd"}},
                ],
            }),
        )])
        .unwrap();

        let ContextBlock::InferenceResponse { items, .. } = blocks[0].as_ref() else {
            panic!("expected inference response");
        };
        assert!(
            matches!(&items[0], InferenceResponseItem::AssistantMessage { content, .. } if text_content(content) == "I'll check.")
        );
        assert!(
            matches!(&items[1], InferenceResponseItem::ToolCall { id, name, arguments, .. }
            if id.as_ref() == "toolu_1" && name.as_ref() == "Bash" && arguments.contains("pwd"))
        );
    }
}
