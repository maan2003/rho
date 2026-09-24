//! Shared notebook host tools. Neither runtime owns the other's tool
//! surface.
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use pyo3::PyClassInitializer;
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use rho_agent_types::{AgentId, AgentRole};
use rho_inference::Inference;
use rho_inference::types::{ImageDetail, ToolExecutionContext};
use rho_tool_shell::{DEFAULT_TIMEOUT_SECS, ShellTools};
use rho_web_search::{WebRequest, WebSearchTools};

use crate::View;
use crate::image_tool::{ImageTools, ViewImageArgs};
use crate::multi_agent_tools::{AdvisorArgs, AgentCall, InterruptArgs, SendArgs, SpawnArgs, Team};
use crate::papercut::PapercutArgs;
use crate::python::{Export, detached, operation};
use crate::worker::{Host, SharedCall};

/// What every runtime's tools are built from: the shell, and the host
/// objects Rho answers itself (images, collaboration, web search,
/// papercuts). Both runtimes expose them only inside the Python notebook.
/// Unavailable services export nothing. Prompt previews use the role's
/// fixed Python interface rather than constructing an inert notebook.
pub(crate) fn host_tools(
    view: &Arc<View>,
    role: AgentRole,
    agent_id: AgentId,
    inference: Option<&Inference>,
    multi_agent: Option<&Team>,
    host: Option<&Arc<Host>>,
) -> (ShellTools, Vec<Export>) {
    let shell = ShellTools::new(
        std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        Arc::clone(view),
    )
    .with_env("RHO_AGENT_ID", agent_id.encoded());
    let daemon = host.map(|host| {
        let host = Arc::clone(host);
        Arc::new(move |call| -> Pin<Box<dyn Future<Output = _> + Send>> {
            let host = Arc::clone(&host);
            Box::pin(async move { host.shared_tool(call).await })
        }) as Daemon
    });
    let mut exports = vec![Export::new(
        "view_image",
        ViewImage {
            images: ImageTools::new(Arc::clone(view)),
        },
    )];
    if let Some(daemon) = daemon.as_ref().filter(|_| multi_agent.is_some()) {
        exports.push(agents(role, Arc::clone(daemon)));
    }
    if let Some(inference) = inference {
        exports.push(Export::new(
            "web",
            Web {
                tools: WebSearchTools::new(inference.clone(), agent_id.encoded().to_owned()),
            },
        ));
    }
    if let Some(daemon) = daemon {
        exports.push(Export::new("papercut", Papercut { daemon }));
    }
    (shell, exports)
}

/// Whoever answers the calls the daemon owns: the worker's host, over IPC.
type Daemon = Arc<
    dyn Fn(SharedCall) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>>
        + Send
        + Sync,
>;

/// A call the daemon answers, as an operation of the running cell. Its reply
/// is both reported and returned.
fn ask(py: Python<'_>, daemon: &Daemon, name: &str, call: SharedCall) -> PyResult<Py<PyAny>> {
    let daemon = Arc::clone(daemon);
    operation(py, name, move |cx| async move {
        let text = daemon(call).await?;
        cx.report(&text);
        Ok(text)
    })
}

/// `agents`: collaboration. Engineers may also start and stop others.
fn agents(role: AgentRole, daemon: Daemon) -> Export {
    let agents = Agents { daemon };
    match role {
        AgentRole::Engineer { .. } => Export::build("agents", move |py| {
            let engineer = PyClassInitializer::from(agents).add_subclass(EngineerAgents);
            Ok(Py::new(py, engineer)?.into_any())
        }),
        AgentRole::Advisor { .. } => Export::new("agents", agents),
    }
}

#[pyclass(subclass, frozen, module = "__main__")]
struct Agents {
    daemon: Daemon,
}

