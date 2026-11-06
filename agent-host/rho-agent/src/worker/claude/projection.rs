use std::collections::BTreeMap;

use rho_agent_types::AgentId;
use rho_agent_types::transcript::{ExecId, ToolName};
use rho_agents_client::protocol::transcript::{ArgumentsFormat, Item, StreamingResponse};
use rho_claude::protocol::{
    AssistantContent, AssistantMessage, OutputContent, SystemCompactMetadata, TokenUsage,
    UserOutputMessage,
};
use serde_json::Value;

use crate::TranscriptCall;
use crate::record::{CallResult, Event};

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

    /// Claude's visible live projection. Assistant prose belongs in the
    /// committed transcript, not in the live response.
    pub(super) fn to_live_item(&self) -> Option<Item> {
        match self {
            Self::Text(_) => None,
            Self::Thinking(text) => Some(Item::Reasoning { text: text.clone() }),
            Self::ToolUse {
                id,
                name,
                arguments,
            } => Some(Item::ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
                format: ArgumentsFormat::Json,
            }),
        }
    }

    /// A call's id and name are ones Rho can hold.
    pub(super) fn named_well(&self) -> bool {
        match self {
            Self::Text(_) | Self::Thinking(_) => true,
            Self::ToolUse { id, name, .. } => {
                ExecId::try_from(id.as_str()).is_ok() && ToolName::try_from(name.as_str()).is_ok()
            }
        }
    }
}

/// What the model has said that has no row yet. A finished block's row takes
/// it out of `items`, so this never repeats a row.
pub(super) fn said_so_far(items: &BTreeMap<usize, ClaudeStreamItem>) -> String {
    let said: Vec<&str> = items
        .values()
        .filter_map(|item| match item {
            ClaudeStreamItem::Text(text) if !text.trim().is_empty() => Some(text.as_str()),
            _ => None,
        })
        .collect();
    said.join("\n")
}

/// A complete replaceable snapshot of the visible blocks of one provider
/// response. The id belongs to the provider message, never to an item delta.
pub(super) fn live_response(
    id: Option<&str>,
    items: &BTreeMap<usize, ClaudeStreamItem>,
) -> Option<StreamingResponse> {
    let id = id?;
    let items: Vec<_> = items
        .values()
        .filter_map(ClaudeStreamItem::to_live_item)
        .collect();
    (!items.is_empty()).then_some(StreamingResponse {
        id: id.to_owned(),
        items,
    })
}

/// What the projection remembers from one line to the next.
///
/// `usage_told` is the id of the API message whose usage a row already
/// carries: every block's event repeats the message's usage, and a
/// reader counts a request once.
///
/// `answered` is the call the newest request answered: that request's row
/// holds what the stream's result says.
#[derive(Debug)]
pub(super) struct Projection {
    agent: AgentId,
    usage_told: Option<String>,
    pub(super) answered: Option<ExecId>,
}

impl Projection {
    pub(super) fn new(agent: AgentId) -> Self {
        Self {
            agent,
            usage_told: None,
            answered: None,
        }
    }
}

/// The row an `assistant` event of the stream becomes: one finished
/// content block, with the message's usage. `None` for a block a reader
/// never sees (thinking only, empty).
pub(super) fn reply_row(
    message: &AssistantMessage,
    usage_model: crate::log::AgentUsageModel,
    projection: &mut Projection,
) -> anyhow::Result<Option<Event<'static>>> {
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
        (Some(usage), Some(id)) if projection.usage_told.as_deref() != Some(id.as_str()) => {
            projection.usage_told = Some(id.clone());
            Some(usage_bucket(usage, usage_model))
        }
        (Some(usage), None) => Some(usage_bucket(usage, usage_model)),
        _ => None,
    };
    Ok(Some(Event::ClaudeReply {
        agent: projection.agent,
        text,
        calls,
        usage,
        context_used,
    }))
}

/// The row a `user` event of the stream becomes: the results no request
/// answered. An echo of text makes no row, since the request holds that text.
pub(super) fn results_row(
    message: &UserOutputMessage,
    projection: &Projection,
) -> anyhow::Result<Option<Event<'static>>> {
    let Some(body) = &message.message else {
        return Ok(None);
    };
    let mut results = Vec::new();
    for content in &body.content {
        let OutputContent::ToolResult {
            tool_use_id,
            content,
            ..
        } = content
        else {
            continue;
        };
        let call = ExecId::try_from(tool_use_id.as_str())?;
        if projection.answered.as_ref() != Some(&call) {
            results.push(CallResult {
                call,
                output: result_text(content)?,
            });
        }
    }
    Ok((!results.is_empty()).then_some(Event::ClaudeResults {
        agent: projection.agent,
        results,
    }))
}

