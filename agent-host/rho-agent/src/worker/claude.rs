//! Claude Code agent support.
//!
//! `rho-claude` owns the Claude Code protocol. This module owns the projection
//! from Claude protocol/transcript messages into Rho agent vocabulary.

use std::borrow::Cow;
use std::collections::{BTreeMap, VecDeque};
use std::io::Write as _;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Context as _;
use camino::Utf8PathBuf;
use rho_agent_types::transcript::{ContextItemEvent, PendingInferenceResponse};
use rho_agent_types::{AgentId, ContentPart, InputWake, MailId, UnixMs, Wpos};
use rho_claude::{ClaudeCode, ClaudeCodeOptions, Effort, Model, SdkMcpServer, Session};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::entry::{Notice, Report, Wake};
use crate::inference::Inference;
use crate::log::ClaudeRewind;
use crate::record::{AgentSpec, ClaudeRow, Event, Record, Runtime};
use crate::store::AgentHead;
use crate::worker::host_client::{HostClient, StoreError};
use crate::worker::python::{self, TurnEnd};
use crate::worker::shared::outbox::{Outbound, Outbox};
use crate::{AgentStatus, InferenceState, RuntimeState, TranscriptLine};

pub(crate) mod projection;
pub(crate) mod python_host;

use projection::{
    ClaudeStreamItem, Projection, assistant_row, compacted_row, live_response, user_row,
};

#[derive(Clone)]
pub struct ClaudeAgent {
    status: Arc<RwLock<AgentStatus>>,
    control: mpsc::UnboundedSender<ClaudeControl>,
}

impl ClaudeAgent {
    pub fn status(&self) -> AgentStatus {
        self.status.read().expect("poison").clone()
    }

    /// Say the whole tail again: a client just started looking.
    pub fn tell_tail(&self) {
        let _ = self.control.send(ClaudeControl::TellTail);
    }

    /// Log an input as the agent's own row. Returns once the row is
    /// durable; the loop delivers it with its next request.
    pub(crate) async fn input(
        &self,
        content: Vec<ContentPart>,
        wake: InputWake,
        mail: Option<MailId>,
    ) -> anyhow::Result<()> {
        let (logged, reply) = oneshot::channel();
        self.control
            .send(ClaudeControl::Input {
                content,
                wake,
                mail,
                logged,
            })
            .map_err(|_| anyhow::anyhow!("Claude agent stopped before logging the input"))?;
        reply
            .await
            .map_err(|_| anyhow::anyhow!("Claude agent stopped before logging the input"))
    }

    pub fn compact(&self) {
        let _ = self.control.send(ClaudeControl::Compact);
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
    /// An input to log; `logged` is told once its row is durable.
    Input {
        content: Vec<ContentPart>,
        wake: InputWake,
        mail: Option<MailId>,
        logged: oneshot::Sender<()>,
    },
    Compact,
    Cancel,
    Rewind {
        turns: u32,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    /// Tell the live tail whole, for a client that just started looking.
    TellTail,
}

/// An `Input` row no request has delivered yet.
#[derive(Clone, Debug, PartialEq)]
struct Unread {
    at: UnixMs,
    content: Vec<ContentPart>,
    wake: InputWake,
}

/// What the running Claude Code process was started with: a change to any
/// of it restarts the process at the next send.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Launch {
    account: String,
    model: Model,
    effort: Effort,
}

pub(crate) struct ClaudeLoop {
    /// The Claude configuration this agent runs against, handed down from
    /// the agent host rather than resolved here.
    claude: rho_claude::accounts::ClaudePaths,
    inference: Inference,
    agent_id: AgentId,
    /// Where Claude Code runs and where new notebooks start.
    root: Utf8PathBuf,
    /// The session the process runs: the spec's, or a pending rewind's.
    session_id: Uuid,
    start_mode: ClaudeStartMode,
    process: Option<ClaudeCode>,
    claude_prompt_path: Option<tempfile::TempPath>,
    /// The generated `settings.json` of a Python-mode agent, kept alive for
    /// the same reason as the prompt.
    claude_settings_path: Option<tempfile::TempPath>,
    /// Rho's Claude Code plugin, written once and loaded by every spawn.
    claude_plugin_dir: Option<tempfile::TempDir>,
    /// What the running process was spawned with, so a switch of account,
    /// model or effort is noticed at the next send.
    launched: Option<Launch>,
    /// The notebook, once the first spawn has built it. Outlives the
    /// process: cells keep running across a respawn.
    python: Option<python_host::PythonHost>,
    outbox: Arc<Outbox>,
    outbound: mpsc::UnboundedReceiver<Outbound>,
    awaiting: bool,
    /// The model called `end_turn()` since the loop last settled.
    ended: bool,
    archived: bool,
    /// The agent's `Input` rows after its last request, oldest first.
    unread: Vec<Unread>,
    pending_output: Option<crate::ClaudeOutputBatch>,
    /// Why the notebook last spoke, until the transcript row it produced
    /// arrives to carry it.
    python_wake: Option<crate::WakeFacts>,
    /// When the boundary said to ask it again, if it can change by itself.
    python_recheck: Option<UnixMs>,
    pending_response: PendingInferenceResponse,
    response_id: Option<String>,
    stream_items: BTreeMap<usize, ClaudeStreamItem>,
    response_execs: BTreeMap<usize, rho_agent_types::transcript::ExecId>,
    /// Turns written to Claude Code that it has not echoed yet.
    queued_turns: VecDeque<ClaudeTurn>,
    /// Usage of the in-flight message: `message_start` seeds it,
    /// `message_delta` overlays the final counts (`message_start`'s
    /// `input_tokens` is a streaming placeholder). Snapshots are taken as-is,
    /// never accumulated — stream-json repeats usage per content block.
    turn_usage: Option<rho_claude::protocol::TokenUsage>,
    cancelling: bool,
    pending_rewind: bool,
    execution_generation: u64,
    inference_state: InferenceState,
    status: Arc<RwLock<AgentStatus>>,
    control_rx: mpsc::UnboundedReceiver<ClaudeControl>,
    /// The loop's own address, for the tasks it starts (the sidecar, the
    /// file watch).
    host: Arc<HostClient>,
    /// The agent's head as the host last told it; its spec says the model
    /// and effort of the next launch.
    head: AgentHead,
    head_updates: tokio::sync::watch::Receiver<Option<AgentHead>>,
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
    /// When the oldest person's and other agent's input it carries came.
    human: Option<UnixMs>,
    agent: Option<UnixMs>,
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

