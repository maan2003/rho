//! Token-budget middle truncation for exec/wait results, matching the
//! approximation used by `rho-tool-shell` (4 bytes per token, keep head and
//! tail, note the elision).

const APPROX_BYTES_PER_TOKEN: usize = 4;

pub(crate) fn truncate_middle(content: &str, max_tokens: usize) -> String {
    let max_bytes = max_tokens.saturating_mul(APPROX_BYTES_PER_TOKEN);
    if content.len() <= max_bytes {
        return content.to_owned();
    }

    let original_token_count = content.len().div_ceil(APPROX_BYTES_PER_TOKEN);
    let total_lines = content.lines().count();
    let keep = max_bytes / 2;
    let head_end = floor_char_boundary(content, keep);
    let tail_start = ceil_char_boundary(content, content.len() - keep.min(content.len()));
    let truncated_tokens = (tail_start.saturating_sub(head_end)).div_ceil(APPROX_BYTES_PER_TOKEN);

    format!(
        "Warning: truncated output (original token count: {original_token_count})\nTotal output lines: {total_lines}\n\n{}…{truncated_tokens} tokens truncated…{}",
        &content[..head_end],
        &content[tail_start..],
    )
}

fn floor_char_boundary(content: &str, mut index: usize) -> usize {
    index = index.min(content.len());
    while index > 0 && !content.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(content: &str, mut index: usize) -> usize {
    index = index.min(content.len());
    while index < content.len() && !content.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::truncate_middle;

    #[test]
    fn short_output_is_unchanged() {
        assert_eq!(truncate_middle("hello", 10), "hello");
    }

    #[test]
    fn long_output_keeps_head_and_tail() {
        let content = "a".repeat(100);
        let truncated = truncate_middle(&content, 5);
        assert!(
            truncated.starts_with("Warning: truncated output"),
            "{truncated}"
        );
        assert!(truncated.contains("tokens truncated"), "{truncated}");
        assert!(truncated.len() < content.len() + 100);
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        let content = "é".repeat(100);
        let truncated = truncate_middle(&content, 5);
        assert!(truncated.contains("tokens truncated"), "{truncated}");
    }
}
