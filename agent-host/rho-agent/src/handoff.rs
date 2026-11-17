//! A runtime switch: the history one runtime hands the other
//! ([SPEC-runtime-switch](../specs/SPEC-runtime-switch.md)).
//!
//! Both runtimes already leave a provider-neutral record of each turn in the
//! log: what every `Request` carried, with its roles, and what the model said
//! and ran (`ClaudeReply` rows, or a native `Step`'s prose and exec). A switch
//! rebuilds the new runtime's history from those rows. What only one provider
//! can read stays behind: Claude's thinking and signatures, and OpenAI's
//! encrypted reasoning and compactions.

use std::collections::HashMap;

use rho_agent_types::{ContentPart, Role, Wpos};
use serde_json::{Value, json};

use crate::entry::{RequestNotice, add_output, compose};
use crate::inference::{Call, Carry};
use crate::record::{Event, Input, Record, Runtime};

/// How much history a switch hands over, in bytes of text: about 120K
/// tokens. That is half of what a native agent compacts at, and well inside
/// Claude's window, so the new runtime has room to work before it compacts.
pub const HANDOFF_BYTES: usize = 480_000;

/// The most one output or input keeps; the rest of a longer one is cut from
/// its middle, so one huge output cannot take the whole handoff.
const PART_BYTES: usize = 40_000;

