use std::borrow::Cow;
use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::{Arc, RwLock};

use async_stream::stream;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{Stream, StreamExt};
use rho_core::{
    ApplyPatchMetadata, ContentPart, ContextBlock, InferenceEvent, InferenceRequest,
    InferenceResponseItem, PendingInferenceResponse, ProviderResponseId, ToolCall, ToolCallId,
    ToolOutput, ToolOutputStatus, ToolResult, ToolResultMetadata, ToolSpec, UnixMs,
};
use rho_db::RhoDb;
use rho_inference::{InferenceAuth, InferenceSession, PromptCacheKey};
use rho_tool_shell::{DEFAULT_TIMEOUT_SECS, ShellTools};
use rho_workspaces::{Repo, Workspace};
use senax_encoder::{Decode, Encode, Pack, Unpack};
use tokio::sync::{Notify, mpsc, oneshot};

use crate::db::{
    AgentEventPos, AgentId, AgentMode, AgentReadTxnExt, AgentRuntime, AgentWriteTxnExt, DeepConfig,
    UnixMillis,
};
use crate::multi_agent_tools::MultiAgentTools;

pub mod claude;
pub mod db;
pub mod multi_agent_tools;
pub mod pool;
pub mod system_prompt;
pub mod title;

/// An agent timeline event. Some events fold into model context; future
/// runtime-only events, like tool output chunks, can live here without becoming
/// inference input.
#[derive(Clone, Debug, PartialEq, Encode)]
pub enum AgentEvent<'a> {
    InferenceResponse {
        items: Cow<'a, [InferenceResponseItem]>,
        provider_response_id: Option<ProviderResponseId>,
        /// Context-window occupancy reported with this response (all input
        /// plus output tokens). `None` in events persisted before this field
        /// existed, or when the provider omitted usage.
        context_used: Option<u64>,
    },
    ToolResult {
        result: Cow<'a, ToolResult>,
    },
    /// An input entered the agent's queue. It only becomes model context
    /// when a later `Dequeued` boundary delivers it, so the pending queue
    /// survives restarts.
    Queued(QueuedItem),
    /// The loop reached `boundary` and delivered the eligible lanes into
    /// model context. Only written when at least one item was delivered.
    Dequeued {
        boundary: MessageDelivery,
    },
    /// All queued items were dropped (cancel).
    QueueCleared,
}

/// Temporary: accepts the pre-queue event shape (`UserMessage`,
/// `CompactionTriggered`) so `migrate_queued_event_shape` can read old rows.
/// Delete with the migration and restore `derive(Decode)`.
impl senax_encoder::Decoder for AgentEvent<'static> {
    fn decode(reader: &mut impl bytes::Buf) -> senax_encoder::Result<Self> {
        #[derive(Decode)]
        enum Shadow {
            UserMessage {
                content: Vec<ContentPart>,
            },
            CompactionTriggered,
            InferenceResponse {
                items: Vec<InferenceResponseItem>,
                provider_response_id: Option<ProviderResponseId>,
                context_used: Option<u64>,
            },
            ToolResult {
                result: ToolResult,
            },
            Queued(QueuedItem),
            Dequeued {
                boundary: MessageDelivery,
            },
            QueueCleared,
        }

        Ok(match Shadow::decode(reader)? {
            Shadow::UserMessage { content } => AgentEvent::Queued(QueuedItem {
                kind: QueuedItemKind::UserMessage {
                    sender: MessageSender::User,
                    content: Arc::new(content),
                },
                delivery: MessageDelivery::Immediate,
            }),
            Shadow::CompactionTriggered => AgentEvent::Queued(QueuedItem {
                kind: QueuedItemKind::Compaction,
                delivery: MessageDelivery::Immediate,
            }),
            Shadow::InferenceResponse {
                items,
                provider_response_id,
                context_used,
            } => AgentEvent::InferenceResponse {
                items: Cow::Owned(items),
                provider_response_id,
                context_used,
            },
            Shadow::ToolResult { result } => AgentEvent::ToolResult {
                result: Cow::Owned(result),
            },
            Shadow::Queued(item) => AgentEvent::Queued(item),
            Shadow::Dequeued { boundary } => AgentEvent::Dequeued { boundary },
            Shadow::QueueCleared => AgentEvent::QueueCleared,
        })
    }
}

pub use rho_core::MessageSender;

/// Live runtime state of an agent turn.
#[derive(Clone, Debug, PartialEq)]
// should be cheap to clone, it is cloned a lot
pub struct AgentState {
    /// Rho-runtime blocks are append-only. Provider-managed runtimes may
    /// replace this with a compacted transcript snapshot when the provider
    /// rewrites history.
    pub blocks: Vec<Arc<ContextBlock>>,
    /// Invariant: immutable. Set once at construction and never changed for the
    /// life of the agent. Enforced by exposing no mutator.
    pub tool_specs: Arc<[ToolSpec]>,
    /// Invariant: immutable
    pub system_prompt: Arc<str>,
    /// Inputs waiting to enter model context. Persisted as
    /// `AgentEvent::Queued` at enqueue and replayed on load, so the pending
    /// queue survives restarts; delivery boundaries are marked by
    /// `AgentEvent::Dequeued`.
    pub queued_inputs: InputQueues,
    pub kind: AgentStateKind,
    /// Tokens occupying the model's context window after the latest
    /// response (all input, cached or not, plus that response's output).
    /// Restored on load from the event log (Rho runtime) or the session
    /// transcript (Claude runtime); `None` until the agent's first response
    /// reports usage.
    pub context_used: Option<u64>,
}

/// One input waiting in the agent's queue. Persisted verbatim inside
/// `AgentEvent::Queued`, so the live queue and the log share one shape.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct QueuedItem {
    pub kind: QueuedItemKind,
    pub delivery: MessageDelivery,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum QueuedItemKind {
    // content is Arc'd because the queue rides AgentState, which is cloned a lot
    UserMessage {
        sender: MessageSender,
        content: Arc<Vec<ContentPart>>,
    },
    Compaction,
}

/// Pending inputs in arrival order. Delivery filters by eligibility at the
/// boundary: `NextTurn` items wait for the turn to end, while later
/// deliverable items may enter context earlier. Replay applies the same
/// boundary filters, so the live loop and event log agree.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputQueues {
    items: Vec<QueuedItem>,
}

