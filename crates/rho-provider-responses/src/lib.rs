//! OpenAI Responses API provider building blocks.
//!
//! The request-body shape and tool-name encoding are adapted from Tau's
//! Responses backend. Tau's protocol messages, event bus, VCR, WebSocket pool,
//! and HTTP loop are intentionally not copied into this crate; `rho-agent` or a
//! fork should own those runtime policies.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{Result, bail};
use futures::StreamExt;
use futures::future::{BoxFuture, FutureExt};
use futures::stream::{self, BoxStream};
use rho::{
    ItemKind, Message, MessagePhase, ProviderItem, ProviderItemKind, ProviderRequest,
    ProviderResponse, ReasoningText, ReasoningTextKind, Role, TokenUsage, ToolCall, ToolCallId,
    ToolSpec, ToolType,
};
use serde::Serialize;
use serde_json::Value;
#[cfg(test)]
use serde_json::json;

mod build_request;
pub mod oauth;
mod session;
mod ws;

pub use oauth::{OAuthFile, ResponsesAuth, ResponsesOAuthCredentials};
pub use session::ProviderSession;

pub const DEFAULT_CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";
pub const DEFAULT_MODEL: &str = "gpt-5.5";
pub(crate) const DEFAULT_CONTEXT_WINDOW: u64 = 258_400;
pub(crate) const DEFAULT_WEBSOCKET_EVENT_TIMEOUT_SECS: u64 = 120;
pub(crate) const DEFAULT_WEBSOCKET_PING_INTERVAL_SECS: u64 = 25;
pub(crate) const DEFAULT_WEBSOCKET_POOL_MAX_CONNECTIONS: usize = 10;
pub(crate) const DEFAULT_WEBSOCKET_POOL_MAX_CONNECTION_AGE_SECS: u64 = 55 * 60;
pub(crate) const DEFAULT_WEBSOCKET_POOL_CHECKOUT_WAIT_MS: u64 = 50;
pub(crate) const OPENAI_BETA_WS: &str = "responses_websockets=2026-02-06";

pub type ResponsesStream = BoxStream<'static, Result<ResponsesUpdate>>;

