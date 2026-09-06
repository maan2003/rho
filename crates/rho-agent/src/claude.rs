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
use notify::Watcher as _;
use rho_claude::{
    ClaudeCode, ClaudeCodeOptions, Effort, Model, Session, TailLine, TailRead, TranscriptTail,
};
use rho_core::{ContentPart, ContextItemEvent, PendingInferenceResponse};
use rho_db::{RhoDb, WriteTxn};
use rho_inference::Inference;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::db::{
    AgentEventPos, AgentId, AgentPresentationCache, AgentPresentationUpdate,
    AgentProfileWriteTxnExt, AgentReadTxnExt, AgentRole, AgentRoleSessionProfile as _,
    AgentRuntime, AgentWriteTxnExt, ClaudeRewind, ClaudeTranscriptCursor, EngineerIntelligence,
    SessionBinding, UnixMillis,
};
use crate::multi_agent_tools::MultiAgentTools;
use crate::{
    AgentEvent, AgentState, AgentStateKind, AgentStatus, FailedInferenceResponse, InputKind,
    InputQueues, MessageDelivery, QueuedInput, StartWorkdir, TranscriptLine, system_prompt,
};

pub(crate) mod backfill;
pub(crate) mod projection;

use projection::{ClaudeStreamItem, transcript_line};

use crate::lazy::Lazy;

#[derive(Clone)]
pub struct ClaudeAgent {
    status: Arc<RwLock<AgentStatus>>,
    control: mpsc::UnboundedSender<ClaudeControl>,
    head: Arc<RwLock<crate::db::AgentHead>>,
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
        let materialized = crate::materialize_workdirs(start).await?;
        let entries = materialized.entries.clone();
        let view = match rho_workspaces::View::new(entries.clone()) {
            Ok(view) => view,
            Err(error) => {
                drop(entries);
                materialized.discard();
                return Err(error);
            }
        };
        let session_id = Uuid::new_v4();
        write.create_agent(
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
        let head = db.read().get_agent(agent_id);
        let primary_repo = head.primary_workdir().repo().to_owned();
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
                primary_repo,
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
        let primary_repo = record.primary_workdir().repo().to_owned();
        // The transcript's rows come from the file when the loop starts
        // (`sync_transcript`); a load reads the file only to settle a
        // rewind that was cut short.
        let (session_id, start_mode, pending_rewind) = if let Some(rewind) =
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
            let start_mode =
                match rho_claude::find_session_transcript(session_id, &primary_repo).await? {
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
            agent_id,
            view,
            primary_repo,
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
        ))
    }

    #[expect(clippy::too_many_arguments)]
    fn new(
        db: RhoDb,
        inference: Inference,
        agent_id: AgentId,
        view: Arc<Lazy<Arc<rho_workspaces::View>>>,
        primary_repo: Utf8PathBuf,
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
            crate::presentation::Session::new(inference),
        ));
        let loop_state = ClaudeLoop {
            db,
            presentation_session,
            agent_id,
            view,
            primary_repo,
            model,
            effort,
            session_id,
            start_mode,
            process: None,
            claude_prompt_path: None,
            claude_account: None,
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
            transcript: None,
            transcript_watch: None,
        };
        tokio::spawn(loop_state.run());
        Self {
            status,
            control,
            head,
        }
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
    /// The transcript file changed (the watch saw it).
    TranscriptChanged,
}

struct ClaudeLoop {
    db: RhoDb,
    /// The agent's one persistent Luna session, shared by activity updates
    /// and turn reports so both keep one prompt prefix warm.
    presentation_session: Arc<tokio::sync::Mutex<crate::presentation::Session>>,
    agent_id: AgentId,
    view: Arc<Lazy<Arc<rho_workspaces::View>>>,
    /// The primary workdir's repo, which is where Claude files the
    /// session. Known without materializing the view.
    primary_repo: Utf8PathBuf,
    model: Model,
    effort: Effort,
    session_id: Uuid,
    start_mode: ClaudeStartMode,
    process: Option<ClaudeCode>,
    claude_prompt_path: Option<tempfile::TempPath>,
    /// The account the running process was spawned on, so a switch is
    /// noticed at the next turn.
    claude_account: Option<String>,
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
    /// The session's transcript, copied into the log as it grows; `None`
    /// until the first sync, and again whenever the session changes
    /// under it.
    transcript: Option<TranscriptCopy>,
    transcript_watch: Option<notify::RecommendedWatcher>,
}

