//! A project's index on its host. Its identity is its anchor, a root of
//! the chat; its name is that root's thread name; its settings are the
//! newest [`ChatEvent::Project`](rho_agent_types::ChatEvent::Project) for
//! it in the log, indexed here in the commit that appends it. Which agent
//! stands for a project (`one` mode) and whether it is retired are logged
//! the same way ([`ChatEvent::Standing`](rho_agent_types::ChatEvent::Standing),
//! [`ChatEvent::Retired`](rho_agent_types::ChatEvent::Retired)): every
//! device names a standing agent's workset by the project alone, and
//! leaves a retired project off its `to:` line.

use redb::TableDefinition;
use rho_agent_types::{
    AgentId, MessageId, ProjectConfig, ProjectId, ProjectMode, Revision, RuntimeLabel,
};
use rho_db::{ReadTxn, Sen, SenValue, WriteTxn};
use senax_encoder::{Decode, Encode};

use super::MailRead as _;

/// The newest settings of each project, by its anchor.
const PROJECTS: TableDefinition<u64, Sen<ProjectConfig>> = TableDefinition::new("chat_projects");
/// The agent that receives each idea of a project in `one` mode.
const STANDING: TableDefinition<u64, AgentId> = TableDefinition::new("project_standing_agents");
/// Read by the one-hop migration that logs what it holds.
pub(super) const STANDING_TABLE: TableDefinition<u64, AgentId> = STANDING;
/// The projects the user retired.
const RETIRED: TableDefinition<u64, ()> = TableDefinition::new("project_retired");
// Set in the same transaction as an existing agent's idea, or after a new
// agent or a failure stop is recorded. A lost receipt cannot start it twice.
const STARTED: TableDefinition<u64, ()> = TableDefinition::new("mail_project_starts");
/// The `in:` and `on:` lines an idea named, by the idea, written with it.
/// A retry after a refused start asks for the same.
const CHOICES: TableDefinition<u64, Sen<IdeaChoices>> = TableDefinition::new("mail_idea_choices");

/// What an idea's own lines chose over the project's settings; `None` is
/// the project's.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct IdeaChoices {
    pub runtime: Option<RuntimeLabel>,
    pub mode: Option<ProjectMode>,
}

pub trait ProjectRead {
    /// A live project's settings; `None` for any other message.
    fn project(&self, project: ProjectId) -> Option<ProjectConfig>;
    fn projects(&self) -> Vec<(ProjectId, ProjectConfig)>;
    fn standing_agent(&self, project: ProjectId) -> Option<AgentId>;
    /// Every standing agent, by project, as the index holds them.
    fn standing_agents(&self) -> Vec<(ProjectId, AgentId)>;
    fn project_retired(&self, project: ProjectId) -> bool;
    fn project_start_complete(&self, idea: MessageId) -> bool;
    fn idea_choices(&self, idea: MessageId) -> IdeaChoices;
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

    fn standing_agents(&self) -> Vec<(ProjectId, AgentId)> {
        self.open_table(STANDING)
            .iter()
            .map(|(anchor, agent)| {
                let anchor = MessageId(rho_agent_types::ChatSeq(anchor.value()));
                (ProjectId(anchor), agent.value())
            })
            .collect()
    }

    fn project_retired(&self, ProjectId(anchor): ProjectId) -> bool {
        self.open_table(RETIRED).get(anchor.0.0).is_some()
    }

    fn project_start_complete(&self, idea: MessageId) -> bool {
        self.open_table(STARTED).get(idea.0.0).is_some()
    }

    fn idea_choices(&self, idea: MessageId) -> IdeaChoices {
        self.open_table(CHOICES)
            .get(idea.0.0)
            .map(|row| row.value().into_owned())
            .unwrap_or_default()
    }
}

pub trait ProjectWrite {
    fn init_project_tables(&mut self);
    /// The settings the index holds for a project, read in this write.
    fn held_project(&mut self, project: ProjectId) -> Option<ProjectConfig>;
    /// Folds a logged [`rho_agent_types::ChatEvent::Project`] into the index.
    fn index_project(&mut self, project: ProjectId, config: &ProjectConfig);
    /// The standing agent the index holds, read in this write.
    fn held_standing_agent(&mut self, project: ProjectId) -> Option<AgentId>;
    /// Folds a logged [`rho_agent_types::ChatEvent::Standing`] into the index.
    fn index_standing_agent(&mut self, project: ProjectId, agent: Option<AgentId>);
    fn held_retired(&mut self, project: ProjectId) -> bool;
    /// Folds a logged [`rho_agent_types::ChatEvent::Retired`] into the index.
    fn index_retired(&mut self, project: ProjectId);
    fn complete_project_start(&mut self, idea: MessageId);
    fn choose_for_idea(&mut self, idea: MessageId, choices: &IdeaChoices);
}

impl ProjectWrite for WriteTxn {
    fn init_project_tables(&mut self) {
        self.open_table(PROJECTS);
        self.open_table(STANDING);
        self.open_table(RETIRED);
        self.open_table(STARTED);
        self.open_table(CHOICES);
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

    fn held_standing_agent(&mut self, ProjectId(anchor): ProjectId) -> Option<AgentId> {
        self.open_table(STANDING)
            .get(anchor.0.0)
            .map(|agent| agent.value())
    }

    fn index_standing_agent(&mut self, ProjectId(anchor): ProjectId, agent: Option<AgentId>) {
        let mut standing = self.open_table(STANDING);
        match agent {
            Some(agent) => standing.insert(anchor.0.0, agent),
            None => standing.remove(anchor.0.0),
        };
    }

    fn held_retired(&mut self, ProjectId(anchor): ProjectId) -> bool {
        self.open_table(RETIRED).get(anchor.0.0).is_some()
    }

    fn index_retired(&mut self, ProjectId(anchor): ProjectId) {
        self.open_table(RETIRED).insert(anchor.0.0, ());
    }

    fn complete_project_start(&mut self, idea: MessageId) {
        self.open_table(STARTED).insert(idea.0.0, ());
    }

    fn choose_for_idea(&mut self, idea: MessageId, choices: &IdeaChoices) {
        if *choices != IdeaChoices::default() {
            self.open_table(CHOICES)
                .insert(idea.0.0, SenValue::borrowed(choices));
        }
    }
}
