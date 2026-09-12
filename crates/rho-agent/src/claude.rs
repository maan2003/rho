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
use rho_claude::{ClaudeCode, ClaudeCodeOptions, Effort, Model, SdkMcpServer, Session};
use rho_core::{ContentPart, ContextItemEvent, PendingInferenceResponse};
use rho_db::RhoDb;
use rho_inference::Inference;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::db::{
    AgentEventPos, AgentId, AgentPresentationCache, AgentPresentationUpdate,
    AgentProfileWriteTxnExt, AgentReadTxnExt, AgentRole, AgentRoleSessionProfile as _,
    AgentRuntime, AgentWriteTxnExt, ClaudeRewind, EngineerIntelligence, SessionBinding, UnixMillis,
};
use crate::multi_agent_tools::MultiAgentTools;
use crate::{
    AgentEvent, AgentState, AgentStateKind, AgentStatus, FailedInferenceResponse, InputKind,
    InputQueues, MessageDelivery, QueuedInput, TranscriptLine, prompt,
};

pub(crate) mod projection;
pub(crate) mod python_host;
pub mod rebuild;

use projection::{ClaudeStreamItem, Projection, assistant_row, compacted_row, user_row};

use crate::lazy::Lazy;

#[derive(Clone)]
pub struct ClaudeAgent {
    status: Arc<RwLock<AgentStatus>>,
    control: mpsc::UnboundedSender<ClaudeControl>,
    head: Arc<RwLock<crate::db::AgentHead>>,
    /// The agent's place, materialized on first use: a new agent's clone
    /// may still be in flight when a terminal or shell asks for it.
    view: Arc<Lazy<Arc<crate::View>>>,
}

impl ClaudeAgent {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn create(
        db: RhoDb,
        inference: Inference,
        claude: rho_claude::accounts::ClaudePaths,
        display_name: Option<String>,
        start: crate::StartPlace,
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
        let crate::StartPlace { view, info, .. } = start;
        let session_id = Uuid::new_v4();
        write.create_agent(
            UnixMillis::now(),
            agent_id,
            display_name,
            vec![info],
            role,
            mode,
            AgentRuntime::Claude { session_id },
            parent,
        );
        write.commit();
        let python_mode = mode.claude_python();

        let pool_events = pool.clone();
        let multi_agent = pool
            .upgrade()
            .map(|_| MultiAgentTools::new(pool, agent_id, parent));
        let head = db.read().get_agent(agent_id);
        let state = AgentState {
            blocks: Vec::new(),
            queued_inputs: InputQueues::default(),
            kind: AgentStateKind::Idle,
            context_used: None,
            total_usage: db.read().agent_usage_total(agent_id),
            usage_provider: match model {
                rho_claude::Model::Opus => crate::db::AgentUsageModel::OPUS,
                rho_claude::Model::Fable | rho_claude::Model::Sonnet => {
                    crate::db::AgentUsageModel::FABLE
                }
            },
        };
        Ok((
            agent_id,
            Self::new(
                db,
                inference,
                claude,
                agent_id,
                view,
                model,
                effort,
                session_id,
                state,
                ClaudeStartMode::New,
                false,
                multi_agent,
                pool_events,
                role,
                head,
                python_mode,
            ),
        ))
    }

