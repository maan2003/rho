//! Opinionated, forkable rho agent harness.
//!
//! This crate deliberately owns the state/inference/tool policy for one
//! simple workflow. Users who need different behavior should patch this code or
//! fork the crate while keeping the lower crates reusable.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, anyhow, bail};
use futures::StreamExt;
use futures::future::{BoxFuture, FutureExt, join_all};
use rho::{
    InferenceRequest, InferenceResponse, Item, ItemBlock, ItemKind, Message, MessagePhase,
    ReasoningText, ReasoningTextKind, Role, ToolCall, ToolResult, ToolSpec,
};
use rho_inference_responses::{InferenceService, ResponsesStream, ResponsesUpdate};
use rho_store_cbor::CborLog;
use rho_store_redb::RedbLog;
use rho_tool_shell::ShellTools;

mod thread;

use thread::AgentThread;

pub type InferenceStream = ResponsesStream;
pub type ToolFuture = BoxFuture<'static, ToolResult>;
type InferenceUpdateHandler = Box<dyn FnMut(ResponsesUpdate) + Send>;
type AgentUpdateHandler = Arc<Mutex<Box<dyn FnMut(AgentUpdate) + Send>>>;

#[derive(Clone, Debug, PartialEq)]
pub enum AgentUpdate {
    ToolCallStarted(ToolCall),
    ToolCallFinished(ToolResult),
}

pub enum AgentInference {
    Responses(InferenceService),
    #[cfg(test)]
    Test {
        stream: Arc<dyn Fn(InferenceRequest) -> InferenceStream + Send + Sync>,
    },
}

pub enum AgentTools {
    Shell(ShellTools),
}

#[derive(Clone, Debug)]
pub enum AgentStore {
    CborLog(CborLog),
    RedbLog(RedbLog),
}

#[derive(Default)]
pub enum AgentState {
    #[default]
    Idle,
    ApiRequest {
        streamed_transcript: StreamingTranscript,
        stream: InferenceStream,
    },
    WaitingForTools {
        futures: Vec<PendingToolCall>,
        results: Vec<ToolResult>,
    },
}

pub struct PendingToolCall {
    pub call: ToolCall,
    pub future: ToolFuture,
}

#[derive(Debug)]
enum QueueItem {
    UserMessage(Message),
}

pub struct Agent {
    inference: AgentInference,
    tools: Vec<AgentTools>,
    store: Option<AgentStore>,
    thread: AgentThread,
    queue: VecDeque<QueueItem>,
    pending_tool_results: Vec<ToolResult>,
    inference_updates: Option<InferenceUpdateHandler>,
    agent_updates: Option<AgentUpdateHandler>,
    pub state: AgentState,
}

#[doc(hidden)]
#[derive(Default)]
pub struct StreamingTranscript {
    output_items: BTreeMap<usize, ItemKind>,
    text_by_output_index: BTreeMap<usize, String>,
    reasoning_by_output_index: BTreeMap<(usize, ReasoningTextKind), String>,
}

impl Agent {
    pub fn new(inference: AgentInference, tools: Vec<AgentTools>) -> Self {
        Self::from_blocks(inference, tools, Vec::new())
    }

    pub fn with_store(mut self, store: AgentStore) -> Self {
        self.store = Some(store);
        self
    }

    pub fn with_inference_updates(
        mut self,
        on_update: impl FnMut(ResponsesUpdate) + Send + 'static,
    ) -> Self {
        self.inference_updates = Some(Box::new(on_update));
        self
    }

    pub fn with_agent_updates(
        mut self,
        on_update: impl FnMut(AgentUpdate) + Send + 'static,
    ) -> Self {
        self.agent_updates = Some(Arc::new(Mutex::new(Box::new(on_update))));
        self
    }

    pub async fn from_store(
        inference: AgentInference,
        tools: Vec<AgentTools>,
        store: AgentStore,
    ) -> Result<Self> {
        let blocks = store.read_blocks().await?;
        Ok(Self::from_blocks(inference, tools, blocks).with_store(store))
    }

