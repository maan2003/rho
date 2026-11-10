//! Transcripts: what a client is told of a workset's log. `strip` takes
//! one row to what a client keeps of it, in that row's position: the
//! transcript is a pure function of the log, one row at a time. The runtimes
//! write their own rows; this is the one place they become the client's words.

use rho_agent::entry::{Notice, compose};
use rho_agent::log::AgentUsageBucket;
use rho_agent::record::{AgentSpec, Event, Record, Runtime};
use rho_agent_types::{ContentPart, Role};
use rho_agents_client::protocol::transcript::{ArgumentsFormat, Item, RuntimeKind, Usage};
use rho_agents_client::protocol::worksets::{InputView, LogEvent, SpecView};

/// An agent's spec as a reader needs it.
pub fn spec_view(spec: &AgentSpec) -> SpecView {
    SpecView {
        runtime: match &spec.runtime {
            Runtime::Native { .. } => RuntimeKind::Rho,
            Runtime::Claude { .. } => RuntimeKind::Claude,
        },
        model: spec.runtime.model_name().to_owned(),
        effort: spec.runtime.effort_name().to_owned(),
        props: spec.props.clone(),
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

/// The text of some content: its text parts, in order.
pub fn content_text(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } => Some(text.as_str()),
            ContentPart::Image { .. } => None,
        })
        .collect()
}

/// What a client keeps of one row: the same fact with the bodies left
/// behind. `None` for rows that say nothing a client uses.
pub fn strip(record: &Record<'_>) -> Option<LogEvent> {
    let at = record.at;
    Some(match &record.event {
        Event::WorksetCreated { kind, code } => LogEvent::WorksetCreated {
            kind: kind.clone(),
            code: code.clone(),
            at,
        },
        Event::Evaluated { code } => LogEvent::Evaluated {
            code: code.clone(),
            at,
        },
        Event::WorksetPropsSet { .. } => return None,
        Event::AgentCreated { agent, spec } => LogEvent::AgentCreated {
            agent: *agent,
            spec: spec_view(spec),
            at,
        },
        Event::AgentChanged { agent, spec } => LogEvent::AgentChanged {
            agent: *agent,
            spec: spec_view(spec),
            at,
        },
        Event::Request {
            agent,
            output,
            inputs,
            notices,
            compact,
            timing,
            ..
        } => LogEvent::Request {
            agent: *agent,
            inputs: inputs
                .iter()
                .map(|input| {
                    let text = content_text(&input.content);
                    InputView {
                        message: input.message,
                        // A spoken request reads as what was asked, not as the
                        // markup the agent is handed.
                        text: match input.role {
                            Role::User => crate::realtime::shown_to_reader(&text).unwrap_or(text),
                            _ => text,
                        },
                        role: input.role,
                    }
                })
                .collect(),
            answers: !compose(true, notices, output, inputs).result.is_empty(),
            compaction: *compact,
            timing: timing.clone(),
            at,
        },
        Event::Rewound { agent, to } => LogEvent::Rewound {
            agent: *agent,
            to: *to,
            at,
        },
        Event::Failed {
            agent,
            partial,
            error,
            retrying,
        } => LogEvent::Failed {
            agent: *agent,
            text: partial.clone(),
            error: error.to_string(),
            retrying: *retrying,
            at,
        },
        Event::Notice { agent, notice } => match notice {
            Notice::Error(error) => LogEvent::Failed {
                agent: *agent,
                text: String::new(),
                error: error.clone(),
                retrying: true,
                at,
            },
            Notice::Stopped(error) => LogEvent::Stopped {
                agent: *agent,
                error: error.clone(),
                at,
            },
            // The runtime state says these.
            Notice::Archived | Notice::FreshNotebook => return None,
        },
        Event::Step {
            agent,
            carry,
            usage,
            ..
        } => LogEvent::Replied {
            agent: *agent,
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
            at,
        },
        Event::CompactionTrigger { agent, .. } => {
            LogEvent::CompactionRequested { agent: *agent, at }
        }
        Event::ClaudeReply {
            agent,
            calls,
            usage: cost,
            context_used,
            ..
        } => LogEvent::Replied {
            agent: *agent,
            items: calls
                .iter()
                .map(|call| Item::ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                    // Claude's tools are all schema'd: their arguments
                    // are always JSON.
                    format: ArgumentsFormat::Json,
                })
                .collect(),
            compacted: false,
            usage: cost.as_ref().map(usage),
            context_used: *context_used,
            at,
        },
        Event::ClaudeResults { agent, results } => LogEvent::CallsEnded {
            agent: *agent,
            calls: results
                .iter()
                .map(|result| result.call.as_str().to_owned())
                .collect(),
            at,
        },
        Event::ClaudeCompacted {
            agent,
            context_used,
        } => LogEvent::Replied {
            agent: *agent,
            items: Vec::new(),
            compacted: true,
            usage: None,
            context_used: *context_used,
            at,
        },
        Event::RewindPending { .. } => return None,
    })
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
