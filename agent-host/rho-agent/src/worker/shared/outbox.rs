//! `archive()` and `end_turn()` in the notebook: the model's way of saying
//! it is done until something happens. `end_turn()` tells the loop that the
//! current exec ends the model's turn: only news wakes it after that.

use std::sync::Arc;

use pyo3::prelude::*;
use rho_notebook::Export;
use tokio::sync::mpsc;

/// What the notebook hands the agent loop.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outbound {
    Archive,
    /// The current exec ends the model's turn.
    EndTurn,
}

/// Shared by the notebook's `archive` and `end_turn` and the agent loop.
pub(crate) struct Outbox {
    outbox: mpsc::UnboundedSender<Outbound>,
}

impl Outbox {
    pub(crate) fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<Outbound>) {
        let (outbox, rx) = mpsc::unbounded_channel();
        (Arc::new(Self { outbox }), rx)
    }

    /// The notebook's globals that reach this outbox.
    pub(crate) fn exports(self: &Arc<Self>) -> Vec<Export> {
        let archive = Arc::clone(self);
        let end_turn = Arc::clone(self);
        vec![
            Export::build("archive", move |py| {
                Py::new(py, Bridge(archive))?.getattr(py, "archive")
            }),
            Export::build("end_turn", move |py| {
                Py::new(py, Bridge(end_turn))?.getattr(py, "end_turn")
            }),
        ]
    }
}

#[pyclass(frozen)]
struct Bridge(Arc<Outbox>);

#[pymethods]
impl Bridge {
    fn archive(&self) {
        let _ = self.0.outbox.send(Outbound::Archive);
    }

    fn end_turn(&self) {
        let _ = self.0.outbox.send(Outbound::EndTurn);
    }
}
