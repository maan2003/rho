//! Opinionated, forkable rho agent harness.
//!
//! This crate deliberately owns the state/inference/tool policy for one
//! simple workflow. Users who need different behavior should patch this code or
//! fork the crate while keeping the lower crates reusable.
//!
//! The agent runs as an in-process actor. A spawned loop owns the inference
//! session, tools, and store outright and drives turns. That loop is a single
//! `select!` whose arms — incoming commands, the inference session's `run`, and
//! tool completion — are the agent's whole event surface ("a distributed
//! `select!`"). Because the inference session's `run` is always one of those
//! arms, the connection stays warm whether or not a turn is in flight, with no
//! separate keepalive machinery.
//!
//! Callers hold a cheap [`Agent`] handle: they send work in over a command
//! channel and observe the conversation out through an append-only history
//! subscription plus an idle/running [`AgentStatus`] watch. Nothing reaches
//! into the loop's state; the shared, readable things are history and status,
//! both behind observables.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow};
use futures::future::{BoxFuture, FutureExt};
use futures::stream::{FuturesUnordered, StreamExt};
use rho_core::{
    IInferenceSession, InferenceUpdate, Item, ItemBlock, ItemKind, Message, MessagePhase, Role,
    ToolCall, ToolResult, ToolSpec,
};
use rho_store_cbor::CborLog;
use rho_store_redb::RedbLog;
use rho_tool_shell::ShellTools;
use tokio::sync::{broadcast, mpsc};

mod invariants;
mod observable;

use invariants::AgentInvariantsEnforcer;
use observable::Observable;

pub type ToolFuture = BoxFuture<'static, ToolResult>;

#[derive(Clone, Debug, PartialEq)]
pub enum AgentUpdate {
    ToolCallStarted(ToolCall),
    ToolCallFinished(ToolResult),
}

/// Whether the agent is between turns or driving one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    Idle,
    Running,
}

pub enum AgentTools {
    Shell(ShellTools),
}

#[derive(Clone, Debug)]
pub enum AgentStore {
    CborLog(CborLog),
    RedbLog(RedbLog),
}

/// Work sent from a handle into the agent loop.
enum Command {
    /// Add a user message and run a turn. Completion is observed through the
    /// status watch, not signalled back per-command.
    UserMessage { content: String },
    /// Interrupt the running turn, if any.
    Cancel,
}

/// A cheap, cloneable handle to a running agent.
///
/// Commands go in over a channel; the conversation comes out through
/// [`Agent::subscribe`] and turn boundaries through the [`AgentStatus`] watch.
/// The loop keeps running until every handle is dropped.
#[derive(Clone)]
pub struct Agent {
    commands: mpsc::UnboundedSender<Command>,
    invariants: AgentInvariantsEnforcer,
    status: Observable<AgentStatus, AgentStatus>,
    inference_updates: Observable<(), InferenceUpdate>,
    agent_updates: Observable<(), AgentUpdate>,
}

impl Agent {
    pub fn builder(inference: Box<dyn IInferenceSession>, tools: Vec<AgentTools>) -> AgentBuilder {
        AgentBuilder {
            inference,
            tools,
            store: None,
            store_blocks: None,
        }
    }

    /// The conversation so far.
    pub fn blocks(&self) -> Vec<ItemBlock> {
        self.invariants.snapshot()
    }

    /// Current conversation history plus a receiver for every block appended
    /// afterwards, taken atomically. Consumers (a UI) fold the stream to mirror
    /// the conversation without polling.
    pub fn subscribe(&self) -> (Vec<ItemBlock>, broadcast::Receiver<ItemBlock>) {
        self.invariants.subscribe()
    }

    pub fn subscribe_status(&self) -> (AgentStatus, broadcast::Receiver<AgentStatus>) {
        self.status.subscribe()
    }

