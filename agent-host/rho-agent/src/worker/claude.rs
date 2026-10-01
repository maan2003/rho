//! Claude Code agent support.
//!
//! `rho-claude` owns the Claude Code protocol. This module owns the projection
//! from Claude protocol/transcript messages into Rho agent vocabulary.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::Write as _;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Context as _;
use camino::Utf8PathBuf;
use rho_agent_types::transcript::{ContextBlock, ContextItemEvent, PendingInferenceResponse};
use rho_agent_types::{AgentId, AgentRole, ContentPart, EngineerIntelligence, SendKind};
use rho_claude::{ClaudeCode, ClaudeCodeOptions, Effort, Model, SdkMcpServer, Session};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::entry::{Block, Entry, MessageId, Notice, Party, Report};
use crate::inference::Inference;
use crate::log::{AgentRoleSessionProfile as _, AgentRuntime, ClaudeRewind};
use crate::worker::host_client::{HostClient, StoreError};
use crate::worker::shared::mailroom::{Mailroom, Outbound};
use crate::{
    AgentEvent, AgentStatus, InferenceState, InputKind, InputQueues, QueuedInput, RuntimeState,
    TranscriptLine, prompt,
};

/// The Claude runtime's own view of its transcript and queue. Internal
/// to that loop; the Rho loop keeps its history as blocks of its own.
#[derive(Clone, Debug, PartialEq)]
struct AgentState {
    /// Rho-runtime blocks are append-only. Provider-managed runtimes may
    /// replace this with a compacted transcript snapshot when the provider
    /// rewrites history.
    blocks: Vec<Arc<ContextBlock>>,
    /// Inputs waiting to enter model context, in arrival order.
    queued_inputs: InputQueues,
    kind: InferenceState,
    /// Tokens occupying the model's context window after the latest
    /// response (all input, cached or not, plus that response's output).
    /// `None` until the agent's first response reports usage.
    context_used: Option<u64>,
    /// Cumulative provider-reported usage across this agent's requests.
    total_usage: crate::log::AgentUsageBucket,
    usage_provider: crate::log::AgentUsageModel,
}

pub(crate) mod projection;
pub(crate) mod python_host;

use projection::{
    ClaudeStreamItem, Projection, assistant_row, compacted_row, live_response, user_row,
};

#[derive(Clone)]
pub struct ClaudeAgent {
    status: Arc<RwLock<AgentStatus>>,
    control: mpsc::UnboundedSender<ClaudeControl>,
    head: Arc<RwLock<crate::log::AgentHead>>,
}

