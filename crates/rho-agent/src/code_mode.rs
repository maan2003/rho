//! Code-mode tool surface. When `InferenceProfile::code_mode` is on, the model
//! gets only `exec` and its cell-scoped `wait`; shell and collaboration tools
//! are reached from JavaScript through the session's nested `tools.*` API.

use std::sync::Arc;

use futures::future::BoxFuture;
use rho_code_mode::{CodeModeSession, NestedTool, NestedToolOutput, ToolDispatcher};
use rho_core::{
    ToolCall, ToolCallId, ToolExecutionContext, ToolOutputStatus, ToolSpec, ToolType, UnixMs,
};
use rho_tool_shell::ShellTools;
use rho_web_search::WebSearchTools;
use tokio::sync::mpsc;

use crate::multi_agent_tools::{self, MultiAgentTools};
use crate::{AgentControl, ToolUpdate};

/// The model-facing tool surface: `exec` (whose description embeds the nested
/// tools' TypeScript docs) and `wait`.
pub(crate) fn tool_specs(
    shell_tools: &ShellTools,
    role: Option<crate::db::AgentRole>,
) -> Vec<ToolSpec> {
    let nested = nested_tools(shell_tools, role);
    let documented = nested
        .iter()
        .filter(|tool| {
            !role.is_some_and(crate::db::AgentRole::is_engineer)
                || !matches!(
                    tool.name.as_str(),
                    multi_agent_tools::SPAWN_ENGINEER_TOOL_NAME
                        | multi_agent_tools::INTERRUPT_ENGINEER_TOOL_NAME
                        | multi_agent_tools::WAIT_TOOL_NAME
                )
        })
        .cloned()
        .collect::<Vec<_>>();
    vec![
        rho_code_mode::exec_tool_spec(&documented),
        rho_code_mode::wait_tool_spec(),
    ]
}

/// Tools reachable from scripts. `wait_agent` is distinct from code mode's
/// model-facing `wait`, which observes a yielded JavaScript cell.
fn nested_tools(shell_tools: &ShellTools, role: Option<crate::db::AgentRole>) -> Vec<NestedTool> {
    let mut specs = if role.is_some_and(crate::db::AgentRole::is_pm) {
        Vec::new()
    } else {
        shell_tools.specs()
    };
    if let Some(role) = role {
        specs.extend(multi_agent_tools::agent_tool_specs(role));
    }
    specs.push(rho_web_search::web_search_spec());
    specs
        .iter()
        .map(|spec| {
            let tool = NestedTool::from_spec(spec);
            match ShellTools::code_mode_output_schema(spec.name.as_str()) {
                Some(schema) => tool.with_output_schema(schema),
                None => tool,
            }
        })
        .collect()
}

struct Dispatcher {
    shell_tools: ShellTools,
    multi_agent: Option<MultiAgentTools>,
    web_search: WebSearchTools,
    /// Nested calls run on the agent's runtime, not the code-mode thread's
    /// current-thread runtime: agent tools spawn tasks (sub-agent loops) that
    /// must outlive the session.
    runtime: tokio::runtime::Handle,
    /// `notify(...)` updates go to the agent loop, which queues them for the
    /// next request (or drops them when no turn is active).
    control: mpsc::WeakUnboundedSender<AgentControl>,
}

