//! Durable agent log events and values shared by the host and workers.

use std::borrow::Cow;

use redb_derive::{Key, Value as RedbValue};
use rho_agent_types::transcript::{MessageSender, PendingInferenceResponse};
use rho_agent_types::{
    AdvisorIntelligence, AgentId, AgentRole, AgentWant, ContentPart, EngineerIntelligence, Place,
    TurnEdge, UnixMs,
};
use senax_encoder::{Decode, Encode, Pack, Unpack};
use uuid::Uuid;

use crate::entry;
use crate::entry::CompactionState;
use crate::inference::PromptCacheKey;
use crate::inference::config::{InferenceModel, InferenceProfile, ReasoningEffort};

pub const AGENT_USAGE_BUCKET_MS: u64 = 5 * 60 * 1_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Key, RedbValue, Encode, Decode)]
pub struct AgentUsageModel(u8);

impl AgentUsageModel {
    pub const UNKNOWN: Self = Self(0);
    pub const GPT: Self = Self(1);
    pub const FABLE: Self = Self(2);
    pub const OPUS: Self = Self(3);
    pub const TERRA: Self = Self(4);
    pub const LUNA: Self = Self(5);
    pub const ASTRA: Self = Self(7);

    pub fn name(self) -> &'static str {
        match self {
            Self::GPT => "gpt",
            Self::FABLE => "fable",
            Self::OPUS => "opus",
            Self::TERRA => "terra",
            Self::LUNA => "luna",
            Self::ASTRA => "astra",
            _ => "unknown",
        }
    }

    /// The model [`Self::name`] names.
    pub fn named(name: &str) -> Self {
        [
            Self::GPT,
            Self::FABLE,
            Self::OPUS,
            Self::TERRA,
            Self::LUNA,
            Self::ASTRA,
        ]
        .into_iter()
        .find(|model| model.name() == name)
        .unwrap_or(Self::UNKNOWN)
    }
}

