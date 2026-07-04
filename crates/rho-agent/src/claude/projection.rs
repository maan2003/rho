use std::sync::Arc;

use anyhow::Context as _;
use rho_core::{
    ContentPart, ContextBlock, InferenceResponseItem, StreamingContextItem, ToolCallId, ToolName,
    ToolOutput, ToolOutputStatus, ToolResult, ToolType, UnixMs,
};
use serde_json::Value;
use uuid::Uuid;

pub(super) enum ClaudeStreamItem {
    Text(String),
    Thinking(String),
    ToolUse {
        id: String,
        name: String,
        arguments: String,
    },
}

impl ClaudeStreamItem {
    pub(super) fn from_content_block(
        block: rho_claude::protocol::StreamContentBlock,
    ) -> anyhow::Result<Self> {
        Ok(match block {
            rho_claude::protocol::StreamContentBlock::Text { text } => Self::Text(text),
            rho_claude::protocol::StreamContentBlock::Thinking { thinking, .. } => {
                Self::Thinking(thinking)
            }
            rho_claude::protocol::StreamContentBlock::ToolUse { id, name, input }
            | rho_claude::protocol::StreamContentBlock::ServerToolUse { id, name, input } => {
                Self::ToolUse {
                    id,
                    name,
                    arguments: serde_json::to_string(&input)?,
                }
            }
            rho_claude::protocol::StreamContentBlock::RedactedThinking { data } => {
                Self::Thinking(data)
            }
            rho_claude::protocol::StreamContentBlock::WebSearchToolResult { content, .. } => {
                Self::Text(serde_json::to_string(&content)?)
            }
        })
    }

    pub(super) fn apply_delta(
        &mut self,
        delta: rho_claude::protocol::ContentBlockDelta,
    ) -> anyhow::Result<()> {
        match (self, delta) {
            (
                Self::Text(text),
                rho_claude::protocol::ContentBlockDelta::TextDelta { text: delta },
            ) => {
                text.push_str(&delta);
            }
            (
                Self::Thinking(thinking),
                rho_claude::protocol::ContentBlockDelta::ThinkingDelta { thinking: delta },
            ) => {
                thinking.push_str(&delta);
            }
            (
                Self::ToolUse { arguments, .. },
                rho_claude::protocol::ContentBlockDelta::InputJsonDelta { partial_json },
            ) => {
                if arguments == "null" || arguments == "{}" {
                    arguments.clear();
                }
                arguments.push_str(&partial_json);
            }
            (_, rho_claude::protocol::ContentBlockDelta::SignatureDelta { .. })
            | (_, rho_claude::protocol::ContentBlockDelta::CitationsDelta { .. }) => {}
            _ => {}
        }
        Ok(())
    }

    pub(super) fn to_streaming_context_item(&self) -> anyhow::Result<StreamingContextItem> {
        Ok(match self {
            Self::Text(text) => StreamingContextItem::AssistantMessage {
                content: vec![text.as_str().into()],
                phase: None,
            },
            Self::Thinking(thinking) => StreamingContextItem::RawReasoning {
                content: thinking.as_str().into(),
                summary: Vec::new(),
            },
            Self::ToolUse {
                id,
                name,
                arguments,
            } => StreamingContextItem::ToolCall {
                id: ToolCallId::try_from(id.as_str())?,
                name: ToolName::try_from(name.as_str())?,
                tool_type: ToolType::Function,
                arguments: arguments.as_str().into(),
            },
        })
    }
}

pub fn transcript_messages_to_context(
    messages: &[rho_claude::SessionMessage],
) -> anyhow::Result<Vec<Arc<ContextBlock>>> {
    messages
        .iter()
        .filter_map(transcript_message_to_context)
        .collect()
}

