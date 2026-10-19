//! What each block of an agent's transcript shows in the terminal, in the
//! chat view and in the activity view.
//!
//! The GUI makes the same choices in `rho-agents-view`'s `render.rs`; this
//! is a copy adapted to terminal cells, kept apart so the GUI is free to
//! change its own.

use std::time::Duration;

use rho_agent_types::AgentId;
use rho_agents_client::protocol::transcript::ArgumentsFormat;
use rho_agents_client::state::{UiBlock, UiMessagePhase, UiTool, UiToolStatus};
use rho_term_ui::{Color, Span, Style, StyledBlock, StyledText};

/// Which projection of the transcript the screen shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum View {
    /// What was said: messages both ways, notices.
    #[default]
    Chat,
    /// What was done: every tool call and notice.
    Activity,
}

impl View {
    pub fn toggled(self) -> Self {
        match self {
            Self::Chat => Self::Activity,
            Self::Activity => Self::Chat,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Activity => "activity",
        }
    }

    /// Whether a block shows in this view at all.
    pub fn visible(self, block: &UiBlock) -> bool {
        let belongs = match self {
            Self::Chat => !matches!(block, UiBlock::Tool(_) | UiBlock::Reasoning { .. }),
            Self::Activity => matches!(block, UiBlock::Tool(_) | UiBlock::Notice { .. }),
        };
        belongs && has_text(block)
    }
}

/// Coarse classification for the blank line between turns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    User,
    Response,
}

pub fn kind(block: &UiBlock) -> Kind {
    match block {
        UiBlock::UserMessage { .. }
        | UiBlock::AgentMessage { .. }
        | UiBlock::QueuedMessage { .. }
        | UiBlock::Unsent { .. }
        | UiBlock::Status { .. } => Kind::User,
        _ => Kind::Response,
    }
}

fn has_text(block: &UiBlock) -> bool {
    match block {
        UiBlock::Reasoning { .. } => false,
        UiBlock::Tool(_) => true,
        UiBlock::UserMessage { text }
        | UiBlock::AssistantMessage { text, .. }
        | UiBlock::MessageSent { text, .. }
        | UiBlock::MessageDraft { text }
        | UiBlock::Notice { text }
        | UiBlock::AgentMessage { text, .. }
        | UiBlock::QueuedMessage { text, .. }
        | UiBlock::Unsent { text }
        | UiBlock::Status { text } => !text.trim().is_empty(),
    }
}

/// How a few things look. Named colors only, so the terminal's own theme
/// decides what they are.
fn user() -> Style {
    Style::default().fg(Color::Cyan).bold()
}
fn dim() -> Style {
    Style::default().fg(Color::DarkGrey)
}
fn label() -> Style {
    Style::default().fg(Color::Magenta)
}
fn code() -> Style {
    Style::default().fg(Color::Yellow)
}

/// Lines of a code cell shown in the activity view before the rest is
/// counted instead, and how much of each line.
const MAX_SOURCE_LINES: usize = 8;
const MAX_SOURCE_LINE_CHARS: usize = 200;

