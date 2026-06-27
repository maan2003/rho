use std::path::{Path, PathBuf};

use rho_cli_term_raw::{Candidate, Color, CompletionView, Span, Style, StyledBlock, StyledText};

use crate::slash_commands::slash_commands;

#[derive(Clone)]
struct CompletionItem {
    value: &'static str,
    description: &'static str,
}

pub(crate) fn completion_candidates(buffer: &str, cursor: usize) -> Vec<Candidate> {
    if first_non_whitespace_starts_action(buffer) {
        return action_candidates(buffer, cursor);
    }
    let Some(token) = word_token(buffer, cursor) else {
        return Vec::new();
    };
    if token.prefix.starts_with('@') {
        return agent_candidates(&token);
    }
    if is_path_token(token.prefix) {
        return path_candidates(&token, dirs::home_dir().as_deref());
    }
    Vec::new()
}

fn action_candidates(buffer: &str, cursor: usize) -> Vec<Candidate> {
    let leading_len = buffer.len() - buffer.trim_start().len();
    if cursor < leading_len {
        return Vec::new();
    }
    let view = &buffer[leading_len..];
    let view_cursor = clamp_to_char_boundary(view, cursor.saturating_sub(leading_len));
    if view_cursor == 0 {
        return Vec::new();
    }
    let command_end = first_whitespace(view)
        .map(|(index, _)| index)
        .unwrap_or(view.len());
    if view_cursor <= command_end {
        let prefix = &view[..view_cursor];
        let suffix = &view[command_end..];
        return command_candidates(prefix)
            .into_iter()
            .map(|candidate| Candidate {
                replacement: format!(
                    "{}{}{}",
                    &buffer[..leading_len],
                    candidate.replacement,
                    suffix
                ),
                ..candidate
            })
            .collect();
    }
    let Some((space_pos, space_ch)) = first_whitespace(view) else {
        return Vec::new();
    };
    let command = &view[..space_pos];
    let rest_start = space_pos + space_ch.len_utf8();
    let rest = &view[rest_start..];
    let rest_cursor = view_cursor.saturating_sub(rest_start).min(rest.len());
    argument_candidates(command, rest, rest_cursor)
        .into_iter()
        .map(|candidate| Candidate {
            replacement: format!("{}{}", &buffer[..leading_len], candidate.replacement),
            ..candidate
        })
        .collect()
}

fn command_candidates(prefix: &str) -> Vec<Candidate> {
    let needle = prefix.to_lowercase();
    let mut prefix_matches = Vec::new();
    let mut contains_matches = Vec::new();
    for command in slash_commands() {
        let haystack = command.name.to_lowercase();
        let candidate = Candidate {
            label: command.name.to_owned(),
            description: command.description.to_owned(),
            replacement: command.name.to_owned(),
        };
        if haystack.starts_with(&needle) {
            prefix_matches.push(candidate);
        } else if haystack.contains(&needle) {
            contains_matches.push(candidate);
        }
    }
    prefix_matches.extend(contains_matches);
    prefix_matches
}

fn argument_candidates(command: &str, rest: &str, rest_cursor: usize) -> Vec<Candidate> {
    let rest_cursor = clamp_to_char_boundary(rest, rest_cursor);
    let token_start = rest[..rest_cursor]
        .char_indices()
        .rev()
        .find_map(|(pos, ch)| ch.is_whitespace().then_some(pos + ch.len_utf8()))
        .unwrap_or(0);
    let token_end = rest[rest_cursor..]
        .find(char::is_whitespace)
        .map(|pos| rest_cursor + pos)
        .unwrap_or(rest.len());
    let prior_args = rest[..token_start].split_whitespace().collect::<Vec<_>>();
    let partial = &rest[token_start..rest_cursor];
    let replacement_prefix = format!("{command} {}", &rest[..token_start]);
    let replacement_suffix = &rest[token_end..];

    flat_arg_items(command, &prior_args)
        .into_iter()
        .filter(|item| item_matches(item.value, partial))
        .map(|item| Candidate {
            label: item.value.to_owned(),
            description: item.description.to_owned(),
            replacement: format!("{replacement_prefix}{}{}", item.value, replacement_suffix),
        })
        .collect()
}

