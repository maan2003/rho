//! Durable project settings. Names and identity still belong to the roots of
//! the mail forest; changing settings never rewrites a message or an agent.

use redb::{TableDefinition, TableHandle as _};
use rho_agent_types::{AgentId, MessageId, ProjectConfig, ProjectId, ProjectMode, ProjectPlace};
use rho_db::{ReadTxn, Sen, SenValue, WriteTxn};
use senax_encoder::{Decode, Encode};

use super::MailRead as _;

const PROJECTS: TableDefinition<u64, Sen<ProjectSettings>> = TableDefinition::new("mail_projects");
// Set in the same transaction as an existing agent's idea, or after a new
// agent or a failure stop is recorded. A lost receipt cannot start it twice.
const STARTED: TableDefinition<u64, ()> = TableDefinition::new("mail_project_starts");

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ProjectSettings {
    pub place: ProjectPlace,
    pub routing: Routing,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Routing {
    One { agent: Option<AgentId> },
    PerIdea,
}

impl ProjectSettings {
    pub fn new(config: ProjectConfig) -> Self {
        Self {
            place: config.place,
            routing: match config.mode {
                ProjectMode::One => Routing::One { agent: None },
                ProjectMode::PerIdea => Routing::PerIdea,
            },
        }
    }

    pub fn config(&self) -> ProjectConfig {
        ProjectConfig {
            place: self.place.clone(),
            mode: match self.routing {
                Routing::One { .. } => ProjectMode::One,
                Routing::PerIdea => ProjectMode::PerIdea,
            },
        }
    }
}

pub trait ProjectRead {
    fn has_project_settings(&self) -> bool;
    fn is_project(&self, project: ProjectId) -> bool;
    fn project_settings(&self, project: ProjectId) -> Option<ProjectSettings>;
    fn configured_projects(&self) -> Vec<(ProjectId, ProjectConfig)>;
    fn project_start_complete(&self, idea: MessageId) -> bool;
}

impl ProjectRead for ReadTxn {
    fn has_project_settings(&self) -> bool {
        self.has_table(PROJECTS.name())
    }

    fn is_project(&self, ProjectId(root): ProjectId) -> bool {
        self.message(root).is_some_and(|message| {
            message.parent.is_none()
                && message.mentions.is_empty()
                && message.revision != rho_agent_types::Revision::Deleted
        })
    }

    fn project_settings(&self, ProjectId(root): ProjectId) -> Option<ProjectSettings> {
        self.open_table(PROJECTS)
            .get(root.0.0)
            .map(|row| row.value().into_owned())
    }

    fn configured_projects(&self) -> Vec<(ProjectId, ProjectConfig)> {
        self.open_table(PROJECTS)
            .iter()
            .map(|(id, settings)| {
                (
                    ProjectId(MessageId(rho_agent_types::ChatSeq(id.value()))),
                    settings.value().into_owned().config(),
                )
            })
            .collect()
    }

    fn project_start_complete(&self, idea: MessageId) -> bool {
        self.open_table(STARTED).get(idea.0.0).is_some()
    }
}

pub trait ProjectWrite {
    fn init_project_tables(&mut self);
    fn set_project_settings(&mut self, project: ProjectId, settings: &ProjectSettings);
    fn complete_project_start(&mut self, idea: MessageId);
}

impl ProjectWrite for WriteTxn {
    fn init_project_tables(&mut self) {
        self.open_table(PROJECTS);
        self.open_table(STARTED);
    }

    fn set_project_settings(&mut self, ProjectId(root): ProjectId, settings: &ProjectSettings) {
        self.open_table(PROJECTS)
            .insert(root.0.0, SenValue::borrowed(settings));
    }

    fn complete_project_start(&mut self, idea: MessageId) {
        self.open_table(STARTED).insert(idea.0.0, ());
    }
}
