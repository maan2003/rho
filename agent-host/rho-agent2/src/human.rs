//! `human` and `agents` in the notebook: the model's only way to speak, and
//! its way of saying it waits on the human.
//!
//! Sending is instant and hands the message to the agent loop, which logs
//! it. `await human.reply()` resolves to `None` when the human next writes;
//! the message itself arrives in the model's next report like anything else.
//! While any cell awaits it, the agent is awaiting the human: the fact the
//! dealer reads.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyModule;
use rho_agent_types::AgentRole;
use rho_notebook2::{Export, operation};
use senax_encoder::{Decode, Encode};
use tokio::sync::mpsc;

use crate::log::{AgentId, Party};

/// A notebook collaboration request. The host uses the source agent ID from
/// the worker port, never a caller-supplied identity.
#[derive(Debug, Encode, Decode)]
pub enum Agent2Call {
    SpawnEngineer {
        task_name: String,
        prompt: String,
        workdir: Option<String>,
    },
    SpawnUserOwnedEngineer {
        task_name: String,
        prompt: String,
        workdir: Option<String>,
    },
    SpawnAdvisor {
        message: String,
    },
    Message {
        agent_id: AgentId,
        message: String,
    },
    Cancel {
        agent_id: AgentId,
    },
    Team,
}

impl Agent2Call {
    /// The host checks this against persisted role metadata on every call.
    pub fn allowed(&self, role: AgentRole) -> bool {
        role.is_engineer() || matches!(self, Self::Message { .. } | Self::Team)
    }
}

#[derive(Debug, Encode, Decode)]
pub struct Agent2Reply {
    pub text: String,
}

pub type Agent2HostCall = Arc<
    dyn Fn(Agent2Call) -> Pin<Box<dyn Future<Output = Result<Agent2Reply, String>> + Send>>
        + Send
        + Sync,
>;

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
    agents: Option<Agent2HostCall>,
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
    pub fn new(agents: Option<Agent2HostCall>) -> (Arc<Self>, mpsc::UnboundedReceiver<Outbound>) {
        let (outbox, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                outbox,
                agents,
                waits: Mutex::default(),
            }),
            rx,
        )
    }

    /// Queue an outgoing message in this agent's log before the host relays it.
    pub fn send_to(&self, to: Party, text: String) -> anyhow::Result<()> {
        anyhow::ensure!(!text.trim().is_empty(), "a message needs text");
        self.outbox
            .send(Outbound::Send { to, text })
            .map_err(|_| anyhow::anyhow!("agent notebook closed"))
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

impl Bridge {
    fn tool(&self, py: Python<'_>, call: Agent2Call, name: &'static str) -> PyResult<Py<PyAny>> {
        let tools = self
            .0
            .agents
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("agent service unavailable"))?
            .clone();
        operation(py, name, move |cx| async move {
            let result = tools(call).await?;
            cx.report(&result.text);
            Ok(result.text)
        })
    }
}

#[pymethods]
impl Bridge {
    #[pyo3(signature = (to, text))]
    fn send(&self, to: Option<String>, text: String) -> PyResult<()> {
        if text.trim().is_empty() {
            return Err(PyValueError::new_err("a message needs text"));
        }
        let to = match to {
            None => Party::Human,
            Some(id) => Party::Agent(
                AgentId::from_encoded(&id)
                    .map_err(|error| PyValueError::new_err(error.to_string()))?,
            ),
        };
        let _ = self.0.outbox.send(Outbound::Send { to, text });
        Ok(())
    }

    fn archive(&self) {
        self.0.archive();
    }

    #[pyo3(signature = (*, task_name, prompt, workdir = None))]
    fn spawn_new_engineer(
        &self,
        py: Python<'_>,
        task_name: String,
        prompt: String,
        workdir: Option<String>,
    ) -> PyResult<Py<PyAny>> {
        self.tool(
            py,
            Agent2Call::SpawnEngineer {
                task_name,
                prompt,
                workdir,
            },
            "agents.spawn_new_engineer",
        )
    }

