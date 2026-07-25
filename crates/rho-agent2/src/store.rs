//! Durable transcript and queue storage.
//!
//! History is append-only and the core is its sole writer, so the log is just
//! the order in which the core did things. Queue insertion is recorded before
//! it becomes live state, so a message that was accepted cannot be lost.

use std::borrow::Cow;
use std::path::Path;

use redb::TableHandle as _;
use rho_core::{ContextBlock, ToolCall, ToolCallId};
use rho_db::{RhoDb, Sen, SenValue};
use rho_inference::PromptCacheKey;
use rho_inference::config::{InferenceModel, InferenceProfile};
use senax_encoder::{Decode, Encode};

use crate::source::QueuedInput;

const RECORDS: redb::TableDefinition<[u8; 16], Sen<AgentRecord>> =
    redb::TableDefinition::new("rho-agent2.records.v1");
const EVENTS: redb::TableDefinition<Sen<EventKey>, Sen<AgentEvent<'static>>> =
    redb::TableDefinition::new("rho-agent2.events.v1");

/// Stable identifier for an agent in a [`Store`]. Distinct from
/// [`rho_core::AgentId`], which names a *peer* an agent exchanges mail with.
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

    pub(crate) fn load(&self, id: AgentId) -> Option<(AgentRecord, Vec<AgentEvent<'static>>)> {
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

    pub(crate) async fn create_record(&self, record: &AgentRecord) -> AgentId {
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

    pub(crate) async fn append(&self, id: AgentId, sequence: u64, event: &AgentEvent<'_>) {
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
pub(crate) struct AgentRecord {
    pub instructions: String,
    pub profile: InferenceProfile,
    pub model: PersistedModel,
    pub prompt_cache_key: PromptCacheKey,
    pub next_event: u64,
}

#[derive(Clone, Copy, Debug, Encode, Decode)]
pub(crate) enum PersistedModel {
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

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub(crate) enum AgentEvent<'a> {
    /// An input was accepted into a queue.
    Queued(QueuedInput),
    /// Blocks the core appended to history.
    ///
    /// `drained` marks the appends that came from pulling the sources. One bool
    /// suffices because a drain is total: there is no deferred delivery, so
    /// every source with something pending contributes to the same request.
    Appended {
        blocks: Cow<'a, [ContextBlock]>,
        drained: bool,
    },
    QueueCleared,
    RequestStarted,
    RequestEnded {
        context_used: Option<u64>,
    },
    ToolSpawned {
        call: Cow<'a, ToolCall>,
    },
    ToolReaped {
        call_id: ToolCallId,
    },
}