#[derive(Clone, Debug, PartialEq)]
pub enum ResponsesUpdate {
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
    Finished(ProviderResponse),
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ResponsesRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub input: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextRequest>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context_management: Vec<ContextManagementRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct TextRequest {
    pub verbosity: &'static str,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ResponsesCompaction {
    pub compact_threshold: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ContextManagementRequest {
    #[serde(rename = "type")]
    pub ty: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact_threshold: Option<u64>,
}

fn responses_url(base_url: &str) -> String {
    format!("{}/codex/responses", base_url.trim_end_matches('/'))
}

impl ProviderSession {
    pub fn stream(&self, request: ProviderRequest) -> ResponsesStream {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let session = self.clone();
        let websocket_pool = Arc::clone(&self.websocket_pool);
        tokio::spawn(async move {
            if let Err(error) =
                stream_provider_request(session, websocket_pool, request, &sender).await
            {
                let _ = sender.send(Err(error));
            }
        });

        stream::unfold(receiver, |mut receiver| async {
            receiver.recv().await.map(|item| (item, receiver))
        })
        .boxed()
    }

    pub fn prewarm_websocket(
        &self,
        prompt_cache_key: impl Into<String>,
    ) -> BoxFuture<'static, Result<bool>> {
        let session = self.clone();
        let prompt_cache_key = prompt_cache_key.into();
        async move { ws::prewarm_websocket(session, prompt_cache_key).await }.boxed()
    }
}

async fn stream_provider_request(
    session: ProviderSession,
    websocket_pool: Arc<tokio::sync::Mutex<ws::WebSocketPool>>,
    request: ProviderRequest,
    updates: &tokio::sync::mpsc::UnboundedSender<Result<ResponsesUpdate>>,
) -> Result<()> {
    let tool_names = tool_name_map(&request.tools);
    let responses_request = ResponsesRequest::from_provider_request(&session, request.clone());
    match ws::send_websocket(
        &session,
        &websocket_pool,
        responses_request,
        &tool_names,
        updates,
    )
    .await
    {
        Ok(()) => Ok(()),
        Err(error) => {
            if let Some(replay_request) =
                stale_previous_response_replay_request(&session, &request, &error)
            {
                ws::send_websocket(
                    &session,
                    &websocket_pool,
                    replay_request,
                    &tool_names,
                    updates,
                )
                .await
            } else {
                Err(error)
            }
        }
    }
}

pub fn parse_response_events(
    events: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<ProviderResponse> {
    let mut state = ResponseState::default();
    let mut completed = false;
    for event in events {
        if apply_response_event_str(&mut state, event.as_ref())?.0 {
            completed = true;
            break;
        }
    }
    if !completed {
        bail!("response stream ended before response.completed");
    }
    Ok(state.finish())
}

#[cfg(test)]
fn collect_response_events_with_updates(
    events: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<(ProviderResponse, Vec<ResponsesUpdate>)> {
    let mut state = ResponseState::default();
    let mut completed = false;
    let mut updates = Vec::new();
    for event in events {
        let (done, event_updates) = apply_response_event_str(&mut state, event.as_ref())?;
        updates.extend(event_updates);
        if done {
            completed = true;
            break;
        }
    }
    if !completed {
        bail!("response stream ended before response.completed");
    }
    let response = state.finish();
    updates.push(ResponsesUpdate::Finished(response.clone()));
    Ok((response, updates))
}

#[derive(Default)]
pub(crate) struct ResponseState {
    message_text_by_output_index: BTreeMap<usize, String>,
    tool_calls_by_output_index: BTreeMap<usize, ToolCallAccumulator>,
    items_by_output_index: BTreeMap<usize, ItemKind>,
    reasoning_summary_by_output_index: BTreeMap<usize, String>,
    tool_names_by_wire: BTreeMap<String, String>,
    usage: Option<TokenUsage>,
    provider_response_id: Option<String>,
}

#[derive(Clone)]
struct ToolCallAccumulator {
    id: String,
    name: String,
    tool_type: ToolType,
    arguments_json: String,
}

impl Default for ToolCallAccumulator {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            tool_type: ToolType::Function,
            arguments_json: String::new(),
        }
    }
}

impl ResponseState {
    pub(crate) fn with_tool_names(tool_names_by_wire: BTreeMap<String, String>) -> Self {
        Self {
            tool_names_by_wire,
            ..Default::default()
        }
    }

    fn local_tool_name(&self, wire_name: &str) -> String {
        self.tool_names_by_wire
            .get(wire_name)
            .cloned()
            .unwrap_or_else(|| wire_name.to_owned())
    }

    fn tool_call_at_mut(
        &mut self,
        output_index: usize,
        tool_type: ToolType,
    ) -> &mut ToolCallAccumulator {
        let call = self
            .tool_calls_by_output_index
            .entry(output_index)
            .or_default();
        call.tool_type = tool_type;
        call
    }

