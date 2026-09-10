//! Raw redb schema for persisted agents.
//!
//! One log per agent (`agent_log`), dense positions from zero; one journal
//! (`journal`) naming every append in the order it landed. Nothing derived
//! is stored: what an agent is now is folded from its log on read
//! (`AGENT-LOG-DESIGN.md`, "the mirror is a pure function of the raw log").

use std::collections::{BTreeMap, HashMap};

use redb::{TableDefinition, Value as _};
use redb_derive::{Key, Value as RedbValue};
use rho_core::UnixMs;
use rho_db::{ReadTxn, Sen, SenValue, WriteTxn};
use rho_inference::PromptCacheKey;
pub(crate) use rho_inference::config::{InferenceModel, InferenceProfile, ReasoningEffort};
pub use rho_ui_proto::mirror::{AgentWant, PresentationField, Seq, TurnEdge, TurnOutcome};
use rho_workspaces::WorkspaceInfo;
use senax_encoder::{Decode, Encode, Pack, Unpack};
use uuid::Uuid;

use crate::AgentEvent;
use crate::mirror::{Feed, Journal, LogAppended};

const COUNTERS: TableDefinition<CounterKey, u64> = TableDefinition::new("counters");
/// Singleton row holding this database's random machine seed (see
/// [`PrefixIdDomain::machine_seed`]), generated once at init.
const MACHINE: TableDefinition<u8, u64> = TableDefinition::new("machine");
const MACHINE_SEED_KEY: u8 = 0;
pub(crate) const FORMAT: TableDefinition<(), String> = TableDefinition::new("format");
/// The redb savepoint taken before a migration, by the hop it guards
/// (`from->to`), so `rho debug rollback` can put the store back.
const RECOVERY: TableDefinition<String, u64> = TableDefinition::new("recovery_savepoints");
/// Every agent's raw log: one row per event, keyed by agent then
/// position, so a range read gives one agent and nothing else. Never
/// rewritten; a rewind is a row like any other.
const AGENT_LOG: TableDefinition<(AgentId, u64), Sen<AgentEvent<'static>>> =
    TableDefinition::new("agent_log");
/// The order every append landed in, across agents: `seq -> (agent, pos)`,
/// written in the same transaction as the row it names. What a client
/// follows to stay current.
const JOURNAL: TableDefinition<u64, (AgentId, u64)> = TableDefinition::new("journal");
/// Where each Claude agent's transcript rows stop: the session file they
/// came from and one past the last line copied. Written with the rows.
const MAX_PRESENTATION_SOURCE_SCANNED_EVENTS: usize = 256;
const AGENT_RESPONSE_SUBSCRIPTIONS: TableDefinition<AgentResponseSubscription, ()> =
    TableDefinition::new("agent_response_subscriptions");
const QUOTA_OBSERVATIONS: TableDefinition<QuotaObservationKey, Sen<QuotaObservationRecord>> =
    TableDefinition::new("quota_observations_by_model_time");
const AGENT_USAGE_BUCKETS: TableDefinition<AgentUsageKey, Sen<AgentUsageBucket>> =
    TableDefinition::new("agent_usage_by_agent_time");
const AGENT_USAGE_TOTALS: TableDefinition<AgentId, Sen<AgentUsageBucket>> =
    TableDefinition::new("agent_usage_totals");
const GLOBAL_AGENT_USAGE: TableDefinition<GlobalAgentUsageKey, Sen<AgentUsageBucket>> =
    TableDefinition::new("agent_usage_by_time_provider");
