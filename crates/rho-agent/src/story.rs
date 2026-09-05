//! The story: the log a person reads. One typed event per thing that
//! happened, positions that only grow, no tool output and no raw model
//! exchange (`AGENT-LOG-DESIGN.md`). The raw log stays the runtime's
//! business; this is what clients mirror.

use camino::Utf8PathBuf;
use rho_core::{AgentId, InferenceResponseItem, ToolName};
use rho_workspaces::WorkspaceInfo;
use senax_encoder::{Decode, Encode};

use crate::db::{AgentRole, AgentSpawnedBy, AgentUsageBucket, StoryPos, UnixMillis};

/// What an agent says this turn asks of the person, declared by the tag
/// its reply ended with (`AGENT-WANTS-DESIGN.md`). Never derived: no tag
/// means nothing is declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum AgentWant {
    /// Something concrete to look at.
    Show,
    /// Something only the person can give: a decision, or an act.
    Ask,
    /// The person asked a question and this reply answers it.
    Answer,
}

/// Which runtime ran the turn. The runtime's own configuration is the
/// head's; a reader only needs to know whose transcript this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub enum RuntimeKind {
    Rho,
    Claude,
}

/// The one line a tool call shows: the thing it acted on, never its
/// output. `Nothing` is for calls whose arguments say nothing a person
/// would read.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum ToolLine {
    Path(Utf8PathBuf),
    Command(String),
    Query(String),
    Agent(AgentId),
    Nothing,
}

/// How a turn stopped.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum TurnOutcome {
    Completed,
    Cancelled,
    Errored { message: String },
}

/// One thing that happened, as a person would hear it told. Every
/// variant carries when it happened, because the reader's questions
/// ("how long has it been?") are about time and nothing else in the
/// story answers them.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum StoryEvent {
    Created {
        role: AgentRole,
        runtime_kind: RuntimeKind,
        workdirs: Vec<WorkspaceInfo>,
        spawned_by: AgentSpawnedBy,
        spawn_name: Option<String>,
        at: UnixMillis,
    },
    UserMessage {
        text: String,
        at: UnixMillis,
    },
    AgentMail {
        from: AgentId,
        text: String,
        at: UnixMillis,
    },
    TurnStarted {
        at: UnixMillis,
    },
    TurnEnded {
        outcome: TurnOutcome,
        at: UnixMillis,
    },
    /// The agent's visible message text, whole, once the turn wrote it.
    Reply {
        text: String,
        at: UnixMillis,
    },
    /// The call, never the answer: the path, the command, the query.
    ToolCall {
        name: ToolName,
        what: ToolLine,
        at: UnixMillis,
    },
    /// The tag the reply ended with, when there was one.
    Wants {
        want: AgentWant,
        at: UnixMillis,
    },
    Titled {
        title: String,
        at: UnixMillis,
    },
    Activity {
        label: Option<String>,
        at: UnixMillis,
    },
    Cost {
        usage: AgentUsageBucket,
        at: UnixMillis,
    },
    /// A rewind is told, not undone: positions never go backwards and a
    /// reader hides its view past `to`.
    Rewound {
        to: StoryPos,
        at: UnixMillis,
    },
    Compacted {
        at: UnixMillis,
    },
    RoleChanged {
        role: AgentRole,
        at: UnixMillis,
    },
    WorkdirAdded {
        workdir: WorkspaceInfo,
        at: UnixMillis,
    },
    /// The first event of a migrated agent whose history could not be
    /// recovered: the Claude session file it lived in is gone.
    HistoryUnavailableBefore {
        at: UnixMillis,
    },
}