    pub(crate) fn finish(self) -> ProviderResponse {
        let ResponseState {
            message_text_by_output_index,
            tool_calls_by_output_index,
            items_by_output_index,
            reasoning_summary_by_output_index,
            tool_names_by_wire: _,
            usage,
            provider_response_id,
        } = self;

        let explicit_indexes = items_by_output_index
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();
        let mut ordered_items = Vec::new();

        for (index, reasoning_summary) in reasoning_summary_by_output_index {
            if !reasoning_summary.is_empty() {
                ordered_items.push((
                    index,
                    0,
                    ItemKind::ReasoningText(ReasoningText {
                        kind: ReasoningTextKind::Summary,
                        text: reasoning_summary,
                    }),
                ));
            }
        }

        for (index, item) in items_by_output_index {
            ordered_items.push((index, 1, item));
        }

        for (index, text) in message_text_by_output_index {
            if !text.is_empty() && !explicit_indexes.contains(&index) {
                ordered_items.push((
                    index,
                    2,
                    ItemKind::Message(Message::text(Role::Assistant, text)),
                ));
            }
        }

        for (index, call) in tool_calls_by_output_index {
            if !explicit_indexes.contains(&index)
                && let Some(call) = call.finish()
            {
                ordered_items.push((index, 3, ItemKind::ToolCall(call)));
            }
        }

        ordered_items.sort_by_key(|(index, priority, _)| (*index, *priority));

        ProviderResponse {
            items: ordered_items.into_iter().map(|(_, _, item)| item).collect(),
            usage,
            provider_response_id,
        }
    }
}

impl ToolCallAccumulator {
    fn finish(self) -> Option<ToolCall> {
        if self.name.is_empty() {
            return None;
        }
        let arguments = match self.tool_type {
            ToolType::Function => serde_json::from_str(&self.arguments_json)
                .unwrap_or(Value::String(self.arguments_json)),
            ToolType::Custom => Value::String(self.arguments_json),
        };
        Some(ToolCall {
            id: ToolCallId(self.id),
            name: self.name,
            tool_type: self.tool_type,
            arguments,
        })
    }
}

fn apply_response_event_str(
    state: &mut ResponseState,
    data: &str,
) -> Result<(bool, Vec<ResponsesUpdate>)> {
    let data = data.trim_end();
    if data == "[DONE]" {
        return Ok((true, Vec::new()));
    }
    let event: Value = match serde_json::from_str(data) {
        Ok(event) => event,
        Err(_) => return Ok((false, Vec::new())),
    };
    apply_response_event(state, &event)
}

pub(crate) fn apply_response_event(
    state: &mut ResponseState,
    event: &Value,
) -> Result<(bool, Vec<ResponsesUpdate>)> {
    let mut updates = Vec::new();
    match event["type"].as_str().unwrap_or_default() {
        "response.output_text.delta" => {
            if let Some(delta) = event["delta"].as_str() {
                let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                state
                    .message_text_by_output_index
                    .entry(output_index)
                    .or_default()
                    .push_str(delta);
                updates.push(ResponsesUpdate::TextDelta {
                    output_index,
                    text: delta.to_owned(),
                });
            }
        }
        "response.output_text.done" => {
            if let Some(text) = event["text"].as_str() {
                let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                state
                    .message_text_by_output_index
                    .insert(output_index, text.to_owned());
                updates.push(ResponsesUpdate::OutputItem {
                    output_index,
                    item: ItemKind::Message(Message::text(Role::Assistant, text)),
                });
            }
        }
        "response.reasoning_summary_text.delta" => {
            if let Some(delta) = event["delta"].as_str() {
                let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                state
                    .reasoning_summary_by_output_index
                    .entry(output_index)
                    .or_default()
                    .push_str(delta);
                updates.push(ResponsesUpdate::ReasoningTextDelta {
                    output_index,
                    kind: ReasoningTextKind::Summary,
                    text: delta.to_owned(),
                });
            }
        }
        "response.reasoning_summary_part.added" => {
            let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
            let summary = state
                .reasoning_summary_by_output_index
                .entry(output_index)
                .or_default();
            if !summary.is_empty() {
                summary.push_str("\n\n");
            }
        }
        "response.function_call_arguments.delta" => {
            let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
            if let Some(delta) = event["delta"].as_str() {
                state
                    .tool_call_at_mut(output_index, ToolType::Function)
                    .arguments_json
                    .push_str(delta);
            }
        }
        "response.function_call_arguments.done" => {
            let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
            if let Some(arguments) = event["arguments"].as_str() {
                state
                    .tool_call_at_mut(output_index, ToolType::Function)
                    .arguments_json = arguments.to_owned();
            }
        }
        "response.custom_tool_call_input.delta" => {
            let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
            if let Some(delta) = event["delta"].as_str() {
                state
                    .tool_call_at_mut(output_index, ToolType::Custom)
                    .arguments_json
                    .push_str(delta);
            }
        }
        "response.custom_tool_call_input.done" => {
            let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
            if let Some(input) = event["input"].as_str() {
                state
                    .tool_call_at_mut(output_index, ToolType::Custom)
                    .arguments_json = input.to_owned();
            }
        }
        "response.output_item.added" | "response.output_item.done" => {
            let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
            if let Some(item) = event.get("item") {
                let tool_type = match item["type"].as_str() {
                    Some("function_call") => Some(ToolType::Function),
                    Some("custom_tool_call") => Some(ToolType::Custom),
                    _ => None,
                };
                if let Some(tool_type) = tool_type {
                    let local_name = item["name"]
                        .as_str()
                        .map(|name| state.local_tool_name(name));
                    let call = state.tool_call_at_mut(output_index, tool_type);
                    if let Some(id) = item["call_id"].as_str() {
                        call.id = id.to_owned();
                    }
                    if let Some(name) = local_name {
                        call.name = name;
                    }
                    if call.arguments_json.is_empty() {
                        let final_input = match tool_type {
                            ToolType::Function => item["arguments"].as_str(),
                            ToolType::Custom => item["input"].as_str(),
                        };
                        if let Some(final_input) = final_input {
                            call.arguments_json = final_input.to_owned();
                        }
                    }
                    if let Some(call) = call.clone().finish() {
                        updates.push(ResponsesUpdate::ToolCall { output_index, call });
                    }
                }

                if event["type"].as_str() == Some("response.output_item.done")
                    && item["type"].as_str() == Some("message")
                    && let Some(text) = message_text_from_output_item(item)
                {
                    let mut message = Message::text(Role::Assistant, text.clone());
                    message.phase = message_phase_from_output_item(item);
                    state
                        .message_text_by_output_index
                        .insert(output_index, text);
                    state
                        .items_by_output_index
                        .insert(output_index, ItemKind::Message(message.clone()));
                    updates.push(ResponsesUpdate::OutputItem {
                        output_index,
                        item: ItemKind::Message(message),
                    });
                }
                if event["type"].as_str() == Some("response.output_item.done")
                    && item["type"].as_str() == Some("reasoning")
                    && item["encrypted_content"].is_string()
                {
                    let provider_item = ProviderItem {
                        kind: ProviderItemKind::Reasoning,
                        payload: item.clone(),
                    };
                    state
                        .items_by_output_index
                        .insert(output_index, ItemKind::ProviderItem(provider_item.clone()));
                    updates.push(ResponsesUpdate::OutputItem {
                        output_index,
                        item: ItemKind::ProviderItem(provider_item),
                    });
                }
                if item["type"].as_str() == Some("compaction") {
                    if event["type"].as_str() == Some("response.output_item.added") {
                        updates.push(ResponsesUpdate::CompactionStarted { output_index });
                    } else if event["type"].as_str() == Some("response.output_item.done") {
                        let provider_item = ProviderItem {
                            kind: ProviderItemKind::Compaction,
                            payload: item.clone(),
                        };
                        state
                            .items_by_output_index
                            .insert(output_index, ItemKind::ProviderItem(provider_item.clone()));
                        updates.push(ResponsesUpdate::OutputItem {
                            output_index,
                            item: ItemKind::ProviderItem(provider_item),
                        });
                    }
                }
                if event["type"].as_str() == Some("response.output_item.done")
                    && should_preserve_unknown_provider_item(item)
                {
                    let provider_item = ProviderItem {
                        kind: ProviderItemKind::Unknown,
                        payload: item.clone(),
                    };
                    state
                        .items_by_output_index
                        .insert(output_index, ItemKind::ProviderItem(provider_item.clone()));
                    updates.push(ResponsesUpdate::OutputItem {
                        output_index,
                        item: ItemKind::ProviderItem(provider_item),
                    });
                }
            }
        }
        "response.completed" | "response.done" => {
            state.usage = usage_from_event(event);
            if let Some(usage) = state.usage.clone() {
                updates.push(ResponsesUpdate::Usage(usage));
            }
            state.provider_response_id = event
                .get("response")
                .and_then(|response| response["id"].as_str())
                .or_else(|| event["id"].as_str())
                .map(str::to_owned);
            if let Some(response_id) = state.provider_response_id.clone() {
                updates.push(ResponsesUpdate::ResponseId(response_id));
            }
            return Ok((true, updates));
        }
        "response.incomplete" => {
            let reason = event
                .get("response")
                .and_then(|response| response["incomplete_details"]["reason"].as_str())
                .unwrap_or("unknown reason");
            bail!("response incomplete: {reason}");
        }
        "response.failed" => {
            let detail = event
                .get("response")
                .and_then(|response| {
                    response["error"]["message"]
                        .as_str()
                        .or_else(|| response["error"]["code"].as_str())
                })
                .unwrap_or("unknown error");
            bail!("response failed: {detail}");
        }
        "error" => {
            let detail = event["error"]["message"]
                .as_str()
                .or_else(|| event["message"].as_str())
                .unwrap_or("unknown error");
            let error_code = event["error"]["code"]
                .as_str()
                .or_else(|| event["code"].as_str())
                .or_else(|| event["error"]["type"].as_str());
            match error_code {
                Some(code) => bail!("stream error: {detail} (type={code})"),
                None => bail!("stream error: {detail}"),
            }
        }
        _ => {}
    }

    Ok((false, updates))
}

fn should_preserve_unknown_provider_item(item: &Value) -> bool {
    !matches!(
        item["type"].as_str(),
        Some("message" | "function_call" | "custom_tool_call" | "reasoning" | "compaction")
    )
}

fn message_text_from_output_item(item: &Value) -> Option<String> {
    let mut text = String::new();
    for part in item
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let is_text_part = matches!(
            part.get("type").and_then(Value::as_str),
            Some("output_text") | Some("text")
        );
        if is_text_part && let Some(part_text) = part.get("text").and_then(Value::as_str) {
            text.push_str(part_text);
        }
    }