    pub fn subscribe_inference_updates(&self) -> ((), broadcast::Receiver<InferenceUpdate>) {
        self.inference_updates.subscribe()
    }

    pub fn subscribe_agent_updates(&self) -> ((), broadcast::Receiver<AgentUpdate>) {
        self.agent_updates.subscribe()
    }

    /// Add a user message and start a turn. Returns immediately: observe
    /// progress through the history and status watches; errors surface in the
    /// conversation.
    pub fn send(&self, content: impl Into<String>) {
        let _ = self.commands.send(Command::UserMessage {
            content: content.into(),
        });
    }

    /// Ask the loop to interrupt the running turn.
    pub fn cancel(&self) {
        let _ = self.commands.send(Command::Cancel);
    }
}

pub struct AgentBuilder {
    inference: Box<dyn IInferenceSession>,
    tools: Vec<AgentTools>,
    store: Option<AgentStore>,
    store_blocks: Option<Vec<ItemBlock>>,
}

impl AgentBuilder {
    pub fn with_store(mut self, store: AgentStore) -> Self {
        self.store = Some(store);
        self
    }

    /// Load existing history from `store` and keep writing to it.
    pub async fn with_store_loaded(mut self, store: AgentStore) -> Result<Self> {
        self.store_blocks = Some(store.read_blocks().await?);
        self.store = Some(store);
        Ok(self)
    }

    /// Spawn the agent loop and return a handle to it.
    pub fn spawn(self) -> Agent {
        let invariants = AgentInvariantsEnforcer::new(
            tool_specs(&self.tools),
            self.store_blocks.unwrap_or_default(),
        );
        let status = Observable::new(AgentStatus::Idle);
        let inference_updates = Observable::new(());
        let agent_updates = Observable::new(());
        let (commands, command_rx) = mpsc::unbounded_channel();
        let loop_state = AgentLoop {
            inference: self.inference,
            tools: self.tools,
            store: self.store,
            invariants: invariants.clone(),
            status: status.clone(),
            inference_updates: inference_updates.clone(),
            agent_updates: agent_updates.clone(),
            active: None,
        };
        tokio::spawn(run(loop_state, command_rx));
        Agent {
            commands,
            invariants,
            status,
            inference_updates,
            agent_updates,
        }
    }
}

/// The loop-owned half of the agent: it owns the inference session, tools, and
/// store, and is the only thing that mutates history.
struct AgentLoop {
    inference: Box<dyn IInferenceSession>,
    tools: Vec<AgentTools>,
    store: Option<AgentStore>,
    invariants: AgentInvariantsEnforcer,
    status: Observable<AgentStatus, AgentStatus>,
    inference_updates: Observable<(), InferenceUpdate>,
    agent_updates: Observable<(), AgentUpdate>,
    active: Option<ActiveTurn>,
}

/// State for the turn currently in flight.
struct ActiveTurn {
    /// The tools requested by the last response, running concurrently. Empty
    /// while waiting on inference rather than tools.
    tools: FuturesUnordered<ToolFuture>,
}

