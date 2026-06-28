use std::num::NonZeroU64;
use std::sync::{Arc, RwLock};

use async_stream::stream;
use futures::future::BoxFuture;
use futures::stream::FuturesUnordered;
use futures::{Stream, StreamExt};
use rho_core::{
    ContentPart, ContextBlock, InferenceEvent, InferenceRequest, InferenceResponseItem,
    PendingInferenceResponse, ToolCall, ToolResult, ToolSpec,
};
use rho_db::RhoDb;
use rho_inference::config::InferenceConfig;
use rho_inference::{InferenceAuth, InferenceSession, PromptCacheKey};
use rho_tool_shell::{DEFAULT_TIMEOUT_SECS, ShellTools};
use tokio::sync::{Notify, mpsc};

use crate::db::{AgentId, AgentReadTxnExt, AgentTimelineRef, AgentWriteTxnExt, UnixMillis};

pub mod db;

/// Live runtime state of an agent turn.
#[derive(Clone)]
pub struct AgentState {
    /// Invariant: append-only. Blocks are only ever pushed — never removed,
    /// replaced, or reordered.
    pub blocks: Vec<Arc<ContextBlock>>,
    /// Invariant: immutable. Set once at construction and never changed for the
    /// life of the agent. Enforced by exposing no mutator.
    pub tool_specs: Arc<[ToolSpec]>,
    pub kind: AgentStateKind,
}

#[derive(Clone)]
pub enum AgentStateKind {
    ApiStreaming {
        pending_response: PendingInferenceResponse,
        previous_attempt: Option<FailedInferenceResponse>,
    },
    // we are now calling tools now
    // Note: in future we might add ToolCallingWhileStreaming state for proactive execution while
    // streaming
    ToolCalling {
        // Results of the calls that have finished so far. The still-running ones
        // live in `Agent::pending_tools`; the invariant is that it is non-empty
        // while in this state — once the last future resolves we transition.
        // communication of tool calls is done out of band! tools maybe even persist the updates in
        // db for example
        results: Vec<ToolResult>,
    },
    // Permanent error, thread is paused
    Error(FailedInferenceResponse),
    Idle,
}

#[derive(Clone)]
pub struct FailedInferenceResponse {
    pub partial_response: PendingInferenceResponse,
    pub attempt_count: NonZeroU64,
    pub error: Arc<anyhow::Error>,
}

/// Cheap handle for observing and controlling the agent loop.
#[derive(Clone)]
pub struct Agent {
    state: Arc<RwLock<AgentState>>,
    control: mpsc::UnboundedSender<AgentControl>,
    notify: Arc<Notify>,
}

impl Agent {
    pub fn create_ephemeral(
        auth: InferenceAuth,
        config: InferenceConfig,
        blocks: Vec<Arc<ContextBlock>>,
    ) -> Self {
        let inference_session =
            InferenceSession::new(auth, config.protect(), PromptCacheKey::generate());
        Self::new(inference_session, blocks, None)
    }

    pub async fn create(
        db: RhoDb,
        auth: InferenceAuth,
        config: InferenceConfig,
        display_name: Option<String>,
    ) -> Self {
        let prompt_cache_key = PromptCacheKey::generate();
        let config = config.protect();
        let mut write = db.write().await;
        let (_, next_block) = write.create_agent(
            UnixMillis::now(),
            display_name,
            prompt_cache_key,
            config.clone(),
        );
        write.commit();
        let inference_session = InferenceSession::new(auth, config, prompt_cache_key);
        Self::new(
            inference_session,
            Vec::new(),
            Some(AgentPersistence { db, next_block }),
        )
    }

    pub fn load(db: RhoDb, auth: InferenceAuth, agent_id: AgentId) -> Self {
        let record = db.read().get_agent(agent_id);
        let (next_block, blocks) = db.read().agent_blocks(agent_id);
        let inference_session = InferenceSession::new(auth, record.config, record.prompt_cache_key);
        Self::new(
            inference_session,
            blocks,
            Some(AgentPersistence { db, next_block }),
        )
    }

    fn new(
        inference_session: InferenceSession,
        blocks: Vec<Arc<ContextBlock>>,
        persistence: Option<AgentPersistence>,
    ) -> Self {
        let shell_tools = ShellTools::new(std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        let tool_specs: Arc<[ToolSpec]> = shell_tools.specs().into();
        let state = Arc::new(RwLock::new(AgentState {
            blocks,
            tool_specs,
            kind: AgentStateKind::Idle,
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
    next_block: AgentTimelineRef,
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
            let mut append_block =
                async |blocks: &mut Vec<Arc<ContextBlock>>, block: Arc<ContextBlock>| {
                    if let Some(persistence) = &mut self.persistence {
                        let mut write = persistence.db.write().await;
                        persistence.next_block = write.append_agent_block(
                            persistence.next_block,
                            UnixMillis::now(),
                            block.clone(),
                        );
                        write.commit();
                    }
                    blocks.push(block);
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

                            let block = Arc::new(ContextBlock::UserMessage { content });
                            append_block(&mut state.blocks, block).await;
                            self.inference_session.request(InferenceRequest {
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
                                    error,
                                }),
                            };
                        }
                        InferenceEvent::Failed { error } => {
                            let attempt_count = previous_attempt
                                .map_or(NonZeroU64::MIN, |a| a.attempt_count.saturating_add(1));
                            state.kind = AgentStateKind::Error(FailedInferenceResponse {
                                partial_response: pending_response,
                                attempt_count,
                                error,
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
                                    error: Arc::new(error),
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
                                let block = Arc::new(ContextBlock::InferenceResponse {
                                    items,
                                    provider_response_id,
                                });
                                append_block(&mut state.blocks, block).await;
                                if calls.is_empty() {
                                    state.kind = AgentStateKind::Idle;
                                } else {
                                    for call in calls {
                                        let shell_tools = self.shell_tools.clone();
                                        self.pending_tools.push(Box::pin(async move {
                                            let call_id = call.id.clone();
                                            let tool_type = call.tool_type;
                                            let body = shell_tools.call(call).await;
                                            ToolResult { call_id, tool_type, body }
                                        }));
                                    }
                                    state.kind = AgentStateKind::ToolCalling {
                                        results: Vec::new(),
                                    };
                                }
                            }
                        },
                    }
                }
                Some(result) = self.pending_tools.next() => {
                    let AgentStateKind::ToolCalling { mut results } = state.kind else {
                        unreachable!("tool finished outside ToolCalling");
                    };
                    results.push(result);
                    if self.pending_tools.is_empty() {
                        let block = Arc::new(ContextBlock::ToolResults { results });
                        append_block(&mut state.blocks, block).await;
                        self.inference_session.request(InferenceRequest {
                            input: state.blocks.clone(),
                            tools: Arc::clone(&state.tool_specs),
                        });
                        state.kind = AgentStateKind::ApiStreaming {
                            pending_response: PendingInferenceResponse::default(),
                            previous_attempt: None,
                        };
                    } else {
                        state.kind = AgentStateKind::ToolCalling { results };
                    }
                }
            }
            *self.state.write().expect("poison") = state.clone();
            self.notify.notify_waiters();
        }
    }
}
