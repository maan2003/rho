use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{Sink, Stream};
use rho_core::{
    ContentPart, ContextBlock, ContextItemEvent, InferenceEvent, InferenceRequest,
    InferenceResponseItem, MessagePhase, PendingInferenceResponse, ProviderResponseId,
    StreamingContextItem, TokenUsage, ToolCall, ToolCallId, ToolFormat, ToolGrammarSyntax,
    ToolName, ToolOutput, ToolOutputStatus, ToolResult, ToolSpec, ToolType, UnixMs, text_content,
};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::oauth::{InferenceAuth, OAuthFile, ResponsesOAuthCredentials};
use super::session::{
    AutoCompaction, ReasoningContext, ResponsesEffort, ResponsesModel, ServiceTier, TextVerbosity,
    is_transient_turn_error, transient_backoff,
};
use super::wire::{
    OpenAiResponsesProviderData, ResponseState, ResponsesRequest, openai_provider_specific_data,
};
use super::ws::{WsResponseCreate, build_ws_request, next_ws_message};
use super::*;
use crate::config::{DeepConfig, DeepEffort, DeepModel};

fn first_assistant_message(
    items: &[InferenceResponseItem],
) -> (&[ContentPart], Option<MessagePhase>) {
    items
        .iter()
        .find_map(|item| match item {
            InferenceResponseItem::AssistantMessage { content, phase, .. } => {
                Some((content.as_slice(), *phase))
            }
            _ => None,
        })
        .expect("assistant message item")
}

fn assistant_message_text(items: &[InferenceResponseItem]) -> String {
    let (content, _) = first_assistant_message(items);
    text_content(content)
}

fn first_tool_call(items: &[InferenceResponseItem]) -> ToolCall {
    items
        .iter()
        .find_map(|item| match item {
            InferenceResponseItem::ToolCall {
                id,
                name,
                tool_type,
                arguments,
                ..
            } => Some(ToolCall {
                id: id.clone(),
                name: name.clone(),
                tool_type: *tool_type,
                arguments: arguments.clone(),
            }),
            _ => None,
        })
        .expect("tool call item")
}

fn content_parts(text: &str) -> Vec<ContentPart> {
    vec![ContentPart::Text {
        text: text.to_owned(),
    }]
}

fn assistant_message(text: &str) -> InferenceResponseItem {
    InferenceResponseItem::AssistantMessage {
        provider_specific: provider_specific(
            "message",
            json!({
                "type": "message",
                "id": "msg_test",
                "role": "assistant",
                "phase": "final_answer",
                "content": [{"type": "output_text", "text": text}],
            }),
        ),
        content: content_parts(text),
        phase: None,
    }
}

fn assistant_message_with_phase(text: &str, phase: MessagePhase) -> InferenceResponseItem {
    InferenceResponseItem::AssistantMessage {
        provider_specific: provider_specific(
            "message",
            json!({
                "type": "message",
                "id": "msg_test",
                "role": "assistant",
                "phase": match phase {
                    MessagePhase::Commentary => "commentary",
                    MessagePhase::FinalAnswer => "final_answer",
                },
                "content": [{"type": "output_text", "text": text}],
            }),
        ),
        content: content_parts(text),
        phase: Some(phase),
    }
}

fn provider_specific(_tag: &str, payload: Value) -> Box<dyn rho_core::ProviderSpecificData> {
    let item_id =
        rho_core::ProviderResponseItemId::try_from(payload["id"].as_str().unwrap_or("test_item"))
            .unwrap();
    Box::new(match payload["type"].as_str().unwrap_or_default() {
        "message" => OpenAiResponsesProviderData::Message { item_id },
        "function_call" => OpenAiResponsesProviderData::FunctionCall { item_id },
        "custom_tool_call" => OpenAiResponsesProviderData::CustomToolCall { item_id },
        "reasoning" => OpenAiResponsesProviderData::EncryptedReasoning {
            item_id,
            encrypted_content: payload["encrypted_content"]
                .as_str()
                .expect("reasoning test item missing encrypted_content")
                .to_owned(),
        },
        "compaction" => OpenAiResponsesProviderData::Compaction {
            item_id,
            encrypted_content: payload["encrypted_content"]
                .as_str()
                .expect("compaction test item missing encrypted_content")
                .to_owned(),
        },
        other => panic!("unexpected OpenAI test provider item type: {other}"),
    })
}

/// A `ContextBlock::UserMessage` carrying a single text part.
fn user_block(text: &str) -> Arc<ContextBlock> {
    Arc::new(ContextBlock::UserMessage {
        sender: rho_core::MessageSender::User,
        content: content_parts(text),
    })
}

fn tool_result_success(call_id: ToolCallId, content: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id,
        tool_type: ToolType::Function,
        body: ToolOutput {
            output: Arc::from(content.into()),
            status: ToolOutputStatus::Success,
        },
        started_at: UnixMs(1),
        finished_at: UnixMs(2),
        metadata: None,
    }
}

