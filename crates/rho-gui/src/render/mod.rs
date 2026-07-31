//! Pure projection from protocol state to styled text spans.
//!
//! Nothing in this module touches editor buffers or entities: given a block
//! it produces the exact spans the transcript should contain. The transcript
//! model applies these as bounded buffer edits. Keeping this layer pure makes
//! block rendering testable as plain string assertions.

pub mod elision;
pub mod markdown;

use std::ops::Range;
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlaySpec {
    pub span_index: usize,
    pub content: InlayContent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InlayContent {
    /// Refreshed once per second while the tool runs.
    RunningDuration { started_at_ms: u64 },
    /// Fixed text, e.g. a queued message's delivery label.
    Label(&'static str),
    /// Display-only text anchored in a transcript record.
    Text(String),
}

#[derive(Debug)]
pub struct RenderedBlock {
    pub spans: Vec<Span>,
    pub kind: BlockKind,
    /// Index of the span that should carry the user-message gutter accent.
    pub gutter_span: Option<usize>,
    pub inlay: Option<InlaySpec>,
    /// Whether this block belongs to a Markdown syntax buffer.
    pub markdown: bool,
    /// Immutable visualization references embedded in this message.
    pub visualizations: Vec<VisualizationSpec>,
    /// Virtual tab padding that aligns source-visible Markdown table columns.
    pub table_padding: Vec<TablePaddingSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualizationSpec {
    pub range: Range<usize>,
    pub id: String,
    pub rows: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TablePaddingSpec {
    pub position: usize,
    pub tabs: usize,
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

fn visualization_refs(text: &str) -> Vec<(Range<usize>, String, u32)> {
    let mut refs = Vec::new();
    let lines = text
        .split_inclusive('\n')
        .scan(0, |offset, line| {
            let start = *offset;
            *offset += line.len();
            Some((start, line.trim_end_matches(['\r', '\n'])))
        })
        .collect::<Vec<_>>();
    let mut enclosing_fence = None;
    let mut ix = 0;
    while ix < lines.len() {
        let (start, line) = lines[ix];
        if let Some((marker, width)) = enclosing_fence {
            if is_fence_close(line, marker, width) {
                enclosing_fence = None;
            }
            ix += 1;
            continue;
        }

        if strip_fence_indent(line) == Some("```visualization") && ix + 2 < lines.len() {
            let end = lines[ix + 2].0 + lines[ix + 2].1.len();
            if strip_fence_indent(lines[ix + 2].1) == Some("```")
                && let Some((id, rows)) = parse_visualization_attributes(lines[ix + 1].1.trim())
            {
                refs.push((start..end, id, rows));
                ix += 3;
                continue;
            }
        }
        enclosing_fence = fence_open(line);
        ix += 1;
    }
    refs
}

fn fence_open(line: &str) -> Option<(u8, usize)> {
    let line = strip_fence_indent(line)?;
    let marker = *line.as_bytes().first()?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let width = line.bytes().take_while(|byte| *byte == marker).count();
    (width >= 3).then_some((marker, width))
}

fn is_fence_close(line: &str, marker: u8, width: usize) -> bool {
    let Some(line) = strip_fence_indent(line) else {
        return false;
    };
    let marker_width = line.bytes().take_while(|byte| *byte == marker).count();
    marker_width >= width && line[marker_width..].trim().is_empty()
}

fn strip_fence_indent(line: &str) -> Option<&str> {
    let indent = line.bytes().take_while(|byte| *byte == b' ').count();
    (indent <= 3).then(|| &line[indent..])
}

const MAX_VISUALIZATION_ROWS: u32 = 50;

fn parse_visualization_attributes(body: &str) -> Option<(String, u32)> {
    let body = body.strip_prefix("ref=")?;
    let (id, rows) = body.split_once(" rows=")?;
    let rows = rows
        .parse()
        .ok()
        .filter(|rows| (1..=MAX_VISUALIZATION_ROWS).contains(rows))?;
    (id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| (id.to_ascii_lowercase(), rows))
}

pub fn render_block_with_agent_labels(
    block: &UiBlock,
    prev: Option<BlockKind>,
    now_ms: u64,
    agent_label: &impl Fn(AgentId) -> String,
) -> RenderedBlock {
    let kind = block_kind(block);
    let mut spans = Vec::new();
    let mut gutter_span = None;
    let mut inlay = None;
    let mut markdown = false;
    let mut visualizations = Vec::new();
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
            markdown = true;
            let offset = spans_len(&spans);
            visualizations = if text.contains("```visualization") {
                visualization_refs(text)
                    .into_iter()
                    .map(|(range, id, rows)| VisualizationSpec {
                        range: range.start + offset..range.end + offset,
                        id,
                        rows,
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let mut text = text.clone();
            if !text.ends_with('\n') {
                text.push('\n');
            }
            let table_padding = if text.contains('|') {
                table_padding(&text)
            } else {
                Vec::new()
            }
            .into_iter()
                .map(|mut padding| {
                    padding.position += offset;
                    padding
                })
                .collect();
            spans.push(Span::new(text, StyleClass::Default));
            return RenderedBlock {
                spans,
                kind,
                gutter_span,
                inlay,
                markdown,
                visualizations,
                table_padding,
            };
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
        markdown,
        visualizations,
        table_padding: Vec::new(),
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
        markdown: false,
        visualizations: Vec::new(),
        table_padding: Vec::new(),
    }
}

const TABLE_TAB_WIDTH: usize = 4;
const MAX_TABLE_WIDTH: usize = 96;

fn table_padding(text: &str) -> Vec<TablePaddingSpec> {
    let mut parser = tree_sitter::Parser::new();
    let Ok(()) = parser.set_language(&tree_sitter_md::LANGUAGE.into()) else {
        return Vec::new();
    };
    let Some(tree) = parser.parse(text, None) else {
        return Vec::new();
    };
    let mut padding = Vec::new();
    collect_table_padding(text, tree.root_node(), &mut padding);
    padding
}

fn collect_table_padding(
    text: &str,
    node: tree_sitter::Node<'_>,
    padding: &mut Vec<TablePaddingSpec>,
) {
    if node.kind() == "pipe_table" {
        padding.extend(table_padding_for_node(text, node));
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_table_padding(text, child, padding);
    }
}

fn table_padding_for_node(text: &str, table: tree_sitter::Node<'_>) -> Vec<TablePaddingSpec> {
    let mut rows = Vec::new();
    let mut cursor = table.walk();
    for row in table.children(&mut cursor).filter(|node| {
        matches!(
            node.kind(),
            "pipe_table_header" | "pipe_table_delimiter_row" | "pipe_table_row"
        )
    }) {
        let mut cells = row.walk();
        let boundaries = row
            .children(&mut cells)
            .filter(|node| matches!(node.kind(), "pipe_table_cell" | "pipe_table_delimiter_cell"))
            .filter_map(|cell| next_pipe(text, cell.end_byte(), row.end_byte()))
            .collect::<Vec<_>>();
        if !boundaries.is_empty() {
            rows.push((row.start_byte(), boundaries));
        }
    }

    let columns = rows.iter().map(|(_, row)| row.len()).min().unwrap_or(0);
    if columns == 0 {
        return Vec::new();
    }

    let mut targets = Vec::with_capacity(columns);
    for column in 0..columns {
        let widest = rows
            .iter()
            .map(|(line_start, boundaries)| {
                let previous = column
                    .checked_sub(1)
                    .and_then(|previous| boundaries.get(previous).copied())
                    .unwrap_or(*line_start);
                display_columns(&text[previous..boundaries[column]])
            })
            .max()
            .unwrap_or(0);
        let start = targets.last().copied().unwrap_or(0);
        let target = next_tab_stop(start + widest);
        if target > MAX_TABLE_WIDTH {
            return Vec::new();
        }
        targets.push(target);
    }

    rows.into_iter()
        .flat_map(|(line_start, boundaries)| {
            let mut previous_target = 0;
            let mut previous_boundary = line_start;
            boundaries
                .into_iter()
                .take(columns)
                .zip(targets.iter().copied())
                .filter_map(move |(boundary, target)| {
                    let current =
                        previous_target + display_columns(&text[previous_boundary..boundary]);
                    previous_target = target;
                    previous_boundary = boundary;
                    let tabs = target.saturating_sub(current).div_ceil(TABLE_TAB_WIDTH);
                    (tabs > 0).then_some(TablePaddingSpec {
                        position: boundary,
                        tabs,
                    })
                })
        })
        .collect()
}

fn next_pipe(text: &str, start: usize, end: usize) -> Option<usize> {
    text.get(start..end)?.find('|').map(|offset| start + offset)
}

fn display_columns(text: &str) -> usize {
    text.chars().count()
}

fn next_tab_stop(column: usize) -> usize {
    column.div_ceil(TABLE_TAB_WIDTH) * TABLE_TAB_WIDTH
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
    // Completed historical calls should be parsed in one pass. The streaming
    // parser repairs its partial result after every character, which is useful
    // for live arguments but quadratic for a large completed command.
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) {
        return value.get(key)?.as_str().map(str::to_owned);
    }

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
    fn table_padding_aligns_pipe_columns_without_changing_source() {
        let text = "## Results\n\n| Name | Outcome |\n| --- | --- |\n| one | passed |\n| longest name | failed |\n";
        let padding = table_padding(text);
        assert!(!padding.is_empty());

        let mut columns = Vec::new();
        let mut line_start = 0;
        for (index, line) in text.split_inclusive('\n').enumerate() {
            if index < 2 {
                line_start += line.len();
                continue;
            }
            let mut column = 0;
            let mut pipes = Vec::new();
            for (offset, character) in line.char_indices() {
                if let Some(padding) = padding
                    .iter()
                    .find(|padding| padding.position == line_start + offset)
                {
                    for _ in 0..padding.tabs {
                        column += TABLE_TAB_WIDTH - column % TABLE_TAB_WIDTH;
                    }
                }
                if character == '|' {
                    pipes.push(column);
                }
                column += 1;
            }
            columns.push(pipes);
            line_start += line.len();
        }
        assert_eq!(columns[0], columns[1]);
        assert_eq!(columns[1], columns[2]);
        assert_eq!(columns[2], columns[3]);
    }

    #[test]
    fn table_padding_ignores_incomplete_tables() {
        assert!(table_padding("| Name | Outcome |\n").is_empty());
    }

    #[test]
    fn visualization_ref_parser_accepts_complete_canonical_fences() {
        let fence = "```visualization\nref=0123456789abcdef0123456789abcdef rows=12\n```";
        let text = format!("before\n{fence}\nafter");
        assert_eq!(
            visualization_refs(&text),
            vec![(
                7..7 + fence.len(),
                "0123456789abcdef0123456789abcdef".to_owned(),
                12
            )]
        );
    }

    #[test]
    fn visualization_ref_parser_ignores_streaming_and_malformed_fences() {
        let refs = visualization_refs;
        assert!(refs("```visualization\nref=0123").is_empty());
        assert!(refs("```visualization\nref=not-an-id rows=12\n```").is_empty());
        assert!(refs("```visualization\nref=0123456789abcdef0123456789abcdef\n```").is_empty());
        assert!(
            refs("```visualization\nref=0123456789abcdef0123456789abcdef rows=0\n```").is_empty()
        );
        assert!(
            refs("```visualization\nref=0123456789abcdef0123456789abcdef rows=many\n```")
                .is_empty()
        );
        assert_eq!(
            refs("```visualization\nref=0123456789abcdef0123456789abcdef rows=50\n```").len(),
            1
        );
        assert!(
            refs("```visualization\nref=0123456789abcdef0123456789abcdef rows=51\n```").is_empty()
        );
        assert!(refs("```rust\nref=0123456789abcdef0123456789abcdef rows=12\n```").is_empty());
        assert!(
            refs("````text\n```visualization\nref=0123456789abcdef0123456789abcdef rows=12\n```\n````")
                .is_empty()
        );
        assert!(
            refs("    ```visualization\n    ref=0123456789abcdef0123456789abcdef rows=12\n    ```")
                .is_empty()
        );
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