/// The row a `compact_boundary` becomes.
pub(super) fn compacted_row(
    projection: &Projection,
    metadata: Option<&SystemCompactMetadata>,
) -> Event<'static> {
    Event::ClaudeCompacted {
        agent: projection.agent,
        context_used: metadata.and_then(|metadata| metadata.post_tokens),
    }
}

fn usage_bucket(
    usage: &TokenUsage,
    model: crate::log::AgentUsageModel,
) -> crate::log::AgentUsageBucket {
    crate::log::AgentUsageBucket {
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
        ..crate::log::AgentUsageBucket::default()
    }
}

fn result_text(content: &Value) -> anyhow::Result<String> {
    Ok(match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        Value::Null => String::new(),
        other => serde_json::to_string(other)?,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const UUID: &str = "00000000-0000-4000-8000-000000000001";

    fn agent() -> AgentId {
        AgentId::from_counter(1, &rho_agent_types::AgentIdDomain(0)).unwrap()
    }

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

    fn results_of(event: Option<Event<'static>>) -> Vec<CallResult> {
        match event {
            Some(Event::ClaudeResults { results, .. }) => results,
            None => Vec::new(),
            other => panic!("expected results, got {other:?}"),
        }
    }

    /// The request's row holds what it answered with. What no request
    /// answered, such as a call Claude Code refused, has only this row.
    #[test]
    fn only_a_result_no_request_answered_is_a_row() {
        let mut projection = Projection::new(agent());
        projection.answered = Some(ExecId::try_from("toolu_exec").unwrap());
        let message = user(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_exec", "content": "from the notebook"},
            {"type": "tool_result", "tool_use_id": "toolu_refused", "is_error": true, "content": "boom"},
        ]}));
        let results = results_of(results_row(&message, &projection).unwrap());
        assert_eq!(
            results,
            vec![CallResult {
                call: ExecId::try_from("toolu_refused").unwrap(),
                output: "boom".into(),
            }]
        );
        let answered = user(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_exec", "content": "from the notebook"},
        ]}));
        assert_eq!(results_row(&answered, &projection).unwrap(), None);
    }

    #[test]
    fn an_echo_is_no_row() {
        let projection = Projection::new(agent());
        for content in [
            json!([{"type": "text", "text": "hello"}]),
            json!([{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}]),
            json!("<command-name>/compact</command-name>"),
        ] {
            let message = user(json!({"role": "user", "content": content}));
            assert_eq!(results_row(&message, &projection).unwrap(), None);
        }
    }

    #[test]
    fn a_results_parts_join() {
        let message = user(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                {"type": "text", "text": "a"}, {"type": "text", "text": "b"},
            ]},
        ]}));
        let results = results_of(results_row(&message, &Projection::new(agent())).unwrap());
        assert_eq!(results[0].output, "ab");
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
        let mut told = Projection::new(agent());
        let first = reply_row(&text, crate::log::AgentUsageModel::OPUS, &mut told)
            .unwrap()
            .unwrap();
        let second = reply_row(&call, crate::log::AgentUsageModel::OPUS, &mut told)
            .unwrap()
            .unwrap();
        let Event::ClaudeReply {
            text,
            calls,
            usage,
            context_used,
            ..
        } = first
        else {
            panic!("expected a reply");
        };
        assert_eq!(text, "reading");
        assert!(calls.is_empty());
        let usage = usage.expect("first row carries usage");
        assert_eq!(usage.cache_read_tokens, 100);
        assert_eq!(usage.requests, 1);
        assert_eq!(context_used, Some(110));
        let Event::ClaudeReply { calls, usage, .. } = second else {
            panic!("expected a reply");
        };
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "Read");
        assert_eq!(calls[0].arguments, r#"{"path":"a.rs"}"#);
        assert!(usage.is_none(), "the same message's usage is told once");
    }

    #[test]
    fn a_thinking_only_block_is_no_row() {
        let message = assistant(
            json!({"role": "assistant", "id": "msg_2", "content": [{"type": "thinking", "thinking": "hmm"}]}),
        );
        assert_eq!(
            reply_row(
                &message,
                crate::log::AgentUsageModel::OPUS,
                &mut Projection::new(agent()),
            )
            .unwrap(),
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
        assert_eq!(
            compacted_row(&Projection::new(agent()), Some(&metadata)),
            Event::ClaudeCompacted {
                agent: agent(),
                context_used: Some(2000),
            }
        );
    }

    /// Only what the model said counts, not what it thought or called.
    #[test]
    fn said_so_far_is_only_the_text() {
        let mut items = BTreeMap::new();
        assert_eq!(said_so_far(&items), "");
        items.insert(2, ClaudeStreamItem::Thinking("hmm".into()));
        items.insert(5, ClaudeStreamItem::Text("first".into()));
        items.insert(6, ClaudeStreamItem::Text("  ".into()));
        items.insert(
            7,
            ClaudeStreamItem::ToolUse {
                id: "call-1".into(),
                name: "exec".into(),
                arguments: "{}".into(),
            },
        );
        items.insert(9, ClaudeStreamItem::Text("second".into()));
        assert_eq!(said_so_far(&items), "first\nsecond");
    }

    #[test]
    fn a_misnamed_call_is_not_named_well() {
        let call = |id: &str, name: &str| ClaudeStreamItem::ToolUse {
            id: id.into(),
            name: name.into(),
            arguments: "{}".into(),
        };
        assert!(call("toolu_1", "exec").named_well());
        assert!(!call("toolu_1", "no such name!").named_well());
        assert!(!call("", "exec").named_well());
    }

    #[test]
    fn live_response_replaces_items_but_keeps_provider_message_identity() {
        let mut blocks = BTreeMap::new();
        blocks.insert(0, ClaudeStreamItem::Text("hidden prose".into()));
        assert!(live_response(Some("response-a"), &blocks).is_none());
        blocks.insert(2, ClaudeStreamItem::Thinking("first".into()));
        let first = live_response(Some("response-a"), &blocks).unwrap();
        assert_eq!(first.id, "response-a");
        assert_eq!(
            first.items,
            vec![Item::Reasoning {
                text: "first".into()
            }]
        );
        blocks.insert(2, ClaudeStreamItem::Thinking("first and more".into()));
        blocks.insert(
            3,
            ClaudeStreamItem::ToolUse {
                id: "call-1".into(),
                name: "exec".into(),
                arguments: "{\"code\":\"x\"}".into(),
            },
        );
        let next = live_response(Some("response-a"), &blocks).unwrap();
        assert_eq!(next.id, first.id);
        assert_eq!(next.items.len(), 2);
        assert_eq!(
            next.items[0],
            Item::Reasoning {
                text: "first and more".into()
            }
        );
        assert!(matches!(&next.items[1], Item::ToolCall { id, .. } if id == "call-1"));
        blocks.remove(&2);
        assert_eq!(
            live_response(Some("response-a"), &blocks)
                .unwrap()
                .items
                .len(),
            1
        );
        blocks.clear();
        assert!(live_response(Some("response-a"), &blocks).is_none());
        blocks.insert(0, ClaudeStreamItem::Thinking("new message".into()));
        assert_eq!(
            live_response(Some("response-b"), &blocks).unwrap().id,
            "response-b"
        );
    }

    #[test]
    fn live_projection_hides_prose_and_keeps_partial_tool_json_and_reasoning() {
        let prose = ClaudeStreamItem::Text("not for the live tail".into());
        assert_eq!(prose.to_live_item(), None);

        let mut tool = ClaudeStreamItem::from_content_block(
            rho_claude::protocol::StreamContentBlock::ToolUse {
                id: "tool-7".into(),
                name: "mcp__py__exec".into(),
                input: serde_json::Value::Null,
            },
        )
        .unwrap()
        .unwrap();
        tool.apply_delta(rho_claude::protocol::ContentBlockDelta::InputJsonDelta {
            partial_json: "{\"code\":\"x=1\"}".into(),
        })
        .unwrap();
        assert_eq!(
            tool.to_live_item(),
            Some(Item::ToolCall {
                id: "tool-7".into(),
                name: "mcp__py__exec".into(),
                arguments: "{\"code\":\"x=1\"}".into(),
                format: ArgumentsFormat::Json,
            })
        );
        assert_eq!(
            ClaudeStreamItem::Thinking("why\nnext".into()).to_live_item(),
            Some(Item::Reasoning {
                text: "why\nnext".into()
            }),
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
