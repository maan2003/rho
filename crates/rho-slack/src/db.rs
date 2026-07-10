//! Slack-owned persisted state stored in rho's local redb database.
//!
//! Table names preserve the original generic `platform_*` names so existing
//! Slack thread mappings survive the move out of `rho-agent`.

use camino::Utf8PathBuf;
use redb::TableDefinition;
use rho_agent::db::AgentId;
use rho_db::{ReadTxn, Sen, SenValue, WriteTxn};
use senax_encoder::{Decode, Encode};

/// Slack thread session key (`slack:<channel>:<thread_ts>`) -> the coordinator
/// agent carrying that conversation.
const PLATFORM_SESSIONS: TableDefinition<String, AgentId> =
    TableDefinition::new("platform_sessions");
/// Slack configuration. Kept under the old generic table prefix for
/// continuity with pre-split platform storage.
const PLATFORM_CONFIGS: TableDefinition<String, Sen<SlackConfigRecord>> =
    TableDefinition::new("platform_configs");

const SLACK_CONFIG_KEY: &str = "slack";

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct SlackConfigRecord {
    pub coordinator_repo: Utf8PathBuf,
}

pub trait SlackReadTxnExt {
    fn get_slack_session(&self, session_key: &str) -> Option<AgentId>;
    fn list_slack_sessions(&self) -> Vec<(String, AgentId)>;
    fn get_slack_config(&self) -> Option<SlackConfigRecord>;
}

pub trait SlackWriteTxnExt {
    fn init_slack_tables(&mut self);
    fn set_slack_session(&mut self, session_key: &str, agent_id: AgentId);
    fn set_slack_config(&mut self, config: SlackConfigRecord);
}

impl SlackReadTxnExt for ReadTxn {
    fn get_slack_session(&self, session_key: &str) -> Option<AgentId> {
        self.open_table(PLATFORM_SESSIONS)
            .get(&session_key.to_owned())
            .map(|value| value.value())
    }

    fn list_slack_sessions(&self) -> Vec<(String, AgentId)> {
        self.open_table(PLATFORM_SESSIONS)
            .iter()
            .map(|(key, value)| (key.value(), value.value()))
            .collect()
    }

    fn get_slack_config(&self) -> Option<SlackConfigRecord> {
        self.open_table(PLATFORM_CONFIGS)
            .get(&SLACK_CONFIG_KEY.to_owned())
            .map(|value| value.value().into_owned())
    }
}

impl SlackWriteTxnExt for WriteTxn {
    fn init_slack_tables(&mut self) {
        self.open_table(PLATFORM_SESSIONS);
        self.open_table(PLATFORM_CONFIGS);
    }

    fn set_slack_session(&mut self, session_key: &str, agent_id: AgentId) {
        self.open_table(PLATFORM_SESSIONS)
            .insert(&session_key.to_owned(), &agent_id);
    }

    fn set_slack_config(&mut self, config: SlackConfigRecord) {
        self.open_table(PLATFORM_CONFIGS)
            .insert(&SLACK_CONFIG_KEY.to_owned(), SenValue::borrowed(&config));
    }
}

#[cfg(test)]
mod tests {
    use rho_agent::db::{AgentId, AgentIdDomain};
    use rho_db::RhoDb;

    use super::*;

    #[tokio::test]
    async fn slack_config_and_sessions_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let db = RhoDb::open(temp.path().join("rho.redb"));

        let agent_id = AgentId::from_counter(1, &AgentIdDomain(0)).unwrap();
        let mut write = db.write().await;
        write.init_slack_tables();
        write.set_slack_config(SlackConfigRecord {
            coordinator_repo: "/home/user/src/coordinator".into(),
        });
        write.set_slack_session("slack:C123:1700000000.000001", agent_id);
        write.commit();

        let read = db.read();
        assert_eq!(
            read.get_slack_config(),
            Some(SlackConfigRecord {
                coordinator_repo: "/home/user/src/coordinator".into(),
            })
        );
        assert_eq!(
            read.get_slack_session("slack:C123:1700000000.000001"),
            Some(agent_id)
        );
        assert_eq!(
            read.list_slack_sessions(),
            vec![("slack:C123:1700000000.000001".to_owned(), agent_id)]
        );
    }
}