/// The Claude account every agent runs on. One row: the account is global,
/// and switching it moves every agent at its next turn.
const CLAUDE_ACCOUNT: TableDefinition<(), String> = TableDefinition::new("claude_account");
const CURRENT_AGENT_DB_FORMAT: &str = "3ac1e7d4";
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
    pub const GEMINI: Self = Self(6);
    pub const ASTRA: Self = Self(7);

    pub fn name(self) -> &'static str {
        match self {
            Self::GPT => "gpt",
            Self::FABLE => "fable",
            Self::OPUS => "opus",
            Self::TERRA => "terra",
            Self::LUNA => "luna",
            Self::GEMINI => "gemini",
            Self::ASTRA => "astra",
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

fn usage_model(config: &AgentConfig) -> AgentUsageModel {
    usage_model_of(&config.runtime, config.binding)
}

/// The model a runtime and binding bill as.
pub(crate) fn usage_model_of(runtime: &AgentRuntime, binding: SessionBinding) -> AgentUsageModel {
    match runtime {
        AgentRuntime::Rho { .. } => match binding.deep_model() {
            Some(InferenceModel::Gpt6Astra) => AgentUsageModel::ASTRA,
            Some(InferenceModel::Gpt56Terra) => AgentUsageModel::TERRA,
            Some(InferenceModel::Gpt56Luna) => AgentUsageModel::LUNA,
            Some(InferenceModel::Gemini37FlashLow) => AgentUsageModel::GEMINI,
            _ => AgentUsageModel::GPT,
        },
        AgentRuntime::Claude { .. } => match binding.claude_model() {
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

pub use rho_core::{AdvisorIntelligence, AgentId, AgentIdDomain, AgentRole, EngineerIntelligence};

/// A position in one agent's log: dense from zero, never reused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode)]
pub struct AgentEventPos {
    pub pos: u64,
}

impl AgentEventPos {
    pub const ZERO: Self = Self { pos: 0 };

    pub fn new(pos: u64) -> Self {
        Self { pos }
    }

    pub fn next(self) -> Self {
        Self {
            pos: self
                .pos
                .checked_add(1)
                .expect("agent log position overflow"),
        }
    }

    /// The position before this one; zero stays zero.
    pub fn previous(self) -> Self {
        Self {
            pos: self.pos.saturating_sub(1),
        }
    }
}

impl From<AgentEventPos> for rho_ui_proto::mirror::AgentPos {
    fn from(pos: AgentEventPos) -> Self {
        Self(pos.pos)
    }
}

impl From<rho_ui_proto::mirror::AgentPos> for AgentEventPos {
    fn from(pos: rho_ui_proto::mirror::AgentPos) -> Self {
        Self { pos: pos.0 }
    }
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

/// The title and activity a reader sees, and what seeds a fresh Luna turn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentPresentationCache {
    pub generated_title: Option<String>,
    pub activity: Option<String>,
}

pub type UnixMillis = UnixMs;

/// What the agent is, folded from `Created` and the config events that
/// follow it. Nothing here is written directly: a change is an event
/// first and reaches the head through the fold.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentConfig {
    pub role: AgentRole,
    pub(crate) binding: SessionBinding,
    pub runtime: AgentRuntime,
    /// The agent's working set: where it works, primary workdir first.
    /// Fixed at spawn - never removed or reordered, because accumulated
    /// model context assumes the entries stay valid. Managed workspace ids
    /// are repository-local and allocated by jj; joined agents retain the
    /// owning agent's id for that repository.
    pub workdirs: Vec<WorkspaceInfo>,
    pub spawned_by: AgentSpawnedBy,
    /// The name the spawner gave. A generated title is never made for an
    /// agent that has one, and it always beats a generated title.
    pub spawn_name: Option<String>,
    pub created_at: UnixMillis,
    /// A message-only Claude rewind whose destination transcript has not yet
    /// been durably materialized and verified. The old runtime remains
    /// authoritative until then.
    pub claude_rewind: Option<ClaudeRewind>,
}

/// What an agent is now: the fold of its whole log, hidden rows included
/// (a rewind takes back history, not configuration). Made on read, never
/// stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentHead {
    pub config: AgentConfig,
    /// The sidecar title. A spawn name always takes precedence.
    pub generated_title: Option<String>,
    /// The last durable, model-derived activity label.
    pub activity: Option<String>,
    /// Whether a turn is running, folded from the log's turn events.
    pub turn_running: bool,
    /// The agent that spawned this one.
    pub parent: Option<AgentId>,
    /// The user has messaged this agent directly (agent mail doesn't count).
    /// Sticky: once engaged, the agent's turn ends are the user's court even
    /// for a sub-agent, so it gets turn reports like a root.
    pub user_interacted: bool,
    pub last_turn_ended: Option<UnixMillis>,
    /// Where the next event goes: one past the last row, hidden or not.
    pub next: AgentEventPos,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct TurnReport {
    pub needs_you: bool,
    /// Activity-shaped few-word label of the outcome. Defaulted so records
    /// written before the rename from `one_liner` still decode.
    #[senax(default)]
    pub summary: String,
}

impl AgentHead {
    pub fn config(&self) -> AgentRole {
        self.config.role
    }

    /// The primary workdir (entry 0): default cwd, prompt header, UI label.
    pub fn primary_workdir(&self) -> &WorkspaceInfo {
        self.config.primary_workdir()
    }

    /// The agent's name for a reader: what the spawner called it, else what
    /// the sidecar made of it.
    pub fn title(&self) -> Option<&str> {
        self.config
            .spawn_name
            .as_deref()
            .or(self.generated_title.as_deref())
    }
}

impl AgentConfig {
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode)]
pub enum AgentSpawnedBy {
    #[default]
    Direct,
    Engineer,
}

/// `AgentSpawnedBy` as rows wrote it while the PM role existed; a PM
/// parent reads as an Engineer parent now.
#[derive(Decode)]
enum StoredAgentSpawnedBy {
    Direct,
    PM,
    Engineer,
}

impl senax_encoder::Decoder for AgentSpawnedBy {
    fn decode(reader: &mut impl bytes::Buf) -> Result<Self, senax_encoder::EncoderError> {
        Ok(match StoredAgentSpawnedBy::decode(reader)? {
            StoredAgentSpawnedBy::Direct => Self::Direct,
            StoredAgentSpawnedBy::PM | StoredAgentSpawnedBy::Engineer => Self::Engineer,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Pack, Unpack)]
pub enum SessionBinding {
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
    /// Reduced function-tool Gemini agent. Appended for persisted
    /// compatibility.
    AntigravityFlashLow(InferenceProfile),
    /// GPT-6 Astra-backed engineer.
    ResponsesAstra(InferenceProfile),
    /// GPT-6 Astra-backed advisor; distinct so its role survives pinning.
    AdvisorAstra(InferenceProfile),
    ResponsesSolPython(InferenceProfile),
}

/// `SessionBinding` as rows wrote it while the PM role existed. The
/// coordinator bindings were Sol and Terra with a PM prompt; they read as
/// the plain Sol and Terra bindings now.
#[derive(Decode)]
enum StoredSessionBinding {
    ResponsesGpt55(InferenceProfile),
    ClaudeFable { effort: ClaudeEffort },
    ClaudeOpus { effort: ClaudeEffort },
    ResponsesSol(InferenceProfile),
    ResponsesLuna(InferenceProfile),
    ResponsesTerra(InferenceProfile),
    CoordinatorTerra(InferenceProfile),
    CoordinatorSol(InferenceProfile),
    ClaudeAdvisor { effort: ClaudeEffort },
    AdvisorSol(InferenceProfile),
    AdvisorTerra(InferenceProfile),
    AntigravityFlashLow(InferenceProfile),
    ResponsesAstra(InferenceProfile),
    AdvisorAstra(InferenceProfile),
    ResponsesSolPython(InferenceProfile),
}

impl senax_encoder::Decoder for SessionBinding {
    fn decode(reader: &mut impl bytes::Buf) -> Result<Self, senax_encoder::EncoderError> {
        use StoredSessionBinding as Stored;
        Ok(match Stored::decode(reader)? {
            Stored::ResponsesGpt55(config) => Self::ResponsesGpt55(config),
            Stored::ClaudeFable { effort } => Self::ClaudeFable { effort },
            Stored::ClaudeOpus { effort } => Self::ClaudeOpus { effort },
            Stored::ResponsesSol(config) | Stored::CoordinatorSol(config) => {
                Self::ResponsesSol(config)
            }
            Stored::ResponsesLuna(config) => Self::ResponsesLuna(config),
            Stored::ResponsesTerra(config) | Stored::CoordinatorTerra(config) => {
                Self::ResponsesTerra(config)
            }
            Stored::ClaudeAdvisor { effort } => Self::ClaudeAdvisor { effort },
            Stored::AdvisorSol(config) => Self::AdvisorSol(config),
            Stored::AdvisorTerra(config) => Self::AdvisorTerra(config),
            Stored::AntigravityFlashLow(config) => Self::AntigravityFlashLow(config),
            Stored::ResponsesAstra(config) => Self::ResponsesAstra(config),
            Stored::AdvisorAstra(config) => Self::AdvisorAstra(config),
            Stored::ResponsesSolPython(config) => Self::ResponsesSolPython(config),
        })
    }
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
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Mini,
            } => SessionBinding::ResponsesLuna(InferenceProfile {
                fast_mode: true,
                code_mode: false,
                ..deep(ReasoningEffort::Xhigh)
            }),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Low,
            } => SessionBinding::ResponsesTerra(deep(ReasoningEffort::Low)),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Cheap,
            } => SessionBinding::ResponsesTerra(deep(ReasoningEffort::High)),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Medium,
            } => SessionBinding::ResponsesSol(deep(ReasoningEffort::Medium)),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Python,
            } => SessionBinding::ResponsesSolPython(deep(ReasoningEffort::Medium)),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::High,
            } => SessionBinding::ResponsesAstra(deep(ReasoningEffort::Medium)),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Ultra,
            } => SessionBinding::ClaudeFable {
                effort: ClaudeEffort::High,
            },
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Alt,
            } => SessionBinding::ClaudeOpus {
                effort: ClaudeEffort::Medium,
            },
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Gemini,
            } => SessionBinding::AntigravityFlashLow(InferenceProfile {
                effort: ReasoningEffort::Medium,
                fast_mode: false,
                code_mode: false,
            }),
            AgentRole::Advisor {
                intelligence: AdvisorIntelligence::Medium,
            } => SessionBinding::AdvisorSol(deep(ReasoningEffort::High)),
            AgentRole::Advisor {
                intelligence: AdvisorIntelligence::Cheap,
            } => SessionBinding::AdvisorTerra(deep(ReasoningEffort::Xhigh)),
            AgentRole::Advisor {
                intelligence: AdvisorIntelligence::High,
            } => SessionBinding::AdvisorAstra(deep(ReasoningEffort::Medium)),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ClaudeEffort {
    Medium,
    Xhigh,
    High,
}