    pub(crate) async fn load(
        db: RhoDb,
        inference: Inference,
        claude: rho_claude::accounts::ClaudePaths,
        agent_id: AgentId,
        view: Arc<Lazy<Arc<crate::View>>>,
        pool: std::sync::Weak<crate::pool::AgentPool>,
    ) -> anyhow::Result<Self> {
        let record = db.read().get_agent(agent_id);
        let head = record.clone();
        let parent_agent = record.parent;
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
        let python_mode = record.config.binding.claude_python();
        let primary_repo = record.primary_workdir().repo().to_owned();
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
                let mut write = db.write().await;
                write.complete_agent_claude_rewind(agent_id, rewind.session_id);
                write.commit();
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
                let mut write = db.write().await;
                write.set_agent_claude_rewind(agent_id, Some(rewind.clone()));
                write.commit();
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
            total_usage: db.read().agent_usage_total(agent_id),
            usage_provider: match model {
                rho_claude::Model::Opus => crate::db::AgentUsageModel::OPUS,
                rho_claude::Model::Fable | rho_claude::Model::Sonnet => {
                    crate::db::AgentUsageModel::FABLE
                }
            },
        };
        let pool_events = pool.clone();
        Ok(Self::new(
            db,
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
            pool.upgrade()
                .map(|_| MultiAgentTools::new(pool, agent_id, parent_agent)),
            pool_events,
            record.config.role,
            head,
            python_mode,
        ))
    }

    #[expect(clippy::too_many_arguments)]
    fn new(
        db: RhoDb,
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
        multi_agent: Option<MultiAgentTools>,
        pool_events: std::sync::Weak<crate::pool::AgentPool>,
        role: crate::db::AgentRole,
        head: crate::db::AgentHead,
        python_mode: bool,
    ) -> Self {
        let status = Arc::new(RwLock::new(AgentStatus {
            kind: state.kind.clone(),
            queued: state.queued_inputs.len(),
        }));
        let head = Arc::new(RwLock::new(head));
        let (control, control_rx) = mpsc::unbounded_channel();
        let last_presentation_source = {
            let records = db
                .read()
                .agent_presentation_source_tail(agent_id, crate::PRESENTATION_SOURCE_TAIL_BYTES);
            crate::presentation_sources(agent_id, &records)
                .last()
                .map(|source| source.through)
        };
        let presentation_session = Arc::new(tokio::sync::Mutex::new(
            crate::presentation::Session::new(inference.clone()),
        ));
        let loop_state = ClaudeLoop {
            db,
            claude,
            inference,
            presentation_session,
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
            python_mode,
            python: None,
            python_wake: None,
            python_recheck: None,
            pending_response: PendingInferenceResponse::default(),
            stream_items: BTreeMap::new(),
            queued_turns: VecDeque::new(),
            turn_usage: None,
            cancelling: false,
            pending_rewind,
            execution_generation: 0,
            state,
            status: Arc::clone(&status),
            head: Arc::clone(&head),
            teller: std::sync::Mutex::new(crate::live::Teller::default()),
            control_rx,
            control: control.downgrade(),
            multi_agent,
            pool_events,
            role,
            presentation: ClaudePresentationState::default(),
            last_presentation_source,
            projection: Projection::default(),
        };
        tokio::spawn(loop_state.run());
        Self {
            status,
            control,
            head,
            view,
        }
    }

    /// The agent's view, ready once its place is.
    pub async fn view(&self) -> anyhow::Result<Arc<crate::View>> {
        Ok(Arc::clone(self.view.get().await?))
    }

    pub fn status(&self) -> AgentStatus {
        self.status.read().expect("poison").clone()
    }

    /// The record as of the loop's last change to it.
    pub fn head(&self) -> crate::db::AgentHead {
        self.head.read().expect("poison").clone()
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

    /// Whether anyone is looking at this agent; titles and activity are
    /// made only then.
    pub(crate) fn set_watched(&self, watching: bool) {
        let _ = self
            .control
            .send(ClaudeControl::PresentationWatch { watching });
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
    PresentationWatch {
        watching: bool,
    },
    PresentationStarted {
        generation: u64,
        acknowledged: oneshot::Sender<bool>,
    },
    PresentationFinished {
        generation: u64,
        result: Result<Option<AgentPresentationUpdate>, String>,
    },
    /// Tell the live tail whole, for a client that just started looking.
    TellTail,
}

struct ClaudeLoop {
    db: RhoDb,
    /// The Claude configuration this agent runs against, handed down from
    /// the daemon rather than resolved here.
    claude: rho_claude::accounts::ClaudePaths,
    inference: Inference,
    /// The agent's one persistent Luna session, shared by activity updates
    /// and turn reports so both keep one prompt prefix warm.
    presentation_session: Arc<tokio::sync::Mutex<crate::presentation::Session>>,
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
    python_mode: bool,
    /// The notebook, once the first spawn has built it. Outlives the
    /// process: cells keep running across a respawn.
    python: Option<python_host::PythonHost>,
    /// Why the notebook last spoke, until the transcript row it produced
    /// arrives to carry it.
    python_wake: Option<crate::WakeFacts>,
    /// When the boundary said to ask it again, if it can change by itself.
    python_recheck: Option<rho_core::UnixMs>,
    pending_response: PendingInferenceResponse,
    stream_items: BTreeMap<usize, ClaudeStreamItem>,
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
    teller: std::sync::Mutex<crate::live::Teller>,
    control_rx: mpsc::UnboundedReceiver<ClaudeControl>,
    /// The loop's own address, for the tasks it starts (the sidecar, the
    /// file watch).
    control: mpsc::WeakUnboundedSender<ClaudeControl>,
    multi_agent: Option<MultiAgentTools>,
    pool_events: std::sync::Weak<crate::pool::AgentPool>,
    role: crate::db::AgentRole,
    presentation: ClaudePresentationState,
    last_presentation_source: Option<AgentEventPos>,
    /// What the projection of Claude's log keeps from one line to the
    /// next: usage already told, calls awaiting their result's times.
    projection: Projection,
}

#[derive(Default)]
struct ClaudePresentationState {
    watched: bool,
    dirty: bool,
    generation: u64,
    last_started: Option<tokio::time::Instant>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for ClaudeLoop {
    fn drop(&mut self) {
        if let Some(task) = self.presentation.task.take() {
            task.abort();
        }
    }
}

struct ClaudeTurn {
    uuid: String,
    content: Arc<Vec<ContentPart>>,
}

impl ClaudeLoop {
    async fn run(mut self) {
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
                        control = control_rx.recv() => ClaudeLoopEvent::Control(control),
                        event = process.next_event() => ClaudeLoopEvent::Protocol(Box::new(event)),
                        _ = python_wake(notify.as_deref(), recheck) => ClaudeLoopEvent::PythonWake,
                    }
                };
                match event {
                    ClaudeLoopEvent::PythonWake => {}
                    ClaudeLoopEvent::Control(Some(control)) => self.handle_control(control).await,
                    ClaudeLoopEvent::Control(None) => {
                        if self.pending_rewind {
                            let _ = self.complete_rewind().await;
                        } else {
                            self.close_process().await;
                        }
                        return;
                    }
                    ClaudeLoopEvent::Protocol(event) => match *event {
                        Ok(Some(event)) => self.handle_event(event).await,
                        Ok(None) => {
                            self.process = None;
                            self.forget_pending_exec();
                            self.recover_pending_rewind().await;
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
                                .await;
                            }
                        }
                        Err(error) => {
                            self.process = None;
                            self.forget_pending_exec();
                            self.recover_pending_rewind().await;
                            self.queued_turns.clear();
                            self.fail(error).await;
                        }
                    },
                }
                // Every event may have changed what the notebook's cells
                // have to say or whether the model can hear it; ask once.
                self.python_tick().await;
            } else {
                let Some(control) = self.control_rx.recv().await else {
                    return;
                };
                self.handle_control(control).await;
            }
            let kind = self.state.kind.clone();
            crate::tell_turn_boundary(
                &self.db,
                self.agent_id,
                &initial_kind,
                &kind,
                self.execution_generation != initial_execution_generation,
            )
            .await;
            if crate::execution_settled(
                &initial_kind,
                &kind,
                self.execution_generation != initial_execution_generation,
            ) {
                // The activity throttle coalesces within a turn; the next
                // turn's first update should not inherit this one's spacing.
                self.presentation.last_started = None;
                if let Some(pool) = self.pool_events.upgrade() {
                    pool.settle_turn(self.agent_id).await;
                    // set_kind notified before the durable disposition changed;
                    // wake projections again so they observe the settled pair.
                    self.published();
                }
            }
        }
    }

    async fn handle_control(&mut self, control: ClaudeControl) {
        match control {
            ClaudeControl::UserMessage {
                content,
                uuid,
                accepted,
            } => {
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
                    at: rho_core::UnixMs::now(),
                };
                // The queue is Claude Code's, in its process: no row says
                // a message waits (nothing would persist it across a
                // restart); the live queue does, and the echo's own row is
                // the message going in.
                if let Err(error) = self.ensure_process().await {
                    if let Some(accepted) = accepted {
                        let _ = accepted.send(Err(anyhow::anyhow!("{error:#}")));
                    }
                    self.fail(error).await;
                    return;
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
                    self.fail(error).await;
                } else if let Some(accepted) = accepted {
                    let _ = accepted.send(Ok(()));
                }
            }
            ClaudeControl::SetEffort { effort, reply } => {
                let _ = reply.send(self.set_effort(effort).await);
            }
            ClaudeControl::ChangeRole { role, reply } => {
                let _ = reply.send(self.change_role(role).await);
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
                self.cancel_python().await;
                if busy && self.process.is_some() {
                    let result =
                        tokio::time::timeout(Duration::from_secs(30), self.soft_cancel(&queued))
                            .await;
                    if !matches!(result, Ok(Ok(()))) {
                        if let Ok(Err(error)) = result {
                            eprintln!("rho-agent: Claude soft cancel failed: {error:#}");
                        } else {
                            eprintln!("rho-agent: Claude soft cancel timed out");
                        }
                        self.close_process().await;
                    }
                } else if matches!(kind, AgentStateKind::Error(_)) {
                    self.close_process().await;
                }
                self.cancelling = false;
                self.pending_response = PendingInferenceResponse::default();
                self.stream_items.clear();
                self.set_kind(AgentStateKind::Idle);
                if self.pending_rewind && self.complete_rewind().await.is_err() {
                    self.rotate_pending_rewind().await;
                }
            }
            ClaudeControl::Rewind { turns, reply } => {
                let _ = reply.send(self.rewind(turns).await);
            }
            ClaudeControl::PresentationWatch { watching } => {
                if watching == self.presentation.watched {
                    return;
                }
                self.presentation.watched = watching;
                if watching {
                    self.presentation.dirty = true;
                    self.schedule_presentation();
                } else {
                    if let Some(task) = self.presentation.task.take() {
                        task.abort();
                    }
                    self.presentation.generation = self.presentation.generation.wrapping_add(1);
                    self.presentation.dirty = false;
                    self.presentation.last_started = None;
                }
            }
            ClaudeControl::PresentationStarted {
                generation,
                acknowledged,
            } => {
                let accepted = self.presentation.generation == generation
                    && self.presentation.watched
                    && self.presentation.task.is_some();
                if accepted {
                    self.presentation.dirty = false;
                    self.presentation.last_started = Some(tokio::time::Instant::now());
                }
                let _ = acknowledged.send(accepted);
            }
            ClaudeControl::TellTail => {
                self.teller.lock().expect("poison").reset();
                self.published();
            }
            ClaudeControl::PresentationFinished { generation, result } => {
                if self.presentation.generation != generation {
                    return;
                }
                self.presentation.task = None;
                match result {
                    Ok(Some(update)) if self.last_presentation_source == Some(update.through) => {
                        let _ = self.persist_presentation(update).await;
                    }
                    Ok(Some(_)) | Ok(None) => {}
                    Err(error) => {
                        eprintln!("rho-agent: Claude presentation generation failed: {error}");
                    }
                }
                self.schedule_presentation();
            }
        }
    }

    async fn persist_presentation(
        &mut self,
        update: AgentPresentationUpdate,
    ) -> Option<AgentPresentationCache> {
        let mut write = self.db.write().await;
        let cache = write.apply_agent_presentation(UnixMillis::now(), self.agent_id, &update)?;
        write.commit();
        if let Some(pool) = self.pool_events.upgrade() {
            pool.publish_presentation_changed(
                self.agent_id,
                cache.generated_title.clone(),
                cache.activity.clone(),
            );
        }
        Some(cache)
    }

    fn reset_presentation(&mut self) {
        if let Some(task) = self.presentation.task.take() {
            task.abort();
        }
        self.presentation.generation = self.presentation.generation.wrapping_add(1);
        self.presentation.last_started = None;
        self.presentation.dirty = self.presentation.watched;
    }

    fn schedule_presentation(&mut self) {
        if !self.presentation.watched
            || !self.presentation.dirty
            || self.presentation.task.is_some()
        {
            return;
        }
        self.presentation.dirty = false;
        self.presentation.generation = self.presentation.generation.wrapping_add(1);
        let generation = self.presentation.generation;
        let now = tokio::time::Instant::now();
        let delay = self
            .presentation
            .last_started
            .and_then(|started| {
                crate::presentation::MIN_INTERVAL.checked_sub(now.duration_since(started))
            })
            .unwrap_or_default();
        let db = self.db.clone();
        let session = Arc::clone(&self.presentation_session);
        let agent_id = self.agent_id;
        let control = self.control.clone();
        self.presentation.task = Some(tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let result = if !crate::presentation::has_input(&db, agent_id) {
                Ok(None)
            } else {
                match crate::presentation::acquire_request().await {
                    Ok(permit) => {
                        let (acknowledged, accepted) = oneshot::channel();
                        let Some(control) = control.upgrade() else {
                            return;
                        };
                        if control
                            .send(ClaudeControl::PresentationStarted {
                                generation,
                                acknowledged,
                            })
                            .is_err()
                        {
                            return;
                        }
                        drop(control);
                        if !accepted.await.unwrap_or(false) {
                            return;
                        }
                        crate::presentation::generate(db, session, agent_id, permit)
                            .await
                            .map_err(|error| format!("{error:#}"))
                    }
                    Err(error) => Err(format!("{error:#}")),
                }
            };
            if let Some(control) = control.upgrade() {
                let _ = control.send(ClaudeControl::PresentationFinished { generation, result });
            }
        }));
    }

    async fn close_process(&mut self) {
        self.forget_pending_exec();
        if let Some(process) = self.process.take() {
            let _ = process.close().await;
        }
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
                event => self.handle_event(event).await,
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
            _ => anyhow::bail!("role changes currently support only eng-ultra and eng-alt"),
        };
        anyhow::ensure!(
            matches!(
                requested,
                EngineerIntelligence::Ultra | EngineerIntelligence::Alt
            ),
            "role changes currently support only eng-ultra and eng-alt"
        );

        let role = match self.role {
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Ultra | EngineerIntelligence::Alt,
            } => AgentRole::Engineer {
                intelligence: requested,
            },
            _ => anyhow::bail!("role changes currently support only eng-ultra and eng-alt"),
        };
        if role == self.role {
            return Ok(());
        }

        let binding = role.session_profile()?;
        let model = binding
            .claude_model()
            .ok_or_else(|| anyhow::anyhow!("role change would leave the Claude runtime"))?;
        let effort = binding
            .claude_effort()
            .ok_or_else(|| anyhow::anyhow!("role change has no Claude effort"))?;

        self.close_process().await;
        let mut write = self.db.write().await;
        write.set_agent_profile(self.agent_id, role, binding);
        write.commit();
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
                event => self.handle_event(event).await,
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
        let dropped = self
            .db
            .read()
            .agent_event_records(self.agent_id)
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
        let mut write = self.db.write().await;
        if let Some(to) = dropped {
            write.rewind_agent(UnixMillis::now(), self.agent_id, to);
        }
        write.set_agent_claude_rewind(
            self.agent_id,
            Some(ClaudeRewind {
                source_session_id,
                session_id: new_session_id,
                resume_at,
            }),
        );
        write.commit();
        self.last_presentation_source = None;
        self.reset_presentation();
        self.schedule_presentation();
        if let Some(pool) = self.pool_events.upgrade() {
            let record = self.db.read().get_agent(self.agent_id);
            pool.publish_presentation_changed(
                self.agent_id,
                record.generated_title,
                record.activity,
            );
        }
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
        let account = self.db.read().claude_account();
        if self.process.is_some() {
            if self.claude_account.as_deref() == Some(account.as_str()) {
                return Ok(());
            }
            eprintln!(
                "rho-agent: restarting {} on Claude account {account}",
                self.agent_id.encoded()
            );
            self.close_process().await;
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
        if let Some(tools) = &self.multi_agent {
            options.set_env("RHO_AGENT_ID", tools.self_id().encoded());
        }
        if self.python_mode {
            self.ensure_python(&view)?;
            // Tool search would defer the one tool behind a lookup; the deny
            // list in the generated settings removes ToolSearch too. The
            // timeout is the CLI's ceiling on an open exec call.
            options.set_env("ENABLE_TOOL_SEARCH", "false");
            options.set_env(
                "MCP_TOOL_TIMEOUT",
                python_host::EXEC_TIMEOUT.as_millis().to_string(),
            );
        }
        self.configure_claude_home(&view, &mut options, &account)
            .await?;
        let mut command = options.command().await?;
        view.prepare_command(&mut command, None).await?;
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
                    name: python_host::SERVER_NAME.to_owned(),
                    timeout: Some(python_host::EXEC_TIMEOUT),
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
    fn ensure_python(&mut self, view: &Arc<crate::View>) -> anyhow::Result<()> {
        if self.python.is_some() {
            return Ok(());
        }
        let (shell, others) = crate::agent::host_tools(
            view,
            self.role,
            self.agent_id,
            Some(&self.inference),
            self.multi_agent.as_ref(),
            &self.pool_events,
        );
        let specs = others.iter().map(|tool| tool.spec()).collect::<Vec<_>>();
        let tool = rho_agent_tools::PythonTool::new(shell, others)
            .map_err(|error| anyhow::anyhow!("Python notebook failed to start: {error}"))?;
        self.python = Some(python_host::PythonHost::new(tool, specs));
        Ok(())
    }

    /// Gives the view the Claude configuration this agent runs against: its
    /// account, when it has one, and its own generated `CLAUDE.md`. Both are
    /// mounted at `~/.claude` when the view's namespace is built, so this
    /// has to run before the first spawn; a respawn only rewrites the prompt
    /// the standing mount already points at.
    async fn configure_claude_home(
        &mut self,
        view: &crate::View,
        options: &mut rho_claude::ClaudeCodeOptions,
        account: &str,
    ) -> anyhow::Result<()> {
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
        options.set_env("CLAUDE_CONFIG_DIR", config_home.as_str());
        let prompt = prompt::claude_prompt(
            Some(view),
            self.multi_agent.as_ref(),
            self.role,
            self.python.as_ref().map(|host| host.host_specs()),
        );
        // Keep one source inode alive for the lifetime of the view namespace.
        // Unlinking a bind-mounted source makes the target pathname disappear
        // inside that namespace, so a rewrite has to reuse this file rather
        // than replace it.
        let source = write_generated_source(
            &mut self.claude_prompt_path,
            "rho-claude-prompt-",
            ".md",
            &prompt,
        )?;
        // A Python-mode agent runs on the account's own settings with every
        // Claude tool denied, so the notebook is all the model has. The
        // generated file covers the account's `settings.json`.
        let settings = if self.python_mode {
            let base = self.claude.account_settings(account)?;
            let settings = rho_claude::settings::deny_all_but_own_tools(&base);
            let text = serde_json::to_string_pretty(&settings)?;
            Some(
                write_generated_source(
                    &mut self.claude_settings_path,
                    "rho-claude-settings-",
                    ".json",
                    &text,
                )?
                .into_std_path_buf(),
            )
        } else {
            None
        };
        view.set_claude_home(rho_workset::ClaudeHome {
            account: account_dir.into_std_path_buf(),
            shared_projects: config_home.join("projects").into_std_path_buf(),
            config_home: config_home.into_std_path_buf(),
            prompt: source.into_std_path_buf(),
            settings,
        })
        .await?;
        self.claude_account = Some(account.to_owned());
        Ok(())
    }

    async fn handle_event(&mut self, event: rho_claude::ClaudeEvent) {
        match event {
            rho_claude::ClaudeEvent::System(message) => {
                self.handle_system_message(message).await;
            }
            rho_claude::ClaudeEvent::ControlResponse(_) => {}
            rho_claude::ClaudeEvent::ControlRequest(message) => {
                self.handle_control_request(message).await;
            }
            // One content block, finished: its row, then the live tail
            // lets go of the streamed copy. A subagent's blocks are its
            // own.
            rho_claude::ClaudeEvent::Assistant(message) => {
                if message.parent_tool_use_id.is_some() {
                    return;
                }
                match assistant_row(&message, self.state.usage_provider, &mut self.projection) {
                    Ok(Some((uuid, line, at))) => self.tell_line(uuid, line, at).await,
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
                    return;
                }
                match user_row(&message, &mut self.projection) {
                    Ok(Some((uuid, line, at))) => self.tell_line(uuid, line, at).await,
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
                    host.turn_ended(rho_core::UnixMs::now());
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
                        .await;
                } else {
                    let final_text = message.result.unwrap_or_default();
                    if let Some(pool) = self.pool_events.upgrade() {
                        pool.publish_completed_turn(crate::pool::AgentTurnCompleted {
                            agent_id: self.agent_id,
                            final_answer: final_text.clone(),
                        })
                        .await;
                    }
                    crate::presentation::spawn_turn_report(
                        self.db.clone(),
                        self.pool_events.clone(),
                        Arc::clone(&self.presentation_session),
                        self.agent_id,
                        &final_text,
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
                }
                if self.pending_rewind
                    && successful
                    && self.queued_turns.is_empty()
                    && let Err(error) = self.complete_rewind().await
                {
                    self.rotate_pending_rewind().await;
                    self.fail(error.context("finalize rewound Claude session"))
                        .await;
                }
            }
            rho_claude::ClaudeEvent::StreamEvent(event) => {
                let message_stopped = matches!(
                    &event.event,
                    rho_claude::protocol::MessageStreamEvent::MessageStop
                );
                if let Err(error) = self.handle_stream_event(event.event) {
                    self.fail(error).await;
                    return;
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
                    if let Some(pool) = self.pool_events.upgrade() {
                        pool.record_agent_usage(self.agent_id, turn_usage).await;
                    }
                    self.published();
                }
            }
            rho_claude::ClaudeEvent::RateLimitEvent(_) => {}
            rho_claude::ClaudeEvent::CommandLifecycle(message) => {
                self.handle_command_lifecycle(message).await;
            }
            rho_claude::ClaudeEvent::Other => {}
        }
    }

    /// A request from the CLI: with the notebook registered, its MCP
    /// traffic. The handshake and listing are answered here; an exec call
    /// is answered when the boundary says so, from `python_tick`.
    async fn handle_control_request(
        &mut self,
        message: rho_claude::protocol::ControlRequestMessage,
    ) {
        use rho_claude::protocol::ControlRequest;
        let request_id = message.request_id;
        let reply = match message.request {
            ControlRequest::McpMessage {
                server_name,
                message: rpc,
            } if server_name == python_host::SERVER_NAME => match python_host::handle_rpc(&rpc) {
                python_host::Rpc::Reply(value) => Ok(serde_json::json!({ "mcp_response": value })),
                python_host::Rpc::Ignore => Ok(serde_json::json!({})),
                python_host::Rpc::Exec { id, source } => match self.python.as_mut() {
                    Some(host) => {
                        match host.exec(request_id.clone(), id, source, rho_core::UnixMs::now()) {
                            Some(refused) => Ok(serde_json::json!({ "mcp_response": refused })),
                            None => return,
                        }
                    }
                    None => Err("the Python notebook is not running".to_owned()),
                },
            },
            ControlRequest::McpMessage { server_name, .. } => {
                Err(format!("unknown MCP server {server_name}"))
            }
            ControlRequest::Other => Err("unsupported control request".to_owned()),
        };
        self.respond_control(&request_id, reply).await;
    }

    async fn respond_control(
        &mut self,
        request_id: &str,
        reply: Result<serde_json::Value, String>,
    ) {
        let Some(process) = self.process.as_mut() else {
            return;
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
        }
    }

    /// Asks the boundary whether the model should hear from the notebook,
    /// and acts on the answer: an open exec call returns with everything
    /// waiting, or an idle model is woken with it as a message. A working
    /// model with no call open hears it at its next call or turn end.
    async fn python_tick(&mut self) {
        let Some(host) = self.python.as_mut() else {
            return;
        };
        let now = rho_core::UnixMs::now();
        let oldest_user = self.state.queued_inputs.iter().map(|input| input.at).min();
        // The model can be reached through an open call, or as an idle
        // process that takes a message. Otherwise it is mid-turn, and the
        // notebook waits for its next call.
        let idle = matches!(self.state.kind, AgentStateKind::Idle)
            && self.process.is_some()
            && self.queued_turns.is_empty()
            && !self.pending_rewind;
        let available = host.has_pending() || idle;
        match host.decide(available, oldest_user, now) {
            crate::agent::boundary::Boundary::No { recheck } => {
                self.python_recheck = recheck;
            }
            crate::agent::boundary::Boundary::AbortAndResend
            | crate::agent::boundary::Boundary::RetryExhausted => {
                self.python_recheck = None;
            }
            crate::agent::boundary::Boundary::Now { wake } => {
                self.python_recheck = None;
                if let Some((pending, drained)) = host.answer_pending() {
                    // Recorded on the transcript row the results become.
                    self.python_wake = Some(wake);
                    let reply = serde_json::json!({
                        "mcp_response": {
                            "jsonrpc": "2.0",
                            "id": pending.rpc_id,
                            "result": drained.into_mcp_result(),
                        },
                    });
                    self.respond_control(&pending.request_id, Ok(reply)).await;
                } else if idle {
                    let drained = host.drain_idle();
                    if drained.is_empty() {
                        return;
                    }
                    self.python_wake = Some(wake);
                    let mut content = vec![ContentPart::Text {
                        text: "Output from Python cells that were still running when your last \
                               turn ended:"
                            .to_owned(),
                    }];
                    for output in drained.own.into_iter().chain(drained.updates) {
                        content.push(ContentPart::Text {
                            text: (*output.output).clone(),
                        });
                        content.extend(output.images.iter().map(|image| ContentPart::Image {
                            media_type: image.media_type.clone(),
                            data: image.data.clone(),
                        }));
                    }
                    self.handle_control(ClaudeControl::UserMessage {
                        content,
                        uuid: Uuid::new_v4().to_string(),
                        accepted: None,
                    })
                    .await;
                }
            }
        }
    }

    /// Stops the notebook's cells and answers an open exec call as
    /// cancelled, ahead of the interrupt that abandons it on the CLI's side.
    async fn cancel_python(&mut self) {
        let Some(host) = self.python.as_mut() else {
            return;
        };
        if let Some(pending) = host.cancel(rho_core::UnixMs::now()) {
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
    ) {
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
    }

    /// One row of the conversation, from the stream: appended, and what
    /// it says of the context and of who spoke carried on.
    async fn tell_line(&mut self, uuid: Uuid, line: TranscriptLine, at: rho_core::UnixMs) {
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
        let said = matches!(
            line,
            TranscriptLine::User { .. } | TranscriptLine::Assistant { .. }
        );
        let mut write = self.db.write().await;
        let pos = write.append_agent_event(
            self.agent_id,
            &AgentEvent::Transcript {
                uuid,
                line,
                at,
                wake,
            },
        );
        write.commit();
        if reported.is_some() {
            self.state.context_used = reported;
        }
        if said {
            self.last_presentation_source = Some(pos);
            self.presentation.dirty = true;
            self.schedule_presentation();
        }
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
        self.close_process().await;
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
        let mut write = self.db.write().await;
        write.complete_agent_claude_rewind(self.agent_id, self.session_id);
        write.commit();
        self.pending_rewind = false;
        self.start_mode = ClaudeStartMode::Resume;
        Ok(())
    }

    async fn rotate_pending_rewind(&mut self) {
        let (source_session_id, resume_at) = match self.start_mode {
            ClaudeStartMode::Fork {
                source_session_id,
                resume_at,
            } => (source_session_id, Some(resume_at)),
            ClaudeStartMode::New => (self.session_id, None),
            ClaudeStartMode::Resume => return,
        };
        self.session_id = Uuid::new_v4();
        let mut write = self.db.write().await;
        write.set_agent_claude_rewind(
            self.agent_id,
            Some(ClaudeRewind {
                source_session_id,
                session_id: self.session_id,
                resume_at,
            }),
        );
        write.commit();
    }

    async fn recover_pending_rewind(&mut self) {
        if self.pending_rewind && self.complete_rewind().await.is_err() {
            self.rotate_pending_rewind().await;
        }
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

    async fn handle_system_message(&mut self, message: rho_claude::protocol::SystemMessage) {
        let rho_claude::protocol::SystemMessage::CompactBoundary {
            uuid,
            compact_metadata,
            ..
        } = message
        else {
            return;
        };

        remove_compact_commands(&mut self.state.queued_inputs);
        let (uuid, line, at) = compacted_row(uuid.as_deref(), compact_metadata.as_ref());
        self.tell_line(uuid, line, at).await;
        self.published();
    }

    /// The loop's state changed: publish the status, and say what changed
    /// in the tail if anyone is looking. Every row this loop writes is
    /// committed before the state moves, so the tail follows its row.
    fn published(&self) {
        *self.status.write().expect("poison") = AgentStatus {
            kind: self.state.kind.clone(),
            queued: self.state.queued_inputs.len(),
        };
        let live = self
            .pool_events
            .upgrade()
            .is_some_and(|pool| pool.is_live(self.agent_id));
        let mut teller = self.teller.lock().expect("poison");
        if !live {
            teller.reset();
            return;
        }
        let queue = self
            .state
            .queued_inputs
            .iter()
            .map(queued_item)
            .collect::<Vec<_>>();
        if let Some(live) = teller.tell_queue(&queue) {
            crate::mirror::tell_live(&self.db, self.agent_id, live);
        }
        let kind = self.state.kind.clone();
        for live in teller.tell(&kind) {
            crate::mirror::tell_live(&self.db, self.agent_id, live);
        }
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

    async fn fail(&mut self, error: anyhow::Error) {
        // The row first, so what Claude had said is kept and the tail
        // the loop tells next follows it.
        let partial = std::mem::take(&mut self.pending_response);
        {
            let mut write = self.db.write().await;
            write.append_agent_event(
                self.agent_id,
                &AgentEvent::Failed {
                    partial: partial.clone(),
                    error: Cow::Owned(error.to_string()),
                    retrying: false,
                    at: UnixMillis::now(),
                },
            );
            write.commit();
        }
        if let Some(pool) = self.pool_events.upgrade() {
            pool.publish_failed_turn(self.agent_id, error.to_string())
                .await;
        }
        self.set_kind(AgentStateKind::Error(FailedInferenceResponse {
            partial_response: partial,
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

/// Wakes when a notebook cell has something new, or when the boundary said
/// to ask it again. Never, for an agent without a notebook.
async fn python_wake(notify: Option<&tokio::sync::Notify>, recheck: Option<rho_core::UnixMs>) {
    let Some(notify) = notify else {
        return std::future::pending().await;
    };
    let timer = async move {
        match recheck {
            Some(at) => {
                let wait = at.0.saturating_sub(rho_core::UnixMs::now().0);
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

/// A queued input as the wire tells it.
fn queued_item(input: &QueuedInput) -> rho_ui_proto::mirror::QueuedItem {
    use rho_ui_proto::mirror::QueuedItem;
    match &input.kind {
        InputKind::Message { content } => QueuedItem::Message {
            from: match input.source {
                crate::MessageSender::User => None,
                crate::MessageSender::Agent { id } => Some(id),
            },
            text: content
                .iter()
                .map(|part| match part {
                    ContentPart::Text { text } => text.as_str(),
                    ContentPart::Image { .. } => "[image]",
                })
                .collect::<Vec<_>>()
                .join("\n"),
            delivery: input.delivery,
        },
        InputKind::Compaction => QueuedItem::Compaction,
    }
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
            at: rho_core::UnixMs(0),
        });
        assert!(promote_queued_user_message(&mut state));

        assert!(state.queued_inputs.is_empty());
        assert!(!promote_queued_user_message(&mut state));
    }
}
