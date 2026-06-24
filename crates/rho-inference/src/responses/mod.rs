//! OpenAI Responses API provider building blocks.
//!
//! The request-body shape and tool-name encoding are adapted from Tau's
//! Responses backend. Tau's protocol messages, event bus, VCR, WebSocket pool,
//! and HTTP loop are intentionally not copied into this crate; `rho-agent` or a
//! fork should own those runtime policies.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use rho_core::{InferenceRequest, InferenceUpdate, ToolSpec};

pub(crate) mod oauth;
mod session;
#[cfg(test)]
mod tests;
mod wire;
mod ws;

pub use oauth::InferenceAuth;
pub use session::InferenceService;
pub(crate) use wire::ResponseState;
use wire::ResponsesRequest;

pub(crate) const DEFAULT_CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";
pub(crate) const DEFAULT_MODEL: &str = "gpt-5.5";
pub(crate) const DEFAULT_CONTEXT_WINDOW: u64 = 258_400;
pub(crate) const OPENAI_BETA_WS: &str = "responses_websockets=2026-02-06";

pub type InferenceStream = BoxStream<'static, Result<InferenceUpdate>>;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ResponsesCompaction {
    pub compact_threshold: Option<u64>,
}

fn responses_url(base_url: &str) -> String {
    format!("{}/codex/responses", base_url.trim_end_matches('/'))
}

impl InferenceService {
    pub fn stream(&self, request: InferenceRequest) -> InferenceStream {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let session = self.clone();
        let websocket_pool = Arc::clone(&self.websocket_pool);
        tokio::spawn(async move {
            if let Err(error) =
                stream_inference_request(session, websocket_pool, request, &sender).await
            {
                let _ = sender.send(Err(error));
            }
        });

        stream::unfold(receiver, |mut receiver| async {
            receiver.recv().await.map(|item| (item, receiver))
        })
        .boxed()
    }
}

async fn stream_inference_request(
    session: InferenceService,
    websocket_pool: Arc<tokio::sync::Mutex<ws::WebSocketPool>>,
    request: InferenceRequest,
    updates: &tokio::sync::mpsc::UnboundedSender<Result<InferenceUpdate>>,
) -> Result<()> {
    let tool_names = tool_name_map(&request.tools);
    let responses_request = ResponsesRequest::from_inference_request(&session, request.clone());
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
    session: &InferenceService,
    request: &InferenceRequest,
    error: &anyhow::Error,
) -> Option<ResponsesRequest> {
    if !is_stale_previous_response_error(error) {
        return None;
    }

    let sliced = ResponsesRequest::from_inference_request(session, request.clone());
    sliced.previous_response_id.as_ref()?;

    Some(ResponsesRequest::from_inference_request_full_replay(
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