/// The session's transcript copied into the log: the file read as it
/// grows, and what a read from its start must not tell twice.
struct TranscriptCopy {
    agent_id: AgentId,
    session_id: Uuid,
    tail: TranscriptTail,
    /// The file was there when the path was worked out; a guessed path
    /// is worked out again while the file is missing.
    found: bool,
    /// A read from the file's start in progress: the lines the log
    /// already holds. `None` once past the start.
    told: Option<HashSet<Uuid>>,
    /// The API message whose usage a row already carries.
    usage_told: Option<String>,
    /// The uuids of the last lines read, so a sync asked to wait for one
    /// that is already in does not.
    recent_lines: VecDeque<Uuid>,
}

/// What one copy did.
#[derive(Debug, PartialEq)]
enum Copied {
    /// Lines were read and the cursor moved (the log may already have
    /// held every one of them): the last row said by a person or the
    /// model, and the context occupancy the last one reported.
    Read {
        spoke: Option<AgentEventPos>,
        context_used: Option<u64>,
    },
    /// Nothing new.
    Nothing,
    /// No file at the path.
    Missing,
}

impl TranscriptCopy {
    /// A copier for the session's file at `path`, starting where the
    /// cursor says; from the start when the cursor is another session's
    /// or there is none.
    fn open(
        db: &RhoDb,
        agent_id: AgentId,
        session_id: Uuid,
        path: Utf8PathBuf,
        found: bool,
    ) -> Self {
        let end = cursor_end(db.read().claude_transcript_cursor(agent_id), session_id);
        let mut copy = Self {
            agent_id,
            session_id,
            tail: TranscriptTail::new(path, end),
            found,
            told: None,
            usage_told: None,
            recent_lines: VecDeque::new(),
        };
        copy.reopen(db, end);
        copy
    }

    /// Reads from `end` on; from the start, what the log holds is
    /// skipped.
    fn reopen(&mut self, db: &RhoDb, end: u64) {
        self.tail = TranscriptTail::new(self.tail.path().to_owned(), end);
        self.told = (end == 0).then(|| told_lines(&db.read(), self.agent_id));
        self.usage_told = None;
    }

    fn path(&self) -> &camino::Utf8Path {
        self.tail.path()
    }

    /// Whether a recent read saw this line.
    fn has_line(&self, uuid: Uuid) -> bool {
        self.recent_lines.contains(&uuid)
    }

    /// One read from the cursor: the lines since, as rows, and the cursor
    /// after them, in one transaction. A file shorter than the cursor is
    /// read from its start, skipping what the log holds. A cursor moved
    /// by another writer since the read (the copy at daemon start) is
    /// read from again.
    async fn copy(
        &mut self,
        db: &RhoDb,
        usage_model: crate::db::AgentUsageModel,
    ) -> anyhow::Result<Copied> {
        let mut restarts = 0;
        loop {
            let from = self.tail.end();
            let lines = match self.tail.read().await? {
                TailRead::Missing => return Ok(Copied::Missing),
                TailRead::Truncated => {
                    restarts += 1;
                    anyhow::ensure!(restarts <= 2, "Claude transcript keeps shrinking");
                    let mut write = db.write().await;
                    write.set_claude_transcript_cursor(
                        self.agent_id,
                        &ClaudeTranscriptCursor {
                            session_id: self.session_id,
                            end: 0,
                        },
                    );
                    write.commit();
                    self.reopen(db, 0);
                    continue;
                }
                TailRead::Lines(lines) => lines,
            };
            if lines.is_empty() {
                return Ok(Copied::Nothing);
            }
            let end = self.tail.end();
            for line in &lines {
                if self.recent_lines.len() >= 128 {
                    self.recent_lines.pop_front();
                }
                self.recent_lines.push_back(line.uuid());
            }
            let rows = project_rows(
                self.agent_id,
                &lines,
                self.told.as_ref(),
                usage_model,
                &mut self.usage_told,
            );
            let mut write = db.write().await;
            let stored = cursor_end(
                write.claude_transcript_cursor(self.agent_id),
                self.session_id,
            );
            if stored != from {
                drop(write);
                restarts += 1;
                anyhow::ensure!(restarts <= 2, "Claude transcript cursor keeps moving");
                self.reopen(db, stored);
                continue;
            }
            let (spoke, context_used) = append_rows(&mut write, self.agent_id, rows);
            write.set_claude_transcript_cursor(
                self.agent_id,
                &ClaudeTranscriptCursor {
                    session_id: self.session_id,
                    end,
                },
            );
            write.commit();
            return Ok(Copied::Read {
                spoke,
                context_used,
            });
        }
    }
}