impl StoryEvent {
    /// When it happened. Every event has one.
    pub fn at(&self) -> UnixMillis {
        match self {
            Self::Created { at, .. }
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

/// The story a raw event tells, if it tells one. Tool output, reasoning
/// and the provider's own bookkeeping tell nothing: they stay in the raw
/// log and are fetched on demand when a reader opens that call.
pub fn from_raw_event(event: &crate::AgentEvent<'_>, at: UnixMillis) -> Vec<StoryEvent> {
    use crate::{AgentEvent, QueuedItem, QueuedItemKind};

    match event {
        AgentEvent::Queued(QueuedItem {
            kind: QueuedItemKind::UserMessage {
                sender, content, ..
            },
            ..
        }) => {
            let text = rho_core::text_content(content);
            if text.trim().is_empty() {
                return Vec::new();
            }
            match sender {
                rho_core::MessageSender::User => vec![StoryEvent::UserMessage { text, at }],
                rho_core::MessageSender::Agent { id } => {
                    vec![StoryEvent::AgentMail {
                        from: *id,
                        text,
                        at,
                    }]
                }
            }
        }
        AgentEvent::InferenceResponse { items, .. } => from_inference_items(items, at),
        // The capped mirror event tells nothing: the Claude paths tell
        // the same message whole through `from_claude_source`.
        AgentEvent::ClaudePresentationSource { speaker, text, .. } => {
            match (speaker, text.trim().is_empty()) {
                (_, true) => Vec::new(),
                (crate::PresentationSpeaker::User, false) => vec![StoryEvent::UserMessage {
                    text: text.to_string(),
                    at,
                }],
                (
                    crate::PresentationSpeaker::Agent | crate::PresentationSpeaker::Assistant,
                    false,
                ) => {
                    vec![StoryEvent::Reply {
                        text: text.to_string(),
                        at,
                    }]
                }
            }
        }
        AgentEvent::PresentationUpdated { update } => from_presentation_update(update, at),
        AgentEvent::Created {
            role,
            runtime,
            workdirs,
            spawned_by,
            spawn_name,
            created_at,
            ..
        } => vec![StoryEvent::Created {
            role: *role,
            runtime_kind: match runtime {
                crate::db::AgentRuntime::Rho { .. } => RuntimeKind::Rho,
                crate::db::AgentRuntime::Claude { .. } => RuntimeKind::Claude,
            },
            workdirs: workdirs.clone(),
            spawned_by: *spawned_by,
            spawn_name: spawn_name.clone(),
            at: *created_at,
        }],
        AgentEvent::RoleChanged { role, .. } => vec![StoryEvent::RoleChanged { role: *role, at }],
        AgentEvent::WorkdirAdded { workdir } => vec![StoryEvent::WorkdirAdded {
            workdir: workdir.clone(),
            at,
        }],
        // A tool's answer, a delivery boundary, a cleared queue, a
        // queued compaction or tool update, and a runtime rebinding are
        // the machinery, not the story.
        AgentEvent::Queued(_)
        | AgentEvent::ToolResult { .. }
        | AgentEvent::Dequeued { .. }
        | AgentEvent::QueueCleared
        | AgentEvent::RuntimeRebound { .. } => Vec::new(),
    }
}

/// What one model response tells: the visible message as a single reply,
/// each call it made, and a compaction if it asked for one. Reasoning and
/// provider bookkeeping tell nothing.
pub fn from_inference_items(items: &[InferenceResponseItem], at: UnixMillis) -> Vec<StoryEvent> {
    let mut told = Vec::new();
    let mut reply = String::new();
    for item in items {
        match item {
            InferenceResponseItem::AssistantMessage { content, .. } => {
                let text = rho_core::text_content(content);
                if !text.trim().is_empty() {
                    if !reply.is_empty() {
                        reply.push('\n');
                    }
                    reply.push_str(&text);
                }
            }
            InferenceResponseItem::ToolCall {
                name, arguments, ..
            } => told.push(StoryEvent::ToolCall {
                name: name.clone(),
                what: tool_line(arguments),
                at,
            }),
            InferenceResponseItem::Compaction { .. } => told.push(StoryEvent::Compacted { at }),
            InferenceResponseItem::EncryptedReasoning { .. }
            | InferenceResponseItem::RawReasoning { .. }
            | InferenceResponseItem::Unknown { .. } => {}
        }
    }
    if !reply.is_empty() {
        told.insert(0, StoryEvent::Reply { text: reply, at });
    }
    told
}

/// What one mirrored Claude message tells: the person's message, or the
/// agent's reply, whole. Claude's own transcript is not ours to keep, so
/// this text is the durable copy a reader gets.
pub fn from_claude_source(
    speaker: crate::PresentationSpeaker,
    text: &str,
    at: UnixMillis,
) -> Vec<StoryEvent> {
    use crate::PresentationSpeaker;
    if text.trim().is_empty() {
        return Vec::new();
    }
    match speaker {
        PresentationSpeaker::User => vec![StoryEvent::UserMessage {
            text: text.to_owned(),
            at,
        }],
        PresentationSpeaker::Agent | PresentationSpeaker::Assistant => vec![StoryEvent::Reply {
            text: text.to_owned(),
            at,
        }],
    }
}

/// The story a sidecar proposal tells: the title it set, the activity it
/// set or cleared. `Unchanged` tells nothing.
pub fn from_presentation_update(
    update: &crate::db::AgentPresentationUpdate,
    at: UnixMillis,
) -> Vec<StoryEvent> {
    use crate::db::PresentationField;
    let mut told = Vec::new();
    if let PresentationField::Set(title) = &update.generated_title {
        told.push(StoryEvent::Titled {
            title: title.clone(),
            at,
        });
    }
    match &update.activity {
        PresentationField::Set(label) => told.push(StoryEvent::Activity {
            label: Some(label.clone()),
            at,
        }),
        PresentationField::Clear => told.push(StoryEvent::Activity { label: None, at }),
        PresentationField::Unchanged => {}
    }
    told
}

/// The one line a call shows, read out of its arguments: the first of
/// the fields a person would recognise. Never the whole argument blob,
/// which is as long as the model made it.
pub fn tool_line(arguments: &str) -> ToolLine {
    let Ok(serde_json::Value::Object(fields)) =
        serde_json::from_str::<serde_json::Value>(arguments)
    else {
        return ToolLine::Nothing;
    };
    let text = |key: &str| {
        fields
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    if let Some(path) = text("path").or_else(|| text("file_path")) {
        return ToolLine::Path(path.into());
    }
    if let Some(command) = text("command").or_else(|| text("cmd")) {
        return ToolLine::Command(cut(&command));
    }
    if let Some(query) = text("query").or_else(|| text("pattern")) {
        return ToolLine::Query(cut(&query));
    }
    ToolLine::Nothing
}

/// One line's worth, cut on a character boundary.
fn cut(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(200) {
        Some((limit, _)) => format!("{}…", &line[..limit]),
        None => line.to_owned(),
    }
}
