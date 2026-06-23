//! Output-truncation helpers adapted from Tau's shell tools.

pub(crate) const MAX_OUTPUT_LINES: usize = 2000;
pub(crate) const TRUNCATED_OUTPUT_HEAD_LINES: usize = MAX_OUTPUT_LINES / 2;
pub(crate) const TRUNCATED_OUTPUT_TAIL_LINES: usize = MAX_OUTPUT_LINES / 2;
pub(crate) const MAX_OUTPUT_BYTES: usize = 50 * 1024;

pub(crate) struct Truncated {
    pub(crate) content: String,
    pub(crate) was_truncated: bool,
    pub(crate) total_lines: usize,
    pub(crate) total_bytes: usize,
}

pub(crate) fn truncate_line_oriented(input: &str) -> Truncated {
    let lines: Vec<&str> = input.lines().collect();
    truncate_line_oriented_lines(lines.iter().copied(), lines.len(), input.len())
}

pub(crate) fn truncate_line_oriented_lines<'a>(
    lines: impl IntoIterator<Item = &'a str>,
    total_lines: usize,
    total_bytes: usize,
) -> Truncated {
    let all_lines: Vec<&str> = lines.into_iter().collect();
    let line_count_truncated = MAX_OUTPUT_LINES < total_lines;
    let selected: Vec<Option<&str>> = if line_count_truncated {
        all_lines
            .iter()
            .take(TRUNCATED_OUTPUT_HEAD_LINES)
            .copied()
            .map(Some)
            .chain(std::iter::once(None))
            .chain(
                all_lines
                    .iter()
                    .skip(all_lines.len().saturating_sub(TRUNCATED_OUTPUT_TAIL_LINES))
                    .copied()
                    .map(Some),
            )
            .collect()
    } else {
        all_lines.iter().copied().map(Some).collect()
    };

    let mut rendered = Vec::with_capacity(selected.len());
    let mut rendered_bytes = 0usize;
    let mut was_truncated = line_count_truncated || MAX_OUTPUT_BYTES < total_bytes;
    for line in selected {
        let line = match line {
            Some(line) => line,
            None => {
                if !push_budgeted_line(&mut rendered, &mut rendered_bytes, "...") {
                    was_truncated = true;
                    break;
                }
                continue;
            }
        };
        let separator_bytes = usize::from(!rendered.is_empty());
        if MAX_OUTPUT_BYTES < line.len()
            || MAX_OUTPUT_BYTES < rendered_bytes.saturating_add(separator_bytes + line.len())
        {
            let marker = mark_line(line, "truncated");
            if !push_budgeted_line(&mut rendered, &mut rendered_bytes, &marker) {
                break;
            }
            was_truncated = true;
        } else if !push_budgeted_line(&mut rendered, &mut rendered_bytes, line) {
            was_truncated = true;
            break;
        }
    }

    Truncated {
        content: rendered.join("\n"),
        was_truncated,
        total_lines,
        total_bytes,
    }
}

fn can_push_budgeted_line(rendered: &[String], rendered_bytes: usize, line: &str) -> bool {
    let separator_bytes = usize::from(!rendered.is_empty());
    rendered_bytes.saturating_add(separator_bytes + line.len()) <= MAX_OUTPUT_BYTES
}

fn push_budgeted_line(rendered: &mut Vec<String>, rendered_bytes: &mut usize, line: &str) -> bool {
    if !can_push_budgeted_line(rendered, *rendered_bytes, line) {
        return false;
    }
    let separator_bytes = usize::from(!rendered.is_empty());
    rendered.push(line.to_owned());
    *rendered_bytes += separator_bytes + line.len();
    true
}

fn mark_line(line: &str, marker: &str) -> String {
    let prefix = line.split_once(' ').map_or_else(
        || {
            if line.chars().all(|ch| ch.is_ascii_digit()) {
                line
            } else {
                ""
            }
        },
        |(prefix, _)| prefix,
    );
    if let Some((base, existing)) = prefix.split_once('(')
        && let Some(existing) = existing.strip_suffix(')')
    {
        return format!("{base}({existing},{marker})");
    }
    format!("{prefix}({marker})")
}

#[cfg(test)]
mod tests;
