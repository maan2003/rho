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
use crate::story::StoryEvent;

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
const MAX_PRESENTATION_SOURCE_SCANNED_EVENTS: usize = 256;
/// The fold over an agent's logs: config, the latest presentation, where
/// its story stands. A cache, never a source; [`rebuild_agent_head`]
/// makes it again from the log.
const AGENT_HEADS: TableDefinition<AgentId, Sen<AgentHead>> = TableDefinition::new("agent_heads");
/// The daemon's remaining opinions about an agent, which the raw log
/// cannot rebuild because it carries no wall clock. Deleted in slice B,
/// when attention moves to the client and the story log carries times
/// (`AGENT-LOG-DESIGN.md`).
const AGENT_ATTENTION: TableDefinition<AgentId, Sen<AgentAttention>> =
    TableDefinition::new("agent_attention_until_slice_b");
/// The story log: the events a person reads, one row per position, in
/// the order they happened. Range-read per agent; never rewritten.
const AGENT_STORY: TableDefinition<StoryKey, Sen<StoryEvent>> = TableDefinition::new("agent_story");
/// Which raw event each story event was told from, for the events that
/// came from one. Daemon-only and never on the wire: a rewind reads it to
/// say how far back the story a reader keeps still holds.
const AGENT_STORY_SOURCE: TableDefinition<StoryKey, AgentEventPos> =
    TableDefinition::new("agent_story_source");
const AGENT_RESPONSE_SUBSCRIPTIONS: TableDefinition<AgentResponseSubscription, ()> =
    TableDefinition::new("agent_response_subscriptions");
const PROJECTS: TableDefinition<String, Sen<ProjectRecord>> = TableDefinition::new("projects");
/// Opaque client-owned view configuration (see
/// [`AgentReadTxnExt::view_config`]). A client setting the daemon only
/// keeps; it moves into the GUI's own db in slice B.
const VIEW_CONFIG: TableDefinition<(), Vec<u8>> = TableDefinition::new("view_config");
const QUOTA_OBSERVATIONS: TableDefinition<QuotaObservationKey, Sen<QuotaObservationRecord>> =
    TableDefinition::new("quota_observations_by_model_time");
const AGENT_USAGE_BUCKETS: TableDefinition<AgentUsageKey, Sen<AgentUsageBucket>> =
    TableDefinition::new("agent_usage_by_agent_time");
const AGENT_USAGE_TOTALS: TableDefinition<AgentId, Sen<AgentUsageBucket>> =
    TableDefinition::new("agent_usage_totals");
const GLOBAL_AGENT_USAGE: TableDefinition<GlobalAgentUsageKey, Sen<AgentUsageBucket>> =
    TableDefinition::new("agent_usage_by_time_provider");
const CURRENT_AGENT_DB_FORMAT: &str = "b1e40c93";
const QUOTA_RESET_JITTER_SECONDS: u64 = 60;

struct AgentDbMigration {
    from: &'static str,
    to: &'static str,
    migrate: fn(&mut WriteTxn),
}

