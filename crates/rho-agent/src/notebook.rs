//! Shared notebook host functions. Neither runtime owns the other's tool
//! surface.
use std::sync::Arc;

use rho_agent_tools::{HostFunction, ToolCx};
use rho_core::AgentId;
use rho_inference::Inference;
use rho_tool_shell::{DEFAULT_TIMEOUT_SECS, ShellTools};
use rho_web_search::WebSearchTools;

use crate::View;
use crate::db::AgentRole;
use crate::image_tool::{ImageTools, ViewImageArgs};
use crate::multi_agent_tools::{AgentCall, Team};
use crate::papercut::PapercutArgs;
use crate::worker::{Host, SharedCall};

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
) -> (ShellTools, Vec<HostFunction>) {
    let shell = ShellTools::new(
        std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        Arc::clone(view),
    )
    .with_env("RHO_AGENT_ID", agent_id.encoded());
    let images = ImageTools::new(Arc::clone(view));
    let mut functions = vec![
        HostFunction::new("view_image", &["path"], move |cx: ToolCx, args: ViewImageArgs| {
            let images = images.clone();
            async move {
                let (text, image) = images.view(args).await.map_err(|e| e.to_string())?;
                cx.report(&text);
                cx.show_image(image);
                Ok(())
            }
        })
        .detached(),
    ];
    if let Some(host) = host.filter(|_| multi_agent.is_some()) {
        functions.push(shared(host, "agents.message", &[], |args| {
            SharedCall::Agent(AgentCall::Message(args))
        }));
        if matches!(role, AgentRole::Engineer { .. }) {
            functions.extend([
                shared(host, "agents.spawn_new_engineer", &[], |args| {
                    SharedCall::Agent(AgentCall::SpawnEngineer(args))
                }),
                shared(host, "agents.cancel", &[], |args| {
                    SharedCall::Agent(AgentCall::Cancel(args))
                }),
                shared(host, "agents.spawn_new_advisor", &["msg"], |args| {
                    SharedCall::Agent(AgentCall::SpawnAdvisor(args))
                }),
            ]);
        }
    }
    if let Some(inference) = inference {
        functions.push(rho_agent_tools::web_run(WebSearchTools::new(
            inference.clone(),
            agent_id.encoded().to_owned(),
        )));
    }
    if let Some(host) = host {
        functions.push(shared(host, "papercut", &[], |args: PapercutArgs| {
            SharedCall::Papercut(args)
        }));
    }
    (shell, functions)
}

/// A function the daemon answers. Its reply is both reported and returned.
fn shared<A>(
    host: &Arc<Host>,
    path: &'static str,
    positional: &'static [&'static str],
    call: impl Fn(A) -> SharedCall + Send + Sync + 'static,
) -> HostFunction
where
    A: serde::de::DeserializeOwned + Send + 'static,
{
    let host = Arc::clone(host);
    HostFunction::new(path, positional, move |cx: ToolCx, args: A| {
        let host = Arc::clone(&host);
        let call = call(args);
        async move {
            let text = host.shared_tool(call).await?;
            cx.report(&text);
            Ok(text)
        }
    })
}
