//! Raw redb schema for persisted agents.

use redb::{TableDefinition, Value as _};
use redb_derive::{Key, Value as RedbValue};
use rho_db::{ReadTxn, Sen, WriteTxn};
use rho_inference::PromptCacheKey;
use rho_inference::config::InferenceProtectedConfig;
use senax_encoder::{Decode, Encode};

use crate::AgentEvent;

const COUNTERS: TableDefinition<CounterKey, u64> = TableDefinition::new("counters");
const LINEAGE_PARENTS: TableDefinition<AgentLineageId, AgentEventPos> =
    TableDefinition::new("lineage_parents");
const AGENT_EVENTS: TableDefinition<AgentEventPos, Sen<AgentEvent>> =
    TableDefinition::new("agent_events");
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
pub struct AgentEventPos {
    lineage_id: AgentLineageId,
    seq: u32,
}

impl AgentEventPos {
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

impl UnixMillis {
    pub fn now() -> Self {
        Self(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time before unix epoch")
                .as_millis()
                .try_into()
                .expect("unix millis overflow"),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AgentRecord {
    pub display_name: Option<String>,
    pub created_at: UnixMillis,
    pub updated_at: UnixMillis,
    pub current_lineage: AgentLineageId,
    pub parent_agent: Option<AgentId>,
    pub prompt_cache_key: PromptCacheKey,
    pub config: InferenceProtectedConfig,
}

pub trait AgentReadTxnExt {
    fn get_agent(&self, agent_id: AgentId) -> AgentRecord;
    fn agent_events(&self, agent_id: AgentId) -> (AgentEventPos, Vec<AgentEvent>);
}

pub trait AgentWriteTxnExt {
    fn create_agent(
        &mut self,
        now: UnixMillis,
        display_name: Option<String>,
        prompt_cache_key: PromptCacheKey,
        config: InferenceProtectedConfig,
    ) -> (AgentId, AgentEventPos);

    fn append_agent_event(&mut self, at: AgentEventPos, event: AgentEvent) -> AgentEventPos;
}

impl AgentReadTxnExt for ReadTxn {
    fn get_agent(&self, agent_id: AgentId) -> AgentRecord {
        self.open_table(AGENTS)
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .0
    }

    fn agent_events(&self, agent_id: AgentId) -> (AgentEventPos, Vec<AgentEvent>) {
        let agent = self.get_agent(agent_id);
        let mut segments = Vec::new();
        let mut lineage_id = agent.current_lineage;
        let mut end_seq = u32::MAX;
        let lineage_parents = self.open_table(LINEAGE_PARENTS);
        loop {
            segments.push((lineage_id, end_seq));
            let Some(parent) = lineage_parents.get(&lineage_id) else {
                break;
            };
            let parent = parent.value();
            lineage_id = parent.lineage_id;
            end_seq = parent.seq;
        }
        drop(lineage_parents);

        let mut events = Vec::new();
        let mut next = AgentEventPos::root(agent.current_lineage);
        let timeline = self.open_table(AGENT_EVENTS);
        for (lineage_id, end_seq) in segments.into_iter().rev() {
            let is_current_lineage = lineage_id == agent.current_lineage;
            for (key, value) in timeline.range(
                AgentEventPos::root(lineage_id)..=AgentEventPos {
                    lineage_id,
                    seq: end_seq,
                },
            ) {
                let key = key.value();
                if key.seq == end_seq && end_seq != u32::MAX {
                    break;
                }
                if is_current_lineage {
                    next = key.next();
                }
                events.push(value.value().0);
            }
        }
        (next, events)
    }
}

impl AgentWriteTxnExt for WriteTxn {
    fn create_agent(
        &mut self,
        now: UnixMillis,
        display_name: Option<String>,
        prompt_cache_key: PromptCacheKey,
        config: InferenceProtectedConfig,
    ) -> (AgentId, AgentEventPos) {
        let agent_id = AgentId(next_counter(self, CounterKey::LAST_AGENT_ID));
        let lineage_id = AgentLineageId(next_counter(self, CounterKey::LAST_LINEAGE_ID));
        self.open_table(LINEAGE_PARENTS);
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
        (agent_id, AgentEventPos::root(lineage_id))
    }

    fn append_agent_event(&mut self, at: AgentEventPos, event: AgentEvent) -> AgentEventPos {
        self.open_table(AGENT_EVENTS).insert(&at, Sen(event));
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