fn flat_arg_items(command: &str, prior_args: &[&str]) -> Vec<CompletionItem> {
    match command {
        "/agent" => match prior_args {
            [] => vec![
                item("new", "Create a new agent"),
                item("suspend", "Suspend the selected agent"),
                item("resume", "Resume a suspended agent"),
                item("tree", "Show agent tree"),
            ],
            ["new"] => vec![item("default", "Default role")],
            _ => Vec::new(),
        },
        "/new" => vec![item("default", "Default role")],
        "/suspend" | "/resume" => Vec::new(),
        "/model" => vec![
            item("deep", "Default rho inference config"),
            item("openai/gpt-5", "OpenAI GPT-5"),
            item("openai/gpt-5-mini", "OpenAI GPT-5 mini"),
            item("anthropic/claude-sonnet-4", "Claude Sonnet 4"),
        ],
        "/role" => vec![
            item("default", "Default agent role"),
            item("engineer", "Software engineering role"),
            item("assistant", "General assistant role"),
        ],
        "/prompt" => vec![item("new", "Create a saved prompt")],
        "/skill" => vec![item("list", "List available skills")],
        "/set" => vec![
            item("show-tools", "Set tool rendering mode"),
            item("show-diff", "Set diff rendering mode"),
            item("theme", "Set active theme"),
            item("model", "Set selected model"),
        ],
        "/theme" => vec![
            item("tau-plain-dark", "Tau plain dark theme"),
            item("tau-plain-light", "Tau plain light theme"),
            item("tau-dpc", "Tau dpc theme"),
        ],
        "/provider-auth" => vec![
            item("list", "List provider auths"),
            item("login", "Create provider auth"),
            item("logout", "Remove provider auth"),
        ],
        "/fast" => vec![
            item("on", "Enable fast tier"),
            item("off", "Disable fast tier"),
        ],
        _ => Vec::new(),
    }
}

fn item(value: &'static str, description: &'static str) -> CompletionItem {
    CompletionItem { value, description }
}

fn item_matches(value: &str, partial: &str) -> bool {
    let value = value.to_lowercase();
    let partial = partial.to_lowercase();
    partial.is_empty() || value.starts_with(&partial) || value.contains(&partial)
}

fn agent_candidates(_token: &WordToken<'_>) -> Vec<Candidate> {
    Vec::new()
}

fn is_path_token(token: &str) -> bool {
    matches!(token, "~")
        || token.starts_with("~/")
        || token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with('/')
}

fn path_candidates(token: &WordToken<'_>, home_dir: Option<&Path>) -> Vec<Candidate> {
    let Some(lookup_path) = home_expanded_path(token.prefix, home_dir) else {
        return Vec::new();
    };
    let display_path = Path::new(token.prefix);
    let (lookup_dir, display_dir, partial) = if token.prefix == "~" {
        (lookup_path, PathBuf::from("~"), "")
    } else if token.prefix.ends_with('/') {
        (lookup_path, display_path.to_path_buf(), "")
    } else {
        let Some(lookup_parent) = lookup_path.parent() else {
            return Vec::new();
        };
        let Some(display_parent) = display_path.parent() else {
            return Vec::new();
        };
        let partial = display_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let lookup_dir = if lookup_parent.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            lookup_parent.to_path_buf()
        };
        let display_dir = if display_parent.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            display_parent.to_path_buf()
        };
        (lookup_dir, display_dir, partial)
    };
    let Ok(entries) = std::fs::read_dir(lookup_dir) else {
        return Vec::new();
    };
    let mut candidates = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !name.starts_with(partial) || (!partial.starts_with('.') && name.starts_with('.')) {
                return None;
            }
            let is_dir = entry.file_type().ok()?.is_dir();
            let mut replacement = display_dir.join(&name).to_string_lossy().into_owned();
            if is_dir && !replacement.ends_with('/') {
                replacement.push('/');
            }
            Some(Candidate {
                label: replacement.clone(),
                description: if is_dir { "directory" } else { "file" }.to_owned(),
                replacement: format!("{}{}{}", token.before, replacement, token.after),
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| a.label.cmp(&b.label));
    candidates
}

