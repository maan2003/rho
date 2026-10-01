//! One-time rewrite of the rows the dealer no longer reads: a status
//! becomes a send of kind status, a turn that ended in error becomes the
//! notice that the agent stopped, and turn edges, waits and wants retire.
//! Rows are rewritten in place: the journal points at positions. Remove
//! with the variants it reads once the store has moved on.

use rho_agent_types::{SendKind, TurnEdge, TurnOutcome};
use rho_db::{SenValue, WriteTxn};

use super::AGENT_LOG;
use crate::AgentEvent;
use crate::entry::{Entry, MessageId, Notice, Party};

/// What the host wrote when it found a turn its last run left open; the
/// restart notice says this now.
const ORPHANED: &str = "the agent host stopped during this turn";

pub(super) fn migrate(write: &mut WriteTxn) {
    let rewrites = write
        .open_table(AGENT_LOG)
        .iter()
        .filter_map(|(key, row)| Some((key.value(), rewrite(row.value().into_owned())?)))
        .collect::<Vec<_>>();
    eprintln!("rho-agent: rewriting {} conversation rows", rewrites.len());
    let mut log = write.open_table(AGENT_LOG);
    for (key, event) in rewrites {
        log.insert(&key, SenValue::borrowed(&event));
    }
}

fn rewrite(event: AgentEvent<'_>) -> Option<AgentEvent<'static>> {
    Some(match event {
        AgentEvent::Entry(Entry::Status { at, text }) if !text.is_empty() => {
            AgentEvent::Entry(Entry::Sent {
                at,
                id: MessageId::new(),
                to: Party::Human,
                text,
                kind: SendKind::Status,
            })
        }
        AgentEvent::Turn {
            edge: TurnEdge::Ended(TurnOutcome::Errored { message }),
            at,
        } if message != ORPHANED => AgentEvent::Entry(Entry::Notice {
            at,
            notice: Notice::Stopped(message),
        }),
        AgentEvent::Entry(
            Entry::Status { at, .. }
            | Entry::AwaitingHuman { at }
            | Entry::StoppedAwaitingHuman { at }
            | Entry::LegacyAwaiting { at, .. },
        )
        | AgentEvent::Turn { at, .. }
        | AgentEvent::Wants { at, .. } => AgentEvent::Retired { at },
        _ => return None,
    })
}