pub(super) fn assistant_message_to_block(
    message: rho_claude::protocol::AssistantMessage,
) -> anyhow::Result<Arc<ContextBlock>> {
    let message = rho_claude::SessionMessage {
        kind: rho_claude::SessionMessageKind::Assistant,
        uuid: message
            .uuid
            .and_then(|uuid| Uuid::parse_str(&uuid).ok())
            .unwrap_or_else(Uuid::new_v4),
        session_id: message.session_id.unwrap_or_else(Uuid::new_v4),
        message: serde_json::to_value(message.message)?,
        parent_tool_use_id: message.parent_tool_use_id,
        timestamp: None,
    };
    let mut blocks = transcript_messages_to_context(&[message])?;
    blocks
        .pop()
        .context("assistant message projected no blocks")
}

pub(super) fn user_output_to_block(
    message: rho_claude::protocol::UserOutputMessage,
) -> anyhow::Result<Option<Arc<ContextBlock>>> {
    if message.is_synthetic.unwrap_or(false) || message.is_replay.unwrap_or(false) {
        return Ok(None);
    }
    let Some(output) = message.message else {
        return Ok(None);
    };
    let message = rho_claude::SessionMessage {
        kind: rho_claude::SessionMessageKind::User,
        uuid: message
            .uuid
            .and_then(|uuid| Uuid::parse_str(&uuid).ok())
            .unwrap_or_else(Uuid::new_v4),
        session_id: message.session_id.unwrap_or_else(Uuid::new_v4),
        message: serde_json::to_value(output)?,
        parent_tool_use_id: message.parent_tool_use_id,
        timestamp: None,
    };
    Ok(transcript_messages_to_context(&[message])?.pop())
}

fn transcript_message_to_context(
    message: &rho_claude::SessionMessage,
) -> Option<anyhow::Result<Arc<ContextBlock>>> {
    match message.kind {
        rho_claude::SessionMessageKind::User => Some(project_user_message(message)),
        rho_claude::SessionMessageKind::Assistant => Some(project_assistant_message(message)),
        rho_claude::SessionMessageKind::System => None,
    }
}

fn project_user_message(message: &rho_claude::SessionMessage) -> anyhow::Result<Arc<ContextBlock>> {
    let mut text = String::new();
    let mut results = Vec::new();
    for content in message_content(&message.message) {
        match content.get("type").and_then(Value::as_str) {
            Some("text") => push_text(&mut text, content),
            Some("tool_result") => {
                if let Some(result) = project_tool_result(content)? {
                    results.push(result);
                }
            }
            _ => {}
        }
    }
    if !results.is_empty() {
        return Ok(Arc::new(ContextBlock::ToolResults { results }));
    }
    Ok(Arc::new(ContextBlock::UserMessage {
        content: vec![ContentPart::Text { text }],
    }))
}

fn project_assistant_message(
    message: &rho_claude::SessionMessage,
) -> anyhow::Result<Arc<ContextBlock>> {
    let mut items = Vec::new();
    let mut text = String::new();
    for content in message_content(&message.message) {
        match content.get("type").and_then(Value::as_str) {
            Some("text") => push_text(&mut text, content),
            Some("thinking") => {
                flush_text(&mut text, &mut items);
                if let Some(thinking) = content.get("thinking").and_then(Value::as_str) {
                    items.push(InferenceResponseItem::RawReasoning {
                        content: thinking.to_owned(),
                        summary: Vec::new(),
                    });
                }
            }
            Some("tool_use") => {
                flush_text(&mut text, &mut items);
                items.push(project_tool_call(content)?);
            }
            _ => {}
        }
    }
    flush_text(&mut text, &mut items);
    Ok(Arc::new(ContextBlock::InferenceResponse {
        items,
        provider_response_id: None,
    }))
}

fn message_content(message: &Value) -> Vec<&Value> {
    match message.get("content") {
        Some(Value::Array(content)) => content.iter().collect(),
        Some(Value::String(_)) => vec![message],
        _ => Vec::new(),
    }
}

fn push_text(output: &mut String, content: &Value) {
    if let Some(text) = content
        .get("text")
        .or_else(|| content.get("content"))
        .and_then(Value::as_str)
    {
        output.push_str(text);
    }
}

