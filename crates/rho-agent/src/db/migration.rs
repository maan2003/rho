//! One format hop from b4e2c7a1. Remove after the migrated build has opened
//! active developer databases. Never normalize legacy rows in live readers.
use crate::*;
use rho_core::{ExecOutput, ProviderResponseId};
use rho_db::{RecordedTypeName, SenAs, SenValue, WriteTxn};
use redb::TableDefinition;

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum Event<'a> {
    /// An input entered a queue: user text, mail, or a `/compact`. It becomes
    /// context when a later `Sent` carries it.
    Accepted(QueuedInput),
    /// A boundary: every source was drained into `blocks`, they were appended
    /// to history, and a request went out carrying all of it.
    ///
    /// One event rather than an append and a start, because it was always one
    /// thing — and because the drain, the append and the send cannot come
    /// apart even in a crash. `blocks` can be empty: a retry or a resume
    /// sends with nothing pending, which is a fact worth being able to write
    /// down.
    Sent {
        blocks: Cow<'a, [ContextBlock]>,
        #[senax(default)]
        at: UnixMs,
        /// Why the request went out when it did; absent on rows from before
        /// the scheduler recorded it.
        #[senax(default)]
        wake: Option<WakeFacts>,
    },
    /// A request boundary that also advances context rotation. Preparation
    /// preserves queued input; other context boundaries drain it normally.
    ContextSent {
        blocks: Cow<'a, [ContextBlock]>,
        change: ContextChange,
        at: UnixMs,
        wake: Option<WakeFacts>,
    },
    /// The model answered, and the request is over.
    Replied {
        blocks: Cow<'a, [ContextBlock]>,
        /// Context-window occupancy after this response (all input plus
        /// output tokens), or `None` when it compacted or usage was missing.
        context_used: Option<u64>,
        /// What the response cost, as the provider reported it. Told
        /// here, at the response, so a reader can price the transcript
        /// without a usage table (`AGENT-LOG-DESIGN.md`).
        #[senax(default)]
        usage: Option<crate::db::AgentUsageBucket>,
        #[senax(default)]
        at: UnixMs,
    },
    /// All queued items were dropped (cancel). Written before the log
    /// carried times; `Cleared` is what is written now.
    QueueCleared,
    Cleared {
        at: UnixMs,
    },
    /// A turn started or stopped: the edge both runtimes cross.
    Turn {
        edge: TurnEdge,
        at: UnixMs,
    },
    /// The sidecar's title and activity, applied.
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
        #[senax(default)]
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
        created_at: rho_core::UnixMs,
        /// The agent that spawned this one.
        #[senax(default)]
        parent: Option<AgentId>,
    },
    RoleChanged {
        role: AgentRole,
        /// `None` when only the role moved and the session binding stands.
        binding: Option<SessionBinding>,
        #[senax(default)]
        at: UnixMs,
    },
    /// The agent now sees the filesystem this way: the same workset and
    /// directory, entered in the other mode at its next load.
    ModeChanged {
        mode: WorksetMode,
        #[senax(default)]
        at: UnixMs,
    },
    /// Something Rho has to tell the agent, carried ahead of its next user
    /// message and then done: what a migration did to its place, say.
    Notice {
        text: Cow<'a, str>,
        #[senax(default)]
        at: UnixMs,
    },
    /// The runtime itself changing under the agent: a Claude rewind before
    /// and after its destination transcript is verified, or a new prompt
    /// cache key for the Rho runtime.
    RuntimeRebound {
        change: RuntimeChange,
        #[senax(default)]
        at: UnixMs,
    },
    /// Durable admission and settlement of streaming Python units.
    PythonStream {
        event: PythonStreamEvent,
        at: UnixMs,
    },
    /// A provider/host lifecycle observation shared only for presentation.
    /// It neither changes native context nor claims ownership of Claude
    /// history.
    ExecObserved {
        id: rho_core::ExecId,
        milestone: rho_core::ExecMilestone,
        at: UnixMs,
    },
    /// Claude owns its conversation; this records only Rho's permission to
    /// execute a cell, committed before the notebook can perform side effects.
    ClaudeExecAdmitted {
        call: rho_core::ExecCall,
        at: UnixMs,
    },
    /// Canonical native conversation records; legacy block rows are read-only.
    Native(NativeEvent),
    ClaudeOutput {
        batch: ClaudeOutputBatch,
    },
    ClaudeOutputHandedOff {
        id: uuid::Uuid,
        at: UnixMs,
    },
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum Input {
    UserMessage {
        sender: MessageSender,
        content: Vec<ContentPart>,
    },
    ExecOutput(ExecOutput),
    /// Read compatibility only; new notebook output never uses this shape.
    Historical(ContextBlock),
    DeveloperMessage {
        text: String,
    },
    CompactionTrigger,
    ContextRotation {
        retain_from: u64,
    },
    /// A crash-recovered admitted prefix, not a new model response.
    RecoveredResponse {
        items: Vec<InferenceResponseItem>,
        provider_response_id: Option<ProviderResponseId>,
    },
}

