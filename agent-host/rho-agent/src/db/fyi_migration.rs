//! One-time rewrite of every aside send: its kind was stored as `Other`
//! and is `Fyi` now, which `SendKind` reads either way until this has run.
//! Rows are rewritten in place: the journal points at positions. Remove
//! with `SendKind`'s legacy decode once the store has moved on.

use rho_agent_types::SendKind;
use rho_db::{SenValue, WriteTxn};

use super::AGENT_LOG;
use crate::AgentEvent;
use crate::entry::Entry;

pub(super) fn migrate(write: &mut WriteTxn) {
    let rewrites = write
        .open_table(AGENT_LOG)
        .iter()
        .filter_map(|(key, row)| {
            let event = row.value().into_owned();
            matches!(
                event,
                AgentEvent::Entry(Entry::Sent {
                    kind: SendKind::Fyi,
                    ..
                })
            )
            .then(|| (key.value(), event))
        })
        .collect::<Vec<_>>();
    eprintln!("rho-agent: rewriting {} fyi sends", rewrites.len());
    let mut log = write.open_table(AGENT_LOG);
    for (key, event) in rewrites {
        log.insert(&key, SenValue::borrowed(&event));
    }
}
