//! Transcripts: what a client is told of an agent's raw log. `strip`
//! takes one raw event to what a client keeps of it, per event, in that
//! event's position: the transcript is a pure function of the raw log.
//! The runtime writes its own events; this is
//! the one place they become the client's words.

use rho_agent::db::{AgentRuntime, AgentSpawnedBy, AgentUsageBucket, usage_model_of};
use rho_agent::entry::{Block, Entry, Notice, Party, Wake};
use rho_agent::{AgentEvent, InputKind, QueuedInput};
use rho_agent_types::PresentationField;
#[cfg(test)]
use rho_agent_types::UnixMs;
use rho_agents_client::protocol::transcript::{
    ArgumentsFormat, Item, RuntimeKind, SpawnedBy, TranscriptEvent, Usage,
};
use rho_inference::types::{MessageSender, ToolType};

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
        AgentSpawnedBy::UserOwned { by } => SpawnedBy::UserOwned { by },
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
    let message =
        |sender: &MessageSender, content: &[rho_agent_types::ContentPart], delivery, at| {
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
        AgentEvent::TitleAttempted { .. } | AgentEvent::Native(_) => return None,
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
            rho_agent::TranscriptLine::User { .. } => return None,
            rho_agent::TranscriptLine::Assistant {
                text: _,
                calls,
                usage: cost,
                context_used,
            } => TranscriptEvent::Replied {
                items: calls
                    .iter()
                    .map(|call| Item::ToolCall {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        // A transcript call is Claude's, and Claude's tools
                        // are all schema'd: its arguments are always JSON.
                        format: rho_agents_client::protocol::transcript::ArgumentsFormat::Json,
                    })
                    .collect(),
                compacted: false,
                usage: cost.as_ref().map(usage),
                context_used: *context_used,
                at: *at,
            },
            rho_agent::TranscriptLine::ToolResults { results } => TranscriptEvent::NotebookReport {
                delivered: Vec::new(),
                acknowledged: Vec::new(),
                calls: results
                    .iter()
                    .map(|result| result.call_id.as_str().to_owned())
                    .collect(),
                compaction: false,
                at: *at,
            },
            rho_agent::TranscriptLine::Compacted { context_used } => TranscriptEvent::Replied {
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
        AgentEvent::Entry(entry) => return strip_entry(entry),
    })
}

/// The Rho runtime's rows, in the words a reader already knows: a received
/// message is a message, a wake the request that carried the queue, a step
/// the model's reply, and what it sends the person a final answer.
fn strip_entry(entry: &Entry) -> Option<TranscriptEvent> {
    Some(match entry {
        Entry::Received { id, from, body, at } => TranscriptEvent::Received {
            id: id.0,
            from: match from {
                Party::Human => None,
                Party::Agent(id) => Some(*id),
            },
            text: body
                .iter()
                .filter_map(|block| match block {
                    Block::Text(text) => Some(text.as_str()),
                    Block::Image(_) => None,
                })
                .collect(),
            at: *at,
        },
        Entry::Woken {
            why,
            results,
            messages,
            acknowledged,
            at,
            ..
        } => TranscriptEvent::NotebookReport {
            delivered: messages.iter().map(|id| id.0).collect(),
            acknowledged: acknowledged.iter().map(|id| id.0).collect(),
            calls: results
                .iter()
                .map(|result| result.id.as_str().to_owned())
                .collect(),
            compaction: *why == Wake::Compaction,
            at: *at,
        },
        Entry::Step {
            calls,
            prose: _,
            carry,
            usage,
            at,
        } => TranscriptEvent::Replied {
            items: step_items(calls),
            compacted: carry.has_compaction(),
            usage: None,
            context_used: (usage.input_tokens > 0)
                .then(|| usage.input_tokens.saturating_add(usage.output_tokens)),
            at: *at,
        },
        // Cost rides on a reply of its own, which shows nothing.
        Entry::Usage { usage, at } => TranscriptEvent::Replied {
            items: Vec::new(),
            compacted: false,
            usage: Some(Usage {
                model: usage.model.clone(),
                input_tokens: usage.input_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                cache_write_1h_tokens: usage.cache_write_1h_tokens,
                output_tokens: usage.output_tokens,
            }),
            context_used: None,
            at: *at,
        },
        Entry::Sent { to, text, at, .. } => TranscriptEvent::MessageSent {
            to: match to {
                Party::Human => None,
                Party::Agent(id) => Some(*id),
            },
            text: text.clone(),
            at: *at,
        },
        Entry::Status { text, at } => TranscriptEvent::Presented {
            title: PresentationField::Unchanged,
            activity: PresentationField::Set(text.clone()),
            at: *at,
        },
        Entry::Awaiting { since, at } => TranscriptEvent::AwaitingHuman {
            since: *since,
            at: *at,
        },
        Entry::Activity {
            responding,
            running_tasks,
            checkin_at,
            archived,
            at,
        } => TranscriptEvent::NotebookActivity {
            responding: *responding,
            running_tasks: *running_tasks,
            checkin_at: *checkin_at,
            archived: *archived,
            at: *at,
        },
        Entry::Notice {
            notice: Notice::Error(error),
            at,
        } => TranscriptEvent::Failed {
            text: String::new(),
            error: error.clone(),
            retrying: true,
            at: *at,
        },
        Entry::Notice { notice, at } => TranscriptEvent::Notice {
            text: match notice {
                Notice::Restarted => "rho restarted; the notebook was lost",
                Notice::Archived => "archived",
                Notice::FreshNotebook => "started a fresh notebook",
                Notice::Error(_) => unreachable!("matched above"),
            }
            .to_owned(),
            at: *at,
        },
        Entry::CompactionTrigger { at, .. } => TranscriptEvent::CompactionRequested { at: *at },
    })
}

