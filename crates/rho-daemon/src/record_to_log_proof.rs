//! The store half of the record→log proof: how many agents carry a name
//! the daemon is about to stop keeping, and how many of those the desk
//! store already names. Point `RHO_PROOF_DB` at a copy of the user's
//! store, run it after the rho-agent proof has migrated that copy.
//! Deleted with the migration.

use rho_agent::db::AgentReadTxnExt;
use rho_db::RhoDb;
use rho_desk::cells::{Id, Property};

use crate::desk_cells::DeskCellStore;

#[tokio::test]
#[ignore = "needs a copy of a real daemon store in RHO_PROOF_DB"]
async fn counts_spawn_names_against_store_name_facts() {
    let path = std::env::var("RHO_PROOF_DB").expect("RHO_PROOF_DB must name a copy");
    let db = RhoDb::open(&path);
    let named = db
        .read()
        .list_agents()
        .into_iter()
        .filter_map(|(agent_id, head)| head.config.spawn_name.map(|name| (agent_id, name)))
        .collect::<Vec<_>>();

    let store = DeskCellStore::new(db.clone()).await.unwrap();
    let snapshot = store.sync_since(&rho_desk::cells::Version::new()).unwrap();
    let store_named = snapshot
        .cells
        .iter()
        .filter_map(|cell| match (&cell.id, &cell.property) {
            (Id::Agent(agent_id), Property::Name(name)) => Some((*agent_id, name.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    let with_fact = named
        .iter()
        .filter(|(agent_id, _)| store_named.iter().any(|(named_id, _)| named_id == agent_id))
        .count();

    eprintln!(
        "proof: {} agents, {} with a spawn name, {} of those with a store Name fact \
         ({} agent Name facts in the store)",
        db.read().list_agents().len(),
        named.len(),
        with_fact,
        store_named.len()
    );
}
