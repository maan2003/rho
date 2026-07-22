//! Claude Code agent support.
//!
//! `rho-claude` owns the Claude Code protocol. This module owns the projection
//! from Claude protocol/transcript messages into Rho agent vocabulary.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write as _;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use anyhow::Context as _;
use async_stream::stream;
use camino::Utf8PathBuf;
use futures::Stream;
use rho_claude::{ClaudeCode, ClaudeCodeOptions, Effort, Model, Session};
use rho_core::{ContentPart, ContextBlock, ContextItemEvent, PendingInferenceResponse};
use rho_db::RhoDb;
use tokio::sync::{Notify, mpsc, oneshot};
use uuid::Uuid;

use crate::db::{
    AgentId, AgentProfileWriteTxnExt, AgentReadTxnExt, AgentRole, AgentRuntime, AgentWriteTxnExt,
    SessionBinding, UnixMillis, WorkstreamId,
};
use crate::multi_agent_tools::MultiAgentTools;
use crate::{
    AgentState, AgentStateKind, FailedInferenceResponse, InputQueues, MessageDelivery, QueuedItem,
    QueuedItemKind, StartWorkdir, system_prompt,
};

mod projection;

use projection::{
    ClaudeStreamItem, assistant_message_to_block, transcript_messages_to_context,
    user_output_to_block,
};

use crate::lazy::Lazy;

#[derive(Clone)]
pub struct ClaudeAgent {
    state: Arc<RwLock<AgentState>>,
    control: mpsc::UnboundedSender<ClaudeControl>,
    notify: Arc<Notify>,
    input_seq: Arc<AtomicU64>,
    wait_baseline_seq: Arc<AtomicU64>,
    input_notify: Arc<Notify>,
}

impl ClaudeAgent {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn create(
        db: RhoDb,
        workstream: WorkstreamId,
        display_name: Option<String>,
        start: Vec<StartWorkdir>,
        mode: SessionBinding,
        role: AgentRole,
        parent: Option<AgentId>,
        pool: std::sync::Weak<crate::pool::AgentPool>,
    ) -> anyhow::Result<(AgentId, Self)> {
        let model = mode
            .claude_model()
            .ok_or_else(|| anyhow::anyhow!("cannot create Claude runtime for Rho agent mode"))?;
        let effort = mode
            .claude_effort()
            .ok_or_else(|| anyhow::anyhow!("cannot create Claude runtime for Rho agent mode"))?;
        let mut write = db.write().await;
        let agent_id = write.alloc_agent_id();
        let entries = crate::materialize_workdirs(start).await?;
        let view = rho_workspaces::View::new(entries.clone())?;
        let session_id = Uuid::new_v4();
        write.create_agent(
            UnixMillis::now(),
            agent_id,
            workstream,
            display_name,
            entries
                .iter()
                .map(|workspace| workspace.info().clone())
                .collect(),
            mode,
            AgentRuntime::Claude { session_id },
            parent,
        );
        write.set_agent_role(agent_id, role);
        write.commit();

        let multi_agent = pool
            .upgrade()
            .map(|_| MultiAgentTools::new(pool, agent_id, parent));
        let state = AgentState {
            blocks: Vec::new(),
            queued_inputs: InputQueues::default(),
            kind: AgentStateKind::Idle,
            context_used: None,
        };
        Ok((
            agent_id,
            Self::new(
                Arc::new(Lazy::ready(view)),
                model,
                effort,
                session_id,
                state,
                ClaudeStartMode::New,
                multi_agent,
                role,
            ),
        ))
    }

    pub(crate) async fn load(
        db: RhoDb,
        agent_id: AgentId,
        view: Arc<Lazy<Arc<rho_workspaces::View>>>,
        pool: std::sync::Weak<crate::pool::AgentPool>,
    ) -> anyhow::Result<Self> {
        let record = db.read().get_agent(agent_id);
        let AgentRuntime::Claude { session_id } = record.runtime else {
            anyhow::bail!("cannot load Rho agent with the Claude agent runtime");
        };
        let model = record
            .binding
            .claude_model()
            .ok_or_else(|| anyhow::anyhow!("Claude runtime stored with non-Claude agent mode"))?;
        let effort = record
            .binding
            .claude_effort()
            .ok_or_else(|| anyhow::anyhow!("Claude runtime stored with non-Claude agent mode"))?;
        let primary = record.primary_workdir();
        let messages = rho_claude::read_session_messages_by_id(
            session_id,
            primary.repo(),
            rho_claude::SessionMessagesOptions::default(),
        )
        .await?;
        let blocks = transcript_messages_to_context(&messages)?;
        let context_used =
            rho_claude::read_session_context_used_by_id(session_id, primary.repo()).await?;
        let state = AgentState {
            blocks,
            queued_inputs: InputQueues::default(),
            kind: AgentStateKind::Idle,
            context_used,
        };
        Ok(Self::new(
            view,
            model,
            effort,
            session_id,
            state,
            ClaudeStartMode::Resume,
            pool.upgrade()
                .map(|_| MultiAgentTools::new(pool, agent_id, record.parent_agent)),
            record.role,
        ))
    }

