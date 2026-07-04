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
    ToolResult, ToolResultMetadata, ToolSpec, UnixMs,
};
use rho_db::RhoDb;
use rho_inference::{InferenceAuth, InferenceSession, PromptCacheKey};
use rho_tool_shell::{DEFAULT_TIMEOUT_SECS, ShellTools};
use rho_workspaces::{Repo, Workspace};
use senax_encoder::{Decode, Encode};
use tokio::sync::{Notify, mpsc};

use crate::db::{
    AgentEventPos, AgentId, AgentMode, AgentReadTxnExt, AgentRuntime, AgentWriteTxnExt, UnixMillis,
};

pub mod claude;
pub mod db;
pub mod system_prompt;

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
    },
    ToolResult {
        result: Cow<'a, ToolResult>,
    },
}

/// Live runtime state of an agent turn.
#[derive(Clone, Debug, PartialEq)]
// should be cheap to clone, it is cloned a lot
pub struct AgentState {
    /// Invariant: append-only. Blocks are only ever pushed — never removed,
    /// replaced, or reordered.
    pub blocks: Vec<Arc<ContextBlock>>,
    /// Invariant: immutable. Set once at construction and never changed for the
    /// life of the agent. Enforced by exposing no mutator.
    pub tool_specs: Arc<[ToolSpec]>,
    /// Invariant: immutable
    pub system_prompt: Arc<str>,
    pub kind: AgentStateKind,
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
            .inference_config()
            .ok_or_else(|| anyhow::anyhow!("cannot create Rho runtime for Claude agent mode"))?;
        let config = config.protect();
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
        let inference_session = InferenceSession::new(auth, config, prompt_cache_key);
        let shell_tools = ShellTools::new(
            std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            Arc::clone(&workspace),
        );
        let state = AgentState {
            blocks: Vec::new(),
            tool_specs: shell_tools.specs().into(),
            system_prompt: system_prompt::prompt(workspace.repo()),
            kind: AgentStateKind::Idle,
        };
        let agent = Self::new(
            inference_session,
            shell_tools,
            Some(workspace),
            state,
            Some(AgentPersistence { db, next_event }),
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
        struct RestoreToolTurn {
            outstanding_calls: Vec<ToolCall>,
            completed_tool_calls: Vec<ToolResult>,
        }

        let (next_event, events) = db.read().agent_events(agent_id);
        let mut blocks = Vec::new();
        let mut turn: Option<RestoreToolTurn> = None;
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
                } => {
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
        let AgentRuntime::Rho { prompt_cache_key } = record.runtime else {
            panic!("cannot load Claude agent with the Rho agent runtime");
        };
        let config = record
            .mode
            .inference_config()
            .expect("Rho runtime stored with non-Rho agent mode")
            .protect();
        let inference_session = InferenceSession::new(auth, config, prompt_cache_key);
        let shell_tools = ShellTools::new(
            std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            Arc::clone(&workspace),
        );
        let state = AgentState {
            blocks,
            tool_specs: shell_tools.specs().into(),
            system_prompt: system_prompt::prompt(workspace.repo()),
            kind,
        };
        Self::new(
            inference_session,
            shell_tools,
            Some(workspace),
            state,
            Some(AgentPersistence { db, next_event }),
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

    pub fn send_user_message(&self, text: impl Into<String>) {
        let _ = self.control.send(AgentControl::UserMessage {
            content: vec![ContentPart::Text { text: text.into() }],
        });
    }

    pub fn cancel(&self) {
        let _ = self.control.send(AgentControl::Cancel);
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
    next_event: AgentEventPos,
}

enum AgentControl {
    UserMessage { content: Vec<ContentPart> },
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
    async fn run(mut self) {
        loop {
            let mut state = self.state.read().expect("poison").clone();
            let mut persist_event = async |event: AgentEvent<'_>| {
                if let Some(persistence) = &mut self.persistence {
                    let mut write = persistence.db.write().await;
                    persistence.next_event =
                        write.append_agent_event(persistence.next_event, &event);
                    write.commit();
                }
            };

            tokio::select! {
                biased;
                control = self.control_rx.recv() => {
                    let Some(control) = control else {
                        return;
                    };
                    match control {
                        AgentControl::UserMessage { content } => {
                            self.inference_session.abort();
                            self.pending_tools.clear();

                            persist_event(AgentEvent::UserMessage {
                                content: Cow::Borrowed(&content),
                            })
                            .await;
                            state
                                .blocks
                                .push(Arc::new(ContextBlock::UserMessage { content }));
                            self.inference_session.request(InferenceRequest {
                                instructions: state.system_prompt.clone(),
                                input: state.blocks.clone(),
                                tools: Arc::clone(&state.tool_specs),
                            });
                            state.kind = AgentStateKind::ApiStreaming {
                                pending_response: PendingInferenceResponse::default(),
                                previous_attempt: None,
                            };
                        }
                        AgentControl::Cancel => {
                            self.inference_session.abort();
                            self.pending_tools.clear();

                            state.kind = AgentStateKind::Idle;
                        }
                    }
                }
                update = self.inference_session.run() => {
                    let AgentStateKind::ApiStreaming {
                        mut pending_response,
                        previous_attempt,
                    } = state.kind
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
                            usage: _,
                            provider_response_id,
                        } => match pending_response.finish() {
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
                                persist_event(AgentEvent::InferenceResponse {
                                    items: Cow::Borrowed(&items),
                                    provider_response_id: provider_response_id.clone(),
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
                                    state.kind = AgentStateKind::Idle;
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
                        },
                    }
                }
                Some(result) = self.pending_tools.next() => {
                    let AgentStateKind::ToolCalling {
                        mut previews,
                        mut results,
                    } = state.kind else {
                        unreachable!("tool finished outside ToolCalling");
                    };
                    previews.remove(&result.call_id);
                    results.push(result);
                    if self.pending_tools.is_empty() {
                        for result in &results {
                            persist_event(AgentEvent::ToolResult {
                                result: Cow::Borrowed(result),
                            })
                            .await;
                        }
                        state.blocks.push(Arc::new(ContextBlock::ToolResults { results }));
                        self.inference_session.request(InferenceRequest {
                            instructions: state.system_prompt.clone(),
                            input: state.blocks.clone(),
                            tools: Arc::clone(&state.tool_specs),
                        });
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
}

fn tool_preview_metadata(metadata: ToolResultMetadata) -> ToolPreviewMetadata {
    match metadata {
        ToolResultMetadata::ApplyPatch(metadata) => ToolPreviewMetadata::ApplyPatch(metadata),
    }
}
