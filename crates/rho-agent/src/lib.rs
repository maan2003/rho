//! Opinionated, forkable rho agent harness.
//!
//! This crate deliberately owns the state/provider/tool policy for one
//! simple workflow. Users who need different behavior should patch this code or
//! fork the crate while keeping the lower crates reusable.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow, bail};
use futures::future::{BoxFuture, FutureExt, join_all};
use rho::{
    Item, ItemBlock, ItemKind, Message, ProviderRequest, ProviderResponse, ReasoningText,
    ReasoningTextKind, Role, ToolResult,
};
use rho_provider_responses::{ProviderSession, ResponsesProvider, ResponsesUpdate};
use rho_store_cbor::CborLog;
use rho_store_redb::RedbLog;
use rho_tool_shell::ShellTools;

pub type ProviderFuture = BoxFuture<'static, Result<ProviderResponse>>;
pub type ToolFuture = BoxFuture<'static, ToolResult>;
type ProviderUpdateHandler = Arc<Mutex<Box<dyn FnMut(ResponsesUpdate) + Send>>>;
pub type StreamedTranscript = Arc<Mutex<StreamingTranscript>>;

pub enum AgentProvider {
    Responses {
        provider: Box<ResponsesProvider>,
        session: ProviderSession,
    },
    #[cfg(test)]
    Test {
        complete: Arc<
            dyn Fn(ProviderRequest, Option<ProviderUpdateHandler>) -> ProviderFuture + Send + Sync,
        >,
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
        attempt_count: u64,
        request: ProviderRequest,
        streamed_transcript: StreamedTranscript,
        future: ProviderFuture,
    },
    WaitingForTools {
        futures: Vec<ToolFuture>,
        results: Vec<ToolResult>,
    },
}

#[derive(Debug)]
enum QueueItem {
    UserMessage(Message),
}

pub struct Agent {
    provider: AgentProvider,
    tools: Vec<AgentTools>,
    store: Option<AgentStore>,
    blocks: Vec<ItemBlock>,
    queue: VecDeque<QueueItem>,
    pending_tool_results: Vec<ToolResult>,
    max_provider_retries: u64,
    provider_updates: Option<ProviderUpdateHandler>,
    next_id: u64,
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
    pub fn new(provider: AgentProvider) -> Self {
        Self {
            provider,
            tools: Vec::new(),
            store: None,
            blocks: Vec::new(),
            queue: VecDeque::new(),
            pending_tool_results: Vec::new(),
            max_provider_retries: 0,
            provider_updates: None,
            next_id: 0,
            state: AgentState::Idle,
        }
    }

    pub fn with_tool(mut self, tool: AgentTools) -> Self {
        self.tools.push(tool);
        self
    }

    pub fn with_store(mut self, store: AgentStore) -> Self {
        self.store = Some(store);
        self
    }

    pub fn with_max_provider_retries(mut self, max_provider_retries: u64) -> Self {
        self.max_provider_retries = max_provider_retries;
        self
    }

    pub fn with_provider_updates(
        mut self,
        on_update: impl FnMut(ResponsesUpdate) + Send + 'static,
    ) -> Self {
        self.provider_updates = Some(Arc::new(Mutex::new(Box::new(on_update))));
        self
    }

    pub fn from_items(provider: AgentProvider, items: Vec<Item>) -> Self {
        let blocks = if items.is_empty() {
            Vec::new()
        } else {
            vec![ItemBlock::Local { items }]
        };
        Self::from_blocks(provider, blocks)
    }

    pub async fn from_store(provider: AgentProvider, store: AgentStore) -> Result<Self> {
        let blocks = store.read_blocks().await?;
        Ok(Self::from_blocks(provider, blocks).with_store(store))
    }

    pub fn items(&self) -> Vec<Item> {
        self.blocks
            .iter()
            .flat_map(|block| match block {
                ItemBlock::Local { items } | ItemBlock::ProviderResponse { items, .. } => items,
            })
            .cloned()
            .collect()
    }