    pub fn blocks(&self) -> &[ItemBlock] {
        self.thread.blocks()
    }

    pub fn is_idle(&self) -> bool {
        matches!(self.state, AgentState::Idle)
            && self.queue.is_empty()
            && self.pending_tool_results.is_empty()
    }

    pub fn push_user_message(&mut self, content: impl Into<String>) {
        self.queue
            .push_back(QueueItem::UserMessage(Message::text(Role::User, content)));
    }

    pub async fn cancel_current_turn(&mut self, reason: impl Into<String>) -> Result<()> {
        let reason = reason.into();
        let state = std::mem::take(&mut self.state);
        if let AgentState::WaitingForTools { futures, .. } = state {
            let results = futures
                .into_iter()
                .map(|pending| {
                    let mut result = ToolResult::cancelled(pending.call.id, reason.clone());
                    result.tool_type = pending.call.tool_type;
                    result
                })
                .collect::<Vec<_>>();
            if !results.is_empty() {
                let block = self.tool_results_to_block(results);
                self.record_block(block).await?;
            }
        }
        self.queue.clear();
        self.pending_tool_results.clear();
        let id = alloc_item_id();
        self.record_block(ItemBlock::Local {
            items: vec![Item {
                id,
                kind: ItemKind::Message(
                    Message::text(Role::Assistant, reason).with_phase(MessagePhase::FinalAnswer),
                ),
            }],
        })
        .await
    }

    pub async fn run_until_idle(&mut self, max_steps: usize) -> Result<usize> {
        for steps in 0..max_steps {
            if self.is_idle() {
                return Ok(steps);
            }
            self.step().await?;
        }

        if self.is_idle() {
            Ok(max_steps)
        } else {
            bail!("agent did not become idle within {max_steps} steps")
        }
    }

    pub async fn step(&mut self) -> Result<()> {
        let state = std::mem::take(&mut self.state);

        self.state = match state {
            AgentState::Idle => self.start_next_request().await?,
            AgentState::ApiRequest {
                mut streamed_transcript,
                mut stream,
            } => loop {
                match stream.next().await {
                    Some(Ok(update)) => {
                        streamed_transcript.record(&update);
                        notify_inference_update(&mut self.inference_updates, update.clone());
                        if let ResponsesUpdate::Finished(response) = update {
                            break self
                                .finish_inference_request(response, streamed_transcript)
                                .await?;
                        }
                    }
                    Some(Err(error)) => return Err(error),
                    None => bail!("inference stream ended before final response"),
                }
            },
            AgentState::WaitingForTools {
                futures,
                mut results,
            } => {
                results.extend(join_all(futures.into_iter().map(|pending| pending.future)).await);
                self.push_tool_results(results);
                self.start_next_request().await?
            }
        };

        Ok(())
    }

    async fn start_next_request(&mut self) -> Result<AgentState> {
        let mut recorded_local_work = false;

        if !self.pending_tool_results.is_empty() {
            let results = std::mem::take(&mut self.pending_tool_results);
            let block = self.tool_results_to_block(results);
            self.record_block(block).await?;
            recorded_local_work = true;
        }

        while let Some(item) = self.queue.pop_front() {
            let block = self.queue_item_to_block(item);
            self.record_block(block).await?;
            recorded_local_work = true;
        }

        if !recorded_local_work || self.thread.blocks().is_empty() {
            return Ok(AgentState::Idle);
        }

        let streamed_transcript = StreamingTranscript::default();
        Ok(AgentState::ApiRequest {
            stream: self.inference.stream(self.thread.inference_request()),
            streamed_transcript,
        })
    }

