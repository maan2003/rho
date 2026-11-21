//! Pure projection from protocol state to styled text spans.
//!
//! Nothing in this module touches editor buffers or entities: given a block
//! it produces the exact spans the transcript should contain. The transcript
//! model applies these as bounded buffer edits. Keeping this layer pure makes
//! block rendering testable as plain string assertions.

use std::collections::HashMap;
use std::ops::Range;
use std::time::Duration;

use rho_agent_types::{AgentId, ChatSeq, MessageId};
use rho_agents_client::protocol::transcript::ArgumentsFormat;
use rho_agents_client::state::{FailureKind, UiBlock, UiMessagePhase, UiTool, UiToolStatus};
use rho_window::style::StyleClass;

/// A caption crosses from the agent directory or the mail log into the
/// transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Label {
    Agent(AgentId),
    Write(MessageId, Option<ChatSeq>),
    Delivery(MessageId),
}

impl Label {
    pub(crate) fn for_block(block: &UiBlock) -> Option<Self> {
        match block {
            UiBlock::MessageDraft { from, .. } => Some(Self::Agent(*from)),
            UiBlock::MailWritten { message, version } => Some(Self::Write(*message, *version)),
            UiBlock::Delivered { message, .. } => Some(Self::Delivery(*message)),
            _ => None,
        }
    }
}

pub(crate) type Labels = HashMap<Label, String>;

/// The one projection of an agent's blocks: everything it read and did,
/// in order. Hidden blocks keep their indices so detail requests, lazy
/// history, and cursor anchors still agree.
pub(crate) fn render(
    block: &UiBlock,
    previous: Option<BlockKind>,
    now_ms: u64,
    label: &impl Fn(Label) -> String,
) -> RenderedBlock {
    if block_visible(block) {
        render_labelled(block, previous, now_ms, label)
    } else {
        invisible(block_kind(block))
    }
}

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
/// boundaries. Queued messages stay inside the current live turn until
/// delivery.
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
        UiBlock::UserMessage { .. } | UiBlock::Delivered { .. } => BlockKind::User,
        UiBlock::AssistantMessage { phase, .. } => BlockKind::Response {
            working: *phase != Some(UiMessagePhase::FinalAnswer),
        },
        UiBlock::Reasoning { .. }
        | UiBlock::MessageDraft { .. }
        | UiBlock::Tool(_)
        | UiBlock::MailWritten { .. }
        | UiBlock::Notice { .. }
        | UiBlock::Failure { .. }
        | UiBlock::Responded => BlockKind::Response { working: true },
        UiBlock::Status { .. } => BlockKind::QueuedUser,
    }
}

