//! Raw redb schema for persisted agents.

use std::sync::Arc;

use redb::{TableDefinition, Value as _};
use redb_derive::{Key, Value as RedbValue};
use rho_core::ContextBlock;
use rho_db::{ReadTxn, Sen, WriteTxn};
use rho_inference::PromptCacheKey;
use rho_inference::config::InferenceConfig;
use senax_encoder::{Decode, Encode};

const COUNTERS: TableDefinition<CounterKey, u64> = TableDefinition::new("counters");
#[allow(dead_code)]
const LINEAGE_PARENTS: TableDefinition<AgentLineageId, AgentTimelineRef> =
    TableDefinition::new("lineage_parents");
const AGENT_TIMELINE: TableDefinition<AgentTimelineRef, Sen<AgentTimelineEntry>> =
    TableDefinition::new("agent_timeline");
const AGENTS: TableDefinition<AgentId, Sen<AgentRecord>> = TableDefinition::new("agents");

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue)]
struct CounterKey(u8);

impl CounterKey {
    pub const LAST_AGENT_ID: Self = Self(1);
    pub const LAST_LINEAGE_ID: Self = Self(2);
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Key, RedbValue, Encode, Decode,
)]
pub struct AgentId(u64);

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Key, RedbValue, Encode, Decode,
)]
pub struct AgentLineageId(u64);

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Key, RedbValue, Encode, Decode,
)]
pub struct AgentTimelineRef {
    lineage_id: AgentLineageId,
    seq: u32,
}

impl AgentTimelineRef {
    fn root(lineage_id: AgentLineageId) -> Self {
        Self { lineage_id, seq: 0 }
    }

    fn next(self) -> Self {
        Self {
            lineage_id: self.lineage_id,
            seq: self
                .seq
                .checked_add(1)
                .expect("agent timeline sequence overflow"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
pub struct UnixMillis(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AgentRecord {
    pub display_name: Option<String>,
    pub created_at: UnixMillis,
    pub updated_at: UnixMillis,
    pub current_lineage: AgentLineageId,
    pub parent_agent: Option<AgentId>,
    pub prompt_cache_key: PromptCacheKey,
    pub config: InferenceConfig,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct AgentTimelineEntry {
    pub created_at: UnixMillis,
    pub context_block: ContextBlock,
}

pub trait AgentReadTxnExt {
    fn get_agent(&self, agent_id: AgentId) -> AgentRecord;
    fn agent_blocks(&self, agent_id: AgentId) -> (AgentTimelineRef, Vec<Arc<ContextBlock>>);
}

pub trait AgentWriteTxnExt {
    fn create_agent(
        &mut self,
        now: UnixMillis,
        display_name: Option<String>,
        prompt_cache_key: PromptCacheKey,
        config: InferenceConfig,
    ) -> (AgentId, AgentTimelineRef);

    fn append_agent_block(
        &mut self,
        at: AgentTimelineRef,
        created_at: UnixMillis,
        context_block: Arc<ContextBlock>,
    ) -> AgentTimelineRef;
}

impl AgentReadTxnExt for ReadTxn {
    fn get_agent(&self, agent_id: AgentId) -> AgentRecord {
        self.open_table(AGENTS)
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .0
    }

    fn agent_blocks(&self, agent_id: AgentId) -> (AgentTimelineRef, Vec<Arc<ContextBlock>>) {
        let agent = self.get_agent(agent_id);
        let start = AgentTimelineRef::root(agent.current_lineage);
        let end = AgentTimelineRef {
            lineage_id: agent.current_lineage,
            seq: u32::MAX,
        };
        let mut next = start;
        let blocks = self
            .open_table(AGENT_TIMELINE)
            .range(start..=end)
            .map(|(key, value)| {
                next = key.value().next();
                Arc::new(value.value().0.context_block)
            })
            .collect();
        (next, blocks)
    }
}

impl AgentWriteTxnExt for WriteTxn {
    fn create_agent(
        &mut self,
        now: UnixMillis,
        display_name: Option<String>,
        prompt_cache_key: PromptCacheKey,
        config: InferenceConfig,
    ) -> (AgentId, AgentTimelineRef) {
        let agent_id = AgentId(next_counter(self, CounterKey::LAST_AGENT_ID));
        let lineage_id = AgentLineageId(next_counter(self, CounterKey::LAST_LINEAGE_ID));
        let agent = AgentRecord {
            display_name,
            created_at: now,
            updated_at: now,
            current_lineage: lineage_id,
            parent_agent: None,
            prompt_cache_key,
            config,
        };
        self.open_table(AGENTS).insert(&agent_id, Sen(agent));
        (agent_id, AgentTimelineRef::root(lineage_id))
    }

    fn append_agent_block(
        &mut self,
        at: AgentTimelineRef,
        created_at: UnixMillis,
        context_block: Arc<ContextBlock>,
    ) -> AgentTimelineRef {
        self.open_table(AGENT_TIMELINE).insert(
            &at,
            Sen(AgentTimelineEntry {
                created_at,
                context_block: (*context_block).clone(),
            }),
        );
        at.next()
    }
}

fn next_counter(write: &mut WriteTxn, key: CounterKey) -> u64 {
    let mut counters = write.open_table(COUNTERS);
    let next = counters.get(&key).map(|value| value.value()).unwrap_or(0) + 1;
    counters.insert(&key, &next);
    next
}

#[cfg(test)]
mod tests;