    async fn finish_inference_request(
        &mut self,
        mut response: InferenceResponse,
        streamed_transcript: StreamingTranscript,
    ) -> Result<AgentState> {
        streamed_transcript.supplement_response(&mut response);
        let provider_response_id = response.provider_response_id.clone();

        let tool_calls = response
            .items
            .iter()
            .filter_map(|item| match item {
                ItemKind::ToolCall(call) => Some(call.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();

        let response_items = response
            .items
            .into_iter()
            .map(|kind| {
                let id = alloc_item_id();
                Item { id, kind }
            })
            .collect::<Vec<_>>();
        self.record_provider_response_block(response_items, provider_response_id.clone())
            .await?;

        let futures = tool_calls
            .into_iter()
            .map(|call| {
                let future = self
                    .tools
                    .iter()
                    .find(|tool| tool.supports(&call.name))
                    .ok_or_else(|| anyhow!("no tool registered for {}", call.name))
                    .map(|tool| tool.call(call.clone()))?;
                let agent_updates = self.agent_updates.clone();
                let pending_call = call.clone();
                let future = async move {
                    notify_agent_update(&agent_updates, AgentUpdate::ToolCallStarted(call));
                    let result = future.await;
                    notify_agent_update(
                        &agent_updates,
                        AgentUpdate::ToolCallFinished(result.clone()),
                    );
                    result
                }
                .boxed();
                Ok(PendingToolCall {
                    call: pending_call,
                    future,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        if futures.is_empty() {
            Ok(AgentState::Idle)
        } else {
            Ok(AgentState::WaitingForTools {
                futures,
                results: Vec::new(),
            })
        }
    }

    fn push_tool_results(&mut self, results: Vec<ToolResult>) {
        self.pending_tool_results.extend(results);
    }

    async fn record_provider_response_block(
        &mut self,
        items: Vec<Item>,
        provider_response_id: Option<String>,
    ) -> Result<()> {
        self.record_block(ItemBlock::InferenceResponse {
            provider_response_id,
            items,
        })
        .await
    }

    async fn record_block(&mut self, block: ItemBlock) -> Result<()> {
        if let Some(store) = &self.store {
            store.append_block(&block).await?;
        }
        self.thread.append_block(block);
        Ok(())
    }

    fn queue_item_to_block(&mut self, item: QueueItem) -> ItemBlock {
        match item {
            QueueItem::UserMessage(message) => {
                let id = alloc_item_id();
                ItemBlock::Local {
                    items: vec![Item {
                        id,
                        kind: ItemKind::Message(message),
                    }],
                }
            }
        }
    }

    fn tool_results_to_block(&mut self, results: Vec<ToolResult>) -> ItemBlock {
        let items = results
            .into_iter()
            .map(|result| {
                let id = alloc_item_id();
                Item {
                    id,
                    kind: ItemKind::ToolResult(result),
                }
            })
            .collect();
        ItemBlock::Local { items }
    }
}

impl Agent {
    fn from_blocks(
        inference: AgentInference,
        tools: Vec<AgentTools>,
        blocks: Vec<ItemBlock>,
    ) -> Self {
        let thread = AgentThread::new(tool_specs(&tools), blocks);
        Self {
            inference,
            tools,
            store: None,
            thread,
            queue: VecDeque::new(),
            pending_tool_results: Vec::new(),
            inference_updates: None,
            agent_updates: None,
            state: AgentState::Idle,
        }
    }
}

static NEXT_ITEM_ID: std::sync::OnceLock<AtomicU64> = std::sync::OnceLock::new();

fn alloc_item_id() -> rho::ItemId {
    let counter = NEXT_ITEM_ID.get_or_init(|| {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .min(u128::from(u64::MAX)) as u64;
        AtomicU64::new(seed)
    });
    rho::ItemId(format!("item-{}", counter.fetch_add(1, Ordering::Relaxed)))
}

fn tool_specs(tools: &[AgentTools]) -> Vec<ToolSpec> {
    tools.iter().flat_map(AgentTools::specs).collect()
}

fn notify_agent_update(handler: &Option<AgentUpdateHandler>, update: AgentUpdate) {
    if let Some(handler) = handler {
        let mut on_update = handler.lock().expect("agent update lock");
        on_update(update);
    }
}

fn notify_inference_update(handler: &mut Option<InferenceUpdateHandler>, update: ResponsesUpdate) {
    if let Some(handler) = handler {
        handler(update);
    }
}

impl StreamingTranscript {
    fn record(&mut self, update: &ResponsesUpdate) {
        match update {
            ResponsesUpdate::TextDelta { output_index, text } => {
                self.text_by_output_index
                    .entry(*output_index)
                    .or_default()
                    .push_str(text);
            }
            ResponsesUpdate::ReasoningTextDelta {
                output_index,
                kind,
                text,
            } => {
                self.reasoning_by_output_index
                    .entry((*output_index, *kind))
                    .or_default()
                    .push_str(text);
            }
            ResponsesUpdate::ToolCall { output_index, call } => {
                self.output_items
                    .insert(*output_index, ItemKind::ToolCall(call.clone()));
            }
            ResponsesUpdate::OutputItem { output_index, item } => {
                self.output_items.insert(*output_index, item.clone());
            }
            ResponsesUpdate::CompactionStarted { .. }
            | ResponsesUpdate::Usage(_)
            | ResponsesUpdate::ResponseId(_)
            | ResponsesUpdate::Finished(_) => {}
        }
    }

    fn supplement_response(&self, response: &mut InferenceResponse) {
        let mut streamed_items = self.output_items.clone();
        for (output_index, text) in &self.text_by_output_index {
            streamed_items
                .entry(*output_index)
                .or_insert_with(|| ItemKind::Message(Message::text(Role::Assistant, text.clone())));
        }
        for ((output_index, kind), text) in &self.reasoning_by_output_index {
            streamed_items.entry(*output_index).or_insert_with(|| {
                ItemKind::ReasoningText(ReasoningText {
                    kind: *kind,
                    text: text.clone(),
                })
            });
        }

        for item in streamed_items.into_values() {
            if !response
                .items
                .iter()
                .any(|existing| same_item(existing, &item))
            {
                response.items.push(item);
            }
        }
    }
}

fn same_item(left: &ItemKind, right: &ItemKind) -> bool {
    match (left, right) {
        (ItemKind::Message(left), ItemKind::Message(right)) => {
            left.role == right.role
                && left.text_content() == right.text_content()
                && left.phase == right.phase
        }
        (ItemKind::ReasoningText(left), ItemKind::ReasoningText(right)) => left == right,
        (ItemKind::ToolCall(left), ItemKind::ToolCall(right)) => left.id == right.id,
        (ItemKind::ToolResult(left), ItemKind::ToolResult(right)) => left.call_id == right.call_id,
        (ItemKind::ProviderItem(left), ItemKind::ProviderItem(right)) => {
            left.kind == right.kind && left.payload == right.payload
        }
        _ => false,
    }
}

impl AgentStore {
    async fn append_block(&self, block: &ItemBlock) -> Result<()> {
        match self {
            AgentStore::CborLog(log) => log.append_block(block).await,
            AgentStore::RedbLog(log) => log.append_block(block).await,
        }
    }

    async fn read_blocks(&self) -> Result<Vec<ItemBlock>> {
        match self {
            AgentStore::CborLog(log) => log.read_blocks().await,
            AgentStore::RedbLog(log) => log.read_blocks().await,
        }
    }
}

impl AgentInference {
    fn stream(&self, request: InferenceRequest) -> InferenceStream {
        match self {
            AgentInference::Responses(session) => session.stream(request),
            #[cfg(test)]
            AgentInference::Test { stream } => stream(request),
        }
    }
}

impl AgentTools {
    fn specs(&self) -> Vec<rho::ToolSpec> {
        match self {
            AgentTools::Shell(tool) => tool.specs(),
        }
    }

    fn supports(&self, name: &str) -> bool {
        match self {
            AgentTools::Shell(tool) => tool.supports(name),
        }
    }

    fn call(&self, call: rho::ToolCall) -> ToolFuture {
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
