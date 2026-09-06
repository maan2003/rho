use std::sync::Arc;

use rho_claude::protocol::{
    AssistantContent, AssistantMessage, OutputContent, SystemCompactMetadata, TokenUsage,
    UserOutputMessage,
};
use rho_core::{
    ProviderSpecificData, StreamingContextItem, ToolCallId, ToolName, ToolOutput, ToolOutputStatus,
    ToolResult, ToolType, UnixMs,
};
use senax_encoder::{Decode, Decoder, Encode, TaggedSenax};
use serde_json::Value;
use uuid::Uuid;

use crate::{TranscriptCall, TranscriptLine};

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
struct ClaudeProviderSpecificData;

impl TaggedSenax for ClaudeProviderSpecificData {
    const TAG: &'static str = "claude.projected";
}

senax_encoder::__private::inventory::submit! {
    rho_core::__SenaxProviderSpecificDataEntry::new(
        ClaudeProviderSpecificData::TAG,
        |mut body: bytes::Bytes| -> senax_encoder::Result<Box<dyn ProviderSpecificData>> {
            use bytes::Buf as _;
            let value = ClaudeProviderSpecificData::decode(&mut body)?;
            if body.remaining() != 0 {
                return Err(senax_encoder::EncoderError::Decode(format!(
                    "Trailing bytes while decoding registered tagged senax value '{}': {}",
                    ClaudeProviderSpecificData::TAG,
                    body.remaining()
                )));
            }
            Ok(Box::new(value) as Box<dyn ProviderSpecificData>)
        },
    )
}

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
    ) -> anyhow::Result<Option<Self>> {
        Ok(Some(match block {
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
            rho_claude::protocol::StreamContentBlock::Other => return Ok(None),
        }))
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
            | (_, rho_claude::protocol::ContentBlockDelta::CitationsDelta { .. })
            | (_, rho_claude::protocol::ContentBlockDelta::Other) => {}
            _ => {}
        }
        Ok(())
    }

    pub(super) fn to_streaming_context_item(&self) -> anyhow::Result<StreamingContextItem> {
        Ok(match self {
            Self::Text(text) => StreamingContextItem::AssistantMessage {
                provider_specific: Box::new(ClaudeProviderSpecificData),
                content: vec![text.as_str().into()],
                phase: None,
            },
            Self::Thinking(thinking) => StreamingContextItem::RawReasoning {
                provider_specific: Box::new(ClaudeProviderSpecificData),
                content: thinking.as_str().into(),
                summary: Vec::new(),
            },
            Self::ToolUse {
                id,
                name,
                arguments,
            } => StreamingContextItem::ToolCall {
                provider_specific: Box::new(ClaudeProviderSpecificData),
                id: ToolCallId::try_from(id.as_str())?,
                name: ToolName::try_from(name.as_str())?,
                tool_type: ToolType::Function,
                arguments: arguments.as_str().into(),
            },
        })
    }
}

/// The row an `assistant` event of the stream becomes: one finished
/// content block, with the message's id, usage, uuid and time. `None`
/// for a block a reader never sees (thinking only, empty).
///
/// `usage_told` is the id of the API message whose usage a row already
/// carries: every block's event repeats the message's usage, and a
/// reader counts a request once.
pub(super) fn assistant_row(
    message: &AssistantMessage,
    usage_model: crate::db::AgentUsageModel,
    usage_told: &mut Option<String>,
) -> anyhow::Result<Option<(Uuid, TranscriptLine, UnixMs)>> {
    let mut text = String::new();
    let mut calls = Vec::new();
    for content in &message.message.content {
        match content {
            AssistantContent::Text { text: part } => text.push_str(part),
            AssistantContent::ToolUse { id, name, input } => calls.push(TranscriptCall {
                id: id.clone(),
                name: name.clone(),
                arguments: serde_json::to_string(input)?,
            }),
            AssistantContent::Thinking { .. } | AssistantContent::Other => {}
        }
    }
    if text.trim().is_empty() && calls.is_empty() {
        return Ok(None);
    }
    let usage = message.message.usage.as_ref();
    let context_used = usage.map(TokenUsage::context_total);
    let usage = match (usage, &message.message.id) {
        (Some(usage), Some(id)) if usage_told.as_deref() != Some(id.as_str()) => {
            *usage_told = Some(id.clone());
            Some(usage_bucket(usage, usage_model))
        }
        (Some(usage), None) => Some(usage_bucket(usage, usage_model)),
        _ => None,
    };
    Ok(Some((
        row_uuid(message.uuid.as_deref()),
        TranscriptLine::Assistant {
            text,
            calls,
            usage,
            context_used,
        },
        line_time(message.timestamp.as_deref()),
    )))
}

