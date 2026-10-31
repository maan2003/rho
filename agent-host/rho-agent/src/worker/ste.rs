//! The check a message to the user must pass: Simplified Technical English
//! (ASD-STE100), as far as a program can tell. A rejected send raises, and
//! the model rewrites it. Only rules that rarely misfire on technical prose
//! gate; the approved-word list and passive voice flag too much of it.

use std::sync::LazyLock;

use ste_checker::Ctx;
use ste_checker::config::{AppConfig, TextType};
use ste_checker::glossary::Glossary;

/// Gating ste_checker rules; every other rule is off.
const STE_RULES: &[&str] = &[
    "sentence-length",
    "contraction",
    "ing-after-preposition",
    "semicolon",
];

static STE: LazyLock<Ctx> = LazyLock::new(|| {
    let config = AppConfig {
        text_type: TextType::Description,
        disable: ste_checker::rule_names()
            .filter(|rule| !STE_RULES.contains(rule))
            .map(String::from)
            .collect(),
    };
    Ctx::new(config, Glossary::default())
});

/// Shown around a finding, in chars on each side of its start.
const CONTEXT: usize = 30;
/// The widest excerpt of a line, in chars.
const EXCERPT: usize = 100;

/// One block per STE violation in the prose of `text`: where it is, the line
/// with the violation underlined, and what to do. Code is exempt.
pub(crate) fn ste_findings(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    ste_checker::check(text, &STE)
        .into_iter()
        .map(|f| {
            let mut span = f.lint.span;
            // A sentence span can start at the line break before it.
            span.start += chars[span.start..span.end]
                .iter()
                .take_while(|c| c.is_whitespace())
                .count();
            let line_start = chars[..span.start]
                .iter()
                .rposition(|&c| c == '\n')
                .map_or(0, |i| i + 1);
            let line_end = chars[span.start..]
                .iter()
                .position(|&c| c == '\n')
                .map_or(chars.len(), |i| span.start + i);
            let line = chars[..span.start].iter().filter(|&&c| c == '\n').count() + 1;
            // A sentence can run past its line: underline only this line.
            let end = span.end.min(line_end).max(span.start + 1);
            // Cut the line at spaces, not inside a word.
            let mut from = span.start.saturating_sub(CONTEXT).max(line_start);
            if from > line_start {
                from = chars[from..span.start]
                    .iter()
                    .position(|c| c.is_whitespace())
                    .map_or(span.start, |i| from + i + 1);
            }
            let mut to = (end + CONTEXT).min(line_end).min(from + EXCERPT);
            if to < line_end && !chars[to].is_whitespace() {
                // A long sentence is cut inside it.
                let lo = if end < to { end } else { span.start + 1 };
                to = chars[lo..to]
                    .iter()
                    .rposition(|c| c.is_whitespace())
                    .map_or(to, |i| lo + i);
            }
            let end = end.min(to);
            let mut excerpt = String::from(if from > line_start { "…" } else { "" });
            let pad = excerpt.chars().count() + span.start - from;
            excerpt.extend(&chars[from..to]);
            if to < line_end {
                excerpt.push('…');
            }
            format!(
                "line {line}, {}: {}\n    {excerpt}\n    {}{}",
                f.rule,
                f.lint.message,
                " ".repeat(pad),
                "^".repeat(end - span.start),
            )
        })
        .collect()
}



#[cfg(test)]
mod tests {
    use super::ste_findings;

    fn sentence(words: usize) -> String {
        format!("{}.", vec!["Open"; words].join(" "))
    }

    #[test]
    fn code_is_exempt_from_ste() {
        let text = "Run `cargo test; it's fine` now.\n\n```rust\nlet x = 1; // it's running\n```\n\nThe test passes.";
        assert_eq!(ste_findings(text), Vec::<String>::new());
    }

    #[test]
    fn sentences_stop_at_25_words() {
        assert!(ste_findings(&sentence(25)).is_empty());
        let findings = ste_findings(&sentence(26));
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].starts_with("line 1, sentence-length: This sentence has 26 words."),
            "{findings:?}"
        );
    }

    #[test]
    fn underlines_each_finding_on_its_own_line() {
        let findings = ste_findings(
            "Done.\n\nI am running the tests before changing code. Stop the job; then wait for it.",
        );
        assert_eq!(
            findings,
            [
                "line 3, ing-after-preposition: `before changing` hides who acts. Write a clause with \
                 a subject, for example `before selecting a fix` -> `before I select a fix`.\n    \
                 I am running the tests before changing code. Stop the job; then wait…\n    \
                 \x20                             ^^^^^^^^",
                "line 3, semicolon: A semicolon is not allowed. Write two sentences.\n    \
                 …changing code. Stop the job; then wait for it.\n    \
                 \x20                           ^",
            ],
        );
    }

    #[test]
    fn underlines_a_long_sentence_only_on_its_first_line() {
        let text = format!(
            "Intro.\n{}\n{}.",
            ["Open"; 20].join(" "),
            ["Open"; 6].join(" ")
        );
        let findings = ste_findings(&text);
        assert_eq!(findings.len(), 1, "{findings:?}");
        let excerpt = &findings[0].lines().collect::<Vec<_>>()[1..];
        let first = ["Open"; 20].join(" ");
        assert_eq!(
            excerpt,
            [
                format!("    {first}"),
                format!("    {}", "^".repeat(first.len()))
            ]
        );
    }

    #[test]
    fn places_the_underline_by_char_not_byte() {
        let findings = ste_findings("Café ünïcode naïve. Now it's here.");
        assert_eq!(
            findings,
            [format!(
                "line 1, contraction: `it's` is a contraction. Write the full words, for example \
                 `it's` -> `it is`.\n    Café ünïcode naïve. Now it's here.\n    {}^^^^",
                " ".repeat(24)
            )],
        );
    }
}
