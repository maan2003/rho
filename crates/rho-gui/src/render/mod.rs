//! Pure projection from protocol state to styled text spans.
//!
//! Nothing in this module touches editor buffers or entities: given a block
//! it produces the exact spans the transcript should contain. The transcript
//! model applies these as bounded buffer edits. Keeping this layer pure makes
//! block rendering testable as plain string assertions.

pub mod conceal;
pub mod elision;
pub mod markdown;

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::hash::{Hash as _, Hasher as _};
use std::ops::Range;
use std::sync::Mutex;
use std::time::Duration;

use rho_ui_proto::remote::{UiBlock, UiMessagePhase, UiTool, UiToolStatus};
use rho_ui_proto::{AgentId, MessageDelivery};

use crate::style::StyleClass;

#[derive(Clone, Debug, PartialEq)]
pub struct Span {
    pub text: String,
    pub class: StyleClass,
}

impl Span {
    pub fn new(text: impl Into<String>, class: StyleClass) -> Self {
        Self {
            text: text.into(),
            class,
        }
    }
}

/// Coarse block classification used for separators and transcript turn
/// boundaries. Immediate queued messages render like user messages and open
/// a turn right away; queued/steering placeholders render like user messages
/// but stay inside the current live turn until delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    User,
    QueuedUser,
    Response { working: bool },
}

