//! Rho's agents: the runtime loop (`agent`), the Claude Code runtime
//! (`claude`), the log both write into (`db`, `story`), and the pool that
//! keeps them running (`pool`).
//!
//! This file holds what the runtimes and their readers share: the raw log's
//! event type, the queued input it records, the state a reader sees, and
//! the text helpers both runtimes tell the story with.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::Arc;

pub use rho_agent_types::MessageDelivery;
use rho_agent_types::{AgentWant, ContentPart, TurnEdge, TurnOutcome, UnixMs};
pub use rho_fs_view::{Place, WorksetMode, WorkspaceInfo};
pub use rho_inference::types::MessageSender;
use rho_inference::types::{
    ApplyPatchMetadata, ContextBlock, InferenceResponseItem, PendingInferenceResponse, ToolCall,
    ToolCallId, ToolResult, ToolSpec,
};
use senax_encoder::{Decode, Encode};

use crate::db::{
    AgentEventPos, AgentId, AgentRole, AgentRuntime, AgentSpawnedBy, ClaudeRewind, SessionBinding,
};

pub mod agent;
mod boundary;
mod claude;
pub mod native;
pub mod python;
pub use agent::{AgentHandle, render_agent_surface};

pub mod db;
mod image_tool;
pub mod journal;
mod lazy;
pub mod multi_agent_tools;
mod papercut;
pub mod pool;
pub mod prompt;
pub mod shell;
pub mod terminal;
mod title;
mod worker;
pub use worker::{
    Process as WorksetProcess, WorksetAction, WorksetAttach, WorksetClient, WorksetReply,
    worker_main,
};

/// Model-facing prompt and top-level tools for a newly created role. Dynamic
/// agent identity/team text and stateful integration hosts are omitted.
pub struct RenderedAgentSurface {
    pub system_prompt: Arc<str>,
    pub tools: Arc<[ToolSpec]>,
}

