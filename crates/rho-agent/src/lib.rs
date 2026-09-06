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

use rho_core::{
    ApplyPatchMetadata, ContentPart, ContextBlock, InferenceResponseItem, PendingInferenceResponse,
    ToolCall, ToolCallId, ToolResult, ToolSpec, UnixMs,
};
pub use rho_core::{MessageDelivery, MessageSender};
use rho_db::RhoDb;
use rho_workspaces::{Repo, Workspace, WorkspaceInfo};
use senax_encoder::{Decode, Encode, Pack, Unpack};

use crate::db::{
    AgentEventPos, AgentId, AgentRole, AgentRuntime, AgentSpawnedBy, AgentWant,
    AgentWriteTxnExt as _, ClaudeRewind, PresentationField, SessionBinding, TurnEdge, TurnOutcome,
    UnixMillis,
};

pub mod agent;
mod claude;
pub use agent::{AgentHandle, WAIT_TOOL_NAME, render_agent_surface};

pub mod db;
mod image_tool;
mod lazy;
pub mod live;
pub mod mirror;
pub mod multi_agent_tools;
pub mod pool;
pub mod presentation;
pub mod system_prompt;

const PRESENTATION_SOURCE_TAIL_BYTES: usize = 12 * 1024;

/// Model-facing prompt and top-level tools for a newly created role. Dynamic
/// agent identity/team text and stateful integration hosts are omitted.
pub struct RenderedAgentSurface {
    pub system_prompt: Arc<str>,
    pub tools: Arc<[ToolSpec]>,
}

