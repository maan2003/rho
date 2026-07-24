//! Raw redb schema for persisted agents.

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
const AGENTS: TableDefinition<AgentId, Sen<AgentRecord>> = TableDefinition::new("agents");
const WORKSTREAMS: TableDefinition<WorkstreamId, Sen<WorkstreamRecord>> =
    TableDefinition::new("workstreams");
const PROJECTS: TableDefinition<String, Sen<ProjectRecord>> = TableDefinition::new("projects");
/// Opaque client-owned view configuration (see
/// [`AgentReadTxnExt::view_config`]).
const VIEW_CONFIG: TableDefinition<(), Vec<u8>> = TableDefinition::new("view_config");
const QUOTA_OBSERVATIONS: TableDefinition<QuotaObservationKey, Sen<QuotaObservationRecord>> =
    TableDefinition::new("quota_observations_by_model_time");
const AGENT_USAGE_BUCKETS: TableDefinition<AgentUsageKey, Sen<AgentUsageBucket>> =
    TableDefinition::new("agent_usage_by_agent_time");
const AGENT_USAGE_TOTALS: TableDefinition<AgentId, Sen<AgentUsageBucket>> =
    TableDefinition::new("agent_usage_totals");
const GLOBAL_AGENT_USAGE: TableDefinition<GlobalAgentUsageKey, Sen<AgentUsageBucket>> =
    TableDefinition::new("agent_usage_by_time_provider");
const MIGRATION_RECOVERY: TableDefinition<(), Sen<MigrationRecoveryPoint>> =
    TableDefinition::new("migration_recovery");

const CURRENT_AGENT_DB_FORMAT: &str = "d93b71e4";

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
        from: "f12a7c9d",
        to: "a61e39c4",
        migrate: |_| {},
    },
    AgentDbMigration {
        from: "a61e39c4",
        to: "d93b71e4",
        migrate: rebuild_global_agent_usage,
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue)]
struct CounterKey(u8);

impl CounterKey {
    pub const LAST_AGENT_ID: Self = Self(1);
    pub const LAST_LINEAGE_ID: Self = Self(2);
    /// Formerly the topic and then tag id counter; workstreams continue
    /// its sequence.
    pub const LAST_WORKSTREAM_ID: Self = Self(3);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue, Encode, Decode)]
pub struct QuotaModel(u8);

impl QuotaModel {
    pub const GPT: Self = Self(1);
    pub const FABLE: Self = Self(2);

