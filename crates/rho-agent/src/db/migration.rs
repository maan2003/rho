//! Temporary 7a2ecf91 format hop. Remove after active databases have rewritten
//! retired model bindings and role names into the current role matrix.

use std::collections::HashMap;

use rho_db::{SenValue, WriteTxn};

use super::{AGENT_LOG, AgentId, SessionBinding};
use crate::AgentEvent;

/// Re-encode every log row so compatibility decoders materialize current
/// variants. Role text was persisted separately from its binding, so derive
/// the migrated role from the effective binding to keep old advisor and
/// engineer configurations coherent.
pub(super) fn migrate(write: &mut WriteTxn) {
    use std::ops::Bound::{Excluded, Unbounded};

    let mut binding_by_agent = HashMap::<AgentId, SessionBinding>::new();
    let mut after = None;
    loop {
        let rows = {
            let log = write.open_table(AGENT_LOG);
            log.range((after.map_or(Unbounded, Excluded), Unbounded))
                .take(128)
                .map(|(key, value)| (key.value(), value.value().into_owned()))
                .collect::<Vec<_>>()
        };
        if rows.is_empty() {
            break;
        }
        let mut log = write.open_table(AGENT_LOG);
        for (key, mut event) in rows {
            match &mut event {
                AgentEvent::Created { role, binding, .. } => {
                    *role = binding.agent_role();
                    binding_by_agent.insert(key.0, *binding);
                }
                AgentEvent::RoleChanged { role, binding, .. } => {
                    if let Some(binding) = binding {
                        binding_by_agent.insert(key.0, *binding);
                    }
                    if let Some(binding) = binding_by_agent.get(&key.0) {
                        *role = binding.agent_role();
                    }
                }
                _ => {}
            }
            log.insert(&key, SenValue::borrowed(&event));
            after = Some(key);
        }
    }
}
