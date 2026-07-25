//! A small, standalone agent harness.
//!
//! `rho-agent2` deliberately contains only the native inference loop, durable
//! transcript/queue storage, and a cheap observable handle. It has no tools,
//! workspace, collaboration, prompt discovery, Claude, or code-mode support.

use std::borrow::Cow;
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::{Arc, RwLock};

use redb::TableHandle as _;
use rho_core::{
    ContentPart, ContextBlock, InferenceEvent, InferenceRequest, InferenceResponseItem,
    MessageDelivery, MessageSender, PendingInferenceResponse, ProviderResponseId,
};
use rho_db::{RhoDb, Sen, SenValue};
use rho_inference::config::{InferenceModel, InferenceProfile};
use rho_inference::{InferenceAuth, InferenceSession, PromptCacheKey};
use senax_encoder::{Decode, Encode};
use tokio::sync::{Notify, mpsc};

const RECORDS: redb::TableDefinition<[u8; 16], Sen<AgentRecord>> =
    redb::TableDefinition::new("rho-agent2.records.v1");
const EVENTS: redb::TableDefinition<Sen<EventKey>, Sen<AgentEvent<'static>>> =
    redb::TableDefinition::new("rho-agent2.events.v1");

/// Stable identifier for an agent in a [`Store`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct AgentId([u8; 16]);

impl AgentId {
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(self) -> [u8; 16] {
        self.0
    }

    fn generate() -> Self {
        let mut bytes = [0; 16];
        use rand::RngCore as _;
        rand::thread_rng().fill_bytes(&mut bytes);
        Self(bytes)
    }
}

/// An isolated redb-backed store. It may use its own database file or coexist
/// with unrelated `rho-db` tables.
#[derive(Clone, Debug)]
pub struct Store(RhoDb);

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Self {
        Self(RhoDb::open(path))
    }

    fn load(&self, id: AgentId) -> Option<(AgentRecord, Vec<AgentEvent<'static>>)> {
        let read = self.0.read();
        if !read.has_table(RECORDS.name()) {
            return None;
        }
        let records = read.open_table(RECORDS);
        let record = records.get(id.0)?.value().as_ref().clone();
        let mut events = Vec::with_capacity(record.next_event as usize);
        if record.next_event != 0 {
            let table = read.open_table(EVENTS);
            for sequence in 0..record.next_event {
                let key = SenValue::owned(EventKey { id: id.0, sequence });
                events.push(
                    table
                        .get(key)
                        .unwrap_or_else(|| panic!("missing rho-agent2 event {sequence}"))
                        .value()
                        .as_ref()
                        .clone(),
                );
            }
        }
        Some((record, events))
    }

    async fn create_record(&self, record: &AgentRecord) -> AgentId {
        let mut write = self.0.write().await;
        let mut records = write.open_table(RECORDS);
        let id = loop {
            let id = AgentId::generate();
            if records.get(id.0).is_none() {
                break id;
            }
        };
        records.insert(id.0, SenValue::borrowed(record));
        drop(records);
        write.commit();
        id
    }

    async fn append(&self, id: AgentId, sequence: u64, event: &AgentEvent<'_>) {
        let mut write = self.0.write().await;
        let mut records = write.open_table(RECORDS);
        let mut record = records
            .get(id.0)
            .unwrap_or_else(|| panic!("rho-agent2 agent disappeared"))
            .value()
            .as_ref()
            .clone();
        assert_eq!(record.next_event, sequence, "stale agent event writer");
        record.next_event += 1;
        records.insert(id.0, SenValue::borrowed(&record));
        drop(records);
        let mut events = write.open_table(EVENTS);
        events.insert(
            SenValue::owned(EventKey { id: id.0, sequence }),
            SenValue::borrowed(event),
        );
        drop(events);
        write.commit();
    }
}

#[derive(Clone, Debug, Encode, Decode)]
struct AgentRecord {
    instructions: String,
    profile: InferenceProfile,
    model: PersistedModel,
    prompt_cache_key: PromptCacheKey,
    next_event: u64,
}

#[derive(Clone, Copy, Debug, Encode, Decode)]
enum PersistedModel {
    Gpt55,
    Gpt56Sol,
    Gpt56Luna,
    Gpt56Terra,
}

impl From<InferenceModel> for PersistedModel {
    fn from(value: InferenceModel) -> Self {
        match value {
            InferenceModel::Gpt55 => Self::Gpt55,
            InferenceModel::Gpt56Sol => Self::Gpt56Sol,
            InferenceModel::Gpt56Luna => Self::Gpt56Luna,
            InferenceModel::Gpt56Terra => Self::Gpt56Terra,
        }
    }
}

