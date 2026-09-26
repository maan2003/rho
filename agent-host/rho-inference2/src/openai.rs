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
use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::{Call, CallId, Carry, EXEC, Image, Inner, Item, Request, Step, Stream, Usage};

/// Resolve named OAuth credentials over a private host channel for each step.
/// The callback returns only the bearer and account id, never the refresh
/// secret.
pub type AuthResolver = Arc<
    dyn Fn(String) -> BoxFuture<'static, anyhow::Result<rho_inference::ResolvedOAuth>>
        + Send
        + Sync,
>;

pub const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";
const OPENAI_BETA: &str = "responses_websockets=2026-02-06";
/// A step that goes this long without an event is treated as wedged.
const EVENT_TIMEOUT: Duration = Duration::from_secs(300);

pub struct OpenAi {
    pub base_url: String,
    pub model: String,
    pub effort: Effort,
    /// The OAuth credentials file in rho's auth directory, read each step so
    /// a refresh between steps is picked up.
    pub auth: String,
}

/// How hard the model reasons before it answers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Effort {
    Low,
    #[default]
    Medium,
    High,
    XHigh,
}

impl Effort {
    fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
        }
    }
}

impl std::str::FromStr for Effort {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "low" => Ok(Effort::Low),
            "medium" => Ok(Effort::Medium),
            "high" => Ok(Effort::High),
            "xhigh" => Ok(Effort::XHigh),
            _ => Err(format!(
                "unknown effort {s:?}: use low, medium, high or xhigh"
            )),
        }
    }
}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl OpenAi {
    pub(crate) async fn step(
        &self,
        request: &Request,
        stream: &mut (dyn FnMut(Stream<'_>) + Send),
        resolve_auth: Option<&AuthResolver>,
    ) -> anyhow::Result<Step> {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        }
        let credentials = self.credentials(resolve_auth).await?;
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
        // The item whose code is streamed: the first call, as only it runs.
        let mut streaming: Option<String> = None;
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
                "response.output_item.added" if streaming.is_none() && is_call(&event["item"]) => {
                    let item = &event["item"];
                    streaming = Some(item["id"].as_str().unwrap_or_default().to_owned());
                    stream(Stream::Call {
                        id: &CallId::new(item["call_id"].as_str().unwrap_or_default()),
                    });
                }
                "response.custom_tool_call_input.delta"
                    if streaming.as_deref() == event["item_id"].as_str() =>
                {
                    stream(Stream::Code(event["delta"].as_str().unwrap_or_default()));
                }
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

    async fn credentials(
        &self,
        resolve_auth: Option<&AuthResolver>,
    ) -> anyhow::Result<rho_inference::ResolvedOAuth> {
        if let Some(resolve_auth) = resolve_auth {
            return resolve_auth(self.auth.clone()).await;
        }
        let name = self.auth.clone();
        Ok(tokio::task::spawn_blocking(move || {
            rho_inference::InferenceAuth::named(&name)?.resolve_oauth()
        })
        .await??)
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
                    "call_id": call_id.as_str(),
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
            "reasoning": { "context": "all_turns", "effort": self.effort.as_str(), "summary": "auto" },
            "service_tier": "default",
            "include": ["reasoning.encrypted_content"],
            "prompt_cache_key": request.cache_key.to_string(),
            "client_metadata": {
                "ws_request_header_x_openai_internal_codex_responses_lite": "true",
            },
        })
    }
}

fn is_call(item: &Value) -> bool {
    item["type"] == "custom_tool_call" && item["name"] == EXEC
}

fn call_item(call: &Call) -> Value {
    json!({
        "type": "custom_tool_call",
        "id": format!("ctc_{}", call.id),
        "call_id": call.id.as_str(),
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
                    id: CallId::new(item["call_id"].as_str().unwrap_or_default()),
                    code,
                };
                let replay = json!({
                    "type": "custom_tool_call",
                    "id": item["id"],
                    "call_id": this.id.as_str(),
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

    #[tokio::test]
    async fn injected_resolver_is_used_each_step_without_file_fallback() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let resolver: AuthResolver = Arc::new({
            let calls = Arc::clone(&calls);
            move |name| {
                assert_eq!(name, "not-a-file/invalid");
                let n = calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    if n == 2 {
                        anyhow::bail!("host auth channel closed");
                    }
                    Ok(rho_inference::ResolvedOAuth {
                        bearer_token: format!("bearer-{n}"),
                        account_id: Some(format!("account-{n}")),
                    })
                })
            }
        });
        let model = OpenAi {
            base_url: String::new(),
            model: String::new(),
            effort: Effort::Low,
            auth: "not-a-file/invalid".into(),
        };
        for n in 0..2 {
            let credentials = model.credentials(Some(&resolver)).await.unwrap();
            assert_eq!(credentials.bearer_token, format!("bearer-{n}"));
            assert_eq!(
                credentials.account_id.as_deref(),
                Some(format!("account-{n}").as_str())
            );
        }
        let error = model.credentials(Some(&resolver)).await.unwrap_err();
        assert_eq!(error.to_string(), "host auth channel closed");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn model_routes_host_auth_into_request_headers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_hdr_async(
                stream,
                |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                    assert_eq!(request.headers()["authorization"], "Bearer ephemeral");
                    assert_eq!(request.headers()["chatgpt-account-id"], "account-9");
                    Ok(response)
                },
            )
            .await
            .unwrap();
            let _request = socket.next().await.unwrap().unwrap();
            socket
                .send(WsMessage::Text(
                    r#"{"type":"response.completed","response":{"usage":{}}}"#.into(),
                ))
                .await
                .unwrap();
        });
        let model = crate::Model::OpenAiWithAuth {
            model: OpenAi {
                base_url: format!("http://{addr}"),
                model: "test".into(),
                effort: Effort::Low,
                auth: "not-a-file/invalid".into(),
            },
            resolve_auth: Arc::new(|name| {
                assert_eq!(name, "not-a-file/invalid");
                Box::pin(async {
                    Ok(rho_inference::ResolvedOAuth {
                        bearer_token: "ephemeral".into(),
                        account_id: Some("account-9".into()),
                    })
                })
            }),
        };
        let request = Request {
            instructions: "test".into(),
            items: Vec::new(),
            cache_key: crate::CacheKey::new(),
        };
        tokio::time::timeout(Duration::from_secs(2), model.step(&request, &mut |_| {}))
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();
    }

    /// Against the real endpoint, with the `default` credentials:
    /// `cargo test -p rho-inference2 -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn the_call_streams_in_pieces() {
        let model = OpenAi {
            base_url: CHATGPT_BASE_URL.into(),
            model: "gpt-6-sol".into(),
            effort: Effort::Low,
            auth: "default".into(),
        };
        let request = Request {
            instructions: "Answer by calling exec.".into(),
            items: vec![Item::User {
                text: "Print the first five squares, one statement per line.".into(),
                images: Vec::new(),
            }],
            cache_key: crate::CacheKey::new(),
        };
        let mut pieces = Vec::new();
        let step = model
            .step(
                &request,
                &mut |piece| {
                    if let Stream::Code(code) = piece {
                        pieces.push(code.to_owned());
                    }
                },
                None,
            )
            .await
            .unwrap();
        let code = step.call.unwrap().code;
        assert!(pieces.len() > 1, "{pieces:?}");
        assert_eq!(pieces.concat(), code);
    }

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
                id: CallId::new("c1"),
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
