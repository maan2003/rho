//! Built-in model-facing surface for the global Iris coordinator role.

use std::sync::Arc;

use futures::future::BoxFuture;
use rho_core::{ToolCall, ToolName, ToolOutput, ToolSpec, ToolType};
use serde_json::json;

pub const PROMPT: &str = r#"## Iris

You are the backend executor for Iris, the single global assistant in the Rho GUI. The user experiences the realtime voice and you as one unified assistant. Never mention a backend, handoff, or separate components. You control the user's fleet of agents; you are not an ordinary worker and never inspect or modify repositories yourself.

Requests arrive as realtime transcripts and may omit punctuation or contain recognition errors. Keep responses concise and action-oriented so the voice can respond quickly. Use the Iris control tools for current status and every control action; never claim an action succeeded without its tool result. Ask a brief spoken clarification only when needed to avoid a materially harmful mistake.

Follow this control workflow:
- List agents when current fleet state or the intended target is unclear. Do not list reflexively when the target is already unambiguous.
- Prefer an existing responsible agent over starting a duplicate. Send with immediate delivery to steer active work; use next_turn for a distinct follow-up that should begin after the current turn. Start a new agent when the user asks or no suitable owner exists.
- Starting or sending subscribes you to that agent's future completed replies and errors. Results arrive as ordinary agent messages containing the result: summarize that mail directly. Use iris_get_agent_reply only when the user explicitly asks what an agent last said or when recovering a result that is not present in your context; it returns the latest non-empty final answer, not errors or commentary. Unsubscribe only when the user no longer wants future updates.
- Continue only a known native Rho agent that is blocked in an error or unfinished turn; continuing has no effect on Claude agents. Cancellation stops the current turn and clears queued inputs. Hiding or unsubscribing does not cancel work.
- Marking done acknowledges the named agent and its visible descendants only after all of them have stopped working. Snoozing uses the same scope and may cover working agents; their eventual results stay quiet until the snooze expires. Neither operation cancels work or changes subscriptions. Never mark an agent done merely because you received or summarized its result.
- Before cancellation, hiding, renaming, or another state-changing operation, ask for confirmation unless the user's request already explicitly authorizes that exact action.

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
            "List every user-visible Rho agent with its handle, name, current attention state, and latest request.",
            json!({"type":"object","additionalProperties":false}),
        ),
        spec(
            "iris_start_agent",
            "Start a new agent, subscribe Iris to its future completed replies and errors, and send its initial request. Omit project only when exactly one project is registered.",
            json!({
                "type":"object","additionalProperties":false,"required":["prompt"],
                "properties":{
                    "prompt":{"type":"string"},
                    "task_name":{"type":"string","description":"Short display name."},
                    "project":{"type":"string","description":"Registered project name or absolute repository path."},
                    "role":{"type":"string","enum":["eng-mini","eng-low","eng-cheap","eng","eng-high","eng-ultra","eng-alt","pm"]}
                }
            }),
        ),
        spec(
            "iris_send_agent",
            "Send an existing agent a message and subscribe Iris to its future completed replies and errors. Immediate delivery steers active work; next_turn starts a distinct follow-up after the current turn finishes.",
            json!({
                "type":"object","additionalProperties":false,"required":["agent","message"],
                "properties":{
                    "agent":{"type":"string"},"message":{"type":"string"},
                    "delivery":{"type":"string","enum":["immediate","next_turn"]}
                }
            }),
        ),
        spec(
            "iris_unsubscribe_agent",
            "Stop receiving an agent's future terminal responses.",
            target_schema(),
        ),
        spec(
            "iris_get_agent_reply",
            "Get an agent's latest non-empty final answer from its transcript. This does not return errors or commentary; use result mail directly when available.",
            target_schema(),
        ),
        spec(
            "iris_mark_agent_done",
            "Acknowledge an agent and its currently visible descendants in the GUI. Every scoped agent must have stopped working. This keeps agents visible and does not cancel work or change Iris subscriptions.",
            target_schema(),
        ),
        spec(
            "iris_snooze_agent",
            "Snooze an agent and its currently visible descendants for a number of minutes. Work continues, and turns that finish during the snooze stay quiet until it expires. This does not change Iris subscriptions.",
            json!({
                "type":"object","additionalProperties":false,"required":["agent","duration_minutes"],
                "properties":{
                    "agent":{"type":"string"},
                    "duration_minutes":{"type":"integer","minimum":1}
                }
            }),
        ),
        spec(
            "iris_cancel_agent",
            "Cancel an agent's current turn and clear its queued inputs without deleting the agent.",
            target_schema(),
        ),
        spec(
            "iris_continue_agent",
            "Resume a native Rho agent blocked in an error or unfinished turn. This has no effect on Claude agents.",
            target_schema(),
        ),
        spec(
            "iris_rename_agent",
            "Rename an agent.",
            json!({"type":"object","additionalProperties":false,"required":["agent","name"],"properties":{"agent":{"type":"string"},"name":{"type":"string"}}}),
        ),
        spec(
            "iris_set_agent_visibility",
            "Show or hide an agent in the GUI.",
            json!({"type":"object","additionalProperties":false,"required":["agent","hidden"],"properties":{"agent":{"type":"string"},"hidden":{"type":"boolean"}}}),
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
        assert_eq!(names.len(), 11);
        assert!(names.iter().all(|name| name.starts_with("iris_")));
        assert!(!names.iter().any(|name| name.contains("desk")));
        assert!(PROMPT.contains("single global assistant"));
        assert!(PROMPT.contains("Cancellation stops the current turn and clears queued inputs"));
        assert!(PROMPT.contains("Use iris_get_agent_reply only"));
        assert!(!PROMPT.contains("Desk"));
    }
}
