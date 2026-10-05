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
                    "not sent: write it in Simplified Technical English (ASD-STE100), then send again\n{}",
                    findings.join("\n")
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

/// One line per STE violation in the prose of `text`; code is exempt.
fn ste_findings(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    ste_checker::check(text, &STE)
        .into_iter()
        .map(|f| {
            let span = f.lint.span;
            // A lone semicolon says nothing: quote the text around it.
            let (start, end) = match f.rule {
                "semicolon" => (
                    span.start.saturating_sub(30),
                    (span.end + 30).min(chars.len()),
                ),
                _ => (span.start, span.end),
            };
            let mut excerpt: String = chars[start..end].iter().take(60).collect();
            if end - start > 60 {
                excerpt.push('…');
            }
            format!("- {}: \"{excerpt}\": {}", f.rule, f.lint.message)
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
            findings[0].starts_with("- sentence-length:"),
            "{findings:?}"
        );
    }

    #[test]
    fn rejects_contractions_progressive_verbs_and_semicolons() {
        let findings = ste_findings("I am running the tests. Stop the job; then wait.");
        assert!(
            findings
                .iter()
                .any(|f| f.starts_with("- ing-verb: \"running\"")),
            "{findings:?}"
        );
        assert!(
            findings.iter().any(|f| f.starts_with("- semicolon:")),
            "{findings:?}"
        );
    }

    #[test]
    fn quotes_findings_by_char_not_byte() {
        let findings = ste_findings("Café ünïcode naïve. Now it's here.");
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert!(
            findings[0].starts_with("- contraction: \"it's\""),
            "{findings:?}"
        );
    }
}
