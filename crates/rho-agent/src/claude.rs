//! Claude Code agent support.
//!
//! `rho-claude` owns the Claude Code protocol. This module owns the projection
//! from Claude protocol/transcript messages into Rho agent vocabulary.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::Write as _;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Context as _;
use async_stream::stream;
use camino::Utf8PathBuf;
use futures::Stream;
use rho_claude::{ClaudeCode, ClaudeCodeOptions, Effort, Model, Session};
use rho_core::{ContentPart, ContextBlock, ContextItemEvent, PendingInferenceResponse};
use rho_db::{RhoDb, WriteTxn};
use rho_inference::Inference;
use tokio::sync::{Notify, mpsc, oneshot};
use uuid::Uuid;

use crate::db::{
    AgentEventPos, AgentId, AgentPresentationCache, AgentPresentationUpdate,
    AgentProfileWriteTxnExt, AgentReadTxnExt, AgentRole, AgentRoleSessionProfile as _,
    AgentRuntime, AgentWriteTxnExt, ClaudeRewind, EngineerIntelligence, SessionBinding, UnixMillis,
};
use crate::multi_agent_tools::MultiAgentTools;
use crate::{
    AgentEvent, AgentState, AgentStateKind, FailedInferenceResponse, InputQueues, MessageDelivery,
    PresentationSpeaker, QueuedItem, QueuedItemKind, StartWorkdir, system_prompt,
};

mod projection;

use projection::{
    ClaudeStreamItem, assistant_message_to_block, assistant_presentation_source,
    presentation_source, queued_user_presentation_source, transcript_messages_to_context,
    user_output_to_block,
};

use crate::lazy::Lazy;

/// Last real assistant-message timestamp in a persisted Claude transcript.
/// Missing or malformed timestamps are ignored rather than replaced with an
/// invented chronology value.
pub fn last_assistant_message_at(
    messages: &[rho_claude::SessionMessage],
) -> Option<rho_core::UnixMs> {
    messages
        .iter()
        .filter(|message| message.kind == rho_claude::SessionMessageKind::Assistant)
        .filter_map(|message| message.timestamp.as_deref())
        .filter_map(|timestamp| chrono::DateTime::parse_from_rfc3339(timestamp).ok())
        .filter_map(|timestamp| timestamp.timestamp_millis().try_into().ok())
        .map(rho_core::UnixMs)
        .max()
}