/// The agent's event loop: one `select!` over incoming commands, inference
/// updates, and tool completion. The inference `run` arm is always present, so
/// the connection is kept warm whether the agent is streaming a response,
/// running tools, or idle.
async fn run(mut agent: AgentLoop, mut commands: mpsc::UnboundedReceiver<Command>) {
    loop {
        let tools_running = agent.tools_running();
        tokio::select! {
            // Prefer draining inference progress before handling a command, so a
            // cancel always sees a consistent, fully-recorded turn state. This
            // can't starve commands: between network events `run` pends, which
            // lets the command arm fire.
            biased;
            update = agent.inference.run() => {
                let update = match update {
                    Ok(update) => update,
                    Err(error) => {
                        agent.finish_with_error(error).await;
                        continue;
                    }
                };
                if agent.active.is_none() {
                    continue;
                }
                agent.inference_updates.update(|()| update.clone());

                let InferenceUpdate::Finished(response) = update else {
                    continue;
                };

                let provider_response_id = response.provider_response_id.clone();
                let tool_calls = response
                    .items
                    .iter()
                    .filter_map(|item| match item {
                        ItemKind::ToolCall(call) => Some(call.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                let items = response
                    .items
                    .into_iter()
                    .map(|kind| Item {
                        id: alloc_item_id(),
                        kind,
                    })
                    .collect::<Vec<_>>();
                agent.record_block(ItemBlock::InferenceResponse {
                    provider_response_id,
                    items,
                }).await;
                if tool_calls.is_empty() {
                    agent.finish_turn();
                    continue;
                }
                match agent.tool_futures(tool_calls) {
                    Ok(futures) => {
                        agent.active.as_mut().unwrap().tools = futures;
                    }
                    Err(error) => agent.finish_with_error(error).await,
                }
            },
            command = commands.recv() => match command {
                Some(Command::UserMessage { content }) => {
                    // A handle gates concurrent sends; ignore a stray one rather than
                    // disturbing the running turn.
                    if agent.active.is_some() {
                        return;
                    }
                    agent.status.set(AgentStatus::Running);
                    agent.append_user_message(content).await;
                    agent.inference.request(agent.invariants.inference_request());
                    agent.active = Some(ActiveTurn { tools: FuturesUnordered::new() });
                }
                Some(Command::Cancel) => {
                    if agent.active.is_some() {
                        agent.inference.abort();
                        agent.cancel().await;
                        agent.finish_turn();
                    }
                }
                None => break,
            },
            result = async {
                agent.active.as_mut().unwrap().tools.next().await
            }, if tools_running => {
                let Some(result) = result else {
                    continue;
                };
                let block = agent.tool_results_to_block(vec![result]);
                agent.record_block(block).await;
                if agent.active.as_ref().is_some_and(|active| active.tools.is_empty()) {
                    agent.inference.request(agent.invariants.inference_request());
                }
            }
        }
    }
}

impl AgentLoop {
    fn tools_running(&self) -> bool {
        self.active
            .as_ref()
            .is_some_and(|active| !active.tools.is_empty())
    }

    fn finish_turn(&mut self) {
        if self.active.take().is_some() {
            self.status.set(AgentStatus::Idle);
        }
    }

    /// End the turn after a fatal inference/tool error by recording it as a
    /// final assistant message — the same shape as a cancellation, so the error
    /// is visible in the conversation rather than swallowed.
    async fn finish_with_error(&mut self, error: anyhow::Error) {
        if self.active.is_some() {
            self.record_block(ItemBlock::Local {
                items: vec![Item {
                    id: alloc_item_id(),
                    kind: ItemKind::Message(
                        // review: we can't add Assistant role messages ourselves! this must be an
                        // invariant
                        Message::text(Role::Assistant, error.to_string())
                            .with_phase(MessagePhase::FinalAnswer),
                    ),
                }],
            })
            .await;
            self.finish_turn();
        }
    }

    async fn append_user_message(&mut self, content: String) {
        let block = ItemBlock::Local {
            items: vec![Item {
                id: alloc_item_id(),
                kind: ItemKind::Message(Message::text(Role::User, content)),
            }],
        };
        self.record_block(block).await;
    }

    fn tool_futures(&self, tool_calls: Vec<ToolCall>) -> Result<FuturesUnordered<ToolFuture>> {
        tool_calls
            .into_iter()
            .map(|call| {
                let future = self
                    .tools
                    .iter()
                    .find(|tool| tool.supports(&call.name))
                    .ok_or_else(|| anyhow!("no tool registered for {}", call.name))
                    .map(|tool| tool.call(call.clone()))?;
                let agent_updates = self.agent_updates.clone();
                Ok(async move {
                    agent_updates.update(|()| AgentUpdate::ToolCallStarted(call));
                    let result = future.await;
                    agent_updates.update(|()| AgentUpdate::ToolCallFinished(result.clone()));
                    result
                }
                .boxed())
            })
            .collect()
    }

    /// Record cancelled results for any tool calls still awaiting them.
    async fn cancel(&mut self) {
        let pending = unanswered_tool_calls(&self.invariants.snapshot());
        if !pending.is_empty() {
            let results = pending
                .into_iter()
                .map(|call| {
                    let mut result = ToolResult::cancelled(call.id, "cancelled");
                    result.tool_type = call.tool_type;
                    result
                })
                .collect::<Vec<_>>();
            let block = self.tool_results_to_block(results);
            self.record_block(block).await;
        }
    }

    async fn record_block(&mut self, block: ItemBlock) {
        if let Some(store) = &self.store {
            store.append_block(&block).await;
        }
        self.invariants.append_block(block);
    }

    fn tool_results_to_block(&self, results: Vec<ToolResult>) -> ItemBlock {
        let items = results
            .into_iter()
            .map(|result| Item {
                id: alloc_item_id(),
                kind: ItemKind::ToolResult(result),
            })
            .collect();
        ItemBlock::Local { items }
    }
}

/// Tool calls in `blocks` that have no matching tool result yet.
fn unanswered_tool_calls(blocks: &[ItemBlock]) -> Vec<ToolCall> {
    let mut answered = HashSet::new();
    for item in block_items(blocks) {
        if let ItemKind::ToolResult(result) = &item.kind {
            answered.insert(result.call_id.clone());
        }
    }
    block_items(blocks)
        .filter_map(|item| match &item.kind {
            ItemKind::ToolCall(call) if !answered.contains(&call.id) => Some(call.clone()),
            _ => None,
        })
        .collect()
}

fn block_items(blocks: &[ItemBlock]) -> impl Iterator<Item = &Item> {
    blocks.iter().flat_map(|block| match block {
        ItemBlock::Local { items } | ItemBlock::InferenceResponse { items, .. } => items,
    })
}

static NEXT_ITEM_ID: std::sync::OnceLock<AtomicU64> = std::sync::OnceLock::new();

fn alloc_item_id() -> rho_core::ItemId {
    let counter = NEXT_ITEM_ID.get_or_init(|| {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64;
        AtomicU64::new(seed)
    });
    rho_core::ItemId(format!("item-{}", counter.fetch_add(1, Ordering::Relaxed)))
}

fn tool_specs(tools: &[AgentTools]) -> Vec<ToolSpec> {
    tools.iter().flat_map(AgentTools::specs).collect()
}

impl AgentStore {
    /// Append a block, panicking on a store failure: a write failure here means
    /// the on-disk transcript is broken, which the agent cannot meaningfully
    /// recover from mid-turn.
    async fn append_block(&self, block: &ItemBlock) {
        let result = match self {
            AgentStore::CborLog(log) => log.append_block(block).await,
            AgentStore::RedbLog(log) => log.append_block(block).await,
        };
        result.expect("agent store append failed");
    }

    async fn read_blocks(&self) -> Result<Vec<ItemBlock>> {
        match self {
            AgentStore::CborLog(log) => log.read_blocks().await,
            AgentStore::RedbLog(log) => log.read_blocks().await,
        }
    }
}

impl AgentTools {
    fn specs(&self) -> Vec<rho_core::ToolSpec> {
        match self {
            AgentTools::Shell(tool) => tool.specs(),
        }
    }

    fn supports(&self, name: &str) -> bool {
        match self {
            AgentTools::Shell(tool) => tool.supports(name),
        }
    }

    fn call(&self, call: rho_core::ToolCall) -> ToolFuture {
        match self {
            AgentTools::Shell(tool) => {
                let tool = tool.clone();
                async move { tool.call(call).await }.boxed()
            }
        }
    }
}

#[cfg(test)]
mod tests;
