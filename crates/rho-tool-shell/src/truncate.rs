//! Output-truncation helpers adapted from Codex's shell tools.

const APPROX_BYTES_PER_TOKEN: usize = 4;

pub(crate) const MAX_OUTPUT_TOKENS: usize = 10_000;
pub(crate) const MAX_OUTPUT_BYTES: usize = MAX_OUTPUT_TOKENS * APPROX_BYTES_PER_TOKEN;

pub(crate) struct Truncated {
    pub(crate) content: String,
}

pub(crate) fn formatted_truncate_text(content: &str) -> Truncated {
    let original_token_count = approx_token_count(content);
    let total_lines = content.lines().count();
    let (content, was_truncated) = truncate_middle_with_token_budget(content, MAX_OUTPUT_TOKENS);
    let content = if was_truncated {
        format!(
            "Warning: truncated output (original token count: {original_token_count})\nTotal output lines: {total_lines}\n\n{content}"
        )
    } else {
        content
    };

    Truncated { content }
}

fn truncate_middle_with_token_budget(content: &str, max_tokens: usize) -> (String, bool) {
    if content.is_empty() {
        return (String::new(), false);
    }

    let max_bytes = if max_tokens == MAX_OUTPUT_TOKENS {
        MAX_OUTPUT_BYTES
    } else {
        approx_bytes_for_tokens(max_tokens)
    };
    if max_tokens > 0 && content.len() <= max_bytes {
        return (content.to_owned(), false);
    }

    let truncated = truncate_with_byte_estimate(content, max_bytes);
    let was_truncated = truncated != content;
    (truncated, was_truncated)
}

fn truncate_with_byte_estimate(content: &str, max_bytes: usize) -> String {
    if content.is_empty() {
        return String::new();
    }

    if max_bytes == 0 {
        return truncation_marker(approx_tokens_from_byte_count(content.len()));
    }

    if content.len() <= max_bytes {
        return content.to_owned();
    }

    let (left_budget, right_budget) = split_budget(max_bytes);
    let (_removed_chars, left, right) = split_string(content, left_budget, right_budget);
    let removed_bytes = content.len().saturating_sub(max_bytes);
    let marker = truncation_marker(approx_tokens_from_byte_count(removed_bytes));

    assemble_truncated_output(left, right, &marker)
}

fn approx_token_count(text: &str) -> usize {
    text.len()
        .saturating_add(APPROX_BYTES_PER_TOKEN.saturating_sub(1))
        / APPROX_BYTES_PER_TOKEN
}

fn approx_bytes_for_tokens(tokens: usize) -> usize {
    tokens.saturating_mul(APPROX_BYTES_PER_TOKEN)
}

fn approx_tokens_from_byte_count(bytes: usize) -> usize {
    bytes.saturating_add(APPROX_BYTES_PER_TOKEN.saturating_sub(1)) / APPROX_BYTES_PER_TOKEN
}

fn split_budget(budget: usize) -> (usize, usize) {
    let left = budget / 2;
    (left, budget - left)
}

fn split_string(content: &str, beginning_bytes: usize, end_bytes: usize) -> (usize, &str, &str) {
    if content.is_empty() {
        return (0, "", "");
    }

    let len = content.len();
    let tail_start_target = len.saturating_sub(end_bytes);
    let mut prefix_end = 0usize;
    let mut suffix_start = len;
    let mut removed_chars = 0usize;
    let mut suffix_started = false;

    for (idx, ch) in content.char_indices() {
        let char_end = idx + ch.len_utf8();
        if char_end <= beginning_bytes {
            prefix_end = char_end;
            continue;
        }

        if idx >= tail_start_target {
            if !suffix_started {
                suffix_start = idx;
                suffix_started = true;
            }
            continue;
        }

        removed_chars = removed_chars.saturating_add(1);
    }

    if suffix_start < prefix_end {
        suffix_start = prefix_end;
    }

    (
        removed_chars,
        &content[..prefix_end],
        &content[suffix_start..],
    )
}

fn truncation_marker(removed_tokens: usize) -> String {
    format!("…{removed_tokens} tokens truncated…")
}

fn assemble_truncated_output(prefix: &str, suffix: &str, marker: &str) -> String {
    let mut out = String::with_capacity(prefix.len() + marker.len() + suffix.len() + 1);
    out.push_str(prefix);
    out.push_str(marker);
    out.push_str(suffix);
    out
}

#[cfg(test)]
mod tests;
