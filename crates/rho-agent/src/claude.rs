//! Claude Code agent support.
//!
//! `rho-claude` owns the Claude Code protocol. This module owns the projection
//! from Claude protocol/transcript messages into Rho agent vocabulary.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::Write as _;
use std::num::NonZeroU64;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Context as _;
use camino::Utf8PathBuf;
use rho_agent_types::ContentPart;
use rho_claude::{ClaudeCode, ClaudeCodeOptions, Effort, Model, SdkMcpServer, Session};
use rho_inference::Inference;
use rho_inference::types::{ContextItemEvent, PendingInferenceResponse};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::db::{
    AgentId, AgentRole, AgentRoleSessionProfile as _, AgentRuntime, ClaudeRewind,
    EngineerIntelligence, UnixMillis,
};
use crate::{
    AgentEvent, AgentState, AgentStateKind, AgentStatus, FailedInferenceResponse, InputKind,
    InputQueues, MessageDelivery, QueuedInput, TranscriptLine, prompt,
};

pub(crate) mod projection;
pub(crate) mod python_host;

use projection::{ClaudeStreamItem, Projection, assistant_row, compacted_row, user_row};

use crate::lazy::Lazy;

#[derive(Clone)]
pub struct ClaudeAgent {
    status: Arc<RwLock<AgentStatus>>,
    control: mpsc::UnboundedSender<ClaudeControl>,
    head: Arc<RwLock<crate::db::AgentHead>>,
}

impl ClaudeAgent {
    #[expect(clippy::too_many_arguments)]
    fn new(
        host: Arc<crate::worker::Host>,
        inference: Inference,
        claude: rho_claude::accounts::ClaudePaths,
        agent_id: AgentId,
        view: Arc<Lazy<Arc<crate::View>>>,
        model: Model,
        effort: Effort,
        session_id: Uuid,
        state: AgentState,
        start_mode: ClaudeStartMode,
        pending_rewind: bool,
        pending_output: Option<crate::ClaudeOutputBatch>,
        role: crate::db::AgentRole,
        head: crate::db::AgentHead,
    ) -> (Self, ClaudeLoop) {
        let status = Arc::new(RwLock::new(AgentStatus {
            kind: state.kind.clone(),
            queued: state.queued_inputs.len(),
        }));
        let head = Arc::new(RwLock::new(head));
        let (control, control_rx) = mpsc::unbounded_channel();
        host.observe(&status);
        let loop_state = ClaudeLoop {
            name_updates: host.names(),
            host,
            claude,
            inference,
            agent_id,
            view: Arc::clone(&view),
            model,
            effort,
            session_id,
            start_mode,
            process: None,
            claude_prompt_path: None,
            claude_settings_path: None,
            claude_account: None,
            python: None,
            pending_output,
            python_wake: None,
            python_recheck: None,
            pending_response: PendingInferenceResponse::default(),
            stream_items: BTreeMap::new(),
            response_execs: BTreeMap::new(),
            queued_turns: VecDeque::new(),
            turn_usage: None,
            cancelling: false,
            pending_rewind,
            execution_generation: 0,
            state,
            status: Arc::clone(&status),
            head: Arc::clone(&head),
            control_rx,
            role,
            projection: Projection::default(),
        };
        (
            Self {
                status,
                control,
                head,
            },
            loop_state,
        )
    }

    /// The agent's view, ready once its place is.

    pub fn status(&self) -> AgentStatus {
        self.status.read().expect("poison").clone()
    }

    /// The record as of the loop's last change to it.

    /// A user message carried the pending notice: it is not said again.
    /// The log agrees once the message's row is in it.
    pub fn notice_carried(&self) {
        self.head.write().expect("poison").pending_notice = None;
    }

    /// Say the whole tail again: a client just started looking.
    pub fn tell_tail(&self) {
        let _ = self.control.send(ClaudeControl::TellTail);
    }

    pub fn send_user_message(&self, text: impl Into<String>) {
        self.send_user_content(vec![ContentPart::Text { text: text.into() }]);
    }

    pub fn send_user_content(&self, content: Vec<ContentPart>) {
        let uuid = Uuid::new_v4().to_string();
        let _ = self.control.send(ClaudeControl::UserMessage {
            content,
            uuid,
            accepted: None,
        });
    }

    pub async fn send_user_content_accepted(
        &self,
        content: Vec<ContentPart>,
    ) -> anyhow::Result<()> {
        self.send_content_accepted(content).await
    }

    /// Deliver agent mail and wait for acceptance into Rho's volatile Claude
    /// queue. A process or daemon restart may lose it before Claude records it.
    pub async fn send_agent_message_accepted(&self, text: String) -> anyhow::Result<()> {
        self.send_content_accepted(vec![ContentPart::Text { text }])
            .await
    }

    async fn send_content_accepted(&self, content: Vec<ContentPart>) -> anyhow::Result<()> {
        let uuid = Uuid::new_v4().to_string();
        let (accepted, reply) = oneshot::channel();
        self.control
            .send(ClaudeControl::UserMessage {
                content,
                uuid,
                accepted: Some(accepted),
            })
            .map_err(|_| anyhow::anyhow!("Claude agent stopped before accepting mail"))?;
        reply
            .await
            .map_err(|_| anyhow::anyhow!("Claude agent stopped before accepting mail"))?
    }

    pub fn compact(&self) {
        self.send_user_message("/compact");
    }