    if text.is_empty() { None } else { Some(text) }
}

fn message_phase_from_output_item(item: &Value) -> Option<MessagePhase> {
    if item.get("type").and_then(Value::as_str)? != "message" {
        return None;
    }
    match item.get("phase")?.as_str()? {
        "commentary" => Some(MessagePhase::Commentary),
        "final_answer" => Some(MessagePhase::FinalAnswer),
        _ => None,
    }
}

fn usage_from_event(event: &Value) -> Option<TokenUsage> {
    let usage = event
        .get("response")
        .and_then(|response| response.get("usage"))
        .or_else(|| event.get("usage"))?;
    let input_tokens = usage["input_tokens"].as_u64().unwrap_or(0);
    let cached_input_tokens = usage["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);
    let output_tokens = usage["output_tokens"].as_u64().unwrap_or(0);
    if input_tokens == 0 && cached_input_tokens == 0 && output_tokens == 0 {
        None
    } else {
        Some(TokenUsage {
            input_tokens,
            cached_input_tokens,
            output_tokens,
        })
    }
}

fn tool_name_map(tools: &[ToolSpec]) -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();
    for tool in tools {
        let wire_name = encode_tool_name(&tool.name);
        match names.get(&wire_name) {
            Some(existing) if existing != &tool.name => {
                names.insert(wire_name.clone(), wire_name);
            }
            Some(_) => {}
            None => {
                names.insert(wire_name, tool.name.clone());
            }
        }
    }
    names
}

fn encode_tool_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn stale_previous_response_replay_request(
    session: &ProviderSession,
    request: &ProviderRequest,
    error: &anyhow::Error,
) -> Option<ResponsesRequest> {
    if !is_stale_previous_response_error(error) {
        return None;
    }

    let sliced = ResponsesRequest::from_provider_request(session, request.clone());
    sliced.previous_response_id.as_ref()?;

    Some(ResponsesRequest::from_provider_request_full_replay(
        session,
        request.clone(),
    ))
}

fn is_stale_previous_response_error(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("previous_response")
        || message.contains("previous response")
        || message.contains("response not found")
}

#[cfg(test)]
mod tests;