fn tool_call_id(id: &str) -> ToolCallId {
    ToolCallId::try_from(id).unwrap()
}

fn tool_name(name: &str) -> ToolName {
    ToolName::try_from(name).unwrap()
}

fn inference_request(input: Vec<Arc<ContextBlock>>, tools: Vec<ToolSpec>) -> InferenceRequest {
    InferenceRequest {
        instructions: Arc::from(""),
        input,
        agent_id_labels: std::collections::BTreeMap::new(),
        tools: tools.into(),
    }
}

/// Drives `ResponseState` over a wire-event stream, folding the emitted
/// [`InferenceUpdate`]s into a `PendingInferenceResponse` exactly as a consumer
/// would, then exposes the finalized items alongside the raw event stream.
#[derive(Debug)]
struct ParsedResponse {
    items: Vec<InferenceResponseItem>,
    usage: Option<TokenUsage>,
    provider_response_id: Option<String>,
    events: Vec<(usize, ContextItemEvent)>,
    saw_finished: bool,
}

fn parse_response_events(
    events: impl IntoIterator<Item = impl AsRef<str>>,
) -> anyhow::Result<ParsedResponse> {
    let mut state = ResponseState::new();
    let mut pending = PendingInferenceResponse::default();
    let mut usage = None;
    let mut provider_response_id = None;
    let mut item_events = Vec::new();
    let mut saw_finished = false;
    let mut completed = false;

    for event in events {
        let data = event.as_ref().trim_end();
        if data == "[DONE]" {
            completed = true;
            break;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        let (done, updates) = state.apply_event(&value)?;
        for update in updates {
            match update {
                InferenceEvent::ContextItem { index, event } => {
                    pending.apply(index, event.clone());
                    item_events.push((index, event));
                }
                InferenceEvent::Finished {
                    usage: turn_usage,
                    provider_response_id: id,
                } => {
                    usage = turn_usage;
                    provider_response_id = id.map(|id| id.as_str().to_owned());
                    saw_finished = true;
                }
                _ => {}
            }
        }
        if done {
            completed = true;
            break;
        }
    }

    if !completed {
        anyhow::bail!("response stream ended before response.completed");
    }

    Ok(ParsedResponse {
        items: pending.finish()?,
        usage,
        provider_response_id,
        events: item_events,
        saw_finished,
    })
}

fn test_oauth_file(
    access_token: &str,
    account_id: Option<&str>,
) -> (tempfile::TempDir, InferenceAuth) {
    let temp = tempfile::tempdir().unwrap();
    let file = test_oauth_file_in(temp.path(), "chatgpt").unwrap();
    file.save(&ResponsesOAuthCredentials {
        access_token: access_token.to_owned(),
        refresh_token: "refresh".to_owned(),
        expires_at_ms: u64::MAX,
        account_id: account_id.map(str::to_owned),
        client_secret: *b"secret00secret00secret00secret00",
    })
    .unwrap();
    let auth = InferenceAuth::oauth_file(file.path());
    (temp, auth)
}

fn test_inference_service(model: impl Into<String>) -> InferenceSession {
    let (_temp, auth) = test_oauth_file("token", None);
    test_inference_service_with(auth, model, PromptCacheKey::from_bytes(*b"testkey0"), None)
}

fn test_inference_service_with(
    auth: InferenceAuth,
    model: impl Into<String>,
    prompt_cache_key: PromptCacheKey,
    auto_compaction: Option<AutoCompaction>,
) -> InferenceSession {
    let mut session = InferenceSession::new_deep(
        auth,
        DeepConfig {
            effort: DeepEffort::Medium,
            fast_mode: false,
        },
        DeepModel::Gpt55,
        prompt_cache_key,
    );
    session.responses_config = super::session::ResponsesConfig {
        model: ResponsesModel::Test(model.into()),
        auto_compaction,
        reasoning_context: super::session::ReasoningContext::AllTurns,
        effort: ResponsesEffort::Medium,
        text_verbosity: TextVerbosity::Medium,
        service_tier: ServiceTier::Normal,
    };
    session
}

fn test_oauth_file_in(
    state_dir: impl AsRef<Path>,
    name: impl AsRef<str>,
) -> std::io::Result<OAuthFile> {
    OAuthFile::open_at(state_dir.as_ref().join("auth.d"), name)
}

#[derive(Default)]
struct PendingSocket {
    sent: Vec<WsMessage>,
}

impl Stream for PendingSocket {
    type Item = std::result::Result<WsMessage, tungstenite::Error>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}

impl Sink<WsMessage> for PendingSocket {
    type Error = tungstenite::Error;

    fn poll_ready(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: WsMessage) -> std::result::Result<(), Self::Error> {
        self.get_mut().sent.push(item);
        Ok(())
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

mod parser;
mod request;
mod session;
mod websocket;