/// An inlay position: an empty span marking where the transcript places
/// non-buffer text (a running tool's ticking duration, a queue label).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InlaySpec {
    pub span_index: usize,
    pub content: InlayContent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InlayContent {
    /// Refreshed once per second while the tool runs.
    RunningDuration { started_at_ms: u64 },
    /// Fixed text, e.g. a queued message's delivery label.
    Label(&'static str),
}

#[derive(Debug)]
pub struct RenderedBlock {
    pub spans: Vec<Span>,
    pub kind: BlockKind,
    /// Index of the span that should carry the user-message gutter accent.
    pub gutter_span: Option<usize>,
    pub inlay: Option<InlaySpec>,
    /// Markdown markup to hide at the display layer, as byte ranges into
    /// this block's rendered text.
    pub conceal: Vec<Range<usize>>,
}

impl RenderedBlock {
    pub fn visible(&self) -> bool {
        !self.spans.is_empty()
    }
}

pub fn block_kind(block: &UiBlock) -> BlockKind {
    match block {
        UiBlock::UserMessage { .. } => BlockKind::User,
        UiBlock::AssistantMessage { phase, .. } => BlockKind::Response {
            working: *phase != Some(UiMessagePhase::FinalAnswer),
        },
        UiBlock::Reasoning { .. } | UiBlock::Tool(_) | UiBlock::Notice { .. } => {
            BlockKind::Response { working: true }
        }
        UiBlock::QueuedMessage { delivery, .. } => match delivery {
            MessageDelivery::Immediate => BlockKind::User,
            MessageDelivery::NextRequest | MessageDelivery::NextTurn => BlockKind::QueuedUser,
        },
        UiBlock::AgentMessage { .. } => BlockKind::User,
    }
}

/// Separator inserted before a block, given the previous visible block's kind.
fn separator(prev: Option<BlockKind>, current: BlockKind) -> Option<Span> {
    match (prev, current) {
        // First block: no separator.
        (None, _) => None,
        // A new user message starts a new turn; the previous response block
        // ended with a single newline, so one more makes a blank line.
        (Some(BlockKind::Response { .. }), BlockKind::User | BlockKind::QueuedUser) => {
            Some(Span::new("\n", StyleClass::Default))
        }
        // User messages already end with a blank line.
        (Some(BlockKind::User | BlockKind::QueuedUser), _) => None,
        // Consecutive response items are separated by their own trailing
        // newlines.
        (Some(BlockKind::Response { .. }), BlockKind::Response { .. }) => None,
    }
}

/// One assistant message parsed: its syntax spans and the markup to
/// conceal, both derived from the message text alone.
#[derive(Clone)]
pub struct ParsedMessage {
    spans: Vec<Span>,
    conceal: Vec<Range<usize>>,
}

/// Assistant messages parsed before the view that will show them exists.
///
/// An agent nobody has opened yet stores its frames and renders them only on
/// the first view, which puts the whole scrollback's markdown parse on the
/// window's thread at the worst possible moment. Parsing is pure, so the
/// frame that delivers the text hands it to a background thread instead and
/// the first view collects the result.
#[derive(Default)]
pub struct ParseAhead {
    /// Keyed by the hash of the message text: blocks shift as a transcript
    /// grows and a delta re-renders its tail, but a message whose text did
    /// not change hashes the same. `None` marks a parse already handed out,
    /// so a burst of frames asks for the same text once.
    parsed: Mutex<HashMap<u64, Option<ParsedMessage>>>,
}

/// Message text worth parsing ahead for one agent. A first view lands at the
/// end of a transcript, so the budget is spent from the tail back.
const PARSE_AHEAD_BYTES: usize = 512 * 1024;

impl ParseAhead {
    /// The messages of `blocks` this cache neither holds nor has handed out,
    /// claimed by the caller to parse. Parses of text the state no longer
    /// carries are dropped here, which is what bounds the cache.
    pub fn claim(&self, blocks: &[UiBlock]) -> Vec<String> {
        let mut budget = PARSE_AHEAD_BYTES;
        let wanted = blocks
            .iter()
            .rev()
            .filter_map(message_text)
            .map_while(|text| {
                budget = budget.checked_sub(text.len())?;
                Some((text_hash(text), text))
            })
            .collect::<Vec<_>>();

        let mut parsed = self.lock();
        let keys = wanted.iter().map(|(key, _)| *key).collect::<Vec<_>>();
        parsed.retain(|key, _| keys.contains(key));
        wanted
            .into_iter()
            .filter(|(key, _)| match parsed.entry(*key) {
                Entry::Occupied(_) => false,
                Entry::Vacant(slot) => {
                    slot.insert(None);
                    true
                }
            })
            .map(|(_, text)| text.to_owned())
            .collect()
    }

    /// Parses claimed texts, over as many threads as they are worth. Runs
    /// off the main thread, but a transcript is still a second of parsing
    /// and the view it is meant for can arrive at any moment.
    pub fn fill(&self, texts: Vec<String>, markdown: &markdown::Markdown) {
        let fill = |texts: &[String]| {
            for text in texts {
                let message = parse_message(text, markdown);
                // The state may have moved on while this parsed; fill only a
                // claim that is still outstanding.
                if let Some(slot @ None) = self.lock().get_mut(&text_hash(text)) {
                    *slot = Some(message);
                }
            }
        };

        let total = texts.iter().map(String::len).sum::<usize>();
        let threads = (total / MIN_BYTES_PER_PARSE_THREAD).clamp(1, max_parse_threads());
        if threads == 1 {
            fill(&texts);
            return;
        }
        let per_thread = total.div_ceil(threads);
        std::thread::scope(|scope| {
            let mut texts = texts.as_slice();
            while !texts.is_empty() {
                let take = texts_worth(texts, per_thread);
                let (head, tail) = texts.split_at(take);
                scope.spawn(move || fill(head));
                texts = tail;
            }
        });
    }

    /// The parse of `text`, if one was made ahead of this view. Copied
    /// rather than taken: a transcript may carry the same message twice, and
    /// copying spans costs a fraction of parsing them again.
    fn get(&self, text: &str) -> Option<ParsedMessage> {
        self.lock().get(&text_hash(text))?.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Option<ParsedMessage>>> {
        self.parsed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn text_hash(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

/// Parses the assistant messages in `blocks`, over as many threads as the
/// text is worth, taking whatever [`ParseAhead`] already holds. Parsing is
/// pure and dominates a full sync of a long transcript; leaving it inline
/// would parse the whole scrollback on the thread the window is waiting on.
pub fn parse_messages(
    blocks: &[UiBlock],
    markdown: &markdown::Markdown,
    ahead: Option<&ParseAhead>,
) -> Vec<Option<ParsedMessage>> {
    fn parse(
        blocks: &[UiBlock],
        markdown: &markdown::Markdown,
        into: &mut [Option<ParsedMessage>],
    ) {
        for (block, slot) in blocks.iter().zip(into) {
            if slot.is_none()
                && let Some(text) = message_text(block)
            {
                *slot = Some(parse_message(text, markdown));
            }
        }
    }

    let mut parsed = Vec::new();
    parsed.resize_with(blocks.len(), || None);
    if let Some(ahead) = ahead {
        for (block, slot) in blocks.iter().zip(&mut parsed) {
            *slot = message_text(block).and_then(|text| ahead.get(text));
        }
    }

    let total = unparsed_bytes(blocks, &parsed);
    let threads = (total / MIN_BYTES_PER_PARSE_THREAD).clamp(1, max_parse_threads());
    if threads == 1 {
        parse(blocks, markdown, &mut parsed);
        return parsed;
    }

    let per_thread = total.div_ceil(threads);
    std::thread::scope(|scope| {
        let mut blocks = blocks;
        let mut slots = parsed.as_mut_slice();
        while !blocks.is_empty() {
            let take = blocks_worth(blocks, slots, per_thread);
            let (head, blocks_tail) = blocks.split_at(take);
            let (head_slots, slots_tail) = slots.split_at_mut(take);
            scope.spawn(move || parse(head, markdown, head_slots));
            blocks = blocks_tail;
            slots = slots_tail;
        }
    });
    parsed
}

/// How many leading texts carry `bytes` worth, at least one.
fn texts_worth(texts: &[String], bytes: usize) -> usize {
    let mut taken = 0;
    for (index, text) in texts.iter().enumerate() {
        taken += text.len();
        if taken >= bytes {
            return index + 1;
        }
    }
    texts.len()
}

/// The message text in `blocks` that no slot holds a parse for.
fn unparsed_bytes(blocks: &[UiBlock], parsed: &[Option<ParsedMessage>]) -> usize {
    blocks
        .iter()
        .zip(parsed)
        .filter(|(_, slot)| slot.is_none())
        .filter_map(|(block, _)| message_text(block))
        .map(str::len)
        .sum()
}

/// How many leading blocks carry `bytes` worth of unparsed text, at least one.
fn blocks_worth(blocks: &[UiBlock], parsed: &[Option<ParsedMessage>], bytes: usize) -> usize {
    let mut taken = 0;
    for (index, (block, slot)) in blocks.iter().zip(parsed).enumerate() {
        if slot.is_none() {
            taken += message_text(block).map_or(0, str::len);
        }
        if taken >= bytes {
            return index + 1;
        }
    }
    blocks.len()
}

fn message_text(block: &UiBlock) -> Option<&str> {
    match block {
        UiBlock::AssistantMessage { text, .. } if !text.is_empty() => Some(text),
        _ => None,
    }
}

/// Text worth handing to a thread of its own. Below this the hand-off (and
/// the per-thread parser setup behind it) costs more than the parse does:
/// a 32KB transcript measured 0.10s in place against 0.16s on five threads.
const MIN_BYTES_PER_PARSE_THREAD: usize = 64 * 1024;

/// Past this the threads spend more on contention - the allocator, mostly,
/// since parsing allocates heavily - than they save: a 320KB transcript
/// measured 0.92s on one thread, 0.49s on eight, and 2.0s on fifty.
const MAX_PARSE_THREADS: usize = 8;

fn max_parse_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(1, |threads| threads.get())
        .min(MAX_PARSE_THREADS)
}

fn parse_message(text: &str, markdown: &markdown::Markdown) -> ParsedMessage {
    let owned;
    let text = if text.ends_with('\n') {
        text
    } else {
        owned = format!("{text}\n");
        &owned
    };
    // Both halves read the same trees: the grammars are the slowest thing
    // in a transcript sync, so the message is parsed once.
    let trees = conceal::parse(text);
    ParsedMessage {
        conceal: conceal::concealed_ranges_of(text, &trees),
        spans: markdown::markdown_spans_of(text, &trees, markdown),
    }
}

pub fn render_block_with_agent_labels(
    block: &UiBlock,
    prev: Option<BlockKind>,
    now_ms: u64,
    agent_label: &impl Fn(AgentId) -> String,
    markdown: &markdown::Markdown,
    parsed: Option<ParsedMessage>,
) -> RenderedBlock {
    let kind = block_kind(block);
    let mut spans = Vec::new();
    let mut gutter_span = None;
    let mut inlay = None;
    let mut conceal = Vec::new();
    match block {
        UiBlock::UserMessage { text } => {
            if text.is_empty() {
                return invisible(kind);
            }
            spans.extend(separator(prev, kind));
            gutter_span = Some(spans.len());
            spans.push(Span::new(format!("{text}\n\n"), StyleClass::UserMessage));
        }
        UiBlock::AssistantMessage { text, .. } => {
            if text.is_empty() {
                return invisible(kind);
            }
            spans.extend(separator(prev, kind));
            let parsed = parsed.unwrap_or_else(|| parse_message(text, markdown));
            // Conceal ranges are computed over the message text; shift them
            // past whatever separator this block rendered first.
            let offset = spans_len(&spans);
            conceal = parsed
                .conceal
                .into_iter()
                .map(|range| range.start + offset..range.end + offset)
                .collect();
            spans.extend(parsed.spans);
        }
        UiBlock::Reasoning { .. } => return invisible(kind),
        UiBlock::Tool(tool) => {
            spans.extend(separator(prev, kind));
            inlay = push_tool_spans(&mut spans, tool, now_ms);
        }
        UiBlock::Notice { text } => {
            if text.is_empty() {
                return invisible(kind);
            }
            spans.extend(separator(prev, kind));
            let mut text = text.clone();
            if !text.ends_with('\n') {
                text.push('\n');
            }
            spans.push(Span::new(text, StyleClass::SystemInfo));
        }
        UiBlock::AgentMessage { sender, text } => {
            if text.is_empty() {
                return invisible(kind);
            }
            spans.extend(separator(prev, kind));
            spans.push(Span::new(
                format!("from {}\n", agent_label(*sender)),
                StyleClass::AgentLabel,
            ));
            gutter_span = Some(spans.len());
            spans.push(Span::new(format!("{text}\n\n"), StyleClass::AgentMessage));
        }
        UiBlock::QueuedMessage {
            text,
            delivery,
            sender,
        } => {
            if text.is_empty() {
                return invisible(kind);
            }
            spans.extend(separator(prev, kind));
            if let Some(sender) = sender {
                spans.push(Span::new(
                    format!("from {}\n", agent_label(*sender)),
                    StyleClass::AgentLabel,
                ));
            }
            gutter_span = Some(spans.len());
            spans.push(Span::new(
                text.clone(),
                if sender.is_some() {
                    StyleClass::AgentMessage
                } else {
                    StyleClass::UserMessage
                },
            ));
            let label = match delivery {
                MessageDelivery::Immediate => None,
                MessageDelivery::NextRequest => Some(" (steering)"),
                MessageDelivery::NextTurn => Some(" (queued)"),
            };
            if let Some(label) = label {
                inlay = Some(InlaySpec {
                    span_index: spans.len(),
                    content: InlayContent::Label(label),
                });
                spans.push(Span::new("", StyleClass::SystemInfo));
            }
            spans.push(Span::new("\n\n", StyleClass::Default));
        }
    }
    RenderedBlock {
        spans,
        kind,
        gutter_span,
        inlay,
        conceal,
    }
}

fn spans_len(spans: &[Span]) -> usize {
    spans.iter().map(|span| span.text.len()).sum()
}

fn invisible(kind: BlockKind) -> RenderedBlock {
    RenderedBlock {
        spans: Vec::new(),
        kind,
        gutter_span: None,
        inlay: None,
        conceal: Vec::new(),
    }
}

/// Renders one tool call line: `label status [duration]`.
///
/// Finished tools render their duration as text. Running tools with a start
/// timestamp get an empty position span instead: the live duration renders
/// as an inlay there, so per-second ticks never edit the buffer.
fn push_tool_spans(spans: &mut Vec<Span>, tool: &UiTool, now_ms: u64) -> Option<InlaySpec> {
    let (label, class) = tool_label(&tool.name, &tool.arguments);
    spans.push(Span::new(label, class));
    spans.push(Span::new(" ", StyleClass::ToolDetail));
    let status = tool_status_label(tool.status);
    spans.push(Span::new(status, tool_status_class(tool.status)));

    let mut timer = None;
    if tool.status == UiToolStatus::Running {
        if let Some(started_at) = tool.started_at {
            timer = Some(InlaySpec {
                span_index: spans.len(),
                content: InlayContent::RunningDuration {
                    started_at_ms: started_at.0,
                },
            });
            spans.push(Span::new("", StyleClass::Time));
        }
    } else if let Some(duration) = tool_duration_at(tool, now_ms) {
        spans.push(Span::new(
            format!(" {}", format_tool_duration(duration)),
            StyleClass::Time,
        ));
    }

    if !spans.last().is_some_and(|span| span.text.ends_with('\n')) {
        spans.push(Span::new("\n", StyleClass::Default));
    }
    timer
}

fn tool_status_label(status: UiToolStatus) -> &'static str {
    match status {
        UiToolStatus::Running => "…",
        UiToolStatus::Success => "ok",
        UiToolStatus::Error => "error",
        UiToolStatus::Cancelled => "cancelled",
    }
}

fn tool_status_class(status: UiToolStatus) -> StyleClass {
    match status {
        UiToolStatus::Running => StyleClass::StatusRunning,
        UiToolStatus::Success => StyleClass::StatusOk,
        UiToolStatus::Error => StyleClass::StatusError,
        UiToolStatus::Cancelled => StyleClass::StatusCancelled,
    }
}

pub fn tool_duration_at(tool: &UiTool, now_ms: u64) -> Option<Duration> {
    let started_at = tool.started_at?.0;
    let finished_at = tool
        .finished_at
        .map(|finished_at| finished_at.0)
        .or_else(|| (tool.status == UiToolStatus::Running).then_some(now_ms))?;
    let duration = Duration::from_millis(finished_at.saturating_sub(started_at));
    (Duration::from_secs(1) <= duration).then_some(duration)
}

pub fn format_tool_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{}s", seconds / 60, seconds % 60)
    }
}

pub fn format_running_duration(started_at_ms: u64, now_ms: u64) -> String {
    let duration = Duration::from_millis(now_ms.saturating_sub(started_at_ms));
    if duration < Duration::from_secs(1) {
        String::new()
    } else {
        format!(" {}", format_tool_duration(duration))
    }
}

/// Label for a tool call, with the style class for its verb.
///
/// Shell-like tools (Codex `shell`/`shell_command`, Claude `Bash`) render as
/// `$ command`. Claude's file tools render as `read/write/edit path` so the
/// transcript shows the touched file instead of raw JSON arguments. Argument
/// extraction tolerates the partial JSON seen while arguments stream.
fn tool_label(name: &str, arguments: &str) -> (String, StyleClass) {
    match name {
        "shell" | "shell_command" | "Bash" => {
            let command = shell_command_argument_label(arguments);
            let label = if command.is_empty() {
                "$".to_owned()
            } else {
                format!("$ {command}")
            };
            (label, StyleClass::ToolShell)
        }
        "Read" | "Write" | "Edit" => {
            let verb = name.to_ascii_lowercase();
            let label = match streaming_json_text_field(arguments, "file_path") {
                Some(path) if !path.is_empty() => format!("{verb} {path}"),
                _ => verb,
            };
            (label, StyleClass::ToolName)
        }
        _ if arguments.is_empty() => (name.to_owned(), StyleClass::ToolName),
        _ => (format!("{name} {arguments}"), StyleClass::ToolName),
    }
}

fn shell_command_argument_label(arguments: &str) -> String {
    streaming_json_text_field(arguments, "command")
        .or_else(|| (!arguments.trim_start().starts_with('{')).then(|| arguments.to_owned()))
        .unwrap_or_default()
}

fn streaming_json_text_field(arguments: &str, key: &str) -> Option<String> {
    let mut parser = json_stream::JsonStreamParser::new();
    for character in arguments.chars() {
        if parser.add_char(character).is_err() {
            return None;
        }
    }
    parser
        .get_result()
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use rho_core::UnixMs;

    use super::*;

    fn tool(status: UiToolStatus) -> UiTool {
        UiTool {
            id: "tool-1".to_owned(),
            name: "shell_command".to_owned(),
            arguments: "echo ok".to_owned(),
            preview: None,
            status,
            output: None,
            error: None,
            started_at: None,
            finished_at: None,
            metadata: None,
        }
    }

    fn text_of(spans: &[Span]) -> String {
        spans.iter().map(|span| span.text.as_str()).collect()
    }

    #[test]
    fn shell_command_argument_label_extracts_streaming_json() {
        assert_eq!(shell_command_argument_label(r#"{"command":"echo"#), "echo");
        assert_eq!(shell_command_argument_label(r#"{"comm"#), "");
        assert_eq!(shell_command_argument_label("echo ok"), "echo ok");
    }

    #[test]
    fn claude_bash_renders_as_shell_prompt() {
        assert_eq!(
            tool_label(
                "Bash",
                r#"{"command":"cargo test","description":"Run tests"}"#
            ),
            ("$ cargo test".to_owned(), StyleClass::ToolShell)
        );
        // Streaming partial JSON still resolves the command field.
        assert_eq!(
            tool_label("Bash", r#"{"command":"cargo te"#),
            ("$ cargo te".to_owned(), StyleClass::ToolShell)
        );
        assert_eq!(
            tool_label("Bash", r#"{"desc"#),
            ("$".to_owned(), StyleClass::ToolShell)
        );
    }

    #[test]
    fn claude_file_tools_render_verb_and_path() {
        assert_eq!(
            tool_label("Read", r#"{"file_path":"/tmp/a.rs","limit":40}"#),
            ("read /tmp/a.rs".to_owned(), StyleClass::ToolName)
        );
        assert_eq!(
            tool_label(
                "Edit",
                r#"{"file_path":"/tmp/a.rs","old_string":"a","new_string":"b"}"#
            ),
            ("edit /tmp/a.rs".to_owned(), StyleClass::ToolName)
        );
        assert_eq!(
            tool_label("Write", r#"{"file_p"#),
            ("write".to_owned(), StyleClass::ToolName)
        );
    }

    #[test]
    fn tool_spans_render_status_and_duration() {
        let mut finished = tool(UiToolStatus::Success);
        finished.started_at = Some(UnixMs(1_000));
        finished.finished_at = Some(UnixMs(3_500));
        let mut spans = Vec::new();
        let timer = push_tool_spans(&mut spans, &finished, 10_000);
        assert_eq!(timer, None);
        assert_eq!(text_of(&spans), "$ echo ok ok 2s\n");
    }

    #[test]
    fn tool_duration_suppresses_subsecond_values() {
        let mut running = tool(UiToolStatus::Running);
        running.started_at = Some(UnixMs(1_000));
        assert_eq!(tool_duration_at(&running, 1_999), None);
        assert_eq!(
            tool_duration_at(&running, 2_000),
            Some(Duration::from_secs(1))
        );

        let mut finished = tool(UiToolStatus::Success);
        finished.started_at = Some(UnixMs(1_000));
        finished.finished_at = Some(UnixMs(1_999));
        assert_eq!(tool_duration_at(&finished, 10_000), None);
    }

    #[test]
    fn running_tool_gets_a_timer_position_marker_not_text() {
        let mut running = tool(UiToolStatus::Running);
        running.started_at = Some(UnixMs(1_000));
        let mut spans = Vec::new();
        let timer = push_tool_spans(&mut spans, &running, 3_500);
        let timer = timer.expect("running tool with start time should have a timer");
        assert_eq!(spans[timer.span_index].text, "");
        assert_eq!(text_of(&spans), "$ echo ok …\n");
    }

    #[test]
    fn separators_give_user_messages_a_turn_gap() {
        assert_eq!(separator(None, BlockKind::User), None);
        assert_eq!(
            separator(
                Some(BlockKind::Response { working: false }),
                BlockKind::User
            ),
            Some(Span::new("\n", StyleClass::Default))
        );
        assert_eq!(
            separator(Some(BlockKind::User), BlockKind::Response { working: true }),
            None
        );
        assert_eq!(
            separator(
                Some(BlockKind::Response { working: true }),
                BlockKind::QueuedUser
            ),
            Some(Span::new("\n", StyleClass::Default))
        );
    }

    #[test]
    fn only_immediate_queued_messages_are_turn_users() {
        assert_eq!(
            block_kind(&UiBlock::QueuedMessage {
                text: "now".to_owned(),
                delivery: MessageDelivery::Immediate,
                sender: None,
            }),
            BlockKind::User
        );
        assert_eq!(
            block_kind(&UiBlock::QueuedMessage {
                text: "later".to_owned(),
                delivery: MessageDelivery::NextRequest,
                sender: None,
            }),
            BlockKind::QueuedUser
        );
        assert_eq!(
            block_kind(&UiBlock::QueuedMessage {
                text: "later".to_owned(),
                delivery: MessageDelivery::NextTurn,
                sender: None,
            }),
            BlockKind::QueuedUser
        );
    }
}