#[pymethods]
impl Agents {
    /// Send `message` to another agent, by its role-prefixed handle.
    #[pyo3(signature = (*, agent_id, message))]
    fn message(&self, py: Python<'_>, agent_id: String, message: String) -> PyResult<Py<PyAny>> {
        let call = AgentCall::Message(SendArgs { agent_id, message });
        ask(py, &self.daemon, "agents.message", SharedCall::Agent(call))
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
            &this.as_super().daemon,
            "agents.spawn_new_engineer",
            SharedCall::Agent(call),
        )
    }

    /// Interrupt an Engineer's current turn.
    #[pyo3(signature = (*, agent_id))]
    fn cancel(this: PyRef<'_, Self>, py: Python<'_>, agent_id: String) -> PyResult<Py<PyAny>> {
        let call = AgentCall::Cancel(InterruptArgs { agent_id });
        ask(
            py,
            &this.as_super().daemon,
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
            &this.as_super().daemon,
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
    fn __call__(&self, py: Python<'_>, path: PathBuf, detail: &str) -> PyResult<()> {
        let detail = match detail {
            "high" => ImageDetail::High,
            "original" => ImageDetail::Original,
            _ => return Err(PyValueError::new_err("detail must be 'high' or 'original'")),
        };
        let images = self.images.clone();
        detached(py, "view_image", move |cx| async move {
            let (text, image) = images
                .view(ViewImageArgs { path, detail })
                .await
                .map_err(|e| e.to_string())?;
            cx.report(&text);
            cx.show_image(image);
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
    daemon: Daemon,
}

#[pymethods]
impl Papercut {
    #[pyo3(signature = (*, description))]
    fn __call__(&self, py: Python<'_>, description: String) -> PyResult<Py<PyAny>> {
        let call = SharedCall::Papercut(PapercutArgs { description });
        ask(py, &self.daemon, "papercut", call)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use rho_agent_types::{AdvisorIntelligence, ToolOutputStatus};
    use rho_inference::types::ExecCall;

    use super::*;
    use crate::python::PythonNotebook;

    #[tokio::test]
    async fn web_run_arguments_become_a_web_request() {
        let directory = tempfile::tempdir().unwrap();
        // The notebook brings up the interpreter.
        let _notebook = PythonNotebook::new(
            ShellTools::in_directory(
                Duration::from_secs(5),
                directory.path().to_str().unwrap().into(),
                Default::default(),
            ),
            Vec::new(),
        )
        .unwrap();
        Python::attach(|py| {
            let parse = |source: &str| {
                let request = py
                    .eval(&std::ffi::CString::new(source).unwrap(), None, None)
                    .unwrap();
                web_request(request.cast::<PyDict>().unwrap())
            };
            parse("dict(search_query=[{'q': 'rho'}])").unwrap();
            parse("dict(open=[{'ref_id': 'https://example.com'}])").unwrap();
            let error = parse("dict(search_query='rho')").unwrap_err();
            assert!(error.is_instance_of::<PyTypeError>(py), "{error}");
        });
    }

    /// A daemon that answers every call with the call itself.
    fn echo_daemon(calls: Arc<Mutex<Vec<String>>>) -> Daemon {
        Arc::new(move |call| -> Pin<Box<dyn Future<Output = _> + Send>> {
            let text = format!("{call:?}");
            calls.lock().unwrap().push(text.clone());
            Box::pin(async move { Ok(text) })
        })
    }

    async fn run(exports: Vec<Export>, source: &str) -> rho_inference::types::ToolOutput {
        let directory = tempfile::tempdir().unwrap();
        let shell = ShellTools::in_directory(
            Duration::from_secs(5),
            directory.path().to_str().unwrap().into(),
            Default::default(),
        );
        let notebook = PythonNotebook::new(shell, exports).unwrap();
        let wake = Arc::new(tokio::sync::Notify::new());
        let cell = notebook.exec(
            ExecCall {
                id: "agents".try_into().unwrap(),
                source: source.into(),
            },
            wake.clone(),
        );
        let exec = Arc::clone(&cell);
        tokio::time::timeout(Duration::from_secs(10), async {
            while !exec.quiescent() {
                let _ = tokio::time::timeout(Duration::from_millis(100), wake.notified()).await;
            }
        })
        .await
        .unwrap();
        let output = cell.first_output();
        cell.acknowledge_output();
        output
    }

    #[tokio::test]
    async fn agents_api_takes_python_arguments_and_runs_without_await() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let daemon = echo_daemon(calls.clone());
        let exports = vec![
            agents(AgentRole::default(), Arc::clone(&daemon)),
            Export::new("papercut", Papercut { daemon }),
        ];
        let output = run(
            exports,
            r#"
assert not hasattr(agents, "delegate_engineer")
for name in ["tools", "spawn_engineer", "ask_advisor", "message_agent", "interrupt_engineer"]:
    assert name not in globals()
assert agents.spawn_new_engineer.__doc__
result = await agents.spawn_new_engineer(task_name="test", prompt="work", workdir="/src/checkout")
assert 'workdir: Some("/src/checkout")' in result, result
assert "SendArgs" in await agents.message(agent_id="eng-test", message="hello")
from agents import cancel
await cancel(agent_id="eng-test")
try:
    agents.message("eng-test", "positional")
except TypeError:
    pass
else:
    raise AssertionError("keyword-only arguments taken positionally")
agents.spawn_new_advisor("background review")
papercut(description="friction")
"#,
        )
        .await;
        assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
        assert!(output.output.contains("background review"), "{output:?}");
        assert!(
            output.output.contains("Operation papercut completed"),
            "{output:?}"
        );
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 5, "{calls:?}");
        assert!(
            calls[2].contains("Cancel(InterruptArgs { agent_id: \"eng-test\" })"),
            "{calls:?}"
        );
    }

    #[tokio::test]
    async fn advisors_can_only_message() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let role = AgentRole::Advisor {
            intelligence: AdvisorIntelligence::Medium,
        };
        let output = run(
            vec![agents(role, echo_daemon(calls))],
            r#"
assert not hasattr(agents, "spawn_new_engineer")
assert not hasattr(agents, "spawn_new_advisor")
await agents.message(agent_id="eng-test", message="hello")
"#,
        )
        .await;
        assert_eq!(output.status, ToolOutputStatus::Success, "{output:?}");
    }
}