fn flush_text(text: &mut String, items: &mut Vec<InferenceResponseItem>) {
    if text.is_empty() {
        return;
    }
    items.push(InferenceResponseItem::AssistantMessage {
        content: vec![ContentPart::Text {
            text: std::mem::take(text),
        }],
        phase: None,
    });
}

fn project_tool_call(content: &Value) -> anyhow::Result<InferenceResponseItem> {
    let id = content
        .get("id")
        .and_then(Value::as_str)
        .context("Claude tool_use missing id")?;
    let name = content
        .get("name")
        .and_then(Value::as_str)
        .context("Claude tool_use missing name")?;
    let input = content.get("input").cloned().unwrap_or(Value::Null);
    Ok(InferenceResponseItem::ToolCall {
        id: ToolCallId::try_from(id)?,
        name: ToolName::try_from(name)?,
        tool_type: ToolType::Function,
        arguments: serde_json::to_string(&input)?,
    })
}

fn project_tool_result(content: &Value) -> anyhow::Result<Option<ToolResult>> {
    let Some(tool_use_id) = content.get("tool_use_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    let output = match content.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        Some(other) => serde_json::to_string(other)?,
        None => String::new(),
    };
    let status = if content
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        ToolOutputStatus::Error
    } else {
        ToolOutputStatus::Success
    };
    Ok(Some(ToolResult {
        call_id: ToolCallId::try_from(tool_use_id)?,
        tool_type: ToolType::Function,
        body: ToolOutput {
            output: Arc::new(output),
            status,
        },
        started_at: UnixMs(0),
        finished_at: UnixMs(0),
        metadata: None,
    }))
}

#[cfg(test)]
mod tests {
    use rho_core::text_content;
    use serde_json::json;

    use super::*;

    fn session_message(
        kind: rho_claude::SessionMessageKind,
        message: Value,
    ) -> rho_claude::SessionMessage {
        rho_claude::SessionMessage {
            kind,
            uuid: uuid::uuid!("00000000-0000-4000-8000-000000000001"),
            session_id: uuid::uuid!("00000000-0000-4000-8000-000000000002"),
            message,
            parent_tool_use_id: None,
            timestamp: None,
        }
    }

    #[test]
    fn projects_user_text() {
        let blocks = transcript_messages_to_context(&[session_message(
            rho_claude::SessionMessageKind::User,
            json!({"role": "user", "content": [{"type": "text", "text": "hello"}]}),
        )])
        .unwrap();

        let ContextBlock::UserMessage { content } = blocks[0].as_ref() else {
            panic!("expected user message");
        };
        assert_eq!(text_content(content), "hello");
    }

    #[test]
    fn ignores_synthetic_user_output() {
        let message = serde_json::from_value(json!({
            "message": {
                "role": "user",
                "content": "This session is being continued from a previous conversation."
            },
            "isSynthetic": true
        }))
        .unwrap();

        assert!(user_output_to_block(message).unwrap().is_none());
    }

    #[test]
    fn ignores_replayed_user_output() {
        let message = serde_json::from_value(json!({
            "message": {
                "role": "user",
                "content": "<task-notification><status>completed</status></task-notification>"
            },
            "isReplay": true
        }))
        .unwrap();

        assert!(user_output_to_block(message).unwrap().is_none());
    }

    #[test]
    fn projects_assistant_text_and_tool_call() {
        let blocks = transcript_messages_to_context(&[session_message(
            rho_claude::SessionMessageKind::Assistant,
            json!({
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "I'll check."},
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "pwd"}},
                ],
            }),
        )])
        .unwrap();

        let ContextBlock::InferenceResponse { items, .. } = blocks[0].as_ref() else {
            panic!("expected inference response");
        };
        assert!(
            matches!(&items[0], InferenceResponseItem::AssistantMessage { content, .. } if text_content(content) == "I'll check.")
        );
        assert!(
            matches!(&items[1], InferenceResponseItem::ToolCall { id, name, arguments, .. }
            if id.as_ref() == "toolu_1" && name.as_ref() == "Bash" && arguments.contains("pwd"))
        );
    }
}