    pub(crate) async fn retire(&self) -> anyhow::Result<()> {
        let (reply, result) = oneshot::channel();
        self.control
            .send(ClaudeControl::Retire(reply))
            .map_err(|_| anyhow::anyhow!("agent loop is closed"))?;
        result
            .await
            .map_err(|_| anyhow::anyhow!("agent loop is closed"))?
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

    pub async fn change_role(&self, role: AgentRole) -> anyhow::Result<()> {
        let (reply, result) = oneshot::channel();
        self.control
            .send(ClaudeControl::ChangeRole { role, reply })
            .map_err(|_| anyhow::anyhow!("Claude agent control loop is closed"))?;
        result
            .await
            .map_err(|_| anyhow::anyhow!("Claude agent control loop is closed"))?
    }

    pub fn cancel(&self) {
        let _ = self.control.send(ClaudeControl::Cancel);
    }

    pub async fn rewind(&self, turns: u32) -> anyhow::Result<()> {
        let (reply, result) = oneshot::channel();
        self.control
            .send(ClaudeControl::Rewind { turns, reply })
            .map_err(|_| anyhow::anyhow!("Claude agent control loop is closed"))?;
        result
            .await
            .map_err(|_| anyhow::anyhow!("Claude agent control loop is closed"))?
    }
}

#[derive(Clone, Copy)]
enum ClaudeStartMode {
    New,
    Resume,
    Fork {
        source_session_id: Uuid,
        resume_at: Uuid,
    },
}

enum ClaudeControl {
    Retire(oneshot::Sender<anyhow::Result<()>>),
    UserMessage {
        content: Vec<ContentPart>,
        uuid: String,
        accepted: Option<oneshot::Sender<anyhow::Result<()>>>,
    },
    SetEffort {
        effort: Effort,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    ChangeRole {
        role: AgentRole,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    Cancel,
    Rewind {
        turns: u32,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    /// Tell the live tail whole, for a client that just started looking.
    TellTail,
}

pub(crate) struct ClaudeLoop {
    /// The Claude configuration this agent runs against, handed down from
    /// the daemon rather than resolved here.
    claude: rho_claude::accounts::ClaudePaths,
    inference: Inference,
    agent_id: AgentId,
    view: Arc<Lazy<Arc<crate::View>>>,
    /// The primary workdir's repo, which is where Claude files the
    /// session. Known without materializing the view.
    model: Model,
    effort: Effort,
    session_id: Uuid,
    start_mode: ClaudeStartMode,
    process: Option<ClaudeCode>,
    claude_prompt_path: Option<tempfile::TempPath>,
    /// The generated `settings.json` of a Python-mode agent, kept alive for
    /// the same reason as the prompt.
    claude_settings_path: Option<tempfile::TempPath>,
    /// The account the running process was spawned on, so a switch is
    /// noticed at the next turn.
    claude_account: Option<String>,
    /// Whether this agent's only tool is Rho's Python notebook, served to
    /// Claude Code in-process over MCP.
    /// The notebook, once the first spawn has built it. Outlives the
    /// process: cells keep running across a respawn.
    python: Option<python_host::PythonHost>,
    pending_output: Option<crate::ClaudeOutputBatch>,
    /// Why the notebook last spoke, until the transcript row it produced
    /// arrives to carry it.
    python_wake: Option<crate::WakeFacts>,
    /// When the boundary said to ask it again, if it can change by itself.
    python_recheck: Option<rho_agent_types::UnixMs>,
    pending_response: PendingInferenceResponse,
    stream_items: BTreeMap<usize, ClaudeStreamItem>,
    response_execs: BTreeMap<usize, rho_inference::types::ExecId>,
    queued_turns: VecDeque<ClaudeTurn>,
    /// Usage of the in-flight message: `message_start` seeds it,
    /// `message_delta` overlays the final counts (`message_start`'s
    /// `input_tokens` is a streaming placeholder). Snapshots are taken as-is,
    /// never accumulated — stream-json repeats usage per content block.
    turn_usage: Option<rho_claude::protocol::TokenUsage>,
    cancelling: bool,
    pending_rewind: bool,
    execution_generation: u64,
    /// The loop's own transcript and queue; readers get `status`.
    state: AgentState,
    status: Arc<RwLock<AgentStatus>>,
    head: Arc<RwLock<crate::db::AgentHead>>,
    /// What clients have been told of the tail, so each publish says only
    /// what changed.
    control_rx: mpsc::UnboundedReceiver<ClaudeControl>,
    /// The loop's own address, for the tasks it starts (the sidecar, the
    /// file watch).
    host: Arc<crate::worker::Host>,
    name_updates: tokio::sync::watch::Receiver<Option<crate::db::AgentHead>>,
    role: crate::db::AgentRole,
    /// What the projection of Claude's log keeps from one line to the
    /// next: usage already told, calls awaiting their result's times.
    projection: Projection,
}

struct ClaudeTurn {
    uuid: String,
    content: Arc<Vec<ContentPart>>,
}

impl ClaudeLoop {
    pub(crate) async fn shutdown(&mut self) -> anyhow::Result<()> {
        self.forget_pending_exec();
        let python = async {
            if let Some(python) = &mut self.python {
                python.shutdown().await?;
            }
            Ok::<(), anyhow::Error>(())
        };
        let process = async {
            if let Some(process) = self.process.take() {
                process.close().await?;
            }
            Ok::<(), anyhow::Error>(())
        };
        let (python, process) = tokio::join!(python, process);
        python?;
        process
    }
    pub(crate) async fn load(
        agent_id: AgentId,
        host: Arc<crate::worker::Host>,
        inference: Inference,
        claude: rho_claude::accounts::ClaudePaths,
        view: Arc<Lazy<Arc<crate::View>>>,
    ) -> anyhow::Result<(ClaudeAgent, Self)> {
        let record = host.head().await?;
        let AgentRuntime::Claude { session_id } = record.config.runtime else {
            anyhow::bail!("cannot load Rho agent with the Claude agent runtime");
        };
        let model =
            record.config.binding.claude_model().ok_or_else(|| {
                anyhow::anyhow!("Claude runtime stored with non-Claude agent mode")
            })?;
        let effort =
            record.config.binding.claude_effort().ok_or_else(|| {
                anyhow::anyhow!("Claude runtime stored with non-Claude agent mode")
            })?;
        let primary_repo = record.place().cwd.clone();
        // A moved agent's file is where its old place looked, until it is
        // brought here; found nowhere, the session is one never spoken to.
        let mut sessions = vec![session_id];
        if let Some(rewind) = &record.config.claude_rewind {
            sessions.push(rewind.session_id);
            sessions.push(rewind.source_session_id);
        }
        for session in sessions {
            if let Err(error) =
                rho_claude::relocate_session_transcript(&claude.projects(), session, &primary_repo)
                    .await
            {
                eprintln!(
                    "rho-agent: Claude session {session} of {} stays where it was: {error:#}",
                    agent_id.encoded()
                );
            }
        }
        // The transcript's rows come from the file when the loop starts
        // (`sync_transcript`); a load reads the file only to settle a
        // rewind that was cut short.
        let (session_id, start_mode, pending_rewind) = if let Some(rewind) =
            record.config.claude_rewind
        {
            let resumed = rho_claude::read_session_messages_by_id(
                &claude.projects(),
                rewind.session_id,
                &primary_repo,
                rho_claude::SessionMessagesOptions::default(),
            )
            .await?;
            let materialized = match rewind.resume_at {
                Some(resume_at) => {
                    rho_claude::session_messages_through_assistant(&resumed, resume_at).is_some()
                }
                None => !resumed.is_empty(),
            };
            if materialized {
                host.complete_claude_rewind(rewind.session_id).await?;
                (rewind.session_id, ClaudeStartMode::Resume, false)
            } else {
                // A hard-killed fork can leave a partial JSONL that reserves
                // its session id without containing the copied boundary.
                // Rotate the pending destination before retrying.
                let session_id = Uuid::new_v4();
                let rewind = ClaudeRewind {
                    session_id,
                    ..rewind
                };
                host.claude_rewind(UnixMillis::now(), None, Some(rewind.clone()))
                    .await?;
                let source = rho_claude::read_session_messages_by_id(
                    &claude.projects(),
                    rewind.source_session_id,
                    &primary_repo,
                    rho_claude::SessionMessagesOptions::default(),
                )
                .await?;
                if let Some(resume_at) = rewind.resume_at {
                    anyhow::ensure!(
                        rho_claude::session_messages_through_assistant(&source, resume_at)
                            .is_some(),
                        "Claude rewind point is no longer in the transcript"
                    );
                }
                let start_mode = match rewind.resume_at {
                    Some(resume_at) => ClaudeStartMode::Fork {
                        source_session_id: rewind.source_session_id,
                        resume_at,
                    },
                    None => ClaudeStartMode::New,
                };
                (session_id, start_mode, true)
            }
        } else {
            // A file for the session means it has been spoken to.
            let start_mode = match rho_claude::find_session_transcript(
                &claude.projects(),
                session_id,
                &primary_repo,
            )
            .await?
            {
                Some(_) => ClaudeStartMode::Resume,
                None => ClaudeStartMode::New,
            };
            (session_id, start_mode, false)
        };
        let state = AgentState {
            blocks: Vec::new(),
            queued_inputs: InputQueues::default(),
            kind: AgentStateKind::Idle,
            context_used: None,
            total_usage: host.usage_total().await?,
            usage_provider: match model {
                rho_claude::Model::Opus => crate::db::AgentUsageModel::OPUS,
                rho_claude::Model::Fable | rho_claude::Model::Sonnet => {
                    crate::db::AgentUsageModel::FABLE
                }
            },
        };
        let pending_output = host.claude_pending_output().await?;
        let head = host.head().await?;
        Ok(ClaudeAgent::new(
            host,
            inference,
            claude,
            agent_id,
            view,
            model,
            effort,
            session_id,
            state,
            start_mode,
            pending_rewind,
            pending_output,
            record.config.role,
            head,
        ))
    }
    fn apply_name(&mut self, named: crate::db::AgentHead) {
        let mut head = self.head.write().expect("poison");
        head.generated_title = named.generated_title;
        head.title_attempted = named.title_attempted;
    }

    pub(crate) async fn run(&mut self) -> anyhow::Result<()> {
        loop {
            let initial_kind = self.state.kind.clone();
            let initial_execution_generation = self.execution_generation;
            if self.process.is_some() {
                let notify = self.python.as_ref().map(|host| host.notify());
                let recheck = self.python_recheck;
                let event = {
                    let process = self.process.as_mut().expect("checked above");
                    let control_rx = &mut self.control_rx;
                    tokio::select! {
                        biased;
                        changed = self.name_updates.changed() => {
                            changed.context("daemon naming connection closed")?;
                            let named = self.name_updates.borrow_and_update().clone();
                            if let Some(named) = named { self.apply_name(named); self.published(); }
                            continue;
                        }
                        control = control_rx.recv() => ClaudeLoopEvent::Control(control),
                        event = process.next_event() => ClaudeLoopEvent::Protocol(Box::new(event)),
                        _ = python_wake(notify.as_deref(), recheck) => ClaudeLoopEvent::PythonWake,
                    }
                };
                match event {
                    ClaudeLoopEvent::PythonWake => {}
                    ClaudeLoopEvent::Control(Some(control)) => self.handle_control(control).await?,
                    ClaudeLoopEvent::Control(None) => return Ok(()),
                    ClaudeLoopEvent::Protocol(event) => match *event {
                        Ok(Some(event)) => self.handle_event(event).await?,
                        Ok(None) => {
                            self.process = None;
                            self.forget_pending_exec();
                            self.recover_pending_rewind().await?;
                            // Unechoed sends died with the process; a stale
                            // entry here would pin every later turn end in
                            // the streaming state (the rail's lamp never
                            // settles).
                            self.queued_turns.clear();
                            // An exit without a result leaves the turn open;
                            // settle it as an error so the turn end is
                            // observable (attention, parent mail).
                            let mid_turn =
                                matches!(self.state.kind, AgentStateKind::ApiStreaming { .. });
                            if mid_turn {
                                self.fail(anyhow::anyhow!(
                                    "Claude Code exited before finishing the turn"
                                ))
                                .await?;
                            }
                        }
                        Err(error) => {
                            self.process = None;
                            self.forget_pending_exec();
                            self.recover_pending_rewind().await?;
                            self.queued_turns.clear();
                            self.fail(error).await?;
                        }
                    },
                }
                // Every event may have changed what the notebook's cells
                // have to say or whether the model can hear it; ask once.
                self.python_tick().await?;
            } else {
                let control = tokio::select! {
                    control = self.control_rx.recv() => control,
                    changed = self.name_updates.changed() => {
                            changed.context("daemon naming connection closed")?;
                        let named = self.name_updates.borrow_and_update().clone();
                        if let Some(named) = named { self.apply_name(named); self.published(); }
                        continue;
                    }
                };
                let Some(control) = control else {
                    return Ok(());
                };
                self.handle_control(control).await?;
            }
            let kind = self.state.kind.clone();
            if let Some(edge) = crate::turn_edge(
                &initial_kind,
                &kind,
                self.execution_generation != initial_execution_generation,
            ) {
                self.host.turn(UnixMillis::now(), edge).await?;
            }
            if crate::execution_settled(
                &initial_kind,
                &kind,
                self.execution_generation != initial_execution_generation,
            ) {
                self.host.settled().await?;
                self.published();
            }
        }
    }

    async fn handle_control(&mut self, control: ClaudeControl) -> anyhow::Result<()> {
        match control {
            ClaudeControl::Retire(reply) => {
                if (AgentStatus {
                    kind: self.state.kind.clone(),
                    queued: self.state.queued_inputs.len(),
                })
                .settled()
                {
                    let _ = reply.send(Ok(()));
                    // Freeze scheduling and admission at this serialized boundary.
                    // The outer driver cancels this future on daemon disconnect.
                    std::future::pending::<()>().await;
                } else {
                    let _ = reply.send(Err(anyhow::anyhow!("agent still has work")));
                }
            }

            ClaudeControl::UserMessage {
                mut content,
                uuid,
                accepted,
            } => {
                if !matches!(content.first(), Some(ContentPart::Text { text }) if text.trim_start().starts_with('/'))
                {
                    let named = self
                        .host
                        .name(&rho_inference::types::text_content(&content))
                        .await?;
                    self.apply_name(named);
                }
                // Keep CLI slash commands intact. Any retained output remains
                // queued until the command finishes and the CLI is idle.
                let slash_command = matches!(content.first(), Some(ContentPart::Text { text }) if text.trim_start().starts_with('/'));
                let output = self.pending_output.as_ref().filter(|_| !slash_command);
                let output_id = output.map(|batch| batch.id);
                if let Some(batch) = output {
                    let mut combined = batch.message();
                    combined.append(&mut content);
                    content = combined;
                }
                self.cancelling = false;
                if let Some(host) = &mut self.python {
                    host.user_spoke();
                }
                let busy = self.state.kind.is_working();
                if !busy {
                    self.execution_generation = self.execution_generation.wrapping_add(1);
                }
                // Every message mirrors into the queue until its
                // --replay-user-messages echo confirms it entered context and
                // promotes it into history. Mid-turn sends wait on the CLI's
                // internal queue and show the steering label; turn-opening
                // sends render as a plain user message right away (the echo
                // can trail a cold CLI spawn by many seconds).
                let delivery = if busy {
                    MessageDelivery::NextRequest
                } else {
                    MessageDelivery::Immediate
                };
                let content = Arc::new(content);
                let input = QueuedInput {
                    source: crate::MessageSender::User,
                    kind: InputKind::Message {
                        content: (*content).clone(),
                    },
                    delivery,
                    at: rho_agent_types::UnixMs::now(),
                };
                // The queue is Claude Code's, in its process: no row says
                // a message waits (nothing would persist it across a
                // restart); the live queue does, and the echo's own row is
                // the message going in.
                if let Err(error) = self.ensure_process().await {
                    if let Some(accepted) = accepted {
                        let _ = accepted.send(Err(anyhow::anyhow!("{error:#}")));
                    }
                    self.fail(error).await?;
                    return Ok(());
                }
                self.queued_turns.push_back(ClaudeTurn {
                    uuid: uuid.clone(),
                    content: Arc::clone(&content),
                });
                self.state.queued_inputs.push(input);
                self.published();
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
                    .send_user_content_with_uuid((*content).clone(), uuid)
                    .await
                {
                    if let Some(accepted) = accepted {
                        let _ = accepted.send(Err(anyhow::anyhow!("{error:#}")));
                    }
                    self.fail(error).await?;
                } else {
                    if let Some(id) = output_id {
                        self.output_handed_off(id).await?;
                    }
                    if let Some(accepted) = accepted {
                        let _ = accepted.send(Ok(()));
                    }
                }
            }
            ClaudeControl::SetEffort { effort, reply } => {
                let result = self.set_effort(effort).await;
                if result
                    .as_ref()
                    .is_err_and(|error| error.is::<crate::worker::StoreError>())
                {
                    return result;
                }
                let _ = reply.send(result);
            }
            ClaudeControl::ChangeRole { role, reply } => {
                let result = self.change_role(role).await;
                if result
                    .as_ref()
                    .is_err_and(|error| error.is::<crate::worker::StoreError>())
                {
                    return result;
                }
                let _ = reply.send(result);
            }
            ClaudeControl::Cancel => {
                let kind = self.state.kind.clone();
                let busy = matches!(kind, AgentStateKind::ApiStreaming { .. });
                let queued = self
                    .queued_turns
                    .iter()
                    .map(|turn| turn.uuid.clone())
                    .collect::<Vec<_>>();
                self.state.queued_inputs.clear();
                self.queued_turns.clear();
                self.cancelling = busy;
                self.cancel_python().await?;
                if busy && self.process.is_some() {
                    let result =
                        tokio::time::timeout(Duration::from_secs(30), self.soft_cancel(&queued))
                            .await;
                    if !matches!(result, Ok(Ok(()))) {
                        if let Ok(Err(error)) = result {
                            if error.is::<crate::worker::StoreError>() {
                                return Err(error);
                            }
                            eprintln!("rho-agent: Claude soft cancel failed: {error:#}");
                        } else {
                            eprintln!("rho-agent: Claude soft cancel timed out");
                        }
                        self.close_process().await?;
                    }
                } else if matches!(kind, AgentStateKind::Error(_)) {
                    self.close_process().await?;
                }
                self.cancelling = false;
                self.pending_response = PendingInferenceResponse::default();
                self.stream_items.clear();
                self.set_kind(AgentStateKind::Idle);
                self.recover_pending_rewind().await?;
            }
            ClaudeControl::Rewind { turns, reply } => {
                let result = self.rewind(turns).await;
                if result
                    .as_ref()
                    .is_err_and(|error| error.is::<crate::worker::StoreError>())
                {
                    return result;
                }
                let _ = reply.send(result);
            }
            ClaudeControl::TellTail => {
                self.host.tell_tail();
                self.published();
            }
        }
        Ok(())
    }

    async fn close_process(&mut self) -> anyhow::Result<()> {
        self.forget_pending_exec();
        if let Some(process) = self.process.take() {
            process.close().await?;
        }
        Ok(())
    }

    async fn soft_cancel(&mut self, queued: &[String]) -> anyhow::Result<()> {
        let mut cancel_ids = std::collections::HashSet::new();
        for uuid in queued {
            let request_id = self
                .process
                .as_mut()
                .context("Claude Code exited while cancelling queued input")?
                .cancel_async_message(uuid)
                .await?;
            cancel_ids.insert(request_id);
        }
        // Queue cancellations are written first so the CLI cannot begin a
        // surviving queued command in the gap after interrupt processing.
        let interrupt_id = self
            .process
            .as_mut()
            .context("Claude Code process is not running")?
            .interrupt()
            .await?;
        let mut interrupt_done = false;
        let mut idle = false;

        loop {
            let event = self
                .process
                .as_mut()
                .context("Claude Code exited while cancelling")?
                .next_event()
                .await?
                .context("Claude Code exited while cancelling")?;
            match event {
                rho_claude::ClaudeEvent::ControlResponse(message)
                    if message.response.request_id == interrupt_id =>
                {
                    if message.response.subtype != "success" {
                        anyhow::bail!(
                            "{}",
                            message
                                .response
                                .error
                                .unwrap_or_else(|| "Claude Code rejected interrupt".to_owned())
                        );
                    }
                    interrupt_done = true;
                    // The interrupt receipt precedes the interrupted turn's
                    // result/idle. Any idle drained before this barrier can be
                    // a lagging trailer from the preceding turn.
                    idle = false;
                    let still_queued = message
                        .response
                        .response
                        .as_ref()
                        .and_then(|response| response.get("still_queued"))
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(serde_json::Value::as_str)
                        .filter(|uuid| queued.iter().any(|queued| queued == uuid))
                        .map(str::to_owned)
                        .collect::<Vec<_>>();
                    for uuid in still_queued {
                        let request_id = self
                            .process
                            .as_mut()
                            .context("Claude Code exited while reconciling interrupt receipt")?
                            .cancel_async_message(&uuid)
                            .await?;
                        cancel_ids.insert(request_id);
                    }
                }
                rho_claude::ClaudeEvent::ControlResponse(message)
                    if cancel_ids.remove(&message.response.request_id) =>
                {
                    if message.response.subtype != "success" {
                        anyhow::bail!(
                            "{}",
                            message.response.error.unwrap_or_else(|| {
                                "Claude Code rejected queued-message cancellation".to_owned()
                            })
                        );
                    }
                }
                rho_claude::ClaudeEvent::System(
                    rho_claude::protocol::SystemMessage::SessionStateChanged { state, .. },
                ) => {
                    idle |= interrupt_done && state.as_deref() == Some("idle");
                }
                rho_claude::ClaudeEvent::ControlResponse(_) => {}
                event => self.handle_event(event).await?,
            }
            if interrupt_done && cancel_ids.is_empty() && idle {
                return Ok(());
            }
        }
    }

    async fn set_effort(&mut self, effort: Effort) -> anyhow::Result<()> {
        self.effort = effort;
        let Some(process) = self.process.as_mut() else {
            return Ok(());
        };
        let request_id = process.apply_effort(effort).await?;
        self.await_control_response(request_id, "Claude Code rejected effort update")
            .await?;
        Ok(())
    }

    async fn change_role(&mut self, requested: AgentRole) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(
                self.state.kind,
                AgentStateKind::Idle | AgentStateKind::Error(_)
            ),
            "role changes are only available while idle or errored; cancel the turn first"
        );
        anyhow::ensure!(
            self.state.queued_inputs.is_empty() && self.queued_turns.is_empty(),
            "role changes are not available with queued inputs"
        );

        let requested = match requested {
            AgentRole::Engineer { intelligence } => intelligence,
            _ => anyhow::bail!("role changes currently support only med1-eng and high1-eng"),
        };
        anyhow::ensure!(
            matches!(
                requested,
                EngineerIntelligence::High1 | EngineerIntelligence::Medium1
            ),
            "role changes currently support only med1-eng and high1-eng"
        );

        let role = match self.role {
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::High1 | EngineerIntelligence::Medium1,
            } => AgentRole::Engineer {
                intelligence: requested,
            },
            _ => anyhow::bail!("role changes currently support only med1-eng and high1-eng"),
        };
        if role == self.role {
            return Ok(());
        }

        let binding = role.session_profile();
        let model = binding
            .claude_model()
            .ok_or_else(|| anyhow::anyhow!("role change would leave the Claude runtime"))?;
        let effort = binding
            .claude_effort()
            .ok_or_else(|| anyhow::anyhow!("role change has no Claude effort"))?;

        self.close_process().await?;
        self.host.profile(role, binding).await?;
        {
            let mut head = self.head.write().expect("poison");
            head.config.role = role;
            head.config.binding = binding;
        }
        self.model = model;
        self.effort = effort;
        self.role = role;
        Ok(())
    }