/// A step's visible items: the prose it wrote, then its calls.
pub(crate) fn step_items(calls: &[rho_inference::step::Call]) -> Vec<Item> {
    calls
        .iter()
        .map(|call| Item::ToolCall {
            id: call.id.as_str().to_owned(),
            name: rho_inference::step::EXEC.to_owned(),
            arguments: call.code.clone(),
            format: ArgumentsFormat::Text,
        })
        .collect()
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

/// A function tool is given JSON; a custom tool the text the model wrote.
pub(crate) fn arguments_format(tool_type: ToolType) -> ArgumentsFormat {
    match tool_type {
        ToolType::Function => ArgumentsFormat::Json,
        ToolType::Custom => ArgumentsFormat::Text,
    }
}

#[cfg(test)]
mod tests {
    use rho_agent::entry::{CallResult, MessageId};

    use super::*;

    #[test]
    fn reports_deliver_exact_messages_without_claiming_task_completion() {
        let event = AgentEvent::Entry(Entry::Woken {
            at: UnixMs(71),
            why: Wake::Notify,
            report: "still running".into(),
            images: vec![],
            messages: vec![MessageId(42)],
            acknowledged: vec![MessageId(11)],
            results: vec![CallResult {
                id: rho_inference::step::CallId::new("exec-9"),
                text: "x".repeat(10000),
                images: vec![],
            }],
        });
        let projected = strip(&event).unwrap();
        assert_eq!(
            projected,
            TranscriptEvent::NotebookReport {
                calls: vec!["exec-9".into()],
                compaction: false,
                delivered: vec![42],
                acknowledged: vec![11],
                at: UnixMs(71),
            }
        );
        assert!(senax_encoder::encode(&projected).unwrap().len() < 300);
    }

    #[test]
    fn only_explicit_sends_are_human_messages() {
        let call = rho_inference::step::Call {
            id: rho_inference::step::CallId::new("call"),
            code: "human.send('hello')".into(),
        };
        assert_eq!(
            step_items(&[call]),
            vec![Item::ToolCall {
                id: "call".into(),
                name: "exec".into(),
                arguments: "human.send('hello')".into(),
                format: ArgumentsFormat::Text,
            }]
        );
        assert_eq!(
            strip(&AgentEvent::Entry(Entry::Sent {
                at: UnixMs(5),
                id: MessageId(7),
                to: Party::Human,
                text: "hello".into(),
            })),
            Some(TranscriptEvent::MessageSent {
                to: None,
                text: "hello".into(),
                at: UnixMs(5)
            })
        );
        assert_eq!(
            strip(&AgentEvent::Transcript {
                uuid: uuid::Uuid::nil(),
                line: rho_agent::TranscriptLine::User {
                    text: "CLI echo".into()
                },
                at: UnixMs(6),
                wake: None,
            }),
            None
        );
    }

    #[test]
    fn wait_and_activity_are_independent_facts() {
        assert_eq!(
            strip(&AgentEvent::Entry(Entry::Awaiting {
                at: UnixMs(10),
                since: Some(UnixMs(8)),
            })),
            Some(TranscriptEvent::AwaitingHuman {
                at: UnixMs(10),
                since: Some(UnixMs(8))
            })
        );
        assert_eq!(
            strip(&AgentEvent::Entry(Entry::Activity {
                at: UnixMs(12),
                responding: false,
                running_tasks: 3,
                checkin_at: Some(UnixMs(300)),
                archived: false,
            })),
            Some(TranscriptEvent::NotebookActivity {
                at: UnixMs(12),
                responding: false,
                running_tasks: 3,
                checkin_at: Some(UnixMs(300)),
                archived: false,
            })
        );
        assert_eq!(
            strip(&AgentEvent::Entry(Entry::Awaiting {
                at: UnixMs(14),
                since: None,
            })),
            Some(TranscriptEvent::AwaitingHuman {
                at: UnixMs(14),
                since: None
            })
        );
    }

    #[test]
    fn worker_failure_is_mirrored_without_panicking() {
        assert_eq!(
            strip(&AgentEvent::Failed {
                partial: Default::default(),
                error: "worker disconnected".into(),
                retrying: false,
                at: UnixMs(17),
            }),
            Some(TranscriptEvent::Failed {
                text: String::new(),
                error: "worker disconnected".into(),
                retrying: false,
                at: UnixMs(17),
            })
        );
    }
}
