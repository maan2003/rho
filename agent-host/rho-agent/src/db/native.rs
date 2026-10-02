//! Derived native replay position and recovery state. The agent log remains
//! authoritative.

use redb::TableDefinition;
use rho_agent_types::AgentId;
use rho_db::{Sen, SenValue, WriteTxn};
use senax_encoder::{Decode, Encode};

use super::{AGENT_LOG, AgentEvent, AgentEventPos, agent_range, rows, visible_rows};
use crate::entry::{Entry, Notice};
use crate::log::NativeRecovery;

pub(super) const NATIVE_CURSORS: TableDefinition<AgentId, Sen<NativeCursor>> =
    TableDefinition::new("agent_native_cursors");

#[derive(Clone, Debug, Default, Encode, Decode)]
pub(super) struct NativeCursor {
    pub from: AgentEventPos,
    /// The preceding request can establish a compaction boundary only when
    /// no older Received row is still pending.
    request: Option<AgentEventPos>,
    /// Number of Received rows awaiting delivery or acknowledgment.
    pending: usize,
    pub recovery: NativeRecovery,
}

impl NativeCursor {
    fn observe(&mut self, pos: AgentEventPos, event: &AgentEvent<'_>) {
        let AgentEvent::Entry(entry) = event else {
            return;
        };
        self.recovery.compaction.observe(entry);
        match entry {
            Entry::Received { .. } => self.pending += 1,
            Entry::RequestSent { report, .. } => {
                let consumed = report.messages.len() + report.acknowledged.len();
                assert!(
                    consumed <= self.pending,
                    "send consumed more messages than pending"
                );
                self.pending -= consumed;
                self.request = (self.pending == 0).then_some(pos);
                self.recovery.woken = true;
            }
            Entry::Step { carry, .. } => {
                if carry.has_compaction()
                    && let Some(request) = self.request
                {
                    // A Received between the request and Step must survive:
                    // it may not have been delivered in that request.
                    self.from = request.next();
                }
                self.request = None;
            }
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
        .get(agent)
        .map(|row| row.value().into_owned())
        .unwrap_or_default();
    if matches!(event, AgentEvent::Rewound { .. }) {
        cursor = rebuild(write, agent);
    } else {
        cursor.observe(pos, event);
    }
    write
        .open_table(NATIVE_CURSORS)
        .insert(agent, SenValue::borrowed(&cursor));
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
