//! Raw redb schema for persisted agents.

use redb::{TableDefinition, Value as _};
use redb_derive::{Key, Value as RedbValue};
use rho_core::ContextBlock;
use rho_db::Sen;
use rho_inference::PromptCacheKey;
use senax_encoder::{Decode, Encode};

pub const COUNTERS: TableDefinition<CounterKey, u64> = TableDefinition::new("counters");
pub const LINEAGE_PARENTS: TableDefinition<AgentLineageId, AgentTimelineRef> =
    TableDefinition::new("lineage_parents");
pub const AGENT_TIMELINE: TableDefinition<AgentTimelineRef, Sen<AgentTimelineEntry>> =
    TableDefinition::new("agent_timeline");
pub const AGENTS: TableDefinition<AgentId, Sen<AgentRecord>> = TableDefinition::new("agents");

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue)]
pub struct CounterKey(pub u8);

impl CounterKey {
    pub const SCHEMA_VERSION: Self = Self(0);
    pub const LAST_AGENT_ID: Self = Self(1);
    pub const LAST_LINEAGE_ID: Self = Self(2);
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Key, RedbValue, Encode, Decode,
)]
pub struct AgentId(pub u64);

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Key, RedbValue, Encode, Decode,
)]
pub struct AgentLineageId(pub u64);

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Key, RedbValue, Encode, Decode,
)]
pub struct AgentTimelineRef {
    pub lineage_id: AgentLineageId,
    pub seq: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AgentRecord {
    pub display_name: Option<String>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub current_lineage: AgentLineageId,
    pub parent_agent: Option<AgentId>,
    pub provider_state: AgentProviderState,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AgentProviderState {
    pub prompt_cache_key: PromptCacheKey,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct AgentTimelineEntry {
    pub created_at_ms: u64,
    pub context_block: ContextBlock,
}

#[cfg(test)]
mod tests {
    use rho_db::RhoDb;

    use super::*;

    #[tokio::test]
    async fn agent_timeline_refs_sort_by_lineage_then_seq() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));

        let mut write = db.write().await;
        {
            let mut timeline = write.open_table(AGENT_TIMELINE);
            for seq in [2, 0, 1] {
                timeline.insert(
                    &AgentTimelineRef {
                        lineage_id: AgentLineageId(7),
                        seq,
                    },
                    Sen(AgentTimelineEntry {
                        created_at_ms: u64::from(seq),
                        context_block: ContextBlock::UserMessage {
                            content: Vec::new(),
                        },
                    }),
                );
            }
        }
        write.commit();

        let read = db.read();
        let timeline = read.open_table(AGENT_TIMELINE);
        let seqs = timeline
            .range(
                AgentTimelineRef {
                    lineage_id: AgentLineageId(7),
                    seq: 0,
                }..=AgentTimelineRef {
                    lineage_id: AgentLineageId(7),
                    seq: u32::MAX,
                },
            )
            .map(|(key, _)| key.value().seq)
            .collect::<Vec<_>>();

        assert_eq!(seqs, [0, 1, 2]);
    }
}
