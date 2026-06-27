use rho_cli_term_raw::{Color, Span, Style, StyledBlock, StyledText};
use rho_core::ToolOutputStatus;

#[derive(Clone, Copy)]
pub(crate) enum ToolRenderStatus {
    Running,
    Done(ToolOutputStatus),
}

pub(crate) fn tool_call_block(
    name: &str,
    arguments: &str,
    status: ToolRenderStatus,
) -> StyledBlock {
    let mut spans = vec![
        Span::new("tool ", Style::default().fg(Color::DarkMagenta).bold()),
        Span::new(name.to_owned(), Style::default().fg(Color::DarkMagenta)),
    ];
    if let Some(summary) = tool_argument_summary(name, arguments) {
        spans.push(Span::new(" ", Style::default().fg(Color::DarkGrey)));
        spans.push(Span::plain(summary));
    }
    match status {
        ToolRenderStatus::Running => spans.push(Span::new(
            " running 0s",
            Style::default().fg(Color::DarkYellow),
        )),
        ToolRenderStatus::Done(status) => spans.push(Span::new(
            format!(" {} 0s", tool_status_label(&status)),
            Style::default().fg(tool_status_color(&status)),
        )),
    }
    StyledBlock::new(StyledText::from(spans))
}

pub(crate) fn tool_result_block(
    name: &str,
    arguments: &str,
    status: ToolOutputStatus,
    output: &str,
) -> StyledBlock {
    let mut block = tool_call_block(name, arguments, ToolRenderStatus::Done(status));
    block.content.push(Span::plain("\n"));
    block.content.push(Span::plain(output.to_owned()));
    block
}

pub(crate) fn tool_status_label(status: &ToolOutputStatus) -> String {
    match status {
        ToolOutputStatus::Success => "success".to_owned(),
        ToolOutputStatus::Error => "error".to_owned(),
        ToolOutputStatus::Cancelled => "cancelled".to_owned(),
    }
}

pub(crate) fn tool_status_color(status: &ToolOutputStatus) -> Color {
    match status {
        ToolOutputStatus::Success => Color::Green,
        ToolOutputStatus::Error => Color::DarkRed,
        ToolOutputStatus::Cancelled => Color::DarkYellow,
    }
}

fn tool_argument_summary(name: &str, arguments: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    match name {
        "shell_command" => value
            .get("command")
            .and_then(|command| command.as_str())
            .map(|command| truncate_inline(command, 96)),
        _ => Some(truncate_inline(arguments, 96)),
    }
}

fn truncate_inline(text: &str, max_chars: usize) -> String {
    let text = text.replace('\n', " ");
    if text.chars().count() <= max_chars {
        return text;
    }
    let mut output = text.chars().take(max_chars).collect::<String>();
    output.push('…');
    output
}
