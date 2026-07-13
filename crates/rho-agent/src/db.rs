//! Raw redb schema for persisted agents.

use camino::Utf8PathBuf;
use prefix_id::{PrefixId, PrefixIdDomain};
use redb::{TableDefinition, Value as _};
use redb_derive::{Key, Value as RedbValue};
use rho_core::UnixMs;
use rho_db::{ReadTxn, Sen, SenValue, WriteTxn};
use rho_inference::PromptCacheKey;
pub(crate) use rho_inference::config::{InferenceModel, InferenceProfile, ReasoningEffort};
use rho_workspaces::{WorkspaceId, WorkspaceIdDomain, WorkspaceInfo};
use senax_encoder::{Decode, Encode, Pack, Unpack};
use uuid::Uuid;

use crate::AgentEvent;

const COUNTERS: TableDefinition<CounterKey, u64> = TableDefinition::new("counters");
/// Singleton row holding this database's random machine seed (see
/// [`PrefixIdDomain::machine_seed`]), generated once at init.
const MACHINE: TableDefinition<u8, u64> = TableDefinition::new("machine");
const MACHINE_SEED_KEY: u8 = 0;
const FORMAT: TableDefinition<(), String> = TableDefinition::new("format");
const LINEAGE_PARENTS: TableDefinition<AgentLineageId, AgentEventPos> =
    TableDefinition::new("lineage_parents");
const AGENT_EVENTS: TableDefinition<AgentEventPos, Sen<AgentEvent<'static>>> =
    TableDefinition::new("agent_events");
const AGENTS: TableDefinition<AgentId, Sen<AgentRecord>> = TableDefinition::new("agents");
const TOPICS: TableDefinition<TopicId, Sen<TopicRecord>> = TableDefinition::new("topics");
const TOPIC_AGENTS: TableDefinition<TopicAgentKey, ()> = TableDefinition::new("topic_agents");
/// Keyed by the workdir's absolute path (UTF-8; paths are strings on disk
/// and on the wire), making paths unique by construction.
const LEGACY_WORKDIRS: TableDefinition<String, Sen<WorkdirRecord>> =
    TableDefinition::new("workdirs");
const PROJECTS: TableDefinition<String, Sen<ProjectRecord>> = TableDefinition::new("projects");
const MIGRATION_RECOVERY: TableDefinition<(), Sen<MigrationRecoveryPoint>> =
    TableDefinition::new("migration_recovery");

const CURRENT_AGENT_DB_FORMAT: &str = "b6e40c7a";

struct AgentDbMigration {
    from: &'static str,
    to: &'static str,
    migrate: fn(&mut WriteTxn),
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct MigrationRecoveryPoint {
    pub savepoint_id: u64,
    pub from_format: String,
    pub to_format: String,
    pub created_at: UnixMillis,
}

const AGENT_DB_MIGRATIONS: &[AgentDbMigration] = &[
    AgentDbMigration {
        from: "a1f83c6d",
        to: "7c3e91af",
        migrate: migrate_agent_workdirs,
    },
    AgentDbMigration {
        from: "7c3e91af",
        to: "d4a71c2e",
        migrate: |_| {},
    },
    AgentDbMigration {
        from: "d4a71c2e",
        to: "8f2c6a1d",
        migrate: migrate_agent_spawned_by,
    },
    AgentDbMigration {
        from: "8f2c6a1d",
        to: "b6e40c7a",
        migrate: migrate_projects,
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue)]
struct CounterKey(u8);

impl CounterKey {
    pub const LAST_AGENT_ID: Self = Self(1);
    pub const LAST_LINEAGE_ID: Self = Self(2);
    pub const LAST_TOPIC_ID: Self = Self(3);
    pub const LAST_WORKSPACE_ID: Self = Self(4);
}

pub use rho_core::{AgentId, AgentIdDomain};

pub type TopicId = PrefixId<TopicIdDomain>;

/// Keys topic-id encoding with this database's persisted machine seed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TopicIdDomain(pub u64);

impl PrefixIdDomain for TopicIdDomain {
    const KIND: &'static str = "topic-id";

    fn machine_seed(&self) -> u64 {
        self.0
    }
}

/// A registered directory agents can be started in, keyed by its absolute
/// path. Purely selection vocabulary for clients; agents record their own
/// working directory and the daemon never requires it to match a registered
/// workdir.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct WorkdirRecord {
    pub name: String,
    pub created_at: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ProjectRecord {
    pub name: String,
    pub description: String,
    pub created_at: UnixMillis,
}

/// Pin state, shared by topics and agents. Pinned items sort first in
/// client rails.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Status {
    Normal,
    Pinned,
}

