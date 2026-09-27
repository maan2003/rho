//! Derived native replay position and recovery state. The agent log remains
//! authoritative.

use redb::TableDefinition;
use rho_agent_types::AgentId;
use rho_db::{Sen, SenValue, WriteTxn};
use senax_encoder::{Decode, Encode};

use super::{AGENT_LOG, AgentEvent, AgentEventPos, agent_range, rows, visible_rows};
use crate::entry::{CompactionState, Entry, Notice};

pub(super) const NATIVE_CURSORS: TableDefinition<AgentId, Sen<NativeCursor>> =
    TableDefinition::new("agent_native_cursors");

/// A fixed prefix of one log. Both ends are positions, not row counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct ContextBoundary {
    pub from: AgentEventPos,
    pub through: AgentEventPos,
}

#[derive(Clone, Debug, Default, Encode, Decode)]
pub struct NativeRecovery {
    pub archived: bool,
    pub awaiting: bool,
    pub woken: bool,
    pub compaction: CompactionState,
}

#[derive(Clone, Debug, Default, Encode, Decode)]
pub(super) struct NativeCursor {
    pub from: AgentEventPos,
    /// The current response's preceding native request; imported attempts do
    /// not establish a replay boundary.
    request: Option<AgentEventPos>,
    pub recovery: NativeRecovery,
}

impl NativeCursor {
    fn observe(&mut self, pos: AgentEventPos, event: &AgentEvent<'_>) {
        let AgentEvent::Entry(entry) = event else {
            return;
        };
        self.recovery.compaction.observe(entry);
        match entry {
            Entry::RequestSent { imported, .. } => {
                self.request = imported.is_none().then_some(pos);
                self.recovery.woken = true;
            }
            Entry::Step { carry, .. } => {
                if carry.has_compaction() {
                    if let Some(request) = self.request {
                        // A Received between the request and Step must survive:
                        // it may not have been delivered in that request.
                        self.from = request.next();
                    }
                }
                self.request = None;
            }
            Entry::Awaiting { since, .. } => self.recovery.awaiting = since.is_some(),
            Entry::Notice {
                notice: Notice::Archived,
                ..
            } => self.recovery.archived = true,
            Entry::Notice {
                notice: Notice::FreshNotebook,
                ..
            } => self.recovery.archived = false,
            _ => {}
        }
    }
}

pub(super) fn append(
    write: &mut WriteTxn,
    agent: AgentId,
    pos: AgentEventPos,
    event: &AgentEvent<'_>,
) {
    let mut cursor = write
        .open_table(NATIVE_CURSORS)
        .get(&agent)
        .map(|row| row.value().into_owned())
        .unwrap_or_default();
    if matches!(event, AgentEvent::Rewound { .. }) {
        cursor = rebuild(write, agent);
    } else {
        cursor.observe(pos, event);
    }
    write
        .open_table(NATIVE_CURSORS)
        .insert(&agent, SenValue::borrowed(&cursor));
}

pub(super) fn rebuild(write: &mut WriteTxn, agent: AgentId) -> NativeCursor {
    let log = write.open_table(AGENT_LOG);
    let (_, visible) = visible_rows(rows(log.range(agent_range(agent))));
    let mut cursor = NativeCursor::default();
    for (pos, event) in visible {
        cursor.observe(pos, &event);
    }
    cursor
}
