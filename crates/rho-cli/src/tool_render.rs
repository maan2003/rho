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
    if name == "apply_patch" {
        return apply_patch_block();
    }

    if matches!(name, "exec_command" | "write_stdin" | "shell_command") {
        return shell_command_block(arguments, status);
    }

    let mut spans = vec![
        Span::new("tool ", Style::default().fg(Color::DarkMagenta).bold()),
        Span::new(name.to_owned(), Style::default().fg(Color::DarkMagenta)),
    ];
    if let Some(summary) = tool_argument_summary(name, arguments) {
        spans.push(Span::new(" ", Style::default().fg(Color::DarkGrey)));
        spans.push(Span::plain(summary));
    }
    match status {
        ToolRenderStatus::Running => {
            spans.push(Span::new(" …", Style::default().fg(Color::DarkYellow)))
        }
        ToolRenderStatus::Done(ToolOutputStatus::Success) => {}
        ToolRenderStatus::Done(ToolOutputStatus::Error) => {
            spans.push(Span::new(" x", Style::default().fg(Color::Red)))
        }
        ToolRenderStatus::Done(status) => spans.push(Span::new(
            format!(" {}", tool_status_label(&status)),
            Style::default().fg(tool_status_color(&status)),
        )),
    }
    StyledBlock::new(StyledText::from(spans))
}

fn apply_patch_block() -> StyledBlock {
    StyledBlock::new(Span::new("edit", Style::default().fg(Color::DarkGreen)))
}

fn shell_command_block(arguments: &str, status: ToolRenderStatus) -> StyledBlock {
    let prompt_style = Style::default().fg(Color::DarkGreen);
    let command_style = Style::default();
    let mut spans = vec![Span::new("$ ", prompt_style)];
    if let Some(command) = shell_command_summary(arguments) {
        spans.push(Span::new(command, command_style));
    }
    match status {
        ToolRenderStatus::Running => spans.push(Span::new(" …", prompt_style)),
        ToolRenderStatus::Done(ToolOutputStatus::Success) => {}
        ToolRenderStatus::Done(ToolOutputStatus::Error) => {
            spans.push(Span::new(" x", Style::default().fg(Color::Red)));
        }
        ToolRenderStatus::Done(status) => {
            spans.push(Span::new(
                format!(" {}", tool_status_label(&status)),
                prompt_style,
            ));
        }
    }
    StyledBlock::new(StyledText::from(spans))
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
    match name {
        "exec_command" | "shell_command" => shell_command_summary(arguments),
        "write_stdin" => Some("wait for command output".to_owned()),
        _ => {
            serde_json::from_str::<serde_json::Value>(arguments).ok()?;
            Some(truncate_inline(arguments, 96))
        }
    }
}

fn shell_command_summary(arguments: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
    value
        .get("cmd")
        .or_else(|| value.get("command"))
        .and_then(|command| command.as_str())
        .map(|command| truncate_inline(command, 96))
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
