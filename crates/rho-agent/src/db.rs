//! Raw redb schema for persisted agents.

use std::collections::BTreeMap;

use camino::Utf8PathBuf;
use redb::{TableDefinition, Value as _};
use redb_derive::{Key, Value as RedbValue};
use rho_core::UnixMs;
use rho_db::{ReadTxn, Sen, SenValue, WriteTxn};
use rho_inference::PromptCacheKey;
pub(crate) use rho_inference::config::{InferenceModel, InferenceProfile, ReasoningEffort};
use rho_workspaces::WorkspaceInfo;
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
/// A compact index of presentation history. Keeping it separate from the
/// full transcript lets rewind repair inspect only sidecar updates.
const PRESENTATION_EVENTS: TableDefinition<AgentEventPos, Sen<AgentPresentationUpdate>> =
    TableDefinition::new("agent_presentation_events");
const MAX_PRESENTATION_SOURCE_SCANNED_EVENTS: usize = 256;
const AGENTS: TableDefinition<AgentId, Sen<AgentRecord>> = TableDefinition::new("agents");
const AGENT_RESPONSE_SUBSCRIPTIONS: TableDefinition<AgentResponseSubscription, ()> =
    TableDefinition::new("agent_response_subscriptions");
const PROJECTS: TableDefinition<String, Sen<ProjectRecord>> = TableDefinition::new("projects");
/// Opaque client-owned view configuration (see
/// [`AgentReadTxnExt::view_config`]).
const VIEW_CONFIG: TableDefinition<(), Vec<u8>> = TableDefinition::new("view_config");
/// The daemon-wide auth namespace selected at startup.
const DEFAULT_AUTH_NAMESPACE: TableDefinition<(), String> =
    TableDefinition::new("default_auth_namespace");
const QUOTA_OBSERVATIONS: TableDefinition<QuotaObservationKey, Sen<QuotaObservationRecord>> =
    TableDefinition::new("quota_observations_by_model_time");
const AGENT_USAGE_BUCKETS: TableDefinition<AgentUsageKey, Sen<AgentUsageBucket>> =
    TableDefinition::new("agent_usage_by_agent_time");
const AGENT_USAGE_TOTALS: TableDefinition<AgentId, Sen<AgentUsageBucket>> =
    TableDefinition::new("agent_usage_totals");
const GLOBAL_AGENT_USAGE: TableDefinition<GlobalAgentUsageKey, Sen<AgentUsageBucket>> =
    TableDefinition::new("agent_usage_by_time_provider");
const CURRENT_AGENT_DB_FORMAT: &str = "d37a6f02";
const QUOTA_RESET_JITTER_SECONDS: u64 = 60;

struct AgentDbMigration {
    from: &'static str,
    to: &'static str,
    migrate: fn(&mut WriteTxn),
}

const AGENT_DB_MIGRATIONS: &[AgentDbMigration] = &[];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue)]
struct CounterKey(u8);

impl CounterKey {
    pub const LAST_AGENT_ID: Self = Self(1);
    pub const LAST_LINEAGE_ID: Self = Self(2);
}

/// A persistent relationship that routes every future terminal response from
/// `target` into `subscriber` as ordinary agent mail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue)]
struct AgentResponseSubscription {
    target: AgentId,
    subscriber: AgentId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue, Encode, Decode)]
pub struct QuotaModel(u8);

impl QuotaModel {
    pub const GPT: Self = Self(1);
    pub const FABLE: Self = Self(2);
    pub const OPUS: Self = Self(3);

    pub fn name(self) -> &'static str {
        match self {
            Self::GPT => "gpt",
            Self::FABLE => "fable",
            Self::OPUS => "opus",
            _ => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue)]
struct QuotaObservationKey {
    model: QuotaModel,
    observed_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum QuotaProvider {
    ChatGpt,
    Claude,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct QuotaObservationRecord {
    pub provider: QuotaProvider,
    pub model: QuotaModel,
    /// The daemon-local OAuth namespace for ChatGPT observations. Claude and
    /// legacy observations are unscoped.
    pub auth_namespace: Option<String>,
    pub observed_at: UnixMillis,
    pub used_percent: u8,
    pub reset_at_unix: Option<i64>,
}

pub const AGENT_USAGE_BUCKET_MS: u64 = 5 * 60 * 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue)]
struct AgentUsageKey {
    agent_id: AgentId,
    bucket_start_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue, Encode, Decode)]
pub struct AgentUsageModel(u8);

impl AgentUsageModel {
    pub const UNKNOWN: Self = Self(0);
    pub const GPT: Self = Self(1);
    pub const FABLE: Self = Self(2);
    pub const OPUS: Self = Self(3);
    pub const TERRA: Self = Self(4);
    pub const LUNA: Self = Self(5);

    pub fn name(self) -> &'static str {
        match self {
            Self::GPT => "gpt",
            Self::FABLE => "fable",
            Self::OPUS => "opus",
            Self::TERRA => "terra",
            Self::LUNA => "luna",
            _ => "unknown",
        }
    }
}