impl SessionBinding {
    pub fn agent_role(self) -> AgentRole {
        if matches!(self, Self::ResponsesAstra(_)) {
            return AgentRole::Engineer {
                intelligence: EngineerIntelligence::High,
            };
        } else if matches!(self, Self::ClaudeAdvisor { .. } | Self::AdvisorAstra(_)) {
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
            Self::ResponsesSolPython(_) => EngineerIntelligence::Python,
            Self::AntigravityFlashLow(_) => EngineerIntelligence::Gemini,
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
            Self::ResponsesAstra(_) | Self::AdvisorAstra(_) => {
                unreachable!("Astra role binding returned above")
            }
            Self::ResponsesGpt55(config)
            | Self::ResponsesSol(config)
            | Self::ResponsesTerra(config)
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
            | Self::ResponsesSolPython(config)
            | Self::ResponsesLuna(config)
            | Self::ResponsesTerra(config)
            | Self::ResponsesAstra(config)
            | Self::AdvisorAstra(config)
            | Self::AdvisorSol(config)
            | Self::AdvisorTerra(config) => Some(config),
            Self::AntigravityFlashLow(config) => Some(config),
            Self::ClaudeFable { .. } | Self::ClaudeOpus { .. } | Self::ClaudeAdvisor { .. } => None,
        }
    }

    pub fn deep_model(self) -> Option<InferenceModel> {
        match self {
            Self::ResponsesGpt55(_) => Some(InferenceModel::Gpt55),
            Self::ResponsesSol(_) | Self::ResponsesSolPython(_) | Self::AdvisorSol(_) => {
                Some(InferenceModel::Gpt56Sol)
            }
            Self::ResponsesLuna(_) => Some(InferenceModel::Gpt56Luna),
            Self::ResponsesTerra(_) | Self::AdvisorTerra(_) => Some(InferenceModel::Gpt56Terra),
            Self::ResponsesAstra(_) | Self::AdvisorAstra(_) => Some(InferenceModel::Gpt6Astra),
            Self::AntigravityFlashLow(_) => Some(InferenceModel::Gemini37FlashLow),
            Self::ClaudeFable { .. } | Self::ClaudeOpus { .. } | Self::ClaudeAdvisor { .. } => None,
        }
    }