    #[expect(clippy::too_many_arguments)]
    fn new(
        view: Arc<Lazy<Arc<rho_workspaces::View>>>,
        model: Model,
        effort: Effort,
        session_id: Uuid,
        state: AgentState,
        start_mode: ClaudeStartMode,
        multi_agent: Option<MultiAgentTools>,
        role: crate::db::AgentRole,
    ) -> Self {
        let state = Arc::new(RwLock::new(state));
        let notify = Arc::new(Notify::new());
        let input_seq = Arc::new(AtomicU64::new(0));
        let wait_baseline_seq = Arc::new(AtomicU64::new(0));
        let input_notify = Arc::new(Notify::new());
        let (control, control_rx) = mpsc::unbounded_channel();
        let loop_state = ClaudeLoop {
            view,
            model,
            effort,
            session_id,
            start_mode,
            process: None,
            claude_prompt_path: None,
            pending_response: PendingInferenceResponse::default(),
            stream_items: BTreeMap::new(),
            queued_turns: VecDeque::new(),
            turn_usage: None,
            state: Arc::clone(&state),
            notify: Arc::clone(&notify),
            wait_baseline_seq: Arc::clone(&wait_baseline_seq),
            input_notify: Arc::clone(&input_notify),
            control_rx,
            multi_agent,
            role,
        };
        tokio::spawn(loop_state.run());
        Self {
            state,
            control,
            notify,
            input_seq,
            wait_baseline_seq,
            input_notify,
        }
    }

    pub fn state(&self) -> AgentState {
        self.state.read().expect("poison").clone()
    }

    pub fn send_user_message(&self, text: impl Into<String>) {
        let seq = self.input_seq.fetch_add(1, Ordering::AcqRel) + 1;
        let uuid = Uuid::new_v4().to_string();
        self.input_notify.notify_waiters();
        let _ = self.control.send(ClaudeControl::UserMessage {
            text: text.into(),
            seq,
            uuid,
        });
    }