impl From<PersistedModel> for InferenceModel {
    fn from(value: PersistedModel) -> Self {
        match value {
            PersistedModel::Gpt55 => Self::Gpt55,
            PersistedModel::Gpt56Sol => Self::Gpt56Sol,
            PersistedModel::Gpt56Luna => Self::Gpt56Luna,
            PersistedModel::Gpt56Terra => Self::Gpt56Terra,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Encode, Decode)]
struct EventKey {
    id: [u8; 16],
    sequence: u64,
}

/// Durable timeline events. Queue insertion is recorded before it becomes
/// live state, and dequeue boundaries make pending inputs restart-safe.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
enum AgentEvent<'a> {
    Queued(QueuedItem),
    Dequeued {
        boundary: MessageDelivery,
    },
    QueueCleared,
    RequestStarted,
    RequestCancelled,
    InferenceResponse {
        items: Cow<'a, [InferenceResponseItem]>,
        provider_response_id: Option<ProviderResponseId>,
        context_used: Option<u64>,
    },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct QueuedItem {
    pub kind: QueuedItemKind,
    pub delivery: MessageDelivery,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum QueuedItemKind {
    UserMessage {
        sender: MessageSender,
        content: Arc<Vec<ContentPart>>,
    },
    Compaction,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InputQueue {
    items: Vec<QueuedItem>,
}

impl InputQueue {
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &QueuedItem> {
        self.items.iter()
    }

    fn drain(&mut self, boundary: MessageDelivery) -> Vec<QueuedItem> {
        let mut delivered = Vec::new();
        self.items.retain(|item| {
            let eligible =
                boundary == MessageDelivery::NextTurn || item.delivery != MessageDelivery::NextTurn;
            if eligible {
                delivered.push(item.clone());
            }
            !eligible
        });
        delivered
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct AgentState {
    pub blocks: Vec<Arc<ContextBlock>>,
    pub queued_inputs: InputQueue,
    pub status: AgentStatus,
    pub context_used: Option<u64>,
    pub quota: Option<QuotaObservation>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum AgentStatus {
    Idle,
    Streaming {
        pending_response: PendingInferenceResponse,
        temporary_failures: u64,
        last_error: Option<Arc<str>>,
    },
    /// The process stopped after a request began but before its response was
    /// durably recorded. Call [`Agent::continue_turn`] to retry it.
    Interrupted,
    Error {
        error: Arc<str>,
        attempt_count: NonZeroU64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaObservation {
    pub observed_at: rho_core::UnixMs,
    pub used_percent: u8,
    pub reset_at_unix: Option<i64>,
}

/// Cheap handle for observing and controlling a running agent.
#[derive(Clone)]
pub struct Agent {
    id: AgentId,
    state: Arc<RwLock<AgentState>>,
    control: mpsc::UnboundedSender<Control>,
    notify: Arc<Notify>,
}

impl Agent {
    pub async fn create(
        store: Store,
        auth: InferenceAuth,
        profile: InferenceProfile,
        model: InferenceModel,
        instructions: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let record = AgentRecord {
            instructions: instructions.into(),
            // This harness never exposes code mode, regardless of a copied
            // caller profile.
            profile: InferenceProfile {
                code_mode: false,
                ..profile
            },
            model: model.into(),
            prompt_cache_key: PromptCacheKey::generate(),
            next_event: 0,
        };
        let id = store.create_record(&record).await;
        Ok(Self::start(store, auth, id, record, Restored::default()))
    }

    pub fn load(store: Store, auth: InferenceAuth, id: AgentId) -> anyhow::Result<Self> {
        let (mut record, events) = store
            .load(id)
            .ok_or_else(|| anyhow::anyhow!("rho-agent2 agent not found"))?;
        record.profile.code_mode = false;
        Ok(Self::start(store, auth, id, record, restore(events)))
    }

    fn start(
        store: Store,
        auth: InferenceAuth,
        id: AgentId,
        record: AgentRecord,
        restored: Restored,
    ) -> Self {
        let session = InferenceSession::new_deep(
            auth,
            record.profile,
            record.model.into(),
            record.prompt_cache_key,
        );
        let state = Arc::new(RwLock::new(AgentState {
            blocks: restored.blocks,
            queued_inputs: restored.queue,
            status: if restored.request_active {
                AgentStatus::Interrupted
            } else {
                AgentStatus::Idle
            },
            context_used: restored.context_used,
            quota: None,
        }));
        let notify = Arc::new(Notify::new());
        let (control, control_rx) = mpsc::unbounded_channel();
        tokio::spawn(
            AgentLoop {
                store,
                id,
                next_event: record.next_event,
                instructions: Arc::from(record.instructions),
                session,
                auto_compaction_in_flight: false,
                state: Arc::clone(&state),
                notify: Arc::clone(&notify),
                control_rx,
            }
            .run(),
        );
        Self {
            id,
            state,
            control,
            notify,
        }
    }

    pub fn id(&self) -> AgentId {
        self.id
    }

    pub fn state(&self) -> AgentState {
        self.state.read().expect("poisoned agent state").clone()
    }

    pub fn send_user_message(&self, text: impl Into<String>, delivery: MessageDelivery) {
        let _ = self.control.send(Control::Enqueue(QueuedItem {
            kind: QueuedItemKind::UserMessage {
                sender: MessageSender::User,
                content: Arc::new(vec![ContentPart::Text { text: text.into() }]),
            },
            delivery,
        }));
    }

    pub fn compact(&self, delivery: MessageDelivery) {
        let _ = self.control.send(Control::Enqueue(QueuedItem {
            kind: QueuedItemKind::Compaction,
            delivery,
        }));
    }

    pub fn continue_turn(&self) {
        let _ = self.control.send(Control::Continue);
    }

    /// Abort the current request and durably discard queued inputs.
    pub fn cancel(&self) {
        let _ = self.control.send(Control::Cancel);
    }

    /// Yields an immediate snapshot and then every changed state.
    pub fn subscribe(&self) -> impl futures::Stream<Item = AgentState> + use<> {
        let state = Arc::clone(&self.state);
        let notify = Arc::clone(&self.notify);
        async_stream::stream! {
            loop {
                let notified = notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let snapshot = state.read().expect("poisoned agent state").clone();
                yield snapshot;
                notified.await;
            }
        }
    }
}

enum Control {
    Enqueue(QueuedItem),
    Continue,
    Cancel,
}

struct AgentLoop {
    store: Store,
    id: AgentId,
    next_event: u64,
    instructions: Arc<str>,
    session: InferenceSession,
    auto_compaction_in_flight: bool,
    state: Arc<RwLock<AgentState>>,
    notify: Arc<Notify>,
    control_rx: mpsc::UnboundedReceiver<Control>,
}

impl AgentLoop {
    async fn run(mut self) {
        loop {
            let mut state = self.state.read().expect("poisoned agent state").clone();
            tokio::select! {
                biased;
                control = self.control_rx.recv() => {
                    let Some(control) = control else { return };
                    match control {
                        Control::Enqueue(item) => {
                            self.persist(AgentEvent::Queued(item.clone())).await;
                            state.queued_inputs.items.push(item);
                            if !matches!(state.status, AgentStatus::Streaming { .. }) {
                                self.deliver(&mut state, MessageDelivery::NextTurn).await;
                                self.start_request(&mut state).await;
                            }
                        }
                        Control::Continue => {
                            let should_continue = matches!(
                                state.status,
                                AgentStatus::Interrupted | AgentStatus::Error { .. }
                            ) || (matches!(state.status, AgentStatus::Idle)
                                && !state.queued_inputs.is_empty());
                            if should_continue {
                                self.deliver(&mut state, MessageDelivery::NextTurn).await;
                                if !state.blocks.is_empty() {
                                    self.start_request(&mut state).await;
                                }
                            }
                        }
                        Control::Cancel => {
                            self.session.abort();
                            self.auto_compaction_in_flight = false;
                            self.persist(AgentEvent::RequestCancelled).await;
                            if !state.queued_inputs.is_empty() {
                                self.persist(AgentEvent::QueueCleared).await;
                                state.queued_inputs.items.clear();
                            }
                            state.status = AgentStatus::Idle;
                        }
                    }
                }
                event = self.session.run() => {
                    let AgentStatus::Streaming {
                        mut pending_response,
                        mut temporary_failures,
                        mut last_error,
                    } = std::mem::replace(&mut state.status, AgentStatus::Idle) else {
                        unreachable!("inference event without active request")
                    };
                    match event {
                        InferenceEvent::RequestSent | InferenceEvent::StreamingStarted => {}
                        InferenceEvent::Quota { used_percent, reset_at_unix } => {
                            state.quota = Some(QuotaObservation {
                                observed_at: rho_core::UnixMs::now(),
                                used_percent,
                                reset_at_unix,
                            });
                        }
                        InferenceEvent::ContextItem { index, event } => {
                            pending_response.apply(index, event);
                        }
                        InferenceEvent::TemporaryFailure { error, .. } => {
                            temporary_failures = temporary_failures.saturating_add(1);
                            last_error = Some(Arc::from(error.to_string()));
                            pending_response = PendingInferenceResponse::default();
                        }
                        InferenceEvent::Failed { error } => {
                            self.auto_compaction_in_flight = false;
                            state.status = AgentStatus::Error {
                                error: Arc::from(error.to_string()),
                                attempt_count: NonZeroU64::new(temporary_failures.saturating_add(1))
                                    .unwrap(),
                            };
                            self.publish(state);
                            continue;
                        }
                        InferenceEvent::Finished { usage, provider_response_id } => {
                            match pending_response.finish() {
                                Err(error) => {
                                    self.auto_compaction_in_flight = false;
                                    state.status = AgentStatus::Error {
                                        error: Arc::from(error.to_string()),
                                        attempt_count: NonZeroU64::new(
                                            temporary_failures.saturating_add(1),
                                        ).unwrap(),
                                    };
                                }
                                Ok(items) => {
                                    let compacted = items.iter().any(|item| {
                                        matches!(item, InferenceResponseItem::Compaction { .. })
                                    });
                                    let context_used = (!compacted)
                                        .then(|| usage.map(|u| u.input_tokens + u.output_tokens))
                                        .flatten();
                                    state.context_used = context_used.or(state.context_used);
                                    if compacted {
                                        state.context_used = None;
                                    }
                                    self.persist(AgentEvent::InferenceResponse {
                                        items: Cow::Borrowed(&items),
                                        provider_response_id: provider_response_id.clone(),
                                        context_used,
                                    }).await;
                                    let unexpected_tool = items.iter().any(|item| {
                                        matches!(item, InferenceResponseItem::ToolCall { .. })
                                    });
                                    state.blocks.push(Arc::new(ContextBlock::InferenceResponse {
                                        items,
                                        provider_response_id,
                                    }));
                                    if unexpected_tool {
                                        self.auto_compaction_in_flight = false;
                                        state.status = AgentStatus::Error {
                                            error: Arc::from(
                                                "provider returned a tool call with an empty tool surface",
                                            ),
                                            attempt_count: NonZeroU64::MIN,
                                        };
                                    } else if compacted
                                        && std::mem::take(&mut self.auto_compaction_in_flight)
                                    {
                                        self.deliver(&mut state, MessageDelivery::NextRequest).await;
                                        self.start_request(&mut state).await;
                                    } else if !state.queued_inputs.is_empty() {
                                        self.deliver(&mut state, MessageDelivery::NextTurn).await;
                                        self.start_request(&mut state).await;
                                    } else {
                                        state.status = AgentStatus::Idle;
                                    }
                                }
                            }
                            self.publish(state);
                            continue;
                        }
                    }
                    state.status = AgentStatus::Streaming {
                        pending_response,
                        temporary_failures,
                        last_error,
                    };
                }
            }
            self.publish(state);
        }
    }

    async fn persist(&mut self, event: AgentEvent<'_>) {
        self.store.append(self.id, self.next_event, &event).await;
        self.next_event += 1;
    }

    async fn deliver(&mut self, state: &mut AgentState, boundary: MessageDelivery) {
        let items = state.queued_inputs.drain(boundary);
        if items.is_empty() {
            return;
        }
        self.persist(AgentEvent::Dequeued { boundary }).await;
        state.blocks.extend(items.into_iter().map(|item| {
            Arc::new(match item.kind {
                QueuedItemKind::UserMessage { sender, content } => ContextBlock::UserMessage {
                    sender,
                    content: Arc::unwrap_or_clone(content),
                },
                QueuedItemKind::Compaction => ContextBlock::CompactionTrigger,
            })
        }));
    }

    async fn start_request(&mut self, state: &mut AgentState) {
        let compact = self
            .session
            .auto_compact_token_limit()
            .zip(state.context_used)
            .is_some_and(|(limit, used)| used >= limit)
            && !latest_request_has_compaction_trigger(&state.blocks);
        if compact {
            let item = QueuedItem {
                kind: QueuedItemKind::Compaction,
                delivery: MessageDelivery::NextRequest,
            };
            self.persist(AgentEvent::Queued(item.clone())).await;
            state.queued_inputs.items.push(item);
            self.deliver(state, MessageDelivery::NextRequest).await;
        }
        self.auto_compaction_in_flight = compact;
        self.persist(AgentEvent::RequestStarted).await;
        self.session.request(InferenceRequest {
            instructions: Arc::clone(&self.instructions),
            input: state.blocks.clone(),
            agent_id_labels: Default::default(),
            tools: Arc::from([]),
        });
        state.status = AgentStatus::Streaming {
            pending_response: PendingInferenceResponse::default(),
            temporary_failures: 0,
            last_error: None,
        };
    }

    fn publish(&self, state: AgentState) {
        *self.state.write().expect("poisoned agent state") = state;
        self.notify.notify_waiters();
    }
}

fn latest_request_has_compaction_trigger(blocks: &[Arc<ContextBlock>]) -> bool {
    blocks
        .iter()
        .rev()
        .find_map(|block| match &**block {
            ContextBlock::CompactionTrigger => Some(true),
            ContextBlock::InferenceResponse { .. } => Some(false),
            ContextBlock::UserMessage { .. }
            | ContextBlock::ToolResults { .. }
            | ContextBlock::ToolUpdate(_) => None,
        })
        .unwrap_or(false)
}

#[derive(Default)]
struct Restored {
    blocks: Vec<Arc<ContextBlock>>,
    queue: InputQueue,
    request_active: bool,
    context_used: Option<u64>,
}

fn restore(events: Vec<AgentEvent<'static>>) -> Restored {
    let mut restored = Restored::default();
    for event in events {
        match event {
            AgentEvent::Queued(item) => restored.queue.items.push(item),
            AgentEvent::Dequeued { boundary } => {
                for item in restored.queue.drain(boundary) {
                    restored.blocks.push(Arc::new(match item.kind {
                        QueuedItemKind::UserMessage { sender, content } => {
                            ContextBlock::UserMessage {
                                sender,
                                content: Arc::unwrap_or_clone(content),
                            }
                        }
                        QueuedItemKind::Compaction => ContextBlock::CompactionTrigger,
                    }));
                }
            }
            AgentEvent::QueueCleared => restored.queue.items.clear(),
            AgentEvent::RequestStarted => restored.request_active = true,
            AgentEvent::RequestCancelled => restored.request_active = false,
            AgentEvent::InferenceResponse {
                items,
                provider_response_id,
                context_used,
            } => {
                restored.request_active = false;
                if items
                    .iter()
                    .any(|item| matches!(item, InferenceResponseItem::Compaction { .. }))
                {
                    restored.context_used = None;
                } else if context_used.is_some() {
                    restored.context_used = context_used;
                }
                restored
                    .blocks
                    .push(Arc::new(ContextBlock::InferenceResponse {
                        items: items.into_owned(),
                        provider_response_id,
                    }));
            }
        }
    }
    restored
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(text: &str, delivery: MessageDelivery) -> QueuedItem {
        QueuedItem {
            kind: QueuedItemKind::UserMessage {
                sender: MessageSender::User,
                content: Arc::new(vec![ContentPart::Text {
                    text: text.to_owned(),
                }]),
            },
            delivery,
        }
    }

    #[test]
    fn replay_preserves_undelivered_queue() {
        let restored = restore(vec![AgentEvent::Queued(message(
            "later",
            MessageDelivery::NextTurn,
        ))]);
        assert_eq!(restored.queue.len(), 1);
        assert!(restored.blocks.is_empty());
    }

    #[test]
    fn next_request_delivery_leaves_next_turn_items_queued() {
        let restored = restore(vec![
            AgentEvent::Queued(message("steer", MessageDelivery::NextRequest)),
            AgentEvent::Queued(message("later", MessageDelivery::NextTurn)),
            AgentEvent::Dequeued {
                boundary: MessageDelivery::NextRequest,
            },
        ]);
        assert_eq!(restored.blocks.len(), 1);
        assert_eq!(restored.queue.len(), 1);
    }

    #[test]
    fn started_request_restores_as_interrupted() {
        let restored = restore(vec![
            AgentEvent::Queued(message("hello", MessageDelivery::Immediate)),
            AgentEvent::Dequeued {
                boundary: MessageDelivery::NextTurn,
            },
            AgentEvent::RequestStarted,
        ]);
        assert!(restored.request_active);
        assert!(restored.queue.is_empty());
    }

    #[tokio::test]
    async fn store_round_trips_record_and_queue_event() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("agent2.redb"));
        let record = AgentRecord {
            instructions: "minimal".to_owned(),
            profile: InferenceProfile::default(),
            model: PersistedModel::Gpt56Sol,
            prompt_cache_key: PromptCacheKey::generate(),
            next_event: 0,
        };
        let id = store.create_record(&record).await;
        store
            .append(
                id,
                0,
                &AgentEvent::Queued(message("persist me", MessageDelivery::NextTurn)),
            )
            .await;

        let (loaded, events) = store.load(id).unwrap();
        assert_eq!(loaded.instructions, "minimal");
        assert_eq!(loaded.next_event, 1);
        assert_eq!(events.len(), 1);
        assert_eq!(restore(events).queue.len(), 1);
    }
}
