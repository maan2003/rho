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
use crate::multi_agent_tools::{self, MultiAgentTools};
use crate::pool::AgentPool;

/// What every runtime's tools are built from: the shell, and the host
/// functions Rho answers itself (images, collaboration, web search,
/// papercuts). Both runtimes expose them only inside the Python notebook.
/// `inference` and `pool` may be absent for a rendering, which gets specs that
/// cannot be called.
pub(crate) fn host_tools(
    view: &Arc<View>,
    role: AgentRole,
    agent_id: AgentId,
    inference: Option<&Inference>,
    multi_agent: Option<&MultiAgentTools>,
    pool: &std::sync::Weak<AgentPool>,
) -> (ShellTools, Vec<Arc<dyn FutureTool>>) {
    let shell = ShellTools::new(
        std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        Arc::clone(view),
    )
    .with_env("RHO_AGENT_ID", agent_id.encoded());
    let mut others: Vec<Arc<dyn FutureTool>> = vec![Arc::new(ImageTool(
        crate::image_tool::ImageTools::new(Arc::clone(view)),
    ))];
    if let Some(multi_agent) = multi_agent {
        others.extend(
            multi_agent_tools::agent_tool_specs(role)
                .into_iter()
                .map(|spec| {
                    Arc::new(AgentTool {
                        tools: multi_agent.clone(),
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
    others.push(match pool.upgrade() {
        Some(pool) => Arc::new(crate::papercut::PapercutTool {
            db: pool.db().clone(),
            agent_id,
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

/// One of the collaboration tools, answered by the pool.
struct AgentTool {
    tools: MultiAgentTools,
    spec: ToolSpec,
}

impl FutureTool for AgentTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn call(&self, call: ToolCall) -> BoxFuture<'static, ToolOutput> {
        let tools = self.tools.clone();
        Box::pin(async move { multi_agent_tools::call_agent_tool(tools, call).await })
    }
}