/// The row a `user` event of the stream becomes: the results it
/// carries, else what the person said. Claude's own command echoes
/// (`<command-name>`, `<local-command-stdout>`) are not something the
/// person said.
pub(super) fn user_row(
    message: &UserOutputMessage,
) -> anyhow::Result<Option<(Uuid, TranscriptLine, UnixMs)>> {
    let Some(body) = &message.message else {
        return Ok(None);
    };
    let mut text = String::new();
    let mut results = Vec::new();
    for content in &body.content {
        match content {
            OutputContent::Text { text: part } => {
                if !is_auxiliary_user_text(part) {
                    push_line(&mut text, part);
                }
            }
            OutputContent::Image { source } => push_line(&mut text, image_marker(source)),
            OutputContent::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => results.push(tool_result(
                tool_use_id,
                content,
                is_error.unwrap_or(false),
            )?),
            OutputContent::Other => {}
        }
    }
    let uuid = row_uuid(message.uuid.as_deref());
    let at = line_time(message.timestamp.as_deref());
    if !results.is_empty() {
        return Ok(Some((uuid, TranscriptLine::ToolResults { results }, at)));
    }
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some((uuid, TranscriptLine::User { text }, at)))
}

/// The row a `compact_boundary` becomes.
pub(super) fn compacted_row(
    uuid: Option<&str>,
    metadata: Option<&SystemCompactMetadata>,
) -> (Uuid, TranscriptLine, UnixMs) {
    (
        row_uuid(uuid),
        TranscriptLine::Compacted {
            context_used: metadata.and_then(|metadata| metadata.post_tokens),
        },
        UnixMs::now(),
    )
}

/// The line's uuid as Claude names it (the same in its file); one of
/// Rho's own for a message that came without one.
fn row_uuid(uuid: Option<&str>) -> Uuid {
    uuid.and_then(|uuid| Uuid::parse_str(uuid).ok())
        .unwrap_or_else(Uuid::new_v4)
}

pub(super) fn line_time(timestamp: Option<&str>) -> UnixMs {
    timestamp
        .and_then(|timestamp| chrono::DateTime::parse_from_rfc3339(timestamp).ok())
        .map(|time| UnixMs(time.timestamp_millis().max(0) as u64))
        .unwrap_or_else(UnixMs::now)
}

fn push_line(output: &mut String, part: &str) {
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(part);
}

fn image_marker(source: &Value) -> &'static str {
    match source.get("media_type").and_then(Value::as_str) {
        Some("image/png") => "[image: PNG]",
        Some("image/jpeg") => "[image: JPEG]",
        Some("image/webp") => "[image: WebP]",
        Some("image/gif") => "[image: GIF]",
        _ => "[image]",
    }
}

fn usage_bucket(
    usage: &TokenUsage,
    model: crate::db::AgentUsageModel,
) -> crate::db::AgentUsageBucket {
    crate::db::AgentUsageBucket {
        model,
        input_tokens: usage.input_tokens.unwrap_or(0),
        cache_read_tokens: usage.cache_read_input_tokens.unwrap_or(0),
        cache_write_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
        cache_write_1h_tokens: usage
            .cache_creation
            .as_ref()
            .and_then(|cache| cache.ephemeral_1h_input_tokens)
            .unwrap_or(0),
        output_tokens: usage.output_tokens.unwrap_or(0),
        requests: 1,
        ..crate::db::AgentUsageBucket::default()
    }
}

fn tool_result(tool_use_id: &str, content: &Value, is_error: bool) -> anyhow::Result<ToolResult> {
    let output = match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        Value::Null => String::new(),
        other => serde_json::to_string(other)?,
    };
    Ok(ToolResult {
        call_id: ToolCallId::try_from(tool_use_id)?,
        tool_type: ToolType::Function,
        body: ToolOutput {
            images: Arc::new(Vec::new()),
            output: Arc::new(output),
            status: if is_error {
                ToolOutputStatus::Error
            } else {
                ToolOutputStatus::Success
            },
        },
        started_at: UnixMs(0),
        finished_at: UnixMs(0),
        metadata: None,
    })
}

