//! Small, append-only reports about friction encountered while using Rho.

use std::sync::Arc;

use futures::future::BoxFuture;
use redb::TableDefinition;
use rho_agent_tools::FutureTool;
use rho_core::{
    AgentId, ToolCall, ToolName, ToolOutput, ToolOutputStatus, ToolSpec, ToolType, UnixMs,
};
use rho_db::{RhoDb, Sen, SenValue};
use senax_encoder::{Decode, Encode};
use serde::Deserialize;
use serde_json::json;

const PAPERCUTS: TableDefinition<u64, Sen<Papercut>> = TableDefinition::new("papercuts");
const MAX_DESCRIPTION_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Encode, Decode)]
struct Papercut {
    agent_id: AgentId,
    created_at: UnixMs,
    description: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    description: String,
}

#[derive(Clone)]
pub(crate) struct PapercutTool {
    pub db: RhoDb,
    pub agent_id: AgentId,
}

impl PapercutTool {
    pub(crate) fn spec() -> ToolSpec {
        ToolSpec {
            name: ToolName::try_from("papercut").expect("static tool name"),
            tool_type: ToolType::Function,
            description: "Record a concrete Rho bug, confusing behavior, or workflow friction for later improvement. Describe what happened and what you expected, including reproduction details when useful. This only saves a report in the local database; it does not notify anyone or start work.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "description": {
                        "type": "string",
                        "description": "The papercut and enough context to understand it (maximum 16 KiB)."
                    }
                },
                "required": ["description"],
                "additionalProperties": false
            }),
            format: None,
        }
    }

    async fn record(&self, arguments: &str) -> anyhow::Result<u64> {
        let args: Args = serde_json::from_str(arguments)?;
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

impl FutureTool for PapercutTool {
    fn spec(&self) -> ToolSpec {
        Self::spec()
    }

    fn call(&self, call: ToolCall) -> BoxFuture<'static, ToolOutput> {
        let tool = self.clone();
        Box::pin(async move {
            let (output, status) = match tool.record(&call.arguments).await {
                Ok(id) => (format!("Saved papercut #{id}."), ToolOutputStatus::Success),
                Err(error) => (error.to_string(), ToolOutputStatus::Error),
            };
            ToolOutput {
                output: Arc::new(output),
                full_output: None,
                images: Arc::new(Vec::new()),
                status,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use rho_core::{AgentIdDomain, ToolCallId};

    use super::*;

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
                tool.record(r#"{"description":"Unicode: café"}"#),
                tool.record(r#"{"description":"Second report"}"#),
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
        assert_eq!(
            tool(db)
                .record(r#"{"description":"Third report"}"#)
                .await
                .unwrap(),
            3
        );
    }

    #[tokio::test]
    async fn tool_reports_validation_errors_and_confirms_committed_writes() {
        let temp = tempfile::tempdir().unwrap();
        let tool = tool(RhoDb::open(temp.path().join("rho.redb")));
        for arguments in [
            "{}".to_owned(),
            r#"{"description":"  "}"#.to_owned(),
            r#"{"description":"ok","unknown":true}"#.to_owned(),
            json!({"description": "é".repeat(MAX_DESCRIPTION_BYTES)}).to_string(),
            r#"{"description":"A useful report"}"#.to_owned(),
        ] {
            let valid = arguments.contains("A useful report");
            let output = tool
                .call(ToolCall {
                    id: ToolCallId::try_from("papercut-test").unwrap(),
                    name: PapercutTool::spec().name,
                    tool_type: ToolType::Function,
                    arguments,
                })
                .await;
            assert_eq!(
                output.status,
                if valid {
                    ToolOutputStatus::Success
                } else {
                    ToolOutputStatus::Error
                }
            );
            if valid {
                assert_eq!(output.output.as_str(), "Saved papercut #1.");
                assert_eq!(tool.db.read().open_table(PAPERCUTS).iter().count(), 1);
            }
        }
    }
}