    async fn await_control_response(
        &mut self,
        request_id: String,
        fallback_error: &str,
    ) -> anyhow::Result<rho_claude::protocol::ControlResponse> {
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
                        return Ok(message.response);
                    }
                    anyhow::bail!(
                        "{}",
                        message
                            .response
                            .error
                            .unwrap_or_else(|| fallback_error.to_owned())
                    );
                }
                rho_claude::ClaudeEvent::ControlResponse(_) => {}
                event => self.handle_event(event).await?,
            }
        }
    }

    async fn rewind(&mut self, turns: u32) -> anyhow::Result<()> {
        anyhow::ensure!(turns > 0, ":rewind turns must be greater than zero");
        anyhow::ensure!(
            matches!(
                self.state.kind,
                AgentStateKind::Idle | AgentStateKind::Error(_)
            ),
            ":rewind is only available while idle or errored; use :cancel first"
        );
        anyhow::ensure!(
            self.state.queued_inputs.is_empty() && self.queued_turns.is_empty(),
            ":rewind is not available with queued inputs"
        );

        let view = Arc::clone(self.view.get().await?);
        let (source_session_id, messages) = if self.pending_rewind {
            match self.start_mode {
                ClaudeStartMode::Fork {
                    source_session_id,
                    resume_at,
                } => {
                    let source = rho_claude::read_session_messages_by_id(
                        &self.claude.projects(),
                        source_session_id,
                        view.cwd(),
                        rho_claude::SessionMessagesOptions::default(),
                    )
                    .await?;
                    let messages =
                        rho_claude::session_messages_through_assistant(&source, resume_at)
                            .context("Claude rewind point is no longer in the transcript")?;
                    (source_session_id, messages)
                }
                ClaudeStartMode::New => (self.session_id, Vec::new()),
                ClaudeStartMode::Resume => unreachable!("pending rewind must retain its source"),
            }
        } else {
            let messages = rho_claude::read_session_messages_by_id(
                &self.claude.projects(),
                self.session_id,
                view.cwd(),
                rho_claude::SessionMessagesOptions::default(),
            )
            .await?;
            (self.session_id, messages)
        };
        let (messages, resume_at) =
            rho_claude::rewind_session_messages(&messages, turns).context("nothing to rewind")?;
        // The rows from the first line the fork leaves behind are told
        // taken back now; the fork's stream carries on from there.
        let kept = messages
            .iter()
            .map(|message| message.uuid)
            .collect::<HashSet<_>>();
        let dropped =
            self.host
                .history()
                .await?
                .1
                .into_iter()
                .find_map(|(pos, event)| match event {
                    AgentEvent::Transcript { uuid, .. } if !kept.contains(&uuid) => Some(pos),
                    _ => None,
                });
        let context_used =
            rho_claude::last_assistant_usage(&messages).map(|usage| usage.context_total());

        if let Some(process) = self.process.take() {
            process.close().await?;
        }

        let new_session_id = Uuid::new_v4();
        self.session_id = new_session_id;
        self.start_mode = match resume_at {
            Some(resume_at) => ClaudeStartMode::Fork {
                source_session_id,
                resume_at,
            },
            None => ClaudeStartMode::New,
        };
        self.host
            .claude_rewind(
                UnixMillis::now(),
                dropped,
                Some(ClaudeRewind {
                    source_session_id,
                    session_id: new_session_id,
                    resume_at,
                }),
            )
            .await?;
        self.pending_rewind = true;

        self.state.queued_inputs.clear();
        self.state.kind = AgentStateKind::Idle;
        self.state.context_used = context_used;
        self.pending_response = PendingInferenceResponse::default();
        self.stream_items.clear();
        self.turn_usage = None;
        self.published();
        Ok(())
    }

    async fn ensure_process(&mut self) -> anyhow::Result<()> {
        // The account is global and switching it moves every agent. It lands
        // here, at a turn boundary, rather than the moment it is switched:
        // the process holds a namespace with the old account mounted, and
        // killing it mid-turn would throw away the answer in flight.
        let account = self.host.claude_account().await?;
        if self.process.is_some() {
            if self.claude_account.as_deref() == Some(account.as_str()) {
                return Ok(());
            }
            eprintln!(
                "rho-agent: restarting {} on Claude account {account}",
                self.agent_id.encoded()
            );
            self.close_process().await?;
        }
        let view = Arc::clone(self.view.get().await?);
        let session = match self.start_mode {
            ClaudeStartMode::New => Session::New {
                session_id: self.session_id,
            },
            ClaudeStartMode::Resume => Session::Resume {
                session_id: self.session_id,
            },
            ClaudeStartMode::Fork {
                source_session_id,
                resume_at,
            } => Session::Fork {
                session_id: self.session_id,
                source_session_id,
                resume_at,
            },
        };
        let mut options = ClaudeCodeOptions::new(
            view.cwd().to_owned(),
            self.model,
            self.effort,
            self.session_id,
        );
        options.session = session;
        options.set_env("RHO_AGENT_ID", self.agent_id.encoded());
        self.ensure_python(&view).await?;
        // Tool search would defer the one tool behind a lookup; the deny
        // list in the generated settings removes ToolSearch too. The
        // timeout is the CLI's ceiling on an open exec call.
        options.set_env("ENABLE_TOOL_SEARCH", "false");
        options.set_env(
            "MCP_TOOL_TIMEOUT",
            rho_claude::mcp::EXEC_TIMEOUT.as_millis().to_string(),
        );
        let (account_dir, target) = self
            .configure_claude_home(&view, &mut options, &account)
            .await?;
        let mut command = options.command().await?;
        view.prepare_command(&mut command, None).await?;
        rho_claude::namespace::prepare(
            &mut command,
            &target,
            account_dir.as_std_path(),
            self.claude.projects().as_std_path(),
            self.claude_prompt_path.as_deref().expect("prompt prepared"),
            self.claude_settings_path.as_deref(),
        )?;
        self.process = Some(ClaudeCode::spawn_command(command).await?);
        if !self.pending_rewind {
            self.start_mode = ClaudeStartMode::Resume;
        }
        if self.python.is_some() {
            let request_id = self
                .process
                .as_mut()
                .expect("spawned above")
                .initialize_sdk_mcp(&[SdkMcpServer {
                    name: rho_claude::mcp::SERVER_NAME.to_owned(),
                    timeout: Some(rho_claude::mcp::EXEC_TIMEOUT),
                }])
                .await?;
            self.await_control_response(request_id, "Claude Code rejected the Python MCP server")
                .await
                .context("register the Python notebook with Claude Code")?;
        }
        Ok(())
    }

    /// Builds the notebook on the first spawn. It lives as long as the
    /// loop: a respawned CLI finds the same globals and running cells.
    async fn ensure_python(&mut self, view: &Arc<crate::View>) -> anyhow::Result<()> {
        if self.python.is_some() {
            return Ok(());
        }
        let team = self.host.team().await?;
        let (shell, others) = crate::python::host::host_tools(
            view,
            self.role,
            self.agent_id,
            Some(&self.inference),
            team.as_ref(),
            Some(&self.host),
        );
        let tool = crate::python::PythonNotebook::new(shell, others)
            .map_err(|error| anyhow::anyhow!("Python notebook failed to start: {error}"))?;
        self.python = Some(python_host::PythonHost::new(tool));
        Ok(())
    }

    /// Prepare provider-owned files; only the child launcher mounts them.
    async fn configure_claude_home(
        &mut self,
        view: &crate::View,
        options: &mut rho_claude::ClaudeCodeOptions,
        account: &str,
    ) -> anyhow::Result<(camino::Utf8PathBuf, std::path::PathBuf)> {
        let config_home = self.claude.config_home().to_owned();
        // The namespace mounts these, and a missing mount source or target
        // there fails namespace creation rather than the spawn.
        std::fs::create_dir_all(config_home.join("projects"))
            .with_context(|| format!("create Claude config directory {config_home}"))?;
        let account_dir = self.claude.prepare(account)?;
        // Claude keeps `.claude.json` (the account itself, and its
        // credentials) in `$HOME`, not in the config directory, so no mount
        // over `~/.claude` alone could switch accounts. Naming the mount
        // point as the config directory is what pulls that file inside it.
        // The value is the same for every account: only the mount underneath
        // it differs.
        let target = config_home.clone().into_std_path_buf();
        options.set_env("CLAUDE_CONFIG_DIR", target.to_string_lossy());
        let team = self.host.team().await?;
        let prompt = prompt::claude_prompt(Some(view), team.as_ref(), self.role);
        // Keep one source inode alive for the lifetime of the view namespace.
        // Unlinking a bind-mounted source makes the target pathname disappear
        // inside that namespace, so a rewrite has to reuse this file rather
        // than replace it.
        write_generated_source(
            &mut self.claude_prompt_path,
            "rho-claude-prompt-",
            ".md",
            &prompt,
        )?;
        // A Python-mode agent runs on the account's own settings with every
        // Claude tool denied, so the notebook is all the model has. The
        // generated file covers the account's `settings.json`.
        {
            let base = self.claude.account_settings(account)?;
            let settings = rho_claude::settings::deny_all_but_own_tools(&base);
            let text = serde_json::to_string_pretty(&settings)?;
            write_generated_source(
                &mut self.claude_settings_path,
                "rho-claude-settings-",
                ".json",
                &text,
            )?;
        }
        self.claude_account = Some(account.to_owned());
        Ok((account_dir, target))
    }

    async fn handle_event(&mut self, event: rho_claude::ClaudeEvent) -> anyhow::Result<()> {
        match event {
            rho_claude::ClaudeEvent::System(message) => {
                self.handle_system_message(message).await?;
            }
            rho_claude::ClaudeEvent::ControlResponse(_) => {}
            rho_claude::ClaudeEvent::ControlRequest(message) => {
                self.handle_control_request(message).await?;
            }
            // One content block, finished: its row, then the live tail
            // lets go of the streamed copy. A subagent's blocks are its
            // own.
            rho_claude::ClaudeEvent::Assistant(message) => {
                if message.parent_tool_use_id.is_some() {
                    return Ok(());
                }
                match assistant_row(&message, self.state.usage_provider, &mut self.projection) {
                    Ok(Some((uuid, line, at))) => self.tell_line(uuid, line, at).await?,
                    Ok(None) => {}
                    Err(error) => eprintln!(
                        "rho-agent: Claude message {} of {} skipped: {error:#}",
                        message.uuid.as_deref().unwrap_or("?"),
                        self.agent_id.encoded()
                    ),
                }
                self.release_block(&message.message.content);
            }
            // The echo of a send (its turn leaves the queue first), or a
            // call's results.
            rho_claude::ClaudeEvent::User(message) => {
                self.activate_turn_from_user_echo(message.uuid.as_deref());
                if message.parent_tool_use_id.is_some() || message.is_synthetic.unwrap_or(false) {
                    return Ok(());
                }
                match user_row(&message, &mut self.projection) {
                    Ok(Some((uuid, line, at))) => self.tell_line(uuid, line, at).await?,
                    Ok(None) => {}
                    Err(error) => eprintln!(
                        "rho-agent: Claude message {} of {} skipped: {error:#}",
                        message.uuid.as_deref().unwrap_or("?"),
                        self.agent_id.encoded()
                    ),
                }
            }
            rho_claude::ClaudeEvent::Result(message) => {
                let successful = !message.is_error;
                if let Some(host) = &mut self.python {
                    host.turn_ended(rho_agent_types::UnixMs::now());
                    // A turn that ends with a call still open is the CLI
                    // having given up on it (its timeout, or an abort);
                    // answer it anyway so the notebook takes the next one.
                    if let Some(pending) = host.take_pending() {
                        let reply = serde_json::json!({
                            "mcp_response": {
                                "jsonrpc": "2.0",
                                "id": pending.rpc_id,
                                "result": {
                                    "content": [{ "type": "text", "text": "the call was abandoned before the cell reported" }],
                                    "isError": true,
                                },
                            },
                        });
                        self.respond_control(&pending.request_id, Ok(reply)).await;
                    }
                }
                if self.cancelling {
                    self.pending_response = PendingInferenceResponse::default();
                    self.stream_items.clear();
                    self.set_kind(AgentStateKind::Idle);
                } else if message.is_error {
                    self.fail(anyhow::anyhow!("{}", message.errors.join("\n")))
                        .await?;
                } else {
                    let final_text = message.result.unwrap_or_default();
                    self.host.completed(final_text.clone()).await?;
                    // Queued sends run next inside the CLI: staying in the
                    // streaming state avoids a false turn end between them.
                    if self.queued_turns.is_empty() {
                        self.set_kind(AgentStateKind::Idle);
                    } else {
                        self.pending_response = PendingInferenceResponse::default();
                        self.stream_items.clear();
                        self.set_streaming_kind();
                    }
                }
                if self.pending_rewind
                    && successful
                    && self.queued_turns.is_empty()
                    && let Err(error) = self.complete_rewind().await
                {
                    if error.is::<crate::worker::StoreError>() {
                        return Err(error);
                    }
                    self.rotate_pending_rewind().await?;
                    self.fail(error.context("finalize rewound Claude session"))
                        .await?;
                }
            }
            rho_claude::ClaudeEvent::StreamEvent(event) => {
                let message_stopped = matches!(
                    &event.event,
                    rho_claude::protocol::MessageStreamEvent::MessageStop
                );
                if let Err(error) = self.handle_stream_event(event.event).await {
                    self.fail(error).await?;
                    return Ok(());
                }
                if message_stopped && !self.stream_items.is_empty() {
                    // Every block's row is in by now; whatever the tail
                    // still holds goes with the message.
                    self.stream_items.clear();
                    self.pending_response = PendingInferenceResponse::default();
                    self.set_streaming_kind();
                }
                if message_stopped && let Some(usage) = self.turn_usage.take() {
                    let turn_usage = crate::db::AgentUsageBucket {
                        model: match self.model {
                            rho_claude::Model::Opus => crate::db::AgentUsageModel::OPUS,
                            rho_claude::Model::Fable | rho_claude::Model::Sonnet => {
                                crate::db::AgentUsageModel::FABLE
                            }
                        },
                        input_tokens: usage.input_tokens.unwrap_or(0),
                        cache_read_tokens: usage.cache_read_input_tokens.unwrap_or(0),
                        cache_write_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
                        cache_write_1h_tokens: usage
                            .cache_creation
                            .as_ref()
                            .and_then(|cache| cache.ephemeral_1h_input_tokens)
                            .unwrap_or(0),
                        output_tokens: usage.output_tokens.unwrap_or(0),
                        requests: 1,
                        ..crate::db::AgentUsageBucket::default()
                    };
                    self.state.total_usage.add(&turn_usage);
                    self.host.record_usage(turn_usage).await?;
                    self.published();
                }
            }
            rho_claude::ClaudeEvent::RateLimitEvent(_) => {}
            rho_claude::ClaudeEvent::CommandLifecycle(message) => {
                self.handle_command_lifecycle(message).await?;
            }
            rho_claude::ClaudeEvent::Other => {}
        }
        Ok(())
    }

    /// A request from the CLI: with the notebook registered, its MCP
    /// traffic. The handshake and listing are answered here; an exec call
    /// is answered when the boundary says so, from `python_tick`.
    async fn handle_control_request(
        &mut self,
        message: rho_claude::protocol::ControlRequestMessage,
    ) -> anyhow::Result<()> {
        use rho_claude::protocol::ControlRequest;
        let request_id = message.request_id;
        let reply = match message.request {
            ControlRequest::McpMessage {
                server_name,
                message: rpc,
            } if server_name == rho_claude::mcp::SERVER_NAME => {
                match rho_claude::mcp::handle_rpc(&rpc) {
                    rho_claude::mcp::Rpc::Reply(value) => {
                        Ok(serde_json::json!({ "mcp_response": value }))
                    }
                    rho_claude::mcp::Rpc::Ignore => Ok(serde_json::json!({})),
                    rho_claude::mcp::Rpc::Exec {
                        id,
                        exec_id,
                        source,
                    } => {
                        let already_admitted = self.host.exec_was_admitted(exec_id.clone()).await?;
                        if self.cancelling
                            || self.response_execs.values().next() != Some(&exec_id)
                            || already_admitted
                        {
                            Err("Only the first exec in a provider response may run, once. This call was not executed; earlier side effects are not undone.".into())
                        } else if self.python.as_ref().is_none_or(|host| !host.can_admit()) {
                            Err(
                                "The notebook is unavailable or already has an open exec reply."
                                    .into(),
                            )
                        } else {
                            let now = rho_agent_types::UnixMs::now();
                            self.host
                                .append(AgentEvent::ClaudeExecAdmitted {
                                    call: rho_inference::types::ExecCall {
                                        id: exec_id.clone(),
                                        source: source.clone(),
                                    },
                                    at: now,
                                })
                                .await?;
                            match self.python.as_mut().expect("checked above").exec(
                                request_id.clone(),
                                id,
                                exec_id,
                                source,
                                now,
                            ) {
                                Some(refused) => Ok(serde_json::json!({ "mcp_response": refused })),
                                None => return Ok(()),
                            }
                        }
                    }
                }
            }
            ControlRequest::McpMessage { server_name, .. } => {
                Err(format!("unknown MCP server {server_name}"))
            }
            ControlRequest::Other => Err("unsupported control request".to_owned()),
        };
        self.respond_control(&request_id, reply).await;
        Ok(())
    }

    async fn respond_control(
        &mut self,
        request_id: &str,
        reply: Result<serde_json::Value, String>,
    ) -> bool {
        let Some(process) = self.process.as_mut() else {
            return false;
        };
        let result = match reply {
            Ok(value) => process.respond_control(request_id, value).await,
            Err(error) => process.respond_control_error(request_id, &error).await,
        };
        if let Err(error) = result {
            eprintln!(
                "rho-agent: Claude control response of {} failed: {error:#}",
                self.agent_id.encoded()
            );
            false
        } else {
            true
        }
    }

    /// Asks the boundary whether the model should hear from the notebook,
    /// and acts on the answer: an open exec call returns with everything
    /// waiting, or an idle model is woken with it as a message. A working
    /// model with no call open hears it at its next call or turn end.
    async fn python_tick(&mut self) -> anyhow::Result<()> {
        let now = rho_agent_types::UnixMs::now();
        let idle = matches!(self.state.kind, AgentStateKind::Idle)
            && self.process.is_some()
            && self.queued_turns.is_empty()
            && !self.pending_rewind;
        let Some(host) = self.python.as_mut() else {
            return Ok(());
        };
        let oldest_user = self.state.queued_inputs.iter().map(|input| input.at).min();
        let available = host.has_pending() || idle;
        match host.decide(available, oldest_user, self.pending_output.is_some(), now) {
            crate::boundary::Boundary::No { recheck } => self.python_recheck = recheck,
            crate::boundary::Boundary::AbortAndResend
            | crate::boundary::Boundary::RetryExhausted => {
                self.python_recheck = None;
            }
            crate::boundary::Boundary::Now { wake } => {
                self.python_recheck = None;
                if let Some((pending, mut drained)) = host.answer_pending() {
                    let batch = self.record_output(&mut drained, wake, now).await?;
                    let reply = serde_json::json!({
                        "mcp_response": {
                            "jsonrpc": "2.0", "id": pending.rpc_id,
                            "result": drained.into_mcp_result(),
                        },
                    });
                    self.observe_exec(
                        pending.exec_id.clone(),
                        rho_agent_types::ExecMilestone::Boundary,
                        now,
                    )
                    .await?;
                    if self.respond_control(&pending.request_id, Ok(reply)).await {
                        self.output_handed_off(batch).await?;
                        self.observe_exec(
                            pending.exec_id,
                            rho_agent_types::ExecMilestone::HandedOff,
                            rho_agent_types::UnixMs::now(),
                        )
                        .await?;
                    } else {
                        // The outbox survives both this process and the notebook. A
                        // future user send carries its output, never reruns its source.
                        self.close_process().await?;
                        self.fail(anyhow::anyhow!(
                            "Claude Code output transport failed; notebook output is retained"
                        ))
                        .await?;
                    }
                } else if idle {
                    let mut drained = host.drain_idle();
                    if drained.is_empty() && self.pending_output.is_none() {
                        return Ok(());
                    }
                    let batch = self.record_output(&mut drained, wake, now).await?;
                    self.handle_control(ClaudeControl::UserMessage {
                        content: Vec::new(),
                        uuid: batch.to_string(),
                        accepted: None,
                    })
                    .await?;
                }
            }
        }
        Ok(())
    }

    async fn record_output(
        &mut self,
        drained: &mut python_host::Drained,
        wake: crate::WakeFacts,
        at: rho_agent_types::UnixMs,
    ) -> anyhow::Result<Uuid> {
        if let Some(retained) = &self.pending_output {
            drained
                .updates
                .splice(0..0, retained.outputs.iter().cloned());
        }
        let batch = crate::ClaudeOutputBatch {
            id: Uuid::new_v4(),
            outputs: drained
                .own
                .iter()
                .chain(drained.updates.iter())
                .cloned()
                .collect(),
            wake: wake.clone(),
            at,
        };
        let id = batch.id;
        self.host
            .append(AgentEvent::ClaudeOutput {
                batch: batch.clone(),
            })
            .await?;
        self.pending_output = Some(batch);
        self.python_wake = Some(wake);
        self.python
            .as_mut()
            .expect("drained a notebook")
            .acknowledge();
        Ok(id)
    }

    async fn output_handed_off(&mut self, id: Uuid) -> anyhow::Result<()> {
        self.host
            .append(AgentEvent::ClaudeOutputHandedOff {
                id,
                at: rho_agent_types::UnixMs::now(),
            })
            .await?;
        self.pending_output = None;
        Ok(())
    }

    /// Stops the notebook's cells and answers an open exec call as
    /// cancelled, ahead of the interrupt that abandons it on the CLI's side.
    async fn cancel_python(&mut self) -> anyhow::Result<()> {
        let Some(host) = self.python.as_mut() else {
            return Ok(());
        };
        if let Some(pending) = host.cancel(rho_agent_types::UnixMs::now()) {
            let reply = serde_json::json!({
                "mcp_response": {
                    "jsonrpc": "2.0",
                    "id": pending.rpc_id,
                    "result": {
                        "content": [{ "type": "text", "text": "cancelled by the user" }],
                        "isError": true,
                    },
                },
            });
            self.respond_control(&pending.request_id, Ok(reply)).await;
        }
        Ok(())
    }

    /// The process that was waiting on an exec call is gone; the cells run
    /// on and say what they have to the next one.
    fn forget_pending_exec(&mut self) {
        if let Some(host) = self.python.as_mut() {
            host.take_pending();
        }
        self.python_recheck = None;
    }

    async fn handle_command_lifecycle(
        &mut self,
        message: rho_claude::protocol::CommandLifecycleMessage,
    ) -> anyhow::Result<()> {
        match message.state.as_str() {
            "queued" | "started" => {}
            "completed" | "cancelled" | "discarded" => {
                let Some(index) = self
                    .queued_turns
                    .iter()
                    .position(|turn| turn.uuid == message.command_uuid)
                else {
                    return Ok(());
                };
                let turn = self
                    .queued_turns
                    .remove(index)
                    .expect("index came from position");

                if message.state == "completed" {
                    promote_queued_user_message(&mut self.state);
                } else {
                    self.state
                        .queued_inputs
                        .remove_first(|queued| match &queued.kind {
                            InputKind::Message { content } => *content == *turn.content,
                            InputKind::Compaction => false,
                        });
                }
                self.published();

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
        Ok(())
    }

    /// One row of the conversation, from the stream: appended, and what
    /// it says of the context and of who spoke carried on.
    async fn tell_line(
        &mut self,
        uuid: Uuid,
        line: TranscriptLine,
        at: rho_agent_types::UnixMs,
    ) -> anyhow::Result<()> {
        // The notebook's reason for speaking rides on the row it produced:
        // an exec call's results, or the message of output an idle model
        // was woken with.
        let wake = match &line {
            TranscriptLine::ToolResults { .. } | TranscriptLine::User { .. } => {
                self.python_wake.take()
            }
            TranscriptLine::Assistant { .. } | TranscriptLine::Compacted { .. } => None,
        };
        let reported = match &line {
            TranscriptLine::Assistant { context_used, .. }
            | TranscriptLine::Compacted { context_used } => *context_used,
            TranscriptLine::User { .. } | TranscriptLine::ToolResults { .. } => None,
        };
        self.host
            .append(AgentEvent::Transcript {
                uuid,
                line,
                at,
                wake,
            })
            .await?;
        if reported.is_some() {
            self.state.context_used = reported;
        }
        Ok(())
    }

    /// The block's row is in: its streamed copy leaves the tail, which
    /// is told again without it (a shorter tail empties the client's and
    /// says the rest).
    fn release_block(&mut self, content: &[rho_claude::protocol::AssistantContent]) {
        use rho_claude::protocol::AssistantContent;
        let Some(block) = content.first() else {
            return;
        };
        let Some(index) = self.stream_items.iter().find_map(|(index, item)| {
            matches!(
                (block, item),
                (AssistantContent::Text { .. }, ClaudeStreamItem::Text(_))
                    | (
                        AssistantContent::Thinking { .. },
                        ClaudeStreamItem::Thinking(_)
                    )
                    | (
                        AssistantContent::ToolUse { .. },
                        ClaudeStreamItem::ToolUse { .. }
                    )
            )
            .then_some(*index)
        }) else {
            return;
        };
        self.stream_items.remove(&index);
        self.pending_response = PendingInferenceResponse::default();
        for (slot, item) in self.stream_items.values().enumerate() {
            if let Ok(item) = item.to_streaming_context_item() {
                self.pending_response
                    .apply(slot, ContextItemEvent::Update(item));
            }
        }
        self.set_streaming_kind();
    }

    /// Where the API's block `index` sits in the tail: past the blocks
    /// let go.
    fn tail_slot(&self, index: usize) -> usize {
        self.stream_items.range(..index).count()
    }

    async fn complete_rewind(&mut self) -> anyhow::Result<()> {
        if !self.pending_rewind {
            return Ok(());
        }
        self.close_process().await?;
        let view = Arc::clone(self.view.get().await?);
        let messages = rho_claude::read_session_messages_by_id(
            &self.claude.projects(),
            self.session_id,
            view.cwd(),
            rho_claude::SessionMessagesOptions::default(),
        )
        .await?;
        let materialized = match self.start_mode {
            ClaudeStartMode::Fork { resume_at, .. } => {
                rho_claude::session_messages_through_assistant(&messages, resume_at).is_some()
            }
            ClaudeStartMode::New => !messages.is_empty(),
            ClaudeStartMode::Resume => true,
        };
        anyhow::ensure!(
            materialized,
            "rewound Claude transcript did not materialize"
        );
        self.host.complete_claude_rewind(self.session_id).await?;
        self.pending_rewind = false;
        self.start_mode = ClaudeStartMode::Resume;
        Ok(())
    }

    async fn rotate_pending_rewind(&mut self) -> anyhow::Result<()> {
        let (source_session_id, resume_at) = match self.start_mode {
            ClaudeStartMode::Fork {
                source_session_id,
                resume_at,
            } => (source_session_id, Some(resume_at)),
            ClaudeStartMode::New => (self.session_id, None),
            ClaudeStartMode::Resume => return Ok(()),
        };
        self.session_id = Uuid::new_v4();
        self.host
            .claude_rewind(
                UnixMillis::now(),
                None,
                Some(ClaudeRewind {
                    source_session_id,
                    session_id: self.session_id,
                    resume_at,
                }),
            )
            .await?;
        Ok(())
    }

    async fn recover_pending_rewind(&mut self) -> anyhow::Result<()> {
        if self.pending_rewind
            && let Err(error) = self.complete_rewind().await
        {
            if error.is::<crate::worker::StoreError>() {
                return Err(error);
            }
            self.rotate_pending_rewind().await?;
        }
        Ok(())
    }

    /// With --replay-user-messages the CLI echoes every user message when
    /// it enters context: the echo of a queued send is that send leaving
    /// the queue. Its row is the file's.
    /// Claude's echo of a message this loop sent: the turn leaves the
    /// queue; the echo's row (next) is the message in the conversation.
    fn activate_turn_from_user_echo(&mut self, uuid: Option<&str>) -> bool {
        let Some(uuid) = uuid else { return false };
        let Some(index) = self.queued_turns.iter().position(|turn| turn.uuid == uuid) else {
            return false;
        };
        self.queued_turns.remove(index);
        promote_queued_user_message(&mut self.state);
        self.published();
        true
    }

    async fn handle_system_message(
        &mut self,
        message: rho_claude::protocol::SystemMessage,
    ) -> anyhow::Result<()> {
        let rho_claude::protocol::SystemMessage::CompactBoundary {
            uuid,
            compact_metadata,
            ..
        } = message
        else {
            return Ok(());
        };

        remove_compact_commands(&mut self.state.queued_inputs);
        let (uuid, line, at) = compacted_row(uuid.as_deref(), compact_metadata.as_ref());
        self.tell_line(uuid, line, at).await?;
        self.published();
        Ok(())
    }

    /// The loop's state changed: publish the status, and say what changed
    /// in the tail if anyone is looking. Every row this loop writes is
    /// committed before the state moves, so the tail follows its row.
    fn published(&self) {
        *self.status.write().expect("poison") = AgentStatus {
            kind: self.state.kind.clone(),
            queued: self.state.queued_inputs.len(),
        };
        self.host
            .publish_queue(self.state.queued_inputs.iter().cloned().collect());
        self.host.published();
    }

    fn set_kind(&mut self, kind: AgentStateKind) {
        self.state.kind = kind;
        self.published();
    }

    /// Publishes the in-flight message's usage as context occupancy.
    fn update_context_used(&mut self) {
        let Some(usage) = &self.turn_usage else {
            return;
        };
        self.state.context_used = Some(usage.context_total());
        self.published();
    }

    fn set_streaming_kind(&mut self) {
        self.set_kind(AgentStateKind::ApiStreaming {
            pending_response: self.pending_response.clone(),
            previous_attempt: None,
        });
    }

    async fn fail(&mut self, error: anyhow::Error) -> anyhow::Result<()> {
        if error.is::<crate::worker::StoreError>() {
            return Err(error);
        }

        if let Some(host) = &mut self.python {
            host.failed(rho_agent_types::UnixMs::now(), Arc::from(error.to_string()));
        }
        // The row first, so what Claude had said is kept and the tail
        // the loop tells next follows it.
        let partial = std::mem::take(&mut self.pending_response);
        {
            self.host
                .append(AgentEvent::Failed {
                    partial: partial.clone(),
                    error: Cow::Owned(error.to_string()),
                    retrying: false,
                    at: UnixMillis::now(),
                })
                .await?;
        }
        self.host.failed(error.to_string()).await?;
        self.set_kind(AgentStateKind::Error(FailedInferenceResponse {
            partial_response: partial,
            attempt_count: NonZeroU64::MIN,
            error: Arc::new(error.to_string()),
        }));
        Ok(())
    }

    async fn observe_exec(
        &self,
        id: rho_inference::types::ExecId,
        milestone: rho_agent_types::ExecMilestone,
        at: rho_agent_types::UnixMs,
    ) -> anyhow::Result<()> {
        self.host
            .append(AgentEvent::ExecObserved { id, milestone, at })
            .await?;
        Ok(())
    }

    async fn handle_stream_event(
        &mut self,
        event: rho_claude::protocol::MessageStreamEvent,
    ) -> anyhow::Result<()> {
        let now = rho_agent_types::UnixMs::now();
        match &event {
            rho_claude::protocol::MessageStreamEvent::MessageStart { .. } => {
                self.response_execs.clear()
            }
            rho_claude::protocol::MessageStreamEvent::ContentBlockStart {
                index,
                content_block: rho_claude::protocol::StreamContentBlock::ToolUse { id, name, .. },
            } if name == "mcp__py__exec" => {
                let id = rho_inference::types::ExecId::try_from(id.as_str())?;
                self.response_execs.insert(*index, id.clone());
                self.observe_exec(id, rho_agent_types::ExecMilestone::FirstBlock, now)
                    .await?;
            }
            rho_claude::protocol::MessageStreamEvent::ContentBlockStop { index } => {
                if let Some(id) = self.response_execs.get(index).cloned() {
                    self.observe_exec(id, rho_agent_types::ExecMilestone::ArgumentsFinished, now)
                        .await?;
                }
            }
            rho_claude::protocol::MessageStreamEvent::MessageStop => {
                for id in self.response_execs.values().cloned().collect::<Vec<_>>() {
                    self.observe_exec(id, rho_agent_types::ExecMilestone::ResponseFinished, now)
                        .await?;
                }
            }
            _ => {}
        }
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
                let streaming = item.to_streaming_context_item()?;
                self.stream_items.insert(index, item);
                let slot = self.tail_slot(index);
                self.pending_response
                    .apply(slot, ContextItemEvent::Update(streaming));
                self.set_streaming_kind();
            }
            rho_claude::protocol::MessageStreamEvent::ContentBlockDelta { index, delta } => {
                if let Some(item) = self.stream_items.get_mut(&index) {
                    item.apply_delta(delta)?;
                    let streaming = item.to_streaming_context_item()?;
                    let slot = self.tail_slot(index);
                    self.pending_response
                        .apply(slot, ContextItemEvent::Update(streaming));
                    self.set_streaming_kind();
                }
            }
            // The block's own event may have let it go already.
            rho_claude::protocol::MessageStreamEvent::ContentBlockStop { index } => {
                if self.stream_items.contains_key(&index) {
                    let slot = self.tail_slot(index);
                    self.pending_response.apply(slot, ContextItemEvent::Finish);
                    self.set_streaming_kind();
                }
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

impl crate::ClaudeOutputBatch {
    fn message(&self) -> Vec<ContentPart> {
        let mut content = vec![ContentPart::Text {
            text: "Retained Python output from existing work. It may already have reached you if the host restarted during handoff; do not rerun its source.\n".into(),
        }];
        for (id, output) in &self.outputs {
            content.push(ContentPart::Text {
                text: format!("Exec {}:\n{}\n", id.as_str(), output.output),
            });
            content.extend(output.images.iter().map(|image| ContentPart::Image {
                media_type: image.media_type.clone(),
                data: image.data.clone(),
            }));
        }
        content
    }
}

/// Wakes when a notebook cell has something new, or when the boundary said
/// to ask it again. Never, for an agent without a notebook.
async fn python_wake(
    notify: Option<&tokio::sync::Notify>,
    recheck: Option<rho_agent_types::UnixMs>,
) {
    let Some(notify) = notify else {
        return std::future::pending().await;
    };
    let timer = async move {
        match recheck {
            Some(at) => {
                let wait = at.0.saturating_sub(rho_agent_types::UnixMs::now().0);
                tokio::time::sleep(Duration::from_millis(wait)).await;
            }
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        _ = notify.notified() => {}
        _ = timer => {}
    }
}

enum ClaudeLoopEvent {
    PythonWake,
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
    base.cache_creation = update.cache_creation.or(base.cache_creation.take());
}

fn remove_compact_commands(inputs: &mut InputQueues) {
    inputs.retain(|input| match &input.kind {
        InputKind::Message { content } => !is_compact_command(content),
        InputKind::Compaction => true,
    });
}

/// The oldest queued message left the queue: Claude has it now.
fn promote_queued_user_message(state: &mut AgentState) -> bool {
    state
        .queued_inputs
        .remove_first(|queued| matches!(queued.kind, InputKind::Message { .. }))
        .is_some()
}

fn is_compact_command(content: &[ContentPart]) -> bool {
    match content {
        [ContentPart::Text { text }] => text.trim() == "/compact",
        _ => false,
    }
}

/// Writes a generated file that a view namespace bind-mounts, reusing the
/// tempfile in `path` when there is one: the mount follows the inode.
fn write_generated_source(
    path: &mut Option<tempfile::TempPath>,
    prefix: &str,
    suffix: &str,
    contents: &str,
) -> anyhow::Result<Utf8PathBuf> {
    let (mut file, source) = if let Some(path) = path.as_ref() {
        let source = Utf8PathBuf::try_from(path.to_path_buf())
            .context("generated Claude tempfile path is not valid UTF-8")?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)
            .context("reopen generated Claude tempfile")?;
        (file, source)
    } else {
        let source_file = tempfile::Builder::new()
            .prefix(prefix)
            .suffix(suffix)
            .tempfile()
            .context("create generated Claude tempfile")?;
        let source = Utf8PathBuf::try_from(source_file.path().to_owned())
            .context("generated Claude tempfile path is not valid UTF-8")?;
        let (file, temp_path) = source_file.into_parts();
        *path = Some(temp_path);
        (file, source)
    };
    file.write_all(contents.as_bytes())
        .context("write generated Claude tempfile")?;
    file.flush().context("flush generated Claude tempfile")?;
    Ok(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_claude_prompt_without_replacing_bind_source() {
        let mut path = None;
        let first =
            write_generated_source(&mut path, "rho-claude-prompt-", ".md", "ultra").unwrap();
        let second = write_generated_source(&mut path, "rho-claude-prompt-", ".md", "alt").unwrap();

        assert_eq!(second, first);
        assert_eq!(std::fs::read_to_string(first).unwrap(), "alt");
    }

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
            total_usage: crate::db::AgentUsageBucket::default(),
            usage_provider: crate::db::AgentUsageModel::FABLE,
        };
        state.queued_inputs.push(QueuedInput {
            source: crate::MessageSender::User,
            kind: InputKind::Message {
                content: (*text("claude-normalized text")).clone(),
            },
            delivery: MessageDelivery::Immediate,
            at: rho_agent_types::UnixMs(0),
        });
        assert!(promote_queued_user_message(&mut state));

        assert!(state.queued_inputs.is_empty());
        assert!(!promote_queued_user_message(&mut state));
    }
}