impl ClaudeAgent {
    #[expect(clippy::too_many_arguments)]
    fn new(
        host: Arc<HostClient>,
        inference: Inference,
        claude: rho_claude::accounts::ClaudePaths,
        agent_id: AgentId,
        cwd: Utf8PathBuf,
        model: Model,
        effort: Effort,
        session_id: Uuid,
        state: AgentState,
        start_mode: ClaudeStartMode,
        pending_rewind: bool,
        pending_output: Option<crate::ClaudeOutputBatch>,
        role: rho_agent_types::AgentRole,
        head: crate::log::AgentHead,
    ) -> (Self, ClaudeLoop) {
        let status = Arc::new(RwLock::new(AgentStatus {
            runtime: RuntimeState {
                inference: state.kind.clone(),
                ..Default::default()
            },
            response: None,
            draft: None,
            queued: state.queued_inputs.len(),
        }));
        let head = Arc::new(RwLock::new(head));
        let (control, control_rx) = mpsc::unbounded_channel();
        let (mailroom, outbox) = Mailroom::new();
        host.observe(&status);
        let loop_state = ClaudeLoop {
            name_updates: host.names(),
            host,
            claude,
            inference,
            agent_id,
            cwd,
            model,
            effort,
            session_id,
            start_mode,
            process: None,
            claude_prompt_path: None,
            claude_settings_path: None,
            claude_account: None,
            python: None,
            mailroom,
            outbox,
            awaiting: false,
            archived: false,
            pending_human: VecDeque::new(),
            pending_agent: VecDeque::new(),
            delivered: Vec::new(),
            deferred: VecDeque::new(),
            uncertain_receipts: Vec::new(),
            received: HashSet::new(),
            pending_output,
            python_wake: None,
            python_recheck: None,
            pending_response: PendingInferenceResponse::default(),
            response_id: None,
            stream_items: BTreeMap::new(),
            draft: None,
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
            source: InputSource::Human(MessageId::new()),
        });
    }

    /// An `id` already logged is acknowledged without a second row.
    pub async fn send_user_content_accepted(
        &self,
        id: MessageId,
        content: Vec<ContentPart>,
    ) -> anyhow::Result<()> {
        self.send_content_accepted(content, InputSource::Human(id))
            .await
    }

    /// Deliver agent mail and wait for acceptance into Rho's volatile Claude
    /// queue. A process or agent host restart may lose it before Claude records
    /// it.
    pub async fn send_agent_message_accepted(
        &self,
        sender: AgentId,
        text: String,
    ) -> anyhow::Result<()> {
        self.send_content_accepted(vec![ContentPart::Text { text }], InputSource::Agent(sender))
            .await
    }

    async fn send_content_accepted(
        &self,
        content: Vec<ContentPart>,
        source: InputSource,
    ) -> anyhow::Result<()> {
        let uuid = Uuid::new_v4().to_string();
        let (accepted, reply) = oneshot::channel();
        self.control
            .send(ClaudeControl::UserMessage {
                content,
                uuid,
                accepted: Some(accepted),
                source,
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputSource {
    Human(MessageId),
    Agent(AgentId),
    DeferredAgent(AgentId, MessageId),
    Internal,
}

enum ClaudeControl {
    Retire(oneshot::Sender<anyhow::Result<()>>),
    UserMessage {
        content: Vec<ContentPart>,
        uuid: String,
        accepted: Option<oneshot::Sender<anyhow::Result<()>>>,
        source: InputSource,
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
    /// the agent host rather than resolved here.
    claude: rho_claude::accounts::ClaudePaths,
    inference: Inference,
    agent_id: AgentId,
    cwd: Utf8PathBuf,
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
    mailroom: Arc<Mailroom>,
    outbox: mpsc::UnboundedReceiver<Outbound>,
    awaiting: bool,
    archived: bool,
    pending_human: VecDeque<rho_agent_types::UnixMs>,
    pending_agent: VecDeque<rho_agent_types::UnixMs>,
    delivered: Vec<(
        Option<MessageId>,
        crate::entry::Wake,
        Option<crate::ClaudeOutputBatch>,
    )>,
    deferred: VecDeque<(Vec<ContentPart>, String, AgentId, MessageId)>,
    uncertain_receipts: Vec<MessageId>,
    /// Every message id the log holds, so a message sent again after its
    /// answer was lost is not logged twice.
    received: HashSet<MessageId>,
    pending_output: Option<crate::ClaudeOutputBatch>,
    /// Why the notebook last spoke, until the transcript row it produced
    /// arrives to carry it.
    python_wake: Option<crate::WakeFacts>,
    /// When the boundary said to ask it again, if it can change by itself.
    python_recheck: Option<rho_agent_types::UnixMs>,
    pending_response: PendingInferenceResponse,
    response_id: Option<String>,
    stream_items: BTreeMap<usize, ClaudeStreamItem>,
    /// The last streamed exec's source prefix while its cell may still send.
    draft: Option<(String, String)>,
    response_execs: BTreeMap<usize, rho_agent_types::transcript::ExecId>,
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
    head: Arc<RwLock<crate::log::AgentHead>>,
    /// What clients have been told of the tail, so each publish says only
    /// what changed.
    control_rx: mpsc::UnboundedReceiver<ClaudeControl>,
    /// The loop's own address, for the tasks it starts (the sidecar, the
    /// file watch).
    host: Arc<HostClient>,
    name_updates: tokio::sync::watch::Receiver<Option<crate::log::AgentHead>>,
    role: rho_agent_types::AgentRole,
    /// What the projection of Claude's log keeps from one line to the
    /// next: usage already told, calls awaiting their result's times.
    projection: Projection,
}

/// The complete volatile observation made from Claude's process and notebook
/// facts.
fn claude_status(
    inference: &InferenceState,
    python: Option<&python_host::PythonHost>,
    awaiting_human: bool,
    archived: bool,
    response_id: Option<&str>,
    stream_items: &BTreeMap<usize, ClaudeStreamItem>,
    queued: usize,
) -> AgentStatus {
    AgentStatus {
        runtime: claude_runtime(inference, python, awaiting_human, archived),
        response: live_response(response_id, stream_items),
        draft: None,
        queued,
    }
}

/// Source still being written, or about to be admitted as a notebook cell.
fn streamed_draft(items: &BTreeMap<usize, ClaudeStreamItem>) -> Option<(String, String)> {
    items.values().find_map(|item| {
        let ClaudeStreamItem::ToolUse {
            id,
            name,
            arguments,
        } = item
        else {
            return None;
        };
        super::shared::python_preview::tool_preview(
            name,
            arguments,
            rho_agents_client::protocol::transcript::ArgumentsFormat::Json,
        )
        .map(|text| (id.clone(), text))
    })
}

fn claude_runtime(
    inference: &InferenceState,
    python: Option<&python_host::PythonHost>,
    awaiting_human: bool,
    archived: bool,
) -> RuntimeState {
    RuntimeState {
        inference: if matches!(inference, InferenceState::Responding)
            && python.is_some_and(python_host::PythonHost::has_pending)
        {
            InferenceState::Idle
        } else {
            inference.clone()
        },
        running_tasks: python.map_or(0, python_host::PythonHost::running_tasks),
        awaiting_human,
        checkin_at: python.and_then(python_host::PythonHost::checkin_at),
        archived,
        stale: false,
    }
}

/// Materialize unfinished provider context only when a failure needs the
/// partial response. Finished slots are already authoritative and immutable.
fn refresh_pending_partial(
    pending: &mut PendingInferenceResponse,
    items: &BTreeMap<usize, ClaudeStreamItem>,
) {
    for (slot, item) in items.values().enumerate() {
        if matches!(
            pending.items.get(slot),
            Some(rho_agent_types::transcript::StreamingContextItemState::Pending(_))
        ) && let Ok(item) = item.to_streaming_context_item()
        {
            pending.apply(slot, ContextItemEvent::Update(item));
        }
    }
}

/// Finish with the latest bytes rather than the fragment present at start.
fn finish_stream_block(
    pending: &mut PendingInferenceResponse,
    items: &BTreeMap<usize, ClaudeStreamItem>,
    index: usize,
) -> anyhow::Result<bool> {
    let Some(item) = items.get(&index) else {
        return Ok(false);
    };
    let item = item.to_streaming_context_item()?;
    let slot = items.range(..index).count();
    pending.apply(slot, ContextItemEvent::Update(item));
    pending.apply(slot, ContextItemEvent::Finish);
    Ok(true)
}

struct ClaudeTurn {
    uuid: String,
    content: Arc<Vec<ContentPart>>,
    message: Option<MessageId>,
    source: InputSource,
    output: Option<crate::ClaudeOutputBatch>,
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
        host: Arc<HostClient>,
        inference: Inference,
        claude: rho_claude::accounts::ClaudePaths,
        cwd: Utf8PathBuf,
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
                host.claude_rewind(rho_agent_types::UnixMs::now(), None, Some(rewind.clone()))
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
            kind: InferenceState::Idle,
            context_used: None,
            total_usage: host.usage_total().await?,
            usage_provider: match model {
                rho_claude::Model::Opus => crate::log::AgentUsageModel::OPUS,
                rho_claude::Model::Fable | rho_claude::Model::Sonnet => {
                    crate::log::AgentUsageModel::FABLE
                }
            },
        };
        let pending_output = host.claude_pending_output().await?;
        let head = host.head().await?;
        let (agent, mut loop_state) = ClaudeAgent::new(
            host,
            inference,
            claude,
            agent_id,
            cwd,
            model,
            effort,
            session_id,
            state,
            start_mode,
            pending_rewind,
            pending_output,
            record.config.role,
            head,
        );
        let entries = loop_state
            .host
            .history()
            .await?
            .1
            .into_iter()
            .filter_map(|(_, event)| {
                if let AgentEvent::Entry(entry) = event {
                    Some(entry)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        loop_state.received = entries
            .iter()
            .filter_map(|entry| match entry {
                Entry::Received { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        let (archived, deferred, uncertain) = recover_receipts(entries);
        loop_state.archived = archived;
        loop_state.uncertain_receipts = uncertain;
        for (at, id, sender, body) in deferred {
            let content = body
                .into_iter()
                .map(|block| match block {
                    Block::Text(text) => ContentPart::Text { text },
                    Block::Image(image) => ContentPart::Image {
                        media_type: image.media_type,
                        data: image.data,
                    },
                })
                .collect();
            loop_state
                .deferred
                .push_back((content, Uuid::new_v4().to_string(), sender, id));
            loop_state.pending_agent.push_back(at);
        }
        loop_state.published();
        Ok((agent, loop_state))
    }
    fn apply_name(&mut self, named: crate::log::AgentHead) {
        let mut head = self.head.write().expect("poison");
        head.generated_title = named.generated_title;
        head.title_attempted = named.title_attempted;
    }

    pub(crate) async fn run(&mut self) -> anyhow::Result<()> {
        if !self.uncertain_receipts.is_empty() {
            let at = rho_agent_types::UnixMs::now();
            let acknowledged = std::mem::take(&mut self.uncertain_receipts);
            self.entry(Entry::RequestSent {
                at,
                why: crate::entry::Wake::Restarted,
                report: Report {
                    notebook: rho_notebook::Report::from_text(
                        "Claude restarted before confirming whether queued messages entered its context; they were not resent to avoid duplicate work.".into(),
                        Vec::new(),
                    ),
                    acknowledged,
                    ..Default::default()
                },
                compact: false,
            }).await?;
            self.entry(Entry::Notice {
                at,
                notice: Notice::Error(
                    "Claude message delivery was uncertain after restart; no message was resent."
                        .into(),
                ),
            })
            .await?;
        }
        self.published();
        // Only body deltas are frame-limited; lifecycle and notebook changes
        // continue to publish immediately. Each frame still serializes the
        // complete current payload for focused listeners; this bounds its
        // frequency, not its total cumulative bytes. The complete last body
        // is flushed on the next non-delta event, including block completion.
        let mut stream_frame = tokio::time::interval(Duration::from_millis(50));
        stream_frame.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut stream_dirty = false;
        loop {
            // The notebook can finish between iterations; compare against the
            // last published observation rather than freshly sampled facts.
            let initial_runtime = self.status.read().expect("poison").runtime.clone();
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
                            changed.context("agent host naming connection closed")?;
                            let named = self.name_updates.borrow_and_update().clone();
                            if let Some(named) = named { self.apply_name(named); self.published(); }
                            continue;
                        }
                        control = control_rx.recv() => ClaudeLoopEvent::Control(control),
                        Some(outbound) = self.outbox.recv() => ClaudeLoopEvent::Outbound(outbound),
                        _ = stream_frame.tick(), if stream_dirty => ClaudeLoopEvent::StreamFrame,
                        _ = python_wake(notify.as_deref(), recheck) => ClaudeLoopEvent::PythonWake,
                        event = process.next_event() => ClaudeLoopEvent::Protocol(Box::new(event)),
                    }
                };
                match event {
                    ClaudeLoopEvent::PythonWake => {
                        stream_dirty = false;
                    }
                    ClaudeLoopEvent::StreamFrame => {
                        // Fall through the ordinary tick and turn-edge path:
                        // draining output or ticking Python can start or settle
                        // work even when the event only fired for a body frame.
                        stream_dirty = false;
                    }
                    ClaudeLoopEvent::Outbound(outbound) => {
                        stream_dirty = false;
                        self.outbound(outbound).await?;
                    }
                    ClaudeLoopEvent::Control(Some(control)) => {
                        stream_dirty = false;
                        self.handle_control(control).await?;
                    }
                    ClaudeLoopEvent::Control(None) => return Ok(()),
                    ClaudeLoopEvent::Protocol(event) => match *event {
                        Ok(Some(event)) => {
                            stream_dirty = matches!(&event,
                                rho_claude::ClaudeEvent::StreamEvent(stream)
                                if matches!(stream.event, rho_claude::protocol::MessageStreamEvent::ContentBlockDelta { .. }));
                            self.handle_event(event).await?;
                        }
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
                            let mid_turn = matches!(self.state.kind, InferenceState::Responding);
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
                // A body delta only appends to stream_items. Python wakes and
                // outbound/lifecycle events have their own higher-priority
                // branches, so do not rescan retained notebook sources or
                // rebuild provider context on every fragment.
                if stream_dirty {
                    continue;
                }
                self.drain_outbox().await?;
                self.python_tick().await?;
            } else {
                let control = tokio::select! {
                    control = self.control_rx.recv() => Some(control),
                    Some(outbound) = self.outbox.recv() => { self.outbound(outbound).await?; None },
                    changed = self.name_updates.changed() => {
                        changed.context("agent host naming connection closed")?;
                        let named = self.name_updates.borrow_and_update().clone();
                        if let Some(named) = named { self.apply_name(named); self.published(); }
                        continue;
                    }
                };
                if let Some(control) = control {
                    let Some(control) = control else {
                        return Ok(());
                    };
                    self.handle_control(control).await?;
                }
            }
            self.drain_outbox().await?;
            let current = self.runtime_snapshot();
            if !stream_dirty
                || current != initial_runtime
                || self.execution_generation != initial_execution_generation
            {
                self.published();
                stream_dirty = false;
            }
            let settled = (initial_runtime.is_working() && !current.is_working())
                || (self.execution_generation != initial_execution_generation
                    && !current.is_working());
            if settled && let InferenceState::Failed { error } = &current.inference {
                self.entry(Entry::Notice {
                    at: rho_agent_types::UnixMs::now(),
                    notice: Notice::Stopped(error.clone()),
                })
                .await?;
            }
            if settled {
                self.host.settled().await?;
                self.published();
            }
        }
    }

    async fn handle_control(&mut self, control: ClaudeControl) -> anyhow::Result<()> {
        match control {
            ClaudeControl::Retire(reply) => {
                if self.snapshot().settled()
                    && (self.archived
                        || self
                            .python
                            .as_ref()
                            .is_none_or(python_host::PythonHost::retire_settled))
                {
                    let _ = reply.send(Ok(()));
                    // Freeze scheduling and admission at this serialized boundary.
                    // The outer driver cancels this future on agent host disconnect.
                    std::future::pending::<()>().await;
                } else {
                    let _ = reply.send(Err(anyhow::anyhow!("agent still has work")));
                }
            }

            ClaudeControl::UserMessage {
                mut content,
                uuid,
                accepted,
                source,
            } => {
                if let InputSource::Human(id) = source
                    && !self.received.insert(id)
                {
                    if let Some(accepted) = accepted {
                        let _ = accepted.send(Ok(()));
                    }
                    return Ok(());
                }
                let at = rho_agent_types::UnixMs::now();
                let reviving = matches!(source, InputSource::Human(_)) && self.archived;
                if reviving {
                    self.fresh_notebook(at).await?;
                }
                if let InputSource::Agent(sender) = source
                    && self.archived
                {
                    let id = MessageId::new();
                    self.entry(Entry::Received {
                        at,
                        id,
                        from: Party::Agent(sender),
                        body: content
                            .iter()
                            .cloned()
                            .map(|part| match part {
                                ContentPart::Text { text } => Block::Text(text),
                                ContentPart::Image { media_type, data } => {
                                    Block::Image(crate::inference::Image { media_type, data })
                                }
                            })
                            .collect(),
                    })
                    .await?;
                    self.pending_agent.push_back(at);
                    self.deferred.push_back((content, uuid, sender, id));
                    if let Some(accepted) = accepted {
                        let _ = accepted.send(Ok(()));
                    }
                    return Ok(());
                }
                if matches!(source, InputSource::Human(_)) {
                    // The human's message ends a wait it answers.
                    self.awaiting = false;
                    self.pending_human.push_back(at);
                }
                if matches!(source, InputSource::Agent(_)) {
                    self.pending_agent.push_back(at);
                }
                let original_content = content.clone();
                if source != InputSource::Internal
                    && !matches!(content.first(), Some(ContentPart::Text { text }) if text.trim_start().starts_with('/'))
                {
                    let named = self
                        .host
                        .name(&rho_agent_types::transcript::text_content(&content))
                        .await?;
                    self.apply_name(named);
                }
                // Keep CLI slash commands intact. Any retained output remains
                // queued until the command finishes and the CLI is idle.
                let slash_command = matches!(content.first(), Some(ContentPart::Text { text }) if text.trim_start().starts_with('/'));
                let output = self.pending_output.as_ref().filter(|_| !slash_command);
                let output_id = output.map(|batch| batch.id);
                let carried_output = output.cloned();
                if let Some(batch) = output {
                    let mut combined = batch.message();
                    combined.append(&mut content);
                    content = combined;
                }
                self.cancelling = false;
                if matches!(source, InputSource::Human(_)) {
                    if let Some(host) = &mut self.python {
                        host.user_spoke();
                    }
                }
                let busy = matches!(self.state.kind, InferenceState::Responding);
                if !busy {
                    self.execution_generation = self.execution_generation.wrapping_add(1);
                }
                // The CLI echo confirms that a queued message entered context.
                let content = Arc::new(content);
                let input = QueuedInput {
                    source: rho_agent_types::transcript::MessageSender::User,
                    kind: InputKind::Message {
                        content: (*content).clone(),
                    },
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
                let message = match source {
                    InputSource::Human(id) => Some((id, Party::Human)),
                    InputSource::Agent(sender) => Some((MessageId::new(), Party::Agent(sender))),
                    InputSource::DeferredAgent(_, id) => Some((id, Party::Human)),
                    InputSource::Internal => None,
                };
                if let Some((id, from)) = message
                    && !matches!(source, InputSource::DeferredAgent(..))
                {
                    self.entry(Entry::Received {
                        at,
                        id,
                        from,
                        body: original_content
                            .iter()
                            .cloned()
                            .map(|part| match part {
                                ContentPart::Text { text } => Block::Text(text),
                                ContentPart::Image { media_type, data } => {
                                    Block::Image(crate::inference::Image { media_type, data })
                                }
                            })
                            .collect(),
                    })
                    .await?;
                }
                self.queued_turns.push_back(ClaudeTurn {
                    uuid: uuid.clone(),
                    content: Arc::clone(&content),
                    message: message.map(|(id, _)| id),
                    source,
                    output: carried_output,
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
                if reviving {
                    while let Some((content, uuid, sender, id)) = self.deferred.pop_front() {
                        Box::pin(self.handle_control(ClaudeControl::UserMessage {
                            content,
                            uuid,
                            accepted: None,
                            source: InputSource::DeferredAgent(sender, id),
                        }))
                        .await?;
                    }
                }
            }
            ClaudeControl::ChangeRole { role, reply } => {
                let result = self.change_role(role).await;
                if result.as_ref().is_err_and(|error| error.is::<StoreError>()) {
                    return result;
                }
                let _ = reply.send(result);
            }
            ClaudeControl::Cancel => {
                let kind = self.state.kind.clone();
                let busy = matches!(kind, InferenceState::Responding);
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
                            if error.is::<StoreError>() {
                                return Err(error);
                            }
                            eprintln!("rho-agent: Claude soft cancel failed: {error:#}");
                        } else {
                            eprintln!("rho-agent: Claude soft cancel timed out");
                        }
                        self.close_process().await?;
                    }
                } else if matches!(kind, InferenceState::Failed { .. }) {
                    self.close_process().await?;
                }
                self.cancelling = false;
                self.pending_response = PendingInferenceResponse::default();
                self.stream_items.clear();
                self.response_id = None;
                self.set_kind(InferenceState::Idle);
                self.recover_pending_rewind().await?;
            }
            ClaudeControl::Rewind { turns, reply } => {
                let result = self.rewind(turns).await;
                if result.as_ref().is_err_and(|error| error.is::<StoreError>()) {
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

    async fn entry(&self, entry: Entry) -> anyhow::Result<()> {
        self.host.append(AgentEvent::Entry(entry)).await?;
        Ok(())
    }

    async fn outbound(&mut self, outbound: Outbound) -> anyhow::Result<()> {
        let at = rho_agent_types::UnixMs::now();
        match outbound {
            Outbound::Send { cell, text, kind } => {
                if self.python.as_mut().is_some_and(|python| python.sent(cell)) {
                    self.draft = None;
                }
                let to = self
                    .head
                    .read()
                    .expect("poison")
                    .parent
                    .map_or(Party::Human, Party::Agent);
                self.entry(Entry::Sent {
                    at,
                    id: MessageId::new(),
                    to,
                    text: text.clone(),
                    kind,
                })
                .await?;
                // A status is the agent's line, not mail for a subscriber.
                if kind != SendKind::Status {
                    self.host.message_sent(text).await?;
                }
            }
            Outbound::EndTurn if !self.archived => {
                if let Some(python) = self.python.as_mut() {
                    python.end_turn();
                }
                self.awaiting = true;
            }
            Outbound::EndTurn => {}
            Outbound::Archive => {
                self.archived = true;
                self.entry(Entry::Notice {
                    at,
                    notice: Notice::Archived,
                })
                .await?;
                self.close_process().await?;
                if let Some(mut python) = self.python.take() {
                    python.shutdown().await?;
                }
                // Closing Claude may abandon messages already accepted into its
                // volatile queue. Their execution is uncertain; acknowledge
                // the durable receipts without replaying them. Reconcile the
                // log, not just queued_turns: lifecycle events can remove a
                // turn before its user echo confirms delivery.
                let entries = self
                    .host
                    .history()
                    .await?
                    .1
                    .into_iter()
                    .filter_map(|(_, event)| {
                        if let AgentEvent::Entry(entry) = event {
                            Some(entry)
                        } else {
                            None
                        }
                    });
                let (_, _, uncertain) = recover_receipts(entries);
                if !uncertain.is_empty() {
                    self.entry(Entry::RequestSent {
                        at,
                        why: crate::entry::Wake::Restarted,
                        report: Report {
                            notebook: rho_notebook::Report::from_text(
                                "Archiving stopped Claude before confirming whether queued messages entered its context; they were not resent to avoid duplicate work.".into(),
                                Vec::new(),
                            ),
                            acknowledged: uncertain,
                            ..Default::default()
                        },
                        compact: false,
                    }).await?;
                }
                self.awaiting = false;
                self.python_recheck = None;
                self.queued_turns.clear();
                self.state.queued_inputs.clear();
                self.pending_human.clear();
                self.pending_agent.clear();
                self.response_id = None;
                self.stream_items.clear();
                self.pending_response = PendingInferenceResponse::default();
                self.set_kind(InferenceState::Idle);
            }
        }
        self.published();
        Ok(())
    }

    async fn drain_outbox(&mut self) -> anyhow::Result<()> {
        while let Ok(outbound) = self.outbox.try_recv() {
            self.outbound(outbound).await?;
        }
        Ok(())
    }

    async fn fresh_notebook(&mut self, at: rho_agent_types::UnixMs) -> anyhow::Result<()> {
        self.archived = false;
        self.python = None;
        self.entry(Entry::Notice {
            at,
            notice: Notice::FreshNotebook,
        })
        .await?;
        self.published();
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

    async fn change_role(&mut self, requested: AgentRole) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(
                self.state.kind,
                InferenceState::Idle | InferenceState::Failed { .. }
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
                InferenceState::Idle | InferenceState::Failed { .. }
            ),
            ":rewind is only available while idle or errored; use :cancel first"
        );
        anyhow::ensure!(
            self.state.queued_inputs.is_empty() && self.queued_turns.is_empty(),
            ":rewind is not available with queued inputs"
        );

        let cwd = self.cwd.clone();
        let (source_session_id, messages) = if self.pending_rewind {
            match self.start_mode {
                ClaudeStartMode::Fork {
                    source_session_id,
                    resume_at,
                } => {
                    let source = rho_claude::read_session_messages_by_id(
                        &self.claude.projects(),
                        source_session_id,
                        &cwd,
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
                &cwd,
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
                rho_agent_types::UnixMs::now(),
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
        self.state.kind = InferenceState::Idle;
        self.state.context_used = context_used;
        self.response_id = None;
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
        let cwd = self.cwd.clone();
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
        let mut options =
            ClaudeCodeOptions::new(cwd.clone(), self.model, self.effort, self.session_id);
        options.session = session;
        options.set_env("RHO_AGENT_ID", self.role.full_handle(self.agent_id));
        self.ensure_python().await?;
        // Tool search would defer the one tool behind a lookup; the deny
        // list in the generated settings removes ToolSearch too. The
        // timeout is the CLI's ceiling on an open exec call.
        options.set_env("ENABLE_TOOL_SEARCH", "false");
        options.set_env(
            "MCP_TOOL_TIMEOUT",
            rho_claude::mcp::EXEC_TIMEOUT.as_millis().to_string(),
        );
        let (account_dir, target) = self.configure_claude_home(&mut options, &account).await?;
        let mut command = options.command().await?;
        rho_fs_view::command_stdio_only(&mut command);
        command.current_dir(&cwd);
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
    async fn ensure_python(&mut self) -> anyhow::Result<()> {
        if self.python.is_some() {
            return Ok(());
        }
        let team = self.host.team().await?;
        let (shell, exports) = crate::worker::shared::tools::host_tools(
            &self.cwd,
            self.role,
            self.agent_id,
            Some(&self.inference),
            team.as_ref(),
            Some(&self.host),
            Some(&self.mailroom),
            false,
        );
        let notify = Arc::new(tokio::sync::Notify::new());
        let notebook = rho_notebook::Notebook::new(shell, exports, Arc::clone(&notify))
            .map_err(|error| anyhow::anyhow!("Python notebook failed to start: {error}"))?;
        self.python = Some(python_host::PythonHost::new(notebook, notify));
        Ok(())
    }

    /// Prepare provider-owned files; only the child launcher mounts them.
    async fn configure_claude_home(
        &mut self,
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
        let place = prompt::WorksetPrompt::new(&self.cwd);
        let prompt = prompt::claude_prompt(Some(&place), team.as_ref(), self.role);
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
                if message.parent_tool_use_id.is_none() && !message.is_synthetic.unwrap_or(false) {
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
                if !self.delivered.is_empty() {
                    for (id, why, output) in std::mem::take(&mut self.delivered) {
                        self.entry(Entry::RequestSent {
                            at: rho_agent_types::UnixMs::now(),
                            why,
                            report: Report {
                                notebook: output
                                    .as_ref()
                                    .map_or_else(rho_notebook::Report::default, |batch| {
                                        batch.report()
                                    }),
                                messages: id.into_iter().collect(),
                                ..Default::default()
                            },
                            compact: false,
                        })
                        .await?;
                    }
                }
            }
            rho_claude::ClaudeEvent::Result(message) => {
                let successful = !message.is_error;
                self.draft = streamed_draft(&self.stream_items);
                if let Some(host) = &mut self.python {
                    host.turn_ended(
                        rho_agent_types::UnixMs::now(),
                        !self.response_execs.is_empty(),
                    );
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
                    self.draft = None;
                    self.pending_response = PendingInferenceResponse::default();
                    self.stream_items.clear();
                    self.response_id = None;
                    self.set_kind(InferenceState::Idle);
                } else if message.is_error {
                    self.fail(anyhow::anyhow!("{}", message.errors.join("\n")))
                        .await?;
                } else {
                    // CLI result prose is provider output, not a message to the human.
                    // Queued sends run next inside the CLI: staying in the
                    // streaming state avoids a false turn end between them.
                    self.pending_response = PendingInferenceResponse::default();
                    self.stream_items.clear();
                    self.response_id = None;
                    if self.queued_turns.is_empty() {
                        self.set_kind(InferenceState::Idle);
                    } else {
                        self.set_streaming_kind();
                    }
                }
                if self.pending_rewind
                    && successful
                    && self.queued_turns.is_empty()
                    && let Err(error) = self.complete_rewind().await
                {
                    if error.is::<StoreError>() {
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
                    // The SDK message ended; any remaining tail is no longer
                    // streaming. Finished assistant rows release their own
                    // blocks after they are committed.
                    self.stream_items.clear();
                    self.pending_response = PendingInferenceResponse::default();
                    self.response_id = None;
                    self.set_streaming_kind();
                }
                if message_stopped && let Some(usage) = self.turn_usage.take() {
                    let turn_usage = crate::log::AgentUsageBucket {
                        model: match self.model {
                            rho_claude::Model::Opus => crate::log::AgentUsageModel::OPUS,
                            rho_claude::Model::Fable | rho_claude::Model::Sonnet => {
                                crate::log::AgentUsageModel::FABLE
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
                        ..crate::log::AgentUsageBucket::default()
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
                        if self.cancelling || self.response_execs.values().next() != Some(&exec_id)
                        {
                            Err("Only the first exec in a provider response may run. This call was not executed; earlier side effects are not undone.".into())
                        } else if self.python.as_ref().is_none_or(|host| !host.can_admit()) {
                            Err(
                                "The notebook is unavailable or already has an open exec reply."
                                    .into(),
                            )
                        } else {
                            let now = rho_agent_types::UnixMs::now();
                            self.host
                                .append(AgentEvent::ClaudeExecAdmitted {
                                    call: rho_agent_types::transcript::ExecCall {
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
        let idle = matches!(self.state.kind, InferenceState::Idle)
            && self.process.is_some()
            && self.queued_turns.is_empty()
            && !self.pending_rewind;
        let Some(host) = self.python.as_mut() else {
            return Ok(());
        };
        let oldest_user = self.pending_human.front().copied();
        let oldest_agent = self.pending_agent.front().copied();
        let available = host.has_pending() || idle;
        match host.decide(
            available,
            oldest_user,
            oldest_agent,
            self.archived,
            self.pending_output.is_some(),
            now,
        ) {
            python_host::Boundary::No { recheck } => self.python_recheck = recheck,
            python_host::Boundary::Now { wake } => {
                self.python_recheck = None;
                if let Some((pending, mut drained)) = host.answer_pending() {
                    let ends_turn = host.ended();
                    let batch = self.record_output(&mut drained, wake, now).await?;
                    let delivered = self.pending_output.clone().expect("recorded output");
                    let mut result = drained.into_mcp_result();
                    if ends_turn {
                        // Claude Code ends the turn on this result without
                        // sampling the model again (unless it is an error).
                        result["_meta"] = serde_json::json!({ "claude/endTurn": true });
                    }
                    let reply = serde_json::json!({
                        "mcp_response": {
                            "jsonrpc": "2.0", "id": pending.rpc_id,
                            "result": result,
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
                        self.entry(Entry::RequestSent {
                            at: rho_agent_types::UnixMs::now(),
                            why: crate::entry::Wake::Returned,
                            report: Report {
                                notebook: delivered.report(),
                                ..Default::default()
                            },
                            compact: false,
                        })
                        .await?;
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
                    let correction = host.prose_correction();
                    if drained.is_empty()
                        && self.pending_output.is_none()
                        && !correction
                        && wake.trigger != crate::WakeTrigger::Checkin
                    {
                        return Ok(());
                    }
                    let content = if correction {
                        vec![ContentPart::Text { text: "Your last response had no exec call. Text outside a call reaches nobody.".into() }]
                    } else if drained.is_empty() && self.pending_output.is_none() {
                        vec![ContentPart::Text {
                            text: "Check-in: nothing new.".into(),
                        }]
                    } else {
                        Vec::new()
                    };
                    let uuid = if drained.is_empty() && self.pending_output.is_none() {
                        Uuid::new_v4().to_string()
                    } else {
                        self.record_output(&mut drained, wake, now)
                            .await?
                            .to_string()
                    };
                    self.handle_control(ClaudeControl::UserMessage {
                        content,
                        uuid,
                        accepted: None,
                        source: InputSource::Internal,
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
                    self.set_kind(InferenceState::Idle);
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
        let cwd = self.cwd.clone();
        let messages = rho_claude::read_session_messages_by_id(
            &self.claude.projects(),
            self.session_id,
            &cwd,
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
                rho_agent_types::UnixMs::now(),
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
            if error.is::<StoreError>() {
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
        let turn = self.queued_turns.remove(index).expect("found turn");
        if let Some(id) = turn.message {
            match turn.source {
                InputSource::Human(_) => {
                    self.pending_human.pop_front();
                }
                InputSource::Agent(_) | InputSource::DeferredAgent(..) => {
                    self.pending_agent.pop_front();
                }
                InputSource::Internal => {}
            }
            self.delivered.push((
                Some(id),
                if matches!(
                    turn.source,
                    InputSource::Agent(_) | InputSource::DeferredAgent(..)
                ) {
                    crate::entry::Wake::AgentMessage
                } else {
                    crate::entry::Wake::Message
                },
                turn.output,
            ));
        } else if let InputSource::Internal = turn.source {
            self.delivered.push((
                None,
                if turn.output.is_some() {
                    crate::entry::Wake::Returned
                } else if matches!(turn.content.first(), Some(ContentPart::Text { text }) if text.starts_with("Check-in:")) {
                    crate::entry::Wake::Checkin
                } else {
                    crate::entry::Wake::Prose
                },
                turn.output,
            ));
        }
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
    fn snapshot(&self) -> AgentStatus {
        let mut status = claude_status(
            &self.state.kind,
            self.python.as_ref(),
            self.awaiting,
            self.archived,
            self.response_id.as_deref(),
            &self.stream_items,
            self.state.queued_inputs.len(),
        );
        let draft = if status.response.is_some() {
            streamed_draft(&self.stream_items)
        } else {
            self.draft.clone()
        };
        status.draft = draft
            .filter(|(id, _)| {
                self.python
                    .as_ref()
                    .and_then(python_host::PythonHost::published_call)
                    != Some(id.as_str())
                    && (status.response.is_some()
                        || self
                            .python
                            .as_ref()
                            .and_then(python_host::PythonHost::latest_call)
                            == Some(id.as_str())
                            && !self
                                .python
                                .as_ref()
                                .is_some_and(|python| python.latest_finished(id)))
            })
            .map(|(_, text)| text);
        status
    }

    fn runtime_snapshot(&self) -> RuntimeState {
        claude_runtime(
            &self.state.kind,
            self.python.as_ref(),
            self.awaiting,
            self.archived,
        )
    }

    fn published(&self) {
        *self.status.write().expect("poison") = self.snapshot();
        self.host.published();
    }

    fn set_kind(&mut self, kind: InferenceState) {
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
        self.set_kind(InferenceState::Responding);
    }

    async fn fail(&mut self, error: anyhow::Error) -> anyhow::Result<()> {
        if error.is::<StoreError>() {
            return Err(error);
        }

        if let Some(host) = &mut self.python {
            host.failed(rho_agent_types::UnixMs::now(), Arc::from(error.to_string()));
        }
        // The row first, so what Claude had said is kept and the tail
        // the loop tells next follows it.
        // Fragments live in stream_items; materialize the latest incomplete
        // provider items once if the stream fails mid-block.
        refresh_pending_partial(&mut self.pending_response, &self.stream_items);
        let partial = std::mem::take(&mut self.pending_response);
        {
            self.host
                .append(AgentEvent::Failed {
                    partial: partial.clone(),
                    error: Cow::Owned(error.to_string()),
                    retrying: false,
                    at: rho_agent_types::UnixMs::now(),
                })
                .await?;
        }
        self.host.failed(error.to_string()).await?;
        self.response_id = None;
        self.stream_items.clear();
        self.draft = None;
        self.set_kind(InferenceState::Failed {
            error: error.to_string(),
        });
        Ok(())
    }

    async fn observe_exec(
        &self,
        id: rho_agent_types::transcript::ExecId,
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
                let id = rho_agent_types::transcript::ExecId::try_from(id.as_str())?;
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
                self.draft = None;
                self.response_id = Some(Uuid::new_v4().to_string());
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
                    // stream_items owns the partial bytes until a block closes
                    // or a failure needs them. Neither provider context nor the
                    // GUI needs a second complete copy for every fragment.
                    self.state.kind = InferenceState::Responding;
                }
            }
            // The block's own event may have let it go already.
            rho_claude::protocol::MessageStreamEvent::ContentBlockStop { index } => {
                if finish_stream_block(&mut self.pending_response, &self.stream_items, index)? {
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
    fn report(&self) -> rho_notebook::Report {
        let text = self
            .outputs
            .iter()
            .map(|(_, output)| output.output.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let images: Vec<crate::inference::Image> = self
            .outputs
            .iter()
            .flat_map(|(_, output)| output.images.iter())
            .map(|image| crate::inference::Image {
                media_type: image.media_type.clone(),
                data: image.data.clone(),
            })
            .collect();
        rho_notebook::Report::from_text(
            text,
            images
                .into_iter()
                .map(|image: crate::inference::Image| rho_notebook::Image {
                    media_type: image.media_type,
                    data: image.data,
                })
                .collect(),
        )
    }

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
    StreamFrame,
    PythonWake,
    Outbound(Outbound),
    Control(Option<ClaudeControl>),
    Protocol(Box<anyhow::Result<Option<rho_claude::ClaudeEvent>>>),
}

/// Reconcile receipt ids without assuming that an unechoed CLI send did
/// not run. Only messages accepted *while archived* could not have reached
/// Claude: there was no process. An unechoed active send is acknowledged as
/// uncertain rather than resent, so the UI cannot leave a phantom queue.
fn recover_receipts(
    entries: impl IntoIterator<Item = Entry>,
) -> (
    bool,
    Vec<(rho_agent_types::UnixMs, MessageId, AgentId, Vec<Block>)>,
    Vec<MessageId>,
) {
    let mut archived_since = None;
    let mut received = Vec::new();
    let mut accounted = HashSet::new();
    for (position, entry) in entries.into_iter().enumerate() {
        match entry {
            Entry::Notice {
                notice: Notice::Archived,
                ..
            } => archived_since = Some(position),
            Entry::Notice {
                notice: Notice::FreshNotebook,
                ..
            } => archived_since = None,
            Entry::Received { at, id, from, body } => received.push((at, id, from, body, position)),
            Entry::RequestSent { report, .. } => {
                accounted.extend(report.messages.into_iter().chain(report.acknowledged));
            }
            _ => {}
        }
    }
    let mut deferred = Vec::new();
    let mut uncertain = Vec::new();
    for (at, id, from, body, position) in received {
        if accounted.contains(&id) {
            continue;
        }
        if archived_since.is_some_and(|since| position > since)
            && let Party::Agent(sender) = from
        {
            deferred.push((at, id, sender, body));
        } else {
            uncertain.push(id);
        }
    }
    (archived_since.is_some(), deferred, uncertain)
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
    fn streamed_exec_draft_keeps_its_call_identity_after_source_completes() {
        let mut items = BTreeMap::new();
        items.insert(
            0,
            ClaudeStreamItem::ToolUse {
                id: "unrelated".into(),
                name: "shell".into(),
                arguments: "{}".into(),
            },
        );
        items.insert(
            1,
            ClaudeStreamItem::ToolUse {
                id: "exec-1".into(),
                name: "mcp__py__exec".into(),
                arguments: r#"{"source":"human.send('Hel"#.into(),
            },
        );
        assert_eq!(
            streamed_draft(&items),
            Some(("exec-1".into(), "Hel".into()))
        );
        let ClaudeStreamItem::ToolUse { arguments, .. } = items.get_mut(&1).unwrap() else {
            unreachable!()
        };
        *arguments = r#"{"source":"human.send('Hello')"}"#.into();
        assert_eq!(
            streamed_draft(&items),
            Some(("exec-1".into(), "Hello".into()))
        );
    }

    #[test]
    fn pending_context_copies_only_finished_blocks_or_failure_partials() {
        use rho_agent_types::transcript::{StreamingContextItem, StreamingContextItemState};
        let mut items = BTreeMap::new();
        let mut pending = PendingInferenceResponse::default();
        let call = ClaudeStreamItem::ToolUse {
            id: "call-1".into(),
            name: "mcp__py__exec".into(),
            arguments: "{}".into(),
        };
        pending.apply(
            0,
            ContextItemEvent::Update(call.to_streaming_context_item().unwrap()),
        );
        items.insert(4, call);
        for partial_json in ["{\"code\":", "\"print(42)\"}"] {
            items
                .get_mut(&4)
                .unwrap()
                .apply_delta(rho_claude::protocol::ContentBlockDelta::InputJsonDelta {
                    partial_json: partial_json.into(),
                })
                .unwrap();
        }
        assert!(
            matches!(&pending.items[0], StreamingContextItemState::Pending(
            StreamingContextItem::ToolCall { arguments, .. }) if arguments.with_str(|text| text == "{}"))
        );
        assert!(finish_stream_block(&mut pending, &items, 4).unwrap());
        assert!(
            matches!(&pending.items[0], StreamingContextItemState::Finished(
            StreamingContextItem::ToolCall { arguments, .. }) if arguments.with_str(|text| text == "{\"code\":\"print(42)\"}"))
        );
        let mut reasoning = ClaudeStreamItem::Thinking("initial".into());
        pending.apply(
            1,
            ContextItemEvent::Update(reasoning.to_streaming_context_item().unwrap()),
        );
        reasoning
            .apply_delta(rho_claude::protocol::ContentBlockDelta::ThinkingDelta {
                thinking: " plus final".into(),
            })
            .unwrap();
        items.insert(9, reasoning);
        refresh_pending_partial(&mut pending, &items);
        assert!(
            matches!(&pending.items[1], StreamingContextItemState::Pending(
            StreamingContextItem::RawReasoning { content, .. }) if content.with_str(|text| text == "initial plus final"))
        );
        assert!(
            matches!(&pending.items[0], StreamingContextItemState::Finished(
            StreamingContextItem::ToolCall { arguments, .. }) if arguments.with_str(|text| text == "{\"code\":\"print(42)\"}"))
        );
    }

    #[tokio::test]
    async fn notebook_wait_keeps_partial_response_but_is_not_model_inference() {
        let temp = tempfile::tempdir().unwrap();
        let notify = Arc::new(tokio::sync::Notify::new());
        let notebook = rho_notebook::Notebook::new(
            rho_tool_shell::ShellTools::in_directory(
                temp.path().to_str().unwrap().into(),
                Default::default(),
            ),
            Vec::new(),
            Arc::clone(&notify),
        )
        .unwrap();
        let mut python = python_host::PythonHost::new(notebook, notify);
        let mut blocks = BTreeMap::new();
        blocks.insert(
            4,
            ClaudeStreamItem::ToolUse {
                id: "call-1".into(),
                name: "mcp__py__exec".into(),
                arguments: "{\"code\":\"print(1)\"}".into(),
            },
        );
        let before = claude_status(
            &InferenceState::Responding,
            Some(&python),
            false,
            false,
            Some("response-1"),
            &blocks,
            0,
        );
        assert_eq!(before.runtime.inference, InferenceState::Responding);
        assert_eq!(before.response.as_ref().unwrap().id, "response-1");
        assert!(
            python
                .exec(
                    "request".into(),
                    serde_json::json!(1),
                    "call-1".try_into().unwrap(),
                    "print(1)".into(),
                    rho_agent_types::UnixMs::now(),
                )
                .is_none()
        );
        let waiting = claude_status(
            &InferenceState::Responding,
            Some(&python),
            true,
            false,
            Some("response-1"),
            &blocks,
            0,
        );
        assert_eq!(waiting.runtime.inference, InferenceState::Idle);
        assert!(waiting.runtime.awaiting_human);
        assert_eq!(waiting.response, before.response);
        python.shutdown().await.unwrap();
    }

    #[test]
    fn restart_reconciles_only_unconfirmed_active_receipts_and_defers_archived_mail() {
        let sender = AgentId::from_counter(3, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let received = |id, from, at| Entry::Received {
            at: rho_agent_types::UnixMs(at),
            id: MessageId(id),
            from,
            body: vec![Block::Text(format!("message {id}"))],
        };
        let notice = |notice, at| Entry::Notice {
            at: rho_agent_types::UnixMs(at),
            notice,
        };
        let woken = |ids: Vec<MessageId>| Entry::RequestSent {
            at: rho_agent_types::UnixMs(20),
            why: crate::entry::Wake::Message,
            report: Report {
                messages: ids,
                ..Default::default()
            },
            compact: false,
        };
        let entries = vec![
            received(1, Party::Human, 1),
            received(2, Party::Agent(sender), 2),
            woken(vec![MessageId(2)]),
            // Equal timestamps must not turn pre-archive mail into safe replay.
            received(3, Party::Agent(sender), 10),
            notice(Notice::Archived, 10),
            received(4, Party::Agent(sender), 10),
            notice(Notice::FreshNotebook, 11),
            received(5, Party::Human, 12),
            notice(Notice::Archived, 13),
            received(6, Party::Agent(sender), 14),
            received(7, Party::Agent(sender), 15),
            woken(vec![MessageId(7)]),
        ];
        let (archived, deferred, uncertain) = recover_receipts(entries);
        assert!(archived);
        assert_eq!(
            uncertain,
            vec![MessageId(1), MessageId(3), MessageId(4), MessageId(5)]
        );
        assert_eq!(
            deferred,
            vec![(
                rho_agent_types::UnixMs(14),
                MessageId(6),
                sender,
                vec![Block::Text("message 6".into())]
            )]
        );
        let mut acknowledgement = woken(Vec::new());
        if let Entry::RequestSent { report, .. } = &mut acknowledgement {
            report.acknowledged.push(MessageId(1));
        }
        let (archived, deferred, uncertain) =
            recover_receipts(vec![received(1, Party::Human, 1), acknowledgement]);
        assert!(!archived);
        assert!(
            deferred.is_empty() && uncertain.is_empty(),
            "acknowledged receipts never repeat on another restart"
        );
    }

    #[test]
    fn archive_acknowledges_uncertain_receipts_without_consuming_archived_peer_mail() {
        let sender = AgentId::from_counter(4, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let at = rho_agent_types::UnixMs(42);
        let received = |id, from| Entry::Received {
            at,
            id: MessageId(id),
            from,
            body: vec![Block::Text(format!("message {id}"))],
        };
        let entries = vec![
            received(1, Party::Human),
            received(2, Party::Agent(sender)),
            Entry::Notice {
                at,
                notice: Notice::Archived,
            },
            received(3, Party::Agent(sender)),
        ];
        let (_, deferred, uncertain) = recover_receipts(entries.clone());
        assert_eq!(
            uncertain,
            vec![MessageId(1), MessageId(2)],
            "same-millisecond inputs admitted before archive must not be replayed"
        );
        assert_eq!(deferred.len(), 1);
        assert_eq!(deferred[0].1, MessageId(3));
        let mut accounted = entries;
        accounted.push(Entry::RequestSent {
            at,
            why: crate::entry::Wake::Restarted,
            report: Report {
                notebook: rho_notebook::Report::from_text("uncertain delivery".into(), Vec::new()),
                acknowledged: uncertain,
                ..Default::default()
            },
            compact: false,
        });
        let (_, deferred, uncertain) = recover_receipts(accounted);
        assert!(
            uncertain.is_empty(),
            "archive acknowledgement clears the durable queue on the next load"
        );
        assert_eq!(deferred.len(), 1, "archived mail still waits for revival");
    }

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
            kind: InferenceState::Idle,
            context_used: None,
            total_usage: crate::log::AgentUsageBucket::default(),
            usage_provider: crate::log::AgentUsageModel::FABLE,
        };
        state.queued_inputs.push(QueuedInput {
            source: rho_agent_types::transcript::MessageSender::User,
            kind: InputKind::Message {
                content: (*text("claude-normalized text")).clone(),
            },
            at: rho_agent_types::UnixMs(0),
        });
        assert!(promote_queued_user_message(&mut state));

        assert!(state.queued_inputs.is_empty());
        assert!(!promote_queued_user_message(&mut state));
    }
}
