//! One-time rewrite of `Awaiting { since }` rows into a wait's start
//! (`since` set) and stop (unset). Remove with `Entry::LegacyAwaiting` once
//! the store has moved on.

use rho_db::{SenValue, WriteTxn};

use super::AGENT_LOG;
use crate::AgentEvent;
use crate::entry::Entry;

pub(super) fn migrate(write: &mut WriteTxn) {
    let rewrites = write
        .open_table(AGENT_LOG)
        .iter()
        .filter_map(|(key, row)| match row.value().into_owned() {
            AgentEvent::Entry(Entry::LegacyAwaiting { at, since }) => Some((
                key.value(),
                AgentEvent::Entry(match since {
                    Some(_) => Entry::AwaitingHuman { at },
                    None => Entry::StoppedAwaitingHuman { at },
                }),
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    eprintln!("rho-agent: rewriting {} wait rows", rewrites.len());
    let mut log = write.open_table(AGENT_LOG);
    for (key, event) in rewrites {
        log.insert(&key, SenValue::borrowed(&event));
    }
}