/// One event of an agent's raw log.
///
/// The Rho runtime writes `Accepted`, `QueueCleared`, `Sent`, `Replied` and
/// `Failed`; the Claude runtime `Accepted`, `QueueCleared`, `Failed` and
/// `Transcript`; the head's config events are written by the store on the
/// agent's behalf.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum AgentEvent<'a> {
    /// An input entered a queue: user text, mail, or a `/compact`. It becomes
    /// context when a later `Sent` carries it.
    Accepted(QueuedInput),
    Cleared {
        at: UnixMs,
    },
    /// A turn started or stopped: the edge both runtimes cross.
    Turn {
        edge: TurnEdge,
        at: UnixMs,
    },
    /// The lifetime naming opportunity was consumed, before network dispatch.
    TitleAttempted {
        at: UnixMs,
    },
    /// Generated naming metadata; a spawn or user name always takes precedence.
    Titled {
        title: Option<String>,
        at: UnixMs,
    },
    /// What the last turn asks of the person.
    Wants {
        want: AgentWant,
        summary: Option<String>,
        at: UnixMs,
    },
    /// Everything from `to` up to this event is no longer the agent's
    /// history. Told, never undone: positions only grow.
    Rewound {
        to: AgentEventPos,
        at: UnixMs,
    },
    /// A request failed with this much of a response in. `retrying` when
    /// the loop makes the request again by itself; otherwise the turn
    /// ends in error right after. Never history: the next request does
    /// not carry it. Written so what the model said is not lost.
    Failed {
        partial: PendingInferenceResponse,
        error: Cow<'a, str>,
        retrying: bool,
        at: UnixMs,
    },

    /// One line of the conversation, as the Claude runtime's stream told
    /// it: a finished content block, a person's message (Claude's echo
    /// of a send), a call's results, a compaction. Rows before 7 Sep
    /// were copied from Claude Code's session file instead and carried
    /// their offset in it, a field a decoder now skips.
    Transcript {
        /// The line's uuid, the same Claude Code's session file gives it
        /// (a rewind forks the session there).
        uuid: uuid::Uuid,
        line: TranscriptLine,
        at: UnixMs,
        /// On a row the notebook produced (an exec call's results, or a
        /// message of output injected into an idle model): why the notebook
        /// spoke when it did.
        #[senax(default)]
        wake: Option<WakeFacts>,
    },

    // -- the runtimes' shared config log --------------------------------------
    /// The agent coming into being: the first event of every agent's log,
    /// and the base the head's config is folded from. A spawn name given
    /// here is why no title is generated for that agent.
    Created {
        role: AgentRole,
        binding: SessionBinding,
        runtime: AgentRuntime,
        place: Place,
        spawned_by: AgentSpawnedBy,
        spawn_name: Option<String>,
        created_at: rho_agent_types::UnixMs,
        /// The agent that spawned this one.
        #[senax(default)]
        parent: Option<AgentId>,
    },
    RoleChanged {
        role: AgentRole,
        /// `None` when only the role moved and the session binding stands.
        binding: Option<SessionBinding>,
        #[senax(default)]
        at: UnixMs,
    },
    /// The agent now sees the filesystem this way: the same workset and
    /// directory, entered in the other mode at its next load.
    ModeChanged {
        mode: WorksetMode,
        #[senax(default)]
        at: UnixMs,
    },
    /// Something Rho has to tell the agent, carried ahead of its next user
    /// message and then done: what a migration did to its place, say.
    Notice {
        text: Cow<'a, str>,
        #[senax(default)]
        at: UnixMs,
    },
    /// The runtime itself changing under the agent: a Claude rewind before
    /// and after its destination transcript is verified, or a new prompt
    /// cache key for the Rho runtime.
    RuntimeRebound {
        change: RuntimeChange,
        #[senax(default)]
        at: UnixMs,
    },
    /// A provider/host lifecycle observation shared only for presentation.
    /// It neither changes native context nor claims ownership of Claude
    /// history.
    ExecObserved {
        id: rho_inference::types::ExecId,
        milestone: rho_agent_types::ExecMilestone,
        at: UnixMs,
    },
    /// Claude owns its conversation; this records only Rho's permission to
    /// execute a cell, committed before the notebook can perform side effects.
    ClaudeExecAdmitted {
        call: rho_inference::types::ExecCall,
        at: UnixMs,
    },
    /// Canonical native conversation records; legacy block rows are read-only.
    Native(native::NativeEvent),
    ClaudeOutput {
        batch: ClaudeOutputBatch,
    },
    ClaudeOutputHandedOff {
        id: uuid::Uuid,
        at: UnixMs,
    },
}

/// Leased notebook contributions transferred to durable host ownership before
/// contacting Claude Code. A handoff does not prove remote consumption.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ClaudeOutputBatch {
    pub id: uuid::Uuid,
    pub outputs: Vec<(
        rho_inference::types::ExecId,
        rho_inference::types::ToolOutput,
    )>,
    pub wake: WakeFacts,
    pub at: UnixMs,
}

/// Historical notes-preparation transitions, retained for transcript decoding.
/// New eviction boundaries are `ContextBlock::ToolHistoryEvicted` items.
/// Indices refer to full history, never to the provider projection.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum ContextChange {
    Marked { retain_from: u64 },
    Preparing { retain_from: u64, repair: bool },
}

/// Why a request went out when it did: the scheduler's reading of its
/// sources at the boundary (`agent/boundary.rs`), recorded with the request
/// so the pace of a session can be judged from its log.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct WakeFacts {
    /// The candidate whose deadline came first.
    pub trigger: WakeTrigger,
    /// Every event pending at the boundary, all of which the request carries.
    pub events: Vec<WakeEvent>,
    /// Jobs and cells still running that the model was watching...
    pub foreground_running: u64,
    /// ...and ones it had moved on from.
    pub background_running: u64,
    /// The latest cell asked not to be woken by the notebook.
    pub tools_suppressed: bool,
    /// When the model's check-in was due, if its turn had one.
    pub checkin_at: Option<UnixMs>,
}