    #[pyo3(signature = (*, task_name, prompt, workdir = None))]
    fn spawn_user_owned_engineer(
        &self,
        py: Python<'_>,
        task_name: String,
        prompt: String,
        workdir: Option<String>,
    ) -> PyResult<Py<PyAny>> {
        self.tool(
            py,
            Agent2Call::SpawnUserOwnedEngineer {
                task_name,
                prompt,
                workdir,
            },
            "agents.spawn_user_owned_engineer",
        )
    }

    fn spawn_new_advisor(&self, py: Python<'_>, message: String) -> PyResult<Py<PyAny>> {
        self.tool(
            py,
            Agent2Call::SpawnAdvisor { message },
            "agents.spawn_new_advisor",
        )
    }

    #[pyo3(signature = (*, agent_id, message))]
    fn message(&self, py: Python<'_>, agent_id: String, message: String) -> PyResult<Py<PyAny>> {
        let agent_id = AgentId::from_encoded(&agent_id)
            .map_err(|error| PyValueError::new_err(error.to_string()))?;
        self.tool(
            py,
            Agent2Call::Message { agent_id, message },
            "agents.message",
        )
    }

    fn cancel(&self, py: Python<'_>, agent_id: String) -> PyResult<Py<PyAny>> {
        let agent_id = AgentId::from_encoded(&agent_id)
            .map_err(|error| PyValueError::new_err(error.to_string()))?;
        self.tool(py, Agent2Call::Cancel { agent_id }, "agents.cancel")
    }

