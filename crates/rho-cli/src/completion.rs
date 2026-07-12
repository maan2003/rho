use std::path::{Path, PathBuf};

use rho_cli_term_raw::{Candidate, Color, CompletionView, Span, Style, StyledBlock, StyledText};

pub(crate) fn completion_candidates(
    buffer: &str,
    cursor: usize,
    workdirs: &[(String, String)],
    topics: &[String],
) -> Vec<Candidate> {
    let Some(token) = word_token(buffer, cursor) else {
        return Vec::new();
    };
    if buffer.trim_start().starts_with(':') {
        let leading_len = buffer.len() - buffer.trim_start().len();
        let Some(before_cursor) = buffer.get(leading_len..cursor) else {
            return Vec::new();
        };
        let command = rho_commands::completion_candidates(
            before_cursor,
            &rho_commands::CompletionCtx { workdirs, topics },
        )
        .into_iter()
        .map(|candidate| {
            // The current token may carry the `:` prefix; the candidate
            // value replaces only the word after it.
            let colon = if token.prefix.starts_with(':') {
                ":"
            } else {
                ""
            };
            Candidate {
                label: candidate.value.clone(),
                description: candidate.description,
                replacement: format!("{}{colon}{}{}", token.before, candidate.value, token.after),
            }
        })
        .collect::<Vec<_>>();
        if !command.is_empty() {
            return command;
        }
        // Path arguments (`:projects add ./x`, `:agent new ~/src/y`) fall
        // through to filesystem completion below.
    }
    if token.prefix.starts_with('@') {
        return Vec::new();
    }
    if is_path_token(token.prefix) {
        return path_candidates(&token, dirs::home_dir().as_deref());
    }
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
