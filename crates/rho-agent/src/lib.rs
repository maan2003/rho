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
    ProviderResponseId, ToolCall, ToolCallId, ToolResult, ToolSpec, ToolUpdate, UnixMs,
};
pub use rho_core::{MessageDelivery, MessageSender};
use rho_db::RhoDb;
use rho_workspaces::{Repo, Workspace, WorkspaceInfo};
use senax_encoder::{Decode, Encode, Pack, Unpack};

use crate::db::{
    AgentEventPos, AgentId, AgentPresentationUpdate, AgentRole, AgentRuntime, AgentSpawnedBy,
    AgentWriteTxnExt as _, ClaudeRewind, SessionBinding, UnixMillis,
};

pub mod agent;
mod claude;
pub use agent::{AgentHandle, WAIT_TOOL_NAME, render_agent_surface};
pub use claude::{backfill_last_turn_ended_from_claude_messages, last_assistant_message_at};

pub mod db;
mod image_tool;
mod lazy;
pub mod multi_agent_tools;
pub mod pool;
pub mod presentation;
pub mod story;
pub mod story_backfill;
pub mod story_fixture;
#[cfg(test)]
mod story_sizing;
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
/// The Rho runtime writes `Accepted`, `QueueCleared`, `Sent` and `Replied`;
/// the head's config events are written by the store on the agent's behalf.
/// The rest are what earlier runtimes wrote and are decoded only, so a log
/// from before the current loop still replays (`agent::replay`).
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
    },
    /// The model answered, and the request is over.
    Replied {
        blocks: Cow<'a, [ContextBlock]>,
        /// Context-window occupancy after this response (all input plus
        /// output tokens), or `None` when it compacted or usage was missing.
        context_used: Option<u64>,
    },
    /// All queued items were dropped (cancel).
    QueueCleared,

    // -- what the previous Rho loop wrote; decoded, never written ------------
    InferenceResponse {
        items: Cow<'a, [InferenceResponseItem]>,
        provider_response_id: Option<ProviderResponseId>,
        context_used: Option<u64>,
    },
    ToolResult {
        result: Cow<'a, ToolResult>,
    },
    Queued(QueuedItem),
    Dequeued {
        boundary: LegacyDelivery,
    },
    /// Once the presentation's own record; the story carries it now.
    PresentationUpdated {
        update: AgentPresentationUpdate,
    },

    // -- the runtimes' shared config log --------------------------------------
    /// A text-only message confirmed in Claude Code's external transcript.
    /// It gives the shared presentation sidecar a durable, rewindable
    /// source without treating Claude's protocol state as native inference.
    ClaudePresentationSource {
        source_id: uuid::Uuid,
        speaker: PresentationSpeaker,
        text: Cow<'a, str>,
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
    },
    RoleChanged {
        role: AgentRole,
        /// `None` when only the role moved and the session binding stands.
        binding: Option<SessionBinding>,
    },
    WorkdirAdded {
        workdir: WorkspaceInfo,
    },
    /// The runtime itself changing under the agent: a Claude rewind before
    /// and after its destination transcript is verified, or a new prompt
    /// cache key for the Rho runtime.
    RuntimeRebound {
        change: RuntimeChange,
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
            Self::Queued(QueuedItem {
                kind:
                    QueuedItemKind::UserMessage {
                        sender: MessageSender::User,
                        ..
                    },
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

/// What the previous loop queued. Decoded from old logs only.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct QueuedItem {
    pub kind: QueuedItemKind,
    pub delivery: LegacyDelivery,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum QueuedItemKind {
    UserMessage {
        sender: MessageSender,
        content: Arc<Vec<ContentPart>>,
        #[senax(default)]
        source_id: Option<InputSourceId>,
    },
    Compaction,
    ToolUpdate(ToolUpdate),
}

/// The delivery lanes the previous loop had. `NextTurn` no longer exists
/// live; old rows that name it still have to decode, and replay reads it
/// as `NextRequest`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum LegacyDelivery {
    Immediate,
    NextRequest,
    NextTurn,
}

impl From<LegacyDelivery> for MessageDelivery {
    fn from(delivery: LegacyDelivery) -> Self {
        match delivery {
            LegacyDelivery::Immediate => Self::Immediate,
            LegacyDelivery::NextRequest | LegacyDelivery::NextTurn => Self::NextRequest,
        }
    }
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

/// Live runtime state of an agent turn.
#[derive(Clone, Debug, PartialEq)]
// should be cheap to clone, it is cloned a lot
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

/// Tells the story that a turn started or stopped. Both runtimes cross
/// the same edge (working, then not working), and the head's
/// `turn_running` is the fold of the two events.
pub(crate) async fn tell_turn_boundary(
    db: &RhoDb,
    agent_id: AgentId,
    previous: &AgentStateKind,
    current: &AgentStateKind,
    attempt_started: bool,
) {
    let now = UnixMillis::now();
    let event = if !previous.is_working() && current.is_working() {
        story::StoryEvent::TurnStarted { at: now }
    } else if execution_settled(previous, current, attempt_started) {
        story::StoryEvent::TurnEnded {
            outcome: match current {
                AgentStateKind::Error(failed) => story::TurnOutcome::Errored {
                    message: failed.error.to_string(),
                },
                _ => story::TurnOutcome::Completed,
            },
            at: now,
        }
    } else {
        return;
    };
    let mut write = db.write().await;
    write.append_agent_story(agent_id, &event);
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
            AgentEvent::Queued(QueuedItem {
                kind:
                    QueuedItemKind::UserMessage {
                        sender, content, ..
                    },
                ..
            }) => found(through, speaker_of(sender), rho_core::text_content(content)),
            AgentEvent::InferenceResponse { items, .. } => found(
                through,
                PresentationSpeaker::Assistant,
                assistant_text(items),
            ),
            AgentEvent::ClaudePresentationSource { speaker, text, .. } => {
                found(through, *speaker, text.to_string())
            }
            AgentEvent::Accepted(_)
            | AgentEvent::Sent { .. }
            | AgentEvent::ToolResult { .. }
            | AgentEvent::Queued(_)
            | AgentEvent::Dequeued { .. }
            | AgentEvent::QueueCleared
            | AgentEvent::PresentationUpdated { .. }
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
            },
            AgentEvent::Replied {
                blocks: Cow::Owned(Vec::new()),
                context_used: Some(12),
            },
            AgentEvent::Queued(QueuedItem {
                kind: QueuedItemKind::Compaction,
                delivery: LegacyDelivery::NextTurn,
            }),
            AgentEvent::Dequeued {
                boundary: LegacyDelivery::NextRequest,
            },
            AgentEvent::QueueCleared,
            AgentEvent::ClaudePresentationSource {
                source_id: uuid::uuid!("00000000-0000-4000-8000-000000000001"),
                speaker: PresentationSpeaker::Assistant,
                text: Cow::Borrowed("confirmed response"),
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