impl ToolDispatcher for Dispatcher {
    fn call_tool(
        &self,
        context: ToolExecutionContext,
        call: ToolCall,
    ) -> BoxFuture<'static, NestedToolOutput> {
        let shell_tools = self.shell_tools.clone();
        let agent_tools = multi_agent_tools::is_agent_tool(call.name.as_str())
            .then(|| self.multi_agent.clone())
            .flatten();
        let web_search = (call.name.as_str() == rho_web_search::WEB_SEARCH_TOOL_NAME)
            .then(|| self.web_search.clone());
        let task = self.runtime.spawn(async move {
            if let Some(web_search) = web_search {
                let output = web_search.call(call, context).await;
                NestedToolOutput {
                    value: serde_json::Value::String(output.output.as_ref().clone()),
                    status: output.status,
                }
            } else if let Some(tools) = agent_tools {
                let output = multi_agent_tools::call_agent_tool(tools, call).await;
                NestedToolOutput {
                    value: serde_json::Value::String(output.output.as_ref().clone()),
                    status: output.status,
                }
            } else {
                match shell_tools.call_code_mode(call).await {
                    Ok(value) => NestedToolOutput {
                        value,
                        status: ToolOutputStatus::Success,
                    },
                    Err(error) => NestedToolOutput {
                        value: serde_json::Value::String(error.to_string()),
                        status: ToolOutputStatus::Error,
                    },
                }
            }
        });
        Box::pin(async move {
            match task.await {
                Ok(output) => output,
                Err(_) => NestedToolOutput {
                    value: serde_json::Value::String("nested tool task failed".to_owned()),
                    status: ToolOutputStatus::Error,
                },
            }
        })
    }

    fn notify(&self, exec_call_id: ToolCallId, text: String) {
        if let Some(control) = self.control.upgrade() {
            let _ = control.send(AgentControl::ToolUpdate(ToolUpdate {
                call_id: exec_call_id,
                // `exec` is a custom (freeform) tool, so its extra outputs replay
                // as `custom_tool_call_output`.
                tool_type: ToolType::Custom,
                output: Arc::new(text),
                at: UnixMs::now(),
            }));
        }
    }
}

/// Must be called on the agent's runtime; blocks briefly for V8 startup.
pub(crate) fn start_session(
    shell_tools: &ShellTools,
    multi_agent: Option<&MultiAgentTools>,
    web_search: &WebSearchTools,
    control: mpsc::WeakUnboundedSender<AgentControl>,
) -> Result<CodeModeSession, String> {
    let dispatcher = Arc::new(Dispatcher {
        shell_tools: shell_tools.clone(),
        multi_agent: multi_agent.cloned(),
        web_search: web_search.clone(),
        runtime: tokio::runtime::Handle::current(),
        control,
    });
    CodeModeSession::new(
        nested_tools(shell_tools, multi_agent.map(MultiAgentTools::role)),
        dispatcher,
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use rho_tool_shell::ShellTools;
    use rho_workspaces::PathOverrides;

    fn shell_tools() -> ShellTools {
        ShellTools::in_directory(
            Duration::from_secs(5),
            "/tmp".into(),
            PathOverrides::default(),
        )
    }

    #[test]
    fn code_mode_surface_is_exec_and_wait_with_nested_docs() {
        let specs = super::tool_specs(&shell_tools(), Some(crate::db::AgentRole::default()));
        let names: Vec<&str> = specs.iter().map(|spec| spec.name.as_str()).collect();
        assert_eq!(names, ["exec", "wait"]);
        // Optional Engineer team-management declarations live in its skill.
        let exec = &specs[0].description;
        assert!(exec.contains("exec_command"), "{exec}");
        assert!(exec.contains("write_stdin"), "{exec}");
        assert!(exec.contains("web__run"), "{exec}");
        assert!(!exec.contains("spawn_engineer"), "{exec}");
        assert!(exec.contains("message_agent"), "{exec}");
        assert!(!exec.contains("interrupt_engineer"), "{exec}");
        assert!(!exec.contains("wait_agent"), "{exec}");
        assert!(exec.contains("ask_advisor"), "{exec}");
        assert!(!exec.contains("async function wait"), "{exec}");
    }

    #[test]
    fn pm_always_sees_engineer_management_declarations() {
        let specs = super::tool_specs(&shell_tools(), Some(crate::db::AgentRole::pm()));
        let exec = &specs[0].description;
        for name in ["spawn_engineer", "message_agent", "interrupt_engineer"] {
            assert!(exec.contains(name), "missing {name}: {exec}");
        }
        assert!(!exec.contains("wait_agent"), "{exec}");
        assert!(!exec.contains("ask_advisor"), "{exec}");
        for name in ["exec_command", "write_stdin", "apply_patch"] {
            assert!(!exec.contains(name), "unexpected {name}: {exec}");
        }
    }

    #[test]
    fn without_pool_no_agent_tools_are_nested() {
        let specs = super::tool_specs(&shell_tools(), None);
        assert!(!specs[0].description.contains("spawn_engineer"));
    }
}