impl Default for AgentUsageModel {
    fn default() -> Self {
        Self::UNKNOWN
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct AgentUsageBucket {
    pub bucket_start_ms: u64,
    pub model: AgentUsageModel,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub output_tokens: u64,
    pub requests: u64,
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

/// The model a runtime and binding bill as.
pub fn usage_model_of(runtime: &AgentRuntime, binding: SessionBinding) -> AgentUsageModel {
    match runtime {
        AgentRuntime::Rho { .. } => match binding.deep_model() {
            Some(InferenceModel::Gpt6Astra) => AgentUsageModel::ASTRA,
            Some(InferenceModel::Gpt6Luna) => AgentUsageModel::LUNA,
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

impl From<AgentEventPos> for rho_agent_types::AgentPos {
    fn from(pos: AgentEventPos) -> Self {
        Self(pos.pos)
    }
}

impl From<rho_agent_types::AgentPos> for AgentEventPos {
    fn from(pos: rho_agent_types::AgentPos) -> Self {
        Self { pos: pos.0 }
    }
}

/// A sidecar-derived title/activity update. `through` is a durable source
/// position, not the position where this update happens to be recorded. That
/// distinction makes a late result harmless after rewind.

/// What the agent is, folded from `Created` and the config events that
/// follow it. Nothing here is written directly: a change is an event
/// first and reaches the head through the fold.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AgentConfig {
    pub role: AgentRole,
    pub(crate) binding: SessionBinding,
    pub runtime: AgentRuntime,
    /// Where the agent works. Fixed at creation, but for a migration.
    pub place: Place,
    pub spawned_by: AgentSpawnedBy,
    /// The name the spawner gave. A generated title is never made for an
    /// agent that has one, and it always beats a generated title.
    pub spawn_name: Option<String>,
    pub created_at: UnixMs,
    /// A message-only Claude rewind whose destination transcript has not yet
    /// been durably materialized and verified. The old runtime remains
    /// authoritative until then.
    pub claude_rewind: Option<ClaudeRewind>,
}

/// What an agent is now: the fold of its whole log, hidden rows included
/// (a rewind takes back history, not configuration). Stored as a read
/// projection, updated atomically with the log.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct AgentHead {
    pub config: AgentConfig,
    /// Naming is attempted at most once, including across rewind and restart.
    pub title_attempted: bool,
    /// A generated title. A spawn name always takes precedence.
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
    /// What a `Notice` said, until a user message has carried it.
    pub pending_notice: Option<String>,
    pub last_turn_ended: Option<UnixMs>,
    /// Where the next event goes: one past the last row, hidden or not.
    pub next: AgentEventPos,
}

impl AgentHead {
    pub fn config(&self) -> AgentRole {
        self.config.role
    }

    pub fn place(&self) -> &Place {
        &self.config.place
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
    /// Where the agent works: default cwd, prompt header, UI label.
    pub fn place(&self) -> &Place {
        &self.place
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
    Engineer,
    /// An Engineer started it for the user, who manages it from then on.
    UserOwned {
        by: AgentId,
    },
}

impl AgentSpawnedBy {
    /// The user manages the agent: it has no parent to answer to.
    pub fn user_owned(self) -> bool {
        !matches!(self, Self::Engineer)
    }
}

/// How an agent comes to exist. The `parent` of its `Created` event is
/// the agent it answers to, so only a child has one: a user-owned
/// Engineer answers to the user and only remembers who started it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentOrigin {
    /// The user started it.
    User,
    /// An agent spawned it to work for that agent.
    Child { parent: AgentId },
    /// An Engineer started it for the user.
    UserOwned { by: AgentId },
}

impl AgentOrigin {
    pub fn parent(self) -> Option<AgentId> {
        match self {
            Self::Child { parent } => Some(parent),
            Self::User | Self::UserOwned { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum SessionBinding {
    ClaudeFable {
        effort: ClaudeEffort,
    },
    ClaudeOpus {
        effort: ClaudeEffort,
    },
    ResponsesSol(InferenceProfile),
    ResponsesLuna(InferenceProfile),
    /// Fable-backed advisor; distinct so its role survives session pinning.
    ClaudeAdvisor {
        effort: ClaudeEffort,
    },
    /// Sol-backed advisor.
    AdvisorSol(InferenceProfile),
    ResponsesAstra(InferenceProfile),
    /// Astra-backed advisor; distinct so its role survives session pinning.
    AdvisorAstra(InferenceProfile),
}

pub(crate) trait AgentRoleSessionProfile {
    fn session_profile(self) -> SessionBinding;
}

impl AgentRoleSessionProfile for AgentRole {
    fn session_profile(self) -> SessionBinding {
        let deep = |effort| InferenceProfile {
            effort,
            fast_mode: false,
        };
        match self {
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Mini,
            } => SessionBinding::ResponsesLuna(deep(ReasoningEffort::Xhigh)),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Medium,
            } => SessionBinding::ResponsesSol(deep(ReasoningEffort::High)),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::High,
            } => SessionBinding::ResponsesAstra(deep(ReasoningEffort::Medium)),
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::Medium1,
            } => SessionBinding::ClaudeOpus {
                effort: ClaudeEffort::Medium,
            },
            AgentRole::Engineer {
                intelligence: EngineerIntelligence::High1,
            } => SessionBinding::ClaudeFable {
                effort: ClaudeEffort::Medium,
            },
            AgentRole::Advisor {
                intelligence: AdvisorIntelligence::Low,
            } => SessionBinding::AdvisorSol(deep(ReasoningEffort::Xhigh)),
            AgentRole::Advisor {
                intelligence: AdvisorIntelligence::Medium,
            } => SessionBinding::AdvisorAstra(deep(ReasoningEffort::Xhigh)),
            AgentRole::Advisor {
                intelligence: AdvisorIntelligence::Medium1,
            } => SessionBinding::ClaudeAdvisor {
                effort: ClaudeEffort::Xhigh,
            },
        }
    }
}

impl SessionBinding {
    pub fn agent_role(self) -> AgentRole {
        match self {
            Self::ResponsesLuna(_) => AgentRole::Engineer {
                intelligence: EngineerIntelligence::Mini,
            },
            Self::ResponsesSol(_) => AgentRole::Engineer {
                intelligence: EngineerIntelligence::Medium,
            },
            Self::ResponsesAstra(_) => AgentRole::Engineer {
                intelligence: EngineerIntelligence::High,
            },
            Self::ClaudeOpus { .. } => AgentRole::Engineer {
                intelligence: EngineerIntelligence::Medium1,
            },
            Self::ClaudeFable { .. } => AgentRole::Engineer {
                intelligence: EngineerIntelligence::High1,
            },
            Self::AdvisorSol(_) => AgentRole::Advisor {
                intelligence: AdvisorIntelligence::Low,
            },
            Self::AdvisorAstra(_) => AgentRole::Advisor {
                intelligence: AdvisorIntelligence::Medium,
            },
            Self::ClaudeAdvisor { .. } => AgentRole::Advisor {
                intelligence: AdvisorIntelligence::Medium1,
            },
        }
    }

    pub fn deep_config(self) -> Option<InferenceProfile> {
        match self {
            Self::ResponsesSol(config)
            | Self::ResponsesLuna(config)
            | Self::ResponsesAstra(config)
            | Self::AdvisorAstra(config)
            | Self::AdvisorSol(config) => Some(config),
            Self::ClaudeFable { .. } | Self::ClaudeOpus { .. } | Self::ClaudeAdvisor { .. } => None,
        }
    }

    pub fn deep_model(self) -> Option<InferenceModel> {
        match self {
            Self::ResponsesSol(_) | Self::AdvisorSol(_) => Some(InferenceModel::Gpt61Sol),
            Self::ResponsesLuna(_) => Some(InferenceModel::Gpt6Luna),
            Self::ResponsesAstra(_) | Self::AdvisorAstra(_) => Some(InferenceModel::Gpt6Astra),
            Self::ClaudeFable { .. } | Self::ClaudeOpus { .. } | Self::ClaudeAdvisor { .. } => None,
        }
    }

    pub fn claude_model(self) -> Option<rho_claude::Model> {
        match self {
            Self::ClaudeFable { .. } | Self::ClaudeAdvisor { .. } => Some(rho_claude::Model::Fable),
            Self::ClaudeOpus { .. } => Some(rho_claude::Model::Opus),
            Self::ResponsesSol(_)
            | Self::ResponsesLuna(_)
            | Self::ResponsesAstra(_)
            | Self::AdvisorAstra(_)
            | Self::AdvisorSol(_) => None,
        }
    }

    pub fn claude_effort(self) -> Option<rho_claude::Effort> {
        match self {
            Self::ClaudeFable { effort } | Self::ClaudeAdvisor { effort } => {
                Some(effort.to_claude_effort())
            }
            Self::ClaudeOpus { effort } => Some(effort.to_claude_effort()),
            Self::ResponsesSol(_)
            | Self::ResponsesLuna(_)
            | Self::ResponsesAstra(_)
            | Self::AdvisorAstra(_)
            | Self::AdvisorSol(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ClaudeEffort {
    Medium,
    Xhigh,
    High,
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

/// A fixed prefix of one log. Both ends are positions, not row counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode)]
pub struct ContextBoundary {
    pub from: AgentEventPos,
    pub through: AgentEventPos,
}

#[derive(Clone, Debug, Default, Encode, Decode)]
pub struct NativeRecovery {
    pub archived: bool,
    pub woken: bool,
    pub compaction: CompactionState,
}

/// One event of an agent's raw log.
///
/// Both runtimes write typed `Entry` records. Claude also records its stream
/// observations and failed partial responses. The store writes configuration
/// events on the agent's behalf.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum AgentEvent<'a> {
    /// A turn started or stopped: the edge both runtimes cross.
    Turn {
        edge: TurnEdge,
        at: UnixMs,
    },
    /// The lifetime naming opportunity was consumed, before network dispatch.
    TitleAttempted {
        at: UnixMs,
    },
    /// Generated naming metadata; a spawn or user name always takes precedence.
    Titled {
        title: Option<String>,
        at: UnixMs,
    },
    /// What the last turn asks of the person.
    Wants {
        want: AgentWant,
        summary: Option<String>,
        at: UnixMs,
    },
    /// Everything from `to` up to this event is no longer the agent's
    /// history. Told, never undone: positions only grow.
    Rewound {
        to: AgentEventPos,
        at: UnixMs,
    },
    /// A request failed with this much of a response in. `retrying` when
    /// the loop makes the request again by itself; otherwise the turn
    /// ends in error right after. Never history: the next request does
    /// not carry it. Written so what the model said is not lost.
    Failed {
        partial: PendingInferenceResponse,
        error: Cow<'a, str>,
        retrying: bool,
        at: UnixMs,
    },

    /// One line of the conversation, as the Claude runtime's stream told
    /// it: a finished content block, a person's message (Claude's echo
    /// of a send), a call's results, a compaction. Rows before 7 Sep
    /// were copied from Claude Code's session file instead and carried
    /// their offset in it, a field a decoder now skips.
    Transcript {
        /// The line's uuid, the same Claude Code's session file gives it
        /// (a rewind forks the session there).
        uuid: uuid::Uuid,
        line: TranscriptLine,
        at: UnixMs,
        /// On a row the notebook produced (an exec call's results, or a
        /// message of output injected into an idle model): why the notebook
        /// spoke when it did.
        wake: Option<WakeFacts>,
    },

    // -- the runtimes' shared config log --------------------------------------
    /// The agent coming into being: the first event of every agent's log,
    /// and the base the head's config is folded from. A spawn name given
    /// here is why no title is generated for that agent.
    Created {
        role: AgentRole,
        binding: SessionBinding,
        runtime: AgentRuntime,
        place: Place,
        spawned_by: AgentSpawnedBy,
        spawn_name: Option<String>,
        created_at: rho_agent_types::UnixMs,
        /// The agent that spawned this one.
        parent: Option<AgentId>,
    },
    RoleChanged {
        role: AgentRole,
        /// `None` when only the role moved and the session binding stands.
        binding: Option<SessionBinding>,
        at: UnixMs,
    },
    /// Something Rho has to tell the agent, carried ahead of its next user
    /// message and then done: what a migration did to its place, say.
    Notice {
        text: Cow<'a, str>,
        at: UnixMs,
    },
    /// The runtime itself changing under the agent: a Claude rewind before
    /// and after its destination transcript is verified, or a new prompt
    /// cache key for the Rho runtime.
    RuntimeRebound {
        change: RuntimeChange,
        at: UnixMs,
    },
    /// A provider/host lifecycle observation shared only for presentation.
    /// It neither changes native context nor claims ownership of Claude
    /// history.
    ExecObserved {
        id: rho_agent_types::transcript::ExecId,
        milestone: rho_agent_types::ExecMilestone,
        at: UnixMs,
    },
    /// Claude owns its conversation; this records only Rho's permission to
    /// execute a cell, committed before the notebook can perform side effects.
    ClaudeExecAdmitted {
        call: rho_agent_types::transcript::ExecCall,
        at: UnixMs,
    },
    ClaudeOutput {
        batch: ClaudeOutputBatch,
    },
    ClaudeOutputHandedOff {
        id: uuid::Uuid,
        at: UnixMs,
    },
    /// One of the Rho runtime's own rows.
    #[senax(rename = "TypedEntry")]
    Entry(entry::Entry),
}

/// Leased notebook contributions transferred to durable host ownership before
/// contacting Claude Code. A handoff does not prove remote consumption.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct ClaudeOutputBatch {
    pub id: uuid::Uuid,
    pub outputs: Vec<(
        rho_agent_types::transcript::ExecId,
        rho_agent_types::transcript::ToolOutput,
    )>,
    pub wake: WakeFacts,
    pub at: UnixMs,
}

/// Why a request went out when it did: the scheduler's reading of its
/// sources at the boundary (`agent/boundary.rs`), recorded with the request
/// so the pace of a session can be judged from its log.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct WakeFacts {
    /// The candidate whose deadline came first.
    pub trigger: WakeTrigger,
    /// Every event pending at the boundary, all of which the request carries.
    pub events: Vec<WakeEvent>,
    /// Jobs and cells still running that the model was watching...
    pub foreground_running: u64,
    /// ...and ones it had moved on from.
    pub background_running: u64,
    /// The latest cell asked not to be woken by the notebook.
    pub tools_suppressed: bool,
    /// When the model's check-in was due, if its turn had one.
    pub checkin_at: Option<UnixMs>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum WakeTrigger {
    /// A previously selected durable output batch is handed off.
    Delivery,
    /// A request somebody asked for outright: a retry, a compaction.
    Asked,
    /// A failed request's own clock.
    Retry,
    User,
    Mail,
    /// A cell's `notify()`.
    Notify,
    /// A job ended, or a cell returned with output.
    Finished,
    /// The model's check-in came due.
    Checkin,
}

/// One pending event as the scheduler saw it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct WakeEvent {
    pub cell: u64,
    /// The job's registration time, or `u64::MAX` for the cell itself.
    pub source: u64,
    pub kind: WakeKind,
    pub foreground: bool,
    /// When the tool recorded it.
    pub occurred_at: UnixMs,
    /// When the scheduler first saw it while able to act: where its
    /// patience was measured from.
    pub seen_at: UnixMs,
    /// When it would have sent on its own; `None` for an event content to
    /// wait for the foreground or the check-in.
    pub deadline: Option<UnixMs>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Encode, Decode)]
pub enum WakeKind {
    Notify,
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum RuntimeChange {
    /// A message-only Claude rewind whose destination transcript has not
    /// been materialized and verified yet; `None` withdraws one. The old
    /// runtime stays authoritative until it is confirmed.
    ClaudeRewindPending(Option<ClaudeRewind>),
    /// The rewind landed: this session is the runtime now.
    ClaudeRewound {
        session_id: uuid::Uuid,
    },
    PromptCacheKey(crate::inference::PromptCacheKey),
}

/// One input in the live Claude queue waiting to reach the model.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct QueuedInput {
    pub source: MessageSender,
    pub kind: InputKind,
    pub at: UnixMs,
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum InputKind {
    Message {
        content: Vec<ContentPart>,
    },
    /// The user explicitly asked to compact. Automatic compaction is not an
    /// input at all — it happens while building a request.
    Compaction,
}

/// What one transcript line says, as far as a reader needs. Bodies are
/// whole: the wire strips them.
#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum TranscriptLine {
    /// The person spoke (an agent's mail reaches Claude the same way).
    User { text: String },
    /// The model spoke or called. One line per content block, so a text
    /// and the call after it are two rows. Usage rides on the first row
    /// of a message only, so a reader counts each request once.
    Assistant {
        text: String,
        calls: Vec<TranscriptCall>,
        usage: Option<AgentUsageBucket>,
        /// Context-window occupancy after this message.
        context_used: Option<u64>,
    },
    /// What the calls came back with.
    ToolResults {
        results: Vec<rho_agent_types::transcript::ToolResult>,
    },
    /// Claude compacted the context here.
    Compacted { context_used: Option<u64> },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub struct TranscriptCall {
    pub id: String,
    pub name: String,
    /// The arguments as JSON, whole.
    pub arguments: String,
}

#[cfg(test)]
mod encoding_tests {
    use rho_agent_types::{AgentWant, TurnEdge, UnixMs};
    use senax_encoder::{Decoder as _, Encoder as _};

    use super::*;

    #[test]
    fn log_events_roundtrip_through_senax() {
        let events = vec![
            AgentEvent::Entry(entry::Entry::Received {
                at: UnixMs(9),
                id: entry::MessageId(41),
                from: entry::Party::Human,
                body: vec![entry::Block::Text("current log".into())],
            }),
            AgentEvent::Turn {
                edge: TurnEdge::Ended(rho_agent_types::TurnOutcome::Errored {
                    message: "boom".to_owned(),
                }),
                at: UnixMs(12),
            },
            AgentEvent::Titled {
                title: Some("title".to_owned()),
                at: UnixMs(13),
            },
            AgentEvent::Wants {
                want: AgentWant::Ask,
                summary: Some("which one?".to_owned()),
                at: UnixMs(14),
            },
            AgentEvent::Rewound {
                to: crate::log::AgentEventPos::new(3),
                at: UnixMs(15),
            },
            AgentEvent::Transcript {
                uuid: uuid::uuid!("00000000-0000-4000-8000-000000000002"),
                line: TranscriptLine::Assistant {
                    text: "read it".to_owned(),
                    calls: vec![TranscriptCall {
                        id: "toolu_1".to_owned(),
                        name: "Read".to_owned(),
                        arguments: "{\"path\":\"a.rs\"}".to_owned(),
                    }],
                    usage: None,
                    context_used: Some(4321),
                },
                at: UnixMs(17),
                wake: None,
            },
        ];
        for event in events {
            let mut buffer = bytes::BytesMut::new();
            event.encode(&mut buffer).expect("encode");
            let mut reader = buffer.freeze();
            let decoded = AgentEvent::decode(&mut reader).expect("decode");
            assert_eq!(decoded, event);
        }
    }
}
