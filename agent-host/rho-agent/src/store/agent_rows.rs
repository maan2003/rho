//! Each agent's rows by position: an index over the workset log, written in
//! the same transaction as the row it names. An agent's history is read from
//! here rather than by walking its whole workset, which in a crowded
//! workset is mostly other agents' rows.

use redb::TableDefinition;
use rho_agent_types::{AgentId, Wpos};
use rho_db::{ReadTxn, WriteTxn};

use super::WORKSET_LOG;
use crate::record::Record;

/// `(agent, wpos)` for every row of the agent in its workset's log.
const AGENT_ROWS: TableDefinition<(AgentId, u64), ()> = TableDefinition::new("workset_agent_rows");

pub(super) fn init(write: &mut WriteTxn) {
    write.open_table(AGENT_ROWS);
}

/// Names a new row under its agent, if it has one.
pub(super) fn index(write: &mut WriteTxn, pos: Wpos, record: &Record<'_>) {
    if let Some(agent) = record.event.agent() {
        write.open_table(AGENT_ROWS).insert((agent, pos.0), ());
    }
}

/// Forgets an agent's rows, with its workset.
pub(super) fn forget(write: &mut WriteTxn, agent: AgentId) {
    for pos in positions_in_write(write, agent) {
        write.open_table(AGENT_ROWS).remove((agent, pos.0));
    }
}

/// Up to `limit` of the agent's positions from `from`, oldest first.
pub(super) fn after(read: &ReadTxn, agent: AgentId, from: Wpos, limit: usize) -> Vec<Wpos> {
    read.open_table(AGENT_ROWS)
        .range((agent, from.0)..=(agent, u64::MAX))
        .take(limit)
        .map(|(key, _)| Wpos(key.value().1))
        .collect()
}

/// Up to `limit` of the agent's positions before `before`, newest first.
pub(super) fn before(read: &ReadTxn, agent: AgentId, before: Wpos, limit: usize) -> Vec<Wpos> {
    let Some(last) = before.0.checked_sub(1) else {
        return Vec::new();
    };
    read.open_table(AGENT_ROWS)
        .range((agent, 0)..=(agent, last))
        .rev()
        .take(limit)
        .map(|(key, _)| Wpos(key.value().1))
        .collect()
}

/// Every position of the agent, oldest first, read inside a write.
pub(super) fn positions_in_write(write: &mut WriteTxn, agent: AgentId) -> Vec<Wpos> {
    write
        .open_table(AGENT_ROWS)
        .range((agent, 0)..=(agent, u64::MAX))
        .map(|(key, _)| Wpos(key.value().1))
        .collect()
}
