//! The mirror: what a client keeps of an agent's raw log.
//!
//! Every [`MirrorEvent`] is `strip` of exactly one raw event, in that
//! event's position (`AGENT-LOG-DESIGN.md`, "the mirror is a pure function
//! of the raw log"). Bodies a person does not read at a glance (tool
//! output, reasoning, the argument blob) are left behind; a client asks
//! for them by position when it wants them.
//!
//! This vocabulary is shared with the daemon's raw log: the runtimes write
//! these very types, so there are no twins to keep in step.

use camino::Utf8PathBuf;
use rho_core::{AgentId, AgentRole, MessageDelivery, UnixMs};
use senax_encoder::{Decode, Encode, Pack, Unpack};

use crate::WorkspaceInfo;

/// A position in one agent's log: dense, starting at zero with the
/// agent's creation, never reused. A rewind is told at a new position
/// (`Rewound`) and hides the ones before it; nothing moves.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct AgentPos(#[senax(default)] pub u64);

impl AgentPos {
    pub const ZERO: Self = Self(0);

    pub fn next(self) -> Self {
        Self(self.0.checked_add(1).expect("agent log position overflow"))
    }
}

/// One place in a host's journal, the global order of every append on
/// that host. Zero is "before anything".
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Encode, Decode, Pack, Unpack,
)]
pub struct Seq(pub u64);

impl Seq {
    pub fn next(self) -> Self {
        Self(self.0.checked_add(1).expect("journal overflow"))
    }
}

/// What a turn asks of the person.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum AgentWant {
    /// Something concrete to look at.
    Show,
    /// Something only the person can give: a decision, or an act.
    Ask,
    /// The person asked a question and this reply answers it.
    Answer,
}

/// Whose transcript this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum RuntimeKind {
    Rho,
    Claude,
}

/// Who asked for this agent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum SpawnedBy {
    #[default]
    Direct,
    Engineer,
}

/// What a tool call shows: what it acted on, whole, never its output.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ToolLine {
    Path(Utf8PathBuf),
    Command(String),
    Query(String),
    Agent(AgentId),
    Nothing,
}

impl ToolLine {
    /// The line as a reader sees it next to the tool's name.
    pub fn text(&self) -> String {
        match self {
            Self::Path(path) => path.to_string(),
            Self::Command(command) | Self::Query(command) => command.clone(),
            Self::Agent(agent) => agent.encoded(),
            Self::Nothing => String::new(),
        }
    }
}

/// How a turn stopped.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum TurnOutcome {
    Completed,
    Cancelled,
    Errored { message: String },
}

/// A turn beginning or ending.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum TurnEdge {
    Started,
    Ended(TurnOutcome),
}

/// One field of a sidecar proposal. `Clear` stays distinct from
/// `Unchanged` so a stale label can be dropped without inventing a new one.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum PresentationField {
    Unchanged,
    Set(String),
    Clear,
}

/// Who said a mirrored Claude message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Speaker {
    User,
    Agent,
    Assistant,
}

/// What one model response cost, as the provider reported it. The model
/// is named so a client can price it; the tables the daemon keeps hold
/// the same numbers keyed by time.
#[derive(Clone, Debug, Default, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct Usage {
    pub model: String,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_write_1h_tokens: u64,
    pub output_tokens: u64,
}

/// One call a response made: enough to draw it. `what` is the argument a
/// person recognises, for the row's label; `arguments` is what the model
/// actually sent, whole, because that is what a reader of a transcript is
/// reading. A code-mode `exec` call has no field a label could name — its
/// arguments are JavaScript source, not JSON — so without this it drew as
/// the word "exec" and the code was gone.
///
/// A result is still a body fetched by position. Arguments are not: they
/// are small next to an output, they are what the reader came for, and
/// asking for them by position would mean a transcript that cannot be read
/// until it is asked twice.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ToolCallLine {
    pub id: String,
    pub name: String,
    pub what: ToolLine,
    /// Empty when the row came from a daemon older than this field; the
    /// client then draws the label alone, as it did before.
    #[senax(default)]
    pub arguments: String,
}

/// How one call ended. The output is a body; ask for it by position.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct ToolOutcome {
    pub id: String,
    pub status: ToolStatus,
    pub started_at: UnixMs,
    pub finished_at: UnixMs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ToolStatus {
    Success,
    Error,
    Cancelled,
}

