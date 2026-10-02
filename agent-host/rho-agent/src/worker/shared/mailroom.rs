//! `human`, `archive()` and `end_turn()` in the notebook: the model's only
//! way to speak to the person, and its way of saying it is done until
//! something happens.
//!
//! Sending is instant and hands the message to the agent loop, which logs
//! it. `end_turn()` tells the loop that the current exec ends the model's
//! turn: the agent then awaits the human, and only news wakes it.

use std::sync::Arc;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyModule;
use rho_agent_types::SendKind;
use rho_notebook::Export;
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
