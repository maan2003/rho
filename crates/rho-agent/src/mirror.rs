//! The raw log's mirror side: `strip`, which takes one raw event to what
//! a client keeps of it, and the journal observer that tells the daemon
//! about every append after it commits (`AGENT-LOG-DESIGN.md`).

use rho_core::{AgentId, ContextBlock, InferenceResponseItem, MessageSender, UnixMs};
use rho_db::RhoDb;
use rho_ui_proto::mirror::{
    AgentPos, Live, LogEntry, MirrorEvent, RuntimeKind, Seq, SpawnedBy, ToolCallLine, ToolLine,
    ToolOutcome, ToolStatus, Usage,
};

use crate::db::{AgentRuntime, AgentSpawnedBy, AgentUsageBucket, usage_model_of};
use crate::{AgentEvent, InputKind, QueuedInput};

/// One row, the moment it became durable. `event` is `None` for the rows
/// a client is not told about (the previous loop's bookkeeping).
#[derive(Clone, Debug)]
pub struct LogAppended {
    pub seq: Seq,
    pub agent_id: AgentId,
    pub pos: AgentPos,
    pub event: Option<MirrorEvent>,
}

impl LogAppended {
    /// The wire's view of this append, if it has one.
    pub fn entry(&self) -> Option<LogEntry> {
        Some(LogEntry {
            seq: self.seq,
            agent_id: self.agent_id,
            pos: self.pos,
            event: self.event.clone()?,
        })
    }
}

/// One thing a connection forwards, in the order it happened: a row
/// became durable, or a loop said what its tail is now. One feed for
/// both is what makes the ordering rule hold: a loop writes its row,
/// the commit hook sends `Appended`, and only then does the same task
/// send its `Live`.
#[derive(Clone, Debug)]
pub enum Feed {
    Appended(LogAppended),
    Live { agent_id: AgentId, live: Live },
}

/// The database's journal observer. Held in the database's own observer
/// slot rather than passed down, because rows are appended from a dozen
/// places and none of them should have to carry a channel.
pub struct Journal {
    feed: tokio::sync::broadcast::Sender<Feed>,
}

/// Every row appended to this database from now on, and every live
/// delta any loop tells. A slow reader lags rather than blocking the
/// writer; a lagged reader catches up from the journal table, which is
/// the truth, and asks the loops to tell their tails again.
pub fn feed(db: &RhoDb) -> tokio::sync::broadcast::Receiver<Feed> {
    db.observer(Journal::new).feed.subscribe()
}

/// A loop saying what its tail is now.
pub fn tell_live(db: &RhoDb, agent_id: AgentId, live: Live) {
    let _ = db
        .observer(Journal::new)
        .feed
        .send(Feed::Live { agent_id, live });
}

impl Journal {
    fn new() -> Self {
        Self {
            feed: tokio::sync::broadcast::Sender::new(4096),
        }
    }

    pub(crate) fn sender(&self) -> tokio::sync::broadcast::Sender<Feed> {
        self.feed.clone()
    }
}

pub fn runtime_kind(runtime: &AgentRuntime) -> RuntimeKind {
    match runtime {
        AgentRuntime::Rho { .. } => RuntimeKind::Rho,
        AgentRuntime::Claude { .. } => RuntimeKind::Claude,
    }
}

pub fn spawned_by(spawned_by: AgentSpawnedBy) -> SpawnedBy {
    match spawned_by {
        AgentSpawnedBy::Direct => SpawnedBy::Direct,
        AgentSpawnedBy::Engineer => SpawnedBy::Engineer,
    }
}

fn usage(bucket: &AgentUsageBucket) -> Usage {
    Usage {
        model: bucket.model.name().to_owned(),
        input_tokens: bucket.input_tokens,
        cache_read_tokens: bucket.cache_read_tokens,
        cache_write_tokens: bucket.cache_write_tokens,
        cache_write_1h_tokens: bucket.cache_write_1h_tokens,
        output_tokens: bucket.output_tokens,
    }
}

