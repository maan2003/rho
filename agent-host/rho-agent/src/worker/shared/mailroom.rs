//! `human`, `archive()` and `end_turn()` in the notebook: the model's only
//! way to speak to the person, and its way of saying it is done until
//! something happens.
//!
//! Sending is instant and hands the message to the agent loop, which logs
//! it. `end_turn()` tells the loop that the current exec ends the model's
//! turn: the agent then awaits the human, and only news wakes it.
//!
//! A message the human keeps must be in Simplified Technical English
//! (ASD-STE100), as far as a program can tell: a rejected send raises, and
//! the model rewrites it. Only rules that rarely misfire on technical prose
//! gate; the approved-word list and passive voice flag too much of it.

use std::sync::{Arc, LazyLock};

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyModule;
use rho_agent_types::SendKind;
use rho_notebook::Export;
use ste_checker::Ctx;
use ste_checker::config::{AppConfig, TextType};
use ste_checker::glossary::Glossary;
use tokio::sync::mpsc;

/// What the notebook hands the agent loop.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outbound {
    Send {
        cell: u64,
        text: String,
        kind: SendKind,
    },
    Archive,
    /// The current exec ends the model's turn.
    EndTurn,
}

/// Shared by the notebook's `human` and the agent loop.
pub(crate) struct Mailroom {
    outbox: mpsc::UnboundedSender<Outbound>,
}

impl Mailroom {
    pub(crate) fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<Outbound>) {
        let (outbox, rx) = mpsc::unbounded_channel();
        (Arc::new(Self { outbox }), rx)
    }

    /// The notebook's globals that reach this mailroom.
    pub(crate) fn exports(self: &Arc<Self>) -> Vec<Export> {
        let human = Arc::clone(self);
        let archive = Arc::clone(self);
        let end_turn = Arc::clone(self);
        vec![
            Export::build("human", move |py| build(py, "Human", human)),
            Export::build("archive", move |py| {
                Py::new(py, Bridge(archive))?.getattr(py, "archive")
            }),
            Export::build("end_turn", move |py| {
                Py::new(py, Bridge(end_turn))?.getattr(py, "end_turn")
            }),
        ]
    }
}

fn build(py: Python<'_>, class: &str, mailroom: Arc<Mailroom>) -> PyResult<Py<PyAny>> {
    let module = PyModule::from_code(py, SOURCE, c"rho_human.py", c"rho_human")?;
    let bridge = Py::new(py, Bridge(mailroom))?;
    Ok(module.getattr(class)?.call1((bridge,))?.unbind())
}

#[pyclass(frozen)]
struct Bridge(Arc<Mailroom>);

#[pymethods]
impl Bridge {
    fn send(&self, py: Python<'_>, text: String, kind: &str) -> PyResult<()> {
        if text.trim().is_empty() {
            return Err(PyValueError::new_err("a message needs text"));
        }
        let kind = match kind {
            "ask" => SendKind::Ask,
            "result" => SendKind::Result,
            "status" => SendKind::Status,
            "fyi" => SendKind::Fyi,
            _ => {
                return Err(PyValueError::new_err(
                    "kind is one of \"ask\", \"result\", \"fyi\" or \"status\"",
                ));
            }
        };
        if kind != SendKind::Status {
            let findings = ste_findings(&text);
            if !findings.is_empty() {
                return Err(PyValueError::new_err(format!(
                    "not sent, and the rest of this cell did not run. Write the message in \
                     Simplified Technical English (ASD-STE100): fix these problems, then send \
                     it again.\n\n{}",
                    findings.join("\n\n")
                )));
            }
        }
        let cell = rho_notebook::current_source_id(py)?;
        let _ = self.0.outbox.send(Outbound::Send { cell, text, kind });
        Ok(())
    }

    fn archive(&self) {
        let _ = self.0.outbox.send(Outbound::Archive);
    }

    fn end_turn(&self) {
        let _ = self.0.outbox.send(Outbound::EndTurn);
    }
}

/// Gating ste_checker rules; every other rule is off.
const STE_RULES: &[&str] = &["sentence-length", "contraction", "ing-verb", "semicolon"];

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
fn ste_findings(text: &str) -> Vec<String> {
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

const SOURCE: &std::ffi::CStr = cr#"
class Human:
    """The person you work for. They see only what you send."""

    def __init__(self, bridge):
        self._bridge = bridge

    def send(self, text, *, kind):
        """Send the human a message: kind is "ask", "result", "fyi" or "status"."""
        self._bridge.send(str(text), kind)

    def __repr__(self):
        return "<human>"
"#;

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
        let findings =
            ste_findings("Done.\n\nI am running the tests. Stop the job; then wait for it.");
        assert_eq!(
            findings,
            [
                "line 3, ing-verb: `running` is an -ing verb form. Use a simple tense, for example \
                 `is running` -> `runs`. Put a quoted word in a code span.\n    \
                 I am running the tests. Stop the job; then…\n    \
                 \x20    ^^^^^^^",
                "line 3, semicolon: A semicolon is not allowed. Write two sentences.\n    \
                 …the tests. Stop the job; then wait for it.\n    \
                 \x20                       ^",
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
