//! One-off (13 Sep), the daemon's half of `rho-agent`'s move of agents
//! from before worksets: the store's migration gives each such agent an
//! empty workset by id, and the directory that id names is made here,
//! where the state root is known. Remove with that migration.

use std::sync::Arc;

use rho_agent::AgentEvent;
use rho_agent::db::{AgentEventPos, AgentReadTxnExt as _};
use rho_db::RhoDb;
use rho_fs_view::Worksets;

/// Makes the directory of every agent's workset that has none. Returns
/// how many were made; a second run makes none.
pub(crate) async fn create_missing_worksets(
    db: &RhoDb,
    worksets: &Arc<Worksets>,
) -> anyhow::Result<usize> {
    // One row per agent, not a fold of every log: the place is in the
    // creation and a migration is the only thing that changes it.
    let worksets_named: Vec<String> = {
        let read = db.read();
        read.list_agent_ids()
            .into_iter()
            .filter_map(
                |agent_id| match read.agent_event(agent_id, AgentEventPos::ZERO)? {
                    AgentEvent::Created { place, .. } => Some(place.workset),
                    _ => None,
                },
            )
            .collect()
    };
    let mut made = 0;
    for workset in worksets_named {
        if worksets.ensure_workset(&workset).await? {
            made += 1;
        }
    }
    Ok(made)
}

#[cfg(test)]
mod tests {
    use rho_agent::AgentEvent;
    use rho_agent::db::{
        AgentRole, AgentRuntime, AgentSpawnedBy, AgentWriteTxnExt as _, SessionBinding,
    };
    use rho_core::UnixMs;
    use rho_fs_view::{Place, WorksetMode};

    use super::*;

    #[tokio::test]
    async fn a_place_without_a_directory_gets_one_once() {
        let root = tempfile::tempdir().unwrap();
        let db = RhoDb::open(root.path().join("rho.redb"));
        {
            let mut write = db.write().await;
            write.init_agent_tables();
            write.commit();
        }
        let worksets = Worksets::open(
            root.path().join("state"),
            rho_fs_view::UserEnvironment::new(Default::default()),
            Default::default(),
            rho_fs_view::StoreService::None,
        )
        .await
        .unwrap();
        {
            let mut write = db.write().await;
            let agent_id = write.alloc_agent_id();
            write.append_agent_event(
                agent_id,
                &AgentEvent::Created {
                    role: AgentRole::default(),
                    binding: SessionBinding::ClaudeFable {
                        effort: rho_agent::db::ClaudeEffort::High,
                    },
                    runtime: AgentRuntime::Claude {
                        session_id: uuid::Uuid::new_v4(),
                    },
                    place: Place {
                        workset: "0123456789ab".into(),
                        cwd: "/src".into(),
                        mode: WorksetMode::Exposed,
                        origin: None,
                    },
                    spawned_by: AgentSpawnedBy::Direct,
                    spawn_name: None,
                    created_at: UnixMs(1),
                    parent: None,
                },
            );
            write.commit();
        }
        assert!(worksets.open_workset("0123456789ab").await.is_err());
        assert_eq!(create_missing_worksets(&db, &worksets).await.unwrap(), 1);
        assert!(worksets.open_workset("0123456789ab").await.is_ok());
        assert_eq!(create_missing_worksets(&db, &worksets).await.unwrap(), 0);
    }
}
