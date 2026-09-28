//! The notebook's host tools for the Rho runtime: images, collaboration,
//! web search and papercuts, all answered by Rho itself.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use pyo3::PyClassInitializer;
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use rho_agent_types::transcript::{ImageDetail, ToolExecutionContext};
use rho_agent_types::{AgentId, AgentRole};
use rho_notebook::{Export, operation};
use rho_tool_shell::{DEFAULT_TIMEOUT_SECS, ShellTools};
use rho_web_search::{WebRequest, WebSearchTools};

use super::mailroom::Mailroom;
use crate::inference::Inference;
use crate::ipc::protocol::SharedCall;
use crate::multi_agent_tools::{AdvisorArgs, AgentCall, InterruptArgs, SendArgs, SpawnArgs, Team};
use crate::papercut::PapercutArgs;
use crate::worker::host_client::HostClient;
use crate::worker::image_tool::{ImageTools, ViewImageArgs};

/// The shell, and the notebook globals Rho answers itself (images,
/// collaboration, web search, papercuts, and the mailroom's `human` and
/// `archive` when there is one). Unavailable services export nothing.
pub(crate) fn host_tools(
    cwd: &camino::Utf8Path,
    role: AgentRole,
    agent_id: AgentId,
    inference: Option<&Inference>,
    multi_agent: Option<&Team>,
    host: Option<&Arc<HostClient>>,
    mailroom: Option<&Arc<Mailroom>>,
) -> (ShellTools, Vec<Export>) {
    let shell = ShellTools::in_directory(
        std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        cwd.to_owned(),
        Default::default(),
    )
    .with_env("RHO_AGENT_ID", agent_id.encoded());
    let agent_host = host.map(|host| {
        let host = Arc::clone(host);
        Arc::new(move |call| -> Pin<Box<dyn Future<Output = _> + Send>> {
            let host = Arc::clone(&host);
            Box::pin(async move { host.shared_tool(call).await })
        }) as AgentHost
    });
    let mut exports = vec![Export::new(
        "view_image",
        ViewImage {
            images: ImageTools::new(cwd.to_owned()),
        },
    )];
    if let Some(agent_host) = agent_host.as_ref().filter(|_| multi_agent.is_some()) {
        exports.push(agents(role, Arc::clone(agent_host), mailroom.cloned()));
    }
    if let Some(inference) = inference {
        exports.push(Export::new(
            "web",
            Web {
                tools: WebSearchTools::new(
                    {
                        let inference = inference.clone();
                        Arc::new(move || {
                            let inference = inference.clone();
                            Box::pin(async move { inference.web_credentials().await })
                        })
                    },
                    agent_id.encoded().to_owned(),
                ),
            },
        ));
    }
    if let Some(agent_host) = agent_host {
        exports.push(Export::new("papercut", Papercut { agent_host }));
    }
    exports.extend(
        mailroom
            .map(|mailroom| mailroom.exports())
            .unwrap_or_default(),
    );
    (shell, exports)
}

/// Whoever answers the calls the agent host owns: the worker's host, over IPC.
type AgentHost = Arc<
    dyn Fn(SharedCall) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>>
        + Send
        + Sync,
>;

/// A call the agent host answers, as an operation of the running cell. Its
/// reply is both reported and returned.
fn ask(
    py: Python<'_>,
    agent_host: &AgentHost,
    name: &str,
    call: SharedCall,
) -> PyResult<Py<PyAny>> {
    let agent_host = Arc::clone(agent_host);
    operation(py, name, move |cx| async move {
        let text = agent_host(call).await?;
        cx.report(&text);
        Ok(text)
    })
}

/// `agents`: collaboration. Engineers may also start and stop others.
fn agents(role: AgentRole, agent_host: AgentHost, mailroom: Option<Arc<Mailroom>>) -> Export {
    let agents = Agents { agent_host };
    Export::build("agents", move |py| {
        let inner = match role {
            AgentRole::Engineer { .. } => {
                let engineer = PyClassInitializer::from(agents).add_subclass(EngineerAgents);
                Py::new(py, engineer)?.into_any()
            }
            AgentRole::Advisor { .. } => Py::new(py, agents)?.into_any(),
        };
        match &mailroom {
            Some(mailroom) => mailroom.agents(py, inner),
            None => Ok(inner),
        }
    })
}

#[pyclass(subclass, frozen, module = "__main__")]
struct Agents {
    agent_host: AgentHost,
}

#[pymethods]
impl Agents {
    /// Send `message` to another agent, by its role-prefixed handle.
    #[pyo3(signature = (*, agent_id, message))]
    fn message(&self, py: Python<'_>, agent_id: String, message: String) -> PyResult<Py<PyAny>> {
        let call = AgentCall::Message(SendArgs { agent_id, message });
        ask(
            py,
            &self.agent_host,
            "agents.message",
            SharedCall::Agent(call),
        )
    }
}

