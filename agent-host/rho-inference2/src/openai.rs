//! The OpenAI Responses API, as ChatGPT serves it to Codex: one WebSocket
//! per step, `response.create` in, events out until the response ends.
//!
//! Every model rho uses takes the Responses Lite shape: the tool and the
//! instructions are developer items at the head of the input, not top-level
//! fields.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine as _;
use futures::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::{Call, Carry, EXEC, Image, Inner, Item, Model, Request, Step, Usage};

pub const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const OPENAI_BETA: &str = "responses_websockets=2026-02-06";
/// A step that goes this long without an event is treated as wedged.
const EVENT_TIMEOUT: Duration = Duration::from_secs(300);

/// Credentials for one request. Resolved fresh each step, so a refresh
/// between steps is picked up.
#[derive(Clone)]
pub struct Credentials {
    pub bearer_token: String,
    pub account_id: Option<String>,
}

pub trait Auth: Send + Sync {
    fn credentials(&self) -> BoxFuture<'_, anyhow::Result<Credentials>>;
}

pub struct OpenAi {
    pub base_url: String,
    pub model: String,
    /// `low`, `medium`, `high` or `xhigh`.
    pub effort: String,
    pub auth: Arc<dyn Auth>,
}

impl Model for OpenAi {
    fn step<'a>(&'a self, request: &'a Request) -> BoxFuture<'a, anyhow::Result<Step>> {
        Box::pin(self.run(request))
    }
}

impl OpenAi {
    async fn run(&self, request: &Request) -> anyhow::Result<Step> {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        }
        let credentials = self.auth.credentials().await?;
        let url = format!("{}/codex/responses", self.base_url.trim_end_matches('/'));
        let url = match url.split_once("://") {
            Some(("https", rest)) => format!("wss://{rest}"),
            Some(("http", rest)) => format!("ws://{rest}"),
            _ => bail!("base URL must start with http:// or https://"),
        };
        let mut ws = url.into_client_request()?;
        let headers = ws.headers_mut();
        headers.insert("OpenAI-Beta", OPENAI_BETA.parse()?);
        headers.insert(
            "Authorization",
            format!("Bearer {}", credentials.bearer_token).parse()?,
        );
        let session = request.cache_key.to_string();
        headers.insert("session-id", session.parse()?);
        headers.insert("thread-id", session.parse()?);
        if let Some(account) = &credentials.account_id {
            headers.insert("chatgpt-account-id", account.parse()?);
        }
        let (mut socket, _) = tokio_tungstenite::connect_async(ws)
            .await
            .context("connecting to the Responses endpoint")?;
        socket
            .send(WsMessage::Text(self.body(request).to_string().into()))
            .await?;

        let mut items = Vec::new();
        loop {
            let message = tokio::time::timeout(EVENT_TIMEOUT, socket.next())
                .await
                .context("the provider went quiet")?
                .context("the provider closed the connection mid-response")??;
            let text = match message {
                WsMessage::Text(text) => text,
                WsMessage::Ping(payload) => {
                    socket.send(WsMessage::Pong(payload)).await?;
                    continue;
                }
                WsMessage::Close(frame) => bail!("the provider closed the connection: {frame:?}"),
                _ => continue,
            };
            let event: Value = serde_json::from_str(&text)?;
            match event["type"].as_str().unwrap_or_default() {
                "response.output_item.done" => items.push(event["item"].clone()),
                "response.completed" | "response.done" => {
                    let _ = socket.close(None).await;
                    return Ok(step(items, &event["response"]["usage"]));
                }
                "response.incomplete" => bail!(
                    "response incomplete: {}",
                    event["response"]["incomplete_details"]["reason"]
                ),
                "response.failed" => bail!("response failed: {}", event["response"]["error"]),
                "error" => bail!("provider error: {}", event["error"]),
                _ => {}
            }
        }
    }

    fn body(&self, request: &Request) -> Value {
        let mut input = vec![
            json!({
                "type": "additional_tools",
                "role": "developer",
                "tools": [{
                    "type": "custom",
                    "name": EXEC,
                    "description": "Run Python in your notebook. Every response is one call to this tool.",
                    "format": { "type": "text" },
                }],
            }),
            json!({
                "type": "message",
                "role": "developer",
                "content": [{ "type": "input_text", "text": &*request.instructions }],
            }),
        ];
        for item in &request.items {
            match item {
                Item::Step(Carry(Inner::OpenAi { items })) => input.extend(
                    items
                        .iter()
                        .filter_map(|item| serde_json::from_str::<Value>(item).ok()),
                ),
                // Another provider's step: all that can be said is the call.
                Item::Step(Carry(Inner::Scripted { call })) => {
                    if let Some(call) = call {
                        input.push(call_item(call));
                    }
                }
                Item::Result {
                    call_id,
                    text,
                    images,
                } => input.push(json!({
                    "type": "custom_tool_call_output",
                    "call_id": call_id,
                    "output": content(text, images, true),
                })),
                Item::User { text, images } => input.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": content(text, images, false),
                })),
            }
        }
        json!({
            "type": "response.create",
            "model": self.model,
            "instructions": "",
            "input": input,
            "store": false,
            "parallel_tool_calls": false,
            "text": { "verbosity": "low" },
            "reasoning": { "context": "all_turns", "effort": self.effort, "summary": "auto" },
            "service_tier": "default",
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": request.cache_key,
            "client_metadata": {
                "ws_request_header_x_openai_internal_codex_responses_lite": "true",
            },
        })
    }
}