/// Where a cursor says this session's copy stops: zero when it is
/// another session's or there is none.
fn cursor_end(cursor: Option<ClaudeTranscriptCursor>, session_id: Uuid) -> u64 {
    match cursor {
        Some(cursor) if cursor.session_id == session_id => cursor.end,
        _ => 0,
    }
}

/// The uuid of every `Transcript` row in the log.
pub(super) fn told_lines(read: &rho_db::ReadTxn, agent_id: AgentId) -> HashSet<Uuid> {
    read.agent_event_records(agent_id)
        .1
        .into_iter()
        .filter_map(|(_, event)| match event {
            AgentEvent::Transcript { uuid, .. } => Some(uuid),
            _ => None,
        })
        .collect()
}

/// One line of the file, as the row it makes.
pub(super) struct LineRow {
    uuid: Uuid,
    offset: u64,
    line: TranscriptLine,
    at: rho_core::UnixMs,
}

/// The rows for lines read: none for a line in `told`, a hidden one, or
/// one that does not parse (said on stderr).
pub(super) fn project_rows(
    agent_id: AgentId,
    lines: &[TailLine],
    told: Option<&HashSet<Uuid>>,
    usage_model: crate::db::AgentUsageModel,
    usage_told: &mut Option<String>,
) -> Vec<LineRow> {
    let mut rows = Vec::with_capacity(lines.len());
    for line in lines {
        let uuid = line.uuid();
        if told.is_some_and(|told| told.contains(&uuid)) {
            continue;
        }
        match transcript_line(&line.row, usage_model, usage_told) {
            Ok(Some((uuid, row, at))) => rows.push(LineRow {
                uuid,
                offset: line.offset,
                line: row,
                at,
            }),
            Ok(None) => {}
            Err(error) => eprintln!(
                "rho-agent: Claude transcript line {uuid} of {} skipped: {error:#}",
                agent_id.encoded()
            ),
        }
    }
    rows
}