    /// `root` is where Claude Code runs and new notebooks start.
    pub(crate) async fn load(
        agent_id: AgentId,
        host: Arc<HostClient>,
        inference: Inference,
        claude: rho_claude::accounts::ClaudePaths,
        root: Utf8PathBuf,
    ) -> anyhow::Result<(ClaudeAgent, Self)> {
        let mut head = host.head().await?;
        let Runtime::Claude { session_id, .. } = head.spec.runtime else {
            anyhow::bail!("cannot load a native agent with the Claude runtime");
        };
        // A moved agent's file is where its old place looked, until it is
        // brought here; found nowhere, the session is one never spoken to.
        let mut sessions = vec![session_id];
        if let Some(rewind) = &head.claude_rewind {
            sessions.push(rewind.session_id);
            sessions.push(rewind.source_session_id);
        }
        for session in sessions {
            if let Err(error) =
                rho_claude::relocate_session_transcript(&claude.projects(), session, &root).await
            {
                eprintln!(
                    "rho-agent: Claude session {session} of {} stays where it was: {error:#}",
                    agent_id.encoded()
                );
            }
        }
        // The transcript's rows come from the stream; a load reads the file
        // only to settle a rewind that was cut short.
        let (session_id, start_mode, pending_rewind) = if let Some(rewind) =
            head.claude_rewind.clone()
        {
            let resumed = rho_claude::read_session_messages_by_id(
                &claude.projects(),
                rewind.session_id,
                &root,
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
                finish_rewind(&host, agent_id, &mut head, rewind.session_id).await?;
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
                host.append(Event::Claude {
                    agent: agent_id,
                    row: ClaudeRow::RewindPending(Some(rewind.clone())),
                })
                .await?;
                let source = rho_claude::read_session_messages_by_id(
                    &claude.projects(),
                    rewind.source_session_id,
                    &root,
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
            let start_mode =
                match rho_claude::find_session_transcript(&claude.projects(), session_id, &root)
                    .await?
                {
                    Some(_) => ClaudeStartMode::Resume,
                    None => ClaudeStartMode::New,
                };
            (session_id, start_mode, false)
        };
        let pending_output = host.claude_pending_output().await?;
        let (archived, unread) = recover(&host.history().await?);
        let status = Arc::new(RwLock::new(AgentStatus::default()));
        let (control, control_rx) = mpsc::unbounded_channel();
        let (outbox, outbound) = Outbox::new();
        host.observe(&status);
        let runtime = Self {
            head_updates: host.names(),
            host,
            claude,
            inference,
            agent_id,
            root,
            session_id,
            start_mode,
            process: None,
            claude_prompt_path: None,
            claude_settings_path: None,
            claude_plugin_dir: None,
            launched: None,
            python: None,
            outbox,
            outbound,
            awaiting: false,
            ended: false,
            archived,
            unread,
            pending_output,
            python_wake: None,
            python_recheck: None,
            pending_response: PendingInferenceResponse::default(),
            response_id: None,
            stream_items: BTreeMap::new(),
            response_execs: BTreeMap::new(),
            queued_turns: VecDeque::new(),
            turn_usage: None,
            cancelling: false,
            pending_rewind,
            execution_generation: 0,
            inference_state: InferenceState::Idle,
            status: Arc::clone(&status),
            control_rx,
            head,
            projection: Projection::default(),
        };
        runtime.published();
        Ok((ClaudeAgent { status, control }, runtime))
    }

    pub(crate) async fn run(&mut self) -> anyhow::Result<()> {
        // Inputs logged before a restart that no request delivered.
        self.deliver_unread().await?;
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
                        changed = self.head_updates.changed() => {
                            changed.context("agent host head connection closed")?;
                            self.head_changed();
                            continue;
                        }
                        control = control_rx.recv() => ClaudeLoopEvent::Control(control),
                        Some(outbound) = self.outbound.recv() => ClaudeLoopEvent::Outbound(outbound),
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
                            let mid_turn =
                                matches!(self.inference_state, InferenceState::Responding);
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
                    Some(outbound) = self.outbound.recv() => { self.outbound(outbound).await?; None },
                    changed = self.head_updates.changed() => {
                        changed.context("agent host head connection closed")?;
                        self.head_changed();
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
                self.claude_row(ClaudeRow::Notice(Notice::Stopped(error.clone())))
                    .await?;
            }
            if settled && std::mem::take(&mut self.ended) {
                self.turn_ended(TurnEnd::EndTurn, None);
            }
            if settled {
                self.published();
            }
        }
    }

    /// The host told a new head: a new spec reaches the next launch.
    fn head_changed(&mut self) {
        let head = self.head_updates.borrow_and_update().clone();
        if let Some(head) = head {
            self.head = head;
            self.published();
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
                    // Freeze scheduling and admission at this serialized
                    // boundary. The outer driver cancels
                    // this future on agent host disconnect.
                    std::future::pending::<()>().await;
                } else {
                    let _ = reply.send(Err(anyhow::anyhow!("agent still has work")));
                }
            }
            ClaudeControl::Input {
                content,
                wake,
                mail,
                logged,
            } => {
                self.host
                    .append(Event::Input {
                        agent: self.agent_id,
                        mail,
                        content: content.clone(),
                        wake,
                    })
                    .await?;
                let _ = logged.send(());
                self.unread.push(Unread {
                    at: UnixMs::now(),
                    content,
                    wake,
                });
                if wake == InputWake::Person {
                    // The person's input ends a wait it answers.
                    self.awaiting = false;
                    if let Some(python) = &mut self.python {
                        python.user_spoke();
                    }
                }
                self.deliver_unread().await?;
            }
            ClaudeControl::Compact => {
                // A slash command goes alone: inputs and retained output
                // wait for the next turn.
                if self.archived {
                    return Ok(());
                }
                if let Err(error) = self.ensure_process().await {
                    return self.fail(error).await;
                }
                let turn = ClaudeTurn {
                    uuid: Uuid::new_v4().to_string(),
                    human: None,
                    agent: None,
                };
                self.hand_off(
                    vec![ContentPart::Text {
                        text: "/compact".into(),
                    }],
                    turn,
                )
                .await?;
            }
            ClaudeControl::Cancel => {
                let kind = self.inference_state.clone();
                let busy = matches!(kind, InferenceState::Responding);
                let queued = self
                    .queued_turns
                    .iter()
                    .map(|turn| turn.uuid.clone())
                    .collect::<Vec<_>>();
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

    /// Sends the unread inputs when one of them wakes the model: a
    /// person's always, reviving an archived agent; another agent's unless
    /// the agent is archived. Quiet ones wait for a request to carry them.
    async fn deliver_unread(&mut self) -> anyhow::Result<()> {
        let person = self
            .unread
            .iter()
            .any(|input| input.wake == InputWake::Person);
        let agent = self
            .unread
            .iter()
            .any(|input| input.wake == InputWake::Agent);
        if person && self.archived {
            self.fresh_notebook().await?;
        }
        if self.archived || !(person || agent) {
            return Ok(());
        }
        let why = if person {
            Wake::Message
        } else {
            Wake::AgentMessage
        };
        self.send_turn(why, None).await
    }

    /// Hands Claude Code one turn: any retained output, `lead`, and every
    /// unread input, and logs the request that delivers them.
    async fn send_turn(&mut self, why: Wake, lead: Option<&str>) -> anyhow::Result<()> {
        let mut steps = Steps::new();
        if let Err(error) = self.ensure_process().await {
            return self.fail(error).await;
        }
        steps.mark("process");
        let output = self.pending_output.clone();
        let mut content = output
            .as_ref()
            .map_or_else(Vec::new, crate::ClaudeOutputBatch::message);
        content.extend(lead.map(|text| ContentPart::Text {
            text: text.to_owned(),
        }));
        let oldest = |wake| {
            self.unread
                .iter()
                .filter(|input| input.wake == wake)
                .map(|input| input.at)
                .min()
        };
        let turn = ClaudeTurn {
            uuid: Uuid::new_v4().to_string(),
            human: oldest(InputWake::Person),
            agent: oldest(InputWake::Agent),
        };
        content.extend(
            self.unread
                .iter()
                .flat_map(|input| input.content.iter().cloned()),
        );
        if !self.hand_off(content, turn).await? {
            return Ok(());
        }
        steps.mark("written");
        self.unread.clear();
        if let Some(batch) = &output {
            self.output_handed_off(batch.id).await?;
        }
        self.claude_row(ClaudeRow::RequestSent {
            why,
            report: Report {
                notebook: output
                    .as_ref()
                    .map_or_else(rho_notebook::Report::default, crate::ClaudeOutputBatch::report),
                ..Default::default()
            },
        })
        .await?;
        steps.mark("logged");
        eprintln!(
            "rho-agent: Claude input of {} in {steps}",
            self.agent_id.encoded()
        );
        Ok(())
    }

    /// Writes one turn to the running process. A failed write fails the
    /// agent and returns `false`.
    async fn hand_off(
        &mut self,
        content: Vec<ContentPart>,
        turn: ClaudeTurn,
    ) -> anyhow::Result<bool> {
        self.cancelling = false;
        let busy = matches!(self.inference_state, InferenceState::Responding);
        if !busy {
            self.execution_generation = self.execution_generation.wrapping_add(1);
        }
        // The queue is Claude Code's, in its process; the echo of the turn
        // takes it off this mirror.
        let uuid = turn.uuid.clone();
        self.queued_turns.push_back(turn);
        self.published();
        // A turn-opening send starts the turn now: waiting for the
        // CLI's first stream event (seconds on a cold spawn) leaves
        // the agent looking idle while it is working.
        if !busy {
            self.pending_response = PendingInferenceResponse::default();
            self.stream_items.clear();
            self.set_streaming_kind();
        }
        let written = self
            .process
            .as_mut()
            .context("Claude Code process is not running")?
            .send_user_content_with_uuid(content, uuid)
            .await;
        match written {
            Ok(()) => Ok(true),
            Err(error) => {
                self.fail(error).await?;
                Ok(false)
            }
        }
    }

    async fn claude_row(&self, row: ClaudeRow) -> anyhow::Result<()> {
        self.host
            .append(Event::Claude {
                agent: self.agent_id,
                row,
            })
            .await?;
        Ok(())
    }

    /// Tell the policy the turn ended. It runs beside the loop, which its
    /// hook may call back into.
    fn turn_ended(&self, end: TurnEnd, error: Option<String>) {
        tokio::spawn(python::on_turn_end(self.agent_id, end, error));
    }

    async fn outbound(&mut self, outbound: Outbound) -> anyhow::Result<()> {
        match outbound {
            Outbound::EndTurn if !self.archived => {
                if let Some(python) = self.python.as_mut() {
                    python.end_turn();
                }
                self.awaiting = true;
                self.ended = true;
            }
            Outbound::EndTurn => {}
            Outbound::Archive => {
                self.archived = true;
                self.claude_row(ClaudeRow::Notice(Notice::Archived)).await?;
                self.close_process().await?;
                if let Some(mut python) = self.python.take() {
                    python.shutdown().await?;
                }
                self.awaiting = false;
                self.ended = false;
                self.python_recheck = None;
                self.queued_turns.clear();
                self.response_id = None;
                self.stream_items.clear();
                self.pending_response = PendingInferenceResponse::default();
                self.set_kind(InferenceState::Idle);
                self.turn_ended(TurnEnd::Archived, None);
            }
        }
        self.published();
        Ok(())
    }

    async fn drain_outbox(&mut self) -> anyhow::Result<()> {
        while let Ok(outbound) = self.outbound.try_recv() {
            self.outbound(outbound).await?;
        }
        Ok(())
    }

    async fn fresh_notebook(&mut self) -> anyhow::Result<()> {
        self.archived = false;
        self.python = None;
        self.claude_row(ClaudeRow::Notice(Notice::FreshNotebook))
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
                self.inference_state,
                InferenceState::Idle | InferenceState::Failed { .. }
            ),
            ":rewind is only available while idle or errored; use :cancel first"
        );
        anyhow::ensure!(
            self.queued_turns.is_empty(),
            ":rewind is not available with queued inputs"
        );
        let rows = self.host.history().await?;
        let (to, echo) = rewind_point(&rows, turns).context("nothing to rewind")?;
        let echo = echo.context("the turn to rewind to has not reached Claude Code")?;
        let root = self.root.clone();
        let (source_session_id, messages) = if self.pending_rewind {
            match self.start_mode {
                ClaudeStartMode::Fork {
                    source_session_id,
                    resume_at,
                } => {
                    let source = rho_claude::read_session_messages_by_id(
                        &self.claude.projects(),
                        source_session_id,
                        &root,
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
                &root,
                rho_claude::SessionMessagesOptions::default(),
            )
            .await?;
            (self.session_id, messages)
        };
        let resume_at = resume_before(&messages, echo)
            .context("the turn to rewind to is not in Claude Code's transcript")?;

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
        self.append(vec![
            Event::Rewound {
                agent: self.agent_id,
                to,
            },
            Event::Claude {
                agent: self.agent_id,
                row: ClaudeRow::RewindPending(Some(ClaudeRewind {
                    source_session_id,
                    session_id: new_session_id,
                    resume_at,
                })),
            },
        ])
        .await?;
        self.pending_rewind = true;
        // The rows from `to` are no longer history: what is unread, and
        // whether the agent is archived, are read again without them.
        let kept = rows.partition_point(|(pos, _)| *pos < to);
        (self.archived, self.unread) = recover(&rows[..kept]);

        self.inference_state = InferenceState::Idle;
        self.response_id = None;
        self.pending_response = PendingInferenceResponse::default();
        self.stream_items.clear();
        self.turn_usage = None;
        self.published();
        Ok(())
    }

    /// Append rows together, at one time.
    async fn append(&self, events: Vec<Event<'static>>) -> anyhow::Result<()> {
        let at = UnixMs::now();
        self.host
            .append_batch(
                events
                    .into_iter()
                    .map(|event| Record { at, event })
                    .collect(),
            )
            .await?;
        Ok(())
    }

    /// The model and effort the spec asks for.
    fn spec(&self) -> anyhow::Result<(Model, Effort)> {
        match self.head.spec.runtime {
            Runtime::Claude { model, effort, .. } => Ok((model, effort)),
            Runtime::Native { .. } => {
                anyhow::bail!("the agent's spec is no longer a Claude runtime")
            }
        }
    }

    /// The model the running process was started with.
    fn running_model(&self) -> Model {
        self.launched
            .as_ref()
            .expect("Claude Code events come from a launched process")
            .model
    }

    async fn ensure_process(&mut self) -> anyhow::Result<()> {
        // The account is global and switching it moves every agent. It lands
        // here, at a turn boundary, rather than the moment it is switched:
        // the process holds a namespace with the old account mounted, and
        // killing it mid-turn would throw away the answer in flight. A new
        // model or effort in the spec lands the same way.
        let (model, effort) = self.spec()?;
        let launch = Launch {
            account: self.host.claude_account().await?,
            model,
            effort,
        };
        if self.process.is_some() {
            if self.launched.as_ref() == Some(&launch) {
                return Ok(());
            }
            eprintln!(
                "rho-agent: restarting {} on Claude account {} with {:?} at {:?}",
                self.agent_id.encoded(),
                launch.account,
                launch.model,
                launch.effort
            );
            self.close_process().await?;
        }
        let cwd = self.root.clone();
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
        let mut options = ClaudeCodeOptions::new(cwd.clone(), model, effort, self.session_id);
        options.session = session;
        options.set_env("RHO_AGENT_ID", self.agent_id.encoded().to_string());
        self.ensure_python()?;
        // SDK-hosted tools use their bare names, matching Rho's native exec.
        options.set_env("CLAUDE_AGENT_SDK_MCP_NO_PREFIX", "1");
        // Tool search would defer the one tool behind a lookup; the deny
        // list in the generated settings removes ToolSearch too. The
        // timeout is the CLI's ceiling on an open exec call.
        options.set_env("ENABLE_TOOL_SEARCH", "false");
        options.set_env(
            "MCP_TOOL_TIMEOUT",
            rho_claude::mcp::EXEC_TIMEOUT.as_millis().to_string(),
        );
        let (account_dir, target) = self
            .configure_claude_home(&mut options, &launch.account)
            .await?;
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
        self.launched = Some(launch);
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
    fn ensure_python(&mut self) -> anyhow::Result<()> {
        if self.python.is_some() {
            return Ok(());
        }
        let (shell, exports) = crate::worker::shared::tools::host_tools(
            &self.root,
            self.agent_id,
            Some(&self.inference),
            Some(&self.host),
            Some(&self.outbox),
            false,
        );
        let notify = Arc::new(tokio::sync::Notify::new());
        let notebook = rho_notebook::Notebook::new(
            shell,
            exports,
            Arc::clone(&notify),
            Some(python::on_notebook(self.agent_id)),
        )
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
        let prompt = python::instructions(self.agent_id)
            .await
            .context("the policy's instructions")?;
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
        let plugin_dir = match &self.claude_plugin_dir {
            Some(dir) => dir,
            None => {
                let dir = tempfile::Builder::new()
                    .prefix("rho-claude-plugin-")
                    .tempdir()
                    .context("create the Claude plugin directory")?;
                self.claude_plugin_dir.insert(dir)
            }
        };
        let plugin_dir = Utf8PathBuf::try_from(plugin_dir.path().to_owned())
            .context("Claude plugin directory path is not valid UTF-8")?;
        rho_claude::plugin::write(&plugin_dir)?;
        options.plugin_dir = Some(plugin_dir);
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
                match assistant_row(
                    &message,
                    usage_model(self.running_model()),
                    &mut self.projection,
                ) {
                    Ok(Some((uuid, line, _))) => self.tell_line(uuid, line).await?,
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
                        Ok(Some((uuid, line, _))) => self.tell_line(uuid, line).await?,
                        Ok(None) => {}
                        Err(error) => eprintln!(
                            "rho-agent: Claude message {} of {} skipped: {error:#}",
                            message.uuid.as_deref().unwrap_or("?"),
                            self.agent_id.encoded()
                        ),
                    }
                }
            }
            rho_claude::ClaudeEvent::Result(message) => {
                let successful = !message.is_error;
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
                    self.pending_response = PendingInferenceResponse::default();
                    self.stream_items.clear();
                    self.response_id = None;
                    self.set_kind(InferenceState::Idle);
                } else if message.is_error {
                    self.fail(anyhow::anyhow!("{}", message.failure_message()))
                        .await?;
                } else {
                    // CLI result prose is provider output, not a message to the
                    // human. Queued sends run next inside
                    // the CLI: staying in the
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
                        model: usage_model(self.running_model()),
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
                            let now = UnixMs::now();
                            self.claude_row(ClaudeRow::ExecAdmitted {
                                call: rho_agent_types::transcript::ExecCall {
                                    id: exec_id.clone(),
                                    source: source.clone(),
                                },
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
        let now = UnixMs::now();
        let idle = matches!(self.inference_state, InferenceState::Idle)
            && self.process.is_some()
            && self.queued_turns.is_empty()
            && !self.pending_rewind;
        let oldest_human = self.queued_turns.iter().filter_map(|turn| turn.human).min();
        let oldest_agent = self.queued_turns.iter().filter_map(|turn| turn.agent).min();
        let Some(host) = self.python.as_mut() else {
            return Ok(());
        };
        let available = host.has_pending() || idle;
        match host.decide(
            available,
            oldest_human,
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
                    // The answer is a request too: it carries every unread
                    // input, which only quiet ones can be here.
                    if let Some(content) = result["content"].as_array_mut() {
                        content.extend(
                            self.unread
                                .iter()
                                .flat_map(|input| input.content.iter().map(mcp_item)),
                        );
                    }
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
                    )
                    .await?;
                    if self.respond_control(&pending.request_id, Ok(reply)).await {
                        self.unread.clear();
                        self.output_handed_off(batch).await?;
                        self.claude_row(ClaudeRow::RequestSent {
                            why: Wake::Returned,
                            report: Report {
                                notebook: delivered.report(),
                                ..Default::default()
                            },
                        })
                        .await?;
                        self.observe_exec(
                            pending.exec_id,
                            rho_agent_types::ExecMilestone::HandedOff,
                        )
                        .await?;
                    } else {
                        // The outbox survives both this process and the
                        // notebook. A future send carries its output, never
                        // reruns its source.
                        self.close_process().await?;
                        self.fail(anyhow::anyhow!(
                            "Claude Code output transport failed; notebook output is retained"
                        ))
                        .await?;
                    }
                } else if idle {
                    let mut drained = host.drain_idle();
                    let correction = host.prose_correction();
                    let output = !drained.is_empty() || self.pending_output.is_some();
                    if !output && !correction && wake.trigger != crate::WakeTrigger::Checkin {
                        return Ok(());
                    }
                    let lead = if correction {
                        Some("Your last response had no exec call. Text outside a call reaches nobody.")
                    } else if !output {
                        Some("Check-in: nothing new.")
                    } else {
                        None
                    };
                    let why = if output {
                        self.record_output(&mut drained, wake, now).await?;
                        Wake::Returned
                    } else if correction {
                        Wake::Prose
                    } else {
                        Wake::Checkin
                    };
                    self.send_turn(why, lead).await?;
                }
            }
        }
        Ok(())
    }

    async fn record_output(
        &mut self,
        drained: &mut python_host::Drained,
        wake: crate::WakeFacts,
        at: UnixMs,
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
        self.claude_row(ClaudeRow::Output {
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
        self.claude_row(ClaudeRow::OutputHandedOff { id }).await?;
        self.pending_output = None;
        Ok(())
    }

    /// Stops the notebook's cells and answers an open exec call as
    /// cancelled, ahead of the interrupt that abandons it on the CLI's side.
    async fn cancel_python(&mut self) -> anyhow::Result<()> {
        let Some(host) = self.python.as_mut() else {
            return Ok(());
        };
        if let Some(pending) = host.cancel(UnixMs::now()) {
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
                self.queued_turns.remove(index);
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

    /// One row of the conversation, from the stream: appended, with why
    /// the notebook spoke when the row is what it said.
    async fn tell_line(&mut self, uuid: Uuid, line: TranscriptLine) -> anyhow::Result<()> {
        // The notebook's reason for speaking rides on the row it produced:
        // an exec call's results, or the message of output an idle model
        // was woken with.
        let wake = match &line {
            TranscriptLine::ToolResults { .. } | TranscriptLine::User { .. } => {
                self.python_wake.take()
            }
            TranscriptLine::Assistant { .. } | TranscriptLine::Compacted { .. } => None,
        };
        self.claude_row(ClaudeRow::Transcript { uuid, line, wake })
            .await
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
        let root = self.root.clone();
        let messages = rho_claude::read_session_messages_by_id(
            &self.claude.projects(),
            self.session_id,
            &root,
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
        finish_rewind(&self.host, self.agent_id, &mut self.head, self.session_id).await?;
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
        self.claude_row(ClaudeRow::RewindPending(Some(ClaudeRewind {
            source_session_id,
            session_id: self.session_id,
            resume_at,
        })))
        .await
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
    /// it enters context: the echo of a turn this loop sent takes it off
    /// the queue; the echo's row (next) is the message in the conversation.
    fn activate_turn_from_user_echo(&mut self, uuid: Option<&str>) -> bool {
        let Some(uuid) = uuid else { return false };
        let Some(index) = self.queued_turns.iter().position(|turn| turn.uuid == uuid) else {
            return false;
        };
        self.queued_turns.remove(index);
        self.published();
        true
    }

    /// A compaction: its row, then what the policy gives the model back,
    /// as quiet inputs the next request delivers.
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
        let (uuid, line, _) = compacted_row(uuid.as_deref(), compact_metadata.as_ref());
        self.tell_line(uuid, line).await?;
        let texts = python::on_compaction(self.agent_id)
            .await
            .unwrap_or_else(|error| {
                eprintln!("rho: {error:#}");
                Vec::new()
            });
        for text in texts {
            let content = vec![ContentPart::Text { text }];
            self.host
                .append(Event::Input {
                    agent: self.agent_id,
                    mail: None,
                    content: content.clone(),
                    wake: InputWake::Quiet,
                })
                .await?;
            self.unread.push(Unread {
                at: UnixMs::now(),
                content,
                wake: InputWake::Quiet,
            });
        }
        self.published();
        Ok(())
    }

    /// The loop's state as readers see it.
    fn snapshot(&self) -> AgentStatus {
        claude_status(
            &self.inference_state,
            self.python.as_ref(),
            self.awaiting,
            self.archived,
            self.response_id.as_deref(),
            &self.stream_items,
            self.queued_turns.len(),
        )
    }

    fn runtime_snapshot(&self) -> RuntimeState {
        claude_runtime(
            &self.inference_state,
            self.python.as_ref(),
            self.awaiting,
            self.archived,
        )
    }

    /// The loop's state changed: publish the status, and say what changed
    /// in the tail if anyone is looking. Every row this loop writes is
    /// committed before the state moves, so the tail follows its row.
    fn published(&self) {
        *self.status.write().expect("poison") = self.snapshot();
        self.host.published();
    }

    fn set_kind(&mut self, kind: InferenceState) {
        self.inference_state = kind;
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
            host.failed(UnixMs::now(), Arc::from(error.to_string()));
        }
        // The row first, so what Claude had said is kept and the tail
        // the loop tells next follows it.
        // Fragments live in stream_items; materialize the latest incomplete
        // provider items once if the stream fails mid-block.
        refresh_pending_partial(&mut self.pending_response, &self.stream_items);
        let partial = std::mem::take(&mut self.pending_response);
        self.host
            .append(Event::Failed {
                agent: self.agent_id,
                partial,
                error: Cow::Owned(error.to_string()),
                retrying: false,
            })
            .await?;
        self.ended = false;
        self.turn_ended(TurnEnd::Failed, Some(error.to_string()));
        self.response_id = None;
        self.stream_items.clear();
        self.set_kind(InferenceState::Failed {
            error: error.to_string(),
        });
        Ok(())
    }

    async fn observe_exec(
        &self,
        exec: rho_agent_types::transcript::ExecId,
        milestone: rho_agent_types::ExecMilestone,
    ) -> anyhow::Result<()> {
        self.host
            .append(Event::ExecObserved {
                agent: self.agent_id,
                exec,
                milestone,
            })
            .await?;
        Ok(())
    }

    async fn handle_stream_event(
        &mut self,
        event: rho_claude::protocol::MessageStreamEvent,
    ) -> anyhow::Result<()> {
        match &event {
            rho_claude::protocol::MessageStreamEvent::MessageStart { .. } => {
                self.response_execs.clear()
            }
            rho_claude::protocol::MessageStreamEvent::ContentBlockStart {
                index,
                content_block: rho_claude::protocol::StreamContentBlock::ToolUse { id, name, .. },
            } if matches!(name.as_str(), "exec" | "mcp__py__exec") => {
                let id = rho_agent_types::transcript::ExecId::try_from(id.as_str())?;
                self.response_execs.insert(*index, id.clone());
                self.observe_exec(id, rho_agent_types::ExecMilestone::FirstBlock)
                    .await?;
            }
            rho_claude::protocol::MessageStreamEvent::ContentBlockStop { index } => {
                if let Some(id) = self.response_execs.get(index).cloned() {
                    self.observe_exec(id, rho_agent_types::ExecMilestone::ArgumentsFinished)
                        .await?;
                }
            }
            rho_claude::protocol::MessageStreamEvent::MessageStop => {
                for id in self.response_execs.values().cloned().collect::<Vec<_>>() {
                    self.observe_exec(id, rho_agent_types::ExecMilestone::ResponseFinished)
                        .await?;
                }
            }
            _ => {}
        }
        match event {
            rho_claude::protocol::MessageStreamEvent::MessageStart { message } => {
                self.pending_response = PendingInferenceResponse::default();
                self.stream_items.clear();
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
                // A call the model misnamed is not a context item. Claude Code
                // answers it with an error, so the turn goes on without it.
                let Ok(streaming) = item.to_streaming_context_item() else {
                    return Ok(());
                };
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
                    self.inference_state = InferenceState::Responding;
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

/// What the agent's visible rows say the loop resumes with: whether it is
/// archived, and the inputs no request has delivered (those after its
/// last request).
fn recover(rows: &[(Wpos, Record<'_>)]) -> (bool, Vec<Unread>) {
    let mut archived = false;
    let mut unread = Vec::new();
    for (_, record) in rows {
        match &record.event {
            Event::Input { content, wake, .. } => unread.push(Unread {
                at: record.at,
                content: content.clone(),
                wake: *wake,
            }),
            Event::Claude { row, .. } => match row {
                ClaudeRow::RequestSent { .. } => unread.clear(),
                ClaudeRow::Notice(Notice::Archived) => archived = true,
                ClaudeRow::Notice(Notice::FreshNotebook) => archived = false,
                _ => {}
            },
            _ => {}
        }
    }
    (archived, unread)
}

/// Where a rewind of `turns` goes: the position of the `turns`-th last
/// input from a person (the first one, when there are fewer), and the
/// uuid of the first user line after it, which is the turn that took it
/// to Claude Code. `None` with no person's input to go back to.
fn rewind_point(rows: &[(Wpos, Record<'_>)], turns: u32) -> Option<(Wpos, Option<Uuid>)> {
    let people: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, (_, record))| {
            matches!(
                record.event,
                Event::Input {
                    wake: InputWake::Person,
                    ..
                }
            )
        })
        .map(|(index, _)| index)
        .collect();
    let index = *people.get(people.len().saturating_sub(turns as usize))?;
    let echo = rows[index..].iter().find_map(|(_, record)| match &record.event {
        Event::Claude {
            row:
                ClaudeRow::Transcript {
                    uuid,
                    line: TranscriptLine::User { .. },
                    ..
                },
            ..
        } => Some(*uuid),
        _ => None,
    });
    Some((rows[index].0, echo))
}

/// Where a session forked to forget the user message `uuid` and all after
/// it resumes: the last assistant message before it, or `None` for a
/// fresh session. `None` outside when the message is not there.
fn resume_before(messages: &[rho_claude::SessionMessage], uuid: Uuid) -> Option<Option<Uuid>> {
    let selected = messages.iter().position(|message| message.uuid == uuid)?;
    Some(
        messages[..selected]
            .iter()
            .rfind(|message| message.kind == rho_claude::SessionMessageKind::Assistant)
            .map(|message| message.uuid),
    )
}

/// The rewound session `session_id` materialized: it is the spec's
/// session now, and the rewind is no longer pending.
async fn finish_rewind(
    host: &HostClient,
    agent: AgentId,
    head: &mut AgentHead,
    session_id: Uuid,
) -> anyhow::Result<()> {
    let mut spec: AgentSpec = head.spec.clone();
    let Runtime::Claude {
        session_id: session,
        ..
    } = &mut spec.runtime
    else {
        anyhow::bail!("the agent's spec is no longer a Claude runtime");
    };
    *session = session_id;
    let at = UnixMs::now();
    host.append_batch(vec![
        Record {
            at,
            event: Event::AgentChanged {
                agent,
                spec: spec.clone(),
            },
        },
        Record {
            at,
            event: Event::Claude {
                agent,
                row: ClaudeRow::RewindPending(None),
            },
        },
    ])
    .await?;
    head.spec = spec;
    head.claude_rewind = None;
    Ok(())
}

fn usage_model(model: Model) -> crate::log::AgentUsageModel {
    match model {
        Model::Opus => crate::log::AgentUsageModel::OPUS,
        Model::Fable | Model::Sonnet => crate::log::AgentUsageModel::FABLE,
    }
}

/// An input's part as an item of an MCP tool result.
fn mcp_item(part: &ContentPart) -> serde_json::Value {
    use base64::Engine as _;
    match part {
        ContentPart::Text { text } => rho_claude::mcp::text_item(text),
        ContentPart::Image { media_type, data } => serde_json::json!({
            "type": "image",
            "data": base64::engine::general_purpose::STANDARD.encode(data),
            "mimeType": media_type,
        }),
    }
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
    use rho_agent_types::{WorksetId, WorksetKind};

    use super::*;
    use crate::store::StoreWrite as _;

    fn agent(n: u64) -> AgentId {
        AgentId::from_counter(n, &rho_agent_types::AgentIdDomain(0)).unwrap()
    }

    fn row(pos: u64, event: Event<'static>) -> (Wpos, Record<'static>) {
        (
            Wpos(pos),
            Record {
                at: UnixMs(pos),
                event,
            },
        )
    }

    fn input(agent: AgentId, text: &str, wake: InputWake) -> Event<'static> {
        Event::Input {
            agent,
            mail: None,
            content: vec![ContentPart::Text { text: text.into() }],
            wake,
        }
    }

    fn sent(agent: AgentId) -> Event<'static> {
        Event::Claude {
            agent,
            row: ClaudeRow::RequestSent {
                why: Wake::Message,
                report: Report::default(),
            },
        }
    }

    fn echo(agent: AgentId, uuid: Uuid) -> Event<'static> {
        Event::Claude {
            agent,
            row: ClaudeRow::Transcript {
                uuid,
                line: TranscriptLine::User { text: "x".into() },
                wake: None,
            },
        }
    }

    fn notice(agent: AgentId, notice: Notice) -> Event<'static> {
        Event::Claude {
            agent,
            row: ClaudeRow::Notice(notice),
        }
    }

    #[tokio::test]
    async fn quota_failure_reaches_live_status_and_the_persisted_failure() {
        let directory = tempfile::tempdir().unwrap();
        let db = rho_db::RhoDb::open(directory.path().join("rho.redb"));
        let agent_id = agent(1);
        let workset = WorksetId([1; 6]);
        let mut write = db.write().await;
        write.init_store_tables();
        write.create_workset(workset, UnixMs(1), WorksetKind::Isolated, String::new());
        write
            .append(
                workset,
                &Record {
                    at: UnixMs(2),
                    event: Event::AgentCreated {
                        agent: agent_id,
                        spec: AgentSpec {
                            runtime: Runtime::Claude {
                                model: Model::Opus,
                                effort: Effort::High,
                                session_id: Uuid::new_v4(),
                            },
                            props: Default::default(),
                        },
                    },
                },
            )
            .unwrap();
        write.commit();
        let host = crate::testing::services_pair(
            db.clone(),
            crate::inference::testing::accounts(),
            agent_id,
            std::sync::Weak::new(),
        );
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
        let (agent, mut runtime) = ClaudeLoop::load(
            agent_id,
            host.clone(),
            crate::inference::testing::backend(),
            rho_claude::accounts::ClaudePaths::at(root.join("claude")),
            root,
        )
        .await
        .unwrap();
        let explanation = "You've hit your session limit · resets 5:30pm (UTC)";
        runtime
            .handle_event(rho_claude::ClaudeEvent::Result(
                serde_json::from_value(serde_json::json!({
                    "subtype": "success",
                    "is_error": true,
                    "result": explanation
                }))
                .unwrap(),
            ))
            .await
            .unwrap();
        assert_eq!(
            agent.status().runtime.inference,
            InferenceState::Failed {
                error: explanation.to_owned(),
            }
        );
        assert!(
            host.history()
                .await
                .unwrap()
                .into_iter()
                .any(|(_, record)| matches!(
                    record.event,
                    Event::Failed { error, .. } if error == explanation
                ))
        );
    }

    #[test]
    fn unread_inputs_are_those_after_the_last_request_and_archive_follows_its_notices() {
        let a = agent(1);
        let rows = vec![
            row(1, input(a, "delivered", InputWake::Person)),
            row(2, sent(a)),
            row(3, notice(a, Notice::Archived)),
            row(4, input(a, "quiet", InputWake::Quiet)),
            row(5, notice(a, Notice::FreshNotebook)),
            row(6, input(a, "mail", InputWake::Agent)),
        ];
        let (archived, unread) = recover(&rows);
        assert!(!archived, "a fresh notebook revives the agent");
        assert_eq!(
            unread,
            vec![
                Unread {
                    at: UnixMs(4),
                    content: vec![ContentPart::Text {
                        text: "quiet".into()
                    }],
                    wake: InputWake::Quiet,
                },
                Unread {
                    at: UnixMs(6),
                    content: vec![ContentPart::Text {
                        text: "mail".into()
                    }],
                    wake: InputWake::Agent,
                },
            ]
        );
        let (archived, unread) = recover(&rows[..4]);
        assert!(archived);
        assert_eq!(unread.len(), 1, "the archived agent's input waits");
        let (_, unread) = recover(&[row(1, input(a, "x", InputWake::Person)), row(2, sent(a))]);
        assert!(unread.is_empty());
    }

    #[test]
    fn a_rewind_counts_only_peoples_inputs_and_finds_the_turn_that_carried_them() {
        let a = agent(1);
        let (first, mail, second) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let rows = vec![
            row(1, input(a, "first", InputWake::Person)),
            row(2, sent(a)),
            row(3, echo(a, first)),
            row(4, input(a, "mail", InputWake::Agent)),
            row(5, sent(a)),
            row(6, echo(a, mail)),
            row(7, input(a, "quiet", InputWake::Quiet)),
            row(8, input(a, "second", InputWake::Person)),
            row(9, sent(a)),
            row(10, echo(a, second)),
            row(11, input(a, "not yet", InputWake::Person)),
        ];
        assert_eq!(rewind_point(&rows, 1), Some((Wpos(11), None)));
        assert_eq!(rewind_point(&rows, 2), Some((Wpos(8), Some(second))));
        assert_eq!(rewind_point(&rows, 3), Some((Wpos(1), Some(first))));
        assert_eq!(
            rewind_point(&rows, 9),
            Some((Wpos(1), Some(first))),
            "more turns than there are go back to the first"
        );
        assert_eq!(rewind_point(&rows[3..7], 1), None);
    }

    #[test]
    fn a_fork_resumes_at_the_last_assistant_message_before_the_forgotten_turn() {
        let message = |kind, uuid| rho_claude::SessionMessage {
            kind,
            uuid,
            session_id: Uuid::nil(),
            message: serde_json::Value::Null,
            parent_tool_use_id: None,
            timestamp: None,
        };
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        use rho_claude::SessionMessageKind::{Assistant, User};
        let messages = vec![
            message(User, ids[0]),
            message(Assistant, ids[1]),
            message(Assistant, ids[2]),
            message(User, ids[3]),
            message(Assistant, ids[4]),
        ];
        assert_eq!(resume_before(&messages, ids[3]), Some(Some(ids[2])));
        assert_eq!(resume_before(&messages, ids[0]), Some(None));
        assert_eq!(resume_before(&messages, Uuid::new_v4()), None);
    }

    #[test]
    fn pending_context_copies_only_finished_blocks_or_failure_partials() {
        use rho_agent_types::transcript::{StreamingContextItem, StreamingContextItemState};
        let mut items = BTreeMap::new();
        let mut pending = PendingInferenceResponse::default();
        let call = ClaudeStreamItem::ToolUse {
            id: "call-1".into(),
            name: "exec".into(),
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
            None,
        )
        .unwrap();
        let mut python = python_host::PythonHost::new(notebook, notify);
        let mut blocks = BTreeMap::new();
        blocks.insert(
            4,
            ClaudeStreamItem::ToolUse {
                id: "call-1".into(),
                name: "exec".into(),
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

}

/// How long each step of taking a message in took, for the log: the time
/// from a message to Claude Code is latency every reply pays.
struct Steps {
    start: std::time::Instant,
    last: std::time::Instant,
    marks: Vec<(&'static str, Duration)>,
}

impl Steps {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Self {
            start: now,
            last: now,
            marks: Vec::new(),
        }
    }

    fn mark(&mut self, step: &'static str) {
        let now = std::time::Instant::now();
        self.marks.push((step, now - self.last));
        self.last = now;
    }
}

impl std::fmt::Display for Steps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}:", self.last - self.start)?;
        for (step, took) in &self.marks {
            write!(f, " {step} {took:?}")?;
        }
        Ok(())
    }
}
