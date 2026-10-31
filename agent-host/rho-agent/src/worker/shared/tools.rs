//! The notebook's host tools for both runtimes: images, web search and
//! papercuts, answered by Rho itself. Policy Python adds the rest
//! (`hooks.on_notebook`).

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use rho_agent_types::AgentId;
use rho_agent_types::transcript::{ImageDetail, ToolExecutionContext};
use rho_notebook::{Export, operation};
use rho_tool_shell::ShellTools;
use rho_web_search::{WebRequest, WebSearchTools};

use super::outbox::Outbox;
use crate::inference::Inference;
use crate::ipc::protocol::SharedCall;
use crate::papercut::PapercutArgs;
use crate::worker::host_client::HostClient;
use crate::worker::image_tool::{ImageTools, ViewImageArgs};

/// The shell, and the notebook globals Rho answers itself (images, web
/// search, papercuts, and the outbox's `archive` and `end_turn` when there
/// is one). Unavailable services export nothing. Commands and relative
/// paths start where the notebook's thread is, which starts at `root`.
pub(crate) fn host_tools(
    root: &camino::Utf8Path,
    agent_id: AgentId,
    inference: Option<&Inference>,
    host: Option<&Arc<HostClient>>,
    outbox: Option<&Arc<Outbox>>,
    original_images: bool,
) -> (ShellTools, Vec<Export>) {
    let shell = ShellTools::in_directory(root.to_owned(), Default::default())
        .with_env("RHO_AGENT_ID", agent_id.encoded().to_string())
        .with_env(
            "RIPGREP_CONFIG_PATH",
            rho_fs_view::agent_base()
                .join("etc/ripgreprc")
                .display()
                .to_string(),
        );
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
            images: ImageTools::new(root.to_owned()),
            original: original_images,
        },
    )];
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
    exports.extend(outbox.map(|outbox| outbox.exports()).unwrap_or_default());
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

/// `view_image(path, *, detail='high')`: show an image with the cell's
/// next report.
#[pyclass(frozen, module = "__main__")]
struct ViewImage {
    images: ImageTools,
    /// Whether `detail='original'` is honored. Claude scales anything past
    /// 1568 down itself and refuses over 2000 in a many-image request, so
    /// for it original is high.
    original: bool,
}

#[pymethods]
impl ViewImage {
    #[pyo3(signature = (path, *, detail = "high"))]
    fn __call__(&self, py: Python<'_>, path: PathBuf, detail: &str) -> PyResult<Py<PyAny>> {
        let detail = match detail {
            "high" => ImageDetail::High,
            "original" if self.original => ImageDetail::Original,
            "original" => ImageDetail::High,
            _ => return Err(PyValueError::new_err("detail must be 'high' or 'original'")),
        };
        // Relative to this thread's directory: the notebook's own.
        let path = std::path::absolute(&path)
            .map_err(|error| PyValueError::new_err(format!("{}: {error}", path.display())))?;
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