impl Default for AgentUsageModel {
    fn default() -> Self {
        Self::UNKNOWN
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue)]
struct GlobalAgentUsageKey {
    bucket_start_ms: u64,
    model: AgentUsageModel,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct AgentUsageBucket {
    pub bucket_start_ms: u64,
    #[senax(default)]
    pub model: AgentUsageModel,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    #[senax(default)]
    pub cache_write_1h_tokens: u64,
    pub output_tokens: u64,
    pub requests: u64,
    #[senax(default)]
    pub approximate: bool,
}

impl AgentUsageBucket {
    pub fn add(&mut self, other: &Self) {
        if self.requests == 0 {
            self.model = other.model;
        } else if other.model != AgentUsageModel::UNKNOWN && self.model != other.model {
            self.model = AgentUsageModel::UNKNOWN;
        }
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
        self.cache_write_1h_tokens = self
            .cache_write_1h_tokens
            .saturating_add(other.cache_write_1h_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.requests = self.requests.saturating_add(other.requests);
        self.approximate |= other.approximate;
    }
}

fn usage_model(record: &AgentRecord) -> AgentUsageModel {
    match record.runtime {
        AgentRuntime::Rho { .. } => match record.binding.deep_model() {
            Some(InferenceModel::Gpt56Terra) => AgentUsageModel::TERRA,
            Some(InferenceModel::Gpt56Luna) => AgentUsageModel::LUNA,
            _ => AgentUsageModel::GPT,
        },
        AgentRuntime::Claude { .. } => match record.binding.claude_model() {
            Some(rho_claude::Model::Opus) => AgentUsageModel::OPUS,
            Some(rho_claude::Model::Fable | rho_claude::Model::Sonnet) | None => {
                AgentUsageModel::FABLE
            }
        },
    }
}

fn add_global_agent_usage(write: &mut WriteTxn, model: AgentUsageModel, bucket: &AgentUsageBucket) {
    let key = GlobalAgentUsageKey {
        bucket_start_ms: bucket.bucket_start_ms,
        model,
    };
    let mut table = write.open_table(GLOBAL_AGENT_USAGE);
    let mut merged = table
        .get(&key)
        .map(|value| value.value().into_owned())
        .unwrap_or_else(|| AgentUsageBucket {
            bucket_start_ms: bucket.bucket_start_ms,
            ..AgentUsageBucket::default()
        });
    merged.add(bucket);
    table.insert(&key, SenValue::borrowed(&merged));
}

fn quota_observation_unchanged(old: &QuotaObservationRecord, new: &QuotaObservationRecord) -> bool {
    old.provider == new.provider
        && old.model == new.model
        && old.used_percent == new.used_percent
        && match (old.reset_at_unix, new.reset_at_unix) {
            (Some(old), Some(new)) => old.abs_diff(new) <= QUOTA_RESET_JITTER_SECONDS,
            (None, None) => true,
            _ => false,
        }
}

pub use rho_core::{
    AdvisorIntelligence, AgentDisposition, AgentId, AgentIdDomain, AgentRole, AgentWorkflow,
    EngineerIntelligence,
};

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ProjectRecord {
    pub name: String,
    pub description: String,
    pub created_at: UnixMillis,
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

/// One field in a model-derived presentation update. `Clear` remains distinct
/// from `Unchanged` for explicit cache maintenance, while normal turn
/// settlement leaves the last model-derived activity intact.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum PresentationField {
    Unchanged,
    Set(String),
    Clear,
}

/// A sidecar-derived title/activity update. `through` is a durable source
/// position, not the position where this update happens to be recorded. That
/// distinction makes a late result harmless after rewind.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AgentPresentationUpdate {
    pub generated_title: PresentationField,
    pub activity: PresentationField,
    pub through: AgentEventPos,
}

/// The durable cache projected to the UI and used to seed a fresh Luna turn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentPresentationCache {
    pub generated_title: Option<String>,
    pub activity: Option<String>,
}

impl AgentEventPos {
    fn root(lineage_id: AgentLineageId) -> Self {
        Self { lineage_id, seq: 0 }
    }

