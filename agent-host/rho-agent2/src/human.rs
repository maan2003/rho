//! `human` and `agents` in the notebook: the model's only way to speak, and
//! its way of saying it waits on the human.
//!
//! Sending is instant and hands the message to the agent loop, which logs
//! it. `await human.reply()` resolves to `None` when the human next writes;
//! the message itself arrives in the model's next report like anything else.
//! While any cell awaits it, the agent is awaiting the human: the fact the
//! dealer reads.

use std::sync::{Arc, Mutex};

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyModule;
use rho_notebook2::Export;
use tokio::sync::mpsc;

use crate::log::Party;

/// What the notebook hands the agent loop.
#[derive(Debug, PartialEq, Eq)]
pub enum Outbound {
    Send {
        to: Party,
        text: String,
    },
    Status(String),
    Archive,
    /// Whether some cell now awaits `human.reply()`.
    Awaiting(bool),
}

/// Shared by the notebook's `human` and the agent loop.
pub struct Mailroom {
    outbox: mpsc::UnboundedSender<Outbound>,
    waits: Mutex<Waits>,
}

#[derive(Default)]
struct Waits {
    /// Human messages received, ever.
    received: u64,
    agent_received: u64,
    /// Of those, not yet shown to the model.
    unread: u64,
    /// Cells awaiting `human.reply()`.
    waiting: u32,
}

impl Mailroom {
    pub fn new() -> (Arc<Self>, mpsc::UnboundedReceiver<Outbound>) {
        let (outbox, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                outbox,
                waits: Mutex::default(),
            }),
            rx,
        )
    }

    /// An agent message arrived.
    pub fn agent_received(&self) {
        self.waits.lock().unwrap().agent_received += 1;
    }

    pub fn archive(&self) {
        let _ = self.outbox.send(Outbound::Archive);
    }

    /// A human message arrived.
    pub fn received(&self) {
        let mut waits = self.waits.lock().unwrap();
        waits.received += 1;
        waits.unread += 1;
    }

    /// `count` human messages were shown to the model.
    pub fn read(&self, count: u64) {
        let mut waits = self.waits.lock().unwrap();
        waits.unread = waits.unread.saturating_sub(count);
    }

    /// The notebook's globals that reach this mailroom.
    pub fn exports(self: &Arc<Self>) -> Vec<Export> {
        let human = Arc::clone(self);
        let agents = Arc::clone(self);
        vec![
            Export::build("human", move |py| build(py, "Human", human)),
            Export::build("agents", move |py| build(py, "Agents", agents)),
            Export::build("archive", {
                let mailroom = Arc::clone(self);
                move |py| Py::new(py, Bridge(mailroom))?.getattr(py, "archive")
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
    #[pyo3(signature = (to, text))]
    fn send(&self, to: Option<String>, text: String) -> PyResult<()> {
        if text.trim().is_empty() {
            return Err(PyValueError::new_err("a message needs text"));
        }
        let to = match to {
            None => Party::Human,
            Some(id) if !id.trim().is_empty() => Party::Agent(id),
            Some(_) => return Err(PyValueError::new_err("agent_id is empty")),
        };
        let _ = self.0.outbox.send(Outbound::Send { to, text });
        Ok(())
    }

    fn archive(&self) {
        self.0.archive();
    }

    fn status(&self, text: String) {
        let _ = self.0.outbox.send(Outbound::Status(text));
    }

    /// Start waiting: the count to wait past. Unread messages count as
    /// already arrived, so nobody waits on what they have not read.
    fn begin_wait(&self) -> u64 {
        let mut waits = self.0.waits.lock().unwrap();
        waits.waiting += 1;
        if waits.waiting == 1 {
            let _ = self.0.outbox.send(Outbound::Awaiting(true));
        }
        waits.received - waits.unread
    }

    fn agent_wait(&self) -> u64 {
        self.0.waits.lock().unwrap().agent_received
    }

    fn agent_replied(&self, since: u64) -> bool {
        self.0.waits.lock().unwrap().agent_received > since
    }

    fn replied(&self, since: u64) -> bool {
        self.0.waits.lock().unwrap().received > since
    }

    fn end_wait(&self) {
        let mut waits = self.0.waits.lock().unwrap();
        waits.waiting = waits.waiting.saturating_sub(1);
        if waits.waiting == 0 {
            let _ = self.0.outbox.send(Outbound::Awaiting(false));
        }
    }
}

const SOURCE: &std::ffi::CStr = cr#"
import asyncio as _asyncio


class Human:
    """The person you work for. They see only what you send."""

    def __init__(self, bridge):
        self._bridge = bridge

    def send(self, text):
        """Send the human a message."""
        self._bridge.send(None, str(text))

    def status(self, text):
        """Set your one-line status, replacing the last one."""
        self._bridge.status(str(text))

    async def reply(self):
        """Wait until the human writes. Resolves to None; the message arrives
        in your next report. While you await this, you are waiting on them."""
        since = self._bridge.begin_wait()
        try:
            while not self._bridge.replied(since):
                await _asyncio.sleep(0.1)
        finally:
            self._bridge.end_wait()

    def __repr__(self):
        return "<human>"


class Agents:
    """Other agents on this host."""

    def __init__(self, bridge):
        self._bridge = bridge

    def send(self, agent_id, text):
        """Send another agent a message."""
        self._bridge.send(str(agent_id), str(text))

    async def reply(self):
        """Wait for the next agent message; it arrives in the model report."""
        since = self._bridge.agent_wait()
        while not self._bridge.agent_replied(since):
            await _asyncio.sleep(0.1)

    def __repr__(self):
        return "<agents>"
"#;
