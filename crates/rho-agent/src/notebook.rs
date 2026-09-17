//! Shared notebook host functions. Neither runtime owns the other's tool
//! surface.
use std::sync::Arc;

use futures::future::BoxFuture;
use rho_agent_tools::HostFunction;
use rho_core::{AgentId, ToolCall, ToolOutput};
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
/// Unavailable services do not register callbacks. Prompt previews use the
/// role's fixed Python interface rather than constructing an inert notebook.
pub(crate) fn host_tools(
    view: &Arc<View>,
    role: AgentRole,
    agent_id: AgentId,
    inference: Option<&Inference>,
    multi_agent: Option<&Team>,
    host: Option<&Arc<Host>>,
) -> (ShellTools, Vec<Arc<dyn HostFunction>>) {
    let shell = ShellTools::new(
        std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        Arc::clone(view),
    )
    .with_env("RHO_AGENT_ID", agent_id.encoded());
    let mut others: Vec<Arc<dyn HostFunction>> = vec![Arc::new(ImageTool(
        crate::image_tool::ImageTools::new(Arc::clone(view)),
    ))];
    if let Some(host) = host.filter(|_| multi_agent.is_some()) {
        others.extend(
            multi_agent_tools::agent_functions(role)
                .iter()
                .map(|&name| {
                    Arc::new(SharedTool {
                        host: host.clone(),
                        name,
                    }) as Arc<dyn HostFunction>
                }),
        );
    }
    if let Some(inference) = inference {
        others.push(Arc::new(WebSearchTools::new(
            inference.clone(),
            agent_id.encoded().to_owned(),
        )));
    }
    if let Some(host) = host {
        others.push(Arc::new(SharedTool {
            host: host.clone(),
            name: crate::papercut::PAPERCUT_TOOL_NAME,
        }));
    }
    (shell, others)
}

struct ImageTool(crate::image_tool::ImageTools);

impl HostFunction for ImageTool {
    fn name(&self) -> &'static str {
        crate::image_tool::VIEW_IMAGE_TOOL_NAME
    }

    fn call(&self, call: ToolCall) -> BoxFuture<'static, ToolOutput> {
        let tools = self.0.clone();
        Box::pin(async move { tools.call(call).await })
    }
}

/// A collaboration or report tool, answered by the daemon.
struct SharedTool {
    host: Arc<Host>,
    name: &'static str,
}

impl HostFunction for SharedTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn call(&self, call: ToolCall) -> BoxFuture<'static, ToolOutput> {
        let host = self.host.clone();
        Box::pin(async move { host.shared_tool(call).await })
    }
}