fn call_item(call: &Call) -> Value {
    json!({
        "type": "custom_tool_call",
        "id": format!("ctc_{}", call.id),
        "call_id": call.id,
        "name": EXEC,
        "input": call.code,
    })
}

/// Text alone as a string; with images, content parts. Tool output may be
/// either; a message always takes parts.
fn content(text: &str, images: &[Image], output: bool) -> Value {
    if output && images.is_empty() {
        return json!(text);
    }
    let mut parts = vec![json!({ "type": "input_text", "text": text })];
    parts.extend(images.iter().map(|image| {
        json!({
            "type": "input_image",
            "image_url": format!(
                "data:{};base64,{}",
                image.media_type,
                base64::engine::general_purpose::STANDARD.encode(&image.data)
            ),
        })
    }));
    Value::Array(parts)
}

/// The step the output items make. The first `exec` call is the call; a
/// second is dropped, since nothing will ever answer it. Items are kept in
/// the shape they are replayed in.
fn step(items: Vec<Value>, usage: &Value) -> Step {
    let mut call = None;
    let mut prose = String::new();
    let mut carry = Vec::new();
    for item in items {
        let replay = match item["type"].as_str().unwrap_or_default() {
            "reasoning" if item["encrypted_content"].is_string() => json!({
                "type": "reasoning",
                "id": item["id"],
                "encrypted_content": item["encrypted_content"],
                "summary": item["summary"],
            }),
            "custom_tool_call" | "function_call" if call.is_none() => {
                let code = item["input"]
                    .as_str()
                    .or_else(|| item["arguments"].as_str())
                    .unwrap_or_default()
                    .to_owned();
                let this = Call {
                    id: item["call_id"].as_str().unwrap_or_default().to_owned(),
                    code,
                };
                let replay = json!({
                    "type": "custom_tool_call",
                    "id": item["id"],
                    "call_id": this.id,
                    "name": EXEC,
                    "input": this.code,
                });
                call = Some(this);
                replay
            }
            "message" => {
                let text = item["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("");
                prose.push_str(&text);
                json!({
                    "type": "message",
                    "role": "assistant",
                    "id": item["id"],
                    "content": [{ "type": "output_text", "text": text }],
                })
            }
            _ => continue,
        };
        carry.push(replay.to_string());
    }
    Step {
        call,
        prose,
        carry: Carry(Inner::OpenAi { items: carry }),
        usage: Usage {
            input_tokens: usage["input_tokens"].as_u64().unwrap_or(0),
            cached_tokens: usage["input_tokens_details"]["cached_tokens"]
                .as_u64()
                .unwrap_or(0),
            output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_keeps_one_call_and_its_reasoning_for_replay() {
        let step = step(
            vec![
                json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "e", "summary": []}),
                json!({"type": "message", "id": "msg_1", "content": [{"type": "output_text", "text": "hi"}]}),
                json!({"type": "custom_tool_call", "id": "ctc_1", "call_id": "c1", "name": "exec", "input": "print(1)"}),
                json!({"type": "custom_tool_call", "id": "ctc_2", "call_id": "c2", "name": "exec", "input": "print(2)"}),
            ],
            &json!({"input_tokens": 10, "output_tokens": 3, "input_tokens_details": {"cached_tokens": 4}}),
        );
        assert_eq!(
            step.call,
            Some(Call {
                id: "c1".into(),
                code: "print(1)".into()
            })
        );
        assert_eq!(step.prose, "hi");
        assert_eq!(step.usage.cached_tokens, 4);
        let Carry(Inner::OpenAi { items }) = &step.carry else {
            panic!()
        };
        assert_eq!(items.len(), 3, "the second call is dropped");
    }
}
