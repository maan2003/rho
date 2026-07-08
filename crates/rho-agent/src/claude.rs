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
use tokio::sync::{Notify, mpsc, oneshot};
use uuid::Uuid;

use crate::db::{
    AgentId, AgentMode, AgentReadTxnExt, AgentRuntime, AgentWriteTxnExt, TopicId, UnixMillis,
};
use crate::{
    AgentState, AgentStateKind, FailedInferenceResponse, InputQueues, MessageDelivery, QueuedItem,
    QueuedItemKind, StartWorkspace, system_prompt,
};

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
                repo.create_workspace(workspace_id, &parent_revset).await?
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
            AgentRuntime::Claude { session_id },
            None,
        );
        write.commit();

        let state = AgentState {
            blocks: Vec::new(),
            tool_specs: Arc::from([]),
            system_prompt: system_prompt::prompt(workspace.as_ref(), None, None),
            queued_inputs: InputQueues::default(),
            kind: AgentStateKind::Idle,
            context_used: None,
        };
        Ok((
            agent_id,
            Self::new(
                workspace,
                model,
                effort,
                session_id,
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
        let AgentRuntime::Claude { session_id } = record.runtime else {
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
        let messages = rho_claude::read_session_messages_by_id(
            session_id,
            workspace.repo(),
            rho_claude::SessionMessagesOptions::default(),
        )
        .await?;
        let blocks = transcript_messages_to_context(&messages)?;
        let context_used =
            rho_claude::read_session_context_used_by_id(session_id, workspace.repo()).await?;
        let state = AgentState {
            blocks,
            tool_specs: Arc::from([]),
            system_prompt: system_prompt::prompt(workspace.as_ref(), None, None),
            queued_inputs: InputQueues::default(),
            kind: AgentStateKind::Idle,
            context_used,
        };
        Ok(Self::new(
            workspace,
            model,
            effort,
            session_id,
            state,
            ClaudeStartMode::Resume,
        ))
    }

    fn new(
        workspace: Arc<rho_workspaces::Workspace>,
        model: Model,
        effort: Effort,
        session_id: Uuid,
        state: AgentState,
        start_mode: ClaudeStartMode,
    ) -> Self {
        let state = Arc::new(RwLock::new(state));
        let notify = Arc::new(Notify::new());
        let (control, control_rx) = mpsc::unbounded_channel();
        let loop_state = ClaudeLoop {
            workspace,
            model,
            effort,
            session_id,
            start_mode,
            process: None,
            pending_response: PendingInferenceResponse::default(),
            stream_items: BTreeMap::new(),
            turn_usage: None,
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

    pub fn compact(&self) {
        self.send_user_message("/compact");
    }

    pub async fn set_effort(&self, effort: Effort) -> anyhow::Result<()> {
        let (reply, result) = oneshot::channel();
        self.control
            .send(ClaudeControl::SetEffort { effort, reply })
            .map_err(|_| anyhow::anyhow!("Claude agent control loop is closed"))?;
        result
            .await
            .map_err(|_| anyhow::anyhow!("Claude agent control loop is closed"))?
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
    SetEffort {
        effort: Effort,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    Cancel,
}

struct ClaudeLoop {
    workspace: Arc<rho_workspaces::Workspace>,
    model: Model,
    effort: Effort,
    session_id: Uuid,
    start_mode: ClaudeStartMode,
    process: Option<ClaudeCode>,
    pending_response: PendingInferenceResponse,
    stream_items: BTreeMap<usize, ClaudeStreamItem>,
    /// Usage of the in-flight message: `message_start` seeds it,
    /// `message_delta` overlays the final counts (`message_start`'s
    /// `input_tokens` is a streaming placeholder). Snapshots are taken as-is,
    /// never accumulated — stream-json repeats usage per content block.
    turn_usage: Option<rho_claude::protocol::TokenUsage>,
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
                        event = process.next_event() => ClaudeLoopEvent::Protocol(Box::new(event)),
                    }
                };
                match event {
                    ClaudeLoopEvent::Control(Some(control)) => self.handle_control(control).await,
                    ClaudeLoopEvent::Control(None) => return,
                    ClaudeLoopEvent::Protocol(event) => match *event {
                        Ok(Some(event)) => self.handle_event(event).await,
                        Ok(None) => self.process = None,
                        Err(error) => {
                            self.process = None;
                            self.fail(error);
                        }
                    },
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
                // Every message mirrors into the queue until its
                // --replay-user-messages echo confirms it entered context and
                // promotes it into history. Mid-turn sends wait on the CLI's
                // internal queue and show the steering label; turn-opening
                // sends render as a plain user message right away (the echo
                // can trail a cold CLI spawn by many seconds).
                let busy = matches!(
                    self.state.read().expect("poison").kind,
                    AgentStateKind::ApiStreaming { .. }
                );
                let delivery = if busy {
                    MessageDelivery::NextRequest
                } else {
                    MessageDelivery::Immediate
                };
                let content = vec![ContentPart::Text { text: text.clone() }];
                self.state
                    .write()
                    .expect("poison")
                    .queued_inputs
                    .push(QueuedItem {
                        kind: QueuedItemKind::UserMessage {
                            sender: crate::MessageSender::User,
                            content: Arc::new(content),
                        },
                        delivery,
                    });
                self.notify.notify_waiters();
                if let Err(error) = self.process.as_mut().unwrap().send_user_message(text).await {
                    self.fail(error);
                }
            }
            ClaudeControl::SetEffort { effort, reply } => {
                let _ = reply.send(self.set_effort(effort).await);
            }
            ClaudeControl::Cancel => {
                if let Some(process) = self.process.take() {
                    tokio::spawn(async move {
                        let _ = process.close().await;
                    });
                }
                self.state.write().expect("poison").queued_inputs.clear();
                self.set_kind(AgentStateKind::Idle);
            }
        }
    }

    async fn set_effort(&mut self, effort: Effort) -> anyhow::Result<()> {
        self.effort = effort;
        let Some(process) = self.process.as_mut() else {
            return Ok(());
        };
        let request_id = process.apply_effort(effort).await?;
        loop {
            let event = {
                let Some(process) = self.process.as_mut() else {
                    anyhow::bail!("Claude Code exited before applying effort");
                };
                process.next_event().await?
            };
            let Some(event) = event else {
                self.process = None;
                anyhow::bail!("Claude Code exited before applying effort");
            };
            match event {
                rho_claude::ClaudeEvent::ControlResponse(message)
                    if message.response.request_id == request_id =>
                {
                    if message.response.subtype == "success" {
                        return Ok(());
                    }
                    anyhow::bail!(
                        "{}",
                        message
                            .response
                            .error
                            .unwrap_or_else(|| "Claude Code rejected effort update".to_owned())
                    );
                }
                rho_claude::ClaudeEvent::ControlResponse(_) => {}
                event => self.handle_event(event).await,
            }
        }
    }

    /// Routes a user-output block. With --replay-user-messages the CLI echoes
    /// every user message when it enters context: an echo confirms a mirrored
    /// queued message and promotes it to history. Anything else (tool
    /// results, CLI-injected user content) passes through.
    fn handle_user_block(&mut self, block: Arc<ContextBlock>) {
        if let ContextBlock::UserMessage { content, .. } = &*block {
            let mut state = self.state.write().expect("poison");
            let matched = state.queued_inputs.remove_first(|queued| match queued {
                QueuedItem {
                    kind:
                        QueuedItemKind::UserMessage {
                            content: queued, ..
                        },
                    ..
                } => **queued == *content,
                QueuedItem {
                    kind: QueuedItemKind::Compaction,
                    ..
                } => false,
            });
            if matched.is_some() {
                state.blocks.push(block);
                drop(state);
                self.notify.notify_waiters();
                return;
            }
        }
        self.push_block(block);
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
                self.handle_system_message(message).await;
            }
            rho_claude::ClaudeEvent::ControlResponse(_) => {}
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
                Ok(Some(block)) => self.handle_user_block(block),
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

    async fn handle_system_message(&mut self, message: rho_claude::protocol::SystemMessage) {
        let rho_claude::protocol::SystemMessage::CompactBoundary {
            compact_metadata, ..
        } = message
        else {
            return;
        };

        {
            let mut state = self.state.write().expect("poison");
            remove_compact_commands(&mut state.queued_inputs);
            if let Some(post_tokens) = compact_metadata.and_then(|metadata| metadata.post_tokens) {
                state.context_used = Some(post_tokens);
            }
        }
        self.notify.notify_waiters();

        match self.refresh_from_transcript().await {
            Ok(()) => {}
            Err(error) => eprintln!("rho-agent Claude compaction refresh failed: {error:#}"),
        }
    }

    async fn refresh_from_transcript(&self) -> anyhow::Result<()> {
        let messages = rho_claude::read_session_messages_by_id(
            self.session_id,
            self.workspace.repo(),
            rho_claude::SessionMessagesOptions::default(),
        )
        .await?;
        let blocks = transcript_messages_to_context(&messages)?;
        let context_used =
            rho_claude::read_session_context_used_by_id(self.session_id, self.workspace.repo())
                .await?;
        {
            let mut state = self.state.write().expect("poison");
            state.blocks = blocks;
            if context_used.is_some() {
                state.context_used = context_used;
            }
        }
        self.notify.notify_waiters();
        Ok(())
    }

    fn push_block(&self, block: Arc<ContextBlock>) {
        self.state.write().expect("poison").blocks.push(block);
        self.notify.notify_waiters();
    }

    fn set_kind(&self, kind: AgentStateKind) {
        self.state.write().expect("poison").kind = kind;
        self.notify.notify_waiters();
    }

    /// Publishes the in-flight message's usage as context occupancy.
    fn update_context_used(&self) {
        let Some(usage) = &self.turn_usage else {
            return;
        };
        self.state.write().expect("poison").context_used = Some(usage.context_total());
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
            rho_claude::protocol::MessageStreamEvent::MessageStart { message } => {
                self.pending_response = PendingInferenceResponse::default();
                self.stream_items.clear();
                self.turn_usage = message.usage;
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
            rho_claude::protocol::MessageStreamEvent::MessageDelta { delta: _, usage } => {
                if let Some(usage) = usage {
                    match &mut self.turn_usage {
                        Some(turn_usage) => merge_usage(turn_usage, usage),
                        None => self.turn_usage = Some(usage),
                    }
                }
                self.update_context_used();
            }
            rho_claude::protocol::MessageStreamEvent::MessageStop
            | rho_claude::protocol::MessageStreamEvent::Ping => {}
        }
        Ok(())
    }
}

enum ClaudeLoopEvent {
    Control(Option<ClaudeControl>),
    Protocol(Box<anyhow::Result<Option<rho_claude::ClaudeEvent>>>),
}

/// Overlays the fields a later usage snapshot reports onto an earlier one,
/// keeping earlier values for fields the update omits.
fn merge_usage(
    base: &mut rho_claude::protocol::TokenUsage,
    update: rho_claude::protocol::TokenUsage,
) {
    base.input_tokens = update.input_tokens.or(base.input_tokens);
    base.output_tokens = update.output_tokens.or(base.output_tokens);
    base.cache_creation_input_tokens = update
        .cache_creation_input_tokens
        .or(base.cache_creation_input_tokens);
    base.cache_read_input_tokens = update
        .cache_read_input_tokens
        .or(base.cache_read_input_tokens);
}

fn remove_compact_commands(inputs: &mut InputQueues) {
    inputs.retain(|input| match input {
        QueuedItem {
            kind: QueuedItemKind::UserMessage { content, .. },
            ..
        } => !is_compact_command(content),
        QueuedItem {
            kind: QueuedItemKind::Compaction,
            ..
        } => true,
    });
}

fn is_compact_command(content: &[ContentPart]) -> bool {
    match content {
        [ContentPart::Text { text }] => text.trim() == "/compact",
        _ => false,
    }
}