    pub fn claude_model(self) -> Option<rho_claude::Model> {
        match self {
            Self::ClaudeFable { .. } | Self::ClaudeAdvisor { .. } => Some(rho_claude::Model::Fable),
            Self::ClaudeOpus { .. } => Some(rho_claude::Model::Opus),
            Self::ResponsesGpt55(_)
            | Self::ResponsesSol(_)
            | Self::ResponsesSolPython(_)
            | Self::ResponsesLuna(_)
            | Self::ResponsesTerra(_)
            | Self::ResponsesAstra(_)
            | Self::AdvisorAstra(_)
            | Self::AdvisorSol(_)
            | Self::AdvisorTerra(_) => None,
            Self::AntigravityFlashLow(_) => None,
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
            | Self::ResponsesSolPython(_)
            | Self::ResponsesLuna(_)
            | Self::ResponsesTerra(_)
            | Self::ResponsesAstra(_)
            | Self::AdvisorAstra(_)
            | Self::AdvisorSol(_)
            | Self::AdvisorTerra(_) => None,
            Self::AntigravityFlashLow(_) => None,
        }
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
    /// The fold of one agent's log. Panics when there is no such agent.
    fn get_agent(&self, agent_id: AgentId) -> AgentHead;
    fn try_get_agent(&self, agent_id: AgentId) -> Option<AgentHead>;
    fn agent_exists(&self, agent_id: AgentId) -> bool;
    /// Every agent, by the first row of its log; touches one row per agent.
    fn list_agent_ids(&self) -> Vec<AgentId>;
    /// Every agent's fold: the whole store. For conversions and tools.
    fn list_agents(&self) -> Vec<(AgentId, AgentHead)>;
    /// Who spawned an agent, read from its creation alone.
    fn agent_parent(&self, agent_id: AgentId) -> Option<AgentId>;
    fn agent_response_subscribers(&self, target: AgentId) -> Vec<AgentId>;
    fn is_agent_response_subscribed(&self, subscriber: AgentId, target: AgentId) -> bool;
    /// The agent's history as it stands: every row a later `Rewound` did
    /// not take back, oldest first, and where the next row goes.
    fn agent_events(&self, agent_id: AgentId) -> (AgentEventPos, Vec<AgentEvent<'static>>);
    fn agent_event_records(
        &self,
        agent_id: AgentId,
    ) -> (AgentEventPos, Vec<(AgentEventPos, AgentEvent<'static>)>);
    /// One row, hidden or not.
    fn agent_event(&self, agent_id: AgentId, pos: AgentEventPos) -> Option<AgentEvent<'static>>;
    /// Newest text-bearing visible rows, read backward and bounded before
    /// decoding/building a Luna request.
    fn agent_presentation_source_tail(
        &self,
        agent_id: AgentId,
        max_source_bytes: usize,
    ) -> Vec<(AgentEventPos, AgentEvent<'static>)>;
    /// How far the journal runs; zero when nothing has been appended.
    fn journal_head(&self) -> Seq;
    /// Journal entries after `since`, at most `limit`, with the rows they
    /// name.
    fn journal_since(
        &self,
        since: Seq,
        limit: usize,
    ) -> Vec<(Seq, AgentId, AgentEventPos, AgentEvent<'static>)>;
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
    /// The Claude account agents run on, which is the default account until
    /// someone switches it.
    fn claude_account(&self) -> String;
}

#[allow(clippy::too_many_arguments)]
pub trait AgentWriteTxnExt {
    fn init_agent_tables(&mut self);

    /// Appends one event at the agent's tail and names it in the journal.
    /// The tail is read from the table, not from a runtime's cursor, so
    /// any writer may append at any time. Returns where it landed.
    fn append_agent_event(&mut self, agent_id: AgentId, event: &AgentEvent<'_>) -> AgentEventPos;

    fn set_agent_role(&mut self, agent_id: AgentId, role: AgentRole);
    fn set_agent_prompt_cache_key(&mut self, agent_id: AgentId, key: PromptCacheKey);
    fn set_agent_claude_rewind(&mut self, agent_id: AgentId, rewind: Option<ClaudeRewind>);
    fn complete_agent_claude_rewind(&mut self, agent_id: AgentId, session_id: Uuid);

    fn alloc_agent_id(&mut self) -> AgentId;

    /// Applies an update only when its source is still visible. The
    /// returned cache is the acknowledged source of truth for a sidecar
    /// session; `None` means its result was made stale by a rewind.
    fn apply_agent_presentation(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        update: &AgentPresentationUpdate,
    ) -> Option<AgentPresentationCache>;

    /// Takes back history from `to` on: told at a new position, so what
    /// the agent walked away from stays in the log
    /// (`DECISION-history-only-branches`). Returns where it was told.
    fn rewind_agent(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        to: AgentEventPos,
    ) -> AgentEventPos;

    fn tell_turn(&mut self, now: UnixMillis, agent_id: AgentId, edge: TurnEdge);

    fn tell_wants(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        want: AgentWant,
        summary: Option<String>,
    );

    fn set_agent_response_subscription(
        &mut self,
        subscriber: AgentId,
        target: AgentId,
        subscribed: bool,
    );
    /// Records a changed whole-percentage weekly quota sample.
    fn record_quota_observation(&mut self, observation: QuotaObservationRecord) -> bool;
    /// Puts every agent on `account` from its next turn. Running processes
    /// keep the account their namespace has mounted until then.
    fn set_claude_account(&mut self, account: &str);
    fn add_agent_usage(&mut self, agent_id: AgentId, bucket: &AgentUsageBucket);
    fn replace_agent_usage(
        &mut self,
        buckets: &std::collections::HashMap<(AgentId, u64), AgentUsageBucket>,
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) trait AgentProfileWriteTxnExt {
    /// The agent's first event. The role is the caller's: a binding
    /// implies one, but a spawner may ask for a narrower role than the
    /// binding's default.
    fn create_agent(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        spawn_name: Option<String>,
        workdirs: Vec<WorkspaceInfo>,
        role: AgentRole,
        mode: SessionBinding,
        runtime: AgentRuntime,
        parent_agent: Option<AgentId>,
    );

    fn set_agent_profile(&mut self, agent_id: AgentId, role: AgentRole, binding: SessionBinding);
}

impl AgentProfileWriteTxnExt for WriteTxn {
    fn create_agent(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        spawn_name: Option<String>,
        workdirs: Vec<WorkspaceInfo>,
        role: AgentRole,
        mode: SessionBinding,
        runtime: AgentRuntime,
        parent_agent: Option<AgentId>,
    ) {
        assert!(!workdirs.is_empty(), "agent needs at least one workdir");
        let spawned_by = parent_agent.map_or(AgentSpawnedBy::Direct, |parent| {
            match agent_head_write(self, parent)
                .expect("parent agent must exist")
                .config
                .role
            {
                AgentRole::Engineer { .. } => AgentSpawnedBy::Engineer,
                AgentRole::Advisor { .. } => panic!("Advisors cannot spawn agents"),
            }
        });
        let created = AgentEvent::Created {
            role,
            binding: mode,
            runtime,
            workdirs,
            spawned_by,
            spawn_name,
            created_at: now,
            parent: parent_agent,
        };
        let at = self.append_agent_event(agent_id, &created);
        assert_eq!(at, AgentEventPos::ZERO, "agent {agent_id:?} already exists");
    }

    fn set_agent_profile(&mut self, agent_id: AgentId, role: AgentRole, binding: SessionBinding) {
        self.append_agent_event(
            agent_id,
            &AgentEvent::RoleChanged {
                role,
                binding: Some(binding),
                at: UnixMillis::now(),
            },
        );
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

    fn get_agent(&self, agent_id: AgentId) -> AgentHead {
        self.try_get_agent(agent_id)
            .unwrap_or_else(|| panic!("agent id missing: {agent_id:?}"))
    }

    fn try_get_agent(&self, agent_id: AgentId) -> Option<AgentHead> {
        let log = self.open_table(AGENT_LOG);
        fold_head(rows(log.range(agent_range(agent_id))))
    }

    fn agent_exists(&self, agent_id: AgentId) -> bool {
        self.open_table(AGENT_LOG).get(&(agent_id, 0)).is_some()
    }

    fn list_agent_ids(&self) -> Vec<AgentId> {
        let log = self.open_table(AGENT_LOG);
        let mut ids = Vec::new();
        let mut cursor = log.iter().next().map(|(key, _)| key.value().0);
        while let Some(agent_id) = cursor {
            ids.push(agent_id);
            // Jump past this agent's last possible key straight to the
            // next agent's first: one descent per agent, not one per row.
            cursor = log
                .range((agent_id, u64::MAX)..)
                .next()
                .map(|(key, _)| key.value().0)
                .filter(|next| *next != agent_id);
        }
        ids
    }

    fn list_agents(&self) -> Vec<(AgentId, AgentHead)> {
        self.list_agent_ids()
            .into_iter()
            .filter_map(|agent_id| Some((agent_id, self.try_get_agent(agent_id)?)))
            .collect()
    }

    fn agent_parent(&self, agent_id: AgentId) -> Option<AgentId> {
        match self.agent_event(agent_id, AgentEventPos::ZERO)? {
            AgentEvent::Created { parent, .. } => parent,
            _ => None,
        }
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
        let log = self.open_table(AGENT_LOG);
        visible_rows(rows(log.range(agent_range(agent_id))))
    }

    fn agent_event(&self, agent_id: AgentId, pos: AgentEventPos) -> Option<AgentEvent<'static>> {
        self.open_table(AGENT_LOG)
            .get(&(agent_id, pos.pos))
            .map(|value| value.value().into_owned())
    }

    fn agent_presentation_source_tail(
        &self,
        agent_id: AgentId,
        max_source_bytes: usize,
    ) -> Vec<(AgentEventPos, AgentEvent<'static>)> {
        let log = self.open_table(AGENT_LOG);
        let mut hidden = Hidden::default();
        let mut selected = Vec::new();
        let mut source_bytes = 0_usize;
        let mut scanned_events = 0_usize;
        for (position, event) in rows(log.range(agent_range(agent_id)).rev()) {
            if !hidden.visible(position, &event) {
                continue;
            }
            if scanned_events >= MAX_PRESENTATION_SOURCE_SCANNED_EVENTS {
                break;
            }
            scanned_events += 1;
            let bytes = presentation_event_text_bytes(&event);
            if bytes == 0 {
                continue;
            }
            source_bytes = source_bytes.saturating_add(bytes.min(1024));
            selected.push((position, event));
            if source_bytes >= max_source_bytes {
                break;
            }
        }
        selected.reverse();
        selected
    }

    fn journal_head(&self) -> Seq {
        Seq(self
            .open_table(JOURNAL)
            .iter()
            .next_back()
            .map(|(key, _)| key.value())
            .unwrap_or(0))
    }

    fn journal_since(
        &self,
        since: Seq,
        limit: usize,
    ) -> Vec<(Seq, AgentId, AgentEventPos, AgentEvent<'static>)> {
        let journal = self.open_table(JOURNAL);
        let log = self.open_table(AGENT_LOG);
        journal
            .range(since.0.saturating_add(1)..)
            .take(limit)
            .map(|(seq, row)| {
                let (agent_id, pos) = row.value();
                let event = log
                    .get(&(agent_id, pos))
                    .expect("journal names a row that exists")
                    .value()
                    .into_owned();
                (Seq(seq.value()), agent_id, AgentEventPos::new(pos), event)
            })
            .collect()
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

    fn claude_account(&self) -> String {
        self.open_table(CLAUDE_ACCOUNT)
            .get(&())
            .map(|value| value.value())
            .unwrap_or_else(|| rho_claude::accounts::DEFAULT_ACCOUNT.to_owned())
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
        self.open_table(AGENT_LOG);
        self.open_table(JOURNAL);
        self.open_table(AGENT_RESPONSE_SUBSCRIPTIONS);
        self.open_table(QUOTA_OBSERVATIONS);
        self.open_table(AGENT_USAGE_BUCKETS);
        self.open_table(AGENT_USAGE_TOTALS);
        self.open_table(GLOBAL_AGENT_USAGE);
        self.open_table(CLAUDE_ACCOUNT);
        let mut machine = self.open_table(MACHINE);
        if machine.get(&MACHINE_SEED_KEY).is_none() {
            machine.insert(&MACHINE_SEED_KEY, &rand::random::<u64>());
        }
    }
    fn append_agent_event(&mut self, agent_id: AgentId, event: &AgentEvent<'_>) -> AgentEventPos {
        let pos = {
            let log = self.open_table(AGENT_LOG);
            log.range(agent_range(agent_id))
                .next_back()
                .map(|(key, _)| AgentEventPos::new(key.value().1).next())
                .unwrap_or(AgentEventPos::ZERO)
        };
        debug_assert!(
            (pos == AgentEventPos::ZERO) == matches!(event, AgentEvent::Created { .. }),
            "creation is the first row of a log and nothing else is"
        );
        self.open_table(AGENT_LOG)
            .insert(&(agent_id, pos.pos), SenValue::borrowed(event));
        let seq = {
            let mut journal = self.open_table(JOURNAL);
            let seq = journal
                .iter()
                .next_back()
                .map(|(key, _)| key.value() + 1)
                .unwrap_or(1);
            journal.insert(&seq, &(agent_id, pos.pos));
            seq
        };
        // Told after the commit, so a listener woken by it finds the row.
        if let Some(journal) = self.observer::<Journal>() {
            let appends = journal.sender();
            let appended = LogAppended {
                seq: Seq(seq),
                agent_id,
                pos: pos.into(),
                event: crate::mirror::strip(event),
            };
            self.after_commit(move || {
                let _ = appends.send(Feed::Appended(appended));
            });
        }
        pos
    }

    fn set_agent_role(&mut self, agent_id: AgentId, role: AgentRole) {
        self.append_agent_event(
            agent_id,
            &AgentEvent::RoleChanged {
                role,
                binding: None,
                at: UnixMillis::now(),
            },
        );
    }

    fn set_agent_prompt_cache_key(&mut self, agent_id: AgentId, key: PromptCacheKey) {
        self.append_agent_event(
            agent_id,
            &AgentEvent::RuntimeRebound {
                change: crate::RuntimeChange::PromptCacheKey(key),
                at: UnixMillis::now(),
            },
        );
    }

    fn set_agent_claude_rewind(&mut self, agent_id: AgentId, rewind: Option<ClaudeRewind>) {
        self.append_agent_event(
            agent_id,
            &AgentEvent::RuntimeRebound {
                change: crate::RuntimeChange::ClaudeRewindPending(rewind),
                at: UnixMillis::now(),
            },
        );
    }

    fn complete_agent_claude_rewind(&mut self, agent_id: AgentId, session_id: Uuid) {
        self.append_agent_event(
            agent_id,
            &AgentEvent::RuntimeRebound {
                change: crate::RuntimeChange::ClaudeRewound { session_id },
                at: UnixMillis::now(),
            },
        );
    }

    fn alloc_agent_id(&mut self) -> AgentId {
        let domain = AgentIdDomain(machine_seed(self));
        AgentId::from_counter(next_counter(self, CounterKey::LAST_AGENT_ID), &domain)
            .expect("agent id counter exceeds prefix-id capacity")
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
        self.append_agent_event(
            agent_id,
            &AgentEvent::Presented {
                title: update.generated_title.clone(),
                activity: update.activity.clone(),
                at: now,
            },
        );
        let head = agent_head_write(self, agent_id).expect("agent id missing");
        Some(AgentPresentationCache {
            generated_title: head.generated_title,
            activity: head.activity,
        })
    }

    fn rewind_agent(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        to: AgentEventPos,
    ) -> AgentEventPos {
        assert!(
            to != AgentEventPos::ZERO,
            "an agent's creation cannot be rewound away"
        );
        assert!(
            agent_event_visible_write(self, agent_id, to),
            "rewind target {to:?} of {agent_id:?} is not in its history"
        );
        self.append_agent_event(agent_id, &AgentEvent::Rewound { to, at: now })
    }

    fn tell_turn(&mut self, now: UnixMillis, agent_id: AgentId, edge: TurnEdge) {
        self.append_agent_event(agent_id, &AgentEvent::Turn { edge, at: now });
    }

    fn tell_wants(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        want: AgentWant,
        summary: Option<String>,
    ) {
        self.append_agent_event(
            agent_id,
            &AgentEvent::Wants {
                want,
                summary,
                at: now,
            },
        );
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
    fn set_claude_account(&mut self, account: &str) {
        self.open_table(CLAUDE_ACCOUNT)
            .insert(&(), account.to_owned());
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
        if bucket.model == AgentUsageModel::UNKNOWN {
            let head = agent_head_write(self, agent_id).expect("usage agent missing");
            bucket.model = usage_model(&head.config);
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

/// Every key of one agent's log.
fn agent_range(agent_id: AgentId) -> std::ops::RangeInclusive<(AgentId, u64)> {
    (agent_id, 0)..=(agent_id, u64::MAX)
}

/// Rows as `(position, event)`, from either kind of table iterator.
fn rows<'a>(
    iter: impl Iterator<
        Item = (
            redb::AccessGuard<'a, (AgentId, u64)>,
            redb::AccessGuard<'a, Sen<AgentEvent<'static>>>,
        ),
    >,
) -> impl Iterator<Item = (AgentEventPos, AgentEvent<'static>)> {
    iter.map(|(key, value)| {
        (
            AgentEventPos::new(key.value().1),
            value.value().into_owned(),
        )
    })
}

/// The rows a reader sees, oldest first: every `Rewound` takes back what
/// stands from `to` on, itself included in what remains. Also where the
/// next row goes, hidden rows counted.
fn visible_rows(
    all: impl Iterator<Item = (AgentEventPos, AgentEvent<'static>)>,
) -> (AgentEventPos, Vec<(AgentEventPos, AgentEvent<'static>)>) {
    let mut visible = Vec::new();
    let mut next = AgentEventPos::ZERO;
    for (pos, event) in all {
        next = pos.next();
        if let AgentEvent::Rewound { to, .. } = &event {
            let to = *to;
            visible.retain(|(kept, _)| *kept < to);
        }
        visible.push((pos, event));
    }
    (next, visible)
}

/// Walking a log backward: which positions a later `Rewound` hides.
#[derive(Default)]
struct Hidden {
    from: Option<u64>,
}

impl Hidden {
    /// Whether the row at `pos` is visible, noting the rewind it may be.
    /// Rows must come newest first.
    fn visible(&mut self, pos: AgentEventPos, event: &AgentEvent<'_>) -> bool {
        if self.from.is_some_and(|from| pos.pos >= from) {
            return false;
        }
        if let AgentEvent::Rewound { to, .. } = event {
            self.from = Some(self.from.map_or(to.pos, |from| from.min(to.pos)));
        }
        true
    }
}

/// The fold of one agent's rows, hidden ones included; `None` when the
/// log does not begin with a creation (no such agent).
fn fold_head(all: impl Iterator<Item = (AgentEventPos, AgentEvent<'static>)>) -> Option<AgentHead> {
    let mut head: Option<AgentHead> = None;
    for (pos, event) in all {
        match &mut head {
            None => {
                let AgentEvent::Created { parent, .. } = &event else {
                    return None;
                };
                head = Some(AgentHead {
                    config: created_config(&event),
                    generated_title: None,
                    activity: None,
                    turn_running: false,
                    parent: *parent,
                    user_interacted: false,
                    last_turn_ended: None,
                    next: pos.next(),
                });
            }
            Some(head) => {
                fold_agent_head(head, &event);
                head.next = pos.next();
            }
        }
    }
    head
}

fn agent_head_write(write: &mut WriteTxn, agent_id: AgentId) -> Option<AgentHead> {
    let log = write.open_table(AGENT_LOG);
    fold_head(rows(log.range(agent_range(agent_id))))
}

/// Whether `position` still stands in the agent's history, read from a
/// write transaction: a backward walk from the tail, so only the rows
/// after it are decoded.
fn agent_event_visible_write(
    write: &mut WriteTxn,
    agent_id: AgentId,
    position: AgentEventPos,
) -> bool {
    let log = write.open_table(AGENT_LOG);
    let mut hidden = Hidden::default();
    for (pos, event) in rows(log.range(agent_range(agent_id)).rev()) {
        let visible = hidden.visible(pos, &event);
        if pos <= position {
            return pos == position && visible;
        }
    }
    false
}
fn presentation_event_text_bytes(event: &AgentEvent<'_>) -> usize {
    match event {
        AgentEvent::Accepted(crate::QueuedInput {
            kind: crate::InputKind::Message { content },
            ..
        }) => text_bytes(content),
        AgentEvent::Replied { blocks, .. } => blocks
            .iter()
            .map(|block| match block {
                rho_core::ContextBlock::InferenceResponse { items, .. } => {
                    assistant_text_bytes(items)
                }
                _ => 0,
            })
            .sum(),
        AgentEvent::ClaudePresentationSource { text, .. } => text.len(),
        AgentEvent::Transcript {
            line:
                crate::TranscriptLine::User { text } | crate::TranscriptLine::Assistant { text, .. },
            ..
        } => text.len(),
        AgentEvent::Transcript { .. }
        | AgentEvent::Accepted(_)
        | AgentEvent::Sent { .. }
        | AgentEvent::QueueCleared
        | AgentEvent::Cleared { .. }
        | AgentEvent::Turn { .. }
        | AgentEvent::Presented { .. }
        | AgentEvent::Wants { .. }
        | AgentEvent::Rewound { .. }
        | AgentEvent::Failed { .. }
        | AgentEvent::Created { .. }
        | AgentEvent::RoleChanged { .. }
        | AgentEvent::WorkdirAdded { .. }
        | AgentEvent::RuntimeRebound { .. } => 0,
    }
}

fn text_bytes(content: &[rho_core::ContentPart]) -> usize {
    content
        .iter()
        .filter_map(|part| match part {
            rho_core::ContentPart::Text { text } => Some(text.len()),
            rho_core::ContentPart::Image { .. } => None,
        })
        .sum()
}

fn assistant_text_bytes(items: &[crate::InferenceResponseItem]) -> usize {
    items
        .iter()
        .filter_map(|item| match item {
            crate::InferenceResponseItem::AssistantMessage { content, .. } => {
                Some(text_bytes(content))
            }
            _ => None,
        })
        .sum()
}

/// The config a `Created` event states. Panics on any other event: only
/// creation can begin a config.
fn created_config(event: &AgentEvent<'_>) -> AgentConfig {
    let AgentEvent::Created {
        role,
        binding,
        runtime,
        workdirs,
        spawned_by,
        spawn_name,
        created_at,
        ..
    } = event
    else {
        panic!("config can only begin at a Created event");
    };
    AgentConfig {
        role: *role,
        binding: *binding,
        runtime: runtime.clone(),
        workdirs: workdirs.clone(),
        spawned_by: *spawned_by,
        spawn_name: spawn_name.clone(),
        created_at: *created_at,
        claude_rewind: None,
    }
}

/// One event's effect on the head.
fn fold_agent_head(head: &mut AgentHead, event: &AgentEvent<'_>) {
    let mut presented = |title: &PresentationField, activity: &PresentationField| {
        match title {
            PresentationField::Set(title) => {
                head.generated_title = Some(title.clone());
            }
            PresentationField::Clear => head.generated_title = None,
            PresentationField::Unchanged => {}
        }
        match activity {
            PresentationField::Set(activity) => head.activity = Some(activity.clone()),
            PresentationField::Clear => head.activity = None,
            PresentationField::Unchanged => {}
        }
    };
    match event {
        AgentEvent::Created { .. } => head.config = created_config(event),
        AgentEvent::RoleChanged { role, binding, .. } => {
            head.config.role = *role;
            if let Some(binding) = binding {
                head.config.binding = *binding;
            }
        }
        AgentEvent::WorkdirAdded { workdir, .. } => head.config.workdirs.push(workdir.clone()),
        AgentEvent::RuntimeRebound { change, .. } => match change {
            crate::RuntimeChange::ClaudeRewindPending(rewind) => {
                head.config.claude_rewind = rewind.clone();
            }
            crate::RuntimeChange::ClaudeRewound { session_id } => {
                head.config.runtime = AgentRuntime::Claude {
                    session_id: *session_id,
                };
                head.config.claude_rewind = None;
            }
            crate::RuntimeChange::PromptCacheKey(key) => {
                head.config.runtime = AgentRuntime::Rho {
                    prompt_cache_key: *key,
                };
            }
        },
        AgentEvent::Presented {
            title, activity, ..
        } => presented(title, activity),
        AgentEvent::Turn { edge, at } => match edge {
            TurnEdge::Started => head.turn_running = true,
            TurnEdge::Ended(_) => {
                head.turn_running = false;
                // The activity label describes work that just stopped.
                head.activity = None;
                head.last_turn_ended = Some(*at);
            }
        },
        AgentEvent::Accepted(crate::QueuedInput {
            source: rho_core::MessageSender::User,
            kind: crate::InputKind::Message { .. },
            ..
        })
        | AgentEvent::ClaudePresentationSource {
            speaker: crate::PresentationSpeaker::User,
            ..
        }
        | AgentEvent::Transcript {
            line: crate::TranscriptLine::User { .. },
            ..
        } => head.user_interacted = true,
        AgentEvent::Accepted(_)
        | AgentEvent::Sent { .. }
        | AgentEvent::Replied { .. }
        | AgentEvent::QueueCleared
        | AgentEvent::Cleared { .. }
        | AgentEvent::Wants { .. }
        | AgentEvent::Rewound { .. }
        | AgentEvent::Failed { .. }
        | AgentEvent::ClaudePresentationSource { .. }
        | AgentEvent::Transcript { .. } => {}
    }
}

/// The hop the store would make on open, if its format is behind.
pub fn pending_migration(read: &rho_db::ReadTxn) -> Option<(&'static str, &'static str)> {
    if !read.has_table("format") {
        return None;
    }
    let format = read
        .open_table(FORMAT)
        .get(&())
        .map(|value| value.value())?;
    if format == CURRENT_AGENT_DB_FORMAT {
        return None;
    }
    AGENT_DB_MIGRATIONS
        .iter()
        .find(|migration| migration.from == format)
        .map(|migration| (migration.from, migration.to))
}

/// Opens the store for use: a savepoint first when a migration is due,
/// so the store can be put back if the migrated build turns out wrong,
/// then the tables and the migration itself.
pub async fn prepare(db: &rho_db::RhoDb) {
    if let Some((from, to)) = pending_migration(&db.read()) {
        let key = format!("{from}->{to}");
        let id = db
            .persistent_savepoint(|write, id| {
                write.open_table(RECOVERY).insert(&key, &id);
            })
            .await;
        eprintln!(
            "rho-agent: savepoint {id} taken before migrating the store {from} -> {to}; \
             `rho debug rollback` puts it back"
        );
    }
    let mut write = db.write().await;
    write.init_agent_tables();
    let started = std::time::Instant::now();
    write.commit();
    if started.elapsed() > std::time::Duration::from_secs(1) {
        eprintln!("rho-agent: store committed in {:?}", started.elapsed());
    }
}

/// Every persistent savepoint in the store, with the migration hop it
/// was recorded for when `prepare` took it.
pub async fn savepoints(db: &rho_db::RhoDb) -> Vec<(u64, Option<String>)> {
    let mut write = db.write().await;
    let ids = write.persistent_savepoints();
    if ids.is_empty() {
        return Vec::new();
    }
    let recorded = write
        .open_table(RECOVERY)
        .iter()
        .map(|(key, value)| (value.value(), key.value()))
        .collect::<HashMap<_, _>>();
    ids.into_iter()
        .map(|id| (id, recorded.get(&id).cloned()))
        .collect()
}

/// Drops the savepoints `prepare` recorded, once the migration they guard
/// has been verified and nothing will roll back to them. Returns the ids
/// dropped. A dropped savepoint stops pinning the pages the migration
/// freed, so `rho debug compact` can give them back.
pub async fn forget_savepoints(db: &rho_db::RhoDb) -> Vec<u64> {
    let mut write = db.write().await;
    let recorded = write
        .open_table(RECOVERY)
        .iter()
        .map(|(key, value)| (key.value(), value.value()))
        .collect::<Vec<_>>();
    let mut dropped = Vec::new();
    for (key, id) in recorded {
        write.delete_persistent_savepoint(id);
        write.open_table(RECOVERY).remove(&key);
        dropped.push(id);
    }
    write.commit();
    dropped
}

/// Drops every persistent savepoint `prepare` did not record: leftovers
/// of older builds, each pinning every page freed since it was taken.
/// Returns the ids dropped.
pub async fn drop_stale_savepoints(db: &rho_db::RhoDb) -> Vec<u64> {
    let stale = savepoints(db)
        .await
        .into_iter()
        .filter_map(|(id, hop)| hop.is_none().then_some(id))
        .collect::<Vec<_>>();
    if stale.is_empty() {
        return stale;
    }
    let mut write = db.write().await;
    for id in &stale {
        write.delete_persistent_savepoint(*id);
    }
    write.commit();
    stale
}

/// Puts the store back as it was before its last migration, from the
/// savepoint `prepare` took, and drops that savepoint. Returns the hop
/// undone. Nothing else may have the store open.
pub async fn rollback(db: &rho_db::RhoDb) -> anyhow::Result<String> {
    let mut write = db.write().await;
    let recorded = if write.persistent_savepoints().is_empty() {
        Vec::new()
    } else {
        write
            .open_table(RECOVERY)
            .iter()
            .map(|(key, value)| (key.value(), value.value()))
            .collect::<Vec<_>>()
    };
    let Some((hop, id)) = recorded.into_iter().max_by_key(|(_, id)| *id) else {
        anyhow::bail!("no migration savepoint is recorded in this store");
    };
    anyhow::ensure!(
        write.restore_persistent_savepoint(id),
        "savepoint {id} for {hop} is no longer in the store"
    );
    write.delete_persistent_savepoint(id);
    write.commit();
    Ok(hop)
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
pub(crate) mod tests;