#[pyclass(extends = Agents, frozen, module = "__main__")]
struct EngineerAgents;

#[pymethods]
impl EngineerAgents {
    /// Start an Engineer on `prompt`, optionally in `workdir`.
    #[pyo3(signature = (*, task_name, prompt, workdir = None))]
    fn spawn_new_engineer(
        this: PyRef<'_, Self>,
        py: Python<'_>,
        task_name: String,
        prompt: String,
        workdir: Option<String>,
    ) -> PyResult<Py<PyAny>> {
        let call = AgentCall::SpawnEngineer(SpawnArgs {
            task_name,
            prompt,
            workdir,
        });
        ask(
            py,
            &this.as_super().agent_host,
            "agents.spawn_new_engineer",
            SharedCall::Agent(call),
        )
    }

    /// Start an Engineer the user manages as its own thread; it reports to
    /// the user, not to the caller.
    #[pyo3(signature = (*, task_name, prompt, workdir = None))]
    fn spawn_user_owned_engineer(
        this: PyRef<'_, Self>,
        py: Python<'_>,
        task_name: String,
        prompt: String,
        workdir: Option<String>,
    ) -> PyResult<Py<PyAny>> {
        let call = AgentCall::SpawnUserOwnedEngineer(SpawnArgs {
            task_name,
            prompt,
            workdir,
        });
        ask(
            py,
            &this.as_super().agent_host,
            "agents.spawn_user_owned_engineer",
            SharedCall::Agent(call),
        )
    }

    /// Interrupt an Engineer's current turn.
    #[pyo3(signature = (*, agent_id))]
    fn cancel(this: PyRef<'_, Self>, py: Python<'_>, agent_id: String) -> PyResult<Py<PyAny>> {
        let call = AgentCall::Cancel(InterruptArgs { agent_id });
        ask(
            py,
            &this.as_super().agent_host,
            "agents.cancel",
            SharedCall::Agent(call),
        )
    }

    /// Ask an independent Advisor; its findings arrive as mail.
    fn spawn_new_advisor(
        this: PyRef<'_, Self>,
        py: Python<'_>,
        msg: String,
    ) -> PyResult<Py<PyAny>> {
        let call = AgentCall::SpawnAdvisor(AdvisorArgs { message: msg });
        ask(
            py,
            &this.as_super().agent_host,
            "agents.spawn_new_advisor",
            SharedCall::Agent(call),
        )
    }
}

/// `view_image(path, *, detail='high')`: show an image with the cell's
/// next report.
#[pyclass(frozen, module = "__main__")]
struct ViewImage {
    images: ImageTools,
}

#[pymethods]
impl ViewImage {
    #[pyo3(signature = (path, *, detail = "high"))]
    fn __call__(&self, py: Python<'_>, path: PathBuf, detail: &str) -> PyResult<Py<PyAny>> {
        let detail = match detail {
            "high" => ImageDetail::High,
            "original" => ImageDetail::Original,
            _ => return Err(PyValueError::new_err("detail must be 'high' or 'original'")),
        };
        let images = self.images.clone();
        operation(py, "view_image", move |cx| async move {
            let (text, image) = images
                .view(ViewImageArgs { path, detail })
                .await
                .map_err(|e| e.to_string())?;
            cx.report(&text);
            cx.show_image(rho_notebook::Image {
                media_type: image.media_type,
                data: image.data,
            });
            Ok(())
        })
    }
}

/// `web`: search and read the web.
#[pyclass(frozen, module = "__main__")]
struct Web {
    tools: WebSearchTools,
}

#[pymethods]
impl Web {
    /// `web.run(**request)`, with standard OpenAI web request fields. The
    /// results arrive with the cell's report, and the call also returns them.
    #[pyo3(signature = (**request))]
    fn run(&self, py: Python<'_>, request: Option<Bound<'_, PyDict>>) -> PyResult<Py<PyAny>> {
        let request = web_request(&request.unwrap_or_else(|| PyDict::new(py)))?;
        let tools = self.tools.clone();
        operation(py, "web.run", move |cx| async move {
            let output = tools.run(request, ToolExecutionContext::default()).await?;
            cx.report(&output);
            Ok(output)
        })
    }
}

/// The keyword arguments as the request they spell.
fn web_request(request: &Bound<'_, PyDict>) -> PyResult<WebRequest> {
    pythonize::depythonize(request.as_any())
        .map_err(|error| PyTypeError::new_err(format!("web.run(): {error}")))
}

/// `papercut(*, description)`: record Rho friction locally.
#[pyclass(frozen, module = "__main__")]
struct Papercut {
    agent_host: AgentHost,
}

#[pymethods]
impl Papercut {
    #[pyo3(signature = (*, description))]
    fn __call__(&self, py: Python<'_>, description: String) -> PyResult<Py<PyAny>> {
        let call = SharedCall::Papercut(PapercutArgs { description });
        ask(py, &self.agent_host, "papercut", call)
    }
}