/// What a client keeps of one raw event: the same fact with the bodies
/// left behind. Pure, per event; the position is the raw event's own.
/// `None` for rows that say nothing a client uses.
pub fn strip(event: &AgentEvent<'_>) -> Option<MirrorEvent> {
    // Rows copied before the file's `isCompactSummary` flag was read hold
    // Claude's post-compaction summary as a user line. The log is never
    // rewritten, so the reader is the one that leaves them out.
    if let AgentEvent::Transcript {
        line: crate::TranscriptLine::User { text },
        ..
    } = event
        && is_compaction_summary(text)
    {
        return None;
    }
    let message = |sender: &MessageSender, content: &[rho_core::ContentPart], delivery, at| {
        MirrorEvent::Message {
            from: match sender {
                MessageSender::User => None,
                MessageSender::Agent { id } => Some(*id),
            },
            text: rho_core::text_content(content),
            delivery,
            at,
        }
    };
    Some(match event {
        AgentEvent::Accepted(QueuedInput {
            source,
            kind,
            delivery,
            at,
        }) => match kind {
            InputKind::Message { content } => message(source, content, *delivery, *at),
            InputKind::Compaction => MirrorEvent::CompactionRequested { at: *at },
        },
        AgentEvent::Sent { blocks, at, .. } => MirrorEvent::Sent {
            results: blocks
                .iter()
                .flat_map(|block| match block {
                    ContextBlock::ToolResults { results } => {
                        results.iter().map(tool_outcome).collect::<Vec<_>>()
                    }
                    ContextBlock::ToolUpdate(update) => vec![ToolOutcome {
                        id: update.call_id.as_str().to_owned(),
                        status: ToolStatus::Success,
                        started_at: update.at,
                        finished_at: update.at,
                    }],
                    _ => Vec::new(),
                })
                .collect(),
            compaction: blocks
                .iter()
                .any(|block| matches!(block, ContextBlock::CompactionTrigger)),
            at: *at,
        },
        AgentEvent::Replied {
            blocks,
            context_used,
            usage: cost,
            at,
        } => {
            let items = blocks
                .iter()
                .flat_map(|block| match block {
                    ContextBlock::InferenceResponse { items, .. } => items.as_slice(),
                    _ => &[],
                })
                .collect::<Vec<_>>();
            replied(&items, *context_used, cost.as_ref().map(usage), *at)
        }
        AgentEvent::Failed {
            partial,
            error,
            retrying,
            at,
        } => MirrorEvent::Failed {
            text: partial_text(partial),
            error: error.to_string(),
            retrying: *retrying,
            at: *at,
        },
        AgentEvent::QueueCleared => MirrorEvent::QueueCleared { at: UnixMs(0) },
        AgentEvent::Cleared { at } => MirrorEvent::QueueCleared { at: *at },
        AgentEvent::RuntimeRebound { .. } | AgentEvent::PythonStream { .. } => return None,
        AgentEvent::ClaudePresentationSource {
            speaker, text, at, ..
        } => MirrorEvent::ClaudeMessage {
            speaker: match speaker {
                crate::PresentationSpeaker::User => rho_ui_proto::mirror::Speaker::User,
                crate::PresentationSpeaker::Agent => rho_ui_proto::mirror::Speaker::Agent,
                crate::PresentationSpeaker::Assistant => rho_ui_proto::mirror::Speaker::Assistant,
            },
            text: text.to_string(),
            at: *at,
        },
        // Claude's transcript, told in the runtime-neutral words a reader
        // already knows: a person's line is a message, the model's a
        // reply, the results a request that carried them.
        AgentEvent::Transcript { line, at, .. } => match line {
            crate::TranscriptLine::User { text } => MirrorEvent::ClaudeMessage {
                speaker: rho_ui_proto::mirror::Speaker::User,
                text: text.clone(),
                at: *at,
            },
            crate::TranscriptLine::Assistant {
                text,
                calls,
                usage: cost,
                context_used,
            } => MirrorEvent::Replied {
                text: text.clone(),
                calls: calls
                    .iter()
                    .map(|call| ToolCallLine {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        what: tool_line(&call.arguments),
                        arguments: call.arguments.clone(),
                    })
                    .collect(),
                compacted: false,
                usage: cost.as_ref().map(usage),
                context_used: *context_used,
                at: *at,
            },
            crate::TranscriptLine::ToolResults { results } => MirrorEvent::Results {
                results: results.iter().map(tool_outcome).collect(),
                at: *at,
            },
            crate::TranscriptLine::Compacted { context_used } => MirrorEvent::Replied {
                text: String::new(),
                calls: Vec::new(),
                compacted: true,
                usage: None,
                context_used: *context_used,
                at: *at,
            },
        },
        AgentEvent::Created {
            role,
            binding,
            runtime,
            place,
            spawned_by: by,
            spawn_name,
            created_at,
            parent,
        } => MirrorEvent::Created {
            role: *role,
            runtime: runtime_kind(runtime),
            place: place.clone(),
            spawned_by: spawned_by(*by),
            spawn_name: spawn_name.clone(),
            parent: *parent,
            model: usage_model_of(runtime, *binding).name().to_owned(),
            at: *created_at,
        },
        AgentEvent::RoleChanged { role, binding, at } => MirrorEvent::RoleChanged {
            role: *role,
            // The runtime does not change with a role, so either kind
            // names the model the same way.
            model: binding.map(|binding| {
                usage_model_of(
                    &AgentRuntime::Rho {
                        prompt_cache_key: rho_inference::PromptCacheKey::generate(),
                    },
                    binding,
                )
                .name()
                .to_owned()
            }),
            at: *at,
        },
        AgentEvent::WorkdirAdded { at } => MirrorEvent::Notice {
            text: String::new(),
            at: *at,
        },
        AgentEvent::Notice { text, at } => MirrorEvent::Notice {
            text: text.to_string(),
            at: *at,
        },
        AgentEvent::Turn { edge, at } => MirrorEvent::Turn {
            edge: edge.clone(),
            at: *at,
        },
        AgentEvent::Presented {
            title,
            activity,
            at,
        } => MirrorEvent::Presented {
            title: title.clone(),
            activity: activity.clone(),
            at: *at,
        },
        AgentEvent::Wants { want, summary, at } => MirrorEvent::Wants {
            want: *want,
            summary: summary.clone(),
            at: *at,
        },
        AgentEvent::Rewound { to, at } => MirrorEvent::Rewound {
            to: (*to).into(),
            at: *at,
        },
    })
}

