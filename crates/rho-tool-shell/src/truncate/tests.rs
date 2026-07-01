use super::*;

#[test]
fn middle_truncates_to_token_budget() {
    let input = format!(
        "{}middle{}",
        "a".repeat(MAX_OUTPUT_BYTES),
        "z".repeat(MAX_OUTPUT_BYTES)
    );

    let truncated = formatted_truncate_text(&input);

    assert!(truncated.content.len() <= MAX_OUTPUT_BYTES + 128);
    assert!(truncated.content.starts_with("Warning: truncated output"));
    assert!(truncated.content.contains("Total output lines: 1"));
    assert!(truncated.content.contains("\naa"));
    assert!(truncated.content.ends_with('z'));
    assert!(truncated.content.contains("tokens truncated"));
}

#[test]
fn leaves_short_output_unchanged() {
    let truncated = formatted_truncate_text("hello\nworld");

    assert_eq!(truncated.content, "hello\nworld");
}