/// One raw event, stripped. Every variant says when, because "how long
/// has it been?" is the reader's question and nothing else answers it.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum MirrorEvent {
    /// Position zero of every agent.
    Created {
        role: AgentRole,
        runtime: RuntimeKind,
        workdirs: Vec<WorkspaceInfo>,
        spawned_by: SpawnedBy,
        spawn_name: Option<String>,
        parent: Option<AgentId>,
        /// The model the agent's binding names, so cost can be priced
        /// from the first reply.
        model: String,
        at: UnixMs,
    },
    RoleChanged {
        role: AgentRole,
        /// The model after the change, when the binding moved with it.
        model: Option<String>,
        at: UnixMs,
    },
    WorkdirAdded {
        workdir: WorkspaceInfo,
        at: UnixMs,
    },
    /// The first workdir replaced by a workset (`rho debug migrate-agent`).
    WorkdirMigrated {
        workdir: WorkspaceInfo,
        at: UnixMs,
    },
    /// The person or another agent spoke. Queued until a later `Sent`
    /// carries it; a `QueueCleared` before that drops it.
    Message {
        /// `None` when the person wrote it.
        from: Option<AgentId>,
        text: String,
        delivery: MessageDelivery,
        at: UnixMs,
    },
    /// The person asked for a compaction; queued like a message.
    CompactionRequested {
        at: UnixMs,
    },
    QueueCleared {
        at: UnixMs,
    },
    /// A request went out: everything queued went with it, and these
    /// calls came back with these results.
    Sent {
        results: Vec<ToolOutcome>,
        /// The request asked the model to compact.
        compaction: bool,
        at: UnixMs,
    },
    /// Calls came back with these results and nothing else moved: what
    /// was queued is still queued. A Claude agent's tool results; a Rho
    /// request, which carries the queue, is `Sent`.
    Results {
        results: Vec<ToolOutcome>,
        at: UnixMs,
    },
    /// The model answered.
    Replied {
        /// What it said, whole. Empty when it only called tools.
        text: String,
        calls: Vec<ToolCallLine>,
        /// The answer compacted the context.
        compacted: bool,
        usage: Option<Usage>,
        context_used: Option<u64>,
        at: UnixMs,
    },
    Turn {
        edge: TurnEdge,
        at: UnixMs,
    },
    /// The sidecar's title and activity for this agent.
    Presented {
        title: PresentationField,
        activity: PresentationField,
        at: UnixMs,
    },
    /// What the last turn asks of the person.
    Wants {
        want: AgentWant,
        summary: Option<String>,
        at: UnixMs,
    },
    /// Everything from `to` up to here is no longer the agent's history.
    Rewound {
        to: AgentPos,
        at: UnixMs,
    },
    /// A request failed with this much said. `retrying` when the runtime
    /// asks again by itself; otherwise the turn ends in error next.
    Failed {
        text: String,
        error: String,
        retrying: bool,
        at: UnixMs,
    },
    /// One message of a Claude transcript, mirrored as Claude confirmed
    /// it. Claude's own file is not ours to keep; this is the durable copy.
    ClaudeMessage {
        speaker: Speaker,
        text: String,
        at: UnixMs,
    },
}

impl MirrorEvent {
    pub fn at(&self) -> UnixMs {
        match self {
            Self::Created { at, .. }
            | Self::RoleChanged { at, .. }
            | Self::WorkdirAdded { at, .. }
            | Self::WorkdirMigrated { at, .. }
            | Self::Message { at, .. }
            | Self::CompactionRequested { at }
            | Self::QueueCleared { at }
            | Self::Sent { at, .. }
            | Self::Results { at, .. }
            | Self::Replied { at, .. }
            | Self::Turn { at, .. }
            | Self::Presented { at, .. }
            | Self::Wants { at, .. }
            | Self::Rewound { at, .. }
            | Self::Failed { at, .. }
            | Self::ClaudeMessage { at, .. } => *at,
        }
    }
}

/// One journal entry as it crosses the wire: where it is in the host's
/// order, whose log it is, where in that log, and what it says.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct LogEntry {
    pub seq: Seq,
    pub agent_id: AgentId,
    pub pos: AgentPos,
    pub event: MirrorEvent,
}

/// What a runtime has that the log does not yet, told as it changes:
/// the response in flight and the phase. Everything else a reader wants
/// is a row: the queue is `Message` rows no `Sent` has carried, a call
/// runs until a `Sent` answers it, a turn ends with a `Turn` row.
///
/// The runtime writes its row and then says what the tail is from the
/// same task, so a client applies the row first and the tail after;
/// nothing ever shows twice. A joiner is told `Requesting`, one `Item`
/// per index, then the phase, and drops an `Appended` for an index it
/// does not hold. Told for every agent any client is looking at, to
/// every client.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Live {
    /// A request went out; the tail is empty. Also a retry: the partial
    /// response went to the log as `Failed` first.
    Requesting,
    /// First sight of an item, or a change that is not an append.
    Item { index: u32, item: Item },
    /// The item's text grew by this much.
    Appended { index: u32, text: String },
    /// Calls are running, or the model asked to be left alone until then.
    Waiting { until: Option<UnixMs> },
    /// Nothing in flight.
    Idle,
    /// What waits to go in, whole, whenever it changes. A Claude agent's
    /// queue lives in Claude Code's process and nothing persists it, so
    /// it is told here and never as rows; the native runtime's queue is
    /// its `Message` rows, and it says nothing here.
    Queued { items: Vec<QueuedItem> },
}

/// One thing waiting in an agent's queue.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum QueuedItem {
    Message {
        /// `None` when the person wrote it.
        from: Option<AgentId>,
        text: String,
        delivery: MessageDelivery,
    },
    Compaction,
}

/// One item of a response, as it streams or as `Detail` hands it back.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Item {
    Text {
        text: String,
        phase: Option<TextPhase>,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
}

/// Whether a text item is the model thinking aloud or its answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum TextPhase {
    Commentary,
    FinalAnswer,
}

/// The bodies one raw event carries, for a client that asked.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum DetailBody {
    /// The event is not there, or has no body worth asking for.
    Nothing,
    /// A `Sent`: the results, output and all.
    Results(Vec<DetailResult>),
    /// A `Replied`: the response whole, reasoning and arguments included.
    Response(Vec<Item>),
}

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub struct DetailResult {
    pub id: String,
    pub status: ToolStatus,
    pub output: String,
    pub error: Option<String>,
}