    pub fn blocks(&self) -> &[ItemBlock] {
        &self.blocks
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
                attempt_count,
                request,
                streamed_transcript,
                future,
            } => match future.await {
                Ok(response) => {
                    self.finish_provider_request(response, streamed_transcript)
                        .await?
                }
                Err(_) if attempt_count < self.max_provider_retries => {
                    let next_attempt_count = attempt_count + 1;
                    let streamed_transcript = Arc::new(Mutex::new(StreamingTranscript::default()));
                    AgentState::ApiRequest {
                        attempt_count: next_attempt_count,
                        future: self.provider.complete(
                            request.clone(),
                            self.provider_updates.clone(),
                            Arc::clone(&streamed_transcript),
                        ),
                        request,
                        streamed_transcript,
                    }
                }
                Err(error) => return Err(error),
            },
            AgentState::WaitingForTools {
                futures,
                mut results,
            } => {
                results.extend(join_all(futures).await);
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

        if !recorded_local_work || self.blocks.is_empty() {
            return Ok(AgentState::Idle);
        }

        let request = ProviderRequest {
            input: self.blocks.clone(),
            tools: self.tools.iter().flat_map(AgentTools::specs).collect(),
        };

        let streamed_transcript = Arc::new(Mutex::new(StreamingTranscript::default()));
        Ok(AgentState::ApiRequest {
            attempt_count: 0,
            future: self.provider.complete(
                request.clone(),
                self.provider_updates.clone(),
                Arc::clone(&streamed_transcript),
            ),
            request,
            streamed_transcript,
        })
    }

    async fn finish_provider_request(
        &mut self,
        mut response: ProviderResponse,
        streamed_transcript: StreamedTranscript,
    ) -> Result<AgentState> {
        streamed_transcript
            .lock()
            .expect("streamed transcript lock")
            .supplement_response(&mut response);
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
                let id = self.alloc_item_id();
                Item { id, kind }
            })
            .collect::<Vec<_>>();
        self.record_provider_response_block(response_items, provider_response_id.clone())
            .await?;

        let futures = tool_calls
            .into_iter()
            .map(|call| {
                self.tools
                    .iter()
                    .find(|tool| tool.supports(&call.name))
                    .ok_or_else(|| anyhow!("no tool registered for {}", call.name))
                    .map(|tool| tool.call(call))
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
        self.record_block(ItemBlock::ProviderResponse {
            provider_response_id,
            items,
        })
        .await
    }

    async fn record_block(&mut self, block: ItemBlock) -> Result<()> {
        if let Some(store) = &self.store {
            store.append_block(&block).await?;
        }
        self.blocks.push(block);
        Ok(())
    }

    fn queue_item_to_block(&mut self, item: QueueItem) -> ItemBlock {
        match item {
            QueueItem::UserMessage(message) => {
                let id = self.alloc_item_id();
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
                let id = self.alloc_item_id();
                Item {
                    id,
                    kind: ItemKind::ToolResult(result),
                }
            })
            .collect();
        ItemBlock::Local { items }
    }
    fn alloc_item_id(&mut self) -> rho::ItemId {
        let id = self.next_id;
        self.next_id += 1;
        rho::ItemId(format!("item-{id}"))
    }
}

impl Agent {
    fn from_blocks(provider: AgentProvider, blocks: Vec<ItemBlock>) -> Self {
        let next_id = blocks
            .iter()
            .map(|block| match block {
                ItemBlock::Local { items } | ItemBlock::ProviderResponse { items, .. } => {
                    items.len()
                }
            })
            .sum::<usize>() as u64;
        Self {
            provider,
            tools: Vec::new(),
            store: None,
            blocks,
            queue: VecDeque::new(),
            pending_tool_results: Vec::new(),
            max_provider_retries: 0,
            provider_updates: None,
            next_id,
            state: AgentState::Idle,
        }
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

    fn supplement_response(&self, response: &mut ProviderResponse) {
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

impl AgentProvider {
    fn complete(
        &self,
        request: ProviderRequest,
        provider_updates: Option<ProviderUpdateHandler>,
        streamed_transcript: StreamedTranscript,
    ) -> ProviderFuture {
        match self {
            AgentProvider::Responses { provider, session } => provider
                .complete_streaming(session, request, move |update| {
                    streamed_transcript
                        .lock()
                        .expect("streamed transcript lock")
                        .record(&update);
                    if let Some(provider_updates) = &provider_updates {
                        let mut on_update = provider_updates.lock().expect("provider update lock");
                        on_update(update);
                    }
                })
                .boxed(),
            #[cfg(test)]
            AgentProvider::Test { complete } => {
                let provider_updates = {
                    let streamed_transcript = Arc::clone(&streamed_transcript);
                    Some(Arc::new(Mutex::new(Box::new(move |update| {
                        streamed_transcript
                            .lock()
                            .expect("streamed transcript lock")
                            .record(&update);
                        if let Some(provider_updates) = &provider_updates {
                            let mut on_update =
                                provider_updates.lock().expect("provider update lock");
                            on_update(update);
                        }
                    })
                        as Box<dyn FnMut(ResponsesUpdate) + Send>)))
                };
                complete(request, provider_updates)
            }
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