    pub async fn wait_for_input(&self, timeout: std::time::Duration) -> bool {
        tokio::time::timeout(timeout, async {
            loop {
                let notified = self.input_notify.notified();
                let baseline = self.wait_baseline_seq.load(Ordering::Acquire);
                let current = self.input_seq.load(Ordering::Acquire);
                if baseline != 0 && current != baseline {
                    self.wait_baseline_seq.store(current, Ordering::Release);
                    return;
                }
                notified.await;
            }
        })
        .await
        .is_ok()
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
    UserMessage {
        text: String,
        seq: u64,
        uuid: String,
    },
    SetEffort {
        effort: Effort,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    Cancel,
}

struct ClaudeLoop {
    view: Arc<Lazy<Arc<rho_workspaces::View>>>,
    model: Model,
    effort: Effort,
    session_id: Uuid,
    start_mode: ClaudeStartMode,
    process: Option<ClaudeCode>,
    claude_prompt_path: Option<tempfile::TempPath>,
    pending_response: PendingInferenceResponse,
    stream_items: BTreeMap<usize, ClaudeStreamItem>,
    queued_turns: VecDeque<ClaudeTurn>,
    /// Usage of the in-flight message: `message_start` seeds it,
    /// `message_delta` overlays the final counts (`message_start`'s
    /// `input_tokens` is a streaming placeholder). Snapshots are taken as-is,
    /// never accumulated — stream-json repeats usage per content block.
    turn_usage: Option<rho_claude::protocol::TokenUsage>,
    state: Arc<RwLock<AgentState>>,
    notify: Arc<Notify>,
    wait_baseline_seq: Arc<AtomicU64>,
    input_notify: Arc<Notify>,
    control_rx: mpsc::UnboundedReceiver<ClaudeControl>,
    multi_agent: Option<MultiAgentTools>,
    role: crate::db::AgentRole,
}

struct ClaudeTurn {
    uuid: String,
    input_seq: u64,
    content: Arc<Vec<ContentPart>>,
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
                    ClaudeLoopEvent::Control(None) => {
                        self.remove_claude_runtime_files();
                        return;
                    }
                    ClaudeLoopEvent::Protocol(event) => match *event {
                        Ok(Some(event)) => self.handle_event(event).await,
                        Ok(None) => {
                            self.process = None;
                            self.remove_claude_runtime_files();
                            // Unechoed sends died with the process; a stale
                            // entry here would pin every later turn end in
                            // the streaming state (the rail's lamp never
                            // settles).
                            self.queued_turns.clear();
                            // An exit without a result leaves the turn open;
                            // settle it as an error so the turn end is
                            // observable (attention, parent mail).
                            let mid_turn = matches!(
                                self.state.read().expect("poison").kind,
                                AgentStateKind::ApiStreaming { .. }
                            );
                            if mid_turn {
                                self.fail(anyhow::anyhow!(
                                    "Claude Code exited before finishing the turn"
                                ));
                            }
                        }
                        Err(error) => {
                            self.process = None;
                            self.remove_claude_runtime_files();
                            self.queued_turns.clear();
                            self.fail(error);
                        }
                    },
                }
            } else {
                let Some(control) = self.control_rx.recv().await else {
                    self.remove_claude_runtime_files();
                    return;
                };
                self.handle_control(control).await;
            }
        }
    }

    async fn handle_control(&mut self, control: ClaudeControl) {
        match control {
            ClaudeControl::UserMessage { text, seq, uuid } => {
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
                let content = Arc::new(vec![ContentPart::Text { text: text.clone() }]);
                self.queued_turns.push_back(ClaudeTurn {
                    uuid: uuid.clone(),
                    input_seq: seq,
                    content: Arc::clone(&content),
                });
                self.state
                    .write()
                    .expect("poison")
                    .queued_inputs
                    .push(QueuedItem {
                        kind: QueuedItemKind::UserMessage {
                            sender: crate::MessageSender::User,
                            content,
                            source_id: None,
                        },
                        delivery,
                    });
                self.notify.notify_waiters();
                // A turn-opening send starts the turn now: waiting for the
                // CLI's first stream event (seconds on a cold spawn) leaves
                // the agent looking idle while it is working.
                if !busy {
                    self.pending_response = PendingInferenceResponse::default();
                    self.stream_items.clear();
                    self.set_streaming_kind();
                }
                if let Err(error) = self
                    .process
                    .as_mut()
                    .unwrap()
                    .send_user_message_with_uuid(text, uuid)
                    .await
                {
                    self.fail(error);
                }
            }
            ClaudeControl::SetEffort { effort, reply } => {
                let _ = reply.send(self.set_effort(effort).await);
            }
            ClaudeControl::Cancel => {
                if let Some(process) = self.process.take() {
                    self.remove_claude_runtime_files();
                    tokio::spawn(async move {
                        let _ = process.close().await;
                    });
                }
                self.state.write().expect("poison").queued_inputs.clear();
                self.queued_turns.clear();
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
                rho_claude::ClaudeEvent::Other => {}
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
                // Claude agents never queue tool updates.
                QueuedItem {
                    kind: QueuedItemKind::Compaction | QueuedItemKind::ToolUpdate(_),
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
        let view = Arc::clone(self.view.get().await?);
        let session = match self.start_mode {
            ClaudeStartMode::New => Session::New {
                session_id: self.session_id,
            },
            ClaudeStartMode::Resume => Session::Resume {
                session_id: self.session_id,
            },
        };
        let mut options = ClaudeCodeOptions::new(
            view.primary().repo().to_owned(),
            self.model,
            self.effort,
            self.session_id,
        );
        options.session = session;
        if let Some(tools) = &self.multi_agent {
            options.set_env("RHO_AGENT_ID", tools.self_id().encoded());
            options.set_env("RHO_MCP_AGENT_ID", tools.display_id(tools.self_id()));
        }
        let file_mounts = self.write_claude_prompt_mount(&view)?.into_iter().collect();
        let mut command = match options.command().await {
            Ok(command) => command,
            Err(error) => {
                self.remove_claude_runtime_files();
                return Err(error);
            }
        };
        if let Err(error) = view.prepare_command(&mut command, None, file_mounts).await {
            self.remove_claude_runtime_files();
            return Err(error);
        }
        match ClaudeCode::spawn_command(command).await {
            Ok(process) => self.process = Some(process),
            Err(error) => {
                self.remove_claude_runtime_files();
                return Err(error);
            }
        }
        self.start_mode = ClaudeStartMode::Resume;
        Ok(())
    }

    fn write_claude_prompt_mount(
        &mut self,
        view: &rho_workspaces::View,
    ) -> anyhow::Result<Option<(Utf8PathBuf, Utf8PathBuf)>> {
        // A view whose entries are all live checkouts has no private mount
        // namespace to bind the generated prompt into.
        if view
            .entries()
            .iter()
            .all(|workspace| workspace.is_user_checkout())
        {
            eprintln!(
                "rho-agent: not bind-mounting generated CLAUDE.md for Claude live-checkout view"
            );
            return Ok(None);
        }
        let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("home directory not found"))?;
        let target = Utf8PathBuf::try_from(home)
            .context("home directory path is not valid UTF-8")?
            .join(".claude")
            .join("CLAUDE.md");
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create Claude config directory {parent}"))?;
        }
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("create Claude prompt bind target {target}"));
            }
        }
        let prompt = system_prompt::claude_prompt(self.multi_agent.as_ref(), self.role);
        let mut source_file = tempfile::Builder::new()
            .prefix("rho-claude-prompt-")
            .suffix(".md")
            .tempfile()
            .context("create generated Claude prompt tempfile")?;
        source_file
            .write_all(prompt.as_bytes())
            .context("write generated Claude prompt tempfile")?;
        source_file
            .flush()
            .context("flush generated Claude prompt tempfile")?;
        let source = Utf8PathBuf::try_from(source_file.path().to_owned())
            .context("generated Claude prompt tempfile path is not valid UTF-8")?;
        self.claude_prompt_path = Some(source_file.into_temp_path());
        Ok(Some((source, target)))
    }

    fn remove_claude_md(&mut self) {
        let Some(path) = self.claude_prompt_path.take() else {
            return;
        };
        let display_path = path.to_path_buf();
        match path.close() {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                eprintln!(
                    "rho-agent: remove generated Claude prompt {}: {error}",
                    display_path.display()
                )
            }
        }
    }

    fn remove_claude_runtime_files(&mut self) {
        self.remove_claude_md();
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
            rho_claude::ClaudeEvent::User(message) => {
                let promoted_queued = self.activate_turn_from_user_echo(message.uuid.as_deref());
                if promoted_queued {
                    return;
                }
                match user_output_to_block(message) {
                    Ok(Some(block)) => self.handle_user_block(block),
                    Ok(None) => {}
                    Err(error) => self.fail(error),
                }
            }
            rho_claude::ClaudeEvent::Result(message) => {
                if message.is_error {
                    self.fail(anyhow::anyhow!("{}", message.errors.join("\n")));
                } else {
                    // A child's finished turn is its report: mail the result
                    // to the parent so it can react.
                    let final_text = message.result.unwrap_or_default();
                    self.mail_parent(
                        if final_text.is_empty() {
                            "(turn finished with no text response)".to_owned()
                        } else {
                            final_text
                        },
                        MessageDelivery::NextRequest,
                    );
                    // Queued sends run next inside the CLI: staying in the
                    // streaming state avoids a false turn end between them.
                    if self.queued_turns.is_empty() {
                        self.set_kind(AgentStateKind::Idle);
                    } else {
                        self.pending_response = PendingInferenceResponse::default();
                        self.stream_items.clear();
                        self.set_streaming_kind();
                    }
                    if let Some(view) = self.view.get_if_ready() {
                        let view = Arc::clone(view);
                        tokio::spawn(async move {
                            if let Err(error) = view.snapshot().await {
                                eprintln!("rho-agent Claude snapshot failed: {error:#}");
                            }
                        });
                    }
                }
            }
            rho_claude::ClaudeEvent::StreamEvent(event) => {
                if let Err(error) = self.handle_stream_event(event.event) {
                    self.fail(error);
                }
            }
            rho_claude::ClaudeEvent::RateLimitEvent => {}
            rho_claude::ClaudeEvent::CommandLifecycle(message) => {
                self.handle_command_lifecycle(message);
            }
            rho_claude::ClaudeEvent::Other => {}
        }
    }

    fn handle_command_lifecycle(&mut self, message: rho_claude::protocol::CommandLifecycleMessage) {
        match message.state.as_str() {
            "queued" | "started" => {}
            "completed" | "cancelled" | "discarded" => {
                let Some(index) = self
                    .queued_turns
                    .iter()
                    .position(|turn| turn.uuid == message.command_uuid)
                else {
                    return;
                };
                let turn = self
                    .queued_turns
                    .remove(index)
                    .expect("index came from position");

                if message.state == "completed" {
                    self.wait_baseline_seq
                        .store(turn.input_seq, Ordering::Release);
                    let mut state = self.state.write().expect("poison");
                    promote_queued_user_message(&mut state, &turn.content);
                } else {
                    self.state
                        .write()
                        .expect("poison")
                        .queued_inputs
                        .remove_first(|queued| match queued {
                            QueuedItem {
                                kind: QueuedItemKind::UserMessage { content, .. },
                                ..
                            } => **content == *turn.content,
                            _ => false,
                        });
                }
                self.input_notify.notify_waiters();
                self.notify.notify_waiters();

                // Claude emits `completed` after the command's result. If a
                // missing replay echo left this command in our mirror, the
                // result kept the agent streaming; the lifecycle terminal is
                // the final authoritative opportunity to settle it.
                if message.state == "completed" && self.queued_turns.is_empty() {
                    self.set_kind(AgentStateKind::Idle);
                }
            }
            state => {
                eprintln!(
                    "rho-agent: unknown Claude command_lifecycle state {state:?} for {}",
                    message.command_uuid
                );
            }
        }
    }

    async fn persist_inference_block(&self, _block: &Arc<ContextBlock>) {}

    fn activate_turn_from_user_echo(&mut self, uuid: Option<&str>) -> bool {
        let Some(uuid) = uuid else { return false };
        let Some(index) = self.queued_turns.iter().position(|turn| turn.uuid == uuid) else {
            return false;
        };
        let turn = self
            .queued_turns
            .remove(index)
            .expect("index came from position");
        self.wait_baseline_seq
            .store(turn.input_seq, Ordering::Release);

        let mut state = self.state.write().expect("poison");
        promote_queued_user_message(&mut state, &turn.content);
        drop(state);

        self.input_notify.notify_waiters();
        self.notify.notify_waiters();
        true
    }

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

    /// Mail the parent agent, if any (fire-and-forget).
    fn mail_parent(&self, body: String, delivery: MessageDelivery) {
        if let Some(multi_agent) = &self.multi_agent {
            multi_agent.mail_parent(body, delivery);
        }
    }

    fn fail(&self, error: anyhow::Error) {
        // A silently stuck child is the failure mode worth surfacing: errors
        // wake the parent.
        self.mail_parent(
            format!("Agent hit an error and stopped: {error}"),
            MessageDelivery::NextRequest,
        );
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
                let Some(item) = ClaudeStreamItem::from_content_block(content_block)? else {
                    return Ok(());
                };
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
            | rho_claude::protocol::MessageStreamEvent::Ping
            | rho_claude::protocol::MessageStreamEvent::Other => {}
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
            kind: QueuedItemKind::Compaction | QueuedItemKind::ToolUpdate(_),
            ..
        } => true,
    });
}

