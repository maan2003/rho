//! Built-in bridge for the optional Slack thread reply tool.

use futures::future::BoxFuture;
use rho_core::{ToolCall, ToolName, ToolOutput, ToolSpec, ToolType};

pub const REPLY_TOOL_NAME: &str = "slack_reply";

pub trait SlackToolHost: Send + Sync + 'static {
    fn has_agent(&self, agent_id: crate::db::AgentId) -> bool;
    fn call(&self, agent_id: crate::db::AgentId, call: ToolCall) -> BoxFuture<'static, ToolOutput>;
}

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: ToolName::try_from(REPLY_TOOL_NAME).expect("valid tool name"),
        tool_type: ToolType::Function,
        description: "Post a message to this agent's mapped Slack thread. Use this when you want Slack users to see a reply; final answers are not posted automatically.".to_owned(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "The Slack reply text to post."
                }
            },
            "required": ["text"],
            "additionalProperties": false
        }),
        format: None,
    }
}

pub type SharedSlackToolHost = std::sync::Arc<dyn SlackToolHost>;