/// The agent's rows since its runtime kind last changed: the current
/// runtime's own history. The row that changed it is not included.
pub fn era<'a>(rows: &'a [(Wpos, Record<'static>)]) -> &'a [(Wpos, Record<'static>)] {
    let mut kind = None;
    let mut start = 0;
    for (index, (_, record)) in rows.iter().enumerate() {
        let spec = match &record.event {
            Event::AgentCreated { spec, .. } | Event::AgentChanged { spec, .. } => spec,
            _ => continue,
        };
        let now = std::mem::discriminant(&spec.runtime);
        if kind.is_some_and(|kind| kind != now) {
            start = index + 1;
        }
        kind = Some(now);
    }
    &rows[start..]
}

/// The name a runtime goes by in the handoff's opening line.
pub fn runtime_label(runtime: &Runtime) -> String {
    match runtime {
        Runtime::Native { model, .. } => format!("{} in Rho's native runtime", model.as_str()),
        Runtime::Claude { model, .. } => format!("Claude Code ({})", runtime_model(*model)),
    }
}

fn runtime_model(model: rho_claude::Model) -> &'static str {
    match model {
        rho_claude::Model::Fable => "Fable",
        rho_claude::Model::Opus => "Opus",
        rho_claude::Model::Sonnet => "Sonnet",
        rho_claude::Model::Haiku => "Haiku",
    }
}

// -- Claude to native --------------------------------------------------------

/// The native rows a Claude agent's history becomes, to follow the
/// `AgentChanged` that switches it. `rows` is the Claude era ([`era`]).
///
/// Only the turns from the one Claude last compacted in are replayed;
/// `summary`, Claude's own account of what came before, opens the history in
/// Rho's `system` voice, the way `hooks.on_compaction` text does. Every reply
/// becomes a `Step`: its text as prose and its exec call, if any, as the
/// step's call, in a carry `transcribe` makes for the provider. Thinking is
/// dropped. Claude Code's own tools, which a native agent lacks, are told as
/// prose. Every `Request` is kept with its roles. The oldest turns go when the
/// whole is larger than `budget` bytes.
pub fn claude_to_native(
    agent: rho_agent_types::AgentId,
    rows: &[(Wpos, Record<'static>)],
    summary: Option<&str>,
    from: &str,
    to: &str,
    budget: usize,
    transcribe: &dyn Fn(&str, Option<Call>) -> Carry,
) -> Vec<Event<'static>> {
    // Claude keeps the turn it compacted in, so the replay starts there.
    let since = match rows
        .iter()
        .rposition(|(_, record)| matches!(record.event, Event::ClaudeCompacted { .. }))
    {
        None => 0,
        Some(compacted) => claude_turns(rows)
            .into_iter()
            .take_while(|&start| start < compacted)
            .last()
            .unwrap_or(compacted + 1),
    };
    let mut out = Vec::new();
    let mut prose = String::new();
    // The exec call the newest step left for a request to answer.
    let mut open: Option<String> = None;
    let mut tools: HashMap<String, String> = HashMap::new();
    let step = |prose: &mut String, call: Option<Call>| Event::Step {
        agent,
        exec: call.as_ref().map(|call| call.code.clone()),
        carry: transcribe(prose, call),
        prose: std::mem::take(prose),
        usage: None,
    };
    for (_, record) in &rows[since..] {
        match &record.event {
            Event::Request {
                output,
                inputs,
                notices,
                timing,
                ..
            } => {
                // A turn's last words, with no call after them, end it.
                if open.is_none() && !prose.trim().is_empty() {
                    out.push(step(&mut prose, None));
                }
                open = None;
                out.push(Event::Request {
                    agent,
                    output: cut_content(output),
                    inputs: inputs
                        .iter()
                        .map(|input| Input {
                            content: cut_content(&input.content),
                            ..input.clone()
                        })
                        .collect(),
                    notices: notices.clone(),
                    compact: false,
                    turn: None,
                    timing: timing.clone(),
                });
            }
            Event::ClaudeReply { text, calls, .. } => {
                say(&mut prose, text);
                for call in calls {
                    if rho_claude::mcp::names_exec(&call.name) && open.is_none() {
                        let code = exec_source(&call.arguments);
                        open = Some(call.id.clone());
                        out.push(step(&mut prose, Some(Call::new(call.id.clone(), code))));
                    } else {
                        tools.insert(call.id.clone(), call.name.clone());
                        say(
                            &mut prose,
                            &format!(
                                "[I called Claude Code's `{}` tool with {}]",
                                call.name,
                                cut(&call.arguments)
                            ),
                        );
                    }
                }
            }
            Event::ClaudeResults { results, .. } => {
                for result in results {
                    if open.as_deref() == Some(result.call.as_str()) {
                        open = None;
                        out.push(Event::Request {
                            agent,
                            output: vec![ContentPart::Text {
                                text: cut(&result.output),
                            }],
                            inputs: Vec::new(),
                            notices: Vec::new(),
                            compact: false,
                            turn: None,
                            timing: None,
                        });
                    } else {
                        let name = tools
                            .get(result.call.as_str())
                            .map_or("exec", String::as_str);
                        say(
                            &mut prose,
                            &format!("[`{name}` returned: {}]", cut(&result.output)),
                        );
                    }
                }
            }
            _ => {}
        }
    }
    if open.is_none() && !prose.trim().is_empty() {
        out.push(step(&mut prose, None));
    }
    let (kept, left_out) = fit(out, budget, |events, at| {
        // A turn starts at a request no call waits on.
        matches!(events[at], Event::Request { .. })
            && (at == 0
                || matches!(&events[at - 1], Event::Step { carry, .. } if !carry.has_call()))
    });
    let mut opening = format!(
        "This agent ran on {from} until now and runs on {to} from here. Rho rebuilt this \
         history from its log: Claude's thinking did not carry over."
    );
    match summary {
        Some(summary) => opening.push_str(&format!(
            " When Claude Code last compacted the conversation, it summarized what came \
             before like this:\n\n{summary}\n\nThe turns from the one it compacted in follow"
        )),
        None => opening.push_str(" The turns follow"),
    }
    opening.push_str(&left_out_note(left_out));
    opening.push_str(
        ". Claude Code's own tools are gone; `exec` works as before. The notebook \
         restarted with the switch, so its globals and running cells are gone.",
    );
    let mut history = vec![Event::Request {
        agent,
        output: Vec::new(),
        inputs: vec![Input {
            role: Role::System,
            message: None,
            content: vec![ContentPart::Text { text: opening }],
        }],
        notices: Vec::new(),
        compact: false,
        turn: None,
        timing: None,
    }];
    history.extend(kept);
    history
}

/// Where the turns of Claude rows start: at each request no exec call waits
/// on.
fn claude_turns(rows: &[(Wpos, Record<'static>)]) -> Vec<usize> {
    let mut open = false;
    let mut starts = Vec::new();
    for (at, (_, record)) in rows.iter().enumerate() {
        match &record.event {
            Event::Request { .. } => {
                if !open {
                    starts.push(at);
                }
                open = false;
            }
            Event::ClaudeReply { calls, .. } => {
                open |= calls
                    .iter()
                    .any(|call| rho_claude::mcp::names_exec(&call.name));
            }
            _ => {}
        }
    }
    starts
}

fn left_out_note(left_out: usize) -> String {
    match left_out {
        0 => String::new(),
        1 => ", except the oldest turn, left out to fit".into(),
        n => format!(", except the {n} oldest turns, left out to fit"),
    }
}

/// The code of an exec call, from Claude's JSON arguments.
fn exec_source(arguments: &str) -> String {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .and_then(|value| value.get("source")?.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn say(prose: &mut String, text: &str) {
    if text.trim().is_empty() {
        return;
    }
    if !prose.is_empty() {
        prose.push_str("\n\n");
    }
    prose.push_str(text);
}

fn cut(text: &str) -> String {
    if text.len() <= PART_BYTES {
        return text.to_owned();
    }
    let mut head = PART_BYTES / 2;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - PART_BYTES / 2;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n… {} bytes cut by the runtime switch …\n{}",
        &text[..head],
        tail - head,
        &text[tail..]
    )
}

fn cut_content(content: &[ContentPart]) -> Vec<ContentPart> {
    content
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => ContentPart::Text { text: cut(text) },
            image => image.clone(),
        })
        .collect()
}

/// The newest whole turns of `items` that fit `budget`, and how many turns
/// were left out. `starts(items, at)` says whether a turn starts at `at`. The
/// newest turn is kept even when it alone is over.
fn fit<T: Weigh>(
    items: Vec<T>,
    budget: usize,
    starts: impl Fn(&[T], usize) -> bool,
) -> (Vec<T>, usize) {
    let starts: Vec<usize> = (0..items.len()).filter(|&at| starts(&items, at)).collect();
    let mut suffix = vec![0; items.len() + 1];
    for at in (0..items.len()).rev() {
        suffix[at] = suffix[at + 1] + items[at].weight();
    }
    let keep = starts
        .iter()
        .position(|&at| suffix[at] <= budget)
        .unwrap_or(starts.len().saturating_sub(1));
    let from = starts.get(keep).copied().unwrap_or(0);
    (items.into_iter().skip(from).collect(), keep)
}

/// Bytes of text an item puts in the context; an image counts as its bytes.
trait Weigh {
    fn weight(&self) -> usize;
}

fn content_weight(content: &[ContentPart]) -> usize {
    content
        .iter()
        .map(|part| match part {
            ContentPart::Text { text } => text.len(),
            ContentPart::Image { data, .. } => data.len(),
        })
        .sum()
}

impl Weigh for Event<'_> {
    fn weight(&self) -> usize {
        match self {
            Event::Request { output, inputs, .. } => {
                content_weight(output)
                    + inputs
                        .iter()
                        .map(|input| content_weight(&input.content))
                        .sum::<usize>()
            }
            Event::Step { prose, exec, .. } => prose.len() + exec.as_ref().map_or(0, String::len),
            _ => 0,
        }
    }
}

impl Weigh for Value {
    fn weight(&self) -> usize {
        self.to_string().len()
    }
}

// -- native to Claude --------------------------------------------------------

/// The messages of the Claude Code session a native agent's history becomes,
/// each `{"role", "content"}` as the Messages API takes it. `rows` is the
/// native era ([`era`]), all of it: a native compaction is encrypted, so the
/// rows it summarized are replayed instead, as far as `budget` allows.
///
/// Each request becomes a user message, split by role the way `compose`
/// splits it for both runtimes: the open call's answer is a `tool_result`, the
/// rest is text. Each step becomes an assistant message: its prose, then its
/// exec as a `tool_use` of `tool`, the name Claude Code calls exec by.
/// Reasoning and compactions are dropped.
pub fn native_to_claude(
    rows: &[(Wpos, Record<'static>)],
    from: &str,
    to: &str,
    tool: &str,
    budget: usize,
) -> Vec<Value> {
    let mut messages: Vec<Value> = Vec::new();
    let mut open: Option<String> = None;
    let mut output = Vec::new();
    let mut inputs: Vec<Input> = Vec::new();
    let mut notices: Vec<RequestNotice> = Vec::new();
    let mut calls = 0;
    let flush = |messages: &mut Vec<Value>,
                 open: &mut Option<String>,
                 output: &mut Vec<ContentPart>,
                 inputs: &mut Vec<Input>,
                 notices: &mut Vec<RequestNotice>| {
        let composed = compose(open.is_some(), notices, output, inputs);
        let mut content = Vec::new();
        if let Some(call) = open.take() {
            let mut result = blocks(&composed.result);
            if result.is_empty() {
                result.push(json!({"type": "text", "text": "(no output)"}));
            }
            content.push(json!({"type": "tool_result", "tool_use_id": call, "content": result}));
        }
        for user in &composed.user {
            content.extend(blocks(user));
        }
        output.clear();
        inputs.clear();
        notices.clear();
        push(messages, "user", content);
    };
    for (_, record) in rows {
        match &record.event {
            Event::Request {
                output: more,
                inputs: carried,
                notices: said,
                ..
            } => {
                add_output(&mut output, cut_content(more));
                inputs.extend(carried.iter().map(|input| Input {
                    content: cut_content(&input.content),
                    ..input.clone()
                }));
                for notice in said {
                    if !notices.contains(notice) {
                        notices.push(notice.clone());
                    }
                }
            }
            Event::Step {
                prose, exec, carry, ..
            } => {
                flush(
                    &mut messages,
                    &mut open,
                    &mut output,
                    &mut inputs,
                    &mut notices,
                );
                if carry.has_compaction() {
                    continue;
                }
                let mut content = Vec::new();
                if !prose.trim().is_empty() {
                    content.push(json!({"type": "text", "text": prose}));
                }
                if carry.has_call() {
                    calls += 1;
                    let id = format!("toolu_rho_switch_{calls}");
                    content.push(json!({
                        "type": "tool_use",
                        "id": id,
                        "name": tool,
                        "input": {"source": exec.clone().unwrap_or_default()},
                    }));
                    open = Some(id);
                }
                push(&mut messages, "assistant", content);
            }
            _ => {}
        }
    }
    if open.is_some() || !output.is_empty() || !inputs.is_empty() {
        if open.is_some() && output.is_empty() && inputs.is_empty() {
            output.push(ContentPart::Text {
                text: "The runtime switch cut this call off; it has no output.".into(),
            });
        }
        flush(
            &mut messages,
            &mut open,
            &mut output,
            &mut inputs,
            &mut notices,
        );
    }
    let (kept, left_out) = fit(messages, budget, |messages, at| {
        messages[at]["role"] == "user"
            && !messages[at]["content"]
                .as_array()
                .is_some_and(|content| content.iter().any(|block| block["type"] == "tool_result"))
    });
    let opening = format!(
        "This agent ran on {from} until now and runs on {to} from here. Rho rebuilt this \
         history from its log. The native model's reasoning did not carry over, and its \
         compactions are encrypted, so the turns they summarized are replayed instead{}. \
         The notebook restarted with the switch, so its globals and running cells are gone.",
        left_out_note(left_out)
    );
    let mut history = Vec::new();
    push(
        &mut history,
        "user",
        vec![json!({"type": "text", "text": opening})],
    );
    for message in kept {
        let role = message["role"].as_str().expect("a role").to_owned();
        let content = message["content"].as_array().cloned().unwrap_or_default();
        push(&mut history, &role, content);
    }
    // Claude Code resumes after an assistant turn or a user one alike; a
    // dangling call is answered above.
    history
}

/// Adds a message, joining it to the one before when they share a role: the
/// Messages API alternates them.
fn push(messages: &mut Vec<Value>, role: &str, content: Vec<Value>) {
    if content.is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut()
        && last["role"] == role
    {
        let blocks = last["content"].as_array_mut().expect("content blocks");
        // A tool result goes first in its message.
        let (results, rest): (Vec<_>, Vec<_>) = content
            .into_iter()
            .partition(|block| block["type"] == "tool_result");
        let at = blocks
            .iter()
            .position(|block| block["type"] != "tool_result")
            .unwrap_or(blocks.len());
        blocks.splice(at..at, results);
        blocks.extend(rest);
        return;
    }
    messages.push(json!({"role": role, "content": content}));
}

fn blocks(content: &[ContentPart]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } if text.is_empty() => None,
            ContentPart::Text { text } => Some(json!({"type": "text", "text": text})),
            ContentPart::Image { media_type, data } => {
                use base64::Engine as _;
                Some(json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": media_type,
                        "data": base64::engine::general_purpose::STANDARD.encode(data),
                    },
                }))
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests;
