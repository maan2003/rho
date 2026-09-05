//! The story on the wire: the log a person reads, twinned for clients.
//!
//! These mirror `rho_agent::story` one variant for one variant. They are
//! twins rather than the types themselves because this crate is the light,
//! wasm-capable vocabulary and must not pull the agent runtime in; the
//! daemon converts, the way it already does for `UiBlock` and `UiTool`.

use camino::Utf8PathBuf;
use rho_core::{AgentId, AgentRole, UnixMs};
use senax_encoder::{Decode, Encode, Pack, Unpack};

use crate::WorkspaceInfo;

/// A position in one agent's story. Only ever grows: a rewind is told,
/// never unwritten.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct UiStoryPos(pub u64);

impl UiStoryPos {
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// What a turn asks of the person.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiAgentWant {
    /// Something concrete to look at.
    Show,
    /// Something only the person can give: a decision, or an act.
    Ask,
    /// The person asked a question and this reply answers it.
    Answer,
}

/// Whose transcript this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiRuntimeKind {
    Rho,
    Claude,
}

/// Who asked for this agent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiSpawnedBy {
    #[default]
    Direct,
    Engineer,
}

/// The one line a tool call shows: what it acted on, never its output.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiToolLine {
    Path(Utf8PathBuf),
    Command(String),
    Query(String),
    Agent(AgentId),
    Nothing,
}

/// How a turn stopped.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiTurnOutcome {
    Completed,
    Cancelled,
    Errored { message: String },
}

/// One thing that happened, as a person would hear it told. Every variant
/// carries when, because "how long has it been?" is the reader's question
/// and nothing else in the story answers it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum UiStoryEvent {
    Created {
        role: AgentRole,
        runtime_kind: UiRuntimeKind,
        workdirs: Vec<WorkspaceInfo>,
        spawned_by: UiSpawnedBy,
        spawn_name: Option<String>,
        at: UnixMs,
    },
    /// Who spawned this agent, by id: told separately from `Created`
    /// because the agents that predate the story learn it afterwards.
    Parented {
        parent: AgentId,
        at: UnixMs,
    },
    UserMessage {
        text: String,
        at: UnixMs,
    },
    AgentMail {
        from: AgentId,
        text: String,
        at: UnixMs,
    },
    TurnStarted {
        at: UnixMs,
    },
    TurnEnded {
        outcome: UiTurnOutcome,
        at: UnixMs,
    },
    Reply {
        text: String,
        at: UnixMs,
    },
    ToolCall {
        name: String,
        what: UiToolLine,
        at: UnixMs,
    },
    Wants {
        want: UiAgentWant,
        summary: Option<String>,
        at: UnixMs,
    },
    Titled {
        title: String,
        at: UnixMs,
    },
    Activity {
        label: Option<String>,
        at: UnixMs,
    },
    Cost {
        usage: crate::AgentUsageBucket,
        at: UnixMs,
    },
    /// A rewind is told, not undone: a reader hides its view past `to`.
    Rewound {
        to: UiStoryPos,
        at: UnixMs,
    },
    Compacted {
        at: UnixMs,
    },
    RoleChanged {
        role: AgentRole,
        at: UnixMs,
    },
    WorkdirAdded {
        workdir: WorkspaceInfo,
        at: UnixMs,
    },
    /// The first event of a migrated agent whose history could not be
    /// recovered: the Claude session file it lived in is gone.
    HistoryUnavailableBefore {
        at: UnixMs,
    },
}

impl UiStoryEvent {
    /// When it happened. Every event has one.
    pub fn at(&self) -> UnixMs {
        match self {
            Self::Created { at, .. }
            | Self::Parented { at, .. }
            | Self::UserMessage { at, .. }
            | Self::AgentMail { at, .. }
            | Self::TurnStarted { at }
            | Self::TurnEnded { at, .. }
            | Self::Reply { at, .. }
            | Self::ToolCall { at, .. }
            | Self::Wants { at, .. }
            | Self::Titled { at, .. }
            | Self::Activity { at, .. }
            | Self::Cost { at, .. }
            | Self::Rewound { at, .. }
            | Self::Compacted { at }
            | Self::RoleChanged { at, .. }
            | Self::WorkdirAdded { at, .. }
            | Self::HistoryUnavailableBefore { at } => *at,
        }
    }
}

/// What an agent is, and how far its story runs: the agents list, so a
/// title or a role never waits on a log.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct UiAgentHead {
    pub agent_id: AgentId,
    /// How far this agent's story runs; the client asks for what it lacks.
    pub story_pos: UiStoryPos,
    pub role: AgentRole,
    pub runtime_kind: UiRuntimeKind,
    /// Where the agent works, primary workdir first.
    pub workdirs: Vec<WorkspaceInfo>,
    pub spawned_by: UiSpawnedBy,
    /// The agent that spawned this one, for presenting delegated work
    /// beneath its parent.
    pub parent: Option<AgentId>,
    /// The name the spawner gave; it always beats a generated title.
    pub spawn_name: Option<String>,
    /// The sidecar's title, when there is no spawn name.
    pub generated_title: Option<String>,
    /// The last durable activity label; `None` when idle.
    pub activity: Option<String>,
    pub turn_running: bool,
    pub created_at: UnixMs,
}

impl UiAgentHead {
    /// What to call this agent: what the spawner named it, else what the
    /// sidecar titled it.
    pub fn title(&self) -> Option<&str> {
        self.spawn_name
            .as_deref()
            .or(self.generated_title.as_deref())
    }

    /// The agent's primary workdir (entry 0).
    pub fn workspace(&self) -> Option<&WorkspaceInfo> {
        self.workdirs.first()
    }
}