fn promote_queued_user_message(state: &mut AgentState, content: &[ContentPart]) -> bool {
    let matched = state.queued_inputs.remove_first(|queued| {
        matches!(
            queued,
            QueuedItem {
                kind: QueuedItemKind::UserMessage { .. },
                ..
            }
        )
    });
    if matched.is_none() {
        return false;
    }
    state.blocks.push(Arc::new(ContextBlock::UserMessage {
        sender: crate::MessageSender::User,
        content: content.to_vec(),
    }));
    true
}

fn is_compact_command(content: &[ContentPart]) -> bool {
    match content {
        [ContentPart::Text { text }] => text.trim() == "/compact",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(text: &str) -> Arc<Vec<ContentPart>> {
        Arc::new(vec![ContentPart::Text {
            text: text.to_owned(),
        }])
    }

    #[test]
    fn promotes_queued_user_message_from_uuid_matched_turn_content() {
        let mut state = AgentState {
            blocks: Vec::new(),
            queued_inputs: InputQueues::default(),
            kind: AgentStateKind::Idle,
            context_used: None,
        };
        state.queued_inputs.push(QueuedItem {
            kind: QueuedItemKind::UserMessage {
                sender: crate::MessageSender::User,
                content: text("claude-normalized text"),
                source_id: None,
            },
            delivery: MessageDelivery::Immediate,
        });
        let turn_content = vec![ContentPart::Text {
            text: "original text".to_owned(),
        }];

        assert!(promote_queued_user_message(&mut state, &turn_content));

        assert!(state.queued_inputs.is_empty());
        assert_eq!(
            state.blocks,
            vec![Arc::new(ContextBlock::UserMessage {
                sender: crate::MessageSender::User,
                content: turn_content,
            })]
        );
    }
}
