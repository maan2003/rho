//! Small shared vocabulary for rho crates.
//!
//! This crate intentionally avoids owning agent policy. Harnesses, providers,
//! tools, and stores can add their own richer types around these basics.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ItemId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolCallId(pub String);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub id: ItemId,
    pub kind: ItemKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ItemBlock {
    Local {
        items: Vec<Item>,
    },
    InferenceResponse {
        #[serde(skip_serializing_if = "Option::is_none")]
        provider_response_id: Option<String>,
        items: Vec<Item>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ItemKind {
    Message(Message),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
    ReasoningText(ReasoningText),
    ProviderItem(ProviderItem),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentPart>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<MessagePhase>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentPart {
    Text { text: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessagePhase {
    Commentary,
    FinalAnswer,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub tool_type: ToolType,
    pub description: String,
    pub input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<ToolFormat>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: ToolCallId,
    pub name: String,
    pub tool_type: ToolType,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: ToolCallId,
    pub tool_type: ToolType,
    pub status: ToolResultStatus,
    pub output: ToolOutput,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolType {
    #[default]
    Function,
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolGrammarSyntax {
    Lark,
    Regex,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolFormat {
    Text,
    Grammar {
        syntax: ToolGrammarSyntax,
        definition: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolResultStatus {
    Success,
    Error { message: String },
    Cancelled { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningText {
    pub kind: ReasoningTextKind,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ReasoningTextKind {
    Summary,
    Full,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderItem {
    pub kind: ProviderItemKind,
    pub payload: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderItemKind {
    Reasoning,
    Compaction,
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InferenceRequest {
    pub input: Vec<ItemBlock>,
    pub tools: Vec<ToolSpec>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InferenceResponse {
    pub items: Vec<ItemKind>,
    pub usage: Option<TokenUsage>,
    pub provider_response_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum InferenceUpdate {
    TextDelta {
        output_index: usize,
        text: String,
    },
    ReasoningTextDelta {
        output_index: usize,
        kind: ReasoningTextKind,
        text: String,
    },
    ToolCall {
        output_index: usize,
        call: ToolCall,
    },
    OutputItem {
        output_index: usize,
        item: ItemKind,
    },
    CompactionStarted {
        output_index: usize,
    },
    Usage(TokenUsage),
    ResponseId(String),
    Finished(InferenceResponse),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
}

impl Item {
    pub fn message(id: impl Into<String>, role: Role, content: impl Into<String>) -> Self {
        Self {
            id: ItemId(id.into()),
            kind: ItemKind::Message(Message::text(role, content)),
        }
    }
}

impl Message {
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentPart::Text { text: text.into() }],
            phase: None,
        }
    }

    pub fn with_phase(mut self, phase: MessagePhase) -> Self {
        self.phase = Some(phase);
        self
    }

    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .map(|part| match part {
                ContentPart::Text { text } => text.as_str(),
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

impl ToolResult {
    pub fn success(call_id: ToolCallId, content: impl Into<String>) -> Self {
        Self {
            call_id,
            tool_type: ToolType::Function,
            status: ToolResultStatus::Success,
            output: ToolOutput {
                content: content.into(),
            },
        }
    }

    pub fn error(call_id: ToolCallId, message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            call_id,
            tool_type: ToolType::Function,
            status: ToolResultStatus::Error {
                message: message.clone(),
            },
            output: ToolOutput { content: message },
        }
    }

    pub fn cancelled(call_id: ToolCallId, reason: impl Into<String>) -> Self {
        Self {
            call_id,
            tool_type: ToolType::Function,
            status: ToolResultStatus::Cancelled {
                reason: reason.into(),
            },
            output: ToolOutput {
                content: String::new(),
            },
        }
    }

    pub fn rendered_output(&self) -> String {
        match &self.status {
            ToolResultStatus::Success => self.output.content.clone(),
            ToolResultStatus::Error { message } => {
                format!("error: {message}\n\n{}", self.output.content)
            }
            ToolResultStatus::Cancelled { reason } => format!("cancelled: {reason}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn optional_core_fields_serialize_only_when_present() {
        let message = Message::text(Role::User, "hello");
        let message_json = serde_json::to_value(&message).unwrap();
        assert!(message_json.get("phase").is_none());

        let tool = ToolSpec {
            name: "shell_command".to_owned(),
            tool_type: ToolType::Function,
            description: "run a shell command".to_owned(),
            input_schema: json!({"type": "object"}),
            format: None,
        };
        let tool_json = serde_json::to_value(&tool).unwrap();
        assert!(tool_json.get("format").is_none());

        let block = ItemBlock::InferenceResponse {
            provider_response_id: None,
            items: Vec::new(),
        };
        let block_json = serde_json::to_value(&block).unwrap();
        assert!(
            block_json
                .get("InferenceResponse")
                .unwrap()
                .get("provider_response_id")
                .is_none()
        );
    }

    #[test]
    fn optional_core_fields_deserialize_when_missing() {
        let message: Message = serde_json::from_value(json!({
            "role": "User",
            "content": [{ "Text": { "text": "hello" } }]
        }))
        .unwrap();
        assert_eq!(message.phase, None);

        let tool: ToolSpec = serde_json::from_value(json!({
            "name": "shell_command",
            "tool_type": "Function",
            "description": "run a shell command",
            "input_schema": { "type": "object" }
        }))
        .unwrap();
        assert_eq!(tool.format, None);

        let block: ItemBlock = serde_json::from_value(json!({
            "InferenceResponse": {
                "items": []
            }
        }))
        .unwrap();
        assert_eq!(
            block,
            ItemBlock::InferenceResponse {
                provider_response_id: None,
                items: Vec::new()
            }
        );
    }
}
