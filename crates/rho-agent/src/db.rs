//! Raw redb schema for persisted agents.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use redb::{TableDefinition, Value as _};
use redb_derive::{Key, Value as RedbValue};
use rho_core::UnixMs;
use rho_db::{ReadTxn, Sen, SenValue, WriteTxn};
use rho_inference::PromptCacheKey;
use rho_inference::config::InferenceProtectedConfig;
use senax_encoder::{Decode, Encode, Pack, Unpack};

use crate::AgentEvent;

const COUNTERS: TableDefinition<CounterKey, u64> = TableDefinition::new("counters");
const LINEAGE_PARENTS: TableDefinition<AgentLineageId, AgentEventPos> =
    TableDefinition::new("lineage_parents");
const AGENT_EVENTS: TableDefinition<AgentEventPos, Sen<AgentEvent<'static>>> =
    TableDefinition::new("agent_events");
const AGENTS: TableDefinition<AgentId, Sen<AgentRecord>> = TableDefinition::new("agents");
const TOPICS: TableDefinition<TopicId, Sen<TopicRecord>> = TableDefinition::new("topics");
const TOPIC_AGENTS: TableDefinition<TopicAgentKey, ()> = TableDefinition::new("topic_agents");
/// Keyed by the workdir's absolute path (UTF-8; paths are strings on disk
/// and on the wire), making paths unique by construction.
const WORKDIRS: TableDefinition<String, Sen<WorkdirRecord>> = TableDefinition::new("workdirs");

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue)]
struct CounterKey(u8);

impl CounterKey {
    pub const LAST_AGENT_ID: Self = Self(1);
    pub const LAST_LINEAGE_ID: Self = Self(2);
    pub const LAST_TOPIC_ID: Self = Self(3);
}

#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Key,
    RedbValue,
    Encode,
    Decode,
    Pack,
    Unpack,
)]
pub struct AgentId(u64);

impl fmt::Display for AgentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "agent-{}", self.0)
    }
}

#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Key,
    RedbValue,
    Encode,
    Decode,
    Pack,
    Unpack,
)]
pub struct TopicId(u64);

impl fmt::Display for TopicId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "topic-{}", self.0)
    }
}

impl FromStr for TopicId {
    type Err = std::num::ParseIntError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.strip_prefix("topic-").unwrap_or(value);
        value.parse().map(Self)
    }
}

/// A registered directory agents can be started in, keyed by its absolute
/// path. Purely selection vocabulary for clients; agents record their own
/// working directory and the daemon never requires it to match a registered
/// workdir.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct WorkdirRecord {
    pub name: String,
    pub created_at: UnixMillis,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum TopicStatus {
    Normal,
    Pinned,
    Archived,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct TopicRecord {
    pub display_name: Option<String>,
    pub created_at: UnixMillis,
    pub updated_at: UnixMillis,
    pub status: TopicStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Key, RedbValue)]
pub struct TopicAgentKey {
    topic_id: TopicId,
    agent_id: AgentId,
}

impl TopicAgentKey {
    pub fn new(topic_id: TopicId, agent_id: AgentId) -> Self {
        Self { topic_id, agent_id }
    }
}

impl FromStr for AgentId {
    type Err = std::num::ParseIntError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.strip_prefix("agent-").unwrap_or(value);
        value.parse().map(Self)
    }
}

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

pub type UnixMillis = UnixMs;

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AgentRecord {
    pub display_name: Option<String>,
    /// Absolute directory this agent's tools execute in. Fixed at creation:
    /// the accumulated model context assumes one root for the agent's life.
    pub working_directory: PathBuf,
    pub created_at: UnixMillis,
    pub updated_at: UnixMillis,
    pub current_lineage: AgentLineageId,
    pub parent_agent: Option<AgentId>,
    pub prompt_cache_key: PromptCacheKey,
    pub config: InferenceProtectedConfig,
}

pub trait AgentReadTxnExt {
    fn get_topic(&self, topic_id: TopicId) -> TopicRecord;
    fn list_topics(&self) -> Vec<(TopicId, TopicRecord)>;
    fn list_topic_agents(&self, topic_id: TopicId) -> Vec<AgentId>;
    fn get_agent(&self, agent_id: AgentId) -> AgentRecord;
    fn list_agents(&self) -> Vec<(AgentId, AgentRecord)>;
    fn list_workdirs(&self) -> Vec<(PathBuf, WorkdirRecord)>;
    fn agent_events(&self, agent_id: AgentId) -> (AgentEventPos, Vec<AgentEvent<'static>>);
}

pub trait AgentWriteTxnExt {
    fn init_agent_tables(&mut self);

    fn create_topic(
        &mut self,
        now: UnixMillis,
        display_name: Option<String>,
        status: TopicStatus,
    ) -> TopicId;

    fn create_agent(
        &mut self,
        now: UnixMillis,
        topic_id: TopicId,
        display_name: Option<String>,
        working_directory: PathBuf,
        prompt_cache_key: PromptCacheKey,
        config: InferenceProtectedConfig,
    ) -> (AgentId, AgentEventPos);