/// One block as the view shows it, or `None` when the view hides it.
/// `previous` is the kind of the last block the view showed before it: a
/// user's message after a response opens a new turn with a blank line.
pub fn render(
    view: View,
    block: &UiBlock,
    previous: Option<Kind>,
    now_ms: u64,
    agent_label: &dyn Fn(AgentId) -> String,
) -> Option<StyledBlock> {
    if !view.visible(block) {
        return None;
    }
    let mut spans = Vec::new();
    if previous == Some(Kind::Response) && kind(block) == Kind::User {
        spans.push(Span::plain("\n"));
    }
    match block {
        UiBlock::UserMessage { text } => {
            // The user's own words, as typed: not Markdown.
            spans.push(Span::new("› ", user()));
            spans.push(Span::new(text.trim_end(), user()));
            spans.push(Span::plain("\n"));
        }
        UiBlock::AssistantMessage { text, phase } => {
            let style = if *phase == Some(UiMessagePhase::Commentary) {
                dim()
            } else {
                Style::default()
            };
            push_markdown(&mut spans, text, style);
        }
        UiBlock::MessageSent { to, text } => {
            if let Some(to) = to {
                spans.push(Span::new(format!("to {}\n", agent_label(*to)), label()));
            }
            push_markdown(&mut spans, text, Style::default());
        }
        // The host's tentative text for a send still being written: in the
        // chat it reads as the message it will be.
        UiBlock::MessageDraft { text } => push_markdown(&mut spans, text, Style::default()),
        UiBlock::Tool(tool) => push_tool(&mut spans, tool, now_ms),
        UiBlock::Notice { text } => {
            spans.push(Span::new(text.trim_end(), dim()));
            spans.push(Span::plain("\n"));
        }
        UiBlock::AgentMessage { sender, text } => {
            spans.push(Span::new(
                format!("from {}\n", agent_label(*sender)),
                label(),
            ));
            spans.push(Span::new(
                text.trim_end(),
                Style::default().fg(Color::Magenta),
            ));
            spans.push(Span::plain("\n"));
        }
        UiBlock::QueuedMessage { text, sender } => {
            if let Some(sender) = sender {
                spans.push(Span::new(
                    format!("from {}\n", agent_label(*sender)),
                    label(),
                ));
            } else {
                spans.push(Span::new("› ", user()));
            }
            spans.push(Span::new(text.trim_end(), Style::default()));
            spans.push(Span::new(" (queued)\n", dim()));
        }
        UiBlock::Unsent { text } => {
            spans.push(Span::new("› ", user()));
            spans.push(Span::new(text.trim_end(), Style::default()));
            spans.push(Span::new(" (unsent)\n", dim()));
        }
        UiBlock::Status { text } => {
            spans.push(Span::new(text.trim_end(), dim()));
            spans.push(Span::plain("\n"));
        }
        UiBlock::Reasoning { .. } => return None,
    }
    // A block's rows end where its text does: the trailing newline is the
    // separator between blocks, not a blank row inside one.
    if let Some(last) = spans.last_mut()
        && last.text.ends_with('\n')
    {
        last.text.pop();
    }
    Some(StyledBlock::new(StyledText::from(spans)))
}

/// Model prose, lightly styled: fenced code and headings stand out and the
/// rest is left as written, markers and all, so nothing is lost if the
/// guess is wrong.
fn push_markdown(spans: &mut Vec<Span>, text: &str, base: Style) {
    let mut in_fence = false;
    for line in text.trim_end().split('\n') {
        let trimmed = line.trim_start();
        let fence = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        let style = if fence || in_fence {
            code()
        } else if trimmed.starts_with('#') {
            Style { bold: true, ..base }
        } else {
            base
        };
        if fence {
            in_fence = !in_fence;
        }
        if in_fence || fence {
            spans.push(Span::new(line, style));
        } else {
            push_inline_code(spans, line, style);
        }
        spans.push(Span::plain("\n"));
    }
}

/// `code` spans in a line of prose, backticks kept.
fn push_inline_code(spans: &mut Vec<Span>, line: &str, base: Style) {
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        let Some(close) = rest[open + 1..].find('`') else {
            break;
        };
        let close = open + 1 + close;
        if open > 0 {
            spans.push(Span::new(&rest[..open], base));
        }
        spans.push(Span::new(&rest[open..=close], code()));
        rest = &rest[close + 1..];
    }
    if !rest.is_empty() {
        spans.push(Span::new(rest, base));
    }
}