fn home_expanded_path(prefix: &str, home_dir: Option<&Path>) -> Option<PathBuf> {
    if prefix == "~" {
        Some(home_dir?.to_path_buf())
    } else if let Some(rest) = prefix.strip_prefix("~/") {
        Some(home_dir?.join(rest))
    } else {
        Some(PathBuf::from(prefix))
    }
}

struct WordToken<'a> {
    prefix: &'a str,
    before: &'a str,
    after: &'a str,
}

fn word_token(buffer: &str, cursor: usize) -> Option<WordToken<'_>> {
    let before_cursor = buffer.get(..cursor)?;
    let after_cursor = buffer.get(cursor..)?;
    let token_start = before_cursor
        .char_indices()
        .rev()
        .find_map(|(index, ch)| ch.is_whitespace().then_some(index + ch.len_utf8()))
        .unwrap_or(0);
    let token_end = after_cursor
        .char_indices()
        .find_map(|(index, ch)| ch.is_whitespace().then_some(cursor + index))
        .unwrap_or(buffer.len());
    Some(WordToken {
        prefix: &buffer[token_start..cursor],
        before: &buffer[..token_start],
        after: &buffer[token_end..],
    })
}

fn first_non_whitespace_starts_action(buffer: &str) -> bool {
    buffer.trim_start().starts_with('/')
}

fn first_whitespace(text: &str) -> Option<(usize, char)> {
    text.char_indices().find(|(_, ch)| ch.is_whitespace())
}

fn clamp_to_char_boundary(text: &str, cursor: usize) -> usize {
    let mut cursor = cursor.min(text.len());
    while cursor > 0 && !text.is_char_boundary(cursor) {
        cursor -= 1;
    }
    cursor
}

pub(crate) fn render_menu_block(
    view: &CompletionView,
    terminal_width: usize,
    terminal_height: usize,
) -> StyledBlock {
    let max_rows = (terminal_height * 30 / 100).max(1);
    let visible = visible_candidate_range(view, max_rows);
    let max_label_width = view.candidates[visible.clone()]
        .iter()
        .map(|candidate| candidate.label.chars().count())
        .max()
        .unwrap_or(0);
    let mut spans = Vec::new();
    for (row, index) in visible.enumerate() {
        if row > 0 {
            spans.push(Span::plain("\n"));
        }
        let candidate = &view.candidates[index];
        let is_selected = view.selected == Some(index);
        let label = truncate_chars(&candidate.label, max_label_width.min(terminal_width));
        let padding = max_label_width.saturating_sub(label.chars().count()) + 2;
        let description_budget = terminal_width
            .saturating_sub(4)
            .saturating_sub(label.chars().count())
            .saturating_sub(padding);
        let description = truncate_chars(&candidate.description, description_budget);
        if is_selected {
            spans.push(Span::new(
                format!("  {label}{:padding$}{description}  ", "", padding = padding),
                Style::default().bg(Color::DarkGrey),
            ));
        } else {
            spans.push(Span::plain("  "));
            spans.push(Span::new(label, Style::default().fg(Color::Cyan)));
            if !description.is_empty() {
                spans.push(Span::plain(format!("{:padding$}", "", padding = padding)));
                spans.push(Span::new(description, Style::default().fg(Color::DarkGrey)));
            }
            spans.push(Span::plain("  "));
        }
    }
    StyledBlock::new(StyledText::from(spans))
}

fn visible_candidate_range(view: &CompletionView, max_rows: usize) -> std::ops::Range<usize> {
    let total = view.candidates.len();
    let max_rows = max_rows.max(1).min(total.max(1));
    if total <= max_rows {
        return 0..total;
    }
    let selected = view.selected.unwrap_or(0).min(total - 1);
    let half = max_rows / 2;
    let start = selected.saturating_sub(half).min(total - max_rows);
    start..start + max_rows
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    if max_chars == 0 {
        return String::new();
    }
    if max_chars == 1 {
        return "…".to_owned();
    }
    let mut out = text.chars().take(max_chars - 1).collect::<String>();
    out.push('…');
    out
}