/// Whether a block renders to anything at all, answered without rendering
/// it. History is composed only when a reader asks for it, so the shape of
/// the transcript — which blocks a reader could see, which turn an elision
/// plan covers — has to be answerable for blocks whose spans do not exist.
/// [`RenderedBlock::visible`] is the same question asked of the spans.
pub fn block_visible(block: &UiBlock) -> bool {
    match block {
        UiBlock::Reasoning { .. } | UiBlock::Responded => false,
        UiBlock::Tool(_) | UiBlock::MailWritten { .. } | UiBlock::Failure { .. } => true,
        UiBlock::UserMessage { text }
        | UiBlock::AssistantMessage { text, .. }
        | UiBlock::MessageDraft { text, .. }
        | UiBlock::Notice { text, .. }
        | UiBlock::Status { text }
        | UiBlock::Delivered { text, .. } => !text.is_empty(),
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

fn render_labelled(
    block: &UiBlock,
    prev: Option<BlockKind>,
    now_ms: u64,
    label: &impl Fn(Label) -> String,
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
            // The user's own words are shown as they were typed: their
            // asterisks and backticks are text, not markup, so this block
            // is not markdown and gets a plain buffer of its own.
            spans.push(Span::new(format!("{text}\n\n"), StyleClass::UserMessage));
        }
        // A chat message handed to the agent reads as the user's words do:
        // it is what the model was given, shown as it was typed.
        UiBlock::Delivered { message, text } => {
            if text.is_empty() {
                return invisible(kind);
            }
            spans.extend(separator(prev, kind));
            let (fallback, body) = delivered_mail(text);
            let header = label(Label::Delivery(*message));
            if let Some((who, when)) = header.rsplit_once(" · ") {
                spans.push(Span::new(who, StyleClass::MailSpeaker));
                spans.push(Span::new(format!(" · {when}\n"), StyleClass::Time));
            } else if let Some(who) = fallback {
                spans.push(Span::new(format!("{who}\n"), StyleClass::MailSpeaker));
            }
            spans.push(Span::new(format!("{body}\n\n"), StyleClass::Default));
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
        // What an agent is still writing, where its post will land: who,
        // and that it is not posted yet.
        UiBlock::MessageDraft { from, text } => {
            if text.is_empty() {
                return invisible(kind);
            }
            spans.extend(separator(prev, kind));
            spans.push(Span::new(
                format!("{} · writing…\n", label(Label::Agent(*from))),
                StyleClass::AgentLabel,
            ));
            markdown = true;
            let mut text = text.clone();
            if !text.ends_with('\n') {
                text.push('\n');
            }
            spans.push(Span::new(text, StyleClass::Default));
        }
        UiBlock::Tool(tool) => {
            spans.extend(separator(prev, kind));
            // A call is what was run, shown as it was run: no markdown
            // grammar over it, so it keeps a plain buffer of its own.
            inlay = push_tool_spans(&mut spans, tool, now_ms);
        }
        UiBlock::Responded => return invisible(kind),
        UiBlock::Notice { text, times } => {
            if text.is_empty() {
                return invisible(kind);
            }
            spans.extend(separator(prev, kind));
            let mut text = text.trim_end_matches('\n').to_owned();
            if *times > 1 {
                text.push_str(&format!(" ×{times}"));
            }
            text.push('\n');
            spans.push(Span::new(text, StyleClass::SystemInfo));
        }
        UiBlock::Failure {
            kind: failure,
            text,
            times,
            since,
            ..
        } => {
            spans.extend(separator(prev, kind));
            let cluster = if *times == 1 {
                String::new()
            } else {
                format!(" ×{times}")
            };
            let clock = rho_frame::thread::when(since.0 as i64);
            let summary = match failure {
                FailureKind::Retrying { runtime } => {
                    let model = runtime.as_ref().map_or_else(
                        || "its model".to_owned(),
                        |label| format!("its model {label}"),
                    );
                    let problem =
                        if text.contains("connecting to ") || text.contains("Connection refused") {
                            format!("can't reach {model}")
                        } else {
                            format!("{model} did not answer")
                        };
                    format!("{problem} · retrying{cluster} · since {clock}")
                }
                FailureKind::Stopped => {
                    format!("failed: {}{cluster} · since {clock}", preview(text))
                }
            };
            spans.push(Span::new(format!("{summary}\n"), StyleClass::StatusError));
        }
        UiBlock::MailWritten { message, version } => {
            let words = label(Label::Write(*message, *version));
            let line = if words.is_empty() {
                "wrote a message"
            } else {
                &words
            };
            spans.push(Span::new(format!("{line}\n"), StyleClass::SystemInfo));
        }
        UiBlock::Status { text } => {
            if text.is_empty() {
                return invisible(kind);
            }
            spans.extend(separator(prev, kind));
            spans.push(Span::new(text.clone(), StyleClass::SystemInfo));
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

/// Strip the model's envelope, not mail text. The visible header comes
/// from the same mail owner as the thread. The fallbacks cover old copies
/// without a corresponding mail record.
fn delivered_mail(text: &str) -> (Option<&str>, &str) {
    fn numbered(header: &str) -> bool {
        header
            .strip_prefix("[#")
            .and_then(|head| head.strip_suffix(']'))
            .and_then(|head| head.split_once(' '))
            .is_some_and(|(id, _)| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
    }
    let Some((head, body)) = text.split_once('\n') else {
        return (None, text);
    };
    if let Some((where_, sender)) = head.split_once(" Mail from ")
        && numbered(where_)
        && let Some(sender) = sender.strip_suffix(':')
    {
        return (
            Some(if sender == "the user" { "you" } else { sender }),
            body,
        );
    }
    if let Some(sender) = head
        .strip_prefix("[message from ")
        .and_then(|head| head.strip_suffix(']'))
        && let Some((body, where_)) = body.rsplit_once("\n\n")
        && let Some((where_, id)) = where_.split_once(" · mail #")
        && !id.is_empty()
        && id.bytes().all(|b| b.is_ascii_digit())
        && numbered(where_)
    {
        return (Some(sender), body);
    }
    (None, text)
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
    let (source, class) = tool_label(&tool.name, &tool.arguments, tool.format);
    let label = tool_heading(tool, &source);
    spans.push(Span::new(label, class));
    spans.push(Span::new(" · ", StyleClass::ToolDetail));
    let status = tool_status_label(tool.status);
    spans.push(Span::new(status, tool_status_class(tool.status)));

    let mut timer = None;
    if let Some(first) = tool.timing.first_block_at {
        let timing = tool.timing;
        let phases = [
            ("args", Some(first), timing.arguments_finished_at),
            (
                "response",
                timing.arguments_finished_at,
                timing.response_finished_at,
            ),
            ("wait", timing.response_finished_at, timing.boundary_at),
            ("handoff", timing.boundary_at, timing.handed_off_at),
        ];
        for (label, start, end) in phases {
            let Some(start) = start else { continue };
            if let Some(end) = end {
                let value =
                    format_tool_duration(Duration::from_millis(end.0.saturating_sub(start.0)));
                spans.push(Span::new(format!(" {label} {value}"), StyleClass::Time));
            } else if timing.handed_off_at.is_none() && tool.status == UiToolStatus::Running {
                spans.push(Span::new(format!(" {label}"), StyleClass::Time));
                timer = Some(InlaySpec {
                    span_index: spans.len(),
                    content: InlayContent::RunningDuration {
                        started_at_ms: start.0,
                    },
                });
                spans.push(Span::new("", StyleClass::Time));
                break;
            }
        }
    } else if tool.status == UiToolStatus::Running {
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
    if let Some(detail) = tool_detail(tool, &source) {
        spans.push(Span::new(detail, StyleClass::Default));
        spans.push(Span::new("\n", StyleClass::Default));
    }
    timer
}

fn chat_preview(source: &str) -> Option<String> {
    // Preview a literal, not an interpretation of Python.
    let source = source.trim_start();
    if let Some(args) = source.strip_prefix("chat.send(") {
        return literal_argument(args.trim_start());
    }
    let args = source.strip_prefix("mail.send(")?.trim_start();
    let body = args
        .strip_prefix("\"user\"")
        .or_else(|| args.strip_prefix("'user'"))?
        .trim_start()
        .strip_prefix(',')?
        .trim_start();
    literal_argument(body)
}

fn literal_argument(body: &str) -> Option<String> {
    let quote = body.chars().next().filter(|c| matches!(c, '\'' | '"'))?;
    let end = body[1..]
        .char_indices()
        .scan(false, |escape, (at, c)| {
            let terminator = !*escape && c == quote;
            *escape = !*escape && c == '\\';
            Some(terminator.then_some(at + 2))
        })
        .flatten()
        .next()?;
    if !body[end..].trim_start().starts_with([',', ')']) {
        return None;
    }
    let literal = &body[..end];
    if quote == '"' {
        serde_json::from_str::<String>(literal).ok()
    } else {
        Some(
            literal[1..literal.len() - 1]
                .replace("\\'", "'")
                .replace("\\n", "\n"),
        )
    }
}

fn python_shell_command(source: &str) -> Option<String> {
    let source = source.trim_start();
    let args = source
        .strip_prefix("await command(")
        .or_else(|| source.strip_prefix("command("))?;
    literal_argument(args.trim_start())
}

fn status_preview(source: &str) -> Option<String> {
    let args = source.trim_start().strip_prefix("mail.status(")?;
    let (thread, text) = args.split_once(',')?;
    // If it is an expression rather than a bare id, the first comma may
    // belong to that expression. Use the general caption in that case.
    if !thread
        .trim()
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || b"#_'\"".contains(&c))
    {
        return None;
    }
    literal_argument(text.trim_start())
}

// The two APIs share transcript vocabulary, including in old recordings.
fn chat_call(source: &str) -> Option<&str> {
    let source = source.trim_start();
    source
        .strip_prefix("chat.")
        .or_else(|| source.strip_prefix("mail."))
        .and_then(|call| call.split_once('('))
        .map(|(name, _)| name)
}

fn chat_operation(source: &str) -> Option<&'static str> {
    Some(match chat_call(source)? {
        "status" => "set thread status",
        "send" => "send a message",
        "edit" => "edit a message",
        "delete" => "delete a message",
        "read" => "read messages",
        "roots" => "list projects",
        "search" => "search messages",
        "fork" => "start a tangent",
        "start" => "start a thread",
        "join" => "follow a thread",
        "leave" => "leave a thread",
        "name" => "name a thread",
        "move" => "move a message",
        _ => return None,
    })
}

fn tool_heading(tool: &UiTool, source: &str) -> String {
    let native = matches!(tool.name.as_str(), "exec" | "mcp__py__exec");
    if native && let Some(mail) = chat_preview(source) {
        let verb = match tool.status {
            UiToolStatus::Error | UiToolStatus::Cancelled => Some("tried to write to you"),
            UiToolStatus::Running => Some("writing to you"),
            _ => None, // Only the chat log says what was actually written.
        };
        if let Some(verb) = verb {
            return format!("{verb}: {}", preview(&mail));
        }
    }
    if native && let Some(status) = status_preview(source) {
        return if status.is_empty() {
            "cleared the status".to_owned()
        } else {
            format!("set status \"{}\"", preview(&status))
        };
    }
    if native && let Some(action) = chat_operation(source) {
        return action.to_owned();
    }
    if native && let Some(shell) = python_shell_command(source) {
        return format!("$ {shell}");
    }
    if native && source.trim() == "end_turn()" {
        return "ended the turn".into();
    }
    if native {
        return format!("ran code: {}", source.trim_end());
    }
    source.trim_end().to_owned()
}

pub(crate) fn preview(text: &str) -> String {
    let mut lines = text.trim().lines();
    let line = lines.next().unwrap_or_default();
    let mut result = line.chars().take(100).collect::<String>();
    if result.len() < line.len() || lines.next().is_some() {
        result.push('…');
    }
    result
}

fn tool_detail(tool: &UiTool, source: &str) -> Option<String> {
    let mut detail = Vec::new();
    if matches!(tool.name.as_str(), "exec" | "mcp__py__exec")
        && let Some(mail) = chat_preview(source)
        && preview(&mail) != mail
    {
        detail.push(mail);
    }
    if let Some(output) = tool.output.as_deref().or(tool.error.as_deref()) {
        let output = output.trim_end();
        detail.push(if chat_operation(source).is_some() {
            output
                .lines()
                .map(|line| chat_output_line(source, line))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            output.to_owned()
        });
    } else if tool.status == UiToolStatus::Error {
        detail.push("the host recorded no output".into());
    }
    (!detail.is_empty()).then(|| {
        detail
            .join("\n")
            .lines()
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    })
}

fn number(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

fn unnumber(text: &str) -> Option<&str> {
    let (key, text) = text.strip_prefix('#')?.split_once(' ')?;
    number(key).then_some(text)
}

/// A few mail tools print a listing by putting numbers in front of each
/// result. Keep the subject, writer, and excerpt instead of that notation.
fn chat_output_line(source: &str, line: &str) -> String {
    let call = chat_call(source);
    if call == Some("search")
        && let Some((subject, result)) =
            unnumber(line).and_then(|line| line.split_once(" · mail #"))
        && let Some((id, excerpt)) = result.split_once(" · ")
        && number(id)
    {
        return format!("{subject} · {excerpt}");
    }
    if call == Some("roots") {
        return unnumber(line).unwrap_or(line).to_owned();
    }
    if call == Some("read") {
        if let Some(subject) = line
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .and_then(unnumber)
        {
            return subject.to_owned();
        }
        let line_without_arrow = line.strip_prefix("→ ").unwrap_or(line);
        if let Some((writer, body)) =
            unnumber(line_without_arrow).and_then(|line| line.split_once(": "))
        {
            let writer = match writer.rsplit_once(" (reply to #") {
                Some((writer, tail)) if tail.strip_suffix(')').is_some_and(number) => writer,
                _ => writer,
            };
            let label = writer.strip_suffix(" (edited)").unwrap_or(writer);
            if label == "user"
                || label == "system"
                || label.starts_with("eng-")
                || label.starts_with("adv-")
            {
                let writer = writer.replacen("user", "you", usize::from(label == "user"));
                let marker = if line_without_arrow == line {
                    ""
                } else {
                    "→ "
                };
                return format!("{marker}{writer}: {body}");
            }
        }
    }
    chat_receipt(line).to_owned()
}

// These fixed receipts are useful to the model with their machine ids,
// but in the transcript the writing step already points to the actual mail.
fn chat_receipt(line: &str) -> &str {
    for (prefix, words) in [
        ("Sent mail #", "Sent a message."),
        ("Started #", "Started the thread."),
        ("Joined #", "Followed the thread."),
        ("Left #", "Left the thread."),
        ("Named #", "Named the thread."),
    ] {
        if let Some(number) = line
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix('.'))
            && self::number(number)
        {
            return words;
        }
    }
    if let Some((message, thread)) = line
        .strip_prefix("Moved #")
        .and_then(|rest| rest.strip_suffix('.'))
        .and_then(|rest| rest.split_once(" to #"))
        && number(message)
        && number(thread)
    {
        return "Moved the message.";
    }
    if let Some((number, advice)) = line
        .strip_prefix("Mail #")
        .and_then(|rest| rest.split_once(" has no user reply. If it is still live, "))
        && self::number(number)
    {
        if advice == "ask its writer to use `mail.edit` for updates." {
            return "The earlier message is unanswered. If it is still live, ask its writer to update it.";
        }
        if advice
            .strip_prefix("use `mail.edit(")
            .and_then(|rest| rest.strip_suffix(", ...)` for updates."))
            == Some(number)
        {
            return "The earlier message is unanswered. If it is still live, update it instead.";
        }
    }
    line
}

fn tool_status_label(status: UiToolStatus) -> &'static str {
    match status {
        UiToolStatus::Running => "…",
        UiToolStatus::Success => "ok",
        UiToolStatus::Reported => "reported",
        UiToolStatus::Error => "error",
        UiToolStatus::Cancelled => "cancelled",
        UiToolStatus::Ended => "ended",
    }
}

fn tool_status_class(status: UiToolStatus) -> StyleClass {
    match status {
        UiToolStatus::Running => StyleClass::StatusRunning,
        UiToolStatus::Success => StyleClass::StatusOk,
        UiToolStatus::Reported => StyleClass::SystemInfo,
        UiToolStatus::Error => StyleClass::StatusError,
        UiToolStatus::Cancelled => StyleClass::StatusCancelled,
        UiToolStatus::Ended => StyleClass::SystemInfo,
    }
}

/// How long a call ran, or has run while it still runs: a running one
/// counts only once it reaches a second, a finished one says even a moment.
pub fn tool_duration_at(tool: &UiTool, now_ms: u64) -> Option<Duration> {
    let started_at = tool.started_at?.0;
    match tool.finished_at {
        Some(finished_at) => Some(Duration::from_millis(
            finished_at.0.saturating_sub(started_at),
        )),
        None => {
            let duration = Duration::from_millis(now_ms.saturating_sub(started_at));
            (tool.status == UiToolStatus::Running && Duration::from_secs(1) <= duration)
                .then_some(duration)
        }
    }
}

pub fn format_tool_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds == 0 {
        format!("{}ms", duration.as_millis())
    } else if seconds < 60 {
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
/// `$ command`. Code-mode cells (the native `exec` tool and Claude Code's
/// `mcp__py__exec`) render as the source itself, with no tool name in front:
/// the code is what the reader came for, and "exec" says nothing it does not.
/// Claude's file tools render as `read/write/edit path` so the transcript
/// shows the touched file instead of raw JSON arguments. Argument extraction
/// tolerates the partial JSON seen while arguments stream.
pub(crate) fn tool_label(
    name: &str,
    arguments: &str,
    format: ArgumentsFormat,
) -> (String, StyleClass) {
    match name {
        "shell" | "shell_command" | "Bash" => {
            let command = shell_command_argument_label(arguments, format);
            let label = if command.is_empty() {
                "$".to_owned()
            } else {
                format!("$ {command}")
            };
            (label, StyleClass::ToolShell)
        }
        "exec" | "mcp__py__exec" => (exec_source_label(arguments, format), StyleClass::ToolShell),
        "Read" | "Write" | "Edit" => {
            let verb = name.to_ascii_lowercase();
            let label = match json_text_field(arguments, "file_path", format) {
                Some(path) if !path.is_empty() => format!("{verb} {path}"),
                _ => verb,
            };
            (label, StyleClass::ToolName)
        }
        _ if arguments.is_empty() => (name.to_owned(), StyleClass::ToolName),
        _ => (format!("{name} {arguments}"), StyleClass::ToolName),
    }
}

/// The source of a code-mode cell. The native `exec` tool sends the source
/// as its whole argument string; Claude Code's `mcp__py__exec` wraps it as
/// `{"source": ...}`. Trailing blank lines are dropped so the status marker
/// lands after the last line of code rather than on a line of its own.
fn exec_source_label(arguments: &str, format: ArgumentsFormat) -> String {
    let source = match format {
        ArgumentsFormat::Text => arguments.to_owned(),
        ArgumentsFormat::Json => json_text_field(arguments, "source", format).unwrap_or_default(),
    };
    source.trim_end().to_owned()
}

fn shell_command_argument_label(arguments: &str, format: ArgumentsFormat) -> String {
    match format {
        ArgumentsFormat::Text => arguments.to_owned(),
        ArgumentsFormat::Json => json_text_field(arguments, "command", format).unwrap_or_default(),
    }
}

fn json_text_field(arguments: &str, key: &str, format: ArgumentsFormat) -> Option<String> {
    // Text arguments are what the model wrote, not an object with fields in
    // it. Parsing them would be wrong as well as wasted.
    if format == ArgumentsFormat::Text || !arguments.trim_start().starts_with('{') {
        return None;
    }

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
    use rho_agent_types::UnixMs;

    use super::*;

    fn tool(status: UiToolStatus) -> UiTool {
        UiTool {
            timing: Default::default(),
            id: "tool-1".to_owned(),
            name: "shell_command".to_owned(),
            arguments: "echo ok".to_owned(),
            format: ArgumentsFormat::Text,
            preview: None,
            status,
            output: None,
            error: None,
            started_at: None,
            finished_at: None,
        }
    }

    fn text_of(spans: &[Span]) -> String {
        spans.iter().map(|span| span.text.as_str()).collect()
    }

    /// A shell call reads its command out of the arguments the fold keeps
    /// whole.
    #[test]
    fn a_shell_call_labels_with_its_command() {
        let (label, _) = tool_label(
            "shell_command",
            r#"{"command":"cargo build"}"#,
            ArgumentsFormat::Json,
        );
        assert_eq!(label, "$ cargo build");
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
        let json = ArgumentsFormat::Json;
        assert_eq!(
            shell_command_argument_label(r#"{"command":"echo"#, json),
            "echo"
        );
        assert_eq!(shell_command_argument_label(r#"{"comm"#, json), "");
        // Raw text is what `Text` is for; a `Json` tool with no object in it
        // has no command to show.
        assert_eq!(shell_command_argument_label("echo ok", json), "");
        assert_eq!(
            shell_command_argument_label("echo ok", ArgumentsFormat::Text),
            "echo ok"
        );
    }

    /// The point of the format: a text tool's arguments are never offered to
    /// the JSON parser, so source that merely looks like an object is shown
    /// as the code it is instead of being read for fields it does not have.
    #[test]
    fn a_text_tool_is_never_parsed_as_json() {
        let source = "{\"source\": 1}\nprint(2)\n";
        assert_eq!(
            tool_label("exec", source, ArgumentsFormat::Text),
            (
                "{\"source\": 1}\nprint(2)".to_owned(),
                StyleClass::ToolShell
            )
        );
        assert_eq!(
            shell_command_argument_label("{ echo ok; }", ArgumentsFormat::Text),
            "{ echo ok; }"
        );
    }

    #[test]
    fn claude_bash_renders_as_shell_prompt() {
        assert_eq!(
            tool_label(
                "Bash",
                r#"{"command":"cargo test","description":"Run tests"}"#,
                ArgumentsFormat::Json,
            ),
            ("$ cargo test".to_owned(), StyleClass::ToolShell)
        );
        // Streaming partial JSON still resolves the command field.
        assert_eq!(
            tool_label("Bash", r#"{"command":"cargo te"#, ArgumentsFormat::Json),
            ("$ cargo te".to_owned(), StyleClass::ToolShell)
        );
        assert_eq!(
            tool_label("Bash", r#"{"desc"#, ArgumentsFormat::Json),
            ("$".to_owned(), StyleClass::ToolShell)
        );
    }

    #[test]
    fn native_exec_renders_its_source_without_the_tool_name() {
        let code = "x = 1\nprint(x)\n";
        assert_eq!(
            tool_label("exec", code, ArgumentsFormat::Text),
            ("x = 1\nprint(x)".to_owned(), StyleClass::ToolShell)
        );
        assert_eq!(
            tool_label("exec", "", ArgumentsFormat::Text),
            (String::new(), StyleClass::ToolShell)
        );
    }

    #[test]
    fn claude_code_py_exec_renders_its_source_without_the_tool_name() {
        assert_eq!(
            tool_label(
                "mcp__py__exec",
                r#"{"source":"print(1)\n"}"#,
                ArgumentsFormat::Json
            ),
            ("print(1)".to_owned(), StyleClass::ToolShell)
        );
        // Streaming partial JSON still resolves the source field.
        assert_eq!(
            tool_label("mcp__py__exec", r#"{"source":"comm"#, ArgumentsFormat::Json),
            ("comm".to_owned(), StyleClass::ToolShell)
        );
        assert_eq!(
            tool_label("mcp__py__exec", r#"{"sou"#, ArgumentsFormat::Json),
            (String::new(), StyleClass::ToolShell)
        );
    }

    #[test]
    fn claude_file_tools_render_verb_and_path() {
        assert_eq!(
            tool_label(
                "Read",
                r#"{"file_path":"/tmp/a.rs","limit":40}"#,
                ArgumentsFormat::Json
            ),
            ("read /tmp/a.rs".to_owned(), StyleClass::ToolName)
        );
        assert_eq!(
            tool_label(
                "Edit",
                r#"{"file_path":"/tmp/a.rs","old_string":"a","new_string":"b"}"#,
                ArgumentsFormat::Json,
            ),
            ("edit /tmp/a.rs".to_owned(), StyleClass::ToolName)
        );
        assert_eq!(
            tool_label("Write", r#"{"file_p"#, ArgumentsFormat::Json),
            ("write".to_owned(), StyleClass::ToolName)
        );
    }

    #[test]
    fn a_host_draft_and_its_step_both_show_in_user_words() {
        let mut exec = tool(UiToolStatus::Running);
        exec.name = "exec".into();
        exec.arguments = "mail.send('user', 'Hello', thread=2, ping=False)".into();
        let source = UiBlock::Tool(exec);
        let writer = AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap();
        let draft = UiBlock::MessageDraft {
            from: writer,
            text: "Hello".into(),
        };
        let label = |name: Label| {
            if name == Label::Agent(writer) {
                "Ann"
            } else {
                "Bob"
            }
            .to_owned()
        };
        assert!(block_visible(&source));
        assert!(block_visible(&draft));
        let drafted = render(&draft, None, 0, &label);
        assert_eq!(text_of(&drafted.spans), "Ann · writing…\nHello\n");
        assert!(drafted.markdown);
        assert_eq!(drafted.spans[0].class, StyleClass::AgentLabel);
        assert_eq!(drafted.spans[1].class, StyleClass::Default);
        let sourced = render(&source, None, 0, &label);
        assert_eq!(text_of(&sourced.spans), "writing to you: Hello · …\n");

        let mut exec = tool(UiToolStatus::Running);
        exec.name = "exec".into();
        let source = r#"chat.send("Checking the clock.", "flash", thread=2)"#;
        assert_eq!(
            tool_heading(&exec, source),
            "writing to you: Checking the clock."
        );
        assert_eq!(
            tool_heading(&exec, "chat.send(result, kind, thread=2)"),
            "send a message"
        );
        assert_eq!(
            tool_heading(&exec, "mail.send(agent, text, thread=2)"),
            "send a message"
        );
    }

    #[test]
    fn delivered_mail_uses_a_message_header_and_keeps_the_users_words() {
        let message = MessageId(rho_agent_types::ChatSeq(31));
        let block = UiBlock::Delivered {
            message,
            text:
                "[#219 tide › Check the delayed timer] Mail from the user:\nSearch #31 in my tests."
                    .into(),
        };
        let shown = render(&block, None, 0, &|label| {
            assert_eq!(label, Label::Delivery(message));
            "you · 12:22".into()
        });
        assert_eq!(
            text_of(&shown.spans),
            "you · 12:22\nSearch #31 in my tests.\n\n"
        );
        assert_eq!(shown.spans[0].class, StyleClass::MailSpeaker);
        assert_eq!(shown.spans[1].class, StyleClass::Time);
        assert_eq!(shown.spans[2].class, StyleClass::Default);
        assert_eq!(
            delivered_mail(
                "[message from eng-source]\nTiming note\n\n[#31 tide › Slow clock] · mail #46"
            ),
            (Some("eng-source"), "Timing note")
        );
        assert_eq!(
            delivered_mail("[#12] is my code's literal\nkeep it"),
            (None, "[#12] is my code's literal\nkeep it")
        );
    }

    #[test]
    fn a_write_keeps_its_point_but_displays_words_not_its_key() {
        let id = MessageId(rho_agent_types::ChatSeq(97));
        let version = Some(ChatSeq(213));
        let block = UiBlock::MailWritten {
            message: id,
            version,
        };
        let render_words = |label| match label {
            Label::Write(id_, version_) if (id_, version_) == (id, version) => {
                "wrote to you: Run the slower clock test.".into()
            }
            _ => panic!("the writing point has a different identity"),
        };
        assert_eq!(
            text_of(&render(&block, None, 0, &render_words).spans),
            "wrote to you: Run the slower clock test.\n"
        );
        assert_eq!(
            text_of(&render(&block, None, 0, &|_| String::new()).spans),
            "wrote a message\n"
        );
    }

    #[test]
    fn long_shell_and_code_inputs_keep_their_last_token_and_lines() {
        let mut exec = tool(UiToolStatus::Success);
        exec.name = "exec".into();
        let shell = format!("printf '%s' {}", "x".repeat(150));
        let source = format!("command({shell:?})");
        assert_eq!(tool_heading(&exec, &source), format!("$ {shell}"));
        assert!(tool_detail(&exec, &source).is_none());

        let source = format!("{}\nsecond_call()", "long_call(".repeat(12));
        assert_eq!(tool_heading(&exec, &source), format!("ran code: {source}"));
    }

    #[test]
    fn a_status_step_describes_the_action_without_printing_machine_arguments() {
        let mut exec = tool(UiToolStatus::Success);
        exec.name = "exec".into();
        exec.format = ArgumentsFormat::Text;
        exec.arguments = "mail.status(273, 'Waiting for clarification')\nend_turn()".into();
        let mut spans = Vec::new();
        push_tool_spans(&mut spans, &exec, 300);
        assert_eq!(
            text_of(&spans),
            "set status \"Waiting for clarification\" · ok\n"
        );

        exec.arguments = "mail.status(pick_thread(a, b), pending_status)\nend_turn()".into();
        let mut spans = Vec::new();
        push_tool_spans(&mut spans, &exec, 300);
        assert_eq!(text_of(&spans), "set thread status · ok\n");
    }

    #[test]
    fn chat_receipts_leave_out_machine_keys_and_keep_other_output() {
        let mut exec = tool(UiToolStatus::Success);
        exec.name = "exec".into();
        exec.format = ArgumentsFormat::Text;
        exec.arguments = "mail.status(273, 'waiting')\nmail.send('user', reply)".into();
        exec.output = Some(concat!(
            "Sent mail #897.\n",
            "Mail #273 has no user reply. If it is still live, use `mail.edit(273, ...)` for updates.\n",
            "Check #273 in my tests."
        ).into());
        let source = exec.arguments.as_str();
        assert_eq!(
            tool_detail(&exec, source).as_deref(),
            Some(concat!(
                "  Sent a message.\n",
                "  The earlier message is unanswered. If it is still live, update it instead.\n",
                "  Check #273 in my tests."
            ))
        );
        assert_eq!(
            chat_receipt(
                "Mail #16 has no user reply. If it is still live, ask its writer to use `mail.edit` for updates."
            ),
            "The earlier message is unanswered. If it is still live, ask its writer to update it."
        );
        assert_eq!(chat_receipt("Moved #976 to #2."), "Moved the message.");
        assert_eq!(chat_receipt("Sent mail #unknown."), "Sent mail #unknown.");
    }

    #[test]
    fn structured_chat_search_keeps_subjects_and_excerpts_not_tool_keys() {
        for api in ["chat", "mail"] {
            assert_eq!(
                chat_output_line(
                    &format!("{api}.search('race')"),
                    "#14 Race on clock · mail #77 · Check #31 in the fixture"
                ),
                "Race on clock · Check #31 in the fixture"
            );
            assert_eq!(
                chat_output_line(
                    &format!("{api}.read(14)"),
                    "→ #77 user (reply to #97): Keep #31 in the test"
                ),
                "→ you: Keep #31 in the test"
            );
            assert_eq!(
                chat_output_line(&format!("{api}.read(14)"), "#31 in the human continuation"),
                "#31 in the human continuation"
            );
        }
    }

    #[test]
    fn retry_noise_uses_the_runtime_and_the_start_of_the_streak() {
        use rho_agent_types::UnixMs;
        let time = chrono::Local::now().timestamp_millis() as u64;
        let retry = UiBlock::Failure {
            kind: FailureKind::Retrying {
                runtime: Some("sol-low".into()),
            },
            text:
                "connecting to the Responses endpoint: IO error: Connection refused (os error 111)"
                    .into(),
            times: 12,
            since: UnixMs(time - 300_000),
            at: UnixMs(time),
        };
        let original_clock = rho_frame::thread::when((time - 300_000) as i64);
        let shown = render(&retry, None, time, &|_| String::new());
        assert_eq!(
            text_of(&shown.spans),
            format!("can't reach its model sol-low · retrying ×12 · since {original_clock}\n"),
        );
        assert_eq!(shown.spans[0].class, StyleClass::StatusError);
    }

    #[test]
    fn a_literal_notebook_shell_command_reads_as_a_shell_prompt() {
        let mut exec = tool(UiToolStatus::Success);
        exec.name = "exec".into();
        exec.format = ArgumentsFormat::Text;
        exec.arguments = r#"await command("printf '%s' \"the clock\"")"#.into();
        exec.output = Some("the clock".into());
        let mut spans = Vec::new();
        push_tool_spans(&mut spans, &exec, 3_500);
        assert_eq!(
            text_of(&spans),
            "$ printf '%s' \"the clock\" · ok\n  the clock\n"
        );
        assert_eq!(
            python_shell_command("await command('test ' + filename)"),
            None
        );
    }

    #[test]
    fn a_failed_command_keeps_its_output_with_its_heading() {
        let mut failed = tool(UiToolStatus::Error);
        failed.output = Some("500 samples\nFAIL: clock jumped backward\n".to_owned());
        let mut spans = Vec::new();
        push_tool_spans(&mut spans, &failed, 7_000);
        assert_eq!(
            text_of(&spans),
            "$ echo ok · error\n  500 samples\n  FAIL: clock jumped backward\n"
        );
    }

    #[test]
    fn notebook_report_is_not_a_success_or_completion_duration() {
        let mut reported = tool(UiToolStatus::Reported);
        reported.started_at = Some(UnixMs(1_000));
        let mut spans = Vec::new();
        assert_eq!(push_tool_spans(&mut spans, &reported, 10_000), None);
        assert_eq!(text_of(&spans), "$ echo ok · reported\n");
    }

    #[test]
    fn tool_spans_render_status_and_duration() {
        let mut finished = tool(UiToolStatus::Success);
        finished.started_at = Some(UnixMs(1_000));
        finished.finished_at = Some(UnixMs(3_500));
        let mut spans = Vec::new();
        let timer = push_tool_spans(&mut spans, &finished, 10_000);
        assert_eq!(timer, None);
        assert_eq!(text_of(&spans), "$ echo ok · ok 2s\n");
    }

    #[test]
    fn a_running_tool_counts_from_a_second_and_a_finished_one_says_a_moment() {
        let mut running = tool(UiToolStatus::Running);
        running.started_at = Some(UnixMs(1_000));
        assert_eq!(tool_duration_at(&running, 1_999), None);
        assert_eq!(
            tool_duration_at(&running, 2_000),
            Some(Duration::from_secs(1))
        );

        let mut finished = tool(UiToolStatus::Error);
        finished.started_at = Some(UnixMs(1_000));
        finished.finished_at = Some(UnixMs(1_240));
        let mut spans = Vec::new();
        assert_eq!(push_tool_spans(&mut spans, &finished, 10_000), None);
        assert_eq!(
            text_of(&spans),
            "$ echo ok · error 240ms\n  the host recorded no output\n"
        );
    }

    #[test]
    fn running_tool_gets_a_timer_position_marker_not_text() {
        let mut running = tool(UiToolStatus::Running);
        running.started_at = Some(UnixMs(1_000));
        let mut spans = Vec::new();
        let timer = push_tool_spans(&mut spans, &running, 3_500);
        let timer = timer.expect("running tool with start time should have a timer");
        assert_eq!(spans[timer.span_index].text, "");
        assert_eq!(text_of(&spans), "$ echo ok · …\n");
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

    /// Composition is lazy, so visibility is answered from the block
    /// rather than from spans that may never be rendered. The two answers
    /// are the same answer, for every kind of block there is.
    #[test]
    fn block_visibility_agrees_with_rendering() {
        let blocks = [
            UiBlock::UserMessage {
                text: "said".to_owned(),
            },
            UiBlock::UserMessage {
                text: String::new(),
            },
            UiBlock::AssistantMessage {
                text: "answered".to_owned(),
                phase: None,
            },
            UiBlock::AssistantMessage {
                text: String::new(),
                phase: None,
            },
            UiBlock::Reasoning {
                text: "thought".to_owned(),
            },
            UiBlock::Tool(tool(UiToolStatus::Running)),
            UiBlock::Notice {
                text: "notice".to_owned(),
                times: 1,
            },
            UiBlock::Notice {
                text: String::new(),
                times: 1,
            },
            UiBlock::Delivered {
                message: rho_agent_types::MessageId(rho_agent_types::ChatSeq(1)),
                text: "handed".to_owned(),
            },
            UiBlock::Delivered {
                message: rho_agent_types::MessageId(rho_agent_types::ChatSeq(1)),
                text: String::new(),
            },
        ];
        for block in blocks {
            let rendered = render_labelled(&block, None, 0, &|_| "a".to_owned());
            assert_eq!(
                block_visible(&block),
                rendered.visible(),
                "visibility of {block:?}"
            );
        }
    }
    #[test]
    fn a_notice_said_again_reads_once_with_its_count() {
        let notice = |times| UiBlock::Notice {
            text: "The server is overloaded".to_owned(),
            times,
        };
        let text =
            |block: &UiBlock| text_of(&render_labelled(block, None, 0, &|_| "a".to_owned()).spans);
        assert_eq!(text(&notice(1)), "The server is overloaded\n");
        assert_eq!(text(&notice(3)), "The server is overloaded ×3\n");
    }

    #[test]
    fn exec_timing_names_provider_phases_not_python_duration() {
        let mut exec = tool(UiToolStatus::Success);
        exec.timing = rho_agent_types::ExecTiming {
            first_block_at: Some(UnixMs(100)),
            arguments_finished_at: Some(UnixMs(2100)),
            response_finished_at: Some(UnixMs(2600)),
            boundary_at: Some(UnixMs(5600)),
            handed_off_at: Some(UnixMs(5620)),
        };
        // These historical fields must not win over observed provider facts.
        exec.started_at = Some(UnixMs(0));
        exec.finished_at = Some(UnixMs(999999));
        let mut spans = Vec::new();
        assert_eq!(push_tool_spans(&mut spans, &exec, 999999), None);
        let text = text_of(&spans);
        assert!(
            text.contains("args 2s response 500ms wait 3s handoff 20ms"),
            "{text}"
        );
        assert!(!text.contains("16m"));
    }
}
