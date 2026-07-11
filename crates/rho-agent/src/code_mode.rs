//! Code-mode tool surface. When `InferenceProfile::code_mode` is on, the model
//! gets only the `exec` and `wait` tools; shell and multi-agent tools are
//! reached from JavaScript through the session's nested `tools.*` dispatch.

use std::sync::Arc;

use futures::future::BoxFuture;
use rho_code_mode::{CodeModeSession, NestedTool, ToolDispatcher};
use rho_core::{ToolCall, ToolCallId, ToolOutput, ToolOutputStatus, ToolSpec, ToolType, UnixMs};
use rho_tool_shell::ShellTools;
use tokio::sync::mpsc;

use crate::multi_agent_tools::{self, MultiAgentTools};
use crate::{AgentControl, AgentToolExtension, ToolUpdate};

/// The model-facing tool surface: `exec` (whose description embeds the nested
/// tools' TypeScript docs) and `wait`.
pub(crate) fn tool_specs(
    shell_tools: &ShellTools,
    multi_agent: bool,
    tool_extension: Option<&Arc<dyn AgentToolExtension>>,
) -> Vec<ToolSpec> {
    vec![
        rho_code_mode::exec_tool_spec(&nested_tools(shell_tools, multi_agent, tool_extension)),
        rho_code_mode::wait_tool_spec(),
    ]
}

/// Tools reachable from scripts: shell tools plus, on pooled agents, the
/// multi-agent tools except `wait` — that name belongs to the code-mode
/// `wait`, and mail waiting is resolved by the agent loop, not dispatchable
/// as a future. Mail from sub-agents arrives between turns as usual.
fn nested_tools(
    shell_tools: &ShellTools,
    multi_agent: bool,
    tool_extension: Option<&Arc<dyn AgentToolExtension>>,
) -> Vec<NestedTool> {
    let mut specs = shell_tools.specs();
    if multi_agent {
        specs.extend(
            multi_agent_tools::agent_tool_specs()
                .into_iter()
                .filter(|spec| spec.name.as_str() != multi_agent_tools::WAIT_TOOL_NAME),
        );
    }
    if let Some(extension) = tool_extension {
        specs.extend(extension.specs());
    }
    specs.iter().map(NestedTool::from_spec).collect()
}

struct Dispatcher {
    shell_tools: ShellTools,
    multi_agent: Option<MultiAgentTools>,
    tool_extension: Option<Arc<dyn AgentToolExtension>>,
    /// Nested calls run on the agent's runtime, not the code-mode thread's
    /// current-thread runtime: agent tools spawn tasks (sub-agent loops) that
    /// must outlive the session.
    runtime: tokio::runtime::Handle,
    /// `notify(...)` updates go to the agent loop, which queues them for the
    /// next request (or drops them when no turn is active).
    control: mpsc::UnboundedSender<AgentControl>,
}

impl ToolDispatcher for Dispatcher {
    fn call_tool(&self, call: ToolCall) -> BoxFuture<'static, ToolOutput> {
        let shell_tools = self.shell_tools.clone();
        let agent_tools = multi_agent_tools::is_agent_tool(call.name.as_str())
            .then(|| self.multi_agent.clone())
            .flatten();
        let extension = self.tool_extension.as_ref().and_then(|extension| {
            extension
                .specs()
                .iter()
                .any(|spec| spec.name == call.name)
                .then(|| Arc::clone(extension))
        });
        let task = self.runtime.spawn(async move {
            if let Some(extension) = extension {
                extension.call(call).await
            } else if let Some(tools) = agent_tools {
                multi_agent_tools::call_agent_tool(tools, call).await
            } else {
                shell_tools.call(call).await
            }
        });
        Box::pin(async move {
            match task.await {
                Ok(output) => output,
                Err(_) => ToolOutput {
                    output: Arc::new("nested tool task failed".to_string()),
                    status: ToolOutputStatus::Error,
                },
            }
        })
    }

    fn notify(&self, exec_call_id: ToolCallId, text: String) {
        let _ = self.control.send(AgentControl::ToolUpdate(ToolUpdate {
            call_id: exec_call_id,
            // `exec` is a custom (freeform) tool, so its extra outputs replay
            // as `custom_tool_call_output`.
            tool_type: ToolType::Custom,
            output: Arc::new(text),
            at: UnixMs::now(),
        }));
    }
}

/// Must be called on the agent's runtime; blocks briefly for V8 startup.
pub(crate) fn start_session(
    shell_tools: &ShellTools,
    multi_agent: Option<&MultiAgentTools>,
    tool_extension: Option<&Arc<dyn AgentToolExtension>>,
    control: mpsc::UnboundedSender<AgentControl>,
) -> Result<CodeModeSession, String> {
    let dispatcher = Arc::new(Dispatcher {
        shell_tools: shell_tools.clone(),
        multi_agent: multi_agent.cloned(),
        tool_extension: tool_extension.cloned(),
        runtime: tokio::runtime::Handle::current(),
        control,
    });
    CodeModeSession::new(
        nested_tools(shell_tools, multi_agent.is_some(), tool_extension),
        dispatcher,
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use futures::future::BoxFuture;
    use rho_core::{ToolCall, ToolName, ToolOutput, ToolOutputStatus, ToolSpec, ToolType};
    use rho_tool_shell::ShellTools;
    use rho_workspaces::PathOverrides;

    use crate::AgentToolExtension;

    fn shell_tools() -> ShellTools {
        ShellTools::in_directory(
            Duration::from_secs(5),
            "/tmp".into(),
            PathOverrides::default(),
        )
    }

    #[test]
    fn code_mode_surface_is_exec_and_wait_with_nested_docs() {
        let specs = super::tool_specs(&shell_tools(), true, None);
        let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
        assert_eq!(names, ["exec", "wait"]);
        // The exec description embeds the nested tools, including multi-agent
        // tools but not the loop-resolved mail `wait`.
        let exec = &specs[0].description;
        assert!(exec.contains("shell_command"), "{exec}");
        assert!(exec.contains("spawn_engineer"), "{exec}");
        assert!(!exec.contains("async function wait"), "{exec}");
    }

    #[test]
    fn without_pool_no_agent_tools_are_nested() {
        let specs = super::tool_specs(&shell_tools(), false, None);
        assert!(!specs[0].description.contains("spawn_engineer"));
    }

    struct TestExtension;

    impl AgentToolExtension for TestExtension {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: ToolName::try_from("platform_reply").unwrap(),
                tool_type: ToolType::Function,
                description: "Reply on the mapped platform thread.".to_owned(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"]
                }),
                format: None,
            }]
        }

        fn call(&self, _call: ToolCall) -> BoxFuture<'static, ToolOutput> {
            Box::pin(std::future::ready(ToolOutput {
                output: Arc::new("ok".to_owned()),
                status: ToolOutputStatus::Success,
            }))
        }
    }

    #[test]
    fn code_mode_nests_tool_extension_docs() {
        let extension: Arc<dyn AgentToolExtension> = Arc::new(TestExtension);
        let specs = super::tool_specs(&shell_tools(), false, Some(&extension));
        let exec = &specs[0].description;
        assert!(exec.contains("platform_reply"), "{exec}");
        assert!(
            exec.contains("Reply on the mapped platform thread."),
            "{exec}"
        );
    }
}
