//! Claude Code agent support.
//!
//! `rho-claude` owns the Claude Code protocol. This module owns the projection
//! from Claude protocol/transcript messages into Rho agent vocabulary.

use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::{Arc, RwLock};

use async_stream::stream;
use futures::Stream;
use rho_claude::{ClaudeCode, ClaudeCodeOptions, Effort, Model, Session};
use rho_core::{ContentPart, ContextBlock, ContextItemEvent, PendingInferenceResponse};
use rho_db::RhoDb;
use tokio::sync::{Notify, mpsc};
use uuid::Uuid;

use crate::db::{
    AgentId, AgentMode, AgentReadTxnExt, AgentRuntime, AgentWriteTxnExt, TopicId, UnixMillis,
};
use crate::{AgentState, AgentStateKind, FailedInferenceResponse, StartWorkspace, system_prompt};

mod projection;

pub use projection::transcript_messages_to_context;
use projection::{ClaudeStreamItem, assistant_message_to_block, user_output_to_block};

#[derive(Clone)]
pub struct ClaudeAgent {
    state: Arc<RwLock<AgentState>>,
    control: mpsc::UnboundedSender<ClaudeControl>,
    notify: Arc<Notify>,
}

impl ClaudeAgent {
    pub async fn create(
        db: RhoDb,
        topic_id: TopicId,
        display_name: Option<String>,
        start: StartWorkspace,
        mode: AgentMode,
    ) -> anyhow::Result<(AgentId, Self)> {
        let model = mode
            .claude_model()
            .ok_or_else(|| anyhow::anyhow!("cannot create Claude runtime for Rho agent mode"))?;
        let effort = mode
            .claude_effort()
            .ok_or_else(|| anyhow::anyhow!("cannot create Claude runtime for Rho agent mode"))?;
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
            mode,
            AgentRuntime::Claude {
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
                effort,
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
        let AgentRuntime::Claude {
            session_id,
            transcript_path,
        } = record.runtime
        else {
            anyhow::bail!("cannot load Rho agent with the Claude agent runtime");
        };
        let model = record
            .mode
            .claude_model()
            .ok_or_else(|| anyhow::anyhow!("Claude runtime stored with non-Claude agent mode"))?;
        let effort = record
            .mode
            .claude_effort()
            .ok_or_else(|| anyhow::anyhow!("Claude runtime stored with non-Claude agent mode"))?;
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
            effort,
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
        effort: Effort,
        session_id: Uuid,
        transcript_path: Option<camino::Utf8PathBuf>,
        state: AgentState,
        start_mode: ClaudeStartMode,
    ) -> Self {
        let state = Arc::new(RwLock::new(state));
        let notify = Arc::new(Notify::new());
        let (control, control_rx) = mpsc::unbounded_channel();
        let loop_state = ClaudeLoop {
            db,
            agent_id,
            workspace,
            model,
            effort,
            session_id,
            transcript_path,
            start_mode,
            process: None,
            pending_response: PendingInferenceResponse::default(),
            stream_items: BTreeMap::new(),
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
    effort: Effort,
    session_id: Uuid,
    transcript_path: Option<camino::Utf8PathBuf>,
    start_mode: ClaudeStartMode,
    process: Option<ClaudeCode>,
    pending_response: PendingInferenceResponse,
    stream_items: BTreeMap<usize, ClaudeStreamItem>,
    state: Arc<RwLock<AgentState>>,
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
                self.pending_response = PendingInferenceResponse::default();
                self.stream_items.clear();
                let content = vec![ContentPart::Text { text: text.clone() }];
                self.push_block(Arc::new(ContextBlock::UserMessage {
                    content: content.clone(),
                }));
                self.set_kind(AgentStateKind::ApiStreaming {
                    pending_response: self.pending_response.clone(),
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
            self.effort,
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
                        self.pending_response = PendingInferenceResponse::default();
                        self.stream_items.clear();
                        self.persist_inference_block(&block).await;
                        self.push_block(block);
                        self.set_streaming_kind();
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
            rho_claude::ClaudeEvent::StreamEvent(event) => {
                if let Err(error) = self.handle_stream_event(event.event) {
                    self.fail(error);
                }
            }
            rho_claude::ClaudeEvent::RateLimitEvent => {}
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

    fn set_streaming_kind(&self) {
        self.set_kind(AgentStateKind::ApiStreaming {
            pending_response: self.pending_response.clone(),
            previous_attempt: None,
        });
    }

    fn fail(&self, error: anyhow::Error) {
        self.set_kind(AgentStateKind::Error(FailedInferenceResponse {
            partial_response: self.pending_response.clone(),
            attempt_count: NonZeroU64::MIN,
            error: Arc::new(error.to_string()),
        }));
    }

    fn handle_stream_event(
        &mut self,
        event: rho_claude::protocol::MessageStreamEvent,
    ) -> anyhow::Result<()> {
        match event {
            rho_claude::protocol::MessageStreamEvent::MessageStart { .. } => {
                self.pending_response = PendingInferenceResponse::default();
                self.stream_items.clear();
                self.set_streaming_kind();
            }
            rho_claude::protocol::MessageStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                let item = ClaudeStreamItem::from_content_block(content_block)?;
                self.pending_response.apply(
                    index,
                    ContextItemEvent::Update(item.to_streaming_context_item()?),
                );
                self.stream_items.insert(index, item);
                self.set_streaming_kind();
            }
            rho_claude::protocol::MessageStreamEvent::ContentBlockDelta { index, delta } => {
                if let Some(item) = self.stream_items.get_mut(&index) {
                    item.apply_delta(delta)?;
                    self.pending_response.apply(
                        index,
                        ContextItemEvent::Update(item.to_streaming_context_item()?),
                    );
                    self.set_streaming_kind();
                }
            }
            rho_claude::protocol::MessageStreamEvent::ContentBlockStop { index } => {
                self.pending_response.apply(index, ContextItemEvent::Finish);
                self.set_streaming_kind();
            }
            rho_claude::protocol::MessageStreamEvent::Error { error } => {
                anyhow::bail!(
                    "{}",
                    error
                        .message
                        .or(error.error_type)
                        .unwrap_or_else(|| "Claude stream error".to_owned())
                );
            }
            rho_claude::protocol::MessageStreamEvent::MessageDelta { .. }
            | rho_claude::protocol::MessageStreamEvent::MessageStop
            | rho_claude::protocol::MessageStreamEvent::Ping => {}
        }
        Ok(())
    }
}

enum ClaudeLoopEvent {
    Control(Option<ClaudeControl>),
    Protocol(anyhow::Result<Option<rho_claude::ClaudeEvent>>),
}
