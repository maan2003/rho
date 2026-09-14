//! Shared notebook host functions. Neither runtime owns the other's tool
//! surface.
use std::sync::Arc;

use futures::future::BoxFuture;
use rho_agent_tools::FutureTool;
use rho_core::{AgentId, ToolCall, ToolOutput, ToolOutputStatus, ToolSpec};
use rho_inference::Inference;
use rho_tool_shell::{DEFAULT_TIMEOUT_SECS, ShellTools};
use rho_web_search::WebSearchTools;

use crate::View;
use crate::db::AgentRole;
use crate::multi_agent_tools::{self, Team};
use crate::worker::Host;

/// What every runtime's tools are built from: the shell, and the host
/// functions Rho answers itself (images, collaboration, web search,
/// papercuts). Both runtimes expose them only inside the Python notebook.
/// `inference` and `host` may be absent for a rendering, which gets specs that
/// cannot be called.
pub(crate) fn host_tools(
    view: &Arc<View>,
    role: AgentRole,
    agent_id: AgentId,
    inference: Option<&Inference>,
    multi_agent: Option<&Team>,
    host: Option<&Arc<Host>>,
) -> (ShellTools, Vec<Arc<dyn FutureTool>>) {
    let shell = ShellTools::new(
        std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        Arc::clone(view),
    )
    .with_env("RHO_AGENT_ID", agent_id.encoded());
    let mut others: Vec<Arc<dyn FutureTool>> = vec![Arc::new(ImageTool(
        crate::image_tool::ImageTools::new(Arc::clone(view)),
    ))];
    if let Some(host) = host.filter(|_| multi_agent.is_some()) {
        others.extend(
            multi_agent_tools::agent_tool_specs(role)
                .into_iter()
                .map(|spec| {
                    Arc::new(SharedTool {
                        host: host.clone(),
                        spec,
                    }) as Arc<dyn FutureTool>
                }),
        );
    }
    others.push(match inference {
        Some(inference) => Arc::new(WebSearchTools::new(
            inference.clone(),
            agent_id.encoded().to_owned(),
        )),
        // A rendering has no provider behind it; the spec is what it is for.
        None => Arc::new(SpecOnly(rho_web_search::web_search_spec())),
    });
    others.push(match host {
        Some(host) => Arc::new(SharedTool {
            host: host.clone(),
            spec: crate::papercut::PapercutTool::spec(),
        }),
        None => Arc::new(SpecOnly(crate::papercut::PapercutTool::spec())),
    });
    (shell, others)
}

/// A tool that exists only to be listed: calling it is an error.
struct SpecOnly(ToolSpec);

impl FutureTool for SpecOnly {
    fn spec(&self) -> ToolSpec {
        self.0.clone()
    }

    fn call(&self, _call: ToolCall) -> BoxFuture<'static, ToolOutput> {
        let name = self.0.name.clone();
        Box::pin(async move {
            ToolOutput {
                full_output: None,
                images: std::sync::Arc::new(Vec::new()),
                output: Arc::new(format!("{} is not available here", name.as_str())),
                status: ToolOutputStatus::Error,
            }
        })
    }
}

struct ImageTool(crate::image_tool::ImageTools);

impl FutureTool for ImageTool {
    fn spec(&self) -> ToolSpec {
        crate::image_tool::ImageTools::spec()
    }

    fn call(&self, call: ToolCall) -> BoxFuture<'static, ToolOutput> {
        let tools = self.0.clone();
        Box::pin(async move { tools.call(call).await })
    }
}

/// A collaboration or report tool, answered by the daemon.
struct SharedTool {
    host: Arc<Host>,
    spec: ToolSpec,
}

impl FutureTool for SharedTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn call(&self, call: ToolCall) -> BoxFuture<'static, ToolOutput> {
        let host = self.host.clone();
        Box::pin(async move { host.shared_tool(call).await })
    }
}
