//! Built-in model-facing surface for the global Iris coordinator role.

use std::sync::Arc;

use futures::future::BoxFuture;
use rho_core::{ToolCall, ToolName, ToolOutput, ToolSpec, ToolType};
use serde_json::json;

pub const PROMPT: &str = r#"## Iris

You are the backend executor for Iris, the single global assistant in the Rho GUI. The user experiences the realtime voice and you as one unified assistant. Never mention a backend, handoff, or separate components. You control the user's fleet of agents and workstreams; you are not an ordinary worker and never inspect or modify repositories yourself.

Requests arrive as realtime transcripts and may omit punctuation or contain recognition errors. New requests can steer work already in progress. Keep responses concise and action-oriented so the voice can respond quickly. Use the Iris control tools for current status and every control action; never claim an action succeeded without its tool result. Prefer steering an existing responsible agent over starting a duplicate. Start a new agent when the user asks or when work has no suitable owner. Ask a brief spoken clarification only when needed to avoid a materially harmful mistake. For cancellation, hiding, moving, or other destructive operations, ask for voice confirmation before calling the tool unless the user already confirmed explicitly.

Tool results and agent transcripts are authoritative. Do not read code, diffs, tables, identifiers, or long agent output aloud. Summarize the useful state and name the responsible agent. Your final response is spoken by the realtime model, so finish with the shortest useful acknowledgement or status."#;
pub const LABEL: &str = "system:iris";

/// Daemon implementation of Iris's built-in global control operations.
///
/// Tool identity, schemas, prompt policy, and role selection remain owned by
/// `rho-agent`; the daemon supplies only the stateful operation host.
pub trait IrisToolHost: Send + Sync + 'static {
    fn call(&self, call: ToolCall) -> BoxFuture<'static, ToolOutput>;
}

pub fn is_tool(name: &str) -> bool {
    specs().iter().any(|spec| spec.name.as_str() == name)
}

pub fn specs() -> Vec<ToolSpec> {
    vec![
        spec(
            "iris_list_agents",
            "List every user-visible Rho agent with its handle, name, workstream, current attention state, and latest request.",
            json!({"type":"object","additionalProperties":false}),
        ),
        spec(
            "iris_list_workstreams",
            "List workstreams and their member agents.",
            json!({"type":"object","additionalProperties":false}),
        ),
        spec(
            "iris_start_agent",
            "Start a new agent and send its initial request. Omit project only when exactly one project is registered. Omit workstream to found a new one.",
            json!({
                "type":"object","additionalProperties":false,"required":["prompt"],
                "properties":{
                    "prompt":{"type":"string"},
                    "task_name":{"type":"string","description":"Short display/workstream name."},
                    "project":{"type":"string","description":"Registered project name or absolute repository path."},
                    "workstream":{"type":"string","description":"Existing workstream name to join."},
                    "role":{"type":"string","enum":["eng-mini","eng-low","eng","eng-high","eng-ultra","eng-alt","pm"]}
                }
            }),
        ),
        spec(
            "iris_send_agent",
            "Send or steer an existing agent. Immediate delivery steers the current turn; next_turn waits for the current turn to finish.",
            json!({
                "type":"object","additionalProperties":false,"required":["agent","message"],
                "properties":{
                    "agent":{"type":"string"},"message":{"type":"string"},
                    "delivery":{"type":"string","enum":["immediate","next_turn"]}
                }
            }),
        ),
        spec(
            "iris_cancel_agent",
            "Cancel an agent's current turn without deleting the agent.",
            target_schema(),
        ),
        spec(
            "iris_continue_agent",
            "Continue an unfinished agent turn.",
            target_schema(),
        ),
        spec(
            "iris_rename_agent",
            "Rename an agent.",
            json!({"type":"object","additionalProperties":false,"required":["agent","name"],"properties":{"agent":{"type":"string"},"name":{"type":"string"}}}),
        ),
        spec(
            "iris_move_agent",
            "Move an agent and its spawn subtree to an existing or newly named workstream.",
            json!({"type":"object","additionalProperties":false,"required":["agent","workstream"],"properties":{"agent":{"type":"string"},"workstream":{"type":"string"}}}),
        ),
        spec(
            "iris_set_agent_visibility",
            "Show or hide an agent in the GUI.",
            json!({"type":"object","additionalProperties":false,"required":["agent","hidden"],"properties":{"agent":{"type":"string"},"hidden":{"type":"boolean"}}}),
        ),
        spec(
            "iris_rename_workstream",
            "Rename a workstream.",
            json!({"type":"object","additionalProperties":false,"required":["workstream","name"],"properties":{"workstream":{"type":"string"},"name":{"type":"string"}}}),
        ),
    ]
}

fn spec(name: &str, description: &str, input_schema: serde_json::Value) -> ToolSpec {
    ToolSpec {
        name: ToolName::try_from(name).expect("valid Iris tool name"),
        tool_type: ToolType::Function,
        description: description.to_owned(),
        input_schema,
        format: None,
    }
}

fn target_schema() -> serde_json::Value {
    json!({
        "type":"object","additionalProperties":false,"required":["agent"],
        "properties":{"agent":{"type":"string"}}
    })
}

pub type SharedIrisToolHost = Arc<dyn IrisToolHost>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iris_surface_is_builtin_and_fleet_scoped() {
        let names = specs()
            .into_iter()
            .map(|spec| spec.name.as_str().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 10);
        assert!(names.iter().all(|name| name.starts_with("iris_")));
        assert!(PROMPT.contains("single global assistant"));
    }
}