    /// Registers `path` or renames it if already registered.
    fn upsert_workdir(&mut self, now: UnixMillis, path: &str, name: String);

    fn remove_workdir(&mut self, path: &str);

    fn append_agent_event(&mut self, at: AgentEventPos, event: &AgentEvent<'_>) -> AgentEventPos;
}

impl AgentReadTxnExt for ReadTxn {
    fn get_topic(&self, topic_id: TopicId) -> TopicRecord {
        self.open_table(TOPICS)
            .get(&topic_id)
            .expect("topic id missing")
            .value()
            .into_owned()
    }

    fn list_topics(&self) -> Vec<(TopicId, TopicRecord)> {
        self.open_table(TOPICS)
            .iter()
            .map(|(key, value)| (key.value(), value.value().into_owned()))
            .collect()
    }

    fn list_topic_agents(&self, topic_id: TopicId) -> Vec<AgentId> {
        self.open_table(TOPIC_AGENTS)
            .range(
                TopicAgentKey::new(topic_id, AgentId(u64::MIN))
                    ..=TopicAgentKey::new(topic_id, AgentId(u64::MAX)),
            )
            .map(|(key, _)| key.value().agent_id)
            .collect()
    }

    fn get_agent(&self, agent_id: AgentId) -> AgentRecord {
        self.open_table(AGENTS)
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned()
    }

    fn list_agents(&self) -> Vec<(AgentId, AgentRecord)> {
        self.open_table(AGENTS)
            .iter()
            .map(|(key, value)| (key.value(), value.value().into_owned()))
            .collect()
    }

    fn list_workdirs(&self) -> Vec<(PathBuf, WorkdirRecord)> {
        self.open_table(WORKDIRS)
            .iter()
            .map(|(key, value)| (PathBuf::from(key.value()), value.value().into_owned()))
            .collect()
    }

    fn agent_events(&self, agent_id: AgentId) -> (AgentEventPos, Vec<AgentEvent<'static>>) {
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
                events.push(value.value().into_owned());
            }
        }
        (next, events)
    }
}

impl AgentWriteTxnExt for WriteTxn {
    fn init_agent_tables(&mut self) {
        self.open_table(COUNTERS);
        self.open_table(LINEAGE_PARENTS);
        self.open_table(AGENT_EVENTS);
        self.open_table(AGENTS);
        self.open_table(TOPICS);
        self.open_table(TOPIC_AGENTS);
        self.open_table(WORKDIRS);
    }

    fn create_topic(
        &mut self,
        now: UnixMillis,
        display_name: Option<String>,
        status: TopicStatus,
    ) -> TopicId {
        let topic_id = TopicId(next_counter(self, CounterKey::LAST_TOPIC_ID));
        let topic = TopicRecord {
            display_name,
            created_at: now,
            updated_at: now,
            status,
        };
        self.open_table(TOPICS)
            .insert(&topic_id, SenValue::borrowed(&topic));
        topic_id
    }

    fn create_agent(
        &mut self,
        now: UnixMillis,
        topic_id: TopicId,
        display_name: Option<String>,
        working_directory: PathBuf,
        prompt_cache_key: PromptCacheKey,
        config: InferenceProtectedConfig,
    ) -> (AgentId, AgentEventPos) {
        let agent_id = AgentId(next_counter(self, CounterKey::LAST_AGENT_ID));
        let lineage_id = AgentLineageId(next_counter(self, CounterKey::LAST_LINEAGE_ID));
        self.open_table(LINEAGE_PARENTS);
        let agent = AgentRecord {
            display_name,
            working_directory,
            created_at: now,
            updated_at: now,
            current_lineage: lineage_id,
            parent_agent: None,
            prompt_cache_key,
            config,
        };
        self.open_table(AGENTS)
            .insert(&agent_id, SenValue::borrowed(&agent));
        self.open_table(TOPIC_AGENTS)
            .insert(&TopicAgentKey::new(topic_id, agent_id), &());
        (agent_id, AgentEventPos::root(lineage_id))
    }

    fn upsert_workdir(&mut self, now: UnixMillis, path: &str, name: String) {
        let mut workdirs = self.open_table(WORKDIRS);
        let created_at = workdirs
            .get(&path.to_owned())
            .map(|record| record.value().into_owned().created_at)
            .unwrap_or(now);
        workdirs.insert(
            &path.to_owned(),
            SenValue::borrowed(&WorkdirRecord { name, created_at }),
        );
    }

    fn remove_workdir(&mut self, path: &str) {
        self.open_table(WORKDIRS).remove(&path.to_owned());
    }

    fn append_agent_event(&mut self, at: AgentEventPos, event: &AgentEvent<'_>) -> AgentEventPos {
        self.open_table(AGENT_EVENTS)
            .insert(&at, SenValue::borrowed(event));
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
