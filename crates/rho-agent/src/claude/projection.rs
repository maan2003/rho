use std::sync::Arc;

use anyhow::Context as _;
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

/// The row one transcript line becomes, or `None` for a line a reader
/// never sees: a CLI command's own output, an empty message, a
/// thinking-only block.
///
/// `usage_told` is the id of the API message whose usage a row already
/// carries: Claude writes one line per content block and repeats the
/// message's usage on each, and a reader counts a request once.
pub(super) fn transcript_line(
    row: &rho_claude::TranscriptRow,
    usage_model: crate::db::AgentUsageModel,
    usage_told: &mut Option<String>,
) -> anyhow::Result<Option<(Uuid, TranscriptLine, UnixMs)>> {
    match row {
        rho_claude::TranscriptRow::CompactBoundary {
            uuid,
            post_tokens,
            timestamp,
        } => Ok(Some((
            *uuid,
            TranscriptLine::Compacted {
                context_used: *post_tokens,
            },
            line_time(timestamp.as_deref()),
        ))),
        rho_claude::TranscriptRow::Message(message) => {
            let at = line_time(message.timestamp.as_deref());
            let line = match message.kind {
                rho_claude::SessionMessageKind::User => user_line(&message.message)?,
                rho_claude::SessionMessageKind::Assistant => {
                    assistant_line(&message.message, usage_model, usage_told)?
                }
                rho_claude::SessionMessageKind::System => None,
            };
            Ok(line.map(|line| (message.uuid, line, at)))
        }
    }
}

fn line_time(timestamp: Option<&str>) -> UnixMs {
    timestamp
        .and_then(|timestamp| chrono::DateTime::parse_from_rfc3339(timestamp).ok())
        .map(|time| UnixMs(time.timestamp_millis().max(0) as u64))
        .unwrap_or_else(UnixMs::now)
}

/// A person's line: the results it carries, else its text. Claude's own
/// command echoes (`<command-name>`, `<local-command-stdout>`) are not
/// something the person said.
fn user_line(message: &Value) -> anyhow::Result<Option<TranscriptLine>> {
    let mut text = String::new();
    let mut results = Vec::new();
    for content in message_content(message) {
        match content.get("type").and_then(Value::as_str) {
            None | Some("text") => {
                if let Some(part) = content
                    .get("text")
                    .or_else(|| content.get("content"))
                    .and_then(Value::as_str)
                    && !is_auxiliary_user_text(part)
                {
                    if !text.is_empty() && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(part);
                }
            }
            Some("image") => {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                let media_type = content
                    .get("source")
                    .and_then(|source| source.get("media_type"))
                    .and_then(Value::as_str)
                    .unwrap_or("image");
                text.push_str(match media_type {
                    "image/png" => "[image: PNG]",
                    "image/jpeg" => "[image: JPEG]",
                    "image/webp" => "[image: WebP]",
                    "image/gif" => "[image: GIF]",
                    _ => "[image]",
                });
            }
            Some("tool_result") => {
                if let Some(result) = project_tool_result(content)? {
                    results.push(result);
                }
            }
            _ => {}
        }
    }
    if !results.is_empty() {
        return Ok(Some(TranscriptLine::ToolResults { results }));
    }
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(TranscriptLine::User { text }))
}

fn assistant_line(
    message: &Value,
    usage_model: crate::db::AgentUsageModel,
    usage_told: &mut Option<String>,
) -> anyhow::Result<Option<TranscriptLine>> {
    let mut text = String::new();
    let mut calls = Vec::new();
    for content in message_content(message) {
        match content.get("type").and_then(Value::as_str) {
            Some("text") => push_text(&mut text, content),
            Some("tool_use") => {
                let id = content
                    .get("id")
                    .and_then(Value::as_str)
                    .context("Claude tool_use missing id")?;
                let name = content
                    .get("name")
                    .and_then(Value::as_str)
                    .context("Claude tool_use missing name")?;
                let input = content.get("input").cloned().unwrap_or(Value::Null);
                calls.push(TranscriptCall {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    arguments: serde_json::to_string(&input)?,
                });
            }
            _ => {}
        }
    }
    if text.trim().is_empty() && calls.is_empty() {
        return Ok(None);
    }
    let usage = message
        .get("usage")
        .cloned()
        .and_then(|usage| serde_json::from_value::<rho_claude::protocol::TokenUsage>(usage).ok());
    let context_used = usage.as_ref().map(|usage| usage.context_total());
    let message_id = message.get("id").and_then(Value::as_str).map(str::to_owned);
    let usage = match (usage, message_id) {
        (Some(usage), Some(message_id)) if usage_told.as_deref() != Some(&message_id) => {
            *usage_told = Some(message_id);
            Some(usage_bucket(&usage, usage_model))
        }
        (Some(usage), None) => Some(usage_bucket(&usage, usage_model)),
        _ => None,
    };
    Ok(Some(TranscriptLine::Assistant {
        text,
        calls,
        usage,
        context_used,
    }))
}