impl WakeFacts {
    /// A request an interrupt forced: nothing was weighed.
    pub fn interrupt() -> Self {
        Self {
            trigger: WakeTrigger::Interrupt,
            events: Vec::new(),
            foreground_running: 0,
            background_running: 0,
            tools_suppressed: false,
            checkin_at: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum WakeTrigger {
    /// A previously selected durable output batch is handed off.
    Delivery,
    /// The user's message threw away an in-flight request.
    Interrupt,
    /// A request somebody asked for outright: a retry, a compaction.
    Asked,
    /// A failed request's own clock.
    Retry,
    User,
    Mail,
    /// A cell's `notify()`.
    Notify,
    /// A job ended, or a cell returned with output.
    Finished,
    /// The model's check-in came due.
    Checkin,
    ContextRotation,
}

/// One pending event as the scheduler saw it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct WakeEvent {
    pub cell: u64,
    /// The job's registration time, or `u64::MAX` for the cell itself.
    pub source: u64,
    pub kind: WakeKind,
    pub foreground: bool,
    /// When the tool recorded it.
    pub occurred_at: UnixMs,
    /// When the scheduler first saw it while able to act: where its
    /// patience was measured from.
    pub seen_at: UnixMs,
    /// When it would have sent on its own; `None` for an event content to
    /// wait for the foreground or the check-in.
    pub deadline: Option<UnixMs>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
pub enum WakeKind {
    Notify,
    Succeeded,
    Failed,
}

impl AgentEvent<'_> {
    /// Whether this is a message the user typed, in either generation of
    /// the log: what a rewind counts turns by.
    pub fn is_user_message(&self) -> bool {
        matches!(
            self,
            Self::Accepted(QueuedInput {
                source: MessageSender::User,
                kind: InputKind::Message { .. },
                ..
            })
        )
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum RuntimeChange {
    /// A message-only Claude rewind whose destination transcript has not
    /// been materialized and verified yet; `None` withdraws one. The old
    /// runtime stays authoritative until it is confirmed.
    ClaudeRewindPending(Option<ClaudeRewind>),
    /// The rewind landed: this session is the runtime now.
    ClaudeRewound {
        session_id: uuid::Uuid,
    },
    PromptCacheKey(rho_inference::PromptCacheKey),
}

/// One input waiting to reach the model. Persisted verbatim inside
/// [`AgentEvent::Accepted`], so the live queue and the log share one shape.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct QueuedInput {
    pub source: MessageSender,
    pub kind: InputKind,
    pub delivery: MessageDelivery,
    pub at: UnixMs,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum InputKind {
    Message {
        content: Vec<ContentPart>,
    },
    /// The user explicitly asked to compact. Automatic compaction is not an
    /// input at all — it happens while building a request.
    Compaction,
}

/// What one transcript line says, as far as a reader needs. Bodies are
/// whole: the wire strips them.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum TranscriptLine {
    /// The person spoke (an agent's mail reaches Claude the same way).
    User { text: String },
    /// The model spoke or called. One line per content block, so a text
    /// and the call after it are two rows. Usage rides on the first row
    /// of a message only, so a reader counts each request once.
    Assistant {
        text: String,
        calls: Vec<TranscriptCall>,
        usage: Option<db::AgentUsageBucket>,
        /// Context-window occupancy after this message.
        context_used: Option<u64>,
    },
    /// What the calls came back with.
    ToolResults {
        results: Vec<rho_inference::types::ToolResult>,
    },
    /// Claude compacted the context here.
    Compacted { context_used: Option<u64> },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct TranscriptCall {
    pub id: String,
    pub name: String,
    /// The arguments as JSON, whole.
    pub arguments: String,
}

/// A text-only, durably committed transcript source for the presentation
/// sidecar.

/// What a loop publishes about itself: its phase, and how much input
/// waits. Everything else a reader wants is in the log.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct AgentStatus {
    pub kind: AgentStateKind,
    /// Inputs waiting to enter model context.
    pub queued: usize,
}

impl AgentStatus {
    /// Snapshot says nothing is running or queued. Remote snapshots may be
    /// stale; retiring a runtime additionally requires its serialized
    /// admission fence.
    pub fn settled(&self) -> bool {
        !self.kind.is_working() && self.queued == 0
    }
}

/// The Claude runtime's own view of its transcript and queue. Internal
/// to that loop; the Rho loop keeps its history as blocks of its own.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentState {
    /// Rho-runtime blocks are append-only. Provider-managed runtimes may
    /// replace this with a compacted transcript snapshot when the provider
    /// rewrites history.
    pub blocks: Vec<Arc<ContextBlock>>,
    /// Inputs waiting to enter model context, in arrival order.
    pub queued_inputs: InputQueues,
    pub kind: AgentStateKind,
    /// Tokens occupying the model's context window after the latest
    /// response (all input, cached or not, plus that response's output).
    /// `None` until the agent's first response reports usage.
    pub context_used: Option<u64>,
    /// Cumulative provider-reported usage across this agent's requests.
    pub total_usage: db::AgentUsageBucket,
    pub usage_provider: db::AgentUsageModel,
}

/// Pending inputs in arrival order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputQueues {
    items: Vec<QueuedInput>,
}

impl InputQueues {
    pub fn push(&mut self, item: QueuedInput) {
        self.items.push(item);
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Pending items in arrival order, for rendering.
    pub fn iter(&self) -> impl Iterator<Item = &QueuedInput> {
        self.items.iter()
    }

    /// Remove the first pending item matching `pred`.
    pub fn remove_first(&mut self, pred: impl FnMut(&QueuedInput) -> bool) -> Option<QueuedInput> {
        let pos = self.items.iter().position(pred)?;
        Some(self.items.remove(pos))
    }

    pub fn retain(&mut self, pred: impl FnMut(&QueuedInput) -> bool) {
        self.items.retain(pred);
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
// should be cheap to clone, it is cloned a lot
pub enum AgentStateKind {
    ApiStreaming {
        pending_response: PendingInferenceResponse,
        previous_attempt: Option<FailedInferenceResponse>,
    },
    /// Calls are running, or the model asked to be left alone until
    /// `waiting`.
    ToolCalling {
        previews: BTreeMap<ToolCallId, ToolPreview>,
        /// Results of the calls that have finished so far. The Rho runtime
        /// reports results through its blocks; this is for runtimes that
        /// hold them back.
        results: Vec<ToolResult>,
        /// When the model's `wait` runs out, if it asked for one.
        waiting: Option<UnixMs>,
    },
    /// Loaded from a log that ended with calls nobody answered: the next
    /// request owes them placeholder results and a note.
    UnfinishedTurn {
        outstanding_calls: Arc<[rho_inference::types::ExecId]>,
    },
    // Permanent error, thread is paused
    Error(FailedInferenceResponse),
    Idle,
}

/// Tells the log that a turn started or stopped. Both runtimes cross the
/// same edge (working, then not working), and a head's `turn_running` is
/// the fold of the two events.
pub(crate) fn turn_edge(
    previous: &AgentStateKind,
    current: &AgentStateKind,
    attempt_started: bool,
) -> Option<TurnEdge> {
    if !previous.is_working() && current.is_working() {
        Some(TurnEdge::Started)
    } else if execution_settled(previous, current, attempt_started) {
        Some(TurnEdge::Ended(match current {
            AgentStateKind::Error(failed) => TurnOutcome::Errored {
                message: failed.error.to_string(),
            },
            _ => TurnOutcome::Completed,
        }))
    } else {
        None
    }
}

/// A reliable state-machine transition that returns execution to the user's
/// court. A queued successor remains working and therefore does not settle
/// between turns; entering Error settles even when initialization failed
/// before a working snapshot was published.
pub(crate) fn execution_settled(
    previous: &AgentStateKind,
    current: &AgentStateKind,
    attempt_started: bool,
) -> bool {
    (previous.is_working() && !current.is_working()) || (attempt_started && !current.is_working())
}

impl AgentStateKind {
    /// Whether the agent is actively executing a turn.
    pub fn is_working(&self) -> bool {
        matches!(self, Self::ApiStreaming { .. } | Self::ToolCalling { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ToolPreview {
    pub call: ToolCall,
    pub started_at: UnixMs,
    pub metadata: Option<ToolPreviewMetadata>,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum ToolPreviewMetadata {
    ShellCommand { output_tail: String },
    ApplyPatch(ApplyPatchMetadata),
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct FailedInferenceResponse {
    pub partial_response: PendingInferenceResponse,
    pub attempt_count: NonZeroU64,
    pub error: Arc<String>,
}

/// An agent's view of its workset. One value per agent: the mount
/// namespace inside is built on the first command and shared by every
/// process the agent runs.
pub type View = rho_fs_view::Namespace;

/// Where a new agent starts: its view, the workspace record describing
/// it, and the workset this creation made for it, which the pool discards
/// if the creation fails. The view may still be being placed (cloned and
/// entered) when the agent is created: creation returns at once and the
/// agent's first command waits for the placing.
#[derive(Clone)]
pub struct StartPlace {
    pub(crate) view: Arc<lazy::Lazy<Arc<View>>>,
    pub place: Place,
    pub owned_workset: Option<String>,
}

impl StartPlace {
    /// A start in `view`; `origin` is what was cloned to make its workset,
    /// when this creation cloned it.
    pub fn new(view: Arc<View>, origin: Option<camino::Utf8PathBuf>) -> Self {
        let place = Place {
            workset: view.workset_id().to_owned(),
            cwd: view.cwd().to_owned(),
            mode: view.workset_mode(),
            origin,
        };
        Self {
            view: Arc::new(lazy::Lazy::ready(view)),
            place,
            owned_workset: None,
        }
    }

    /// A start whose view `place` makes on first use (and again on the
    /// next use if it failed): the workset the place names exists, the
    /// checkout in it may not yet.
    pub fn pending<F, Fut>(place: Place, view: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = anyhow::Result<Arc<View>>> + Send + 'static,
    {
        Self {
            view: Arc::new(lazy::Lazy::new(view)),
            place,
            owned_workset: None,
        }
    }

    /// Marks the workset as made by this creation.
    pub fn owning_workset(mut self) -> Self {
        self.owned_workset = Some(self.place.workset.clone());
        self
    }
}

pub fn final_answer_text(items: &[InferenceResponseItem]) -> String {
    let text_of = |want_final: bool| {
        items
            .iter()
            .filter_map(|item| match item {
                InferenceResponseItem::AssistantMessage { content, phase, .. }
                    if !want_final
                        || *phase == Some(rho_agent_types::MessagePhase::FinalAnswer) =>
                {
                    Some(content.iter().filter_map(|part| match part {
                        ContentPart::Text { text } => Some(text.as_str()),
                        ContentPart::Image { .. } => None,
                    }))
                }
                _ => None,
            })
            .flatten()
            .collect::<Vec<_>>()
            .join("\n")
    };
    let final_text = text_of(true);
    if final_text.is_empty() {
        text_of(false)
    } else {
        final_text
    }
}

#[cfg(test)]
mod encoding_tests {
    use senax_encoder::{Decoder as _, Encoder as _};

    use super::*;
    use crate::db::AgentIdDomain;

    #[test]
    fn log_events_roundtrip_through_senax() {
        let events = vec![
            AgentEvent::Accepted(QueuedInput {
                source: MessageSender::Agent {
                    id: AgentId::from_counter(3, &AgentIdDomain(9)).unwrap(),
                },
                kind: InputKind::Message {
                    content: vec![ContentPart::Text {
                        text: "mail".to_owned(),
                    }],
                },
                delivery: MessageDelivery::NextRequest,
                at: UnixMs(7),
            }),
            AgentEvent::Accepted(QueuedInput {
                source: MessageSender::User,
                kind: InputKind::Compaction,
                delivery: MessageDelivery::NextRequest,
                at: UnixMs(8),
            }),
            AgentEvent::Native(crate::native::NativeEvent::RequestStarted {
                input: Vec::from(vec![ContextBlock::CompactionTrigger]),
                at: UnixMs(9),
                wake: None,
                context: None,
            }),
            AgentEvent::Native(crate::native::NativeEvent::ResponseFinished {
                output: Vec::from(Vec::new()),
                context_used: Some(12),
                usage: None,
                at: UnixMs(10),
            }),
            AgentEvent::Native(crate::native::NativeEvent::RequestStarted {
                input: Vec::from(vec![ContextBlock::DeveloperMessage {
                    text: "boundary".into(),
                }]),
                context: Some(ContextChange::Marked { retain_from: 1 }),
                at: UnixMs(11),
                wake: None,
            }),
            AgentEvent::Native(crate::native::NativeEvent::RequestStarted {
                input: Vec::from(Vec::new()),
                context: Some(ContextChange::Preparing {
                    retain_from: 1,
                    repair: true,
                }),
                at: UnixMs(11),
                wake: None,
            }),
            AgentEvent::Native(crate::native::NativeEvent::RequestStarted {
                input: Vec::from(vec![ContextBlock::ContextRotation { retain_from: 1 }]),
                at: UnixMs(11),
                wake: None,
                context: None,
            }),
            AgentEvent::Cleared { at: UnixMs(11) },
            AgentEvent::Turn {
                edge: TurnEdge::Ended(TurnOutcome::Errored {
                    message: "boom".to_owned(),
                }),
                at: UnixMs(12),
            },
            AgentEvent::Titled {
                title: Some("title".to_owned()),
                at: UnixMs(13),
            },
            AgentEvent::Wants {
                want: AgentWant::Ask,
                summary: Some("which one?".to_owned()),
                at: UnixMs(14),
            },
            AgentEvent::Rewound {
                to: crate::db::AgentEventPos::new(3),
                at: UnixMs(15),
            },
            AgentEvent::Cleared { at: UnixMs(0) },
            AgentEvent::ModeChanged {
                mode: WorksetMode::Exposed,
                at: UnixMs(16),
            },
            AgentEvent::Transcript {
                uuid: uuid::uuid!("00000000-0000-4000-8000-000000000002"),
                line: TranscriptLine::Assistant {
                    text: "read it".to_owned(),
                    calls: vec![TranscriptCall {
                        id: "toolu_1".to_owned(),
                        name: "Read".to_owned(),
                        arguments: "{\"path\":\"a.rs\"}".to_owned(),
                    }],
                    usage: None,
                    context_used: Some(4321),
                },
                at: UnixMs(17),
                wake: None,
            },
        ];
        let current = events
            .iter()
            .filter_map(|event| {
                event
                    .native_event()
                    .map(|event| AgentEvent::Native(event.clone()))
            })
            .collect::<Vec<_>>();
        for event in events.into_iter().chain(current) {
            let mut buffer = bytes::BytesMut::new();
            event.encode(&mut buffer).expect("encode");
            let mut reader = buffer.freeze();
            let decoded = AgentEvent::decode(&mut reader).expect("decode");
            assert_eq!(decoded, event);
        }
    }
}
