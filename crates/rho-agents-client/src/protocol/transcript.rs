//! Transcripts: what a client keeps of an agent's raw log.
//!
//! Every [`TranscriptEvent`] is `strip` of exactly one raw event, in that
//! event's position: the transcript is a pure function of the raw log.
//! Bodies a person does not read at a glance (tool
//! output, reasoning, the argument blob) are left behind; a client asks
//! for them by position when it wants them.
//!
//! The runtimes write their own events; the agent host's `strip` is the one
//! place they become these. The log's positions and the facts every reader
//! takes as the runtime wrote them (turn edges, wants) are in
//! `rho-agent-types`.

use rho_agent_types::{
    AgentId, AgentPos, AgentRole, AgentWant, MessageDelivery, Place, PresentationField, Seq,
    TurnEdge, UnixMs, WorksetMode,
};
use senax_encoder::{Decode, Encode, Pack, Unpack};

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
    /// An Engineer started it for the user, who manages it: it has no
    /// parent.
    UserOwned {
        by: AgentId,
    },
}

/// Who said a mirrored Claude message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum Speaker {
    User,
    Agent,
    Assistant,
}

/// What one model response cost, as the provider reported it. The model
/// is named so a client can price it; the tables the agent host keeps hold
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
pub enum TranscriptEvent {
    /// Position zero of every agent.
    Created {
        role: AgentRole,
        runtime: RuntimeKind,
        place: Place,
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
    /// The agent sees the filesystem this way from here on.
    ModeChanged {
        mode: WorksetMode,
        at: UnixMs,
    },
    /// Something Rho has to tell the agent, carried by its next user
    /// message: what a migration did to its place, say.
    Notice {
        text: String,
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
        /// Visible response items, in model order, just like the live tail.
        items: Vec<Item>,
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
    ExecObserved {
        id: String,
        milestone: rho_agent_types::ExecMilestone,
        at: UnixMs,
    },
}

impl TranscriptEvent {
    pub fn at(&self) -> UnixMs {
        match self {
            Self::Created { at, .. }
            | Self::RoleChanged { at, .. }
            | Self::ModeChanged { at, .. }
            | Self::Notice { at, .. }
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
            | Self::ClaudeMessage { at, .. }
            | Self::ExecObserved { at, .. } => *at,
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
    pub event: TranscriptEvent,
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

/// One response item, shared by the live tail, committed mirror, and `Detail`.
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
        /// How to read `arguments`.
        format: ArgumentsFormat,
    },
}

/// What a call's `arguments` string holds. A function tool is given a JSON
/// object, which is only whole once the call is; a custom tool is given the
/// text the model wrote, which is never JSON and must not be parsed as it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode, Pack, Unpack)]
pub enum ArgumentsFormat {
    Json,
    Text,
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