fn usage_bucket(
    usage: &rho_claude::protocol::TokenUsage,
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
            images: std::sync::Arc::new(Vec::new()),
            output: Arc::new(output),
            status,
        },
        started_at: UnixMs(0),
        finished_at: UnixMs(0),
        metadata: None,
    }))
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

    fn row(kind: rho_claude::SessionMessageKind, message: Value) -> rho_claude::TranscriptRow {
        rho_claude::TranscriptRow::Message(rho_claude::SessionMessage {
            kind,
            uuid: uuid::uuid!("00000000-0000-4000-8000-000000000001"),
            session_id: uuid::uuid!("00000000-0000-4000-8000-000000000002"),
            message,
            parent_tool_use_id: None,
            timestamp: Some("2026-09-06T10:00:00.000Z".to_owned()),
        })
    }

    fn line(row: &rho_claude::TranscriptRow) -> Option<TranscriptLine> {
        transcript_line(row, crate::db::AgentUsageModel::OPUS, &mut None)
            .unwrap()
            .map(|(_, line, _)| line)
    }

    #[test]
    fn a_persons_text_is_a_user_line_with_the_files_time() {
        let row = row(
            rho_claude::SessionMessageKind::User,
            json!({"role": "user", "content": [{"type": "text", "text": "hello"}]}),
        );
        let (uuid, line, at) = transcript_line(&row, crate::db::AgentUsageModel::OPUS, &mut None)
            .unwrap()
            .unwrap();
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
        let row = row(
            rho_claude::SessionMessageKind::User,
            json!({"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
            ]}),
        );
        assert_eq!(
            line(&row),
            Some(TranscriptLine::User {
                text: "look\n[image: PNG]".to_owned()
            })
        );
    }

    #[test]
    fn a_commands_own_output_is_not_a_line() {
        let command = row(
            rho_claude::SessionMessageKind::User,
            json!({"role": "user", "content": "<command-name>/compact</command-name>"}),
        );
        assert_eq!(line(&command), None);
        let stdout = row(
            rho_claude::SessionMessageKind::User,
            json!({"role": "user", "content": [{"type": "text", "text": "<local-command-stdout>ok</local-command-stdout>"}]}),
        );
        assert_eq!(line(&stdout), None);
    }

    #[test]
    fn a_failed_result_is_an_error() {
        let row = row(
            rho_claude::SessionMessageKind::User,
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "is_error": true, "content": "boom"},
            ]}),
        );
        let Some(TranscriptLine::ToolResults { results }) = line(&row) else {
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
        let text = row(
            rho_claude::SessionMessageKind::Assistant,
            json!({"role": "assistant", "id": "msg_1", "usage": usage, "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": "reading"},
            ]}),
        );
        let call = row(
            rho_claude::SessionMessageKind::Assistant,
            json!({"role": "assistant", "id": "msg_1", "usage": usage, "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {"path": "a.rs"}},
            ]}),
        );
        let mut told = None;
        let (_, first, _) = transcript_line(&text, crate::db::AgentUsageModel::OPUS, &mut told)
            .unwrap()
            .unwrap();
        let (_, second, _) = transcript_line(&call, crate::db::AgentUsageModel::OPUS, &mut told)
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
        let row = row(
            rho_claude::SessionMessageKind::Assistant,
            json!({"role": "assistant", "id": "msg_2", "content": [{"type": "thinking", "thinking": "hmm"}]}),
        );
        assert_eq!(line(&row), None);
    }

    #[test]
    fn a_compaction_boundary_is_compacted() {
        let row = rho_claude::TranscriptRow::CompactBoundary {
            uuid: uuid::uuid!("00000000-0000-4000-8000-000000000003"),
            post_tokens: Some(2000),
            timestamp: None,
        };
        assert_eq!(
            line(&row),
            Some(TranscriptLine::Compacted {
                context_used: Some(2000)
            })
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