fn tool_outcome(result: &rho_core::ToolResult) -> ToolOutcome {
    ToolOutcome {
        id: result.call_id.as_str().to_owned(),
        status: match result.body.status {
            rho_core::ToolOutputStatus::Success => ToolStatus::Success,
            rho_core::ToolOutputStatus::Error => ToolStatus::Error,
            rho_core::ToolOutputStatus::Cancelled => ToolStatus::Cancelled,
        },
        started_at: result.started_at,
        finished_at: result.finished_at,
    }
}

/// One response: the visible text joined, each call as a line, whether
/// it compacted. Reasoning and provider bookkeeping say nothing.
/// What the model had said when its request failed.
fn partial_text(partial: &rho_core::PendingInferenceResponse) -> String {
    use rho_core::{StreamingContextItem, StreamingContextItemState};
    let mut text = String::new();
    for slot in &partial.items {
        let (StreamingContextItemState::Pending(item) | StreamingContextItemState::Finished(item)) =
            slot
        else {
            continue;
        };
        if let StreamingContextItem::AssistantMessage { content, .. } = item {
            let part = content.iter().map(ToString::to_string).collect::<String>();
            if !part.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&part);
            }
        }
    }
    text
}

fn replied(
    items: &[&InferenceResponseItem],
    context_used: Option<u64>,
    usage: Option<Usage>,
    at: UnixMs,
) -> MirrorEvent {
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut compacted = false;
    for item in items {
        match item {
            InferenceResponseItem::AssistantMessage { content, .. } => {
                let part = rho_core::text_content(content);
                if !part.trim().is_empty() {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(&part);
                }
            }
            InferenceResponseItem::ToolCall {
                id,
                name,
                arguments,
                ..
            } => calls.push(ToolCallLine {
                id: id.as_str().to_owned(),
                name: name.as_str().to_owned(),
                what: tool_line(arguments),
                arguments: arguments.clone(),
            }),
            InferenceResponseItem::Compaction { .. } => compacted = true,
            InferenceResponseItem::EncryptedReasoning { .. }
            | InferenceResponseItem::RawReasoning { .. }
            | InferenceResponseItem::Unknown { .. } => {}
        }
    }
    MirrorEvent::Replied {
        text,
        calls,
        compacted,
        usage,
        context_used,
        at,
    }
}

/// What a call shows next to its name, read out of its arguments: the
/// first of the fields a person would recognise, whole. The arguments
/// themselves travel beside it, so a call whose arguments no field of this
/// can name still draws what the model sent.
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
        return ToolLine::Command(command);
    }
    if let Some(query) = text("query").or_else(|| text("pattern")) {
        return ToolLine::Query(query);
    }
    if let Some(agent) = text("agent_id").or_else(|| text("engineer_id")) {
        return ToolLine::Query(agent);
    }
    ToolLine::Nothing
}