/// One event of an agent's raw log.
///
/// The Rho runtime writes `Accepted`, `QueueCleared`, `Sent`, `Replied` and
/// `Failed`; the Claude runtime `ClaudePresentationSource`; the head's
/// config events are written by the store on the agent's behalf.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum AgentEvent<'a> {
    /// An input entered a queue: user text, mail, or a `/compact`. It becomes
    /// context when a later `Sent` carries it.
    Accepted(QueuedInput),
    /// A boundary: every source was drained into `blocks`, they were appended
    /// to history, and a request went out carrying all of it.
    ///
    /// One event rather than an append and a start, because it was always one
    /// thing — and because the drain, the append and the send cannot come
    /// apart even in a crash. `blocks` can be empty: a retry or a resume
    /// sends with nothing pending, which is a fact worth being able to write
    /// down.
    Sent {
        blocks: Cow<'a, [ContextBlock]>,
        #[senax(default)]
        at: UnixMs,
    },
    /// The model answered, and the request is over.
    Replied {
        blocks: Cow<'a, [ContextBlock]>,
        /// Context-window occupancy after this response (all input plus
        /// output tokens), or `None` when it compacted or usage was missing.
        context_used: Option<u64>,
        /// What the response cost, as the provider reported it. Told
        /// here, at the response, so a reader can price the transcript
        /// without a usage table (`AGENT-LOG-DESIGN.md`).
        #[senax(default)]
        usage: Option<crate::db::AgentUsageBucket>,
        #[senax(default)]
        at: UnixMs,
    },
    /// All queued items were dropped (cancel). Written before the log
    /// carried times; `Cleared` is what is written now.
    QueueCleared,
    Cleared {
        at: UnixMs,
    },
    /// A turn started or stopped: the edge both runtimes cross.
    Turn {
        edge: TurnEdge,
        at: UnixMs,
    },
    /// The sidecar's title and activity, applied.
    Presented {
        title: PresentationField,
        activity: PresentationField,
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

    // -- the runtimes' shared config log --------------------------------------
    /// A text-only message confirmed in Claude Code's external transcript.
    /// It gives the shared presentation sidecar a durable, rewindable
    /// source without treating Claude's protocol state as native inference.
    ClaudePresentationSource {
        source_id: uuid::Uuid,
        speaker: PresentationSpeaker,
        /// The message whole (rows from before the mirror existed hold
        /// the first kilobyte only).
        text: Cow<'a, str>,
        #[senax(default)]
        at: UnixMs,
    },
    /// The agent coming into being: the first event of every agent's log,
    /// and the base the head's config is folded from. A spawn name given
    /// here is why no title is generated for that agent.
    Created {
        role: AgentRole,
        binding: SessionBinding,
        runtime: AgentRuntime,
        workdirs: Vec<WorkspaceInfo>,
        spawned_by: AgentSpawnedBy,
        spawn_name: Option<String>,
        created_at: rho_core::UnixMs,
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
    WorkdirAdded {
        workdir: WorkspaceInfo,
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
}

impl AgentEvent<'_> {
    /// Whether this is a message the user typed, in either generation of
    /// the log: what a rewind counts turns by.
    pub fn is_user_message(&self) -> bool {
        match self {
            Self::Accepted(QueuedInput {
                source: MessageSender::User,
                kind: InputKind::Message { .. },
                ..
            }) => true,
            _ => false,
        }
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

/// Opaque tag the previous loop stored for the surface that submitted an
/// input. Kept so old rows decode; nothing reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Encode, Decode, Pack, Unpack)]
pub struct InputSourceId(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum PresentationSpeaker {
    User,
    Agent,
    Assistant,
}

/// A text-only, durably committed transcript source for the presentation
/// sidecar.
#[derive(Clone, Debug)]
pub struct PresentationSource {
    pub agent_id: AgentId,
    pub through: AgentEventPos,
    pub speaker: PresentationSpeaker,
    pub text: String,
}

/// What a loop publishes about itself: its phase, and how much input
/// waits. Everything else a reader wants is in the log.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentStatus {
    pub kind: AgentStateKind,
    /// Inputs waiting to enter model context.
    pub queued: usize,
}

impl AgentStatus {
    /// Nothing running and nothing waiting: safe to drop the loop.
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
        outstanding_calls: Arc<[ToolCall]>,
    },
    // Permanent error, thread is paused
    Error(FailedInferenceResponse),
    Idle,
}

/// Tells the log that a turn started or stopped. Both runtimes cross the
/// same edge (working, then not working), and a head's `turn_running` is
/// the fold of the two events.
pub(crate) async fn tell_turn_boundary(
    db: &RhoDb,
    agent_id: AgentId,
    previous: &AgentStateKind,
    current: &AgentStateKind,
    attempt_started: bool,
) {
    let now = UnixMillis::now();
    let edge = if !previous.is_working() && current.is_working() {
        TurnEdge::Started
    } else if execution_settled(previous, current, attempt_started) {
        TurnEdge::Ended(match current {
            AgentStateKind::Error(failed) => TurnOutcome::Errored {
                message: failed.error.to_string(),
            },
            _ => TurnOutcome::Completed,
        })
    } else {
        return;
    };
    let mut write = db.write().await;
    write.tell_turn(now, agent_id, edge);
    write.commit();
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

/// Where one of a new agent's workdirs comes from. Agents start from a
/// nonempty list of these; the first entry is the primary workdir.
pub enum StartWorkdir {
    /// Create a jj workspace on a new change on top of the revset.
    Create {
        repo: Arc<Repo>,
        parent_revset: String,
    },
    /// Create a jj workspace whose original VCS metadata is masked and whose
    /// child commands are Landlock-restricted.
    Sandbox {
        repo: Arc<Repo>,
        parent_revset: String,
    },
    /// Work in an existing workspace (joining another agent, the user's
    /// checkout, or a plain live directory).
    Existing(Arc<Workspace>),
}

/// Materializes a new agent's workdirs. Each jj repository allocates its own
/// managed workspace id.
pub(crate) async fn materialize_workdirs(
    start: Vec<StartWorkdir>,
) -> anyhow::Result<Vec<Arc<Workspace>>> {
    anyhow::ensure!(!start.is_empty(), "an agent needs at least one workdir");
    let mut entries = Vec::with_capacity(start.len());
    for entry in start {
        entries.push(match entry {
            StartWorkdir::Create {
                repo,
                parent_revset,
            } => repo.create_workspace(&parent_revset).await?,
            StartWorkdir::Sandbox {
                repo,
                parent_revset,
            } => repo.create_sandbox(&parent_revset).await?,
            StartWorkdir::Existing(workspace) => workspace,
        });
    }
    Ok(entries)
}

pub fn final_answer_text(items: &[InferenceResponseItem]) -> String {
    let text_of = |want_final: bool| {
        items
            .iter()
            .filter_map(|item| match item {
                InferenceResponseItem::AssistantMessage { content, phase, .. }
                    if !want_final || *phase == Some(rho_core::MessagePhase::FinalAnswer) =>
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

pub(crate) fn assistant_text(items: &[InferenceResponseItem]) -> String {
    items
        .iter()
        .filter_map(|item| match item {
            InferenceResponseItem::AssistantMessage { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|part| match part {
                        ContentPart::Text { text } => Some(text.as_str()),
                        ContentPart::Image { .. } => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The text a reply carries, if any. A `Replied` event holds one response
/// block; anything else in it is not the model speaking.
fn replied_text(blocks: &[ContextBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContextBlock::InferenceResponse { items, .. } => Some(assistant_text(items)),
            _ => None,
        })
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn presentation_sources(
    agent_id: AgentId,
    records: &[(AgentEventPos, AgentEvent<'static>)],
) -> Vec<PresentationSource> {
    let found = |through: &AgentEventPos, speaker, text: String| {
        (!text.trim().is_empty()).then_some(PresentationSource {
            agent_id,
            through: *through,
            speaker,
            text,
        })
    };
    let speaker_of = |sender: &MessageSender| match sender {
        MessageSender::User => PresentationSpeaker::User,
        MessageSender::Agent { .. } => PresentationSpeaker::Agent,
    };
    records
        .iter()
        .filter_map(|(through, event)| match event {
            AgentEvent::Accepted(QueuedInput {
                source,
                kind: InputKind::Message { content },
                ..
            }) => found(through, speaker_of(source), rho_core::text_content(content)),
            AgentEvent::Replied { blocks, .. } => found(
                through,
                PresentationSpeaker::Assistant,
                replied_text(blocks),
            ),
            AgentEvent::ClaudePresentationSource { speaker, text, .. } => {
                found(through, *speaker, text.to_string())
            }
            AgentEvent::Accepted(_)
            | AgentEvent::Sent { .. }
            | AgentEvent::QueueCleared
            | AgentEvent::Cleared { .. }
            | AgentEvent::Turn { .. }
            | AgentEvent::Presented { .. }
            | AgentEvent::Wants { .. }
            | AgentEvent::Rewound { .. }
            | AgentEvent::Failed { .. }
            | AgentEvent::Created { .. }
            | AgentEvent::RoleChanged { .. }
            | AgentEvent::WorkdirAdded { .. }
            | AgentEvent::RuntimeRebound { .. } => None,
        })
        .collect()
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
            AgentEvent::Sent {
                blocks: Cow::Owned(vec![ContextBlock::CompactionTrigger]),
                at: UnixMs(9),
            },
            AgentEvent::Replied {
                blocks: Cow::Owned(Vec::new()),
                context_used: Some(12),
                usage: None,
                at: UnixMs(10),
            },
            AgentEvent::Cleared { at: UnixMs(11) },
            AgentEvent::Turn {
                edge: TurnEdge::Ended(TurnOutcome::Errored {
                    message: "boom".to_owned(),
                }),
                at: UnixMs(12),
            },
            AgentEvent::Presented {
                title: PresentationField::Set("title".to_owned()),
                activity: PresentationField::Clear,
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
            AgentEvent::QueueCleared,
            AgentEvent::ClaudePresentationSource {
                source_id: uuid::uuid!("00000000-0000-4000-8000-000000000001"),
                speaker: PresentationSpeaker::Assistant,
                text: Cow::Borrowed("confirmed response"),
                at: UnixMs(16),
            },
        ];
        for event in events {
            let mut buffer = bytes::BytesMut::new();
            event.encode(&mut buffer).expect("encode");
            let mut reader = buffer.freeze();
            let decoded = AgentEvent::decode(&mut reader).expect("decode");
            assert_eq!(decoded, event);
        }
    }
}
