//! Small, append-only reports about friction encountered while using Rho.

use redb::TableDefinition;
use rho_agent_types::{AgentId, UnixMs};
use rho_db::{RhoDb, Sen, SenValue};
use senax_encoder::{Decode, Encode};

const PAPERCUTS: TableDefinition<u64, Sen<Papercut>> = TableDefinition::new("papercuts");
const MAX_DESCRIPTION_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Encode, Decode)]
struct Papercut {
    agent_id: AgentId,
    created_at: UnixMs,
    description: String,
}

/// The arguments of the notebook's `papercut()`.
#[derive(Debug, Encode, Decode)]
pub(crate) struct PapercutArgs {
    pub description: String,
}

#[derive(Clone)]
pub(crate) struct PapercutTool {
    pub db: RhoDb,
    pub agent_id: AgentId,
}

impl PapercutTool {
    pub(crate) async fn record(&self, args: PapercutArgs) -> anyhow::Result<u64> {
        anyhow::ensure!(
            !args.description.trim().is_empty(),
            "description must not be empty"
        );
        anyhow::ensure!(
            args.description.len() <= MAX_DESCRIPTION_BYTES,
            "description exceeds 16 KiB"
        );
        let report = Papercut {
            agent_id: self.agent_id,
            created_at: UnixMs::now(),
            description: args.description,
        };
        let mut write = self.db.write().await;
        let id = {
            let mut table = write.open_table(PAPERCUTS);
            let id = table
                .iter()
                .next_back()
                .map_or(1, |(key, _)| key.value() + 1);
            table.insert(id, SenValue::borrowed(&report));
            id
        };
        write.commit();
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use rho_agent_types::AgentIdDomain;

    use super::*;

    fn args(description: &str) -> PapercutArgs {
        PapercutArgs {
            description: description.to_owned(),
        }
    }

    fn tool(db: RhoDb) -> PapercutTool {
        PapercutTool {
            db,
            agent_id: AgentId::from_counter(1, &AgentIdDomain(0)).unwrap(),
        }
    }

    #[tokio::test]
    async fn reports_survive_reopening_and_concurrent_appends() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("rho.redb");
        let agent_id;
        {
            let tool = tool(RhoDb::open(&path));
            agent_id = tool.agent_id;
            let (first, second) = tokio::join!(
                tool.record(args("Unicode: café")),
                tool.record(args("Second report")),
            );
            assert_ne!(first.unwrap(), second.unwrap());
        }
        let db = RhoDb::open(&path);
        let reports: Vec<_> = db
            .read()
            .open_table(PAPERCUTS)
            .iter()
            .map(|(_, value)| value.value().into_owned())
            .collect();
        assert_eq!(reports.len(), 2);
        assert!(reports.iter().all(|report| report.agent_id == agent_id));
        assert!(
            reports
                .iter()
                .any(|report| report.description == "Unicode: café")
        );
        assert_eq!(tool(db).record(args("Third report")).await.unwrap(), 3);
    }

    #[tokio::test]
    async fn validates_descriptions_and_confirms_committed_writes() {
        let temp = tempfile::tempdir().unwrap();
        let tool = tool(RhoDb::open(temp.path().join("rho.redb")));
        for description in ["  ".to_owned(), "é".repeat(MAX_DESCRIPTION_BYTES)] {
            assert!(tool.record(args(&description)).await.is_err());
        }
        assert_eq!(tool.record(args("A useful report")).await.unwrap(), 1);
        assert_eq!(tool.db.read().open_table(PAPERCUTS).iter().count(), 1);
    }
}