    fn team(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.tool(py, Agent2Call::Team, "agents.team")
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

    def spawn_new_engineer(self, *, task_name, prompt, workdir=None):
        """Start an Engineer; its findings arrive as mail. Returns a full ID."""
        return self._bridge.spawn_new_engineer(task_name=str(task_name), prompt=str(prompt), workdir=workdir)

    def spawn_user_owned_engineer(self, *, task_name, prompt, workdir=None):
        """Start an Engineer managed by the human, not by this agent."""
        return self._bridge.spawn_user_owned_engineer(task_name=str(task_name), prompt=str(prompt), workdir=workdir)

    def spawn_new_advisor(self, message):
        """Ask an independent Advisor; its answer arrives as mail."""
        return self._bridge.spawn_new_advisor(str(message))

    def message(self, *, agent_id, message):
        """Send a confirmed message to an agent by its full ID."""
        return self._bridge.message(agent_id=str(agent_id), message=str(message))

    def cancel(self, agent_id):
        """Interrupt an Engineer you manage, by its full ID."""
        return self._bridge.cancel(str(agent_id))

    def team(self):
        """Describe this agent's team and parent."""
        return self._bridge.team()

    def __repr__(self):
        return "<agents>"
"#;

#[cfg(test)]
mod tests {
    use rho_agent_types::AgentIdDomain;

    use super::*;

    #[test]
    fn advisor_can_only_message_or_read_team() {
        let role = AgentRole::Advisor {
            intelligence: rho_agent_types::AdvisorIntelligence::Medium,
        };
        let id = AgentId::from_counter(17, &AgentIdDomain(42)).unwrap();
        for call in [
            Agent2Call::SpawnEngineer {
                task_name: "task".into(),
                prompt: "work".into(),
                workdir: None,
            },
            Agent2Call::SpawnUserOwnedEngineer {
                task_name: "task".into(),
                prompt: "work".into(),
                workdir: None,
            },
            Agent2Call::SpawnAdvisor {
                message: "ask".into(),
            },
            Agent2Call::Cancel { agent_id: id },
        ] {
            assert!(!call.allowed(role));
            assert!(call.allowed(AgentRole::default()));
        }
        assert!(
            Agent2Call::Message {
                agent_id: id,
                message: "reply".into()
            }
            .allowed(role)
        );
        assert!(Agent2Call::Team.allowed(role));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn python_agents_calls_are_awaitable_and_forward_typed_arguments() {
        use std::time::Duration;

        use rho_notebook2::Notebook;
        let dir = tempfile::tempdir().unwrap();
        let (calls, mut received) = mpsc::unbounded_channel();
        let host: Agent2HostCall = Arc::new(move |call| {
            let calls = calls.clone();
            Box::pin(async move {
                calls.send(call).unwrap();
                Ok(Agent2Reply {
                    text: "accepted".into(),
                })
            })
        });
        let (mailroom, _) = Mailroom::new(Some(host));
        let wake = Arc::new(tokio::sync::Notify::new());
        let notebook = Notebook::new(
            rho_tool_shell::ShellTools::in_directory(
                Duration::from_secs(5),
                camino::Utf8PathBuf::try_from(dir.path().to_path_buf()).unwrap(),
                rho_fs_view::PathOverrides::default(),
            ),
            mailroom.exports(),
            wake.clone(),
        )
        .unwrap();
        let id = AgentId::from_counter(17, &AgentIdDomain(42)).unwrap();
        let cell = notebook.run(format!(
            "await agents.spawn_new_engineer(task_name='alpha', prompt='build', workdir='/src/sub')\nawait agents.spawn_new_advisor('question')\nawait agents.spawn_user_owned_engineer(task_name='beta', prompt='inspect')\nawait agents.message(agent_id='{id}', message='details')\nawait agents.cancel('{id}')\nprint(await agents.team())",
            id=id.encoded()
        ));
        tokio::time::timeout(Duration::from_secs(10), async {
            while cell.facts().finished.is_none() {
                wake.notified().await;
            }
        })
        .await
        .unwrap();
        assert!(!cell.facts().finished.unwrap().failed);
        let report = notebook.report().unwrap().text;
        assert!(report.contains("accepted"), "{report}");
        assert!(
            matches!(received.recv().await.unwrap(), Agent2Call::SpawnEngineer { task_name, prompt, workdir }
            if task_name == "alpha" && prompt == "build" && workdir.as_deref() == Some("/src/sub"))
        );
        assert!(
            matches!(received.recv().await.unwrap(), Agent2Call::SpawnAdvisor { message } if message == "question")
        );
        assert!(
            matches!(received.recv().await.unwrap(), Agent2Call::SpawnUserOwnedEngineer { task_name, prompt, workdir }
            if task_name == "beta" && prompt == "inspect" && workdir.is_none())
        );
        assert!(
            matches!(received.recv().await.unwrap(), Agent2Call::Message { agent_id, message }
            if agent_id == id && message == "details")
        );
        assert!(
            matches!(received.recv().await.unwrap(), Agent2Call::Cancel { agent_id } if agent_id == id)
        );
        assert!(matches!(received.recv().await.unwrap(), Agent2Call::Team));
        notebook.shutdown().await.unwrap();
    }

    #[test]
    fn host_sent_agent_message_uses_the_outbound_log_path() {
        let (mailroom, mut outbox) = Mailroom::new(None);
        let target = AgentId::from_counter(9, &AgentIdDomain(42)).unwrap();
        mailroom
            .send_to(Party::Agent(target), "delegated result".into())
            .unwrap();
        assert_eq!(
            outbox.try_recv().unwrap(),
            Outbound::Send {
                to: Party::Agent(target),
                text: "delegated result".into()
            }
        );
        assert!(mailroom.send_to(Party::Human, "  ".into()).is_err());
        assert!(outbox.try_recv().is_err());
    }

    #[test]
    fn agents_send_parses_full_ids_and_rejects_invalid_labels() {
        let (mailroom, mut outbox) = Mailroom::new(None);
        let bridge = Bridge(mailroom);
        let id = AgentId::from_counter(17, &AgentIdDomain(42)).unwrap();
        bridge.send(Some(id.encoded()), "hello".into()).unwrap();
        assert_eq!(
            outbox.try_recv().unwrap(),
            Outbound::Send {
                to: Party::Agent(id),
                text: "hello".into(),
            }
        );
        assert!(bridge.send(Some("short".into()), "hello".into()).is_err());
        assert!(
            bridge
                .send(Some("!!!!!!!!!!!!".into()), "hello".into())
                .is_err()
        );
        assert!(outbox.try_recv().is_err());
    }
}