fn is_auxiliary_user_text(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with("<command-name>")
        || text.starts_with("<local-command-stdout>")
        || text.starts_with("<task-notification>")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const UUID: &str = "00000000-0000-4000-8000-000000000001";

    fn user(message: Value) -> UserOutputMessage {
        serde_json::from_value(json!({
            "uuid": UUID,
            "session_id": "00000000-0000-4000-8000-000000000002",
            "timestamp": "2026-09-06T10:00:00.000Z",
            "message": message,
        }))
        .unwrap()
    }

    fn assistant(message: Value) -> AssistantMessage {
        serde_json::from_value(json!({
            "uuid": UUID,
            "session_id": "00000000-0000-4000-8000-000000000002",
            "timestamp": "2026-09-06T10:00:01.000Z",
            "message": message,
        }))
        .unwrap()
    }

    fn user_line(message: Value) -> Option<TranscriptLine> {
        user_row(&user(message)).unwrap().map(|(_, line, _)| line)
    }

    fn assistant_line(message: Value) -> Option<TranscriptLine> {
        assistant_row(
            &assistant(message),
            crate::db::AgentUsageModel::OPUS,
            &mut None,
        )
        .unwrap()
        .map(|(_, line, _)| line)
    }

    #[test]
    fn a_persons_text_is_a_user_line_with_the_streams_time() {
        let message = user(json!({"role": "user", "content": [{"type": "text", "text": "hello"}]}));
        let (uuid, line, at) = user_row(&message).unwrap().unwrap();
        assert_eq!(uuid, uuid::uuid!("00000000-0000-4000-8000-000000000001"));
        assert_eq!(
            line,
            TranscriptLine::User {
                text: "hello".to_owned()
            }
        );
        assert_eq!(at, UnixMs(1788688800000));
    }

    #[test]
    fn images_become_bounded_markers() {
        assert_eq!(
            user_line(json!({"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
            ]})),
            Some(TranscriptLine::User {
                text: "look\n[image: PNG]".to_owned()
            })
        );
    }

    #[test]
    fn a_commands_own_output_is_not_a_line() {
        assert_eq!(
            user_line(json!({"role": "user", "content": "<command-name>/compact</command-name>"})),
            None
        );
        assert_eq!(
            user_line(
                json!({"role": "user", "content": [{"type": "text", "text": "<local-command-stdout>ok</local-command-stdout>"}]})
            ),
            None
        );
    }

    #[test]
    fn a_failed_result_is_an_error() {
        let Some(TranscriptLine::ToolResults { results }) = user_line(
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "is_error": true, "content": "boom"},
            ]}),
        ) else {
            panic!("expected results");
        };
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].call_id.as_str(), "toolu_1");
        assert_eq!(results[0].body.status, ToolOutputStatus::Error);
        assert_eq!(results[0].body.output.as_str(), "boom");
    }

    #[test]
    fn text_and_call_with_usage_told_once_per_message() {
        let usage = json!({"input_tokens": 3, "cache_read_input_tokens": 100, "output_tokens": 7});
        let text = assistant(
            json!({"role": "assistant", "id": "msg_1", "usage": usage, "content": [
                {"type": "text", "text": "reading"},
            ]}),
        );
        let call = assistant(
            json!({"role": "assistant", "id": "msg_1", "usage": usage, "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"path": "a.rs"}},
            ]}),
        );
        let mut told = None;
        let (_, first, _) = assistant_row(&text, crate::db::AgentUsageModel::OPUS, &mut told)
            .unwrap()
            .unwrap();
        let (_, second, _) = assistant_row(&call, crate::db::AgentUsageModel::OPUS, &mut told)
            .unwrap()
            .unwrap();
        let TranscriptLine::Assistant {
            text,
            calls,
            usage,
            context_used,
        } = first
        else {
            panic!("expected assistant");
        };
        assert_eq!(text, "reading");
        assert!(calls.is_empty());
        let usage = usage.expect("first row carries usage");
        assert_eq!(usage.cache_read_tokens, 100);
        assert_eq!(usage.requests, 1);
        assert_eq!(context_used, Some(110));
        let TranscriptLine::Assistant { calls, usage, .. } = second else {
            panic!("expected assistant");
        };
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "Read");
        assert_eq!(calls[0].arguments, r#"{"path":"a.rs"}"#);
        assert!(usage.is_none(), "the same message's usage is told once");
    }

    #[test]
    fn a_thinking_only_block_is_no_line() {
        assert_eq!(
            assistant_line(
                json!({"role": "assistant", "id": "msg_2", "content": [{"type": "thinking", "thinking": "hmm"}]})
            ),
            None
        );
    }

    #[test]
    fn a_compaction_boundary_is_compacted() {
        let metadata = SystemCompactMetadata {
            trigger: None,
            pre_tokens: Some(9000),
            post_tokens: Some(2000),
        };
        let (uuid, line, _) = compacted_row(Some(UUID), Some(&metadata));
        assert_eq!(uuid, uuid::uuid!("00000000-0000-4000-8000-000000000001"));
        assert_eq!(
            line,
            TranscriptLine::Compacted {
                context_used: Some(2000)
            }
        );
    }

    #[test]
    fn ignores_unknown_stream_content_block() {
        let item =
            ClaudeStreamItem::from_content_block(rho_claude::protocol::StreamContentBlock::Other)
                .unwrap();
        assert!(item.is_none());
    }
}
