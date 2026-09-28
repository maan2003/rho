//! Transcripts: what a client is told of an agent's raw log. `strip`
//! takes one raw event to what a client keeps of it, per event, in that
//! event's position: the transcript is a pure function of the raw log.
//! The runtime writes its own events; this is
//! the one place they become the client's words.

use rho_agent::entry::{Block, Entry, Notice, Party, Report};
use rho_agent::inference::Carry;
use rho_agent::log::{AgentRuntime, AgentSpawnedBy, AgentUsageBucket, usage_model_of};
use rho_agent::{AgentEvent, InputKind, QueuedInput};
#[cfg(test)]
use rho_agent_types::UnixMs;
use rho_agent_types::transcript::{AStr, StreamingContextItem, ToolType};
use rho_agent_types::{ContentPart, PresentationField};
use rho_agents_client::protocol::transcript::{
    ArgumentsFormat, Item, QueuedItem, RuntimeKind, SpawnedBy, TranscriptEvent, Usage,
};

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
pub fn strip(event: &AgentEvent<'_>, prior_carry: Option<&Carry>) -> Option<TranscriptEvent> {
    Some(match event {
        AgentEvent::TitleAttempted { .. } => {
            return None;
        }
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
        AgentEvent::Entry(entry) => return strip_entry(entry, prior_carry),
    })
}