/// What the user did about an agent's last finished turn. Attention is
/// action-cleared (the email-triage model): a turn end always demands a
/// disposition, and merely looking at the agent never provides one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum AgentDisposition {
    /// No disposition yet: the ball is in the user's court.
    Pending,
    /// Acknowledged; nothing more needed until the next turn end. The
    /// default so an agent that never finished a turn has nothing to act
    /// on (and so pre-disposition records decode that way).
    #[default]
    Done,
    /// Deferred: quiet until `until`, then pending again (the Slack-reminder
    /// move for "I'll get back to this").
    Snoozed { until: UnixMillis },
    /// Done, and file it now: skips the rail's idle wait and folds the agent
    /// immediately. Like every disposition it's a verdict on the last turn —
    /// the next user message or turn end overwrites it.
    Hidden,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct TopicRecord {
    pub name: String,
    pub created_at: UnixMillis,
    pub updated_at: UnixMillis,
    pub status: Status,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Key, RedbValue)]
pub struct TopicAgentKey {
    topic_id: TopicId,
    agent_id: AgentId,
}

impl TopicAgentKey {
    pub fn new(topic_id: TopicId, agent_id: AgentId) -> Self {
        Self { topic_id, agent_id }
    }
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Key, RedbValue, Encode, Decode,
)]
pub struct AgentLineageId(u64);

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Key, RedbValue, Encode, Decode,
)]
pub struct AgentEventPos {
    lineage_id: AgentLineageId,
    seq: u32,
}

impl AgentEventPos {
    fn root(lineage_id: AgentLineageId) -> Self {
        Self { lineage_id, seq: 0 }
    }

    fn next(self) -> Self {
        Self {
            lineage_id: self.lineage_id,
            seq: self
                .seq
                .checked_add(1)
                .expect("agent timeline sequence overflow"),
        }
    }
}

pub type UnixMillis = UnixMs;

pub fn migration_recovery_point(db: &rho_db::RhoDb) -> Option<MigrationRecoveryPoint> {
    let read = db.read();
    if !read.has_table("migration_recovery") {
        return None;
    }
    let table = read.open_table(MIGRATION_RECOVERY);
    table.get(&()).map(|point| point.value().into_owned())
}

pub async fn prepare_agent_db_migration(db: &rho_db::RhoDb) {
    let from_format = {
        let read = db.read();
        read.has_table("format")
            .then(|| {
                read.open_table(FORMAT)
                    .get(&())
                    .map(|format| format.value())
            })
            .flatten()
            .filter(|format| format != CURRENT_AGENT_DB_FORMAT)
    };
    let Some(from_format) = from_format else {
        return;
    };
    if migration_recovery_point(db).is_some_and(|point| {
        point.from_format == from_format && point.to_format == CURRENT_AGENT_DB_FORMAT
    }) {
        return;
    }
    db.persistent_savepoint(|write, savepoint_id| {
        let point = MigrationRecoveryPoint {
            savepoint_id,
            from_format,
            to_format: CURRENT_AGENT_DB_FORMAT.to_owned(),
            created_at: UnixMillis::now(),
        };
        write
            .open_table(MIGRATION_RECOVERY)
            .insert(&(), SenValue::borrowed(&point));
    })
    .await;
}

#[derive(Clone, Debug, PartialEq, Eq, Encode)]
pub struct AgentRecord {
    pub display_name: Option<String>,
    /// The agent's working set: where it works, primary workdir first.
    /// Fixed at spawn — never removed or reordered, because accumulated
    /// model context assumes the entries stay valid. For pool workspaces the
    /// jj workspace name is this agent's own workspace id (or the joined
    /// agent's, for agents sharing a workspace).
    pub workdirs: Vec<WorkspaceInfo>,
    pub status: Status,
    pub created_at: UnixMillis,
    pub updated_at: UnixMillis,
    pub current_lineage: AgentLineageId,
    pub parent_agent: Option<AgentId>,
    pub spawned_by: AgentSpawnedBy,
    pub role: AgentRole,
    pub(crate) binding: SessionBinding,
    pub runtime: AgentRuntime,
    /// When the user last sent this agent a message; rail recency seed.
    /// Turn ends reset the disposition but leave this alone — replying is
    /// the engagement signal, finishing is the agent's schedule.
    #[senax(default)]
    pub last_user_message: UnixMillis,
    #[senax(default)]
    pub disposition: AgentDisposition,
}

impl AgentRecord {
    pub fn config(&self) -> AgentRole {
        self.role
    }

    /// The primary workdir (entry 0): default cwd, prompt header, UI label.
    pub fn primary_workdir(&self) -> &WorkspaceInfo {
        self.workdirs
            .first()
            .expect("agent has at least one workdir")
    }
}

impl senax_encoder::Decoder for AgentRecord {
    fn decode(reader: &mut impl bytes::Buf) -> senax_encoder::Result<Self> {
        #[derive(Decode)]
        struct EncodedAgentRecord {
            display_name: Option<String>,
            /// Legacy single-workdir shape; superseded by `workdirs`.
            workspace: Option<WorkspaceInfo>,
            #[senax(default)]
            workdirs: Vec<WorkspaceInfo>,
            status: Status,
            created_at: UnixMillis,
            updated_at: UnixMillis,
            current_lineage: AgentLineageId,
            parent_agent: Option<AgentId>,
            #[senax(default)]
            spawned_by: AgentSpawnedBy,
            role: AgentRole,
            binding: SessionBinding,
            runtime: AgentRuntime,
            #[senax(default)]
            last_user_message: UnixMillis,
            #[senax(default)]
            disposition: AgentDisposition,
        }

        let encoded = EncodedAgentRecord::decode(reader)?;
        let workdirs = if encoded.workdirs.is_empty() {
            match encoded.workspace {
                Some(workspace) => vec![workspace],
                None => return Err(missing_agent_field("workdirs")),
            }
        } else {
            encoded.workdirs
        };
        Ok(Self {
            display_name: encoded.display_name,
            workdirs,
            status: encoded.status,
            created_at: encoded.created_at,
            updated_at: encoded.updated_at,
            current_lineage: encoded.current_lineage,
            parent_agent: encoded.parent_agent,
            spawned_by: encoded.spawned_by,
            role: encoded.role,
            binding: encoded.binding,
            runtime: encoded.runtime,
            last_user_message: encoded.last_user_message,
            disposition: encoded.disposition,
        })
    }
}