    pub(crate) fn next(self) -> Self {
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

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AgentRecord {
    pub display_name: Option<String>,
    /// The sidecar title. A manual `display_name` always takes precedence.
    #[senax(default)]
    pub generated_title: Option<String>,
    /// The last durable, model-derived activity label.
    #[senax(default)]
    pub activity: Option<String>,
    /// The agent's working set: where it works, primary workdir first.
    /// Fixed at spawn — never removed or reordered, because accumulated
    /// model context assumes the entries stay valid. Managed workspace ids
    /// are repository-local and allocated by jj; joined agents retain the
    /// owning agent's id for that repository.
    pub workdirs: Vec<WorkspaceInfo>,
    pub created_at: UnixMillis,
    pub updated_at: UnixMillis,
    pub current_lineage: AgentLineageId,
    pub parent_agent: Option<AgentId>,
    pub spawned_by: AgentSpawnedBy,
    pub role: AgentRole,
    pub(crate) binding: SessionBinding,
    pub runtime: AgentRuntime,
    /// A message-only Claude rewind whose destination transcript has not yet
    /// been durably materialized and verified. The old runtime remains
    /// authoritative until then.
    #[senax(default)]
    pub claude_rewind: Option<ClaudeRewind>,
    /// When the user last sent this agent a message; rail recency seed.
    /// Turn ends raise attention but leave this alone — replying is the
    /// engagement signal, finishing is the agent's schedule.
    #[senax(default)]
    pub last_user_message: UnixMillis,
    /// A one-line snippet of that message, so summaries can say what the
    /// user last asked without replaying the transcript.
    #[senax(default)]
    pub last_user_message_text: String,
    /// Free-form markers ("pin", …); semantics live in the client's view
    /// layer. Not copied on spawn.
    #[senax(default)]
    pub labels: Vec<String>,
    /// The user's verdict on the last finished turn; attention is derived
    /// from this plus live agent state, never stored.
    #[senax(default)]
    pub disposition: AgentDisposition,
    /// One-shot classification of the last finished turn. Cleared when the
    /// user replies — the report describes a ball that is no longer in the
    /// user's court.
    #[senax(default)]
    pub turn_report: Option<TurnReport>,
    /// The user has messaged this agent directly (agent mail doesn't count).
    /// Sticky: once engaged, the agent's turn ends are the user's court even
    /// for a sub-agent, so it gets attention and turn reports like a root.
    #[senax(default)]
    pub user_interacted: bool,
}

/// What a finished turn asks of the user, derived from its final message.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct TurnReport {
    pub needs_you: bool,
    /// Activity-shaped few-word label of the outcome. Defaulted so records
    /// written before the rename from `one_liner` still decode.
    #[senax(default)]
    pub summary: String,
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

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum AgentRuntime {
    Rho { prompt_cache_key: PromptCacheKey },
    Claude { session_id: Uuid },
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ClaudeRewind {
    pub source_session_id: Uuid,
    pub session_id: Uuid,
    pub resume_at: Option<Uuid>,
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
    /// Terra-backed cheap advisory agent. Appended so persisted modes keep
    /// decoding.
    AdvisorTerra(InferenceProfile),
}

pub(crate) trait AgentRoleSessionProfile {
    fn session_profile(self) -> anyhow::Result<SessionBinding>;
}

impl AgentRoleSessionProfile for AgentRole {
    fn session_profile(self) -> anyhow::Result<SessionBinding> {
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
            AgentRole::Iris => SessionBinding::ResponsesTerra(InferenceProfile {
                effort: ReasoningEffort::Medium,
                fast_mode: true,
                code_mode: false,
            }),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Mini,
            }
            | AgentRole::WorkflowEngineer {
                intelligence: EngineerIntelligence::Mini,
                ..
            } => SessionBinding::ResponsesLuna(InferenceProfile {
                fast_mode: true,
                code_mode: false,
                ..deep(ReasoningEffort::Xhigh)
            }),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Low,
            }
            | AgentRole::WorkflowEngineer {
                intelligence: EngineerIntelligence::Low,
                ..
            } => SessionBinding::ResponsesTerra(deep(ReasoningEffort::Low)),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Cheap,
            }
            | AgentRole::WorkflowEngineer {
                intelligence: EngineerIntelligence::Cheap,
                ..
            } => SessionBinding::ResponsesTerra(deep(ReasoningEffort::High)),
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
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Alt,
            }
            | AgentRole::WorkflowEngineer {
                intelligence: EngineerIntelligence::Alt,
                ..
            } => SessionBinding::ClaudeOpus {
                effort: ClaudeEffort::Medium,
            },
            AgentRole::Advisor {
                intelligence: AdvisorIntelligence::Medium,
            } => SessionBinding::AdvisorSol(deep(ReasoningEffort::Xhigh)),
            AgentRole::Advisor {
                intelligence: AdvisorIntelligence::Cheap,
            } => SessionBinding::AdvisorTerra(deep(ReasoningEffort::Xhigh)),
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
        } else if matches!(self, Self::AdvisorTerra(_)) {
            return AgentRole::Advisor {
                intelligence: AdvisorIntelligence::Cheap,
            };
        }
        let intelligence = match self {
            Self::ResponsesLuna(_) => EngineerIntelligence::Mini,
            Self::ClaudeFable {
                effort: ClaudeEffort::High,
            }
            | Self::ClaudeAdvisor {
                effort: ClaudeEffort::High,
            } => EngineerIntelligence::Ultra,
            Self::ClaudeOpus {
                effort: ClaudeEffort::High,
            } => EngineerIntelligence::Alt,
            Self::ResponsesSol(config) if config.effort == ReasoningEffort::Xhigh => {
                EngineerIntelligence::High
            }
            Self::ResponsesTerra(config) if config.effort == ReasoningEffort::Low => {
                EngineerIntelligence::Low
            }
            Self::ResponsesTerra(config) if config.effort == ReasoningEffort::High => {
                EngineerIntelligence::Cheap
            }
            Self::ResponsesGpt55(config)
            | Self::ResponsesSol(config)
            | Self::ResponsesTerra(config)
            | Self::CoordinatorTerra(config)
            | Self::CoordinatorSol(config)
            | Self::AdvisorSol(config)
            | Self::AdvisorTerra(config) => match config.effort {
                ReasoningEffort::Low => EngineerIntelligence::Low,
                ReasoningEffort::Medium => EngineerIntelligence::Medium,
                ReasoningEffort::High => EngineerIntelligence::High,
                ReasoningEffort::Xhigh => EngineerIntelligence::High,
            },
            Self::ClaudeFable { .. } | Self::ClaudeAdvisor { .. } => EngineerIntelligence::Ultra,
            Self::ClaudeOpus { .. } => EngineerIntelligence::Alt,
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
            | Self::AdvisorSol(config)
            | Self::AdvisorTerra(config) => Some(config),
            Self::ClaudeFable { .. } | Self::ClaudeOpus { .. } | Self::ClaudeAdvisor { .. } => None,
        }
    }

    pub fn deep_model(self) -> Option<InferenceModel> {
        match self {
            Self::ResponsesGpt55(_) => Some(InferenceModel::Gpt55),
            Self::ResponsesSol(_) | Self::AdvisorSol(_) => Some(InferenceModel::Gpt56Sol),
            Self::ResponsesLuna(_) => Some(InferenceModel::Gpt56Luna),
            Self::ResponsesTerra(_) | Self::CoordinatorTerra(_) | Self::AdvisorTerra(_) => {
                Some(InferenceModel::Gpt56Terra)
            }
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
            | Self::AdvisorSol(_)
            | Self::AdvisorTerra(_) => None,
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
            | Self::AdvisorSol(_)
            | Self::AdvisorTerra(_) => None,
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
    /// Opaque client-owned view configuration; the daemon stores and
    /// forwards it without interpreting a byte.
    fn view_config(&self) -> Vec<u8>;
    fn default_auth_namespace(&self) -> Option<String>;
    fn get_agent(&self, agent_id: AgentId) -> AgentRecord;
    fn list_agents(&self) -> Vec<(AgentId, AgentRecord)>;
    fn list_projects(&self) -> Vec<(Utf8PathBuf, ProjectRecord)>;
    fn agent_response_subscribers(&self, target: AgentId) -> Vec<AgentId>;
    fn is_agent_response_subscribed(&self, subscriber: AgentId, target: AgentId) -> bool;
    fn agent_events(&self, agent_id: AgentId) -> (AgentEventPos, Vec<AgentEvent<'static>>);
    fn agent_event_records(
        &self,
        agent_id: AgentId,
    ) -> (AgentEventPos, Vec<(AgentEventPos, AgentEvent<'static>)>);
    /// Historical presentation events whose source position remains reachable
    /// from the selected lineage. Their own event positions intentionally do
    /// not decide reachability: a result may arrive after newer input.
    fn agent_presentation_updates(&self, agent_id: AgentId) -> Vec<AgentPresentationUpdate>;
    /// Newest text-bearing event records, read from the selected lineage in
    /// reverse and bounded before decoding/building a Luna request.
    fn agent_presentation_source_tail(
        &self,
        agent_id: AgentId,
        max_source_bytes: usize,
    ) -> Vec<(AgentEventPos, AgentEvent<'static>)>;
    /// Samples for one model, bounded to the horizon plus its preceding
    /// baseline.
    fn quota_observations(
        &self,
        model: QuotaModel,
        since: UnixMillis,
    ) -> Vec<QuotaObservationRecord>;
    fn agent_usage(&self, agent_id: AgentId, since: UnixMillis) -> Vec<AgentUsageBucket>;
    fn agent_usage_total(&self, agent_id: AgentId) -> AgentUsageBucket;
    fn global_agent_usage(&self, since: UnixMillis) -> Vec<(AgentUsageModel, AgentUsageBucket)>;
}

#[allow(clippy::too_many_arguments)]
pub trait AgentWriteTxnExt {
    fn init_agent_tables(&mut self);

    /// Adds or removes one agent label; adding twice is a no-op.
    fn agent_label(&mut self, now: UnixMillis, agent_id: AgentId, label: &str, add: bool);

    fn set_view_config(&mut self, data: Vec<u8>);
    fn set_default_auth_namespace(&mut self, name: String);

    fn set_agent_display_name(&mut self, now: UnixMillis, agent_id: AgentId, name: String);
    fn set_agent_role(&mut self, agent_id: AgentId, role: AgentRole);
    fn set_agent_prompt_cache_key(&mut self, agent_id: AgentId, key: PromptCacheKey);
    fn set_agent_claude_rewind(&mut self, agent_id: AgentId, rewind: Option<ClaudeRewind>);
    fn complete_agent_claude_rewind(&mut self, agent_id: AgentId, session_id: Uuid);

    fn alloc_agent_id(&mut self) -> AgentId;

    fn upsert_project(&mut self, now: UnixMillis, path: &str, name: String, description: String);

    fn remove_project(&mut self, path: &str);

    fn append_agent_event(&mut self, at: AgentEventPos, event: &AgentEvent<'_>) -> AgentEventPos;
    fn append_agent_presentation_history(
        &mut self,
        at: AgentEventPos,
        update: &AgentPresentationUpdate,
    );

    /// Applies an update only when its source is still in the selected
    /// lineage. The returned cache is the acknowledged source of truth for a
    /// sidecar session; `None` means its result was made stale by rewind.
    fn apply_agent_presentation(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        update: &AgentPresentationUpdate,
    ) -> Option<AgentPresentationCache>;
    /// Rebuilds the denormalized cache after a lineage fork.
    fn rebuild_agent_presentation_cache(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
    ) -> AgentPresentationCache;
    fn fork_agent_lineage(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        parent: AgentEventPos,
    ) -> AgentEventPos;

    /// Records a turn end for attention purposes; resets the disposition to
    /// `Pending` — every finished turn demands a fresh disposition. An
    /// unexpired snooze survives: "quiet until T" holds across turn ends and
    /// the expiry broadcast resurfaces whatever finished meanwhile.
    fn record_agent_turn_end(&mut self, now: UnixMillis, agent_id: AgentId);

    /// Stores the one-shot classification of the last finished turn. The
    /// caller checks the disposition is still `Pending` so a late result
    /// never describes a turn the user already answered.
    fn record_agent_turn_report(&mut self, agent_id: AgentId, report: &TurnReport);

    /// Stamps the user's engagement with an agent (rail recency), keeps a
    /// one-line snippet of the message, and clears its disposition:
    /// replying is as much a verdict as acking.
    fn record_agent_user_message(&mut self, now: UnixMillis, agent_id: AgentId, text: &str);

    fn set_agent_disposition(&mut self, agent_id: AgentId, disposition: AgentDisposition);
    fn set_agent_response_subscription(
        &mut self,
        subscriber: AgentId,
        target: AgentId,
        subscribed: bool,
    );
    /// Records a changed whole-percentage weekly quota sample.
    fn record_quota_observation(&mut self, observation: QuotaObservationRecord) -> bool;
    fn add_agent_usage(&mut self, agent_id: AgentId, bucket: &AgentUsageBucket);
    fn replace_agent_usage(
        &mut self,
        buckets: &std::collections::HashMap<(AgentId, u64), AgentUsageBucket>,
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) trait AgentProfileWriteTxnExt {
    fn create_agent(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        display_name: Option<String>,
        workdirs: Vec<WorkspaceInfo>,
        mode: SessionBinding,
        runtime: AgentRuntime,
        parent_agent: Option<AgentId>,
    ) -> AgentEventPos;

    fn set_agent_profile(&mut self, agent_id: AgentId, role: AgentRole, binding: SessionBinding);
}

impl AgentProfileWriteTxnExt for WriteTxn {
    fn create_agent(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
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
                AgentRole::PM | AgentRole::WorkflowPM { .. } | AgentRole::Iris => {
                    AgentSpawnedBy::PM
                }
                AgentRole::Engineer { .. } | AgentRole::WorkflowEngineer { .. } => {
                    AgentSpawnedBy::Engineer
                }
                AgentRole::Advisor { .. } => panic!("Advisors cannot spawn agents"),
            }
        });
        let agent = AgentRecord {
            display_name,
            generated_title: None,
            activity: None,
            workdirs,
            created_at: now,
            updated_at: now,
            current_lineage: lineage_id,
            parent_agent,
            spawned_by,
            role: mode.agent_role(),
            binding: mode,
            runtime,
            claude_rewind: None,
            last_user_message: now,
            last_user_message_text: String::new(),
            labels: Vec::new(),
            disposition: AgentDisposition::Done,
            turn_report: None,
            user_interacted: false,
        };
        self.open_table(AGENTS)
            .insert(&agent_id, SenValue::borrowed(&agent));
        AgentEventPos::root(lineage_id)
    }

    fn set_agent_profile(&mut self, agent_id: AgentId, role: AgentRole, binding: SessionBinding) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent missing")
            .value()
            .into_owned();
        agent.role = role;
        agent.binding = binding;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
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

    fn view_config(&self) -> Vec<u8> {
        if !self.has_table("view_config") {
            return Vec::new();
        }
        self.open_table(VIEW_CONFIG)
            .get(&())
            .map(|value| value.value())
            .unwrap_or_default()
    }

    fn default_auth_namespace(&self) -> Option<String> {
        if !self.has_table("default_auth_namespace") {
            return None;
        }
        self.open_table(DEFAULT_AUTH_NAMESPACE)
            .get(&())
            .map(|value| value.value())
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

    fn agent_response_subscribers(&self, target: AgentId) -> Vec<AgentId> {
        self.open_table(AGENT_RESPONSE_SUBSCRIPTIONS)
            .iter()
            .filter_map(|(key, _)| {
                let key = key.value();
                (key.target == target).then_some(key.subscriber)
            })
            .collect()
    }

    fn is_agent_response_subscribed(&self, subscriber: AgentId, target: AgentId) -> bool {
        self.open_table(AGENT_RESPONSE_SUBSCRIPTIONS)
            .get(&AgentResponseSubscription { target, subscriber })
            .is_some()
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

    fn agent_presentation_updates(&self, agent_id: AgentId) -> Vec<AgentPresentationUpdate> {
        let agent = self.get_agent(agent_id);
        self.open_table(PRESENTATION_EVENTS)
            .iter()
            .map(|(_, update)| update.value().into_owned())
            .filter(|update| agent_event_visible_read(self, &agent, update.through))
            .collect()
    }

    fn agent_presentation_source_tail(
        &self,
        agent_id: AgentId,
        max_source_bytes: usize,
    ) -> Vec<(AgentEventPos, AgentEvent<'static>)> {
        let agent = self.get_agent(agent_id);
        let segments = agent_lineage_segments_read(self, &agent);
        let events = self.open_table(AGENT_EVENTS);
        let mut selected = Vec::new();
        let mut source_bytes = 0_usize;
        let mut scanned_events = 0_usize;
        for (lineage_id, end_seq) in segments {
            for (position, value) in events
                .range(
                    AgentEventPos::root(lineage_id)..=AgentEventPos {
                        lineage_id,
                        seq: end_seq,
                    },
                )
                .rev()
            {
                let position = position.value();
                if position.seq == end_seq && end_seq != u32::MAX {
                    continue;
                }
                if scanned_events >= MAX_PRESENTATION_SOURCE_SCANNED_EVENTS {
                    selected.reverse();
                    return selected;
                }
                scanned_events += 1;
                let event = value.value().into_owned();
                let bytes = presentation_event_text_bytes(&event);
                if bytes == 0 {
                    continue;
                }
                source_bytes = source_bytes.saturating_add(bytes.min(1024));
                selected.push((position, event));
                if source_bytes >= max_source_bytes {
                    selected.reverse();
                    return selected;
                }
            }
        }
        selected.reverse();
        selected
    }

    fn quota_observations(
        &self,
        model: QuotaModel,
        since: UnixMillis,
    ) -> Vec<QuotaObservationRecord> {
        let table = self.open_table(QUOTA_OBSERVATIONS);
        let mut before = BTreeMap::<Option<String>, QuotaObservationRecord>::new();
        let mut observations = Vec::new();
        for (_, value) in table.range(
            QuotaObservationKey {
                model,
                observed_at: 0,
            }..=QuotaObservationKey {
                model,
                observed_at: u64::MAX,
            },
        ) {
            let observation = value.value().into_owned();
            if observation.observed_at < since {
                before.insert(observation.auth_namespace.clone(), observation);
            } else {
                observations.push(observation);
            }
        }
        observations.extend(before.into_values());
        observations.sort_by_key(|observation| observation.observed_at);
        observations
    }

    fn agent_usage(&self, agent_id: AgentId, since: UnixMillis) -> Vec<AgentUsageBucket> {
        self.open_table(AGENT_USAGE_BUCKETS)
            .range(
                AgentUsageKey {
                    agent_id,
                    bucket_start_ms: since.0,
                }..=AgentUsageKey {
                    agent_id,
                    bucket_start_ms: u64::MAX,
                },
            )
            .map(|(_, value)| value.value().into_owned())
            .collect()
    }

    fn agent_usage_total(&self, agent_id: AgentId) -> AgentUsageBucket {
        self.open_table(AGENT_USAGE_TOTALS)
            .get(&agent_id)
            .map(|value| value.value().into_owned())
            .unwrap_or_default()
    }

    fn global_agent_usage(&self, since: UnixMillis) -> Vec<(AgentUsageModel, AgentUsageBucket)> {
        self.open_table(GLOBAL_AGENT_USAGE)
            .range(
                GlobalAgentUsageKey {
                    bucket_start_ms: since.0,
                    model: AgentUsageModel::GPT,
                }..=GlobalAgentUsageKey {
                    bucket_start_ms: u64::MAX,
                    model: AgentUsageModel::LUNA,
                },
            )
            .map(|(key, value)| (key.value().model, value.value().into_owned()))
            .collect()
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
        self.open_table(PRESENTATION_EVENTS);
        self.open_table(AGENTS);
        self.open_table(AGENT_RESPONSE_SUBSCRIPTIONS);
        self.open_table(PROJECTS);
        self.open_table(VIEW_CONFIG);
        self.open_table(DEFAULT_AUTH_NAMESPACE);
        self.open_table(QUOTA_OBSERVATIONS);
        self.open_table(AGENT_USAGE_BUCKETS);
        self.open_table(AGENT_USAGE_TOTALS);
        self.open_table(GLOBAL_AGENT_USAGE);
        let mut machine = self.open_table(MACHINE);
        if machine.get(&MACHINE_SEED_KEY).is_none() {
            machine.insert(&MACHINE_SEED_KEY, &rand::random::<u64>());
        }
    }

    fn agent_label(&mut self, now: UnixMillis, agent_id: AgentId, label: &str, add: bool) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        edit_labels(&mut agent.labels, label, add);
        agent.updated_at = now;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
    }

    fn set_view_config(&mut self, data: Vec<u8>) {
        self.open_table(VIEW_CONFIG).insert(&(), &data);
    }

    fn set_default_auth_namespace(&mut self, name: String) {
        self.open_table(DEFAULT_AUTH_NAMESPACE).insert(&(), &name);
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

    fn set_agent_prompt_cache_key(&mut self, agent_id: AgentId, key: PromptCacheKey) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent missing")
            .value()
            .into_owned();
        agent.runtime = AgentRuntime::Rho {
            prompt_cache_key: key,
        };
        agents.insert(&agent_id, SenValue::borrowed(&agent));
    }

    fn set_agent_claude_rewind(&mut self, agent_id: AgentId, rewind: Option<ClaudeRewind>) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent missing")
            .value()
            .into_owned();
        agent.claude_rewind = rewind;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
    }

    fn complete_agent_claude_rewind(&mut self, agent_id: AgentId, session_id: Uuid) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent missing")
            .value()
            .into_owned();
        agent.runtime = AgentRuntime::Claude { session_id };
        agent.claude_rewind = None;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
    }

    fn alloc_agent_id(&mut self) -> AgentId {
        let domain = AgentIdDomain(machine_seed(self));
        AgentId::from_counter(next_counter(self, CounterKey::LAST_AGENT_ID), &domain)
            .expect("agent id counter exceeds prefix-id capacity")
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

    fn append_agent_presentation_history(
        &mut self,
        at: AgentEventPos,
        update: &AgentPresentationUpdate,
    ) {
        self.open_table(PRESENTATION_EVENTS)
            .insert(&at, SenValue::borrowed(update));
    }

    fn apply_agent_presentation(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        update: &AgentPresentationUpdate,
    ) -> Option<AgentPresentationCache> {
        if !agent_event_visible_write(self, agent_id, update.through) {
            return None;
        }
        let cache = {
            let mut agents = self.open_table(AGENTS);
            let mut agent = agents
                .get(&agent_id)
                .expect("agent id missing")
                .value()
                .into_owned();
            match &update.generated_title {
                PresentationField::Unchanged => {}
                PresentationField::Set(title) if agent.display_name.is_none() => {
                    agent.generated_title = Some(title.clone());
                }
                PresentationField::Set(_) | PresentationField::Clear => {}
            }
            match &update.activity {
                PresentationField::Unchanged => {}
                PresentationField::Set(activity) => agent.activity = Some(activity.clone()),
                PresentationField::Clear => agent.activity = None,
            }
            agent.updated_at = agent.updated_at.max(now);
            let cache = AgentPresentationCache {
                generated_title: agent.generated_title.clone(),
                activity: agent.activity.clone(),
            };
            agents.insert(&agent_id, SenValue::borrowed(&agent));
            cache
        };
        Some(cache)
    }

    fn rebuild_agent_presentation_cache(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
    ) -> AgentPresentationCache {
        let updates = self
            .open_table(PRESENTATION_EVENTS)
            .iter()
            .map(|(_, update)| update.value().into_owned())
            .collect::<Vec<_>>();
        let updates = updates
            .into_iter()
            .filter(|update| agent_event_visible_write(self, agent_id, update.through))
            .collect::<Vec<_>>();
        let mut cache = AgentPresentationCache::default();
        for update in updates {
            match update.generated_title {
                PresentationField::Set(title) => cache.generated_title = Some(title),
                PresentationField::Clear => cache.generated_title = None,
                PresentationField::Unchanged => {}
            }
            match update.activity {
                PresentationField::Set(activity) => cache.activity = Some(activity),
                PresentationField::Clear => cache.activity = None,
                PresentationField::Unchanged => {}
            }
        }
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        agent.generated_title = cache.generated_title.clone();
        agent.activity = cache.activity.clone();
        agent.updated_at = agent.updated_at.max(now);
        agents.insert(&agent_id, SenValue::borrowed(&agent));
        cache
    }

    fn record_agent_turn_end(&mut self, now: UnixMillis, agent_id: AgentId) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        // A turn end puts the ball back in the user's court; it says
        // nothing about engagement, so `last_user_message` stays.
        agent.disposition = match agent.disposition {
            AgentDisposition::Snoozed { until } if until > now => {
                AgentDisposition::Snoozed { until }
            }
            _ => AgentDisposition::Pending,
        };
        // The previous turn's report describes a superseded final message,
        // and the activity label describes work that just stopped.
        agent.turn_report = None;
        agent.activity = None;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
    }

    fn record_agent_turn_report(&mut self, agent_id: AgentId, report: &TurnReport) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        agent.turn_report = Some(report.clone());
        // An FYI asks nothing of the user; settle it like a pressed Done so
        // it carries no attention weight while the row keeps its summary.
        if !report.needs_you {
            agent.disposition = AgentDisposition::Done;
        }
        agents.insert(&agent_id, SenValue::borrowed(&agent));
    }

    fn record_agent_user_message(&mut self, now: UnixMillis, agent_id: AgentId, text: &str) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        agent.last_user_message = now;
        agent.last_user_message_text = message_snippet(text);
        agent.user_interacted = true;
        // Replying is a verdict like acking — the ball moves to the agent's
        // court even if the turn hasn't started yet (queued delivery), so a
        // pending lamp must not linger.
        agent.disposition = AgentDisposition::Done;
        agent.turn_report = None;
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

    fn set_agent_response_subscription(
        &mut self,
        subscriber: AgentId,
        target: AgentId,
        subscribed: bool,
    ) {
        assert_ne!(subscriber, target, "an agent cannot subscribe to itself");
        let key = AgentResponseSubscription { target, subscriber };
        let mut subscriptions = self.open_table(AGENT_RESPONSE_SUBSCRIPTIONS);
        if subscribed {
            subscriptions.insert(&key, &());
        } else {
            subscriptions.remove(&key);
        }
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

    fn record_quota_observation(&mut self, observation: QuotaObservationRecord) -> bool {
        let mut key = QuotaObservationKey {
            model: observation.model,
            observed_at: observation.observed_at.0,
        };
        let mut table = self.open_table(QUOTA_OBSERVATIONS);
        let unchanged = table
            .range(
                QuotaObservationKey {
                    model: observation.model,
                    observed_at: 0,
                }..=QuotaObservationKey {
                    model: observation.model,
                    observed_at: u64::MAX,
                },
            )
            .rev()
            .map(|(_, value)| value.value().into_owned())
            .find(|old| old.auth_namespace == observation.auth_namespace)
            .is_some_and(|old| quota_observation_unchanged(&old, &observation));
        if unchanged {
            return false;
        }
        // Different namespaces can be observed in the same millisecond. Keep
        // the legacy fixed-width key compatible while avoiding replacement.
        while table.get(&key).is_some() {
            key.observed_at = key.observed_at.saturating_add(1);
        }
        table.insert(&key, SenValue::borrowed(&observation));
        true
    }

    fn add_agent_usage(&mut self, agent_id: AgentId, bucket: &AgentUsageBucket) {
        let mut bucket = bucket.clone();
        let record = self
            .open_table(AGENTS)
            .get(&agent_id)
            .expect("usage agent missing")
            .value()
            .into_owned();
        if bucket.model == AgentUsageModel::UNKNOWN {
            bucket.model = usage_model(&record);
        }
        let key = AgentUsageKey {
            agent_id,
            bucket_start_ms: bucket.bucket_start_ms,
        };
        let mut buckets = self.open_table(AGENT_USAGE_BUCKETS);
        let mut merged = buckets
            .get(&key)
            .map(|value| value.value().into_owned())
            .unwrap_or_else(|| AgentUsageBucket {
                bucket_start_ms: bucket.bucket_start_ms,
                ..AgentUsageBucket::default()
            });
        merged.add(&bucket);
        buckets.insert(&key, SenValue::borrowed(&merged));
        drop(buckets);

        let mut totals = self.open_table(AGENT_USAGE_TOTALS);
        let mut total = totals
            .get(&agent_id)
            .map(|value| value.value().into_owned())
            .unwrap_or_default();
        total.add(&bucket);
        total.bucket_start_ms = 0;
        totals.insert(&agent_id, SenValue::borrowed(&total));
        drop(totals);

        add_global_agent_usage(self, bucket.model, &bucket);
    }

    fn replace_agent_usage(
        &mut self,
        replacement: &std::collections::HashMap<(AgentId, u64), AgentUsageBucket>,
    ) {
        let mut buckets = self.open_table(AGENT_USAGE_BUCKETS);
        let old_keys = buckets
            .iter()
            .map(|(key, _)| key.value())
            .collect::<Vec<_>>();
        for key in old_keys {
            buckets.remove(&key);
        }
        for ((agent_id, bucket_start_ms), bucket) in replacement {
            buckets.insert(
                &AgentUsageKey {
                    agent_id: *agent_id,
                    bucket_start_ms: *bucket_start_ms,
                },
                SenValue::borrowed(bucket),
            );
        }
        drop(buckets);

        let mut by_agent = std::collections::HashMap::<AgentId, AgentUsageBucket>::new();
        for ((agent_id, _), bucket) in replacement {
            by_agent.entry(*agent_id).or_default().add(bucket);
        }
        let mut totals = self.open_table(AGENT_USAGE_TOTALS);
        let old_agents = totals
            .iter()
            .map(|(key, _)| key.value())
            .collect::<Vec<_>>();
        for agent_id in old_agents {
            totals.remove(&agent_id);
        }
        for (agent_id, mut total) in by_agent {
            total.bucket_start_ms = 0;
            totals.insert(&agent_id, SenValue::borrowed(&total));
        }
    }
}

fn agent_lineage_segments_read(read: &ReadTxn, agent: &AgentRecord) -> Vec<(AgentLineageId, u32)> {
    let mut segments = Vec::new();
    let mut lineage_id = agent.current_lineage;
    let mut end_seq = u32::MAX;
    let lineage_parents = read.open_table(LINEAGE_PARENTS);
    loop {
        segments.push((lineage_id, end_seq));
        let Some(parent) = lineage_parents.get(&lineage_id) else {
            break;
        };
        let parent = parent.value();
        lineage_id = parent.lineage_id;
        end_seq = parent.seq;
    }
    segments
}

fn agent_event_visible_read(read: &ReadTxn, agent: &AgentRecord, position: AgentEventPos) -> bool {
    agent_lineage_segments_read(read, agent)
        .into_iter()
        .find_map(|(lineage_id, end_seq)| (lineage_id == position.lineage_id).then_some(end_seq))
        .is_some_and(|end_seq| end_seq == u32::MAX || position.seq < end_seq)
}

fn agent_event_visible_write(
    write: &mut WriteTxn,
    agent_id: AgentId,
    position: AgentEventPos,
) -> bool {
    let agent = write
        .open_table(AGENTS)
        .get(&agent_id)
        .expect("agent id missing")
        .value()
        .into_owned();
    let mut lineage_id = agent.current_lineage;
    let mut end_seq = u32::MAX;
    let lineage_parents = write.open_table(LINEAGE_PARENTS);
    loop {
        if lineage_id == position.lineage_id {
            return end_seq == u32::MAX || position.seq < end_seq;
        }
        let Some(parent) = lineage_parents.get(&lineage_id) else {
            break;
        };
        let parent = parent.value();
        lineage_id = parent.lineage_id;
        end_seq = parent.seq;
    }
    false
}

fn presentation_event_text_bytes(event: &AgentEvent<'_>) -> usize {
    match event {
        AgentEvent::Queued(crate::QueuedItem {
            kind: crate::QueuedItemKind::UserMessage { content, .. },
            ..
        }) => content
            .iter()
            .filter_map(|part| match part {
                rho_core::ContentPart::Text { text } => Some(text.len()),
                rho_core::ContentPart::Image { .. } => None,
            })
            .sum(),
        AgentEvent::InferenceResponse { items, .. } => items
            .iter()
            .filter_map(|item| match item {
                crate::InferenceResponseItem::AssistantMessage { content, .. } => Some(
                    content
                        .iter()
                        .filter_map(|part| match part {
                            rho_core::ContentPart::Text { text } => Some(text.len()),
                            rho_core::ContentPart::Image { .. } => None,
                        })
                        .sum::<usize>(),
                ),
                _ => None,
            })
            .sum(),
        AgentEvent::ClaudePresentationSource { text, .. } => text.len(),
        AgentEvent::ToolResult { .. }
        | AgentEvent::Queued(_)
        | AgentEvent::Dequeued { .. }
        | AgentEvent::QueueCleared
        | AgentEvent::PresentationUpdated { .. } => 0,
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

/// One display line from a user message: whitespace collapsed, cut at a
/// character boundary. Long enough to recall what was asked, short enough
/// for a summary row.
fn message_snippet(text: &str) -> String {
    let mut snippet = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if let Some((limit, _)) = snippet.char_indices().nth(160) {
        snippet.truncate(limit);
        snippet.push('\u{2026}');
    }
    snippet
}

/// Adds or removes a label, keeping the set free of duplicates.
fn edit_labels(labels: &mut Vec<String>, label: &str, add: bool) {
    labels.retain(|existing| existing != label);
    if add {
        labels.push(label.to_owned());
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