impl InputQueues {
    pub fn push(&mut self, item: QueuedItem) {
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

    fn eligible(item: &QueuedItem, boundary: MessageDelivery) -> bool {
        boundary == MessageDelivery::NextTurn || item.delivery != MessageDelivery::NextTurn
    }

    /// How many pending items would deliver at `boundary`.
    pub fn deliverable(&self, boundary: MessageDelivery) -> usize {
        self.items
            .iter()
            .filter(|item| Self::eligible(item, boundary))
            .count()
    }

    /// Pending items in arrival order, for rendering.
    pub fn iter(&self) -> impl Iterator<Item = &QueuedItem> {
        self.items.iter()
    }

    /// Remove the first pending item matching `pred`.
    pub fn remove_first(&mut self, pred: impl FnMut(&QueuedItem) -> bool) -> Option<QueuedItem> {
        let pos = self.items.iter().position(pred)?;
        Some(self.items.remove(pos))
    }

    pub fn retain(&mut self, pred: impl FnMut(&QueuedItem) -> bool) {
        self.items.retain(pred);
    }

    /// Remove and return the items eligible at `boundary`, in arrival order.
    /// `NextTurn` (the turn is over) delivers everything; earlier boundaries
    /// hold `NextTurn` items back.
    pub fn drain(&mut self, boundary: MessageDelivery) -> Vec<QueuedItem> {
        if boundary == MessageDelivery::NextTurn {
            return std::mem::take(&mut self.items);
        }
        let (drained, held) = std::mem::take(&mut self.items)
            .into_iter()
            .partition(|item| Self::eligible(item, boundary));
        self.items = held;
        drained
    }
}

/// When a message sent while the agent is busy enters model context.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum MessageDelivery {
    /// Nothing to wait for: the message opens a turn right away. Renders as a
    /// plain user message, never with a queue label. Sent while busy it
    /// behaves like `NextRequest`.
    Immediate,
    /// Steer the current turn: delivered at the next inference request, i.e.
    /// right after the in-flight tool batch commits its results.
    NextRequest,
    /// Wait until the current turn finishes, then start a new turn.
    NextTurn,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
// should be cheap to clone, it is cloned a lot
pub enum AgentStateKind {
    ApiStreaming {
        pending_response: PendingInferenceResponse,
        previous_attempt: Option<FailedInferenceResponse>,
    },
    // we are now calling tools now
    // Note: in future we might add ToolCallingWhileStreaming state for proactive execution while
    // streaming
    ToolCalling {
        previews: BTreeMap<ToolCallId, ToolPreview>,
        // Results of the calls that have finished so far.
        // Communication of tool calls is done out of band; tools may persist
        // richer execution updates separately.
        results: Vec<ToolResult>,
        // This batch's armed `wait` call, if any. The loop resolves it
        // itself (deliverable input arrives, or the deadline passes); this
        // clears back to None when it does.
        waiting: Option<WaitState>,
    },
    // Restored from an event log that ended after a tool-calling response.
    UnfinishedTurn {
        // Can be empty: the restored turn may have answered every tool call, but
        // still stopped before rho observed a final assistant response.
        outstanding_calls: Arc<[ToolCall]>,
        // Completed tool calls restored for this unfinished turn but not yet
        // committed into model context.
        completed_tool_calls: Arc<[ToolResult]>,
    },
    // Permanent error, thread is paused
    Error(FailedInferenceResponse),
    Idle,
}

struct RestoreToolTurn {
    outstanding_calls: Vec<ToolCall>,
    completed_tool_calls: Vec<ToolResult>,
}

/// The context block a queued input becomes at delivery.
fn delivered_block(item: QueuedItem) -> ContextBlock {
    match item.kind {
        QueuedItemKind::UserMessage { sender, content } => ContextBlock::UserMessage {
            sender,
            content: Arc::try_unwrap(content).unwrap_or_else(|content| (*content).clone()),
        },
        QueuedItemKind::Compaction => ContextBlock::CompactionTrigger,
    }
}

struct RestoredAgent {
    blocks: Vec<Arc<ContextBlock>>,
    kind: AgentStateKind,
    context_used: Option<u64>,
    queued_inputs: InputQueues,
}

impl Default for RestoredAgent {
    /// A fresh agent: nothing restored, idle.
    fn default() -> Self {
        Self {
            blocks: Vec::new(),
            kind: AgentStateKind::Idle,
            context_used: None,
            queued_inputs: InputQueues::default(),
        }
    }
}

fn restore_events(events: Vec<AgentEvent<'static>>) -> RestoredAgent {
    let mut blocks = Vec::new();
    let mut turn: Option<RestoreToolTurn> = None;
    let mut context_used = None;
    let mut queue = InputQueues::default();
    let commit_finished_turn =
        |turn: &mut Option<RestoreToolTurn>, blocks: &mut Vec<Arc<ContextBlock>>| {
            let Some(turn) = turn.take() else {
                return;
            };
            if !turn.completed_tool_calls.is_empty() {
                blocks.push(Arc::new(ContextBlock::ToolResults {
                    results: turn.completed_tool_calls,
                }));
            }
        };
    for event in events {
        match event {
            AgentEvent::InferenceResponse {
                items,
                provider_response_id,
                context_used: response_context_used,
            } => {
                if response_context_used.is_some() {
                    context_used = response_context_used;
                }
                commit_finished_turn(&mut turn, &mut blocks);
                let outstanding_calls = items
                    .iter()
                    .filter_map(|item| match item {
                        InferenceResponseItem::ToolCall {
                            id,
                            name,
                            tool_type,
                            arguments,
                        } => Some(ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            tool_type: *tool_type,
                            arguments: arguments.clone(),
                        }),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                if !outstanding_calls.is_empty() {
                    turn = Some(RestoreToolTurn {
                        outstanding_calls,
                        completed_tool_calls: Vec::new(),
                    });
                }
                blocks.push(Arc::new(ContextBlock::InferenceResponse {
                    items: items.into_owned(),
                    provider_response_id,
                }));
            }
            AgentEvent::ToolResult { result } => {
                let Some(turn) = &mut turn else {
                    unreachable!("tool result restored without a preceding tool call");
                };
                turn.outstanding_calls
                    .retain(|call| call.id != result.call_id);
                turn.completed_tool_calls.push(result.into_owned());
            }
            AgentEvent::Queued(item) => queue.push(item),
            AgentEvent::Dequeued { boundary } => {
                // A mid-turn (`NextRequest`) delivery point means the tool
                // batch committed and a new request went out: flush the batch
                // block but keep the turn open so an interrupted log still
                // restores as an unfinished turn.
                let keep_mid_turn = boundary == MessageDelivery::NextRequest && turn.is_some();
                commit_finished_turn(&mut turn, &mut blocks);
                if keep_mid_turn {
                    turn = Some(RestoreToolTurn {
                        outstanding_calls: Vec::new(),
                        completed_tool_calls: Vec::new(),
                    });
                }
                for item in queue.drain(boundary) {
                    blocks.push(Arc::new(delivered_block(item)));
                }
            }
            AgentEvent::QueueCleared => queue.clear(),
        }
    }
    let kind = match turn {
        None => AgentStateKind::Idle,
        Some(RestoreToolTurn {
            outstanding_calls,
            completed_tool_calls,
        }) => AgentStateKind::UnfinishedTurn {
            outstanding_calls: outstanding_calls.into(),
            completed_tool_calls: completed_tool_calls.into(),
        },
    };
    RestoredAgent {
        blocks,
        kind,
        context_used,
        queued_inputs: queue,
    }
}

/// An armed `wait` tool call. Everything else about the call (arguments,
/// start time, tool type) lives in its entry in the batch's previews.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct WaitState {
    pub call_id: ToolCallId,
    /// Wall-clock deadline.
    pub until: UnixMs,
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

/// Cheap handle for observing and controlling the agent loop.
#[derive(Clone)]
pub struct Agent {
    state: Arc<RwLock<AgentState>>,
    control: mpsc::UnboundedSender<AgentControl>,
    notify: Arc<Notify>,
}

/// Where a new agent's workspace comes from.
pub enum StartWorkspace {
    /// Create a jj workspace on a new change on top of the revset.
    Create {
        repo: Arc<Repo>,
        parent_revset: String,
    },
    /// Work in an existing workspace (joining another agent, or the user's
    /// checkout).
    Existing(Arc<Workspace>),
}

impl Agent {
    pub async fn create(
        db: RhoDb,
        auth: InferenceAuth,
        mode: AgentMode,
        topic_id: db::TopicId,
        display_name: Option<String>,
        start: StartWorkspace,
        parent: Option<AgentId>,
        // A dead Weak (e.g. `Weak::default()`) means no pool: the
        // multi-agent tools are not offered.
        pool: std::sync::Weak<pool::AgentPool>,
    ) -> anyhow::Result<(AgentId, Self)> {
        let prompt_cache_key = PromptCacheKey::generate();
        let config = mode
            .deep_config()
            .ok_or_else(|| anyhow::anyhow!("cannot create Rho runtime for Claude agent mode"))?;
        // One transaction spans id allocation, the jj workspace creation
        // (the workspace is named after the id), and the record write:
        // failure anywhere drops the transaction, leaving nothing behind —
        // not even the id counter bump.
        let mut write = db.write().await;
        let agent_id = write.alloc_agent_id();
        let workspace = match start {
            StartWorkspace::Create {
                repo,
                parent_revset,
            } => {
                let workspace_id = write.alloc_workspace_id();
                repo.create_workspace(workspace_id, &parent_revset).await?
            }
            StartWorkspace::Existing(workspace) => workspace,
        };
        let now = UnixMillis::now();
        let next_event = write.create_agent(
            now,
            agent_id,
            topic_id,
            display_name,
            workspace.info().clone(),
            mode,
            AgentRuntime::Rho { prompt_cache_key },
            parent,
        );
        write.commit();
        let agent = Self::new(
            db,
            auth,
            config,
            prompt_cache_key,
            agent_id,
            next_event,
            workspace,
            parent,
            pool,
            RestoredAgent::default(),
        );
        Ok((agent_id, agent))
    }

    pub fn load(
        db: RhoDb,
        auth: InferenceAuth,
        agent_id: AgentId,
        workspace: Arc<Workspace>,
        // A dead Weak (e.g. `Weak::default()`) means no pool: the
        // multi-agent tools are not offered.
        pool: std::sync::Weak<pool::AgentPool>,
    ) -> Self {
        let record = db.read().get_agent(agent_id);
        let (next_event, events) = db.read().agent_events(agent_id);
        let restored = restore_events(events);
        let AgentRuntime::Rho { prompt_cache_key } = record.runtime else {
            panic!("cannot load Claude agent with the Rho agent runtime");
        };
        let config = record
            .mode
            .deep_config()
            .expect("Rho runtime stored with non-Rho agent mode");
        // The record, not the caller, is the source of truth for the parent
        // edge of an existing agent.
        Self::new(
            db,
            auth,
            config,
            prompt_cache_key,
            agent_id,
            next_event,
            workspace,
            record.parent_agent,
            pool,
            restored,
        )
    }

    /// Shared tail of [`Self::create`] and [`Self::load`]: wire the session,
    /// tools, and (possibly restored) state into a running loop.
    #[expect(clippy::too_many_arguments)]
    fn new(
        db: RhoDb,
        auth: InferenceAuth,
        config: DeepConfig,
        prompt_cache_key: PromptCacheKey,
        agent_id: AgentId,
        next_event: AgentEventPos,
        workspace: Arc<Workspace>,
        parent: Option<AgentId>,
        pool: std::sync::Weak<pool::AgentPool>,
        restored: RestoredAgent,
    ) -> Self {
        let inference_session = InferenceSession::new_deep(auth, config, prompt_cache_key);
        let shell_tools = ShellTools::new(
            std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            Arc::clone(&workspace),
        );
        let multi_agent = pool
            .upgrade()
            .map(|_| MultiAgentTools::new(pool, agent_id, parent));
        let state = Arc::new(RwLock::new(AgentState {
            blocks: restored.blocks,
            tool_specs: agent_tool_specs(&shell_tools, multi_agent.is_some()),
            system_prompt: system_prompt::prompt(workspace.as_ref(), multi_agent.as_ref()),
            queued_inputs: restored.queued_inputs,
            kind: restored.kind,
            context_used: restored.context_used,
        }));
        let (control, control_rx) = mpsc::unbounded_channel();
        let notify = Arc::new(Notify::new());
        let agent_loop = AgentLoop {
            inference_session,
            pending_tools: FuturesUnordered::new(),
            state: Arc::clone(&state),
            notify: Arc::clone(&notify),
            control_rx,
            shell_tools,
            workspace,
            persistence: AgentPersistence {
                db,
                agent_id,
                next_event,
            },
            multi_agent,
        };
        tokio::spawn(agent_loop.run());
        Self {
            state,
            control,
            notify,
        }
    }

    pub fn state(&self) -> AgentState {
        self.state.read().expect("poison").clone()
    }

    pub fn blocks(&self) -> Vec<Arc<ContextBlock>> {
        self.state().blocks
    }

    /// Send a message. If the agent is busy it queues and enters model context
    /// at the point `delivery` names; otherwise it starts a turn immediately.
    pub fn send_user_message(&self, text: impl Into<String>, delivery: MessageDelivery) {
        let _ = self.control.send(AgentControl::UserMessage {
            sender: MessageSender::User,
            content: vec![ContentPart::Text { text: text.into() }],
            delivery,
        });
    }

    /// Deliver mail from another agent. Enters context as an
    /// [`ContextBlock::AgentMessage`] identifying the sender.
    pub fn send_agent_message(
        &self,
        sender: AgentId,
        text: impl Into<String>,
        delivery: MessageDelivery,
    ) {
        let _ = self.control.send(AgentControl::UserMessage {
            sender: MessageSender::Agent { id: sender },
            content: vec![ContentPart::Text { text: text.into() }],
            delivery,
        });
    }

    pub fn compact(&self, delivery: MessageDelivery) {
        let _ = self.control.send(AgentControl::Compact { delivery });
    }

    /// Stop the current turn and drop all queued inputs.
    pub fn cancel(&self) {
        let _ = self.control.send(AgentControl::Cancel);
    }

    pub fn continue_unfinished(&self) {
        let _ = self.control.send(AgentControl::ContinueUnfinished);
    }

    pub fn set_deep_config(&self, config: DeepConfig) {
        let _ = self.control.send(AgentControl::SetDeepConfig(config));
    }

    pub async fn rewind(&self, turns: u32) -> anyhow::Result<()> {
        let (reply, result) = oneshot::channel();
        self.control
            .send(AgentControl::Rewind { turns, reply })
            .map_err(|_| anyhow::anyhow!("agent control loop is closed"))?;
        result
            .await
            .map_err(|_| anyhow::anyhow!("agent control loop is closed"))?
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

/// Arm the batch's single wait slot from a `wait` tool call. Errors
/// (duplicate wait, bad arguments) become ordinary error tool results.
fn arm_wait(
    waiting: &mut Option<WaitState>,
    call: &ToolCall,
    started_at: UnixMs,
) -> anyhow::Result<()> {
    if waiting.is_some() {
        anyhow::bail!("a wait is already in progress in this tool batch");
    }
    let timeout_seconds = multi_agent_tools::parse_wait_timeout(&call.arguments)?;
    *waiting = Some(WaitState {
        call_id: call.id.clone(),
        until: UnixMs(started_at.0 + timeout_seconds * 1000),
    });
    Ok(())
}

/// An error outcome for a call that never ran.
fn error_tool_result(call: &ToolCall, started_at: UnixMs, error: anyhow::Error) -> ToolResult {
    ToolResult {
        call_id: call.id.clone(),
        tool_type: call.tool_type,
        body: ToolOutput {
            output: Arc::new(error.to_string()),
            status: ToolOutputStatus::Error,
        },
        started_at,
        finished_at: UnixMs::now(),
        metadata: None,
    }
}

fn agent_tool_specs(shell_tools: &ShellTools, multi_agent: bool) -> Arc<[ToolSpec]> {
    let mut specs = shell_tools.specs();
    if multi_agent {
        specs.extend(multi_agent_tools::agent_tool_specs());
    }
    specs.into()
}

struct AgentPersistence {
    db: RhoDb,
    agent_id: AgentId,
    next_event: AgentEventPos,
}

enum AgentControl {
    UserMessage {
        sender: MessageSender,
        content: Vec<ContentPart>,
        delivery: MessageDelivery,
    },
    Compact {
        delivery: MessageDelivery,
    },
    SetDeepConfig(DeepConfig),
    Rewind {
        turns: u32,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    Cancel,
    ContinueUnfinished,
}

struct AgentLoop {
    inference_session: InferenceSession,
    /// The tool calls from the current `ToolCalling` turn, running
    /// concurrently. Empty in every other state. Driven as a `select!` arm
    /// alongside the provider stream.
    pending_tools: FuturesUnordered<BoxFuture<'static, ToolResult>>,
    state: Arc<RwLock<AgentState>>,
    notify: Arc<Notify>,
    control_rx: mpsc::UnboundedReceiver<AgentControl>,
    shell_tools: ShellTools,
    workspace: Arc<Workspace>,
    persistence: AgentPersistence,
    /// Present on pooled agents: identity + `Weak` pool handle for the
    /// built-in spawn/send/wait tools and parent result/error mail.
    multi_agent: Option<MultiAgentTools>,
}

impl AgentLoop {
    /// Drive the agent through one user turn: stream the provider response, run
    /// whatever tools the model calls, feed the results back, and repeat until
    /// it answers without calling tools (→ `Idle`) or the turn fails for good
    /// (→ `Error`). The whole state machine lives in this one loop on purpose.
    ///
    /// Messages arriving mid-turn queue instead of interrupting: the
    /// `NextRequest` lane drains right before each mid-turn inference request,
    /// the `NextTurn` lane when the turn completes (a non-empty queue then
    /// starts the next turn instead of going `Idle`). On `Error` the queue is
    /// held — no automatic retry — until the user sends another message or
    /// continues (drains everything), or cancels (drops everything).
    async fn run(mut self) {
        loop {
            let mut state = self.state.read().expect("poison").clone();
            // Disabled arms still evaluate their expression, so give
            // `sleep_until` a zero deadline when no wait is armed; the guard
            // keeps it from being polled.
            let armed_wait = match &state.kind {
                AgentStateKind::ToolCalling {
                    waiting: Some(wait),
                    ..
                } => Some(wait.clone()),
                _ => None,
            };
            let wait_deadline = tokio::time::Instant::now()
                + std::time::Duration::from_millis(
                    armed_wait
                        .as_ref()
                        .map(|wait| wait.until.0.saturating_sub(UnixMs::now().0))
                        .unwrap_or(0),
                );

            tokio::select! {
                biased;
                control = self.control_rx.recv() => {
                    let Some(control) = control else {
                        return;
                    };
                    match control {
                        AgentControl::UserMessage {
                            sender,
                            content,
                            delivery,
                        } => {
                            self.enqueue_message(&mut state, sender, content, delivery).await;
                        }
                        AgentControl::Compact { delivery } => {
                            let item = QueuedItem {
                                kind: QueuedItemKind::Compaction,
                                delivery,
                            };
                            self.persist_event(AgentEvent::Queued(item.clone())).await;
                            state.queued_inputs.push(item);
                            match &state.kind {
                                AgentStateKind::Idle | AgentStateKind::Error(_) => {
                                    assert!(!self.inference_session.has_active_request());
                                    assert!(self.pending_tools.is_empty());
                                    self.deliver_queued(&mut state, MessageDelivery::NextTurn)
                                        .await;
                                    self.send_request(&state);
                                    state.kind = AgentStateKind::ApiStreaming {
                                        pending_response: PendingInferenceResponse::default(),
                                        previous_attempt: None,
                                    };
                                }
                                AgentStateKind::ApiStreaming { .. }
                                | AgentStateKind::ToolCalling { .. }
                                | AgentStateKind::UnfinishedTurn { .. } => {}
                            }
                            self.maybe_resolve_wait(&mut state).await;
                        }
                        AgentControl::Cancel => {
                            self.inference_session.abort();
                            self.pending_tools.clear();
                            if !state.queued_inputs.is_empty() {
                                self.persist_event(AgentEvent::QueueCleared).await;
                                state.queued_inputs.clear();
                            }

                            state.kind = AgentStateKind::Idle;
                        }
                        AgentControl::ContinueUnfinished => {
                            assert!(!self.inference_session.has_active_request());
                            assert!(self.pending_tools.is_empty());
                            match std::mem::replace(&mut state.kind, AgentStateKind::Idle) {
                                AgentStateKind::UnfinishedTurn {
                                    outstanding_calls,
                                    completed_tool_calls,
                                } => {
                                    let mut results =
                                        completed_tool_calls.iter().cloned().collect::<Vec<_>>();
                                    for call in outstanding_calls.iter() {
                                        let result = interrupted_tool_result(call);
                                        self.persist_event(AgentEvent::ToolResult {
                                            result: Cow::Borrowed(&result),
                                        })
                                        .await;
                                        results.push(result);
                                    }
                                    if !results.is_empty() {
                                        state.blocks.push(Arc::new(ContextBlock::ToolResults {
                                            results,
                                        }));
                                    }
                                    self.deliver_queued(&mut state, MessageDelivery::NextTurn)
                                        .await;
                                    self.send_request(&state);
                                    state.kind = AgentStateKind::ApiStreaming {
                                        pending_response: PendingInferenceResponse::default(),
                                        previous_attempt: None,
                                    };
                                }
                                AgentStateKind::Error(previous_attempt) => {
                                    self.deliver_queued(&mut state, MessageDelivery::NextTurn)
                                        .await;
                                    self.send_request(&state);
                                    state.kind = AgentStateKind::ApiStreaming {
                                        pending_response: PendingInferenceResponse::default(),
                                        previous_attempt: Some(previous_attempt),
                                    };
                                }
                                // Idle with restored mail: continue delivers it.
                                AgentStateKind::Idle if !state.queued_inputs.is_empty() => {
                                    self.deliver_queued(&mut state, MessageDelivery::NextTurn)
                                        .await;
                                    self.send_request(&state);
                                    state.kind = AgentStateKind::ApiStreaming {
                                        pending_response: PendingInferenceResponse::default(),
                                        previous_attempt: None,
                                    };
                                }
                                other => {
                                    state.kind = other;
                                    continue;
                                }
                            }
                        }
                        AgentControl::SetDeepConfig(config) => {
                            let _ = self.inference_session.set_deep_config(config);
                        }
                        AgentControl::Rewind { turns, reply } => {
                            let result = if turns == 0 {
                                Err(anyhow::anyhow!(":rewind turns must be greater than zero"))
                            } else if !matches!(
                                state.kind,
                                AgentStateKind::Idle | AgentStateKind::Error(_)
                            ) {
                                Err(anyhow::anyhow!(
                                    ":rewind is only available while idle or errored; use :cancel first"
                                ))
                            } else if !state.queued_inputs.is_empty() {
                                Err(anyhow::anyhow!(
                                    ":rewind is not available with queued inputs"
                                ))
                            } else if !self.pending_tools.is_empty()
                                || self.inference_session.has_active_request()
                            {
                                Err(anyhow::anyhow!(
                                    ":rewind is not available while work is running"
                                ))
                            } else {
                                let db = self.persistence.db.clone();
                                let agent_id = self.persistence.agent_id;
                                let cursor = {
                                    let (_, records) = db.read().agent_event_records(agent_id);
                                    let user_positions = records
                                        .iter()
                                        .filter_map(|(pos, event)| {
                                            matches!(
                                                event,
                                                AgentEvent::Queued(QueuedItem {
                                                    kind: QueuedItemKind::UserMessage {
                                                        sender: MessageSender::User,
                                                        ..
                                                    },
                                                    ..
                                                })
                                            )
                                            .then_some(*pos)
                                        })
                                        .collect::<Vec<_>>();
                                    if user_positions.is_empty() {
                                        None
                                    } else {
                                        let index = user_positions
                                            .len()
                                            .saturating_sub(turns as usize);
                                        Some(user_positions[index])
                                    }
                                };
                                match cursor {
                                    None => Err(anyhow::anyhow!("nothing to rewind")),
                                    Some(cursor) => {
                                        let mut write = db.write().await;
                                        let next_event = write.fork_agent_lineage(
                                            UnixMillis::now(),
                                            agent_id,
                                            cursor,
                                        );
                                        write.commit();

                                        let (loaded_next_event, events) =
                                            db.read().agent_events(agent_id);
                                        debug_assert_eq!(loaded_next_event, next_event);
                                        let restored = restore_events(events);
                                        state.blocks = restored.blocks;
                                        state.kind = restored.kind;
                                        state.context_used = restored.context_used;
                                        state.queued_inputs = restored.queued_inputs;
                                        self.inference_session.abort();
                                        self.persistence.next_event = next_event;
                                        Ok(())
                                    }
                                }
                            };
                            let _ = reply.send(result);
                        }
                    }
                }
                update = self.inference_session.run() => {
                    let AgentStateKind::ApiStreaming {
                        mut pending_response,
                        previous_attempt,
                    } = std::mem::replace(&mut state.kind, AgentStateKind::Idle)
                    else {
                        unreachable!("provider streamed outside ApiStreaming");
                    };

                    match update {
                        InferenceEvent::RequestSent | InferenceEvent::StreamingStarted => {
                            state.kind = AgentStateKind::ApiStreaming {
                                pending_response,
                                previous_attempt,
                            };
                        }
                        InferenceEvent::ContextItem { index, event } => {
                            pending_response.apply(index, event);
                            state.kind = AgentStateKind::ApiStreaming {
                                pending_response,
                                previous_attempt,
                            };
                        }
                        InferenceEvent::TemporaryFailure { error, retrying_at: _ } => {
                            let attempt_count = previous_attempt
                                .map_or(NonZeroU64::MIN, |a| a.attempt_count.saturating_add(1));
                            state.kind = AgentStateKind::ApiStreaming {
                                pending_response: PendingInferenceResponse::default(),
                                previous_attempt: Some(FailedInferenceResponse {
                                    attempt_count,
                                    partial_response: pending_response,
                                    error: Arc::new(error.to_string()),
                                }),
                            };
                        }
                        InferenceEvent::Failed { error } => {
                            let attempt_count = previous_attempt
                                .map_or(NonZeroU64::MIN, |a| a.attempt_count.saturating_add(1));
                            // A silently stuck child is the failure mode worth
                            // surfacing: errors wake the parent.
                            self.mail_parent(
                                format!("Agent hit an error and stopped: {error}"),
                                MessageDelivery::NextRequest,
                            );
                            state.kind = AgentStateKind::Error(FailedInferenceResponse {
                                partial_response: pending_response,
                                attempt_count,
                                error: Arc::new(error.to_string()),
                            });
                        }
                        InferenceEvent::Finished {
                            usage,
                            provider_response_id,
                        } => {
                            let context_used = usage
                                .as_ref()
                                .map(|usage| usage.input_tokens + usage.output_tokens);
                            if context_used.is_some() {
                                state.context_used = context_used;
                            }
                            match pending_response.finish() {
                            Err(error) => {
                                let attempt_count = previous_attempt
                                    .map_or(NonZeroU64::MIN, |a| a.attempt_count.saturating_add(1));
                                self.mail_parent(
                                    format!("Agent hit an error and stopped: {error}"),
                                    MessageDelivery::NextRequest,
                                );
                                state.kind = AgentStateKind::Error(FailedInferenceResponse {
                                    partial_response: pending_response,
                                    attempt_count,
                                    error: Arc::new(error.to_string()),
                                });
                            }
                            Ok(items) => {
                                let calls: Vec<ToolCall> = items
                                    .iter()
                                    .filter_map(|item| match item {
                                        InferenceResponseItem::ToolCall {
                                            id,
                                            name,
                                            tool_type,
                                            arguments,
                                        } => Some(ToolCall {
                                            id: id.clone(),
                                            name: name.clone(),
                                            tool_type: *tool_type,
                                            arguments: arguments.clone(),
                                        }),
                                        _ => None,
                                    })
                                    .collect();
                                let final_text = calls.is_empty().then(|| final_answer_text(&items));
                                self.persist_event(AgentEvent::InferenceResponse {
                                    items: Cow::Borrowed(&items),
                                    provider_response_id: provider_response_id.clone(),
                                    context_used,
                                })
                                .await;
                                state.blocks.push(Arc::new(ContextBlock::InferenceResponse {
                                    items,
                                    provider_response_id,
                                }));
                                if calls.is_empty() {
                                    // A child's finished turn is its report:
                                    // mail the result to the parent so it can
                                    // react.
                                    let final_text = final_text.unwrap_or_default();
                                    self.mail_parent(
                                        if final_text.is_empty() {
                                            "(turn finished with no text response)".to_owned()
                                        } else {
                                            final_text
                                        },
                                        MessageDelivery::NextRequest,
                                    );
                                    // Turn complete: commit the checkout's
                                    // state so the user's jj view follows the
                                    // agent's work (fire-and-forget).
                                    let workspace = Arc::clone(&self.workspace);
                                    tokio::spawn(async move {
                                        if let Err(error) = workspace.snapshot().await {
                                            eprintln!("rho-agent: snapshot failed: {error:#}");
                                        }
                                    });
                                    if !state.queued_inputs.is_empty() {
                                        self.deliver_queued(&mut state, MessageDelivery::NextTurn)
                                            .await;
                                        self.send_request(&state);
                                        state.kind = AgentStateKind::ApiStreaming {
                                            pending_response: PendingInferenceResponse::default(),
                                            previous_attempt: None,
                                        };
                                    } else {
                                        state.kind = AgentStateKind::Idle;
                                    }
                                } else {
                                    let mut previews = BTreeMap::new();
                                    let mut waiting = None;
                                    for call in calls {
                                        let started_at = UnixMs::now();
                                        let preview_metadata = self
                                            .shell_tools
                                            .preview_metadata(&call)
                                            .map(tool_preview_metadata);
                                        previews.insert(
                                            call.id.clone(),
                                            ToolPreview {
                                                call: call.clone(),
                                                started_at,
                                                metadata: preview_metadata,
                                            },
                                        );
                                        // `wait` is resolved by the loop
                                        // itself, not run as a future: arm it
                                        // (or fail it in place) and move on.
                                        if self.multi_agent.is_some()
                                            && call.name.as_str()
                                                == multi_agent_tools::WAIT_TOOL_NAME
                                        {
                                            if let Err(error) =
                                                arm_wait(&mut waiting, &call, started_at)
                                            {
                                                self.pending_tools.push(Box::pin(
                                                    std::future::ready(error_tool_result(
                                                        &call, started_at, error,
                                                    )),
                                                ));
                                            }
                                            continue;
                                        }
                                        let shell_tools = self.shell_tools.clone();
                                        let agent_tools = multi_agent_tools::is_agent_tool(call.name.as_str())
                                            .then(|| self.multi_agent.clone())
                                            .flatten();
                                        self.pending_tools.push(Box::pin(async move {
                                            let call_id = call.id.clone();
                                            let tool_type = call.tool_type;
                                            let (body, metadata) = match agent_tools {
                                                Some(tools) => {
                                                    (multi_agent_tools::call_agent_tool(tools, call).await, None)
                                                }
                                                None => {
                                                    let output =
                                                        shell_tools.call_with_metadata(call).await;
                                                    (output.body, output.metadata)
                                                }
                                            };
                                            let finished_at = UnixMs::now();
                                            ToolResult {
                                                call_id,
                                                tool_type,
                                                body,
                                                started_at,
                                                finished_at,
                                                metadata,
                                            }
                                        }));
                                    }
                                    state.kind = AgentStateKind::ToolCalling {
                                        previews,
                                        results: Vec::new(),
                                        waiting,
                                    };
                                    // A wait armed over an already-pending
                                    // queue resolves right away.
                                    self.maybe_resolve_wait(&mut state).await;
                                }
                            }
                        }},
                    }
                }
                Some(result) = self.pending_tools.next() => {
                    self.finish_tool_call(&mut state, result).await;
                }
                _ = tokio::time::sleep_until(wait_deadline), if armed_wait.is_some() => {
                    self.resolve_wait(
                        &mut state,
                        "Wait timed out with no new messages.".to_owned(),
                    )
                    .await;
                }
            }
            *self.state.write().expect("poison") = state.clone();
            self.notify.notify_waiters();
        }
    }

    async fn persist_event(&mut self, event: AgentEvent<'_>) {
        let persistence = &mut self.persistence;
        let mut write = persistence.db.write().await;
        persistence.next_event = write.append_agent_event(persistence.next_event, &event);
        write.commit();
    }

    /// Persist and queue an incoming message (user input or agent mail),
    /// waking the agent if it is idle.
    async fn enqueue_message(
        &mut self,
        state: &mut AgentState,
        sender: MessageSender,
        content: Vec<ContentPart>,
        delivery: MessageDelivery,
    ) {
        let item = QueuedItem {
            kind: QueuedItemKind::UserMessage {
                sender,
                content: Arc::new(content),
            },
            delivery,
        };
        self.persist_event(AgentEvent::Queued(item.clone())).await;
        state.queued_inputs.push(item);
        self.wake_for_queued(state).await;
        // An armed wait counts any deliverable arrival.
        self.maybe_resolve_wait(state).await;
    }

    /// Mail the parent agent, if any (fire-and-forget).
    fn mail_parent(&self, body: String, delivery: MessageDelivery) {
        if let Some(multi_agent) = &self.multi_agent {
            multi_agent.mail_parent(body, delivery);
        }
    }

    /// Fold a finished tool call into the current batch; when the batch is
    /// done (no running tools, no armed wait) commit the results and continue
    /// the turn.
    async fn finish_tool_call(&mut self, state: &mut AgentState, result: ToolResult) {
        let AgentStateKind::ToolCalling {
            mut previews,
            mut results,
            waiting,
        } = std::mem::replace(&mut state.kind, AgentStateKind::Idle)
        else {
            unreachable!("tool finished outside a tool batch");
        };
        previews.remove(&result.call_id);
        results.push(result);
        if self.pending_tools.is_empty() && waiting.is_none() {
            for result in &results {
                self.persist_event(AgentEvent::ToolResult {
                    result: Cow::Borrowed(result),
                })
                .await;
            }
            state
                .blocks
                .push(Arc::new(ContextBlock::ToolResults { results }));
            self.deliver_queued(state, MessageDelivery::NextRequest)
                .await;
            self.send_request(state);
            state.kind = AgentStateKind::ApiStreaming {
                pending_response: PendingInferenceResponse::default(),
                previous_attempt: None,
            };
        } else {
            state.kind = AgentStateKind::ToolCalling {
                previews,
                results,
                waiting,
            };
        }
    }

    /// Resolve the armed `wait` with `body`, folding it into the batch like
    /// any other tool result.
    async fn resolve_wait(&mut self, state: &mut AgentState, body: String) {
        let AgentStateKind::ToolCalling {
            previews, waiting, ..
        } = &mut state.kind
        else {
            return;
        };
        let Some(wait) = waiting.take() else { return };
        let preview = previews
            .get(&wait.call_id)
            .expect("armed wait has a preview");
        let result = ToolResult {
            call_id: wait.call_id.clone(),
            tool_type: preview.call.tool_type,
            body: ToolOutput {
                output: Arc::new(body),
                status: ToolOutputStatus::Success,
            },
            started_at: preview.started_at,
            finished_at: UnixMs::now(),
            metadata: None,
        };
        self.finish_tool_call(state, result).await;
    }

    /// Resolve an armed `wait` when the queue holds anything the batch's
    /// `NextRequest` boundary will actually deliver (`NextTurn` items wait
    /// for the turn to end, so they must not complete a wait).
    async fn maybe_resolve_wait(&mut self, state: &mut AgentState) {
        if !matches!(
            &state.kind,
            AgentStateKind::ToolCalling {
                waiting: Some(_),
                ..
            }
        ) {
            return;
        }
        let pending = state
            .queued_inputs
            .deliverable(MessageDelivery::NextRequest);
        if pending == 0 {
            return;
        }
        self.resolve_wait(
            state,
            format!(
                "Wait completed: {pending} queued message(s) will enter context after this \
                 tool batch."
            ),
        )
        .await;
    }

    /// Move queued inputs into model context at a delivery boundary.
    /// `boundary` is the point the loop has reached: `NextRequest` (about to
    /// issue a mid-turn inference request) delivers everything but the
    /// `NextTurn` lane; `NextTurn` (the turn is over) delivers all lanes.
    /// Inputs were persisted at enqueue, so delivery writes only the
    /// `Dequeued` boundary marker (when anything delivered), which replay
    /// re-executes.
    async fn deliver_queued(&mut self, state: &mut AgentState, boundary: MessageDelivery) {
        let delivered = state.queued_inputs.drain(boundary);
        if delivered.is_empty() {
            return;
        }
        self.persist_event(AgentEvent::Dequeued { boundary }).await;
        for item in delivered {
            state.blocks.push(Arc::new(delivered_block(item)));
        }
    }

    /// Start a turn to deliver queued messages when no turn is in flight.
    /// Busy states leave the queue for the in-turn delivery points; an
    /// unfinished restored turn is interrupted first.
    async fn wake_for_queued(&mut self, state: &mut AgentState) {
        match &state.kind {
            // Busy: the message waits in the queue for its delivery point.
            AgentStateKind::ApiStreaming { .. } | AgentStateKind::ToolCalling { .. } => {}
            // No turn in flight: deliver everything now (including messages
            // held through an Error).
            AgentStateKind::Idle | AgentStateKind::Error(_) => {
                assert!(!self.inference_session.has_active_request());
                assert!(self.pending_tools.is_empty());
                self.deliver_queued(state, MessageDelivery::NextTurn).await;
                self.send_request(state);
                state.kind = AgentStateKind::ApiStreaming {
                    pending_response: PendingInferenceResponse::default(),
                    previous_attempt: None,
                };
            }
            AgentStateKind::UnfinishedTurn { .. } => {
                assert!(!self.inference_session.has_active_request());
                assert!(self.pending_tools.is_empty());
                let AgentStateKind::UnfinishedTurn {
                    outstanding_calls,
                    completed_tool_calls,
                } = std::mem::replace(&mut state.kind, AgentStateKind::Idle)
                else {
                    unreachable!("checked unfinished turn");
                };
                let mut results = completed_tool_calls.iter().cloned().collect::<Vec<_>>();
                for call in outstanding_calls.iter() {
                    let result = interrupted_tool_result(call);
                    self.persist_event(AgentEvent::ToolResult {
                        result: Cow::Borrowed(&result),
                    })
                    .await;
                    results.push(result);
                }
                if !results.is_empty() {
                    state
                        .blocks
                        .push(Arc::new(ContextBlock::ToolResults { results }));
                }
                self.deliver_queued(state, MessageDelivery::NextTurn).await;
                self.send_request(state);
                state.kind = AgentStateKind::ApiStreaming {
                    pending_response: PendingInferenceResponse::default(),
                    previous_attempt: None,
                };
            }
        }
    }

    fn send_request(&mut self, state: &AgentState) {
        self.inference_session.request(InferenceRequest {
            instructions: state.system_prompt.clone(),
            input: state.blocks.clone(),
            agent_id_labels: self.agent_id_labels(&state.blocks),
            tools: Arc::clone(&state.tool_specs),
        });
    }

    fn agent_id_labels(
        &self,
        blocks: &[Arc<ContextBlock>],
    ) -> std::collections::BTreeMap<AgentId, Arc<str>> {
        let Some(tools) = &self.multi_agent else {
            return std::collections::BTreeMap::new();
        };
        blocks
            .iter()
            .filter_map(|block| match &**block {
                ContextBlock::UserMessage {
                    sender: MessageSender::Agent { id },
                    ..
                } => Some((*id, Arc::from(tools.display_id(*id)))),
                _ => None,
            })
            .collect()
    }
}

/// The turn's answer for reporting to a parent agent: final-channel text,
/// falling back to all assistant text when the model skipped phases.
fn final_answer_text(items: &[InferenceResponseItem]) -> String {
    let text_of = |want_final: bool| {
        items
            .iter()
            .filter_map(|item| match item {
                InferenceResponseItem::AssistantMessage { content, phase }
                    if !want_final || *phase == Some(rho_core::MessagePhase::FinalAnswer) =>
                {
                    Some(content.iter().filter_map(|part| match part {
                        ContentPart::Text { text } => Some(text.as_str()),
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

/// Receive mail when this agent has a mailbox; never resolves otherwise, and
/// resolves `None` when the pool re-registered the route (agent reloaded).
fn tool_preview_metadata(metadata: ToolResultMetadata) -> ToolPreviewMetadata {
    match metadata {
        ToolResultMetadata::ApplyPatch(metadata) => ToolPreviewMetadata::ApplyPatch(metadata),
    }
}

fn interrupted_tool_result(call: &ToolCall) -> ToolResult {
    let now = UnixMs::now();
    ToolResult {
        call_id: call.id.clone(),
        tool_type: call.tool_type,
        body: ToolOutput {
            output: Arc::new(
                "Tool execution was interrupted by a daemon restart. It may have completed \
                 partially."
                    .to_owned(),
            ),
            status: ToolOutputStatus::Error,
        },
        started_at: now,
        finished_at: now,
        metadata: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::AgentIdDomain;

    fn agent_id(counter: u64) -> AgentId {
        AgentId::from_counter(counter, &AgentIdDomain(7)).expect("counter fits")
    }

    fn text_parts(text: &str) -> Vec<ContentPart> {
        vec![ContentPart::Text {
            text: text.to_owned(),
        }]
    }

    fn queued_event(
        sender: MessageSender,
        text: &str,
        delivery: MessageDelivery,
    ) -> AgentEvent<'static> {
        AgentEvent::Queued(QueuedItem {
            kind: QueuedItemKind::UserMessage {
                sender,
                content: Arc::new(text_parts(text)),
            },
            delivery,
        })
    }

    fn response_event(items: Vec<InferenceResponseItem>) -> AgentEvent<'static> {
        AgentEvent::InferenceResponse {
            items: Cow::Owned(items),
            provider_response_id: None,
            context_used: None,
        }
    }

    fn tool_call(id: &str) -> InferenceResponseItem {
        InferenceResponseItem::ToolCall {
            id: ToolCallId::try_from(id).unwrap(),
            name: ToolName::try_from("shell_command").unwrap(),
            tool_type: rho_core::ToolType::Function,
            arguments: String::new(),
        }
    }

    fn tool_result(id: &str) -> AgentEvent<'static> {
        AgentEvent::ToolResult {
            result: Cow::Owned(ToolResult {
                call_id: ToolCallId::try_from(id).unwrap(),
                tool_type: rho_core::ToolType::Function,
                body: ToolOutput {
                    output: Arc::new("ok".to_owned()),
                    status: ToolOutputStatus::Success,
                },
                started_at: UnixMs(0),
                finished_at: UnixMs(0),
                metadata: None,
            }),
        }
    }

    use rho_core::ToolName;

    #[test]
    fn dequeue_at_next_turn_delivers_user_and_agent_mail() {
        let restored = restore_events(vec![
            queued_event(MessageSender::User, "hi", MessageDelivery::Immediate),
            queued_event(
                MessageSender::Agent { id: agent_id(1) },
                "done",
                MessageDelivery::NextRequest,
            ),
            AgentEvent::Dequeued {
                boundary: MessageDelivery::NextTurn,
            },
        ]);
        assert_eq!(
            restored.blocks.len(),
            2,
            "both lanes deliver at a turn boundary"
        );
        assert_eq!(
            *restored.blocks[0],
            ContextBlock::UserMessage {
                sender: MessageSender::User,
                content: text_parts("hi")
            }
        );
        assert_eq!(
            *restored.blocks[1],
            ContextBlock::UserMessage {
                sender: MessageSender::Agent { id: agent_id(1) },
                content: text_parts("done")
            }
        );
        assert!(restored.queued_inputs.is_empty());
        assert_eq!(restored.kind, AgentStateKind::Idle);
    }

    #[test]
    fn next_turn_lane_held_at_mid_turn_boundary() {
        let restored = restore_events(vec![
            queued_event(MessageSender::User, "steer", MessageDelivery::NextRequest),
            queued_event(MessageSender::User, "later", MessageDelivery::NextTurn),
            AgentEvent::Dequeued {
                boundary: MessageDelivery::NextRequest,
            },
        ]);
        assert_eq!(restored.blocks.len(), 1);
        assert_eq!(restored.queued_inputs.len(), 1);
        let held = restored.queued_inputs.iter().next().expect("held item");
        assert_eq!(held.delivery, MessageDelivery::NextTurn);
    }

    #[test]
    fn undelivered_queue_survives_restore() {
        let restored = restore_events(vec![queued_event(
            MessageSender::Agent { id: agent_id(2) },
            "pending mail",
            MessageDelivery::NextRequest,
        )]);
        assert!(restored.blocks.is_empty());
        assert_eq!(restored.queued_inputs.len(), 1);
        let pending = restored.queued_inputs.iter().next().expect("pending item");
        assert_eq!(
            pending.kind,
            QueuedItemKind::UserMessage {
                sender: MessageSender::Agent { id: agent_id(2) },
                content: Arc::new(text_parts("pending mail")),
            }
        );
    }

    #[test]
    fn drain_preserves_arrival_order_across_deliveries() {
        let item = |text: &str, delivery| QueuedItem {
            kind: QueuedItemKind::UserMessage {
                sender: MessageSender::User,
                content: Arc::new(text_parts(text)),
            },
            delivery,
        };
        let text = |item: &QueuedItem| match &item.kind {
            QueuedItemKind::UserMessage { content, .. } => {
                let ContentPart::Text { text } = &content[0];
                text.clone()
            }
            QueuedItemKind::Compaction => unreachable!(),
        };
        let mut queue = InputQueues::default();
        queue.push(item("steer", MessageDelivery::NextRequest));
        queue.push(item("later", MessageDelivery::NextTurn));
        queue.push(item("mail", MessageDelivery::NextRequest));
        queue.push(item("steer2", MessageDelivery::Immediate));

        assert_eq!(queue.deliverable(MessageDelivery::NextRequest), 3);
        let drained = queue.drain(MessageDelivery::NextRequest);
        assert_eq!(
            drained.iter().map(text).collect::<Vec<_>>(),
            ["steer", "mail", "steer2"],
            "arrival order holds, NextTurn is held back"
        );
        assert_eq!(
            queue
                .drain(MessageDelivery::NextTurn)
                .iter()
                .map(text)
                .collect::<Vec<_>>(),
            ["later"]
        );
        assert!(queue.is_empty());
    }

    #[test]
    fn queue_cleared_drops_pending_messages() {
        let restored = restore_events(vec![
            queued_event(MessageSender::User, "dropped", MessageDelivery::NextTurn),
            AgentEvent::QueueCleared,
        ]);
        assert!(restored.blocks.is_empty());
        assert!(restored.queued_inputs.is_empty());
    }

    #[test]
    fn mid_turn_dequeue_keeps_turn_unfinished() {
        let restored = restore_events(vec![
            queued_event(MessageSender::User, "start", MessageDelivery::Immediate),
            AgentEvent::Dequeued {
                boundary: MessageDelivery::NextTurn,
            },
            response_event(vec![tool_call("c1")]),
            tool_result("c1"),
            queued_event(MessageSender::User, "steer", MessageDelivery::NextRequest),
            AgentEvent::Dequeued {
                boundary: MessageDelivery::NextRequest,
            },
        ]);
        // The tool batch committed and the steer landed, but the turn's next
        // response never arrived: restore must offer continue.
        assert_eq!(
            restored.kind,
            AgentStateKind::UnfinishedTurn {
                outstanding_calls: Vec::new().into(),
                completed_tool_calls: Vec::new().into(),
            }
        );
        let last = restored.blocks.last().expect("delivered steer block");
        assert_eq!(
            **last,
            ContextBlock::UserMessage {
                sender: MessageSender::User,
                content: text_parts("steer")
            }
        );
    }
}

#[cfg(test)]
mod encoding_tests {
    use senax_encoder::{Decoder as _, Encoder as _};

    use super::*;
    use crate::db::AgentIdDomain;

    #[test]
    fn queue_events_roundtrip_through_senax() {
        let events = vec![
            AgentEvent::Queued(QueuedItem {
                kind: QueuedItemKind::UserMessage {
                    sender: MessageSender::Agent {
                        id: AgentId::from_counter(3, &AgentIdDomain(9)).unwrap(),
                    },
                    content: Arc::new(vec![ContentPart::Text {
                        text: "mail".to_owned(),
                    }]),
                },
                delivery: MessageDelivery::NextRequest,
            }),
            AgentEvent::Queued(QueuedItem {
                kind: QueuedItemKind::Compaction,
                delivery: MessageDelivery::NextTurn,
            }),
            AgentEvent::Dequeued {
                boundary: MessageDelivery::NextRequest,
            },
            AgentEvent::QueueCleared,
        ];
        for event in events {
            let mut buffer = bytes::BytesMut::new();
            event.encode(&mut buffer).expect("encode");
            let mut reader = buffer.freeze();
            let decoded = AgentEvent::decode(&mut reader).expect("decode");
            assert_eq!(decoded, event);
        }
    }

    /// Exercises the temporary legacy arm of the manual `Decoder` impl:
    /// pre-queue events decode as `Queued` items. Delete with the migration.
    #[test]
    fn legacy_events_decode_as_queued_items() {
        #[derive(Encode)]
        enum LegacyAgentEvent {
            UserMessage { content: Vec<ContentPart> },
            CompactionTriggered,
        }

        let content = vec![ContentPart::Text {
            text: "old log".to_owned(),
        }];
        let cases = [
            (
                LegacyAgentEvent::UserMessage {
                    content: content.clone(),
                },
                AgentEvent::Queued(QueuedItem {
                    kind: QueuedItemKind::UserMessage {
                        sender: MessageSender::User,
                        content: Arc::new(content),
                    },
                    delivery: MessageDelivery::Immediate,
                }),
            ),
            (
                LegacyAgentEvent::CompactionTriggered,
                AgentEvent::Queued(QueuedItem {
                    kind: QueuedItemKind::Compaction,
                    delivery: MessageDelivery::Immediate,
                }),
            ),
        ];
        for (legacy, expected) in cases {
            let mut buffer = bytes::BytesMut::new();
            legacy.encode(&mut buffer).expect("encode");
            let mut reader = buffer.freeze();
            let decoded = AgentEvent::decode(&mut reader).expect("decode");
            assert_eq!(decoded, expected);
        }
    }
}