/// Empty until the next format change needs one. The record→log
/// migration that filled it has run on the user's store and come out.
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
    pub const GEMINI: Self = Self(6);

    pub fn name(self) -> &'static str {
        match self {
            Self::GPT => "gpt",
            Self::FABLE => "fable",
            Self::OPUS => "opus",
            Self::TERRA => "terra",
            Self::LUNA => "luna",
            Self::GEMINI => "gemini",
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
    match config.runtime {
        AgentRuntime::Rho { .. } => match config.binding.deep_model() {
            Some(InferenceModel::Gpt56Terra) => AgentUsageModel::TERRA,
            Some(InferenceModel::Gpt56Luna) => AgentUsageModel::LUNA,
            Some(InferenceModel::Gemini37FlashLow) => AgentUsageModel::GEMINI,
            _ => AgentUsageModel::GPT,
        },
        AgentRuntime::Claude { .. } => match config.binding.claude_model() {
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
pub struct ProjectRecord {
    pub name: String,
    pub description: String,
    pub created_at: UnixMillis,
}

/// The position of an agent's story log, the log a person reads. The
/// story itself arrives in slice B (`AGENT-LOG-DESIGN.md`); until then
/// every head carries the same zero.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Key,
    RedbValue,
    Encode,
    Decode,
)]
pub struct StoryPos(pub u64);

impl StoryPos {
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// One agent's story, ordered by position: the key sorts by agent first,
/// so a range read gives one agent's events and nothing else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Key, RedbValue)]
pub struct StoryKey {
    agent_id: AgentId,
    pos: StoryPos,
}

/// What the agent is, folded from `Created` and the config events that
/// follow it. Nothing here is written directly: a change is an event
/// first and reaches the head through the fold.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
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

/// The daemon's cache of the fold over an agent's logs. Lost or doubted,
/// it is made again from the log by [`AgentWriteTxnExt::rebuild_agent_head`];
/// it is never the source of anything.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AgentHead {
    pub config: AgentConfig,
    /// How far the story log runs. A placeholder until slice B writes one.
    #[senax(default)]
    pub story_pos: StoryPos,
    /// The sidecar title. A spawn name always takes precedence.
    pub generated_title: Option<String>,
    /// The last durable, model-derived activity label.
    pub activity: Option<String>,
    /// Whether a turn is running, folded from the story's turn events.
    /// Durable because a reader asks it of every agent, including the
    /// ones no runtime is loaded for.
    #[senax(default)]
    pub turn_running: bool,
    /// Whether this agent's story has been built. False on every agent
    /// migrated from before the story existed, until the background
    /// backfill reaches it (or a load forces it first).
    #[senax(default)]
    pub story_built: bool,
    pub current_lineage: AgentLineageId,
}

/// The last of the daemon's opinions about an agent, plus the two times
/// the raw log cannot rebuild (it carries no wall clock). This whole
/// table goes in slice B, when attention is the client's and every story
/// event carries its `at`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct AgentAttention {
    pub updated_at: UnixMillis,
    /// Who spawned this agent, by id. A parent is the store's `Parent`
    /// fact now, but the daemon's own behaviour still needs the id after a
    /// restart: agent mail replies to it, a PM lists its children by it,
    /// and a sub-agent's turn reports and titles are gated on having one.
    /// Those consumers move to the store in slice B and this goes with the
    /// table.
    pub parent_agent: Option<AgentId>,
    /// When the user last sent this agent a message; rail recency seed.
    /// Turn ends raise attention but leave this alone - replying is the
    /// engagement signal, finishing is the agent's schedule.
    pub last_user_message: UnixMillis,
    /// A one-line snippet of that message, so summaries can say what the
    /// user last asked without replaying the transcript.
    pub last_user_message_text: String,
    /// When the most recent turn returned the agent to idle. Unlike the
    /// disposition, this is a durable chronology fact.
    pub last_turn_ended: Option<UnixMillis>,
    /// The user's verdict on the last finished turn; attention is derived
    /// from this plus live agent state, never stored.
    pub disposition: AgentDisposition,
    /// One-shot classification of the last finished turn. Cleared when the
    /// user replies - the report describes a ball that is no longer in the
    /// user's court.
    pub turn_report: Option<TurnReport>,
    /// The user has messaged this agent directly (agent mail doesn't count).
    /// Sticky: once engaged, the agent's turn ends are the user's court even
    /// for a sub-agent, so it gets attention and turn reports like a root.
    pub user_interacted: bool,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub enum AgentSpawnedBy {
    #[default]
    Direct,
    PM,
    Engineer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
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
    /// Reduced function-tool Gemini agent. Appended for persisted
    /// compatibility.
    AntigravityFlashLow(InferenceProfile),
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
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Gemini,
            }
            | AgentRole::WorkflowEngineer {
                intelligence: EngineerIntelligence::Gemini,
                ..
            } => SessionBinding::AntigravityFlashLow(InferenceProfile {
                effort: ReasoningEffort::Medium,
                fast_mode: false,
                code_mode: false,
            }),
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
pub enum ClaudeEffort {
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
            Self::AntigravityFlashLow(config) => Some(config),
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
            | Self::ResponsesLuna(_)
            | Self::ResponsesTerra(_)
            | Self::CoordinatorTerra(_)
            | Self::CoordinatorSol(_)
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
            | Self::ResponsesLuna(_)
            | Self::ResponsesTerra(_)
            | Self::CoordinatorTerra(_)
            | Self::CoordinatorSol(_)
            | Self::AdvisorSol(_)
            | Self::AdvisorTerra(_) => None,
            Self::AntigravityFlashLow(_) => None,
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
    fn list_projects(&self) -> Vec<(Utf8PathBuf, ProjectRecord)>;
    fn get_agent(&self, agent_id: AgentId) -> AgentHead;
    fn list_agents(&self) -> Vec<(AgentId, AgentHead)>;
    fn agent_attention(&self, agent_id: AgentId) -> AgentAttention;
    /// One agent's story from `from` onward, oldest first. The whole
    /// story when `from` is zero.
    fn agent_story(&self, agent_id: AgentId, from: StoryPos) -> Vec<(StoryPos, StoryEvent)>;
    /// The title and activity a reader sees now: the head's fold of the
    /// story's `Titled` and `Activity` events.
    fn agent_presentation_cache(&self, agent_id: AgentId) -> AgentPresentationCache;
    fn agent_response_subscribers(&self, target: AgentId) -> Vec<AgentId>;
    fn is_agent_response_subscribed(&self, subscriber: AgentId, target: AgentId) -> bool;
    fn agent_events(&self, agent_id: AgentId) -> (AgentEventPos, Vec<AgentEvent<'static>>);
    fn agent_event_records(
        &self,
        agent_id: AgentId,
    ) -> (AgentEventPos, Vec<(AgentEventPos, AgentEvent<'static>)>);
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

    fn set_view_config(&mut self, data: Vec<u8>);

    fn upsert_project(&mut self, now: UnixMillis, path: &str, name: String, description: String);

    fn remove_project(&mut self, path: &str);

    /// Appends one event to the story a person reads and folds it into
    /// the head. The position is the head's, so the story is written
    /// once and never rewritten.
    fn append_agent_story(&mut self, agent_id: AgentId, event: &StoryEvent) -> StoryPos;

    /// The same, for an event told from one raw event, remembering which
    /// one so a rewind can say where the story a reader keeps stops.
    fn append_agent_story_from(
        &mut self,
        agent_id: AgentId,
        event: &StoryEvent,
        through: AgentEventPos,
    ) -> StoryPos;

    /// Appends a config event at the agent's true tail and folds it into
    /// the head. The tail is read from the table, not from a runtime's
    /// cursor, so this is safe while a turn is running.
    fn append_agent_config_event(&mut self, agent_id: AgentId, event: &AgentEvent<'_>);

    /// Makes the head again from the log, as if the cached one were lost.
    fn rebuild_agent_head(&mut self, agent_id: AgentId) -> AgentHead;

    /// Whether this agent's story has been built, read inside the write
    /// transaction that is about to build it.
    fn agent_story_built(&mut self, agent_id: AgentId) -> bool;

    /// Marks this agent's story complete, so the backfill never visits it
    /// again and a runtime may append live events to it.
    fn mark_agent_story_built(&mut self, agent_id: AgentId);

    fn set_agent_role(&mut self, agent_id: AgentId, role: AgentRole);
    fn set_agent_prompt_cache_key(&mut self, agent_id: AgentId, key: PromptCacheKey);
    fn set_agent_claude_rewind(&mut self, agent_id: AgentId, rewind: Option<ClaudeRewind>);
    fn complete_agent_claude_rewind(&mut self, agent_id: AgentId, session_id: Uuid);

    fn alloc_agent_id(&mut self) -> AgentId;

    fn append_agent_event(&mut self, at: AgentEventPos, event: &AgentEvent<'_>) -> AgentEventPos;

    /// Applies an update only when its source is still in the selected
    /// lineage. The returned cache is the acknowledged source of truth for a
    /// sidecar session; `None` means its result was made stale by rewind.
    fn apply_agent_presentation(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        update: &AgentPresentationUpdate,
    ) -> Option<AgentPresentationCache>;

    /// Forks the agent onto a new lineage whose parent is `parent`, so a
    /// rewind hides the abandoned tail without rewriting a position.
    fn fork_agent_lineage(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        parent: AgentEventPos,
    ) -> AgentEventPos;

    fn record_agent_turn_end(&mut self, now: UnixMillis, agent_id: AgentId);

    /// Fills the chronology fact for records created before it existed.
    /// Never overwrites a turn end recorded by a current runtime.
    fn backfill_agent_last_turn_ended(&mut self, agent_id: AgentId, at: UnixMillis) -> bool;

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
    /// The agent's first event and the head it folds to. The role is the
    /// caller's: a binding implies one, but a spawner may ask for a
    /// narrower role than the binding's default.
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
    ) -> AgentEventPos;

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
    ) -> AgentEventPos {
        assert!(!workdirs.is_empty(), "agent needs at least one workdir");
        let lineage_id = AgentLineageId(next_counter(self, CounterKey::LAST_LINEAGE_ID));
        self.open_table(LINEAGE_PARENTS);
        let spawned_by = parent_agent.map_or(AgentSpawnedBy::Direct, |parent| {
            match self
                .open_table(AGENT_HEADS)
                .get(&parent)
                .expect("parent agent must exist")
                .value()
                .into_owned()
                .config
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
        // Creation is the first event of the log, and the head is its fold.
        let created = AgentEvent::Created {
            role,
            binding: mode,
            runtime,
            workdirs,
            spawned_by,
            spawn_name,
            created_at: now,
        };
        let at = AgentEventPos::root(lineage_id);
        self.open_table(AGENT_EVENTS)
            .insert(&at, SenValue::borrowed(&created));
        let head = AgentHead {
            config: created_config(&created),
            story_pos: StoryPos::default(),
            turn_running: false,
            // An agent born after the story exists has one from its first
            // event; nothing is ever backfilled for it.
            story_built: true,
            generated_title: None,
            activity: None,
            current_lineage: lineage_id,
        };
        self.open_table(AGENT_HEADS)
            .insert(&agent_id, SenValue::borrowed(&head));
        for event in crate::story::from_raw_event(&created, now) {
            self.append_agent_story_from(agent_id, &event, at);
        }
        self.open_table(AGENT_ATTENTION).insert(
            &agent_id,
            SenValue::borrowed(&AgentAttention {
                updated_at: now,
                parent_agent,
                last_user_message: now,
                disposition: AgentDisposition::Done,
                ..AgentAttention::default()
            }),
        );
        at.next()
    }

    fn set_agent_profile(&mut self, agent_id: AgentId, role: AgentRole, binding: SessionBinding) {
        self.append_agent_config_event(
            agent_id,
            &AgentEvent::RoleChanged {
                role,
                binding: Some(binding),
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

    fn view_config(&self) -> Vec<u8> {
        if !self.has_table("view_config") {
            return Vec::new();
        }
        self.open_table(VIEW_CONFIG)
            .get(&())
            .map(|value| value.value())
            .unwrap_or_default()
    }

    fn list_projects(&self) -> Vec<(Utf8PathBuf, ProjectRecord)> {
        self.open_table(PROJECTS)
            .iter()
            .map(|(key, value)| (Utf8PathBuf::from(key.value()), value.value().into_owned()))
            .collect()
    }

    fn get_agent(&self, agent_id: AgentId) -> AgentHead {
        self.open_table(AGENT_HEADS)
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned()
    }

    fn list_agents(&self) -> Vec<(AgentId, AgentHead)> {
        self.open_table(AGENT_HEADS)
            .iter()
            .map(|(key, value)| (key.value(), value.value().into_owned()))
            .collect()
    }

    fn agent_attention(&self, agent_id: AgentId) -> AgentAttention {
        self.open_table(AGENT_ATTENTION)
            .get(&agent_id)
            .map(|value| value.value().into_owned())
            .unwrap_or_default()
    }

    fn agent_presentation_cache(&self, agent_id: AgentId) -> AgentPresentationCache {
        let head = self.get_agent(agent_id);
        AgentPresentationCache {
            generated_title: head.generated_title,
            activity: head.activity,
        }
    }

    fn agent_story(&self, agent_id: AgentId, from: StoryPos) -> Vec<(StoryPos, StoryEvent)> {
        self.open_table(AGENT_STORY)
            .range(
                StoryKey {
                    agent_id,
                    pos: from,
                }..=StoryKey {
                    agent_id,
                    pos: StoryPos(u64::MAX),
                },
            )
            .map(|(key, value)| (key.value().pos, value.value().into_owned()))
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

    fn agent_presentation_source_tail(
        &self,
        agent_id: AgentId,
        max_source_bytes: usize,
    ) -> Vec<(AgentEventPos, AgentEvent<'static>)> {
        let agent = self.get_agent(agent_id);
        let segments = agent_lineage_segments_read(self, agent.current_lineage);
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
        self.open_table(AGENT_HEADS);
        self.open_table(AGENT_ATTENTION);
        self.open_table(AGENT_STORY);
        self.open_table(AGENT_STORY_SOURCE);
        self.open_table(AGENT_RESPONSE_SUBSCRIPTIONS);
        self.open_table(PROJECTS);
        self.open_table(VIEW_CONFIG);
        self.open_table(QUOTA_OBSERVATIONS);
        self.open_table(AGENT_USAGE_BUCKETS);
        self.open_table(AGENT_USAGE_TOTALS);
        self.open_table(GLOBAL_AGENT_USAGE);
        let mut machine = self.open_table(MACHINE);
        if machine.get(&MACHINE_SEED_KEY).is_none() {
            machine.insert(&MACHINE_SEED_KEY, &rand::random::<u64>());
        }
    }

    fn set_view_config(&mut self, data: Vec<u8>) {
        self.open_table(VIEW_CONFIG).insert(&(), &data);
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

    fn append_agent_story(&mut self, agent_id: AgentId, event: &StoryEvent) -> StoryPos {
        let mut heads = self.open_table(AGENT_HEADS);
        let mut head = heads
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        let pos = head.story_pos;
        head.story_pos = pos.next();
        fold_story_head(&mut head, event);
        heads.insert(&agent_id, SenValue::borrowed(&head));
        drop(heads);
        self.open_table(AGENT_STORY)
            .insert(&StoryKey { agent_id, pos }, SenValue::borrowed(event));
        pos
    }

    fn append_agent_story_from(
        &mut self,
        agent_id: AgentId,
        event: &StoryEvent,
        through: AgentEventPos,
    ) -> StoryPos {
        let pos = self.append_agent_story(agent_id, event);
        self.open_table(AGENT_STORY_SOURCE)
            .insert(&StoryKey { agent_id, pos }, &through);
        pos
    }

    fn append_agent_config_event(&mut self, agent_id: AgentId, event: &AgentEvent<'_>) {
        let at = agent_tail_position(self, agent_id);
        self.open_table(AGENT_EVENTS)
            .insert(&at, SenValue::borrowed(event));
        let mut heads = self.open_table(AGENT_HEADS);
        let mut head = heads
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        fold_agent_head(&mut head, event);
        heads.insert(&agent_id, SenValue::borrowed(&head));
        drop(heads);
        // A role change and a new workdir are things a reader is told;
        // the rest of the config events are the runtime's own business.
        for told in crate::story::from_raw_event(event, UnixMillis::now()) {
            self.append_agent_story_from(agent_id, &told, at);
        }
    }

    fn rebuild_agent_head(&mut self, agent_id: AgentId) -> AgentHead {
        // Which lineage is selected is the head's own fact: a fork is not
        // an event in the log it forks from.
        let previous = self
            .open_table(AGENT_HEADS)
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        let current_lineage = previous.current_lineage;
        let events = agent_events_write(self, current_lineage);
        let created = events
            .iter()
            .find(|event| matches!(event, AgentEvent::Created { .. }))
            .expect("every agent's log begins with its creation");
        // The story is not rebuilt from the raw log: it is its own
        // append-only log, so where it stands survives a head rebuild.
        let mut head = AgentHead {
            config: created_config(created),
            story_pos: previous.story_pos,
            turn_running: previous.turn_running,
            story_built: previous.story_built,
            generated_title: None,
            activity: None,
            current_lineage,
        };
        for event in &events {
            fold_agent_head(&mut head, event);
        }
        self.open_table(AGENT_HEADS)
            .insert(&agent_id, SenValue::borrowed(&head));
        head
    }

    fn agent_story_built(&mut self, agent_id: AgentId) -> bool {
        self.open_table(AGENT_HEADS)
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned()
            .story_built
    }

    fn mark_agent_story_built(&mut self, agent_id: AgentId) {
        let mut heads = self.open_table(AGENT_HEADS);
        let mut head = heads
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        head.story_built = true;
        heads.insert(&agent_id, SenValue::borrowed(&head));
    }

    fn set_agent_role(&mut self, agent_id: AgentId, role: AgentRole) {
        self.append_agent_config_event(
            agent_id,
            &AgentEvent::RoleChanged {
                role,
                binding: None,
            },
        );
    }

    fn set_agent_prompt_cache_key(&mut self, agent_id: AgentId, key: PromptCacheKey) {
        self.append_agent_config_event(
            agent_id,
            &AgentEvent::RuntimeRebound {
                change: crate::RuntimeChange::PromptCacheKey(key),
            },
        );
    }

    fn set_agent_claude_rewind(&mut self, agent_id: AgentId, rewind: Option<ClaudeRewind>) {
        self.append_agent_config_event(
            agent_id,
            &AgentEvent::RuntimeRebound {
                change: crate::RuntimeChange::ClaudeRewindPending(rewind),
            },
        );
    }

    fn complete_agent_claude_rewind(&mut self, agent_id: AgentId, session_id: Uuid) {
        self.append_agent_config_event(
            agent_id,
            &AgentEvent::RuntimeRebound {
                change: crate::RuntimeChange::ClaudeRewound { session_id },
            },
        );
    }

    fn alloc_agent_id(&mut self) -> AgentId {
        let domain = AgentIdDomain(machine_seed(self));
        AgentId::from_counter(next_counter(self, CounterKey::LAST_AGENT_ID), &domain)
            .expect("agent id counter exceeds prefix-id capacity")
    }

    fn append_agent_event(&mut self, at: AgentEventPos, event: &AgentEvent<'_>) -> AgentEventPos {
        let mut events = self.open_table(AGENT_EVENTS);
        // A config event may have landed at this position while the runtime
        // held its cursor in memory; step past it rather than over it.
        let mut at = at;
        while events.get(&at).is_some() {
            at = at.next();
        }
        events.insert(&at, SenValue::borrowed(event));
        at.next()
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
        touch_agent(self, now, agent_id);
        // The story is the source: `Titled` and `Activity` are told once,
        // and the head's fold of them is the cache every reader sees.
        for event in crate::story::from_presentation_update(update, now) {
            self.append_agent_story(agent_id, &event);
        }
        let head = self
            .open_table(AGENT_HEADS)
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        Some(AgentPresentationCache {
            generated_title: head.generated_title,
            activity: head.activity,
        })
    }

    fn record_agent_turn_end(&mut self, now: UnixMillis, agent_id: AgentId) {
        // The activity label describes work that just stopped.
        let mut heads = self.open_table(AGENT_HEADS);
        let mut head = heads
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        head.activity = None;
        heads.insert(&agent_id, SenValue::borrowed(&head));
        drop(heads);
        edit_agent_attention(self, agent_id, |attention| {
            // A turn end puts the ball back in the user's court; it says
            // nothing about engagement, so `last_user_message` stays.
            attention.disposition = match attention.disposition {
                AgentDisposition::Snoozed { until } if until > now => {
                    AgentDisposition::Snoozed { until }
                }
                _ => AgentDisposition::Pending,
            };
            // The previous turn's report describes a superseded final message.
            attention.turn_report = None;
            attention.last_turn_ended = Some(now);
        });
    }

    fn backfill_agent_last_turn_ended(&mut self, agent_id: AgentId, at: UnixMillis) -> bool {
        let mut filled = false;
        edit_agent_attention(self, agent_id, |attention| {
            if attention.last_turn_ended.is_none() {
                attention.last_turn_ended = Some(at);
                filled = true;
            }
        });
        filled
    }

    fn record_agent_turn_report(&mut self, agent_id: AgentId, report: &TurnReport) {
        edit_agent_attention(self, agent_id, |attention| {
            attention.turn_report = Some(report.clone());
            // An FYI asks nothing of the user; settle it like a pressed Done
            // so it carries no attention weight while the row keeps its
            // summary.
            if !report.needs_you {
                attention.disposition = AgentDisposition::Done;
            }
        });
    }

    fn record_agent_user_message(&mut self, now: UnixMillis, agent_id: AgentId, text: &str) {
        edit_agent_attention(self, agent_id, |attention| {
            attention.last_user_message = now;
            attention.last_user_message_text = message_snippet(text);
            attention.user_interacted = true;
            // Replying is a verdict like acking: the ball moves to the
            // agent's court even if the turn hasn't started yet (queued
            // delivery), so a pending lamp must not linger.
            attention.disposition = AgentDisposition::Done;
            attention.turn_report = None;
        });
    }

    fn set_agent_disposition(&mut self, agent_id: AgentId, disposition: AgentDisposition) {
        edit_agent_attention(self, agent_id, |attention| {
            attention.disposition = disposition;
        });
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
        touch_agent(self, now, agent_id);
        let mut heads = self.open_table(AGENT_HEADS);
        let mut head = heads
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        head.current_lineage = lineage_id;
        heads.insert(&agent_id, SenValue::borrowed(&head));
        drop(heads);
        // A rewind is told, not undone: positions only grow, and this says
        // from where a reader stops showing what it already has.
        let to = story_position_after_rewind(self, agent_id);
        self.append_agent_story(agent_id, &StoryEvent::Rewound { to, at: now });
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
        let head = self
            .open_table(AGENT_HEADS)
            .get(&agent_id)
            .expect("usage agent missing")
            .value()
            .into_owned();
        if bucket.model == AgentUsageModel::UNKNOWN {
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

fn agent_lineage_segments_read(
    read: &ReadTxn,
    current_lineage: AgentLineageId,
) -> Vec<(AgentLineageId, u32)> {
    let mut segments = Vec::new();
    let mut lineage_id = current_lineage;
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

fn agent_event_visible_write(
    write: &mut WriteTxn,
    agent_id: AgentId,
    position: AgentEventPos,
) -> bool {
    let mut lineage_id = write
        .open_table(AGENT_HEADS)
        .get(&agent_id)
        .expect("agent id missing")
        .value()
        .into_owned()
        .current_lineage;
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
        | AgentEvent::PresentationUpdated { .. }
        | AgentEvent::Created { .. }
        | AgentEvent::RoleChanged { .. }
        | AgentEvent::WorkdirAdded { .. }
        | AgentEvent::RuntimeRebound { .. } => 0,
    }
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

/// Puts an agent back the way the record-to-log migration leaves it: a
/// head that says its story is not built, and no story rows. Goes with
/// the backfill it exercises.
#[cfg(test)]
pub(crate) fn clear_agent_story(write: &mut WriteTxn, agent_id: AgentId) {
    let positions = write
        .open_table(AGENT_STORY)
        .range(
            StoryKey {
                agent_id,
                pos: StoryPos::default(),
            }..=StoryKey {
                agent_id,
                pos: StoryPos(u64::MAX),
            },
        )
        .map(|(key, _)| key.value())
        .collect::<Vec<_>>();
    let mut story = write.open_table(AGENT_STORY);
    for key in positions {
        story.remove(&key);
    }
    drop(story);
    let mut heads = write.open_table(AGENT_HEADS);
    let mut head = heads
        .get(&agent_id)
        .expect("agent id missing")
        .value()
        .into_owned();
    head.story_built = false;
    head.story_pos = StoryPos::default();
    heads.insert(&agent_id, SenValue::borrowed(&head));
}

/// One event's effect on the head. Everything the head knows arrives
/// through here, so a rebuild and the live fold cannot disagree.
fn fold_agent_head(head: &mut AgentHead, event: &AgentEvent<'_>) {
    match event {
        AgentEvent::Created { .. } => head.config = created_config(event),
        AgentEvent::RoleChanged { role, binding } => {
            head.config.role = *role;
            if let Some(binding) = binding {
                head.config.binding = *binding;
            }
        }
        AgentEvent::WorkdirAdded { workdir } => head.config.workdirs.push(workdir.clone()),
        AgentEvent::RuntimeRebound { change } => match change {
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
                    prompt_cache_key: key.clone(),
                };
            }
        },
        AgentEvent::PresentationUpdated { update } => {
            match &update.generated_title {
                PresentationField::Set(title) if head.config.spawn_name.is_none() => {
                    head.generated_title = Some(title.clone());
                }
                PresentationField::Clear => head.generated_title = None,
                PresentationField::Set(_) | PresentationField::Unchanged => {}
            }
            match &update.activity {
                PresentationField::Set(activity) => head.activity = Some(activity.clone()),
                PresentationField::Clear => head.activity = None,
                PresentationField::Unchanged => {}
            }
        }
        AgentEvent::InferenceResponse { .. }
        | AgentEvent::ToolResult { .. }
        | AgentEvent::Queued(_)
        | AgentEvent::Dequeued { .. }
        | AgentEvent::QueueCleared
        | AgentEvent::ClaudePresentationSource { .. } => {}
    }
}

/// One story event's effect on the head: the latest title and activity
/// a reader sees, and whether a turn is running.
fn fold_story_head(head: &mut AgentHead, event: &StoryEvent) {
    match event {
        StoryEvent::Titled { title, .. } if head.config.spawn_name.is_none() => {
            head.generated_title = Some(title.clone());
        }
        StoryEvent::Activity { label, .. } => head.activity = label.clone(),
        StoryEvent::TurnStarted { .. } => head.turn_running = true,
        StoryEvent::TurnEnded { .. } => {
            head.turn_running = false;
            // The label described work that just stopped.
            head.activity = None;
        }
        StoryEvent::Titled { .. }
        | StoryEvent::Created { .. }
        | StoryEvent::UserMessage { .. }
        | StoryEvent::AgentMail { .. }
        | StoryEvent::Reply { .. }
        | StoryEvent::ToolCall { .. }
        | StoryEvent::Wants { .. }
        | StoryEvent::Cost { .. }
        | StoryEvent::Rewound { .. }
        | StoryEvent::Compacted { .. }
        | StoryEvent::HistoryUnavailableBefore { .. }
        // Config is the raw log's fold; telling it again here would push
        // the same workdir twice.
        | StoryEvent::RoleChanged { .. }
        | StoryEvent::WorkdirAdded { .. } => {}
    }
}

/// Where the story stops holding after a rewind: the position just past
/// the newest event whose raw source is still on the selected lineage.
/// Scanning back from the tail is cheap because a rewind abandons a
/// short tail, and events with no raw source (a title, a cost, a turn
/// boundary) travel with the events around them.
fn story_position_after_rewind(write: &mut WriteTxn, agent_id: AgentId) -> StoryPos {
    let head_pos = write
        .open_table(AGENT_HEADS)
        .get(&agent_id)
        .expect("agent id missing")
        .value()
        .into_owned()
        .story_pos;
    let sources = write
        .open_table(AGENT_STORY_SOURCE)
        .range(
            StoryKey {
                agent_id,
                pos: StoryPos::default(),
            }..=StoryKey {
                agent_id,
                pos: StoryPos(u64::MAX),
            },
        )
        .rev()
        .map(|(key, value)| (key.value().pos, value.value()))
        .collect::<Vec<_>>();
    // Nothing was ever told from the raw log (a Claude story built from a
    // transcript): there is nothing this can place, so keep it all.
    if sources.is_empty() {
        return head_pos;
    }
    for (pos, through) in sources {
        if agent_event_visible_write(write, agent_id, through) {
            return pos.next();
        }
    }
    // The rewind reached past everything the raw log told.
    StoryPos::default()
}

/// The first free position on the agent's selected lineage, read from the
/// table rather than from a runtime's in-memory cursor.
fn agent_tail_position(write: &mut WriteTxn, agent_id: AgentId) -> AgentEventPos {
    let lineage_id = write
        .open_table(AGENT_HEADS)
        .get(&agent_id)
        .expect("agent id missing")
        .value()
        .into_owned()
        .current_lineage;
    write
        .open_table(AGENT_EVENTS)
        .range(
            AgentEventPos::root(lineage_id)..=AgentEventPos {
                lineage_id,
                seq: u32::MAX,
            },
        )
        .next_back()
        .map(|(key, _)| key.value().next())
        .unwrap_or_else(|| AgentEventPos::root(lineage_id))
}

/// The agent's visible events, oldest first, from a write transaction.
/// The same walk as [`AgentReadTxnExt::agent_event_records`].
fn agent_events_write(
    write: &mut WriteTxn,
    current_lineage: AgentLineageId,
) -> Vec<AgentEvent<'static>> {
    let mut segments = Vec::new();
    let mut lineage_id = current_lineage;
    let mut end_seq = u32::MAX;
    let lineage_parents = write.open_table(LINEAGE_PARENTS);
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

    let events = write.open_table(AGENT_EVENTS);
    let mut collected = Vec::new();
    for (lineage_id, end_seq) in segments.into_iter().rev() {
        for (key, value) in events.range(
            AgentEventPos::root(lineage_id)..=AgentEventPos {
                lineage_id,
                seq: end_seq,
            },
        ) {
            if key.value().seq == end_seq && end_seq != u32::MAX {
                break;
            }
            collected.push(value.value().into_owned());
        }
    }
    collected
}

fn edit_agent_attention(
    write: &mut WriteTxn,
    agent_id: AgentId,
    edit: impl FnOnce(&mut AgentAttention),
) {
    let mut table = write.open_table(AGENT_ATTENTION);
    let mut attention = table
        .get(&agent_id)
        .map(|value| value.value().into_owned())
        .unwrap_or_default();
    edit(&mut attention);
    table.insert(&agent_id, SenValue::borrowed(&attention));
}

/// Marks the agent as changed now. `updated_at` is the last thing the
/// daemon still times for a client; slice B derives it from the story.
fn touch_agent(write: &mut WriteTxn, now: UnixMillis, agent_id: AgentId) {
    edit_agent_attention(write, agent_id, |attention| {
        attention.updated_at = attention.updated_at.max(now);
    });
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