/// Appends the rows: the last one said by a person or the model, and the
/// context occupancy the last one reported.
pub(super) fn append_rows(
    write: &mut WriteTxn,
    agent_id: AgentId,
    rows: Vec<LineRow>,
) -> (Option<AgentEventPos>, Option<u64>) {
    let mut spoke = None;
    let mut context_used = None;
    for row in rows {
        match &row.line {
            TranscriptLine::Assistant {
                context_used: reported,
                ..
            }
            | TranscriptLine::Compacted {
                context_used: reported,
            } => {
                if reported.is_some() {
                    context_used = *reported;
                }
            }
            TranscriptLine::User { .. } | TranscriptLine::ToolResults { .. } => {}
        }
        let said = matches!(
            &row.line,
            TranscriptLine::User { .. } | TranscriptLine::Assistant { .. }
        );
        let pos = write.append_agent_event(
            agent_id,
            &AgentEvent::Transcript {
                uuid: row.uuid,
                offset: row.offset,
                line: row.line,
                at: row.at,
            },
        );
        if said {
            spoke = Some(pos);
        }
    }
    (spoke, context_used)
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
        // The file is the history: whatever it says that the log does
        // not yet, first.
        self.sync_transcript(None).await;
        loop {
            let initial_kind = self.state.kind.clone();
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
                // The row is what a reader sees the moment the message is
                // taken, ahead of a cold spawn; the echo's own row confirms
                // it (a reader matches the text) and a cancel clears it. A
                // `/compact` is the CLI's own command, never echoed, so no
                // row would ever confirm it.
                if !is_compact_command(&content) {
                    let mut write = self.db.write().await;
                    write.append_agent_event(self.agent_id, &AgentEvent::Accepted(input.clone()));
                    write.commit();
                }
                if let Err(error) = self.ensure_process().await {
                    let mut write = self.db.write().await;
                    write.append_agent_event(self.agent_id, &AgentEvent::QueueCleared);
                    write.commit();
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
                let had_queued = !self.state.queued_inputs.is_empty();
                self.state.queued_inputs.clear();
                self.queued_turns.clear();
                if had_queued {
                    let mut write = self.db.write().await;
                    write.append_agent_event(self.agent_id, &AgentEvent::QueueCleared);
                    write.commit();
                }
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
            ClaudeControl::TranscriptChanged => self.sync_transcript(None).await,
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
        // The rows from the first line the fork leaves behind are told
        // taken back now; the fork's own file, once it exists, is read
        // from its start and adds only what the log does not hold.
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
        self.transcript = None;
        self.transcript_watch = None;
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
        self.configure_claude_home(&view, &mut options, &account)
            .await?;
        let mut command = options.command().await?;
        view.prepare_command(&mut command, None).await?;
        self.process = Some(ClaudeCode::spawn_command(command).await?);
        if !self.pending_rewind {
            self.start_mode = ClaudeStartMode::Resume;
        }
        Ok(())
    }

    /// Gives the view the Claude configuration this agent runs against: its
    /// account, when it has one, and its own generated `CLAUDE.md`. Both are
    /// mounted at `~/.claude` when the view's namespace is built, so this
    /// has to run before the first spawn; a respawn only rewrites the prompt
    /// the standing mount already points at.
    async fn configure_claude_home(
        &mut self,
        view: &rho_workspaces::View,
        options: &mut rho_claude::ClaudeCodeOptions,
        account: &str,
    ) -> anyhow::Result<()> {
        let config_home = rho_claude::accounts::config_home()?;
        // The namespace mounts these, and a missing mount source or target
        // there fails namespace creation rather than the spawn.
        std::fs::create_dir_all(config_home.join("projects"))
            .with_context(|| format!("create Claude config directory {config_home}"))?;
        let account_dir = rho_claude::accounts::prepare(account)?;
        // Claude keeps `.claude.json` (the account itself, and its
        // credentials) in `$HOME`, not in the config directory, so no mount
        // over `~/.claude` alone could switch accounts. Naming the mount
        // point as the config directory is what pulls that file inside it.
        // The value is the same for every account: only the mount underneath
        // it differs.
        options.set_env("CLAUDE_CONFIG_DIR", config_home.as_str());
        let prompt = system_prompt::claude_prompt(Some(view), self.multi_agent.as_ref(), self.role);
        // Keep one source inode alive for the lifetime of the view namespace.
        // Unlinking a bind-mounted source makes the target pathname disappear
        // inside that namespace, so a rewrite has to reuse this file rather
        // than replace it.
        let source = write_claude_prompt_source(&mut self.claude_prompt_path, &prompt)?;
        view.set_claude_home(rho_workspaces::ns::ClaudeHome {
            account: account_dir.into_std_path_buf(),
            shared_projects: config_home.join("projects").into_std_path_buf(),
            config_home: config_home.into_std_path_buf(),
            prompt: source.into_std_path_buf(),
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
            // A message the stream tells whole is in the file (or about
            // to be): its row comes from there. The live tail empties
            // first, so a reader never holds the message twice.
            rho_claude::ClaudeEvent::Assistant(message) => {
                self.pending_response = PendingInferenceResponse::default();
                self.stream_items.clear();
                let line = message
                    .parent_tool_use_id
                    .is_none()
                    .then(|| line_uuid(message.uuid.as_deref()))
                    .flatten();
                self.sync_transcript(line).await;
                self.set_streaming_kind();
            }
            rho_claude::ClaudeEvent::User(message) => {
                self.activate_turn_from_user_echo(message.uuid.as_deref());
                // Synthetic and sidechain lines are not in the visible
                // file; only wait for one that is.
                let line = (message.parent_tool_use_id.is_none()
                    && !message.is_synthetic.unwrap_or(false))
                .then(|| line_uuid(message.uuid.as_deref()))
                .flatten();
                self.sync_transcript(line).await;
            }
            rho_claude::ClaudeEvent::Result(message) => {
                // Whatever the turn wrote last, before its end is told.
                self.sync_transcript(None).await;
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
                    self.sync_transcript(None).await;
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

    /// Copies the transcript's new lines into the log. `line` is one the
    /// stream just announced: Claude writes the file and tells the stream
    /// in an order it does not promise, so the copy waits (briefly) until
    /// that line is in, and anything told after it (a turn end, a want)
    /// follows its row.
    async fn sync_transcript(&mut self, line: Option<Uuid>) {
        const TRIES: u32 = 40;
        const PAUSE: Duration = Duration::from_millis(25);
        for attempt in 0..TRIES {
            if let Err(error) = self.copy_transcript_lines().await {
                eprintln!(
                    "rho-agent: Claude transcript of {} not read: {error:#}",
                    self.agent_id.encoded()
                );
                return;
            }
            let Some(uuid) = line else { return };
            if self
                .transcript
                .as_ref()
                .is_some_and(|transcript| transcript.has_line(uuid))
            {
                return;
            }
            if attempt + 1 == TRIES {
                eprintln!(
                    "rho-agent: Claude transcript line {uuid} of {} not in the file after {:?}",
                    self.agent_id.encoded(),
                    PAUSE * TRIES
                );
                return;
            }
            tokio::time::sleep(PAUSE).await;
        }
    }

    async fn copy_transcript_lines(&mut self) -> anyhow::Result<()> {
        if self.transcript.is_none() {
            let (path, found) =
                rho_claude::session_transcript_path(self.session_id, &self.primary_repo).await?;
            self.transcript = Some(TranscriptCopy::open(
                &self.db,
                self.agent_id,
                self.session_id,
                path,
                found,
            ));
            self.watch_transcript();
        }
        let transcript = self.transcript.as_mut().expect("set above");
        match transcript.copy(&self.db, self.state.usage_provider).await? {
            Copied::Read {
                spoke,
                context_used,
            } => {
                if context_used.is_some() {
                    self.state.context_used = context_used;
                }
                if let Some(pos) = spoke {
                    self.last_presentation_source = Some(pos);
                    self.presentation.dirty = true;
                    self.schedule_presentation();
                }
            }
            Copied::Nothing => {}
            // Not written yet. A guessed path is worked out again next
            // time, in case Claude put the file elsewhere.
            Copied::Missing => {
                if !transcript.found {
                    self.transcript = None;
                }
            }
        }
        Ok(())
    }

    /// Follows the transcript's directory, so a change to the file made
    /// while the stream says nothing (or after it is gone) is still
    /// copied. Best effort: the stream is the usual bell.
    fn watch_transcript(&mut self) {
        if self.transcript_watch.is_some() {
            return;
        }
        let Some(path) = self
            .transcript
            .as_ref()
            .map(|transcript| transcript.path().to_owned())
        else {
            return;
        };
        let Some(dir) = path.parent().map(|dir| dir.to_owned()) else {
            return;
        };
        let control = self.control.clone();
        let file = path.clone();
        let watcher = notify::RecommendedWatcher::new(
            move |event: Result<notify::Event, notify::Error>| {
                let Ok(event) = event else { return };
                if !event
                    .paths
                    .iter()
                    .any(|changed| changed.as_path() == file.as_std_path())
                {
                    return;
                }
                if let Some(control) = control.upgrade() {
                    let _ = control.send(ClaudeControl::TranscriptChanged);
                }
            },
            notify::Config::default().with_event_kinds(notify::EventKindMask::CORE),
        );
        match watcher {
            // A missing directory is made with the file; tried again with
            // the next sync.
            Ok(mut watcher) => {
                if watcher
                    .watch(dir.as_std_path(), notify::RecursiveMode::NonRecursive)
                    .is_ok()
                {
                    self.transcript_watch = Some(watcher);
                }
            }
            Err(error) => eprintln!("rho-agent: Claude transcript watch not started: {error}"),
        }
    }

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

    /// With --replay-user-messages the CLI echoes every user message when
    /// it enters context: the echo of a queued send is that send leaving
    /// the queue. Its row is the file's.
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
            compact_metadata, ..
        } = message
        else {
            return;
        };

        remove_compact_commands(&mut self.state.queued_inputs);
        if let Some(post_tokens) = compact_metadata.and_then(|metadata| metadata.post_tokens) {
            self.state.context_used = Some(post_tokens);
        }
        self.published();
        self.sync_transcript(None).await;
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
    inputs.retain(|input| match &input.kind {
        InputKind::Message { content } => !is_compact_command(content),
        InputKind::Compaction => true,
    });
}

/// The oldest queued message left the queue: it is in the file now.
fn promote_queued_user_message(state: &mut AgentState) -> bool {
    state
        .queued_inputs
        .remove_first(|queued| matches!(queued.kind, InputKind::Message { .. }))
        .is_some()
}

fn line_uuid(uuid: Option<&str>) -> Option<Uuid> {
    uuid.and_then(|uuid| Uuid::parse_str(uuid).ok())
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
    use super::*;

    #[test]
    fn rewrites_claude_prompt_without_replacing_bind_source() {
        let mut path = None;
        let first = write_claude_prompt_source(&mut path, "ultra").unwrap();
        let second = write_claude_prompt_source(&mut path, "alt").unwrap();

        assert_eq!(second, first);
        assert_eq!(std::fs::read_to_string(first).unwrap(), "alt");
    }

    pub(super) async fn claude_test_agent(session_id: Uuid) -> (tempfile::TempDir, RhoDb, AgentId) {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));
        let mut write = db.write().await;
        write.init_agent_tables();
        let agent_id = write.alloc_agent_id();
        write.create_agent(
            rho_core::UnixMs(1),
            agent_id,
            None,
            vec![rho_workspaces::WorkspaceInfo::Workspace {
                repo: "/home/user/src/rho".into(),
                id: rho_workspaces::WorkspaceId::from_counter(
                    1,
                    &rho_workspaces::WorkspaceIdDomain(0),
                )
                .unwrap(),
            }],
            crate::db::AgentRole::default(),
            SessionBinding::ClaudeOpus {
                effort: crate::db::ClaudeEffort::High,
            },
            AgentRuntime::Claude { session_id },
            None,
        );
        write.commit();
        (temp, db, agent_id)
    }

    pub(super) fn user_json(uuid: &str, text: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"{uuid}","sessionId":"00000000-0000-4000-8000-000000000002","timestamp":"2026-09-06T10:00:00.000Z","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    pub(super) fn assistant_json(uuid: &str, text: &str) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"{uuid}","sessionId":"00000000-0000-4000-8000-000000000002","timestamp":"2026-09-06T10:00:01.000Z","message":{{"role":"assistant","id":"msg_{uuid}","usage":{{"input_tokens":1,"output_tokens":2}},"content":[{{"type":"text","text":"{text}"}}]}}}}"#
        )
    }

    pub(super) fn transcript_rows(db: &RhoDb, agent_id: AgentId) -> Vec<(AgentEventPos, String)> {
        db.read()
            .agent_event_records(agent_id)
            .1
            .into_iter()
            .filter_map(|(pos, event)| match event {
                AgentEvent::Transcript { line, .. } => Some((
                    pos,
                    match line {
                        TranscriptLine::User { text } => format!("user: {text}"),
                        TranscriptLine::Assistant { text, .. } => format!("assistant: {text}"),
                        TranscriptLine::ToolResults { .. } => "results".to_owned(),
                        TranscriptLine::Compacted { .. } => "compacted".to_owned(),
                    },
                )),
                _ => None,
            })
            .collect()
    }

    pub(super) fn told(rows: &[(AgentEventPos, String)]) -> Vec<&str> {
        rows.iter().map(|(_, line)| line.as_str()).collect()
    }

    pub(super) const U1: &str = "00000000-0000-4000-8000-000000000011";
    pub(super) const A1: &str = "00000000-0000-4000-8000-000000000012";
    const U2: &str = "00000000-0000-4000-8000-000000000013";
    const A2: &str = "00000000-0000-4000-8000-000000000014";

    #[tokio::test]
    async fn the_file_is_copied_as_it_grows_and_the_cursor_follows() {
        let session_id = uuid::uuid!("00000000-0000-4000-8000-000000000002");
        let (temp, db, agent_id) = claude_test_agent(session_id).await;
        let path = Utf8PathBuf::try_from(temp.path().join("session.jsonl")).unwrap();
        let usage = crate::db::AgentUsageModel::OPUS;

        let mut copy = TranscriptCopy::open(&db, agent_id, session_id, path.clone(), false);
        assert_eq!(copy.copy(&db, usage).await.unwrap(), Copied::Missing);

        std::fs::write(
            &path,
            format!("{}\n{}\n", user_json(U1, "hello"), assistant_json(A1, "hi")),
        )
        .unwrap();
        let Copied::Read {
            spoke,
            context_used,
        } = copy.copy(&db, usage).await.unwrap()
        else {
            panic!("expected rows");
        };
        assert_eq!(context_used, Some(3));
        let rows = transcript_rows(&db, agent_id);
        assert_eq!(told(&rows), ["user: hello", "assistant: hi"]);
        assert_eq!(spoke, Some(rows[1].0));
        assert!(copy.has_line(Uuid::parse_str(A1).unwrap()));
        let cursor = db.read().claude_transcript_cursor(agent_id).unwrap();
        assert_eq!(cursor.session_id, session_id);
        assert_eq!(cursor.end, std::fs::metadata(&path).unwrap().len());

        // Nothing new: no transaction, same cursor.
        assert_eq!(copy.copy(&db, usage).await.unwrap(), Copied::Nothing);

        // A line appended is one row more; a copier opened later starts
        // at the cursor and finds only what came after that.
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{}", user_json(U2, "more")).unwrap();
        assert!(matches!(
            copy.copy(&db, usage).await.unwrap(),
            Copied::Read { .. }
        ));
        assert_eq!(
            told(&transcript_rows(&db, agent_id)),
            ["user: hello", "assistant: hi", "user: more"]
        );
        writeln!(file, "{}", assistant_json(A2, "done")).unwrap();
        let mut later = TranscriptCopy::open(&db, agent_id, session_id, path.clone(), true);
        assert!(later.told.is_none(), "past the start: nothing to skip");
        assert!(matches!(
            later.copy(&db, usage).await.unwrap(),
            Copied::Read { .. }
        ));
        assert_eq!(
            told(&transcript_rows(&db, agent_id)),
            [
                "user: hello",
                "assistant: hi",
                "user: more",
                "assistant: done"
            ]
        );
    }

    #[tokio::test]
    async fn a_replaced_file_is_read_from_its_start_without_telling_a_line_twice() {
        let session_id = uuid::uuid!("00000000-0000-4000-8000-000000000002");
        let (temp, db, agent_id) = claude_test_agent(session_id).await;
        let path = Utf8PathBuf::try_from(temp.path().join("session.jsonl")).unwrap();
        let usage = crate::db::AgentUsageModel::OPUS;
        std::fs::write(
            &path,
            format!("{}\n{}\n", user_json(U1, "hello"), assistant_json(A1, "hi")),
        )
        .unwrap();
        let mut copy = TranscriptCopy::open(&db, agent_id, session_id, path.clone(), true);
        assert!(matches!(
            copy.copy(&db, usage).await.unwrap(),
            Copied::Read { .. }
        ));

        // The file is rewritten shorter, then grows past the old cursor:
        // the first line is known, the new one is not.
        std::fs::write(&path, format!("{}\n", user_json(U1, "hello"))).unwrap();
        assert_eq!(
            copy.copy(&db, usage).await.unwrap(),
            Copied::Read {
                spoke: None,
                context_used: None
            }
        );
        assert_eq!(
            db.read().claude_transcript_cursor(agent_id).unwrap().end,
            std::fs::metadata(&path).unwrap().len(),
            "the cursor follows a known line too"
        );
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                user_json(U1, "hello"),
                assistant_json(A2, "again")
            ),
        )
        .unwrap();
        assert!(matches!(
            copy.copy(&db, usage).await.unwrap(),
            Copied::Read { .. }
        ));
        assert_eq!(
            told(&transcript_rows(&db, agent_id)),
            ["user: hello", "assistant: hi", "assistant: again"]
        );
        let cursor = db.read().claude_transcript_cursor(agent_id).unwrap();
        assert_eq!(cursor.end, std::fs::metadata(&path).unwrap().len());
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