/// One call: what was run, how it ended, and how long it took.
fn push_tool(spans: &mut Vec<Span>, tool: &UiTool, now_ms: u64) {
    let (label, shell) = tool_label(&tool.name, &tool.arguments, tool.format);
    let label_style = if shell {
        Style::default().fg(Color::Blue)
    } else {
        Style::default().fg(Color::Blue).bold()
    };
    let lines = label.lines().collect::<Vec<_>>();
    let shown = lines.len().min(MAX_SOURCE_LINES);
    let cut = lines[..shown]
        .iter()
        .map(
            |line| match line.char_indices().nth(MAX_SOURCE_LINE_CHARS) {
                Some((at, _)) => format!("{}…", &line[..at]),
                None => (*line).to_owned(),
            },
        )
        .collect::<Vec<_>>();
    spans.push(Span::new(cut.join("\n"), label_style));
    if lines.len() > shown {
        spans.push(Span::new(
            format!("\n… {} more lines", lines.len() - shown),
            dim(),
        ));
    }
    let (status, color) = match tool.status {
        UiToolStatus::Running => ("…", Color::Yellow),
        UiToolStatus::Success => ("ok", Color::Green),
        UiToolStatus::Reported => ("reported", Color::DarkGrey),
        UiToolStatus::Error => ("error", Color::Red),
        UiToolStatus::Cancelled => ("cancelled", Color::DarkYellow),
    };
    spans.push(Span::plain(" "));
    spans.push(Span::new(status, Style::default().fg(color)));
    if let Some(duration) = tool_duration(tool, now_ms) {
        spans.push(Span::new(format!(" {}", format_duration(duration)), dim()));
    }
    if let Some(error) = tool.error.as_deref().filter(|error| !error.is_empty()) {
        spans.push(Span::new(
            format!("\n  {}", first_line(error)),
            Style::default().fg(Color::Red),
        ));
    }
    spans.push(Span::plain("\n"));
}

fn first_line(text: &str) -> &str {
    text.trim().lines().next().unwrap_or_default()
}

/// How long a call ran, or has run so far. Under a second says nothing.
pub fn tool_duration(tool: &UiTool, now_ms: u64) -> Option<Duration> {
    let started_at = tool.started_at?.0;
    let finished_at = tool
        .finished_at
        .map(|finished_at| finished_at.0)
        .or_else(|| (tool.status == UiToolStatus::Running).then_some(now_ms))?;
    let duration = Duration::from_millis(finished_at.saturating_sub(started_at));
    (Duration::from_secs(1) <= duration).then_some(duration)
}

pub fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{}s", seconds / 60, seconds % 60)
    }
}

/// A call's label, and whether it is code or a command (shown as run)
/// rather than a named tool.
///
/// Shell tools read `$ command`. Code cells (`exec`, Claude Code's
/// `mcp__py__exec`) read as their source. Claude's file tools read as the
/// verb and the path. Fields are read from partial JSON too, since
/// arguments stream.
pub fn tool_label(name: &str, arguments: &str, format: ArgumentsFormat) -> (String, bool) {
    match name {
        "shell" | "shell_command" | "Bash" => {
            let command = match format {
                ArgumentsFormat::Text => arguments.to_owned(),
                ArgumentsFormat::Json => {
                    json_text_field(arguments, "command", format).unwrap_or_default()
                }
            };
            (format!("$ {command}").trim_end().to_owned(), true)
        }
        "exec" | "mcp__py__exec" => {
            let source = match format {
                ArgumentsFormat::Text => arguments.to_owned(),
                ArgumentsFormat::Json => {
                    json_text_field(arguments, "source", format).unwrap_or_default()
                }
            };
            (source.trim_end().to_owned(), true)
        }
        "Read" | "Write" | "Edit" => {
            let verb = name.to_ascii_lowercase();
            let label = match json_text_field(arguments, "file_path", format) {
                Some(path) if !path.is_empty() => format!("{verb} {path}"),
                _ => verb,
            };
            (label, false)
        }
        _ if arguments.is_empty() => (name.to_owned(), false),
        _ => (format!("{name} {arguments}"), false),
    }
}

fn json_text_field(arguments: &str, key: &str, format: ArgumentsFormat) -> Option<String> {
    // Text arguments are what the model wrote, not an object.
    if format == ArgumentsFormat::Text || !arguments.trim_start().starts_with('{') {
        return None;
    }
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