impl Input {
    /// Materialize the provider's replay vocabulary at the adapter boundary.
    pub fn context(&self) -> ContextBlock {
        match self {
            Self::UserMessage { sender, content } => ContextBlock::UserMessage {
                sender: sender.clone(),
                content: content.clone(),
            },
            Self::ExecOutput(output) => rho_inference::exec::output(output),
            Self::Historical(block) => block.clone(),
            Self::DeveloperMessage { text } => {
                ContextBlock::DeveloperMessage { text: text.clone() }
            }
            Self::CompactionTrigger => ContextBlock::CompactionTrigger,
            Self::ContextRotation { retain_from } => ContextBlock::ContextRotation {
                retain_from: *retain_from,
            },
            Self::RecoveredResponse {
                items,
                provider_response_id,
            } => ContextBlock::InferenceResponse {
                items: items.clone(),
                provider_response_id: provider_response_id.clone(),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Encode, Decode)]
pub enum NativeEvent {
    RequestStarted {
        input: Vec<Input>,
        context: Option<ContextChange>,
        wake: Option<WakeFacts>,
        at: UnixMs,
    },
    ResponseFinished {
        items: Vec<InferenceResponseItem>,
        provider_response_id: Option<ProviderResponseId>,
        context_used: Option<u64>,
        usage: Option<crate::db::AgentUsageBucket>,
        at: UnixMs,
    },
    RequestFailed {
        partial: PendingInferenceResponse,
        error: String,
        retrying: bool,
        at: UnixMs,
    },
    PythonStream {
        event: PythonStreamEvent,
        at: UnixMs,
    },
}


impl Event<'static> {
    pub(super) fn current(self, native: bool) -> AgentEvent<'static> {
        use crate::native::NativeEvent as Current;
        let event = match self {
            Self::Sent { blocks, at, wake } => Current::RequestStarted {
                input: blocks.into_owned(), context: None, wake, at,
            },
            Self::ContextSent { blocks, change, at, wake } => Current::RequestStarted {
                input: blocks.into_owned(), context: Some(change), wake, at,
            },
            Self::Replied { blocks, context_used, usage, at } => Current::ResponseFinished {
                output: blocks.into_owned(), context_used, usage, at,
            },
            Self::Failed { partial, error, retrying, at } if native => Current::RequestFailed {
                partial, error: error.into_owned(), retrying, at,
            },
            Self::PythonStream { event, at } => Current::PythonStream { event, at },
            Self::Native(event) => match event {
                NativeEvent::RequestStarted { input, context, wake, at } => Current::RequestStarted {
                    input: input.iter().map(Input::context).collect(), context, wake, at,
                },
                NativeEvent::ResponseFinished { items, provider_response_id, context_used, usage, at } =>
                    Current::ResponseFinished {
                        output: vec![ContextBlock::InferenceResponse { items, provider_response_id }],
                        context_used, usage, at,
                    },
                NativeEvent::RequestFailed { partial, error, retrying, at } =>
                    Current::RequestFailed { partial, error, retrying, at },
                NativeEvent::PythonStream { event, at } => Current::PythonStream { event, at },
            },
            Self::QueueCleared => return AgentEvent::Cleared { at: UnixMs(0) },
            Self::Presented { title, at, .. } => return match title {
                PresentationField::Set(title) => AgentEvent::Titled { title: Some(title), at },
                PresentationField::Clear => AgentEvent::Titled { title: None, at },
                PresentationField::Unchanged => AgentEvent::TitleAttempted { at },
            },
            // Unchanged observation/configuration shapes. Tagged provider data
            // is decoded/re-encoded by its owning codec, not flattened to text.
            other => {
                let bytes = senax_encoder::encode(&other).expect("encode migration row");
                return senax_encoder::decode(&mut bytes.as_ref()).expect("decode unchanged migration row");
            }
        };
        AgentEvent::Native(event)
    }
}

#[derive(Debug)]
struct LogName;
impl RecordedTypeName for LogName {
    const NAME: &'static str = "rho-db::Sen<rho_agent::AgentEvent>";
}
const OLD_LOG: TableDefinition<(AgentId, u64), SenAs<Event<'static>, LogName>> =
    TableDefinition::new("agent_log");

pub(super) fn migrate(write: &mut WriteTxn) {
    use std::ops::Bound::{Excluded, Unbounded};
    let mut after = None;
    let mut native = true;
    // Bounded batches, in RAW key order, including branches hidden by rewind.
    // Every row keeps its key and every context block keeps its grouping.
    loop {
        let rows = {
            let log = write.open_table(OLD_LOG);
            log.range((after.map_or(Unbounded, Excluded), Unbounded))
                .take(128)
                .map(|(key, value)| (key.value(), value.value().into_owned()))
                .collect::<Vec<_>>()
        };
        if rows.is_empty() { break; }
        let mut log = write.open_table(super::AGENT_LOG);
        for (key, event) in rows {
            if let Event::Created { runtime, .. } = &event {
                native = matches!(runtime, crate::db::AgentRuntime::Rho { .. });
            }
            let current = event.current(native);
            log.insert(&key, SenValue::borrowed(&current));
            after = Some(key);
        }
    }
}