fn missing_agent_field(field: &'static str) -> senax_encoder::EncoderError {
    senax_encoder::EncoderError::StructDecode(
        senax_encoder::StructDecodeError::MissingRequiredField {
            field,
            struct_name: "AgentRecord",
        },
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum AgentRuntime {
    Rho { prompt_cache_key: PromptCacheKey },
    Claude { session_id: Uuid },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub enum AgentSpawnedBy {
    #[default]
    Direct,
    PM,
    Engineer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub(crate) enum SessionBinding {
    ResponsesGpt55(InferenceProfile),
    ClaudeFable {
        effort: ClaudeEffort,
    },
    ClaudeOpus {
        effort: ClaudeEffort,
    },
    // gpt-5.6 deep modes; appended after Deep so persisted modes keep
    // decoding.
    ResponsesSol(InferenceProfile),
    ResponsesLuna(InferenceProfile),
    ResponsesTerra(InferenceProfile),
    /// Terra with a coordinator system-prompt section: a user-facing agent
    /// that delegates repo-specific work to spawned workers. Appended so
    /// persisted modes keep decoding.
    CoordinatorTerra(InferenceProfile),
    /// Sol-backed coordinator used by the opinionated medium/high levels.
    CoordinatorSol(InferenceProfile),
    /// Ultra advisory agent. Kept distinct from an ultra engineer so its role
    /// survives session pinning.
    ClaudeAdvisor {
        effort: ClaudeEffort,
    },
    /// Sol-backed advisory agent.
    AdvisorSol(InferenceProfile),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum AgentRole {
    Engineer {
        intelligence: EngineerIntelligence,
    },
    PM,
    Advisor {
        intelligence: AdvisorIntelligence,
    },
    WorkflowEngineer {
        intelligence: EngineerIntelligence,
        workflow: AgentWorkflow,
    },
    WorkflowPM {
        workflow: AgentWorkflow,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum AgentWorkflow {
    #[default]
    Default,
    PrFriendly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum EngineerIntelligence {
    Low,
    Medium,
    High,
    Ultra,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum AdvisorIntelligence {
    Medium,
    High,
}

// Temporary migration-only representation of the previous latency field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
enum Latency {
    Standard,
    Fast,
}

impl Default for AgentRole {
    fn default() -> Self {
        Self::Engineer {
            intelligence: EngineerIntelligence::Medium,
        }
    }
}

impl AgentRole {
    pub fn pm() -> Self {
        Self::PM
    }

    pub fn workflow(self) -> AgentWorkflow {
        match self {
            Self::WorkflowEngineer { workflow, .. } | Self::WorkflowPM { workflow } => workflow,
            Self::Engineer { .. } | Self::PM | Self::Advisor { .. } => AgentWorkflow::Default,
        }
    }
    pub fn is_pm(self) -> bool {
        matches!(self, Self::PM | Self::WorkflowPM { .. })
    }

    pub fn is_engineer(self) -> bool {
        matches!(self, Self::Engineer { .. } | Self::WorkflowEngineer { .. })
    }
    pub fn handle_prefix(self) -> &'static str {
        match self {
            Self::Engineer { .. } | Self::WorkflowEngineer { .. } => "eng",
            Self::PM | Self::WorkflowPM { .. } => "pm",
            Self::Advisor { .. } => "adv",
        }
    }

    pub(crate) fn session_profile(self) -> anyhow::Result<SessionBinding> {
        let deep = |effort| InferenceProfile {
            effort,
            fast_mode: false,
            code_mode: true,
        };
        Ok(match self {
            AgentRole::PM | AgentRole::WorkflowPM { .. } => {
                SessionBinding::CoordinatorSol(InferenceProfile {
                    code_mode: false,
                    ..deep(ReasoningEffort::Low)
                })
            }
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Low,
            }
            | AgentRole::WorkflowEngineer {
                intelligence: EngineerIntelligence::Low,
                ..
            } => SessionBinding::ResponsesTerra(deep(ReasoningEffort::Low)),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Medium,
            } => SessionBinding::ResponsesSol(deep(ReasoningEffort::Medium)),
            AgentRole::WorkflowEngineer {
                intelligence: EngineerIntelligence::Medium,
                workflow: AgentWorkflow::PrFriendly,
            } => SessionBinding::ResponsesSol(deep(ReasoningEffort::High)),
            AgentRole::WorkflowEngineer {
                intelligence: EngineerIntelligence::Medium,
                workflow: AgentWorkflow::Default,
            } => SessionBinding::ResponsesSol(deep(ReasoningEffort::Medium)),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::High,
            }
            | AgentRole::WorkflowEngineer {
                intelligence: EngineerIntelligence::High,
                ..
            } => SessionBinding::ResponsesSol(deep(ReasoningEffort::Xhigh)),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Ultra,
            }
            | AgentRole::WorkflowEngineer {
                intelligence: EngineerIntelligence::Ultra,
                ..
            } => SessionBinding::ClaudeFable {
                effort: ClaudeEffort::High,
            },
            AgentRole::Advisor {
                intelligence: AdvisorIntelligence::Medium,
            } => SessionBinding::AdvisorSol(deep(ReasoningEffort::Xhigh)),
            AgentRole::Advisor {
                intelligence: AdvisorIntelligence::High,
            } => SessionBinding::ClaudeAdvisor {
                effort: ClaudeEffort::High,
            },
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub(crate) enum ClaudeEffort {
    Medium,
    Xhigh,
    High,
}

impl SessionBinding {
    pub fn agent_role(self) -> AgentRole {
        if self.is_coordinator() {
            return AgentRole::pm();
        } else if matches!(self, Self::ClaudeAdvisor { .. }) {
            return AgentRole::Advisor {
                intelligence: AdvisorIntelligence::High,
            };
        } else if matches!(self, Self::AdvisorSol(_)) {
            return AgentRole::Advisor {
                intelligence: AdvisorIntelligence::Medium,
            };
        }
        let (intelligence, _latency) = match self {
            Self::ClaudeFable {
                effort: ClaudeEffort::High,
            }
            | Self::ClaudeAdvisor {
                effort: ClaudeEffort::High,
            } => (EngineerIntelligence::Ultra, Latency::Standard),
            Self::ResponsesSol(config) if config.effort == ReasoningEffort::Xhigh => (
                EngineerIntelligence::High,
                if config.fast_mode {
                    Latency::Fast
                } else {
                    Latency::Standard
                },
            ),
            Self::ResponsesTerra(config) if config.effort == ReasoningEffort::Low => (
                EngineerIntelligence::Low,
                if config.fast_mode {
                    Latency::Fast
                } else {
                    Latency::Standard
                },
            ),
            Self::ResponsesGpt55(config)
            | Self::ResponsesSol(config)
            | Self::ResponsesLuna(config)
            | Self::ResponsesTerra(config)
            | Self::CoordinatorTerra(config)
            | Self::CoordinatorSol(config)
            | Self::AdvisorSol(config) => (
                match config.effort {
                    ReasoningEffort::Low => EngineerIntelligence::Low,
                    ReasoningEffort::Medium => EngineerIntelligence::Medium,
                    ReasoningEffort::High => EngineerIntelligence::High,
                    ReasoningEffort::Xhigh => EngineerIntelligence::High,
                },
                if config.fast_mode {
                    Latency::Fast
                } else {
                    Latency::Standard
                },
            ),
            Self::ClaudeFable { .. } | Self::ClaudeOpus { .. } | Self::ClaudeAdvisor { .. } => {
                (EngineerIntelligence::Ultra, Latency::Standard)
            }
        };
        AgentRole::Engineer { intelligence }
    }

    pub fn deep_config(self) -> Option<InferenceProfile> {
        match self {
            Self::ResponsesGpt55(config)
            | Self::ResponsesSol(config)
            | Self::ResponsesLuna(config)
            | Self::ResponsesTerra(config)
            | Self::CoordinatorTerra(config)
            | Self::CoordinatorSol(config)
            | Self::AdvisorSol(config) => Some(config),
            Self::ClaudeFable { .. } | Self::ClaudeOpus { .. } | Self::ClaudeAdvisor { .. } => None,
        }
    }

    pub fn deep_model(self) -> Option<InferenceModel> {
        match self {
            Self::ResponsesGpt55(_) => Some(InferenceModel::Gpt55),
            Self::ResponsesSol(_) | Self::AdvisorSol(_) => Some(InferenceModel::Gpt56Sol),
            Self::ResponsesLuna(_) => Some(InferenceModel::Gpt56Luna),
            Self::ResponsesTerra(_) | Self::CoordinatorTerra(_) => Some(InferenceModel::Gpt56Terra),
            Self::CoordinatorSol(_) => Some(InferenceModel::Gpt56Sol),
            Self::ClaudeFable { .. } | Self::ClaudeOpus { .. } | Self::ClaudeAdvisor { .. } => None,
        }
    }

    pub fn claude_model(self) -> Option<rho_claude::Model> {
        match self {
            Self::ClaudeFable { .. } | Self::ClaudeAdvisor { .. } => Some(rho_claude::Model::Fable),
            Self::ClaudeOpus { .. } => Some(rho_claude::Model::Opus),
            Self::ResponsesGpt55(_)
            | Self::ResponsesSol(_)
            | Self::ResponsesLuna(_)
            | Self::ResponsesTerra(_)
            | Self::CoordinatorTerra(_)
            | Self::CoordinatorSol(_)
            | Self::AdvisorSol(_) => None,
        }
    }

    pub fn claude_effort(self) -> Option<rho_claude::Effort> {
        match self {
            Self::ClaudeFable { effort } | Self::ClaudeAdvisor { effort } => {
                Some(effort.to_claude_effort())
            }
            Self::ClaudeOpus { effort } => Some(effort.to_claude_effort()),
            Self::ResponsesGpt55(_)
            | Self::ResponsesSol(_)
            | Self::ResponsesLuna(_)
            | Self::ResponsesTerra(_)
            | Self::CoordinatorTerra(_)
            | Self::CoordinatorSol(_)
            | Self::AdvisorSol(_) => None,
        }
    }

    pub fn is_coordinator(self) -> bool {
        matches!(self, Self::CoordinatorTerra(_) | Self::CoordinatorSol(_))
    }
}

impl ClaudeEffort {
    fn to_claude_effort(self) -> rho_claude::Effort {
        match self {
            Self::Medium => rho_claude::Effort::Medium,
            Self::Xhigh => rho_claude::Effort::Xhigh,
            Self::High => rho_claude::Effort::High,
        }
    }
}

pub trait AgentReadTxnExt {
    /// This database's random machine seed; present once
    /// [`AgentWriteTxnExt::init_agent_tables`] has run.
    fn machine_seed(&self) -> u64;
    fn last_agent_counter(&self) -> u64;
    fn last_workspace_counter(&self) -> u64;
    fn get_topic(&self, topic_id: TopicId) -> TopicRecord;
    fn list_topics(&self) -> Vec<(TopicId, TopicRecord)>;
    fn list_topic_agents(&self, topic_id: TopicId) -> Vec<AgentId>;
    /// The topic an agent currently belongs to.
    fn agent_topic(&self, agent_id: AgentId) -> Option<TopicId>;
    fn get_agent(&self, agent_id: AgentId) -> AgentRecord;
    fn list_agents(&self) -> Vec<(AgentId, AgentRecord)>;
    fn list_projects(&self) -> Vec<(Utf8PathBuf, ProjectRecord)>;
    fn agent_events(&self, agent_id: AgentId) -> (AgentEventPos, Vec<AgentEvent<'static>>);
    fn agent_event_records(
        &self,
        agent_id: AgentId,
    ) -> (AgentEventPos, Vec<(AgentEventPos, AgentEvent<'static>)>);
}

#[allow(clippy::too_many_arguments)]
pub trait AgentWriteTxnExt {
    fn init_agent_tables(&mut self);

    fn create_topic(&mut self, now: UnixMillis, name: String, status: Status) -> TopicId;

    fn set_topic_name(&mut self, now: UnixMillis, topic_id: TopicId, name: String);

    fn set_topic_status(&mut self, now: UnixMillis, topic_id: TopicId, status: Status);

    fn set_agent_status(&mut self, now: UnixMillis, agent_id: AgentId, status: Status);

    fn set_agent_display_name(&mut self, now: UnixMillis, agent_id: AgentId, name: String);
    fn set_agent_role(&mut self, agent_id: AgentId, role: AgentRole);

    fn alloc_agent_id(&mut self) -> AgentId;

    /// Reserves a fresh jj workspace name. Ids never repeat, so recreated
    /// workspaces can't collide with forgotten names in the repo view.
    fn alloc_workspace_id(&mut self) -> WorkspaceId;

    /// Re-points the agent's topic membership. Topics are ad-hoc groupings
    /// agents move into after the fact; nothing else about the agent changes.
    fn move_agent_to_topic(&mut self, agent_id: AgentId, topic_id: TopicId);

    fn upsert_project(&mut self, now: UnixMillis, path: &str, name: String, description: String);

    fn remove_project(&mut self, path: &str);

    fn append_agent_event(&mut self, at: AgentEventPos, event: &AgentEvent<'_>) -> AgentEventPos;

    fn fork_agent_lineage(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        parent: AgentEventPos,
    ) -> AgentEventPos;

    /// Records a turn end for attention purposes; resets the disposition to
    /// `Pending` — every finished turn demands a fresh disposition.
    fn record_agent_turn_end(&mut self, agent_id: AgentId);

    /// Stamps the user's engagement with an agent (rail recency) and clears
    /// its disposition: replying is as much a verdict as :done.
    fn record_agent_user_message(&mut self, now: UnixMillis, agent_id: AgentId);

    fn set_agent_disposition(&mut self, agent_id: AgentId, disposition: AgentDisposition);
}

#[allow(clippy::too_many_arguments)]
pub(crate) trait AgentProfileWriteTxnExt {
    fn create_agent(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        topic_id: TopicId,
        display_name: Option<String>,
        workdirs: Vec<WorkspaceInfo>,
        mode: SessionBinding,
        runtime: AgentRuntime,
        parent_agent: Option<AgentId>,
    ) -> AgentEventPos;
}

impl AgentProfileWriteTxnExt for WriteTxn {
    fn create_agent(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        topic_id: TopicId,
        display_name: Option<String>,
        workdirs: Vec<WorkspaceInfo>,
        mode: SessionBinding,
        runtime: AgentRuntime,
        parent_agent: Option<AgentId>,
    ) -> AgentEventPos {
        assert!(!workdirs.is_empty(), "agent needs at least one workdir");
        let lineage_id = AgentLineageId(next_counter(self, CounterKey::LAST_LINEAGE_ID));
        self.open_table(LINEAGE_PARENTS);
        let spawned_by = parent_agent.map_or(AgentSpawnedBy::Direct, |parent| {
            match self
                .open_table(AGENTS)
                .get(&parent)
                .expect("parent agent must exist")
                .value()
                .into_owned()
                .role
            {
                AgentRole::PM | AgentRole::WorkflowPM { .. } => AgentSpawnedBy::PM,
                AgentRole::Engineer { .. } | AgentRole::WorkflowEngineer { .. } => {
                    AgentSpawnedBy::Engineer
                }
                AgentRole::Advisor { .. } => panic!("Advisors cannot spawn agents"),
            }
        });
        let agent = AgentRecord {
            display_name,
            workdirs,
            status: Status::Normal,
            created_at: now,
            updated_at: now,
            current_lineage: lineage_id,
            parent_agent,
            spawned_by,
            role: mode.agent_role(),
            binding: mode,
            runtime,
            last_user_message: now,
            disposition: AgentDisposition::Done,
        };
        self.open_table(AGENTS)
            .insert(&agent_id, SenValue::borrowed(&agent));
        self.open_table(TOPIC_AGENTS)
            .insert(&TopicAgentKey::new(topic_id, agent_id), &());
        AgentEventPos::root(lineage_id)
    }
}

impl AgentReadTxnExt for ReadTxn {
    fn machine_seed(&self) -> u64 {
        self.open_table(MACHINE)
            .get(&MACHINE_SEED_KEY)
            .expect("machine seed missing; init_agent_tables must run first")
            .value()
    }

    fn last_agent_counter(&self) -> u64 {
        self.open_table(COUNTERS)
            .get(&CounterKey::LAST_AGENT_ID)
            .map(|counter| counter.value())
            .unwrap_or(0)
    }

    fn last_workspace_counter(&self) -> u64 {
        self.open_table(COUNTERS)
            .get(&CounterKey::LAST_WORKSPACE_ID)
            .map(|counter| counter.value())
            .unwrap_or(0)
    }

    fn get_topic(&self, topic_id: TopicId) -> TopicRecord {
        self.open_table(TOPICS)
            .get(&topic_id)
            .expect("topic id missing")
            .value()
            .into_owned()
    }

    fn list_topics(&self) -> Vec<(TopicId, TopicRecord)> {
        self.open_table(TOPICS)
            .iter()
            .map(|(key, value)| (key.value(), value.value().into_owned()))
            .collect()
    }

    fn list_topic_agents(&self, topic_id: TopicId) -> Vec<AgentId> {
        self.open_table(TOPIC_AGENTS)
            .range(
                TopicAgentKey::new(topic_id, AgentId::MIN)
                    ..=TopicAgentKey::new(topic_id, AgentId::MAX),
            )
            .map(|(key, _)| key.value().agent_id)
            .collect()
    }

    fn agent_topic(&self, agent_id: AgentId) -> Option<TopicId> {
        self.open_table(TOPIC_AGENTS)
            .iter()
            .map(|(key, _)| key.value())
            .find(|key| key.agent_id == agent_id)
            .map(|key| key.topic_id)
    }

    fn get_agent(&self, agent_id: AgentId) -> AgentRecord {
        self.open_table(AGENTS)
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned()
    }

    fn list_agents(&self) -> Vec<(AgentId, AgentRecord)> {
        self.open_table(AGENTS)
            .iter()
            .map(|(key, value)| (key.value(), value.value().into_owned()))
            .collect()
    }

    fn list_projects(&self) -> Vec<(Utf8PathBuf, ProjectRecord)> {
        self.open_table(PROJECTS)
            .iter()
            .map(|(key, value)| (Utf8PathBuf::from(key.value()), value.value().into_owned()))
            .collect()
    }

    fn agent_events(&self, agent_id: AgentId) -> (AgentEventPos, Vec<AgentEvent<'static>>) {
        let (next, records) = self.agent_event_records(agent_id);
        (next, records.into_iter().map(|(_, event)| event).collect())
    }

    fn agent_event_records(
        &self,
        agent_id: AgentId,
    ) -> (AgentEventPos, Vec<(AgentEventPos, AgentEvent<'static>)>) {
        let agent = self.get_agent(agent_id);
        let mut segments = Vec::new();
        let mut lineage_id = agent.current_lineage;
        let mut end_seq = u32::MAX;
        let lineage_parents = self.open_table(LINEAGE_PARENTS);
        loop {
            segments.push((lineage_id, end_seq));
            let Some(parent) = lineage_parents.get(&lineage_id) else {
                break;
            };
            let parent = parent.value();
            lineage_id = parent.lineage_id;
            end_seq = parent.seq;
        }
        drop(lineage_parents);

        let mut events = Vec::new();
        let mut next = AgentEventPos::root(agent.current_lineage);
        let timeline = self.open_table(AGENT_EVENTS);
        for (lineage_id, end_seq) in segments.into_iter().rev() {
            let is_current_lineage = lineage_id == agent.current_lineage;
            for (key, value) in timeline.range(
                AgentEventPos::root(lineage_id)..=AgentEventPos {
                    lineage_id,
                    seq: end_seq,
                },
            ) {
                let key = key.value();
                if key.seq == end_seq && end_seq != u32::MAX {
                    break;
                }
                if is_current_lineage {
                    next = key.next();
                }
                events.push((key, value.value().into_owned()));
            }
        }
        (next, events)
    }
}

impl AgentWriteTxnExt for WriteTxn {
    fn init_agent_tables(&mut self) {
        // Migrations run before the typed opens below: a migration may need
        // to rewrite a table whose stored key/value types no longer match
        // the current definitions.
        migrate_agent_db_format(self);
        self.open_table(COUNTERS);
        self.open_table(FORMAT);
        self.open_table(LINEAGE_PARENTS);
        self.open_table(AGENT_EVENTS);
        self.open_table(AGENTS);
        self.open_table(TOPICS);
        self.open_table(TOPIC_AGENTS);
        self.open_table(PROJECTS);
        let mut machine = self.open_table(MACHINE);
        if machine.get(&MACHINE_SEED_KEY).is_none() {
            machine.insert(&MACHINE_SEED_KEY, &rand::random::<u64>());
        }
    }

    fn create_topic(&mut self, now: UnixMillis, name: String, status: Status) -> TopicId {
        let domain = TopicIdDomain(machine_seed(self));
        let topic_id =
            TopicId::from_counter(next_counter(self, CounterKey::LAST_TOPIC_ID), &domain)
                .expect("topic id counter exceeds prefix-id capacity");
        let topic = TopicRecord {
            name,
            created_at: now,
            updated_at: now,
            status,
        };
        self.open_table(TOPICS)
            .insert(&topic_id, SenValue::borrowed(&topic));
        topic_id
    }

    fn set_topic_name(&mut self, now: UnixMillis, topic_id: TopicId, name: String) {
        let mut topics = self.open_table(TOPICS);
        let mut topic = topics
            .get(&topic_id)
            .expect("topic id missing")
            .value()
            .into_owned();
        topic.name = name;
        topic.updated_at = now;
        topics.insert(&topic_id, SenValue::borrowed(&topic));
    }

    fn set_topic_status(&mut self, now: UnixMillis, topic_id: TopicId, status: Status) {
        let mut topics = self.open_table(TOPICS);
        let mut topic = topics
            .get(&topic_id)
            .expect("topic id missing")
            .value()
            .into_owned();
        topic.status = status;
        topic.updated_at = now;
        topics.insert(&topic_id, SenValue::borrowed(&topic));
    }

    fn set_agent_status(&mut self, now: UnixMillis, agent_id: AgentId, status: Status) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        agent.status = status;
        agent.updated_at = now;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
    }

    fn set_agent_display_name(&mut self, now: UnixMillis, agent_id: AgentId, name: String) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        agent.display_name = Some(name);
        agent.updated_at = now;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
    }

    fn set_agent_role(&mut self, agent_id: AgentId, role: AgentRole) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent missing")
            .value()
            .into_owned();
        agent.role = role;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
    }

    fn alloc_agent_id(&mut self) -> AgentId {
        let domain = AgentIdDomain(machine_seed(self));
        AgentId::from_counter(next_counter(self, CounterKey::LAST_AGENT_ID), &domain)
            .expect("agent id counter exceeds prefix-id capacity")
    }

    fn alloc_workspace_id(&mut self) -> WorkspaceId {
        let domain = WorkspaceIdDomain(machine_seed(self));
        WorkspaceId::from_counter(next_counter(self, CounterKey::LAST_WORKSPACE_ID), &domain)
            .expect("workspace id counter exceeds prefix-id capacity")
    }

    fn move_agent_to_topic(&mut self, agent_id: AgentId, topic_id: TopicId) {
        let mut topic_agents = self.open_table(TOPIC_AGENTS);
        let previous = topic_agents
            .iter()
            .map(|(key, _)| key.value())
            .filter(|key| key.agent_id == agent_id)
            .collect::<Vec<_>>();
        for key in previous {
            topic_agents.remove(&key);
        }
        topic_agents.insert(&TopicAgentKey::new(topic_id, agent_id), &());
    }

    fn upsert_project(&mut self, now: UnixMillis, path: &str, name: String, description: String) {
        let mut projects = self.open_table(PROJECTS);
        let created_at = projects
            .get(&path.to_owned())
            .map(|record| record.value().into_owned().created_at)
            .unwrap_or(now);
        projects.insert(
            &path.to_owned(),
            SenValue::borrowed(&ProjectRecord {
                name,
                description,
                created_at,
            }),
        );
    }

    fn remove_project(&mut self, path: &str) {
        self.open_table(PROJECTS).remove(&path.to_owned());
    }

    fn append_agent_event(&mut self, at: AgentEventPos, event: &AgentEvent<'_>) -> AgentEventPos {
        self.open_table(AGENT_EVENTS)
            .insert(&at, SenValue::borrowed(event));
        at.next()
    }

    fn record_agent_turn_end(&mut self, agent_id: AgentId) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        // A turn end puts the ball back in the user's court; it says
        // nothing about engagement, so `last_user_message` stays.
        agent.disposition = AgentDisposition::Pending;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
    }

    fn record_agent_user_message(&mut self, now: UnixMillis, agent_id: AgentId) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        agent.last_user_message = now;
        // Replying is a verdict like :done or :snooze — the ball moves to
        // the agent's court even if the turn hasn't started yet (queued
        // delivery), so a pending lamp must not linger.
        agent.disposition = AgentDisposition::Done;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
    }

    fn set_agent_disposition(&mut self, agent_id: AgentId, disposition: AgentDisposition) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        agent.disposition = disposition;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
    }

    fn fork_agent_lineage(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        parent: AgentEventPos,
    ) -> AgentEventPos {
        let lineage_id = AgentLineageId(next_counter(self, CounterKey::LAST_LINEAGE_ID));
        self.open_table(LINEAGE_PARENTS)
            .insert(&lineage_id, &parent);
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        agent.current_lineage = lineage_id;
        agent.updated_at = now;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
        AgentEventPos::root(lineage_id)
    }
}

fn migrate_agent_db_format(write: &mut WriteTxn) {
    let current = CURRENT_AGENT_DB_FORMAT;
    let mut format = {
        let table = write.open_table(FORMAT);
        table
            .get(&())
            .map(|value| value.value())
            .unwrap_or_else(|| current.to_owned())
    };

    while format != current {
        let Some(migration) = AGENT_DB_MIGRATIONS
            .iter()
            .find(|migration| migration.from == format)
        else {
            panic!(
                "this rho agent database was written by an older or different rho version \
                 (database format {format}, this build expects {current}). \
                 Update rho one version at a time so migrations can run, or remove \
                 the local rho database if you do not need the saved agents."
            );
        };
        (migration.migrate)(write);
        format = migration.to.to_owned();
    }

    write.open_table(FORMAT).insert(&(), &current.to_owned());
}

/// Rewrites agent records so the legacy single `workspace` field is stored
/// as the new `workdirs` list (the decoder accepts both; re-inserting
/// normalizes the bytes).
fn migrate_agent_workdirs(write: &mut WriteTxn) {
    let records = {
        let agents = write.open_table(AGENTS);
        agents
            .iter()
            .map(|(id, record)| (id.value(), record.value().into_owned()))
            .collect::<Vec<_>>()
    };
    let mut agents = write.open_table(AGENTS);
    for (agent_id, record) in records {
        agents.insert(&agent_id, SenValue::borrowed(&record));
    }
}

fn migrate_agent_spawned_by(write: &mut WriteTxn) {
    let mut records = {
        let agents = write.open_table(AGENTS);
        agents
            .iter()
            .map(|(id, record)| (id.value(), record.value().into_owned()))
            .collect::<Vec<_>>()
    };
    let roles = records
        .iter()
        .map(|(id, record)| (*id, record.role))
        .collect::<std::collections::HashMap<_, _>>();
    for (_, record) in &mut records {
        record.spawned_by =
            record
                .parent_agent
                .map_or(AgentSpawnedBy::Direct, |parent| {
                    match roles
                        .get(&parent)
                        .expect("migrated parent agent must exist")
                    {
                        AgentRole::PM | AgentRole::WorkflowPM { .. } => AgentSpawnedBy::PM,
                        AgentRole::Engineer { .. } | AgentRole::WorkflowEngineer { .. } => {
                            AgentSpawnedBy::Engineer
                        }
                        AgentRole::Advisor { .. } => {
                            panic!("saved Advisor unexpectedly owns an agent")
                        }
                    }
                });
    }
    let mut agents = write.open_table(AGENTS);
    for (agent_id, record) in records {
        agents.insert(&agent_id, SenValue::borrowed(&record));
    }
}

fn migrate_projects(write: &mut WriteTxn) {
    let records = write
        .open_table(LEGACY_WORKDIRS)
        .iter()
        .map(|(path, record)| (path.value(), record.value().into_owned()))
        .collect::<Vec<_>>();
    let mut projects = write.open_table(PROJECTS);
    for (path, record) in records {
        projects.insert(
            &path,
            SenValue::borrowed(&ProjectRecord {
                name: record.name,
                description: String::new(),
                created_at: record.created_at,
            }),
        );
    }
}

fn next_counter(write: &mut WriteTxn, key: CounterKey) -> u64 {
    let mut counters = write.open_table(COUNTERS);
    let next = counters.get(&key).map(|value| value.value()).unwrap_or(0) + 1;
    counters.insert(&key, &next);
    next
}

fn machine_seed(write: &mut WriteTxn) -> u64 {
    write
        .open_table(MACHINE)
        .get(&MACHINE_SEED_KEY)
        .expect("machine seed missing; init_agent_tables must run first")
        .value()
}

#[cfg(test)]
mod tests;