    pub fn name(self) -> &'static str {
        match self {
            Self::GPT => "gpt",
            Self::FABLE => "fable",
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
pub struct AgentUsageProvider(u8);

impl AgentUsageProvider {
    pub const GPT: Self = Self(1);
    pub const CLAUDE: Self = Self(2);

    pub fn name(self) -> &'static str {
        match self {
            Self::GPT => "gpt",
            Self::CLAUDE => "claude",
            _ => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue)]
struct GlobalAgentUsageKey {
    bucket_start_ms: u64,
    provider: AgentUsageProvider,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct AgentUsageBucket {
    pub bucket_start_ms: u64,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub requests: u64,
    #[senax(default)]
    pub approximate: bool,
}

impl AgentUsageBucket {
    pub fn add(&mut self, other: &Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.cache_write_tokens = self
            .cache_write_tokens
            .saturating_add(other.cache_write_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.requests = self.requests.saturating_add(other.requests);
        self.approximate |= other.approximate;
    }
}

fn usage_provider(runtime: &AgentRuntime) -> AgentUsageProvider {
    match runtime {
        AgentRuntime::Rho { .. } => AgentUsageProvider::GPT,
        AgentRuntime::Claude { .. } => AgentUsageProvider::CLAUDE,
    }
}

fn add_global_agent_usage(
    write: &mut WriteTxn,
    provider: AgentUsageProvider,
    bucket: &AgentUsageBucket,
) {
    let key = GlobalAgentUsageKey {
        bucket_start_ms: bucket.bucket_start_ms,
        provider,
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

fn rebuild_global_agent_usage(write: &mut WriteTxn) {
    write.delete_table("agent_usage_by_time_provider");
    let providers = write
        .open_table(AGENTS)
        .iter()
        .map(|(agent_id, record)| {
            (
                agent_id.value(),
                usage_provider(&record.value().as_ref().runtime),
            )
        })
        .collect::<std::collections::HashMap<_, _>>();
    let buckets = write
        .open_table(AGENT_USAGE_BUCKETS)
        .iter()
        .map(|(key, bucket)| (key.value(), bucket.value().into_owned()))
        .collect::<Vec<_>>();
    for (key, bucket) in buckets {
        if let Some(provider) = providers.get(&key.agent_id) {
            add_global_agent_usage(write, *provider, &bucket);
        }
    }
}

pub use rho_core::{AgentId, AgentIdDomain};

/// Plain sequential workstream id; no prefix-id scrambling — workstreams
/// are addressed by their unique name in user-facing contexts, and clients
/// that need a string render `ws-{n}`.
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
    Pack,
    Unpack,
)]
pub struct WorkstreamId(pub u64);

/// The persistent unit of work: the user's statement that its member
/// agents belong together. Deliberately minimal — repos, attention, and
/// everything else about a workstream is derived from its agents; how
/// workstreams are grouped and sorted is the client's view layer, driven
/// by labels.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct WorkstreamRecord {
    pub name: String,
    /// Free-form markers ("pin", "group:slack", …); semantics live in the
    /// client's view layer, not here.
    pub labels: Vec<String>,
    pub created_at: UnixMillis,
    pub updated_at: UnixMillis,
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct ProjectRecord {
    pub name: String,
    pub description: String,
    pub created_at: UnixMillis,
}

/// What the user did about an agent's last finished turn. Attention is
/// action-cleared (the email-triage model) and *derived*: the daemon
/// combines this stored verdict with live agent state to produce the
/// attention level; only the verdict persists.
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
    let debug_dir = dirs::state_dir().map(|dir| dir.join("rho/debug/provider-requests"));
    prepare_agent_db_migration_with_debug_dir(db, debug_dir).await;
}

async fn prepare_agent_db_migration_with_debug_dir(
    db: &rho_db::RhoDb,
    debug_dir: Option<std::path::PathBuf>,
) {
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
    let recovery_ready = migration_recovery_point(db).is_some_and(|point| {
        point.from_format == from_format && point.to_format == CURRENT_AGENT_DB_FORMAT
    });
    if !recovery_ready {
        db.persistent_savepoint(|write, savepoint_id| {
            let point = MigrationRecoveryPoint {
                savepoint_id,
                from_format: from_format.clone(),
                to_format: CURRENT_AGENT_DB_FORMAT.to_owned(),
                created_at: UnixMillis::now(),
            };
            write
                .open_table(MIGRATION_RECOVERY)
                .insert(&(), SenValue::borrowed(&point));
        })
        .await;
    }
    if from_format == "f12a7c9d" {
        eprintln!("rho-agent: backfilling per-agent token usage");
        let buckets = backfill_agent_usage(db, debug_dir).await;
        eprintln!(
            "rho-agent: writing {} five-minute token-usage buckets",
            buckets.len()
        );
        let mut write = db.write().await;
        write.replace_agent_usage(&buckets);
        write.commit();
    }
}

async fn backfill_agent_usage(
    db: &rho_db::RhoDb,
    debug_dir: Option<std::path::PathBuf>,
) -> std::collections::HashMap<(AgentId, u64), AgentUsageBucket> {
    use futures::StreamExt as _;

    let agents = db.read().list_agents();
    let mut prompt_keys = std::collections::HashMap::<String, Vec<AgentId>>::new();
    let mut claude_sessions = Vec::new();
    for (agent_id, record) in agents {
        match record.runtime {
            AgentRuntime::Rho { prompt_cache_key } => prompt_keys
                .entry(prompt_cache_key.debug_file_stem())
                .or_default()
                .push(agent_id),
            AgentRuntime::Claude { session_id } => claude_sessions.push((
                agent_id,
                session_id,
                record.primary_workdir().repo().to_owned(),
            )),
        }
    }
    let prompt_keys = prompt_keys
        .into_iter()
        .filter_map(|(key, agents)| (agents.len() == 1).then_some((key, agents[0])))
        .collect::<std::collections::HashMap<_, _>>();
    let mut paths = Vec::new();
    if let Some(dir) = debug_dir
        && let Ok(mut entries) = tokio::fs::read_dir(dir).await
    {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with("-response.json")
                && name
                    .get(..16)
                    .is_some_and(|prefix| prompt_keys.contains_key(prefix))
            {
                paths.push(entry.path());
            }
        }
    }

    let native = futures::stream::iter(paths.into_iter().map(|path| {
        let prompt_keys = &prompt_keys;
        async move {
            let name = path.file_name()?.to_string_lossy();
            let agent_id = *prompt_keys.get(name.get(..16)?)?;
            let bytes = tokio::fs::read(&path).await.ok()?;
            let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            let response = value["raw_events"]
                .as_array()?
                .iter()
                .rev()
                .find(|event| event["type"] == "response.completed")?
                .get("response")?;
            let usage = response.get("usage")?;
            let input = usage["input_tokens"].as_u64()?;
            let cache_read = usage["input_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0);
            let cache_write = usage["input_tokens_details"]["cache_write_tokens"]
                .as_u64()
                .unwrap_or(0);
            let completed_at: u64 = tokio::fs::metadata(&path)
                .await
                .ok()?
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis()
                .try_into()
                .ok()?;
            Some((
                agent_id,
                AgentUsageBucket {
                    bucket_start_ms: completed_at / AGENT_USAGE_BUCKET_MS * AGENT_USAGE_BUCKET_MS,
                    input_tokens: input.saturating_sub(cache_read).saturating_sub(cache_write),
                    cache_read_tokens: cache_read,
                    cache_write_tokens: cache_write,
                    output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
                    requests: 1,
                    approximate: false,
                },
            ))
        }
    }))
    .buffer_unordered(32)
    .filter_map(async move |value| value)
    .collect::<Vec<_>>()
    .await;

    let claude = futures::stream::iter(claude_sessions.into_iter().map(
        |(agent_id, session_id, repo)| async move {
            let messages = rho_claude::read_session_messages_by_id(
                session_id,
                &repo,
                rho_claude::SessionMessagesOptions::default(),
            )
            .await
            .ok()?;
            Some((agent_id, messages))
        },
    ))
    .buffer_unordered(16)
    .filter_map(async move |value| value)
    .collect::<Vec<_>>()
    .await;

    let mut buckets = std::collections::HashMap::<(AgentId, u64), AgentUsageBucket>::new();
    for (agent_id, bucket) in native {
        buckets
            .entry((agent_id, bucket.bucket_start_ms))
            .or_insert_with(|| AgentUsageBucket {
                bucket_start_ms: bucket.bucket_start_ms,
                ..AgentUsageBucket::default()
            })
            .add(&bucket);
    }
    for (agent_id, messages) in claude {
        for message in messages {
            if message.kind != rho_claude::SessionMessageKind::Assistant {
                continue;
            }
            let Some(timestamp) = message.timestamp.as_deref().and_then(parse_rfc3339_millis)
            else {
                continue;
            };
            let Ok(usage) = serde_json::from_value::<rho_claude::protocol::TokenUsage>(
                message.message.get("usage").cloned().unwrap_or_default(),
            ) else {
                continue;
            };
            let bucket_start_ms = timestamp / AGENT_USAGE_BUCKET_MS * AGENT_USAGE_BUCKET_MS;
            let bucket = AgentUsageBucket {
                bucket_start_ms,
                input_tokens: usage.input_tokens.unwrap_or(0),
                cache_read_tokens: usage.cache_read_input_tokens.unwrap_or(0),
                cache_write_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
                output_tokens: usage.output_tokens.unwrap_or(0),
                requests: 1,
                approximate: true,
            };
            buckets
                .entry((agent_id, bucket_start_ms))
                .or_insert_with(|| AgentUsageBucket {
                    bucket_start_ms,
                    ..AgentUsageBucket::default()
                })
                .add(&bucket);
        }
    }
    buckets
}

fn parse_rfc3339_millis(timestamp: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()?
        .timestamp_millis()
        .try_into()
        .ok()
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AgentRecord {
    pub display_name: Option<String>,
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
    /// The workstream this agent belongs to: exactly one, founded with the
    /// top-level agent and inherited by its spawn tree.
    #[senax(default)]
    pub workstream: WorkstreamId,
    /// Free-form markers ("pin", …); semantics live in the client's view
    /// layer. Not copied on spawn.
    #[senax(default)]
    pub labels: Vec<String>,
    /// The user's verdict on the last finished turn; attention is derived
    /// from this plus live agent state, never stored.
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
    Mini,
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
            Self::ResponsesLuna(config) => (
                EngineerIntelligence::Mini,
                if config.fast_mode {
                    Latency::Fast
                } else {
                    Latency::Standard
                },
            ),
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
    fn get_workstream(&self, workstream_id: WorkstreamId) -> WorkstreamRecord;
    fn list_workstreams(&self) -> Vec<(WorkstreamId, WorkstreamRecord)>;
    /// Opaque client-owned view configuration; the daemon stores and
    /// forwards it without interpreting a byte.
    fn view_config(&self) -> Vec<u8>;
    fn get_agent(&self, agent_id: AgentId) -> AgentRecord;
    fn list_agents(&self) -> Vec<(AgentId, AgentRecord)>;
    fn list_projects(&self) -> Vec<(Utf8PathBuf, ProjectRecord)>;
    fn agent_events(&self, agent_id: AgentId) -> (AgentEventPos, Vec<AgentEvent<'static>>);
    fn agent_event_records(
        &self,
        agent_id: AgentId,
    ) -> (AgentEventPos, Vec<(AgentEventPos, AgentEvent<'static>)>);
    /// Samples for one model, bounded to the horizon plus its preceding
    /// baseline.
    fn quota_observations(
        &self,
        model: QuotaModel,
        since: UnixMillis,
    ) -> Vec<QuotaObservationRecord>;
    fn agent_usage(&self, agent_id: AgentId, since: UnixMillis) -> Vec<AgentUsageBucket>;
    fn agent_usage_total(&self, agent_id: AgentId) -> AgentUsageBucket;
    fn global_agent_usage(&self, since: UnixMillis) -> Vec<(AgentUsageProvider, AgentUsageBucket)>;
}

#[allow(clippy::too_many_arguments)]
pub trait AgentWriteTxnExt {
    fn init_agent_tables(&mut self);

    /// Creates a workstream; a colliding name gets a numeric suffix (names
    /// are auto-generated from agent titles, so collisions must not fail).
    fn create_workstream(&mut self, now: UnixMillis, name: String) -> WorkstreamId;

    fn set_workstream_name(&mut self, now: UnixMillis, workstream_id: WorkstreamId, name: String);

    /// Adds or removes one workstream label; adding twice is a no-op.
    fn workstream_label(
        &mut self,
        now: UnixMillis,
        workstream_id: WorkstreamId,
        label: &str,
        add: bool,
    );

    /// Adds or removes one agent label; adding twice is a no-op.
    fn agent_label(&mut self, now: UnixMillis, agent_id: AgentId, label: &str, add: bool);

    /// Moves an agent to another workstream (its spawn tree moves with it —
    /// callers pass every member).
    fn set_agent_workstream(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        workstream_id: WorkstreamId,
    );

    fn set_view_config(&mut self, data: Vec<u8>);

    fn set_agent_display_name(&mut self, now: UnixMillis, agent_id: AgentId, name: String);
    fn set_agent_role(&mut self, agent_id: AgentId, role: AgentRole);
    fn set_agent_prompt_cache_key(&mut self, agent_id: AgentId, key: PromptCacheKey);
    fn set_agent_claude_rewind(&mut self, agent_id: AgentId, rewind: Option<ClaudeRewind>);
    fn complete_agent_claude_rewind(&mut self, agent_id: AgentId, session_id: Uuid);

    fn alloc_agent_id(&mut self) -> AgentId;

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

    /// Stamps the user's engagement with an agent (rail recency), keeps a
    /// one-line snippet of the message, and clears its disposition:
    /// replying is as much a verdict as acking.
    fn record_agent_user_message(&mut self, now: UnixMillis, agent_id: AgentId, text: &str);

    /// Removes a workstream record. Callers ensure no agent still points at
    /// it — the daemon deletes only streams its moves emptied.
    fn delete_workstream(&mut self, workstream_id: WorkstreamId);

    fn set_agent_disposition(&mut self, agent_id: AgentId, disposition: AgentDisposition);
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
        workstream: WorkstreamId,
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
        workstream: WorkstreamId,
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
            workstream,
            labels: Vec::new(),
            disposition: AgentDisposition::Done,
        };
        self.open_table(AGENTS)
            .insert(&agent_id, SenValue::borrowed(&agent));
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

    fn get_workstream(&self, workstream_id: WorkstreamId) -> WorkstreamRecord {
        self.open_table(WORKSTREAMS)
            .get(&workstream_id)
            .expect("workstream id missing")
            .value()
            .into_owned()
    }

    fn list_workstreams(&self) -> Vec<(WorkstreamId, WorkstreamRecord)> {
        self.open_table(WORKSTREAMS)
            .iter()
            .map(|(key, value)| (key.value(), value.value().into_owned()))
            .collect()
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

    fn quota_observations(
        &self,
        model: QuotaModel,
        since: UnixMillis,
    ) -> Vec<QuotaObservationRecord> {
        let table = self.open_table(QUOTA_OBSERVATIONS);
        let mut observations = Vec::new();
        for (_, value) in table
            .range(
                QuotaObservationKey {
                    model,
                    observed_at: 0,
                }..=QuotaObservationKey {
                    model,
                    observed_at: u64::MAX,
                },
            )
            .rev()
        {
            let observation = value.value().into_owned();
            let before_horizon = observation.observed_at < since;
            observations.push(observation);
            if before_horizon {
                break;
            }
        }
        observations.reverse();
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

    fn global_agent_usage(&self, since: UnixMillis) -> Vec<(AgentUsageProvider, AgentUsageBucket)> {
        self.open_table(GLOBAL_AGENT_USAGE)
            .range(
                GlobalAgentUsageKey {
                    bucket_start_ms: since.0,
                    provider: AgentUsageProvider::GPT,
                }..=GlobalAgentUsageKey {
                    bucket_start_ms: u64::MAX,
                    provider: AgentUsageProvider::CLAUDE,
                },
            )
            .map(|(key, value)| (key.value().provider, value.value().into_owned()))
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
        self.open_table(AGENTS);
        self.open_table(WORKSTREAMS);
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

    fn create_workstream(&mut self, now: UnixMillis, name: String) -> WorkstreamId {
        let workstream_id = WorkstreamId(next_counter(self, CounterKey::LAST_WORKSTREAM_ID));
        let workstream = WorkstreamRecord {
            name: unique_workstream_name(self, name, None),
            labels: Vec::new(),
            created_at: now,
            updated_at: now,
        };
        self.open_table(WORKSTREAMS)
            .insert(&workstream_id, SenValue::borrowed(&workstream));
        workstream_id
    }

    fn delete_workstream(&mut self, workstream_id: WorkstreamId) {
        self.open_table(WORKSTREAMS).remove(&workstream_id);
    }

    fn set_workstream_name(&mut self, now: UnixMillis, workstream_id: WorkstreamId, name: String) {
        let name = unique_workstream_name(self, name, Some(workstream_id));
        update_workstream(self, now, workstream_id, |workstream| {
            workstream.name = name;
        });
    }

    fn workstream_label(
        &mut self,
        now: UnixMillis,
        workstream_id: WorkstreamId,
        label: &str,
        add: bool,
    ) {
        update_workstream(self, now, workstream_id, |workstream| {
            edit_labels(&mut workstream.labels, label, add);
        });
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

    fn set_agent_workstream(
        &mut self,
        now: UnixMillis,
        agent_id: AgentId,
        workstream_id: WorkstreamId,
    ) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        agent.workstream = workstream_id;
        agent.updated_at = now;
        agents.insert(&agent_id, SenValue::borrowed(&agent));
    }

    fn set_view_config(&mut self, data: Vec<u8>) {
        self.open_table(VIEW_CONFIG).insert(&(), &data);
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

    fn record_agent_user_message(&mut self, now: UnixMillis, agent_id: AgentId, text: &str) {
        let mut agents = self.open_table(AGENTS);
        let mut agent = agents
            .get(&agent_id)
            .expect("agent id missing")
            .value()
            .into_owned();
        agent.last_user_message = now;
        agent.last_user_message_text = message_snippet(text);
        // Replying is a verdict like acking — the ball moves to the agent's
        // court even if the turn hasn't started yet (queued delivery), so a
        // pending lamp must not linger.
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

    fn record_quota_observation(&mut self, observation: QuotaObservationRecord) -> bool {
        let key = QuotaObservationKey {
            model: observation.model,
            observed_at: observation.observed_at.0,
        };
        let unchanged = self
            .open_table(QUOTA_OBSERVATIONS)
            .range(
                QuotaObservationKey {
                    model: observation.model,
                    observed_at: 0,
                }..=QuotaObservationKey {
                    model: observation.model,
                    observed_at: u64::MAX,
                },
            )
            .next_back()
            .map(|(_, value)| value.value().into_owned())
            .is_some_and(|old| {
                old.provider == observation.provider
                    && old.used_percent == observation.used_percent
                    && old.reset_at_unix == observation.reset_at_unix
            });
        if unchanged {
            return false;
        }
        self.open_table(QUOTA_OBSERVATIONS)
            .insert(&key, SenValue::borrowed(&observation));
        true
    }

    fn add_agent_usage(&mut self, agent_id: AgentId, bucket: &AgentUsageBucket) {
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
        merged.add(bucket);
        buckets.insert(&key, SenValue::borrowed(&merged));
        drop(buckets);

        let mut totals = self.open_table(AGENT_USAGE_TOTALS);
        let mut total = totals
            .get(&agent_id)
            .map(|value| value.value().into_owned())
            .unwrap_or_default();
        total.add(bucket);
        total.bucket_start_ms = 0;
        totals.insert(&agent_id, SenValue::borrowed(&total));
        drop(totals);

        let provider = self
            .open_table(AGENTS)
            .get(&agent_id)
            .map(|record| usage_provider(&record.value().as_ref().runtime))
            .expect("usage agent missing");
        add_global_agent_usage(self, provider, bucket);
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

/// Workstream names are unique (so a name identifies a workstream); a
/// colliding name gets a numeric suffix rather than failing, since names
/// are auto-generated from agent titles.
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

fn unique_workstream_name(
    write: &mut WriteTxn,
    base: String,
    exclude: Option<WorkstreamId>,
) -> String {
    let taken = write
        .open_table(WORKSTREAMS)
        .iter()
        .filter(|(workstream_id, _)| Some(workstream_id.value()) != exclude)
        .map(|(_, workstream)| workstream.value().into_owned().name)
        .collect::<std::collections::HashSet<_>>();
    if !taken.contains(&base) {
        return base;
    }
    (2u64..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken.contains(candidate))
        .expect("unbounded suffix search terminates")
}

fn update_workstream(
    write: &mut WriteTxn,
    now: UnixMillis,
    workstream_id: WorkstreamId,
    edit: impl FnOnce(&mut WorkstreamRecord),
) {
    let mut workstreams = write.open_table(WORKSTREAMS);
    let mut workstream = workstreams
        .get(&workstream_id)
        .expect("workstream id missing")
        .value()
        .into_owned();
    edit(&mut workstream);
    workstream.updated_at = now;
    workstreams.insert(&workstream_id, SenValue::borrowed(&workstream));
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
