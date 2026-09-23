//! The raw log's transcript side: `strip`, which takes one raw event to what
//! a client keeps of it, and the journal observer that tells the daemon
//! about every append after it commits (`AGENT-LOG-DESIGN.md`).

use rho_agent_host_proto::transcript::{
    AgentPos, Item, Live, LogEntry, RuntimeKind, Seq, SpawnedBy, ToolOutcome, ToolStatus,
    TranscriptEvent, Usage,
};
use rho_agent_host_proto::{AgentId, UnixMs};
use rho_db::RhoDb;
#[cfg(test)]
use rho_inference::types::ContextBlock;
use rho_inference::types::{InferenceResponseItem, MessageSender};

use crate::db::{AgentRuntime, AgentSpawnedBy, AgentUsageBucket, usage_model_of};
use crate::{AgentEvent, InputKind, PresentationField, QueuedInput};

/// One row, the moment it became durable. `event` is `None` for the rows
/// a client is not told about (the previous loop's bookkeeping).
#[derive(Clone, Debug)]
pub struct LogAppended {
    pub seq: Seq,
    pub agent_id: AgentId,
    pub pos: AgentPos,
    pub event: Option<TranscriptEvent>,
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
pub fn strip(event: &AgentEvent<'_>) -> Option<TranscriptEvent> {
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
    if let Some(native) = event.native_event() {
        use crate::native::NativeEvent;
        return match native {
            NativeEvent::RequestStarted {
                input, context, at, ..
            } => {
                let results = input
                    .iter()
                    .flat_map(|item| match item {
                        rho_inference::types::ContextBlock::ToolResults { results } => {
                            results.iter().map(tool_outcome).collect::<Vec<_>>()
                        }
                        _ => Vec::new(),
                    })
                    .collect();
                Some(
                    if matches!(context, Some(crate::ContextChange::Preparing { .. })) {
                        TranscriptEvent::Results { results, at: *at }
                    } else {
                        TranscriptEvent::Sent {
                            results,
                            compaction: input.iter().any(|item| {
                                matches!(
                                    item,
                                    rho_inference::types::ContextBlock::CompactionTrigger
                                        | rho_inference::types::ContextBlock::ContextRotation { .. }
                                )
                            }),
                            at: *at,
                        }
                    },
                )
            }
            NativeEvent::ResponseFinished {
                output,
                context_used,
                usage: cost,
                at,
                ..
            } => Some(replied(
                &output
                    .iter()
                    .filter_map(|entry| match entry {
                        rho_inference::types::ContextBlock::InferenceResponse { items, .. } => {
                            Some(items)
                        }
                        _ => None,
                    })
                    .flatten()
                    .collect::<Vec<_>>(),
                *context_used,
                cost.as_ref().map(usage),
                *at,
            )),
            NativeEvent::RequestFailed {
                partial,
                error,
                retrying,
                at,
            } => Some(TranscriptEvent::Failed {
                text: partial_text(partial),
                error: error.clone(),
                retrying: *retrying,
                at: *at,
            }),
        };
    }
    let message =
        |sender: &MessageSender, content: &[rho_agent_host_proto::ContentPart], delivery, at| {
            TranscriptEvent::Message {
                from: match sender {
                    MessageSender::User => None,
                    MessageSender::Agent { id } => Some(*id),
                },
                text: rho_inference::types::text_content(content),
                delivery,
                at,
            }
        };
    Some(match event {
        AgentEvent::TitleAttempted { .. } => return None,
        AgentEvent::Titled { title, at } => TranscriptEvent::Presented {
            title: title
                .clone()
                .map_or(PresentationField::Clear, PresentationField::Set),
            activity: PresentationField::Unchanged,
            at: *at,
        },
        AgentEvent::ExecObserved { id, milestone, at } => TranscriptEvent::ExecObserved {
            id: id.as_str().to_owned(),
            milestone: *milestone,
            at: *at,
        },
        AgentEvent::Accepted(QueuedInput {
            source,
            kind,
            delivery,
            at,
        }) => match kind {
            InputKind::Message { content } => message(source, content, *delivery, *at),
            InputKind::Compaction => TranscriptEvent::CompactionRequested { at: *at },
        },
        AgentEvent::Native(_) => unreachable!("normalized above"),
        AgentEvent::Failed {
            partial,
            error,
            retrying,
            at,
        } => TranscriptEvent::Failed {
            text: partial_text(partial),
            error: error.to_string(),
            retrying: *retrying,
            at: *at,
        },
        AgentEvent::Cleared { at } => TranscriptEvent::QueueCleared { at: *at },
        AgentEvent::RuntimeRebound { .. }
        | AgentEvent::ClaudeExecAdmitted { .. }
        | AgentEvent::ClaudeOutput { .. }
        | AgentEvent::ClaudeOutputHandedOff { .. } => return None,
        // Claude's transcript, told in the runtime-neutral words a reader
        // already knows: a person's line is a message, the model's a
        // reply, the results a request that carried them.
        AgentEvent::Transcript { line, at, .. } => match line {
            crate::TranscriptLine::User { text } => TranscriptEvent::ClaudeMessage {
                speaker: rho_agent_host_proto::transcript::Speaker::User,
                text: text.clone(),
                at: *at,
            },
            crate::TranscriptLine::Assistant {
                text,
                calls,
                usage: cost,
                context_used,
            } => TranscriptEvent::Replied {
                items: (!text.is_empty())
                    .then(|| Item::Text {
                        text: text.clone(),
                        phase: None,
                    })
                    .into_iter()
                    .chain(calls.iter().map(|call| Item::ToolCall {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        // A transcript call is Claude's, and Claude's tools
                        // are all schema'd: its arguments are always JSON.
                        format: rho_agent_host_proto::transcript::ArgumentsFormat::Json,
                    }))
                    .collect(),
                compacted: false,
                usage: cost.as_ref().map(usage),
                context_used: *context_used,
                at: *at,
            },
            crate::TranscriptLine::ToolResults { results } => TranscriptEvent::Results {
                results: results.iter().map(tool_outcome).collect(),
                at: *at,
            },
            crate::TranscriptLine::Compacted { context_used } => TranscriptEvent::Replied {
                items: Vec::new(),
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
        } => TranscriptEvent::Created {
            role: *role,
            runtime: runtime_kind(runtime),
            place: place.clone(),
            spawned_by: spawned_by(*by),
            spawn_name: spawn_name.clone(),
            parent: *parent,
            model: usage_model_of(runtime, *binding).name().to_owned(),
            at: *created_at,
        },
        AgentEvent::RoleChanged { role, binding, at } => TranscriptEvent::RoleChanged {
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
        AgentEvent::ModeChanged { mode, at } => TranscriptEvent::ModeChanged {
            mode: *mode,
            at: *at,
        },
        AgentEvent::Notice { text, .. } if text.is_empty() => return None,
        AgentEvent::Notice { text, at } => TranscriptEvent::Notice {
            text: text.to_string(),
            at: *at,
        },
        AgentEvent::Turn { edge, at } => TranscriptEvent::Turn {
            edge: edge.clone(),
            at: *at,
        },
        AgentEvent::Wants { want, summary, at } => TranscriptEvent::Wants {
            want: *want,
            summary: summary.clone(),
            at: *at,
        },
        AgentEvent::Rewound { to, at } => TranscriptEvent::Rewound {
            to: (*to).into(),
            at: *at,
        },
    })
}

fn tool_outcome(result: &rho_inference::types::ToolResult) -> ToolOutcome {
    ToolOutcome {
        id: result.call_id.as_str().to_owned(),
        status: match result.body.status {
            rho_agent_host_proto::ToolOutputStatus::Success => ToolStatus::Success,
            rho_agent_host_proto::ToolOutputStatus::Error => ToolStatus::Error,
            rho_agent_host_proto::ToolOutputStatus::Cancelled => ToolStatus::Cancelled,
        },
        started_at: result.started_at,
        finished_at: result.finished_at,
    }
}

/// What the model had said when its request failed.
fn partial_text(partial: &rho_inference::types::PendingInferenceResponse) -> String {
    use rho_inference::types::{StreamingContextItem, StreamingContextItemState};
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
) -> TranscriptEvent {
    TranscriptEvent::Replied {
        items: items.iter().filter_map(|value| item(value)).collect(),
        compacted: items
            .iter()
            .any(|value| matches!(value, InferenceResponseItem::Compaction { .. })),
        usage,
        context_used,
        at,
    }
}

/// A durable response item uses the same display vocabulary as streaming.
pub fn item(value: &InferenceResponseItem) -> Option<Item> {
    Some(match value {
        InferenceResponseItem::AssistantMessage { content, phase, .. } => Item::Text {
            text: rho_inference::types::text_content(content),
            phase: phase.map(crate::live::text_phase),
        },
        InferenceResponseItem::RawReasoning {
            content, summary, ..
        } => Item::Reasoning {
            text: if summary.is_empty() {
                content.clone()
            } else {
                summary.join("\n")
            },
        },
        InferenceResponseItem::EncryptedReasoning { summary, .. } => {
            if summary.is_empty() {
                return None;
            }
            Item::Reasoning {
                text: summary.join("\n"),
            }
        }
        InferenceResponseItem::ToolCall {
            id,
            name,
            arguments,
            tool_type,
            ..
        } => Item::ToolCall {
            id: id.as_str().to_owned(),
            name: name.as_str().to_owned(),
            arguments: arguments.clone(),
            format: (*tool_type).into(),
        },
        InferenceResponseItem::Compaction { .. } | InferenceResponseItem::Unknown { .. } => {
            return None;
        }
    })
}

fn is_compaction_summary(text: &str) -> bool {
    text.trim_start()
        .starts_with("This session is being continued from a previous conversation")
}

#[cfg(test)]
mod tests {

    use rho_agent_host_proto::{ContentPart, MessageDelivery, ToolOutputStatus};
    use rho_inference::types::{ToolOutput, ToolResult, ToolUpdate};

    use super::*;

    #[test]
    fn responses_preserve_item_order_and_message_phase() {
        let data = || {
            Box::new(rho_inference::OpenAiResponsesProviderData::Message {
                item_id: "visible".try_into().unwrap(),
            })
        };
        let items = vec![
            InferenceResponseItem::AssistantMessage {
                provider_specific: data(),
                content: vec![ContentPart::Text {
                    text: "before".into(),
                }],
                phase: Some(rho_agent_host_proto::MessagePhase::Commentary),
            },
            InferenceResponseItem::ToolCall {
                provider_specific: data(),
                id: "middle".try_into().unwrap(),
                name: "exec".try_into().unwrap(),
                tool_type: rho_inference::types::ToolType::Custom,
                arguments: "print(42)".into(),
            },
            InferenceResponseItem::AssistantMessage {
                provider_specific: data(),
                content: vec![ContentPart::Text {
                    text: "after".into(),
                }],
                phase: Some(rho_agent_host_proto::MessagePhase::FinalAnswer),
            },
            InferenceResponseItem::Unknown {
                provider_specific: data(),
            },
        ];
        let event = replied(
            &items.iter().collect::<Vec<_>>(),
            Some(71),
            None,
            UnixMs(23),
        );
        assert_eq!(
            event,
            TranscriptEvent::Replied {
                items: vec![
                    Item::Text {
                        text: "before".into(),
                        phase: Some(rho_agent_host_proto::transcript::TextPhase::Commentary)
                    },
                    Item::ToolCall {
                        id: "middle".into(),
                        name: "exec".into(),
                        arguments: "print(42)".into(),
                        format: rho_agent_host_proto::transcript::ArgumentsFormat::Text,
                    },
                    Item::Text {
                        text: "after".into(),
                        phase: Some(rho_agent_host_proto::transcript::TextPhase::FinalAnswer)
                    },
                ],
                compacted: false,
                usage: None,
                context_used: Some(71),
                at: UnixMs(23),
            }
        );
    }

    #[test]
    fn worker_failure_is_mirrored_without_panicking() {
        let event = AgentEvent::Failed {
            partial: Default::default(),
            error: "agent service connection closed".into(),
            retrying: false,
            at: UnixMs(17),
        };
        assert_eq!(
            strip(&event),
            Some(TranscriptEvent::Failed {
                text: String::new(),
                error: "agent service connection closed".into(),
                retrying: false,
                at: UnixMs(17),
            })
        );
    }

    #[test]
    fn a_sent_keeps_statuses_and_leaves_output_behind() {
        let event = AgentEvent::Native(crate::native::NativeEvent::RequestStarted {
            input: Vec::from(vec![ContextBlock::ToolResults {
                results: vec![ToolResult {
                    call_id: "call-1".try_into().unwrap(),
                    tool_type: rho_inference::types::ToolType::Function,
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
            context: None,
        });
        let stripped = strip(&event).unwrap();
        assert_eq!(
            stripped,
            TranscriptEvent::Sent {
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
    fn a_later_report_does_not_rewrite_the_first_results_status_or_duration() {
        let event = AgentEvent::Native(crate::native::NativeEvent::RequestStarted {
            input: Vec::from(vec![ContextBlock::ToolUpdate(ToolUpdate {
                status: None,
                images: Default::default(),
                call_id: "call-1".try_into().unwrap(),
                tool_type: rho_inference::types::ToolType::Custom,
                output: std::sync::Arc::new("bounded".to_owned()),
                full_output: Some(std::sync::Arc::new("complete".to_owned())),
                at: UnixMs(2),
            })]),
            at: UnixMs(3),
            wake: None,
            context: None,
        });

        assert_eq!(
            strip(&event),
            Some(TranscriptEvent::Sent {
                results: vec![],
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
            Some(TranscriptEvent::Message {
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
}