/// The Rho runtime's rows, in the words a reader already knows: a received
/// message is a message, a wake the request that carried the queue, a step
/// the model's reply, and what it sends the person a final answer.
fn strip_entry(entry: &Entry, prior_carry: Option<&Carry>) -> Option<TranscriptEvent> {
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
        Entry::RequestSent {
            report,
            compact,
            at,
            ..
        } => TranscriptEvent::NotebookReport {
            delivered: report.messages.iter().map(|id| id.0).collect(),
            acknowledged: report.acknowledged.iter().map(|id| id.0).collect(),
            calls: report_results(report, prior_carry)
                .iter()
                .map(|result| result.display_id().to_owned())
                .collect(),
            compaction: *compact,
            at: *at,
        },
        Entry::Step {
            carry, usage, at, ..
        } => TranscriptEvent::Replied {
            items: step_items(&carry.display_calls()),
            compacted: carry.has_compaction(),
            usage: usage.as_ref().map(|usage| Usage {
                model: usage.model.clone(),
                input_tokens: usage.input_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                cache_write_1h_tokens: usage.cache_write_1h_tokens,
                output_tokens: usage.output_tokens,
            }),
            context_used: usage.as_ref().and_then(|usage| {
                (usage.input_tokens > 0
                    || usage.cache_read_tokens > 0
                    || usage.cache_write_tokens > 0
                    || usage.cache_write_1h_tokens > 0)
                    .then(|| {
                        usage
                            .input_tokens
                            .saturating_add(usage.cache_read_tokens)
                            .saturating_add(usage.cache_write_tokens)
                            .saturating_add(usage.cache_write_1h_tokens)
                            .saturating_add(usage.output_tokens)
                    })
            }),
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

/// A request's rendered notebook output belongs to the prior provider call.
/// Without one, it is ordinary user context, not a tool result.
pub(crate) fn report_results(
    report: &Report,
    prior_carry: Option<&Carry>,
) -> Vec<rho_inference::transcript::ReportOutput> {
    rho_inference::transcript::report_results(report, prior_carry)
}

/// A step's visible items: the prose it wrote, then its calls.
pub(crate) fn step_items(calls: &[rho_agent::inference::Call]) -> Vec<Item> {
    calls
        .iter()
        .map(|call| Item::ToolCall {
            id: call.display_id().to_owned(),
            name: "exec".to_owned(),
            arguments: call.code.clone(),
            format: ArgumentsFormat::Text,
        })
        .collect()
}

/// What the model had said when its request failed.
fn partial_text(partial: &rho_agent_types::transcript::PendingInferenceResponse) -> String {
    use rho_agent_types::transcript::{StreamingContextItem, StreamingContextItemState};
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
    use rho_agent::entry::{MessageId, RequestNotice, ResponseUsage};

    use super::*;

    fn call_carry(id: &str, code: &str) -> Carry {
        Carry::new(
            serde_json::json!({"items":[{
                "type":"custom_tool_call","name":"exec","call_id":id,"input":code
            }]}),
            vec![rho_agent::inference::Call::new(id, code.into())],
            false,
        )
    }

    #[test]
    fn reports_deliver_exact_messages_without_claiming_task_completion() {
        let prior = call_carry("exec-9", "cell");
        let event = AgentEvent::Entry(Entry::RequestSent {
            at: UnixMs(71),
            why: rho_agent::entry::Wake::Notify,
            report: Report {
                notices: vec![RequestNotice::Restarted],
                messages: vec![MessageId(42)],
                acknowledged: vec![MessageId(11)],
                ..Default::default()
            },
            compact: true,
        });
        let projected = strip(&event, Some(&prior)).unwrap();
        assert_eq!(
            projected,
            TranscriptEvent::NotebookReport {
                calls: vec!["exec-9".into()],
                compaction: true,
                delivered: vec![42],
                acknowledged: vec![11],
                at: UnixMs(71),
            }
        );
        assert_eq!(
            report_results(
                match &event {
                    AgentEvent::Entry(Entry::RequestSent { report, .. }) => report,
                    _ => unreachable!(),
                },
                Some(&prior)
            )[0]
            .text,
            "rho restarted. Your notebook and everything running in it are gone, and their side effects may remain. Check the current state before carrying on."
        );
        assert_eq!(
            strip(&event, None).unwrap(),
            TranscriptEvent::NotebookReport {
                calls: vec![],
                compaction: true,
                delivered: vec![42],
                acknowledged: vec![11],
                at: UnixMs(71),
            }
        );
    }

    #[test]
    fn step_usage_and_interruption_preserve_call_display() {
        let carry = call_carry("exec-7", "print(3)");
        let usage = ResponseUsage {
            model: "model-a".into(),
            input_tokens: 17,
            cache_read_tokens: 5,
            cache_write_tokens: 2,
            cache_write_1h_tokens: 3,
            output_tokens: 11,
        };
        let step = |exec, usage| {
            AgentEvent::Entry(Entry::Step {
                at: UnixMs(72),
                exec,
                prose: String::new(),
                carry: carry.clone(),
                usage,
            })
        };
        assert_eq!(
            strip(&step(Some("print(3)".into()), Some(usage)), None),
            Some(TranscriptEvent::Replied {
                items: vec![Item::ToolCall {
                    id: "exec-7".into(),
                    name: "exec".into(),
                    arguments: "print(3)".into(),
                    format: ArgumentsFormat::Text,
                }],
                compacted: false,
                usage: Some(Usage {
                    model: "model-a".into(),
                    input_tokens: 17,
                    cache_read_tokens: 5,
                    cache_write_tokens: 2,
                    cache_write_1h_tokens: 3,
                    output_tokens: 11,
                }),
                context_used: Some(38),
                at: UnixMs(72),
            })
        );
        assert_eq!(
            strip(
                &AgentEvent::Entry(Entry::Step {
                    at: UnixMs(72),
                    exec: None,
                    prose: String::new(),
                    carry: Carry::new(serde_json::json!({"items":[]}), vec![], false),
                    usage: None,
                }),
                None
            ),
            Some(TranscriptEvent::Replied {
                items: vec![],
                compacted: false,
                usage: None,
                context_used: None,
                at: UnixMs(72),
            })
        );
    }

    #[test]
    fn migrated_step_displays_evicted_call_not_in_provider_replay() {
        let carry = Carry::new(
            serde_json::json!({"items":[]}),
            vec![rho_agent::inference::Call::new(
                "evicted",
                "print(4)".into(),
            )],
            false,
        );
        let event = AgentEvent::Entry(Entry::Step {
            at: UnixMs(74),
            exec: Some("print(4)".into()),
            prose: String::new(),
            carry,
            usage: None,
        });
        assert_eq!(
            strip(&event, None),
            Some(TranscriptEvent::Replied {
                items: vec![Item::ToolCall {
                    id: "evicted".into(),
                    name: "exec".into(),
                    arguments: "print(4)".into(),
                    format: ArgumentsFormat::Text,
                }],
                compacted: false,
                usage: None,
                context_used: None,
                at: UnixMs(74),
            })
        );
    }

    #[test]
    fn only_explicit_sends_are_human_messages() {
        let call = rho_agent::inference::Call::new("call", "human.send('hello')".into());
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
            strip(
                &AgentEvent::Entry(Entry::Sent {
                    at: UnixMs(5),
                    id: MessageId(7),
                    to: Party::Human,
                    text: "hello".into(),
                }),
                None
            ),
            Some(TranscriptEvent::MessageSent {
                to: None,
                text: "hello".into(),
                at: UnixMs(5)
            })
        );
        assert_eq!(
            strip(
                &AgentEvent::Transcript {
                    uuid: uuid::Uuid::nil(),
                    line: rho_agent::TranscriptLine::User {
                        text: "CLI echo".into()
                    },
                    at: UnixMs(6),
                    wake: None,
                },
                None
            ),
            None
        );
    }

    #[test]
    fn wait_is_projected() {
        assert_eq!(
            strip(
                &AgentEvent::Entry(Entry::Awaiting {
                    at: UnixMs(10),
                    since: Some(UnixMs(8)),
                }),
                None
            ),
            Some(TranscriptEvent::AwaitingHuman {
                at: UnixMs(10),
                since: Some(UnixMs(8))
            })
        );
        assert_eq!(
            strip(
                &AgentEvent::Entry(Entry::Awaiting {
                    at: UnixMs(14),
                    since: None,
                }),
                None
            ),
            Some(TranscriptEvent::AwaitingHuman {
                at: UnixMs(14),
                since: None
            })
        );
    }

    #[test]
    fn worker_failure_is_mirrored_without_panicking() {
        assert_eq!(
            strip(
                &AgentEvent::Failed {
                    partial: Default::default(),
                    error: "worker disconnected".into(),
                    retrying: false,
                    at: UnixMs(17),
                },
                None
            ),
            Some(TranscriptEvent::Failed {
                text: String::new(),
                error: "worker disconnected".into(),
                retrying: false,
                at: UnixMs(17),
            })
        );
    }
}

/// The item as a client draws it. Compaction and unknown items have no
/// face; their index is never told.
pub fn to_item(item: &StreamingContextItem) -> Option<Item> {
    Some(match item {
        StreamingContextItem::AssistantMessage { .. } => return None,
        StreamingContextItem::RawReasoning {
            content, summary, ..
        } => Item::Reasoning {
            text: reasoning_text(content, summary),
        },
        StreamingContextItem::EncryptedReasoning { summary, .. } => {
            if summary.is_empty() {
                return None;
            }
            Item::Reasoning {
                text: join(summary),
            }
        }
        StreamingContextItem::ToolCall {
            id,
            name,
            arguments,
            tool_type,
            ..
        } => Item::ToolCall {
            id: id.as_str().to_owned(),
            name: name.as_str().to_owned(),
            arguments: arguments.to_string(),
            format: crate::transcript::arguments_format(*tool_type),
        },
        StreamingContextItem::Compaction { .. } | StreamingContextItem::Unknown { .. } => {
            return None;
        }
    })
}

fn reasoning_text(content: &AStr, summary: &[AStr]) -> String {
    if summary.is_empty() {
        content.to_string()
    } else {
        join(summary)
    }
}

fn join(parts: &[AStr]) -> String {
    parts
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

/// A queued input as the wire tells it.
pub fn queued_item(input: &QueuedInput) -> QueuedItem {
    match &input.kind {
        InputKind::Message { content } => QueuedItem::Message {
            from: match input.source {
                rho_agent_types::transcript::MessageSender::User => None,
                rho_agent_types::transcript::MessageSender::Agent { id } => Some(id),
            },
            text: content
                .iter()
                .map(|part| match part {
                    ContentPart::Text { text } => text.as_str(),
                    ContentPart::Image { .. } => "[image]",
                })
                .collect::<Vec<_>>()
                .join("\n"),
            delivery: input.delivery,
        },
        InputKind::Compaction => QueuedItem::Compaction,
    }
}
