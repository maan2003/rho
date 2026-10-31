//! Transcripts: what a client is told of a workset's log. `strip` takes
//! one row to what a client keeps of it, in that row's position: the
//! transcript is a pure function of the log. The runtimes write their own
//! rows; this is the one place they become the client's words.

use rho_agent::db::usage_model_of;
use rho_agent::entry::{Entry, Notice, Report};
use rho_agent::inference::Carry;
use rho_agent::log::{AgentUsageBucket, TranscriptLine};
use rho_agent::record::{AgentSpec, ClaudeRow, Event, Record, Runtime};
use rho_agent_types::transcript::{AStr, StreamingContextItem, ToolType};
use rho_agent_types::{ContentPart, InputWake, UnixMs, Wpos};
use rho_agents_client::protocol::rho2::{LogEvent, SpecView};
use rho_agents_client::protocol::transcript::{ArgumentsFormat, Item, RuntimeKind, Usage};

/// An agent's spec as a reader needs it.
pub fn spec_view(spec: &AgentSpec) -> SpecView {
    let (runtime, effort) = match &spec.runtime {
        Runtime::Native { profile, .. } => (RuntimeKind::Rho, profile.effort.as_str()),
        Runtime::Claude { effort, .. } => (
            RuntimeKind::Claude,
            match effort {
                rho_claude::Effort::Low => "low",
                rho_claude::Effort::Medium => "medium",
                rho_claude::Effort::High => "high",
                rho_claude::Effort::Xhigh => "xhigh",
                rho_claude::Effort::Max => "max",
            },
        ),
    };
    SpecView {
        runtime,
        model: usage_model_of(&spec.runtime).name().to_owned(),
        effort: effort.to_owned(),
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

/// What a request's report is read against: the agent's newest step before
/// it, whose calls the report answers, and whether inputs went with it.
#[derive(Clone, Debug, Default)]
pub struct Before {
    pub carry: Option<Carry>,
    pub inputs: bool,
}

impl Before {
    /// The state after one more of the agent's visible rows.
    pub fn observe(&mut self, event: &Event<'_>) {
        match event {
            Event::Input { .. } => self.inputs = true,
            Event::Native {
                entry: Entry::Step { carry, .. },
                ..
            } => self.carry = Some(carry.clone()),
            Event::Native {
                entry: Entry::RequestSent { .. },
                ..
            }
            | Event::Claude {
                row: ClaudeRow::RequestSent { .. },
                ..
            } => self.inputs = false,
            _ => {}
        }
    }
}

/// The agent's state just before `pos`: its visible rows before it, folded.
/// A rewind before `pos` hides what it took back; one after does not.
pub fn before_at(read: &rho_db::ReadTxn, agent: rho_agent_types::AgentId, pos: Wpos) -> Before {
    use rho_agent::store::StoreRead as _;
    let Some(head) = read.agent(agent) else {
        return Before::default();
    };
    let mut kept: Vec<(Wpos, Record<'static>)> = Vec::new();
    let mut from = head.created;
    while from < pos {
        let page = read.workset_rows(head.workset, from, 4096);
        let Some((last, _)) = page.last() else {
            break;
        };
        from = last.next();
        for (row, record) in page {
            if row >= pos {
                break;
            }
            if record.event.agent() != Some(agent) {
                continue;
            }
            if let Event::Rewound { to, .. } = &record.event {
                let keep = kept.partition_point(|(kept, _)| kept < to);
                kept.truncate(keep);
            }
            kept.push((row, record));
        }
    }
    let mut before = Before::default();
    for (_, record) in &kept {
        before.observe(&record.event);
    }
    before
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
/// behind. `before` is the agent's state as of the row, for a request.
/// `None` for rows that say nothing a client uses.
pub fn strip(record: &Record<'_>, before: &Before) -> Option<LogEvent> {
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
        Event::Input {
            agent,
            mail,
            content,
            wake,
        } => {
            let text = content_text(content);
            LogEvent::Input {
                agent: *agent,
                mail: *mail,
                // A spoken request reads as what was asked, not as the
                // markup the agent is handed.
                text: match wake {
                    InputWake::Person => crate::realtime::shown_to_reader(&text).unwrap_or(text),
                    _ => text,
                },
                wake: *wake,
                at,
            }
        }
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
            text: partial_text(partial),
            error: error.to_string(),
            retrying: *retrying,
            at,
        },
        Event::ExecObserved {
            agent,
            exec,
            milestone,
        } => LogEvent::ExecObserved {
            agent: *agent,
            id: exec.as_str().to_owned(),
            milestone: *milestone,
            at,
        },
        Event::Native { agent, entry } => return strip_entry(*agent, entry, before, at),
        Event::Claude { agent, row } => return strip_claude(*agent, row, at),
    })
}

/// A notice, in the words a reader knows; `None` for one that says nothing
/// the runtime state does not.
fn strip_notice(agent: rho_agent_types::AgentId, notice: &Notice, at: UnixMs) -> Option<LogEvent> {
    match notice {
        Notice::Error(error) => Some(LogEvent::Failed {
            agent,
            text: String::new(),
            error: error.clone(),
            retrying: true,
            at,
        }),
        Notice::Stopped(error) => Some(LogEvent::Stopped {
            agent,
            error: error.clone(),
            at,
        }),
        Notice::Restarted | Notice::Archived | Notice::FreshNotebook => None,
    }
}

/// The native runtime's rows: a request the report that carried the
/// queue, a step the model's reply.
fn strip_entry(
    agent: rho_agent_types::AgentId,
    entry: &Entry,
    before: &Before,
    at: UnixMs,
) -> Option<LogEvent> {
    Some(match entry {
        Entry::RequestSent {
            report, compact, ..
        } => LogEvent::NotebookReport {
            agent,
            calls: report_results(report, before)
                .iter()
                .map(|result| result.display_id().to_owned())
                .collect(),
            compaction: *compact,
            at,
        },
        Entry::Step { carry, usage, .. } => LogEvent::Replied {
            agent,
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
        Entry::Notice { notice } => return strip_notice(agent, notice, at),
        Entry::CompactionTrigger { .. } => LogEvent::CompactionRequested { agent, at },
    })
}

/// Claude's rows, in the runtime-neutral words a reader already knows: the
/// model's line a reply, the results a report, a turn the request that
/// carried the queue.
fn strip_claude(agent: rho_agent_types::AgentId, row: &ClaudeRow, at: UnixMs) -> Option<LogEvent> {
    Some(match row {
        ClaudeRow::Transcript { line, .. } => match line {
            // What Claude was handed is the agent's `Input` rows.
            TranscriptLine::User { .. } => return None,
            TranscriptLine::Assistant {
                text: _,
                calls,
                usage: cost,
                context_used,
            } => LogEvent::Replied {
                agent,
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
            TranscriptLine::ToolResults { results } => LogEvent::NotebookReport {
                agent,
                calls: results
                    .iter()
                    .map(|result| result.call_id.as_str().to_owned())
                    .collect(),
                compaction: false,
                at,
            },
            TranscriptLine::Compacted { context_used } => LogEvent::Replied {
                agent,
                items: Vec::new(),
                compacted: true,
                usage: None,
                context_used: *context_used,
                at,
            },
        },
        ClaudeRow::RequestSent { .. } => LogEvent::NotebookReport {
            agent,
            calls: Vec::new(),
            compaction: false,
            at,
        },
        ClaudeRow::Notice(notice) => return strip_notice(agent, notice, at),
        ClaudeRow::ExecAdmitted { .. }
        | ClaudeRow::Output { .. }
        | ClaudeRow::OutputHandedOff { .. }
        | ClaudeRow::RewindPending(_) => return None,
    })
}

/// A request's rendered notebook output belongs to the prior provider call.
/// Without one, it is ordinary user context, not a tool result.
pub(crate) fn report_results(
    report: &Report,
    before: &Before,
) -> Vec<rho_inference::transcript::ReportOutput> {
    rho_inference::transcript::report_results(report, before.inputs, before.carry.as_ref())
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