/// Persists a legacy turn-end fact from transcript ground truth, if present.
pub fn backfill_last_turn_ended_from_claude_messages(
    write: &mut WriteTxn,
    agent_id: AgentId,
    messages: &[rho_claude::SessionMessage],
) -> bool {
    let Some(at) = last_assistant_message_at(messages) else {
        return false;
    };
    write.backfill_agent_last_turn_ended(agent_id, at)
}

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
        inference: Inference,
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
        let next_event = write.create_agent(
            UnixMillis::now(),
            agent_id,
            display_name,
            entries
                .iter()
                .map(|workspace| workspace.info().clone())
                .collect(),
            role,
            mode,
            AgentRuntime::Claude { session_id },
            parent,
        );
        write.commit();

        let pool_events = pool.clone();
        let multi_agent = pool
            .upgrade()
            .map(|_| MultiAgentTools::new(pool, agent_id, parent));
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
                agent_id,
                Arc::new(Lazy::ready(view)),
                model,
                effort,
                session_id,
                state,
                ClaudeStartMode::New,
                false,
                multi_agent,
                pool_events,
                role,
                next_event,
                HashSet::new(),
            ),
        ))
    }

    pub(crate) async fn load(
        db: RhoDb,
        inference: Inference,
        agent_id: AgentId,
        view: Arc<Lazy<Arc<rho_workspaces::View>>>,
        pool: std::sync::Weak<crate::pool::AgentPool>,
    ) -> anyhow::Result<Self> {
        let record = db.read().get_agent(agent_id);
        let parent_agent = db.read().agent_attention(agent_id).parent_agent;
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
        let primary_repo = record.primary_workdir().repo().to_owned();
        let (session_id, messages, start_mode, pending_rewind, context_used) = if let Some(rewind) =
            record.config.claude_rewind
        {
            let resumed = rho_claude::read_session_messages_by_id(
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
                let context_used =
                    rho_claude::read_session_context_used_by_id(rewind.session_id, &primary_repo)
                        .await?;
                (
                    rewind.session_id,
                    resumed,
                    ClaudeStartMode::Resume,
                    false,
                    context_used,
                )
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
                    rewind.source_session_id,
                    &primary_repo,
                    rho_claude::SessionMessagesOptions::default(),
                )
                .await?;
                let messages = match rewind.resume_at {
                    Some(resume_at) => {
                        rho_claude::session_messages_through_assistant(&source, resume_at)
                            .context("Claude rewind point is no longer in the transcript")?
                    }
                    None => Vec::new(),
                };
                let start_mode = match rewind.resume_at {
                    Some(resume_at) => ClaudeStartMode::Fork {
                        source_session_id: rewind.source_session_id,
                        resume_at,
                    },
                    None => ClaudeStartMode::New,
                };
                let context_used =
                    rho_claude::last_assistant_usage(&messages).map(|usage| usage.context_total());
                (session_id, messages, start_mode, true, context_used)
            }
        } else {
            let messages = rho_claude::read_session_messages_by_id(
                session_id,
                &primary_repo,
                rho_claude::SessionMessagesOptions::default(),
            )
            .await?;
            let start_mode = if messages.is_empty() {
                ClaudeStartMode::New
            } else {
                ClaudeStartMode::Resume
            };
            let context_used =
                rho_claude::read_session_context_used_by_id(session_id, &primary_repo).await?;
            (session_id, messages, start_mode, false, context_used)
        };
        let (next_event, known_presentation_sources, _, rebuilt_cache) =
            reconcile_claude_presentation_sources(&db, agent_id, &messages).await?;
        if let Some(cache) = rebuilt_cache
            && let Some(pool) = pool.upgrade()
        {
            pool.publish_presentation_changed(agent_id, cache.generated_title, cache.activity);
        }
        let blocks = transcript_messages_to_context(&messages)?;
        let state = AgentState {
            blocks,
            queued_inputs: InputQueues::default(),
            kind: AgentStateKind::Idle,
            context_used,
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
            next_event,
            known_presentation_sources,
        ))
    }

    #[expect(clippy::too_many_arguments)]
    fn new(
        db: RhoDb,
        inference: Inference,
        agent_id: AgentId,
        view: Arc<Lazy<Arc<rho_workspaces::View>>>,
        model: Model,
        effort: Effort,
        session_id: Uuid,
        state: AgentState,
        start_mode: ClaudeStartMode,
        pending_rewind: bool,
        multi_agent: Option<MultiAgentTools>,
        pool_events: std::sync::Weak<crate::pool::AgentPool>,
        role: crate::db::AgentRole,
        next_event: AgentEventPos,
        known_presentation_sources: HashSet<Uuid>,
    ) -> Self {
        let state = Arc::new(RwLock::new(state));
        let notify = Arc::new(Notify::new());
        let input_seq = Arc::new(AtomicU64::new(0));
        let wait_baseline_seq = Arc::new(AtomicU64::new(0));
        let input_notify = Arc::new(Notify::new());
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
            crate::presentation::Session::new(inference),
        ));
        let loop_state = ClaudeLoop {
            db,
            presentation_session,
            agent_id,
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
            cancelling: false,
            pending_rewind,
            execution_generation: 0,
            state: Arc::clone(&state),
            notify: Arc::clone(&notify),
            wait_baseline_seq: Arc::clone(&wait_baseline_seq),
            input_notify: Arc::clone(&input_notify),
            control_rx,
            presentation_control: control.downgrade(),
            multi_agent,
            pool_events,
            role,
            next_event,
            known_presentation_sources,
            presentation: ClaudePresentationState::default(),
            last_presentation_source,
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
        self.send_user_content(vec![ContentPart::Text { text: text.into() }]);
    }

    pub fn send_user_content(&self, content: Vec<ContentPart>) {
        let seq = self.input_seq.fetch_add(1, Ordering::AcqRel) + 1;
        let uuid = Uuid::new_v4().to_string();
        self.input_notify.notify_waiters();
        let _ = self.control.send(ClaudeControl::UserMessage {
            content,
            seq,
            uuid,
            user: true,
            accepted: None,
        });
    }

    pub async fn send_user_content_accepted(
        &self,
        content: Vec<ContentPart>,
    ) -> anyhow::Result<()> {
        self.send_content_accepted(content, true).await
    }

    /// Deliver agent mail and wait for acceptance into Rho's volatile Claude
    /// queue. A process or daemon restart may lose it before Claude records it.
    pub async fn send_agent_message_accepted(&self, text: String) -> anyhow::Result<()> {
        self.send_content_accepted(vec![ContentPart::Text { text }], false)
            .await
    }

    async fn send_content_accepted(
        &self,
        content: Vec<ContentPart>,
        user: bool,
    ) -> anyhow::Result<()> {
        let seq = self.input_seq.fetch_add(1, Ordering::AcqRel) + 1;
        let uuid = Uuid::new_v4().to_string();
        let (accepted, reply) = oneshot::channel();
        self.input_notify.notify_waiters();
        self.control
            .send(ClaudeControl::UserMessage {
                content,
                seq,
                uuid,
                user,
                accepted: Some(accepted),
            })
            .map_err(|_| anyhow::anyhow!("Claude agent stopped before accepting mail"))?;
        reply
            .await
            .map_err(|_| anyhow::anyhow!("Claude agent stopped before accepting mail"))?
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

    pub(crate) fn watch_presentation(&self) -> crate::presentation::Watch {
        let _ = self
            .control
            .send(ClaudeControl::PresentationWatch { watching: true });
        let control = self.control.clone();
        crate::presentation::Watch::new(move || {
            let _ = control.send(ClaudeControl::PresentationWatch { watching: false });
        })
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
    Fork {
        source_session_id: Uuid,
        resume_at: Uuid,
    },
}

enum ClaudeControl {
    UserMessage {
        content: Vec<ContentPart>,
        seq: u64,
        uuid: String,
        user: bool,
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
}

struct ClaudeLoop {
    db: RhoDb,
    /// The agent's one persistent Luna session, shared by activity updates
    /// and turn reports so both keep one prompt prefix warm.
    presentation_session: Arc<tokio::sync::Mutex<crate::presentation::Session>>,
    agent_id: AgentId,
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
    cancelling: bool,
    pending_rewind: bool,
    execution_generation: u64,
    state: Arc<RwLock<AgentState>>,
    notify: Arc<Notify>,
    wait_baseline_seq: Arc<AtomicU64>,
    input_notify: Arc<Notify>,
    control_rx: mpsc::UnboundedReceiver<ClaudeControl>,
    presentation_control: mpsc::WeakUnboundedSender<ClaudeControl>,
    multi_agent: Option<MultiAgentTools>,
    pool_events: std::sync::Weak<crate::pool::AgentPool>,
    role: crate::db::AgentRole,
    next_event: AgentEventPos,
    known_presentation_sources: HashSet<Uuid>,
    presentation: ClaudePresentationState,
    last_presentation_source: Option<AgentEventPos>,
}

#[derive(Default)]
struct ClaudePresentationState {
    watches: usize,
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
    input_seq: u64,
    content: Arc<Vec<ContentPart>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ClaudePresentationSource {
    source_id: Uuid,
    speaker: PresentationSpeaker,
    text: String,
}

struct ClaudePresentationReconciliation {
    sources: Vec<ClaudePresentationSource>,
    persisted: Vec<(AgentEventPos, ClaudePresentationSource)>,
    common: usize,
    next_event: AgentEventPos,
    previous_cache: AgentPresentationCache,
}

fn claude_presentation_sources(
    messages: &[rho_claude::SessionMessage],
) -> anyhow::Result<Vec<ClaudePresentationSource>> {
    messages
        .iter()
        .filter_map(|message| presentation_source(message).transpose())
        .filter_map(|source| match source {
            Ok((source_id, speaker, text)) => crate::presentation::canonical_source_text(&text)
                .map(|text| {
                    Ok(ClaudePresentationSource {
                        source_id,
                        speaker,
                        text,
                    })
                }),
            Err(error) => Some(Err(error)),
        })
        .collect()
}

/// Plan reconciliation of the selected external Claude JSONL chain into
/// durable, bounded source events. A crash between Claude's JSONL write and
/// our event write is repaired on load; a shortened/diverged selected chain
/// forks the same lineage that validates Luna cache updates.
fn prepare_claude_presentation_reconciliation(
    db: &RhoDb,
    agent_id: AgentId,
    messages: &[rho_claude::SessionMessage],
) -> anyhow::Result<ClaudePresentationReconciliation> {
    let sources = claude_presentation_sources(messages)?;
    let read = db.read();
    let record = read.get_agent(agent_id);
    let previous_cache = AgentPresentationCache {
        generated_title: record.generated_title,
        activity: record.activity,
    };
    let (next_event, records) = read.agent_event_records(agent_id);
    let persisted = records
        .iter()
        .filter_map(|(position, event)| match event {
            AgentEvent::ClaudePresentationSource {
                source_id,
                speaker,
                text,
            } => Some((
                *position,
                ClaudePresentationSource {
                    source_id: *source_id,
                    speaker: *speaker,
                    text: text.to_string(),
                },
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    let common = persisted
        .iter()
        .zip(&sources)
        .take_while(|((_, persisted), source)| persisted == *source)
        .count();
    Ok(ClaudePresentationReconciliation {
        sources,
        persisted,
        common,
        next_event,
        previous_cache,
    })
}

impl ClaudePresentationReconciliation {
    fn apply(
        &self,
        write: &mut WriteTxn,
        now: UnixMillis,
        agent_id: AgentId,
    ) -> (AgentEventPos, Option<AgentPresentationCache>) {
        let (mut next_event, rebuilt_cache) = if self.common < self.persisted.len() {
            let next = write.fork_agent_lineage(now, agent_id, self.persisted[self.common].0);
            let cache = write.rebuild_agent_presentation_cache(now, agent_id);
            (next, (cache != self.previous_cache).then_some(cache))
        } else {
            (self.next_event, None)
        };
        for source in self.sources.iter().skip(self.common) {
            next_event = write.append_agent_event(
                next_event,
                &AgentEvent::ClaudePresentationSource {
                    source_id: source.source_id,
                    speaker: source.speaker,
                    text: Cow::Borrowed(&source.text),
                },
            );
        }
        (next_event, rebuilt_cache)
    }

    fn state(
        &self,
        db: &RhoDb,
        agent_id: AgentId,
        next_event: AgentEventPos,
    ) -> (AgentEventPos, HashSet<Uuid>, Option<AgentEventPos>) {
        let records = db
            .read()
            .agent_presentation_source_tail(agent_id, crate::PRESENTATION_SOURCE_TAIL_BYTES);
        let last = crate::presentation_sources(agent_id, &records)
            .last()
            .map(|source| source.through);
        (
            next_event,
            self.sources.iter().map(|source| source.source_id).collect(),
            last,
        )
    }
}

async fn reconcile_claude_presentation_sources(
    db: &RhoDb,
    agent_id: AgentId,
    messages: &[rho_claude::SessionMessage],
) -> anyhow::Result<(
    AgentEventPos,
    HashSet<Uuid>,
    Option<AgentEventPos>,
    Option<AgentPresentationCache>,
)> {
    let reconciliation = prepare_claude_presentation_reconciliation(db, agent_id, messages)?;
    let mut write = db.write().await;
    let (next_event, rebuilt_cache) = reconciliation.apply(&mut write, UnixMillis::now(), agent_id);
    write.commit();
    let (next_event, known_sources, last_source) = reconciliation.state(db, agent_id, next_event);
    Ok((next_event, known_sources, last_source, rebuilt_cache))
}

impl ClaudeLoop {
    async fn run(mut self) {
        loop {
            let initial_kind = self.state.read().expect("poison").kind.clone();
            let initial_execution_generation = self.execution_generation;
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
                            self.recover_pending_rewind().await;
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
                                ))
                                .await;
                            }
                        }
                        Err(error) => {
                            self.process = None;
                            self.recover_pending_rewind().await;
                            self.queued_turns.clear();
                            self.fail(error).await;
                        }
                    },
                }
            } else {
                let Some(control) = self.control_rx.recv().await else {
                    return;
                };
                self.handle_control(control).await;
            }
            let kind = self.state.read().expect("poison").kind.clone();
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
                    self.notify.notify_waiters();
                }
            }
        }
    }

    async fn handle_control(&mut self, control: ClaudeControl) {
        match control {
            ClaudeControl::UserMessage {
                content,
                seq,
                uuid,
                user,
                accepted,
            } => {
                self.cancelling = false;
                let busy = self.state.read().expect("poison").kind.is_working();
                if !busy {
                    self.execution_generation = self.execution_generation.wrapping_add(1);
                }
                if user {
                    let mut write = self.db.write().await;
                    write.record_agent_user_message(
                        rho_core::UnixMs::now(),
                        self.agent_id,
                        &rho_core::text_content(&content),
                    );
                    write.commit();
                }
                if let Err(error) = self.ensure_process().await {
                    if let Some(accepted) = accepted {
                        let _ = accepted.send(Err(anyhow::anyhow!("{error:#}")));
                    }
                    self.fail(error).await;
                    return;
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
                            content: Arc::clone(&content),
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
                let kind = self.state.read().expect("poison").kind.clone();
                let busy = matches!(kind, AgentStateKind::ApiStreaming { .. });
                let queued = self
                    .queued_turns
                    .iter()
                    .map(|turn| turn.uuid.clone())
                    .collect::<Vec<_>>();
                self.state.write().expect("poison").queued_inputs.clear();
                self.queued_turns.clear();
                self.cancelling = busy;
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
                if watching {
                    let first_watch = self.presentation.watches == 0;
                    self.presentation.watches += 1;
                    if first_watch {
                        self.presentation.dirty = true;
                        self.schedule_presentation();
                    }
                } else {
                    self.presentation.watches = self.presentation.watches.saturating_sub(1);
                    if self.presentation.watches == 0 {
                        if let Some(task) = self.presentation.task.take() {
                            task.abort();
                        }
                        self.presentation.generation = self.presentation.generation.wrapping_add(1);
                        self.presentation.dirty = false;
                        self.presentation.last_started = None;
                    }
                }
            }
            ClaudeControl::PresentationStarted {
                generation,
                acknowledged,
            } => {
                let accepted = self.presentation.generation == generation
                    && self.presentation.watches > 0
                    && self.presentation.task.is_some();
                if accepted {
                    self.presentation.dirty = false;
                    self.presentation.last_started = Some(tokio::time::Instant::now());
                }
                let _ = acknowledged.send(accepted);
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
        let event_pos = self.next_event;
        write.append_agent_presentation_history(event_pos, &update);
        self.next_event = write.append_agent_event(
            self.next_event,
            &AgentEvent::PresentationUpdated {
                update: update.clone(),
            },
        );
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

    async fn persist_presentation_source(
        &mut self,
        source_id: Uuid,
        speaker: PresentationSpeaker,
        text: String,
    ) {
        let Some(text) = crate::presentation::canonical_source_text(&text) else {
            return;
        };
        if !self.known_presentation_sources.insert(source_id) {
            return;
        }
        let event_pos = self.next_event;
        let mut write = self.db.write().await;
        self.next_event = write.append_agent_event(
            self.next_event,
            &AgentEvent::ClaudePresentationSource {
                source_id,
                speaker,
                text: Cow::Borrowed(&text),
            },
        );
        write.commit();
        self.last_presentation_source = Some(event_pos);
        self.presentation.dirty = true;
        self.schedule_presentation();
    }

    fn reset_presentation(&mut self) {
        if let Some(task) = self.presentation.task.take() {
            task.abort();
        }
        self.presentation.generation = self.presentation.generation.wrapping_add(1);
        self.presentation.last_started = None;
        self.presentation.dirty = self.presentation.watches > 0;
    }

    fn schedule_presentation(&mut self) {
        if self.presentation.watches == 0
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
        let control = self.presentation_control.clone();
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
                self.state.read().expect("poison").kind,
                AgentStateKind::Idle | AgentStateKind::Error(_)
            ),
            "role changes are only available while idle or errored; cancel the turn first"
        );
        anyhow::ensure!(
            self.state.read().expect("poison").queued_inputs.is_empty()
                && self.queued_turns.is_empty(),
            "role changes are not available with queued inputs"
        );

        let requested = match requested {
            AgentRole::Engineer { intelligence }
            | AgentRole::WorkflowEngineer { intelligence, .. } => intelligence,
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
            AgentRole::WorkflowEngineer {
                intelligence: EngineerIntelligence::Ultra | EngineerIntelligence::Alt,
                workflow,
            } => AgentRole::WorkflowEngineer {
                intelligence: requested,
                workflow,
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
                self.state.read().expect("poison").kind,
                AgentStateKind::Idle | AgentStateKind::Error(_)
            ),
            ":rewind is only available while idle or errored; use :cancel first"
        );
        anyhow::ensure!(
            self.state.read().expect("poison").queued_inputs.is_empty()
                && self.queued_turns.is_empty(),
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
                        source_session_id,
                        view.primary().repo(),
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
                self.session_id,
                view.primary().repo(),
                rho_claude::SessionMessagesOptions::default(),
            )
            .await?;
            (self.session_id, messages)
        };
        let (messages, resume_at) =
            rho_claude::rewind_session_messages(&messages, turns).context("nothing to rewind")?;
        let reconciliation =
            prepare_claude_presentation_reconciliation(&self.db, self.agent_id, &messages)?;
        let blocks = transcript_messages_to_context(&messages)?;
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
        let (next_event, _) = reconciliation.apply(&mut write, UnixMillis::now(), self.agent_id);
        write.set_agent_claude_rewind(
            self.agent_id,
            Some(ClaudeRewind {
                source_session_id,
                session_id: new_session_id,
                resume_at,
            }),
        );
        write.commit();
        let (next_event, known_sources, last_source) =
            reconciliation.state(&self.db, self.agent_id, next_event);
        self.next_event = next_event;
        self.known_presentation_sources = known_sources;
        self.last_presentation_source = last_source;
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

        {
            let mut state = self.state.write().expect("poison");
            state.blocks = blocks;
            state.queued_inputs.clear();
            state.kind = AgentStateKind::Idle;
            state.context_used = context_used;
        }
        self.pending_response = PendingInferenceResponse::default();
        self.stream_items.clear();
        self.turn_usage = None;
        self.notify.notify_waiters();
        Ok(())
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
                } => queued_user_content_matches(queued, content),
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
        let mut command = options.command().await?;
        view.prepare_command(&mut command, None, file_mounts)
            .await?;
        self.process = Some(ClaudeCode::spawn_command(command).await?);
        if !self.pending_rewind {
            self.start_mode = ClaudeStartMode::Resume;
        }
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
        let prompt = system_prompt::claude_prompt(Some(view), self.multi_agent.as_ref(), self.role);
        // Keep one source inode alive for the lifetime of the view namespace.
        // Unlinking a bind-mounted source makes the target pathname disappear
        // inside that namespace, so a later cold respawn cannot mount a new
        // prompt over it (the pre-exec hook fails with ENOENT).
        let source = write_claude_prompt_source(&mut self.claude_prompt_path, &prompt)?;
        Ok(Some((source, target)))
    }

    async fn handle_event(&mut self, event: rho_claude::ClaudeEvent) {
        match event {
            rho_claude::ClaudeEvent::System(message) => {
                self.handle_system_message(message).await;
            }
            rho_claude::ClaudeEvent::ControlResponse(_) => {}
            rho_claude::ClaudeEvent::Assistant(message) => {
                let source = assistant_presentation_source(&message);
                match assistant_message_to_block(message) {
                    Ok(block) => {
                        self.pending_response = PendingInferenceResponse::default();
                        self.stream_items.clear();
                        self.persist_inference_block(&block).await;
                        match source {
                            Ok(Some((source_id, speaker, text))) => {
                                self.persist_presentation_source(source_id, speaker, text)
                                    .await;
                            }
                            Ok(None) => {}
                            Err(error) => {
                                self.fail(error).await;
                                return;
                            }
                        }
                        self.push_block(block);
                        self.set_streaming_kind();
                    }
                    Err(error) => self.fail(error).await,
                }
            }
            rho_claude::ClaudeEvent::User(message) => {
                let source_id = message
                    .uuid
                    .as_deref()
                    .and_then(|uuid| Uuid::parse_str(uuid).ok());
                let promoted_queued = self
                    .activate_turn_from_user_echo(message.uuid.as_deref(), source_id)
                    .await;
                if promoted_queued {
                    return;
                }
                match user_output_to_block(message) {
                    Ok(Some(block)) => self.handle_user_block(block),
                    Ok(None) => {}
                    Err(error) => self.fail(error).await,
                }
            }
            rho_claude::ClaudeEvent::Result(message) => {
                let successful = !message.is_error;
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
                    if let Some(view) = self.view.get_if_ready() {
                        let view = Arc::clone(view);
                        tokio::spawn(async move {
                            if let Err(error) = view.snapshot().await {
                                eprintln!("rho-agent Claude snapshot failed: {error:#}");
                            }
                        });
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
                } else if message_stopped && let Some(usage) = self.turn_usage.take() {
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
                    self.state
                        .write()
                        .expect("poison")
                        .total_usage
                        .add(&turn_usage);
                    if let Some(pool) = self.pool_events.upgrade() {
                        pool.record_agent_usage(self.agent_id, turn_usage).await;
                    }
                    self.notify.notify_waiters();
                }
            }
            rho_claude::ClaudeEvent::RateLimitEvent(_) => {}
            rho_claude::ClaudeEvent::CommandLifecycle(message) => {
                self.handle_command_lifecycle(message).await;
            }
            rho_claude::ClaudeEvent::Other => {}
        }
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
                    self.wait_baseline_seq
                        .store(turn.input_seq, Ordering::Release);
                    if let Some(source_id) = Uuid::parse_str(&message.command_uuid).ok()
                        && let Some((source_id, speaker, text)) =
                            queued_user_presentation_source(source_id, &turn.content)
                    {
                        self.persist_presentation_source(source_id, speaker, text)
                            .await;
                    }
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

    async fn complete_rewind(&mut self) -> anyhow::Result<()> {
        if !self.pending_rewind {
            return Ok(());
        }
        self.close_process().await;
        let view = Arc::clone(self.view.get().await?);
        let messages = rho_claude::read_session_messages_by_id(
            self.session_id,
            view.primary().repo(),
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

    async fn activate_turn_from_user_echo(
        &mut self,
        uuid: Option<&str>,
        source_id: Option<Uuid>,
    ) -> bool {
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

        if let Some((source_id, speaker, text)) = source_id
            .and_then(|source_id| queued_user_presentation_source(source_id, &turn.content))
        {
            self.persist_presentation_source(source_id, speaker, text)
                .await;
        }

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

    async fn fail(&mut self, error: anyhow::Error) {
        if let Some(pool) = self.pool_events.upgrade() {
            pool.publish_failed_turn(self.agent_id, error.to_string())
                .await;
        }
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
    base.cache_creation = update.cache_creation.or(base.cache_creation.take());
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

fn queued_user_content_matches(queued: &[ContentPart], echoed: &[ContentPart]) -> bool {
    rho_core::text_content(queued) == rho_core::text_content(echoed)
}

fn is_compact_command(content: &[ContentPart]) -> bool {
    match content {
        [ContentPart::Text { text }] => text.trim() == "/compact",
        _ => false,
    }
}

fn write_claude_prompt_source(
    path: &mut Option<tempfile::TempPath>,
    prompt: &str,
) -> anyhow::Result<Utf8PathBuf> {
    let (mut file, source) = if let Some(path) = path.as_ref() {
        let source = Utf8PathBuf::try_from(path.to_path_buf())
            .context("generated Claude prompt tempfile path is not valid UTF-8")?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(path)
            .context("reopen generated Claude prompt tempfile")?;
        (file, source)
    } else {
        let source_file = tempfile::Builder::new()
            .prefix("rho-claude-prompt-")
            .suffix(".md")
            .tempfile()
            .context("create generated Claude prompt tempfile")?;
        let source = Utf8PathBuf::try_from(source_file.path().to_owned())
            .context("generated Claude prompt tempfile path is not valid UTF-8")?;
        let (file, temp_path) = source_file.into_parts();
        *path = Some(temp_path);
        (file, source)
    };
    file.write_all(prompt.as_bytes())
        .context("write generated Claude prompt tempfile")?;
    file.flush()
        .context("flush generated Claude prompt tempfile")?;
    Ok(source)
}

#[cfg(test)]
mod tests {
    use rho_db::RhoDb;
    use rho_inference::PromptCacheKey;
    use rho_workspaces::{WorkspaceId, WorkspaceIdDomain, WorkspaceInfo};
    use serde_json::json;

    use super::*;

    #[test]
    fn legacy_turn_end_uses_last_real_assistant_transcript_timestamp() {
        let session_id = Uuid::new_v4();
        let message = |kind, timestamp: Option<&str>| rho_claude::SessionMessage {
            kind,
            uuid: Uuid::new_v4(),
            session_id,
            message: json!({}),
            parent_tool_use_id: None,
            timestamp: timestamp.map(str::to_owned),
        };
        let messages = vec![
            message(
                rho_claude::SessionMessageKind::Assistant,
                Some("2026-07-01T10:00:00Z"),
            ),
            message(
                rho_claude::SessionMessageKind::Assistant,
                Some("not-a-timestamp"),
            ),
            message(
                rho_claude::SessionMessageKind::User,
                Some("2026-07-03T10:00:00Z"),
            ),
            message(
                rho_claude::SessionMessageKind::Assistant,
                Some("2026-07-02T10:00:00Z"),
            ),
        ];
        let expected = chrono::DateTime::parse_from_rfc3339("2026-07-02T10:00:00Z")
            .unwrap()
            .timestamp_millis() as u64;
        assert_eq!(
            last_assistant_message_at(&messages),
            Some(rho_core::UnixMs(expected))
        );
        assert_eq!(
            last_assistant_message_at(&[message(rho_claude::SessionMessageKind::Assistant, None,)]),
            None
        );
    }

    #[tokio::test]
    async fn legacy_record_is_backfilled_from_transcript_ground_truth() {
        let (_temp, db, agent_id) = presentation_test_agent().await;
        assert_eq!(db.read().agent_attention(agent_id).last_turn_ended, None);
        let messages = [rho_claude::SessionMessage {
            kind: rho_claude::SessionMessageKind::Assistant,
            uuid: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            message: json!({}),
            parent_tool_use_id: None,
            timestamp: Some("2026-06-01T00:00:00Z".to_owned()),
        }];
        let mut write = db.write().await;
        assert!(backfill_last_turn_ended_from_claude_messages(
            &mut write, agent_id, &messages,
        ));
        write.commit();
        let expected = chrono::DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
            .unwrap()
            .timestamp_millis() as u64;
        assert_eq!(
            db.read().agent_attention(agent_id).last_turn_ended,
            Some(rho_core::UnixMs(expected))
        );
    }

    async fn presentation_test_agent() -> (tempfile::TempDir, RhoDb, AgentId) {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        let mut write = db.write().await;
        write.init_agent_tables();
        let agent_id = write.alloc_agent_id();
        write.create_agent(
            rho_core::UnixMs(1),
            agent_id,
            None,
            vec![WorkspaceInfo::Workspace {
                repo: "/home/user/src/rho".into(),
                id: WorkspaceId::from_counter(1, &WorkspaceIdDomain(0)).unwrap(),
            }],
            crate::db::AgentRole::PM,
            SessionBinding::ResponsesSol(crate::db::InferenceProfile::default()),
            AgentRuntime::Rho {
                prompt_cache_key: PromptCacheKey::generate(),
            },
            None,
        );
        write.commit();
        (temp, db, agent_id)
    }

    fn presentation_message(
        kind: rho_claude::SessionMessageKind,
        uuid: Uuid,
        text: &str,
    ) -> rho_claude::SessionMessage {
        rho_claude::SessionMessage {
            kind,
            uuid,
            session_id: uuid::uuid!("00000000-0000-4000-8000-000000000099"),
            message: json!({
                "role": match kind {
                    rho_claude::SessionMessageKind::User => "user",
                    rho_claude::SessionMessageKind::Assistant => "assistant",
                    rho_claude::SessionMessageKind::System => "system",
                },
                "content": [{"type": "text", "text": text}],
            }),
            parent_tool_use_id: None,
            timestamp: None,
        }
    }

    #[tokio::test]
    async fn reconciliation_appends_only_the_missing_suffix() {
        let (_temp, db, agent_id) = presentation_test_agent().await;
        let first = presentation_message(
            rho_claude::SessionMessageKind::User,
            uuid::uuid!("00000000-0000-4000-8000-000000000001"),
            "first",
        );
        let second = presentation_message(
            rho_claude::SessionMessageKind::Assistant,
            uuid::uuid!("00000000-0000-4000-8000-000000000002"),
            "second",
        );
        reconcile_claude_presentation_sources(&db, agent_id, std::slice::from_ref(&first))
            .await
            .unwrap();
        let (_, sources, _, rebuilt) =
            reconcile_claude_presentation_sources(&db, agent_id, &[first, second])
                .await
                .unwrap();

        assert!(rebuilt.is_none());
        assert_eq!(sources.len(), 2);
        assert_eq!(
            db.read()
                .agent_event_records(agent_id)
                .1
                .into_iter()
                .filter(|(_, event)| matches!(event, AgentEvent::ClaudePresentationSource { .. }))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn reconciliation_divergence_rebuilds_cache_and_discards_stale_updates() {
        let (_temp, db, agent_id) = presentation_test_agent().await;
        let first = presentation_message(
            rho_claude::SessionMessageKind::User,
            uuid::uuid!("00000000-0000-4000-8000-000000000001"),
            "first",
        );
        let discarded = presentation_message(
            rho_claude::SessionMessageKind::Assistant,
            uuid::uuid!("00000000-0000-4000-8000-000000000002"),
            "discarded",
        );
        let replacement = presentation_message(
            rho_claude::SessionMessageKind::Assistant,
            uuid::uuid!("00000000-0000-4000-8000-000000000003"),
            "replacement",
        );
        let (next, _, _, _) =
            reconcile_claude_presentation_sources(&db, agent_id, &[first.clone(), discarded])
                .await
                .unwrap();
        let discarded_position = db
            .read()
            .agent_event_records(agent_id)
            .1
            .into_iter()
            .find_map(|(position, event)| match event {
                AgentEvent::ClaudePresentationSource { source_id, .. }
                    if source_id == uuid::uuid!("00000000-0000-4000-8000-000000000002") =>
                {
                    Some(position)
                }
                _ => None,
            })
            .unwrap();
        let update = AgentPresentationUpdate {
            generated_title: crate::db::PresentationField::Set("discarded-title".to_owned()),
            activity: crate::db::PresentationField::Unchanged,
            through: discarded_position,
        };
        let mut write = db.write().await;
        assert!(
            write
                .apply_agent_presentation(rho_core::UnixMs(2), agent_id, &update)
                .is_some()
        );
        write.append_agent_presentation_history(next, &update);
        write.append_agent_event(
            next,
            &AgentEvent::PresentationUpdated {
                update: update.clone(),
            },
        );
        write.commit();

        let (_, _, _, rebuilt) =
            reconcile_claude_presentation_sources(&db, agent_id, &[first.clone(), replacement])
                .await
                .unwrap();
        assert_eq!(rebuilt, Some(AgentPresentationCache::default()));
        assert!(db.read().agent_presentation_updates(agent_id).is_empty());
        let mut write = db.write().await;
        assert!(
            write
                .apply_agent_presentation(rho_core::UnixMs(3), agent_id, &update)
                .is_none()
        );
        write.commit();

        let (_, sources, _, rebuilt) =
            reconcile_claude_presentation_sources(&db, agent_id, &[first])
                .await
                .unwrap();
        assert_eq!(sources.len(), 1);
        assert!(rebuilt.is_none());
    }

    #[test]
    fn rewrites_claude_prompt_without_replacing_bind_source() {
        let mut path = None;
        let first = write_claude_prompt_source(&mut path, "ultra").unwrap();
        let second = write_claude_prompt_source(&mut path, "alt").unwrap();

        assert_eq!(second, first);
        assert_eq!(std::fs::read_to_string(first).unwrap(), "alt");
    }

    #[test]
    fn raw_image_queue_matches_claude_textual_echo() {
        let queued = vec![
            ContentPart::Text {
                text: "inspect".to_owned(),
            },
            ContentPart::Image {
                media_type: "image/png".to_owned(),
                data: vec![1, 2, 3],
            },
        ];
        let echoed = vec![ContentPart::Text {
            text: "inspect\n[image: PNG]".to_owned(),
        }];
        assert!(queued_user_content_matches(&queued, &echoed));
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
