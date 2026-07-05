//! Pure projection from protocol state to styled text spans.
//!
//! Nothing in this module touches editor buffers or entities: given a block
//! it produces the exact spans the transcript should contain. The transcript
//! model applies these as bounded buffer edits. Keeping this layer pure makes
//! block rendering testable as plain string assertions.

pub mod elision;
pub mod markdown;

use std::time::Duration;

use gpui::App;
use rho_ui_proto::MessageDelivery;
use rho_ui_proto::remote::{UiBlock, UiMessagePhase, UiTool, UiToolStatus};

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

pub fn render_block(
    block: &UiBlock,
    prev: Option<BlockKind>,
    now_ms: u64,
    cx: &App,
) -> RenderedBlock {
    let kind = block_kind(block);
    let mut spans = Vec::new();
    let mut gutter_span = None;
    let mut inlay = None;
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
            spans.extend(markdown::markdown_spans_with_newline(text, cx));
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
        UiBlock::QueuedMessage { text, delivery } => {
            if text.is_empty() {
                return invisible(kind);
            }
            spans.extend(separator(prev, kind));
            gutter_span = Some(spans.len());
            spans.push(Span::new(text.clone(), StyleClass::UserMessage));
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
    }
}

fn invisible(kind: BlockKind) -> RenderedBlock {
    RenderedBlock {
        spans: Vec::new(),
        kind,
        gutter_span: None,
        inlay: None,
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
            }),
            BlockKind::User
        );
        assert_eq!(
            block_kind(&UiBlock::QueuedMessage {
                text: "later".to_owned(),
                delivery: MessageDelivery::NextRequest,
            }),
            BlockKind::QueuedUser
        );
        assert_eq!(
            block_kind(&UiBlock::QueuedMessage {
                text: "later".to_owned(),
                delivery: MessageDelivery::NextTurn,
            }),
            BlockKind::QueuedUser
        );
    }
}