/// The opening Claude Code gives the summary it writes after compacting.
fn is_compaction_summary(text: &str) -> bool {
    text.trim_start()
        .starts_with("This session is being continued from a previous conversation")
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use rho_core::{
        ContentPart, MessageDelivery, ToolOutput, ToolOutputStatus, ToolResult, ToolUpdate,
    };

    use super::*;

    #[test]
    fn a_sent_keeps_statuses_and_leaves_output_behind() {
        let event = AgentEvent::Sent {
            blocks: Cow::Owned(vec![ContextBlock::ToolResults {
                results: vec![ToolResult {
                    call_id: "call-1".try_into().unwrap(),
                    tool_type: rho_core::ToolType::Function,
                    body: ToolOutput {
                        output: std::sync::Arc::new("x".repeat(10_000)),
                        full_output: None,
                        images: std::sync::Arc::new(Vec::new()),
                        status: ToolOutputStatus::Error,
                    },
                    started_at: UnixMs(1),
                    finished_at: UnixMs(2),
                    metadata: None,
                }],
            }]),
            at: UnixMs(3),
            wake: None,
        };
        let stripped = strip(&event).unwrap();
        assert_eq!(
            stripped,
            MirrorEvent::Sent {
                results: vec![ToolOutcome {
                    id: "call-1".into(),
                    status: ToolStatus::Error,
                    started_at: UnixMs(1),
                    finished_at: UnixMs(2),
                }],
                compaction: false,
                at: UnixMs(3),
            }
        );
        assert!(senax_encoder::encode(&stripped).unwrap().len() < 200);
    }

    #[test]
    fn a_sent_update_is_a_result_whose_detail_can_be_requested() {
        let event = AgentEvent::Sent {
            blocks: Cow::Owned(vec![ContextBlock::ToolUpdate(ToolUpdate {
                call_id: "call-1".try_into().unwrap(),
                tool_type: rho_core::ToolType::Custom,
                output: std::sync::Arc::new("bounded".to_owned()),
                full_output: Some(std::sync::Arc::new("complete".to_owned())),
                at: UnixMs(2),
            })]),
            at: UnixMs(3),
            wake: None,
        };

        assert_eq!(
            strip(&event),
            Some(MirrorEvent::Sent {
                results: vec![ToolOutcome {
                    id: "call-1".into(),
                    status: ToolStatus::Success,
                    started_at: UnixMs(2),
                    finished_at: UnixMs(2),
                }],
                compaction: false,
                at: UnixMs(3),
            })
        );
    }

    #[test]
    fn a_message_names_its_sender() {
        let event = AgentEvent::Accepted(QueuedInput {
            source: MessageSender::User,
            kind: InputKind::Message {
                content: vec![ContentPart::Text { text: "hi".into() }],
            },
            delivery: MessageDelivery::Immediate,
            at: UnixMs(9),
        });
        assert_eq!(
            strip(&event),
            Some(MirrorEvent::Message {
                from: None,
                text: "hi".into(),
                delivery: MessageDelivery::Immediate,
                at: UnixMs(9),
            })
        );
    }

    #[test]
    fn bookkeeping_rows_say_nothing() {
        assert_eq!(
            strip(&AgentEvent::RuntimeRebound {
                change: crate::RuntimeChange::ClaudeRewindPending(None),
                at: UnixMs(0),
            }),
            None
        );
    }

    #[test]
    fn an_older_rows_compaction_summary_is_not_told() {
        let summary = AgentEvent::Transcript {
            uuid: uuid::Uuid::nil(),
            line: crate::TranscriptLine::User {
                text: "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion.".to_owned(),
            },
            at: UnixMs(1),
            wake: None,
        };
        assert_eq!(strip(&summary), None);
        let spoken = AgentEvent::Transcript {
            uuid: uuid::Uuid::nil(),
            line: crate::TranscriptLine::User {
                text: "This session is fine".to_owned(),
            },
            at: UnixMs(1),
            wake: None,
        };
        assert!(strip(&spoken).is_some());
    }

    #[test]
    fn tool_lines_read_the_recognisable_field() {
        assert_eq!(
            tool_line(r#"{"command":"ls -la\nmore"}"#),
            ToolLine::Command("ls -la\nmore".into()),
            "the whole command, every line"
        );
        assert_eq!(
            tool_line(r#"{"file_path":"/tmp/a"}"#),
            ToolLine::Path("/tmp/a".into())
        );
        assert_eq!(tool_line("not json"), ToolLine::Nothing);
    }
}
