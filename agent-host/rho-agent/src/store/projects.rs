//! A project's index on its host. Its identity is its anchor, a root of
//! the chat; its name is that root's thread name; its settings are the
//! newest [`ChatEvent::Project`](rho_agent_types::ChatEvent::Project) for
//! it in the log, indexed here in the commit that appends it. Which agent
//! stands for a project in `one` mode is the host's own routing, never on
//! the wire.

use redb::TableDefinition;
use rho_agent_types::{AgentId, MessageId, ProjectConfig, ProjectId, Revision};
use rho_db::{ReadTxn, Sen, SenValue, WriteTxn};

use super::MailRead as _;

/// The newest settings of each project, by its anchor.
const PROJECTS: TableDefinition<u64, Sen<ProjectConfig>> = TableDefinition::new("chat_projects");
/// The agent that receives each idea of a project in `one` mode.
const STANDING: TableDefinition<u64, AgentId> = TableDefinition::new("project_standing_agents");
// Set in the same transaction as an existing agent's idea, or after a new
// agent or a failure stop is recorded. A lost receipt cannot start it twice.
const STARTED: TableDefinition<u64, ()> = TableDefinition::new("mail_project_starts");

pub trait ProjectRead {
    /// A live project's settings; `None` for any other message.
    fn project(&self, project: ProjectId) -> Option<ProjectConfig>;
    fn projects(&self) -> Vec<(ProjectId, ProjectConfig)>;
    fn standing_agent(&self, project: ProjectId) -> Option<AgentId>;
    fn project_start_complete(&self, idea: MessageId) -> bool;
}

impl ProjectRead for ReadTxn {
    fn project(&self, ProjectId(anchor): ProjectId) -> Option<ProjectConfig> {
        let config = self
            .open_table(PROJECTS)
            .get(anchor.0.0)?
            .value()
            .into_owned();
        self.message(anchor)
            .is_some_and(|anchor| anchor.parent.is_none() && anchor.revision != Revision::Deleted)
            .then_some(config)
    }

    fn projects(&self) -> Vec<(ProjectId, ProjectConfig)> {
        let anchors: Vec<u64> = self
            .open_table(PROJECTS)
            .iter()
            .map(|(id, _)| id.value())
            .collect();
        anchors
            .into_iter()
            .filter_map(|id| {
                let project = ProjectId(MessageId(rho_agent_types::ChatSeq(id)));
                Some((project, self.project(project)?))
            })
            .collect()
    }

    fn standing_agent(&self, ProjectId(anchor): ProjectId) -> Option<AgentId> {
        self.open_table(STANDING)
            .get(anchor.0.0)
            .map(|agent| agent.value())
    }

    fn project_start_complete(&self, idea: MessageId) -> bool {
        self.open_table(STARTED).get(idea.0.0).is_some()
    }
}

pub trait ProjectWrite {
    fn init_project_tables(&mut self);
    /// The settings the index holds for a project, read in this write.
    fn held_project(&mut self, project: ProjectId) -> Option<ProjectConfig>;
    /// Folds a logged [`rho_agent_types::ChatEvent::Project`] into the index.
    fn index_project(&mut self, project: ProjectId, config: &ProjectConfig);
    fn set_standing_agent(&mut self, project: ProjectId, agent: AgentId);
    fn clear_standing_agent(&mut self, project: ProjectId);
    fn complete_project_start(&mut self, idea: MessageId);
}

impl ProjectWrite for WriteTxn {
    fn init_project_tables(&mut self) {
        self.open_table(PROJECTS);
        self.open_table(STANDING);
        self.open_table(STARTED);
    }

    fn held_project(&mut self, ProjectId(anchor): ProjectId) -> Option<ProjectConfig> {
        self.open_table(PROJECTS)
            .get(anchor.0.0)
            .map(|row| row.value().into_owned())
    }

    fn index_project(&mut self, ProjectId(anchor): ProjectId, config: &ProjectConfig) {
        self.open_table(PROJECTS)
            .insert(anchor.0.0, SenValue::borrowed(config));
    }

    fn set_standing_agent(&mut self, ProjectId(anchor): ProjectId, agent: AgentId) {
        self.open_table(STANDING).insert(anchor.0.0, agent);
    }

    fn clear_standing_agent(&mut self, ProjectId(anchor): ProjectId) {
        self.open_table(STANDING).remove(anchor.0.0);
    }

    fn complete_project_start(&mut self, idea: MessageId) {
        self.open_table(STARTED).insert(idea.0.0, ());
    }
}
