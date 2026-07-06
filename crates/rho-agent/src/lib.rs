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

pub mod claude;
pub mod db;
pub mod system_prompt;
pub mod title;

/// An agent timeline event. Some events fold into model context; future
/// runtime-only events, like tool output chunks, can live here without becoming
/// inference input.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum AgentEvent<'a> {
    UserMessage {
        content: Cow<'a, [ContentPart]>,
    },
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
}

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
    /// Messages waiting to enter model context. Not persisted: a queued
    /// message only becomes an `AgentEvent::UserMessage` at delivery, so the
    /// event log stays exactly what the model saw. Queued messages are lost
    /// if the process dies before delivery.
    pub queued_messages: Vec<QueuedUserMessage>,
    pub kind: AgentStateKind,
    /// Tokens occupying the model's context window after the latest
    /// response (all input, cached or not, plus that response's output).
    /// Restored on load from the event log (Rho runtime) or the session
    /// transcript (Claude runtime); `None` until the agent's first response
    /// reports usage.
    pub context_used: Option<u64>,
}

/// A user message waiting in the agent's queue.
#[derive(Clone, Debug, PartialEq)]
// content is Arc'd because the queue rides AgentState, which is cloned a lot
pub struct QueuedUserMessage {
    pub content: Arc<Vec<ContentPart>>,
    pub delivery: MessageDelivery,
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

fn restore_events(
    events: Vec<AgentEvent<'static>>,
) -> (Vec<Arc<ContextBlock>>, AgentStateKind, Option<u64>) {
    let mut blocks = Vec::new();
    let mut turn: Option<RestoreToolTurn> = None;
    let mut context_used = None;
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
            AgentEvent::UserMessage { content } => {
                commit_finished_turn(&mut turn, &mut blocks);
                blocks.push(Arc::new(ContextBlock::UserMessage {
                    content: content.into_owned(),
                }));
            }
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
    (blocks, kind, context_used)
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
        );
        write.commit();
        let inference_session = InferenceSession::new_deep(auth, config, prompt_cache_key);
        let shell_tools = ShellTools::new(
            std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            Arc::clone(&workspace),
        );
        let state = AgentState {
            blocks: Vec::new(),
            tool_specs: shell_tools.specs().into(),
            system_prompt: system_prompt::prompt(workspace.repo()),
            queued_messages: Vec::new(),
            kind: AgentStateKind::Idle,
            context_used: None,
        };
        let agent = Self::new(
            inference_session,
            shell_tools,
            Some(workspace),
            state,
            Some(AgentPersistence {
                db,
                agent_id,
                next_event,
            }),
        );
        Ok((agent_id, agent))
    }

    pub fn load(
        db: RhoDb,
        auth: InferenceAuth,
        agent_id: AgentId,
        workspace: Arc<Workspace>,
    ) -> Self {
        let record = db.read().get_agent(agent_id);
        let (next_event, events) = db.read().agent_events(agent_id);
        let (blocks, kind, context_used) = restore_events(events);
        let AgentRuntime::Rho { prompt_cache_key } = record.runtime else {
            panic!("cannot load Claude agent with the Rho agent runtime");
        };
        let config = record
            .mode
            .deep_config()
            .expect("Rho runtime stored with non-Rho agent mode");
        let inference_session = InferenceSession::new_deep(auth, config, prompt_cache_key);
        let shell_tools = ShellTools::new(
            std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            Arc::clone(&workspace),
        );
        let state = AgentState {
            blocks,
            tool_specs: shell_tools.specs().into(),
            system_prompt: system_prompt::prompt(workspace.repo()),
            queued_messages: Vec::new(),
            kind,
            context_used,
        };
        Self::new(
            inference_session,
            shell_tools,
            Some(workspace),
            state,
            Some(AgentPersistence {
                db,
                agent_id,
                next_event,
            }),
        )
    }

    fn new(
        inference_session: InferenceSession,
        shell_tools: ShellTools,
        workspace: Option<Arc<Workspace>>,
        state: AgentState,
        persistence: Option<AgentPersistence>,
    ) -> Self {
        let state = Arc::new(RwLock::new(state));
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
            persistence,
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
            content: vec![ContentPart::Text { text: text.into() }],
            delivery,
        });
    }

    /// Stop the current turn and drop all queued messages.
    pub fn cancel(&self) {
        let _ = self.control.send(AgentControl::Cancel);
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

struct AgentPersistence {
    db: RhoDb,
    agent_id: AgentId,
    next_event: AgentEventPos,
}

enum AgentControl {
    UserMessage {
        content: Vec<ContentPart>,
        delivery: MessageDelivery,
    },
    SetDeepConfig(DeepConfig),
    Rewind {
        turns: u32,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    Cancel,
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
    workspace: Option<Arc<Workspace>>,
    persistence: Option<AgentPersistence>,
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
    /// held — no automatic retry — until the user sends another message
    /// (drains everything) or cancels (drops everything).
    async fn run(mut self) {
        loop {
            let mut state = self.state.read().expect("poison").clone();

            tokio::select! {
                biased;
                control = self.control_rx.recv() => {
                    let Some(control) = control else {
                        return;
                    };
                    match control {
                        AgentControl::UserMessage { content, delivery } => {
                            state.queued_messages.push(QueuedUserMessage {
                                content: Arc::new(content),
                                delivery,
                            });
                            match &state.kind {
                                // Busy: the message waits in the queue for its
                                // delivery point.
                                AgentStateKind::ApiStreaming { .. }
                                | AgentStateKind::ToolCalling { .. } => {}
                                // No turn in flight: deliver everything now
                                // (including messages held through an Error).
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
                                        state
                                            .blocks
                                            .push(Arc::new(ContextBlock::ToolResults { results }));
                                    }
                                    self.deliver_queued(&mut state, MessageDelivery::NextTurn)
                                        .await;
                                    self.send_request(&state);
                                    state.kind = AgentStateKind::ApiStreaming {
                                        pending_response: PendingInferenceResponse::default(),
                                        previous_attempt: None,
                                    };
                                }
                            }
                        }
                        AgentControl::Cancel => {
                            self.inference_session.abort();
                            self.pending_tools.clear();
                            state.queued_messages.clear();

                            state.kind = AgentStateKind::Idle;
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
                            } else if !state.queued_messages.is_empty() {
                                Err(anyhow::anyhow!(
                                    ":rewind is not available with queued messages"
                                ))
                            } else if !self.pending_tools.is_empty()
                                || self.inference_session.has_active_request()
                            {
                                Err(anyhow::anyhow!(
                                    ":rewind is not available while work is running"
                                ))
                            } else if let Some(persistence) = self.persistence.as_ref() {
                                let db = persistence.db.clone();
                                let agent_id = persistence.agent_id;
                                let cursor = {
                                    let (_, records) = db.read().agent_event_records(agent_id);
                                    let user_positions = records
                                        .iter()
                                        .filter_map(|(pos, event)| {
                                            matches!(event, AgentEvent::UserMessage { .. })
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
                                        let (blocks, kind, context_used) = restore_events(events);
                                        state.blocks = blocks;
                                        state.kind = kind;
                                        state.context_used = context_used;
                                        state.queued_messages.clear();
                                        self.inference_session.abort();
                                        if let Some(persistence) = &mut self.persistence {
                                            persistence.next_event = next_event;
                                        }
                                        Ok(())
                                    }
                                }
                            } else {
                                Err(anyhow::anyhow!(":rewind requires a persisted agent"))
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
                                    // Turn complete: commit the checkout's
                                    // state so the user's jj view follows the
                                    // agent's work (fire-and-forget).
                                    if let Some(workspace) = &self.workspace {
                                        let workspace = Arc::clone(workspace);
                                        tokio::spawn(async move {
                                            if let Err(error) = workspace.snapshot().await {
                                                eprintln!("rho-agent: snapshot failed: {error:#}");
                                            }
                                        });
                                    }
                                    if state.queued_messages.is_empty() {
                                        state.kind = AgentStateKind::Idle;
                                    } else {
                                        self.deliver_queued(&mut state, MessageDelivery::NextTurn)
                                            .await;
                                        self.send_request(&state);
                                        state.kind = AgentStateKind::ApiStreaming {
                                            pending_response: PendingInferenceResponse::default(),
                                            previous_attempt: None,
                                        };
                                    }
                                } else {
                                    let mut previews = BTreeMap::new();
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
                                        let shell_tools = self.shell_tools.clone();
                                        self.pending_tools.push(Box::pin(async move {
                                            let call_id = call.id.clone();
                                            let tool_type = call.tool_type;
                                            let output = shell_tools.call_with_metadata(call).await;
                                            let finished_at = UnixMs::now();
                                            ToolResult {
                                                call_id,
                                                tool_type,
                                                body: output.body,
                                                started_at,
                                                finished_at,
                                                metadata: output.metadata,
                                            }
                                        }));
                                    }
                                    state.kind = AgentStateKind::ToolCalling {
                                        previews,
                                        results: Vec::new(),
                                    };
                                }
                            }
                        }},
                    }
                }
                Some(result) = self.pending_tools.next() => {
                    let AgentStateKind::ToolCalling {
                        mut previews,
                        mut results,
                    } = std::mem::replace(&mut state.kind, AgentStateKind::Idle) else {
                        unreachable!("tool finished outside ToolCalling");
                    };
                    previews.remove(&result.call_id);
                    results.push(result);
                    if self.pending_tools.is_empty() {
                        for result in &results {
                            self.persist_event(AgentEvent::ToolResult {
                                result: Cow::Borrowed(result),
                            })
                            .await;
                        }
                        state.blocks.push(Arc::new(ContextBlock::ToolResults { results }));
                        self.deliver_queued(&mut state, MessageDelivery::NextRequest).await;
                        self.send_request(&state);
                        state.kind = AgentStateKind::ApiStreaming {
                            pending_response: PendingInferenceResponse::default(),
                            previous_attempt: None,
                        };
                    } else {
                        state.kind = AgentStateKind::ToolCalling { previews, results };
                    }
                }
            }
            *self.state.write().expect("poison") = state.clone();
            self.notify.notify_waiters();
        }
    }

    async fn persist_event(&mut self, event: AgentEvent<'_>) {
        if let Some(persistence) = &mut self.persistence {
            let mut write = persistence.db.write().await;
            persistence.next_event = write.append_agent_event(persistence.next_event, &event);
            write.commit();
        }
    }

    /// Move queued messages into model context at a delivery boundary.
    /// `boundary` is the point the loop has reached: `NextRequest` (about to
    /// issue a mid-turn inference request) delivers only the steering lane;
    /// `NextTurn` (the turn is over) delivers both lanes. Relative order of
    /// delivered messages is preserved.
    async fn deliver_queued(&mut self, state: &mut AgentState, boundary: MessageDelivery) {
        let mut held = Vec::new();
        for message in std::mem::take(&mut state.queued_messages) {
            if boundary == MessageDelivery::NextTurn
                || message.delivery != MessageDelivery::NextTurn
            {
                self.persist_event(AgentEvent::UserMessage {
                    content: Cow::Borrowed(message.content.as_slice()),
                })
                .await;
                let content =
                    Arc::try_unwrap(message.content).unwrap_or_else(|content| (*content).clone());
                state
                    .blocks
                    .push(Arc::new(ContextBlock::UserMessage { content }));
            } else {
                held.push(message);
            }
        }
        state.queued_messages = held;
    }

    fn send_request(&mut self, state: &AgentState) {
        self.inference_session.request(InferenceRequest {
            instructions: state.system_prompt.clone(),
            input: state.blocks.clone(),
            tools: Arc::clone(&state.tool_specs),
        });
    }
}

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
