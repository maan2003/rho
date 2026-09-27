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
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::{Call, CallId, Carry, EXEC, Image, Inner, Item, Request, Step, Stream, Usage};
use crate::Inference;
use crate::config::InferenceModel;
use crate::responses::{DialRoute, QuotaUpdate, ws};

const EVENT_TIMEOUT: Duration = Duration::from_secs(300);

pub struct OpenAi {
    pub(crate) inference: Inference,
    pub(crate) model: InferenceModel,
    pub(crate) effort: Effort,
    pub(crate) fast: bool,
}

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
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }
}

impl std::str::FromStr for Effort {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::XHigh),
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
    ) -> anyhow::Result<Step> {
        self.exchange(request.cache_key, self.body(request), stream)
            .await
    }

    pub(crate) async fn text(
        &self,
        instructions: Arc<str>,
        input: String,
    ) -> anyhow::Result<String> {
        let request = Request {
            instructions,
            items: vec![Item::User {
                text: input,
                images: Vec::new(),
            }],
            cache_key: super::CacheKey::new(),
        };
        let mut body = self.body(&request);
        body["input"].as_array_mut().unwrap().remove(0); // Text requests have no tools.
        let response = self.exchange(request.cache_key, body, &mut |_| {}).await?;
        if response.call.is_some() {
            bail!("text completion returned a tool call");
        }
        Ok(response.prose)
    }

    async fn exchange(
        &self,
        cache_key: super::CacheKey,
        body: Value,
        stream: &mut (dyn FnMut(Stream<'_>) + Send),
    ) -> anyhow::Result<Step> {
        crate::ensure_crypto_provider();
        let (mut selected, resolved) = self.inference.select_resolved().await?;
        selected.account_id = resolved.account_id.clone();
        let mut route = self
            .inference
            .route_for_model(self.model, self.fast, Some(&selected));
        let request = ws::request(
            self.inference.responses_base_url(),
            Some(&cache_key.to_string()),
            &resolved,
        )?;
        let mut connected = ws::connect(request, route).await;
        if let Err(error) = &connected
            && route != DialRoute::Dns
            && !matches!(websocket_status(error), Some(401 | 403 | 429))
        {
            self.inference
                .report_connect_failure(route, Some(&selected))
                .await;
            route = DialRoute::Dns;
            let request = ws::request(
                self.inference.responses_base_url(),
                Some(&cache_key.to_string()),
                &resolved,
            )?;
            connected = ws::connect(request, route).await;
        }
        let (mut socket, _) = match connected {
            Ok(connection) => connection,
            Err(error) => {
                if websocket_status(&error) == Some(429) {
                    self.inference.mark_rate_limited(&selected).await;
                    return Err(super::Retryable(error.to_string()).into());
                }
                return Err(error).context("connecting to the Responses endpoint");
            }
        };
        socket
            .send(WsMessage::Text(body.to_string().into()))
            .await?;
        let mut items = Vec::new();
        let mut streaming: Option<(String, Value)> = None;
        loop {
            let message = tokio::time::timeout(EVENT_TIMEOUT, socket.next())
                .await
                .context("the provider went quiet")?
                .ok_or_else(|| {
                    super::Retryable("the provider closed the connection mid-response".into())
                })??;
            let text = match message {
                WsMessage::Text(text) => text,
                WsMessage::Ping(payload) => {
                    socket.send(WsMessage::Pong(payload)).await?;
                    continue;
                }
                WsMessage::Close(frame) => {
                    return Err(super::Retryable(format!(
                        "the provider closed the connection: {frame:?}"
                    ))
                    .into());
                }
                _ => continue,
            };
            let event: Value = serde_json::from_str(&text)?;
            if let Some(quota) = QuotaUpdate::from_event(&event) {
                self.inference.observe_quota(&selected, quota).await;
            }
            match event["type"].as_str().unwrap_or_default() {
                "response.output_item.added" if streaming.is_none() && is_call(&event["item"]) => {
                    let item = &event["item"];
                    streaming = Some((
                        item["id"].as_str().unwrap_or_default().to_owned(),
                        event["output_index"].clone(),
                    ));
                    stream(Stream::Call {
                        id: &CallId::new(item["call_id"].as_str().unwrap_or_default()),
                    });
                }
                "response.custom_tool_call_input.delta"
                    if streaming.as_ref().is_some_and(|(id, index)| {
                        match event["item_id"].as_str() {
                            Some(item) => item == id,
                            None => !index.is_null() && event["output_index"] == *index,
                        }
                    }) =>
                {
                    stream(Stream::Code(event["delta"].as_str().unwrap_or_default()))
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
                "response.failed" | "error" => {
                    let error = if event["type"] == "error" {
                        &event["error"]
                    } else {
                        &event["response"]["error"]
                    };
                    let retryable = if is_rate_limit(error) {
                        let replacement = self.inference.mark_rate_limited(&selected).await;
                        replacement
                            || ["code", "type"]
                                .iter()
                                .any(|key| error[key] == "rate_limit_exceeded")
                    } else {
                        ["code", "type"].iter().any(|key| {
                            matches!(
                                error[key].as_str(),
                                Some(
                                    "server_error"
                                        | "internal_server_error"
                                        | "server_is_overloaded"
                                        | "overloaded"
                                        | "service_unavailable"
                                        | "slow_down"
                                )
                            )
                        })
                    };
                    if retryable {
                        return Err(super::Retryable(format!("provider error: {error}")).into());
                    }
                    bail!("provider error: {error}");
                }
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
        // Keep the latest provider compaction item itself and everything after it.
        let latest_compaction = request
            .items
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, item)| match item {
                Item::Step(Carry(Inner::OpenAi { items })) => items
                    .iter()
                    .rposition(|item| {
                        serde_json::from_str::<Value>(item)
                            .is_ok_and(|item| item["type"] == "compaction")
                    })
                    .map(|offset| (index, offset)),
                Item::Step(Carry(Inner::ScriptedCompaction)) => Some((index, 0)),
                _ => None,
            });
        let mut compaction_requested = false;
        for (index, item) in request.items.iter().enumerate() {
            if latest_compaction.is_some_and(|(start, _)| index < start) {
                continue;
            }
            match item {
                Item::Step(Carry(Inner::OpenAi { items })) => input.extend(
                    items
                        .iter()
                        .skip(match latest_compaction {
                            Some((start, offset)) if start == index => offset,
                            _ => 0,
                        })
                        .filter_map(|item| serde_json::from_str::<Value>(item).ok()),
                ),
                Item::Step(Carry(Inner::ScriptedCompaction)) => {}
                Item::CompactionTrigger => compaction_requested = true,
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
        if compaction_requested {
            input.push(json!({ "type": "compaction_trigger" }));
        }
        json!({
            "type": "response.create",
            "model": self.model.as_str(),
            "instructions": "",
            "input": input,
            "store": false,
            "parallel_tool_calls": false,
            "text": { "verbosity": "low" },
            "reasoning": { "context": "all_turns", "effort": self.effort.as_str(), "summary": "auto" },
            "service_tier": if self.fast { "priority" } else { "default" },
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
            "compaction" if item["encrypted_content"].is_string() => item,
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

fn websocket_status(error: &tokio_tungstenite::tungstenite::Error) -> Option<u16> {
    match error {
        tokio_tungstenite::tungstenite::Error::Http(response) => Some(response.status().as_u16()),
        _ => None,
    }
}

fn is_rate_limit(error: &Value) -> bool {
    ["code", "type"]
        .iter()
        .any(|key| error[key] == "rate_limit_exceeded" || error[key] == "usage_limit_reached")
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures::future::BoxFuture;
    use tokio::sync::watch;

    use super::*;
    use crate::accounts::SelectedAuth;
    use crate::inference::{InferenceConfig, InferenceHost};
    use crate::responses::{InferenceAuth, RouteSelection};

    #[derive(Debug)]
    struct Host {
        selected: SelectedAuth,
        calls: Mutex<Vec<&'static str>>,
        route: watch::Sender<RouteSelection>,
    }
    impl Host {
        fn new() -> Arc<Self> {
            let (route, _) = watch::channel(RouteSelection::default());
            Arc::new(Self {
                selected: SelectedAuth {
                    auth: InferenceAuth::oauth_file("/nonexistent/host-only"),
                    namespace: Some("test".into()),
                    account_id: None,
                },
                calls: Mutex::new(vec![]),
                route,
            })
        }
    }
    impl InferenceHost for Host {
        fn select(&self) -> BoxFuture<'_, anyhow::Result<SelectedAuth>> {
            Box::pin(async { panic!("transport must use host's atomic selection and resolution") })
        }
        fn select_resolved(
            &self,
        ) -> BoxFuture<'_, anyhow::Result<(SelectedAuth, crate::ResolvedAuth)>> {
            Box::pin(async {
                let mut calls = self.calls.lock().unwrap();
                let nth = calls
                    .iter()
                    .filter(|call| **call == "select-resolved")
                    .count()
                    + 1;
                calls.push("select-resolved");
                Ok((
                    self.selected.clone(),
                    crate::ResolvedAuth {
                        bearer_token: format!("ephemeral-{nth}"),
                        account_id: Some("account-9".into()),
                        client_secret: [0; 32],
                    },
                ))
            })
        }
        fn resolve_auth(
            &self,
            _: InferenceAuth,
        ) -> BoxFuture<'_, anyhow::Result<crate::ResolvedAuth>> {
            Box::pin(async { panic!("transport must not resolve auth from worker filesystem") })
        }
        fn mark_rate_limited(&self, selected: SelectedAuth) -> BoxFuture<'_, bool> {
            Box::pin(async move {
                assert_eq!(selected.account_id.as_deref(), Some("account-9"));
                self.calls.lock().unwrap().push("limited");
                true
            })
        }
        fn observe_quota(&self, selected: SelectedAuth, quota: QuotaUpdate) -> BoxFuture<'_, ()> {
            Box::pin(async move {
                assert_eq!(selected.account_id.as_deref(), Some("account-9"));
                assert_eq!(quota.weekly_used_percent, 17);
                assert_eq!(quota.routing_used_percent, 83);
                self.calls.lock().unwrap().push("quota");
            })
        }
        fn route_updates(&self) -> watch::Receiver<RouteSelection> {
            self.route.subscribe()
        }
        fn report_connect_failure(
            &self,
            _: DialRoute,
            _: Option<SelectedAuth>,
        ) -> BoxFuture<'_, ()> {
            Box::pin(async { panic!("unexpected route failure") })
        }
    }

    fn model(host: Arc<Host>, addr: std::net::SocketAddr) -> crate::step::Model {
        Inference::from_host(
            host,
            InferenceConfig::with_responses_base_url(format!("http://{addr}")).unwrap(),
        )
        .model(Default::default(), InferenceModel::Gpt6Sol)
    }

    fn request() -> Request {
        Request {
            instructions: "answer with exec".into(),
            items: vec![Item::User {
                text: "run".into(),
                images: vec![],
            }],
            cache_key: super::super::CacheKey::from_u128(19),
        }
    }

    fn local_model() -> OpenAi {
        OpenAi {
            inference: Inference::from_host(
                Host::new(),
                InferenceConfig::with_responses_base_url("http://127.0.0.1:1").unwrap(),
            ),
            model: InferenceModel::Gpt6Sol,
            effort: Effort::Low,
            fast: false,
        }
    }

    #[test]
    fn request_contains_one_exec_tool_and_replays_results() {
        let body = local_model().body(&Request {
            instructions: "instructions".into(),
            cache_key: super::super::CacheKey::from_u128(17),
            items: vec![
                Item::Result {
                    call_id: CallId::new("call-1"),
                    text: "result".into(),
                    images: vec![],
                },
                Item::User {
                    text: "next".into(),
                    images: vec![],
                },
            ],
        });
        assert_eq!(body["input"][0]["tools"][0]["name"], "exec");
        assert_eq!(body["input"][1]["content"][0]["text"], "instructions");
        assert_eq!(
            body["input"][2],
            json!({"type":"custom_tool_call_output","call_id":"call-1","output":"result"})
        );
        assert_eq!(body["input"][3]["content"][0]["text"], "next");
        assert_eq!(
            body["prompt_cache_key"],
            "00000000-0000-0000-0000-000000000011"
        );
        assert_eq!(body["service_tier"], "default");
    }

    #[test]
    fn compaction_keeps_latest_boundary_and_following_items() {
        let latest = json!({"type":"compaction", "id":"new", "encrypted_content":"new-key", "opaque":{"keep":1}});
        let request = Request {
            instructions: "instructions".into(),
            cache_key: super::super::CacheKey::from_u128(17),
            items: vec![
                Item::User {
                    text: "discard".into(),
                    images: vec![],
                },
                Item::Step(Carry(Inner::OpenAi {
                    items: vec![
                        json!({"type":"compaction","id":"old","encrypted_content":"old-key"})
                            .to_string(),
                    ],
                })),
                Item::Step(Carry(Inner::OpenAi {
                    items: vec![
                        json!({"type":"reasoning","id":"discard"}).to_string(),
                        latest.to_string(),
                        json!({"type":"message","id":"keep"}).to_string(),
                    ],
                })),
                Item::User {
                    text: "keep".into(),
                    images: vec![],
                },
                Item::CompactionTrigger,
            ],
        };
        let body = local_model().body(&request);
        assert_eq!(
            body["input"].as_array().unwrap()[2..],
            [
                latest,
                json!({"type":"message","id":"keep"}),
                json!({"type":"message","role":"user","content":[{"type":"input_text","text":"keep"}]}),
                json!({"type":"compaction_trigger"}),
            ]
        );
    }

    #[test]
    fn only_first_exec_call_is_replayed_and_returned() {
        let answer = step(
            vec![
                json!({"type":"reasoning","id":"rs","encrypted_content":"secret","summary":[]}),
                json!({"type":"message","id":"msg","content":[{"text":"prose"}]}),
                json!({"type":"custom_tool_call","id":"one","call_id":"first","input":"print(1)"}),
                json!({"type":"custom_tool_call","id":"two","call_id":"second","input":"print(2)"}),
            ],
            &json!({"input_tokens":10,"output_tokens":3,"input_tokens_details":{"cached_tokens":4}}),
        );
        assert_eq!(answer.call.unwrap().id.as_str(), "first");
        assert_eq!(answer.prose, "prose");
        assert_eq!(answer.usage.cached_tokens, 4);
        assert_eq!(answer.carry.call_ids(), [CallId::new("first")]);
        let Carry(Inner::OpenAi { items }) = answer.carry else {
            panic!()
        };
        assert_eq!(items.len(), 3);
    }

    #[tokio::test]
    async fn host_auth_quota_and_streaming_share_one_step_without_retry() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for nth in 1..=2 {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_hdr_async(
                    stream,
                    |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                     response| {
                        assert_eq!(
                            request.headers()["authorization"],
                            format!("Bearer ephemeral-{nth}")
                        );
                        assert_eq!(request.headers()["chatgpt-account-id"], "account-9");
                        assert_eq!(
                            request.headers()["session-id"],
                            "00000000-0000-0000-0000-000000000013"
                        );
                        Ok(response)
                    },
                )
                .await
                .unwrap();
                let body: Value = serde_json::from_str(
                    &socket.next().await.unwrap().unwrap().into_text().unwrap(),
                )
                .unwrap();
                assert_eq!(body["input"][0]["type"], "additional_tools");
                for event in [
                    json!({"type":"codex.rate_limits","rate_limits":{"primary":{"window_minutes":10080,"used_percent":17,"reset_at":123},"secondary":{"window_minutes":300,"used_percent":83,"reset_at":456}}}),
                    json!({"type":"response.output_item.added","output_index":0,"item":{"type":"custom_tool_call","id":"item-1","name":"exec","call_id":"call-1"}}),
                    json!({"type":"response.custom_tool_call_input.delta","item_id":"item-1","delta":"print(1)"}),
                    json!({"type":"response.output_item.done","item":{"type":"custom_tool_call","id":"item-1","name":"exec","call_id":"call-1","input":"print(1)"}}),
                    json!({"type":"response.completed","response":{"usage":{"input_tokens":20,"input_tokens_details":{"cached_tokens":5},"output_tokens":7}}}),
                ] {
                    socket
                        .send(WsMessage::Text(event.to_string().into()))
                        .await
                        .unwrap();
                }
            }
        });
        let host = Host::new();
        let model = model(host.clone(), addr);
        for _ in 0..2 {
            let mut stream = Vec::new();
            let result = model
                .step(&request(), &mut |event| match event {
                    Stream::Call { id } => stream.push(format!("id:{id}")),
                    Stream::Code(code) => stream.push(code.into()),
                })
                .await
                .unwrap();
            assert_eq!(stream, ["id:call-1", "print(1)"]);
            assert_eq!(result.call.unwrap().code, "print(1)");
            assert_eq!(
                result.usage,
                Usage {
                    input_tokens: 20,
                    cached_tokens: 5,
                    output_tokens: 7
                }
            );
        }
        server.await.unwrap();
        assert_eq!(
            *host.calls.lock().unwrap(),
            ["select-resolved", "quota", "select-resolved", "quota"]
        );
    }

    #[tokio::test]
    async fn provider_rate_limit_reports_account_without_retry() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            socket.next().await.unwrap().unwrap();
            socket.send(WsMessage::Text(json!({"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded","message":"weekly quota"}}}).to_string().into())).await.unwrap();
        });
        let host = Host::new();
        let error = model(host.clone(), addr)
            .step(&request(), &mut |_| {})
            .await
            .unwrap_err();
        assert!(error.to_string().contains("weekly quota"));
        assert!(super::super::is_retryable(&error));
        server.await.unwrap();
        assert_eq!(*host.calls.lock().unwrap(), ["select-resolved", "limited"]);
    }

    #[tokio::test]
    async fn provider_failure_codes_control_retryability() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cases = [("server_error", true), ("invalid_request_error", false)];
        let server = tokio::spawn(async move {
            for (code, _) in cases {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                socket.next().await.unwrap().unwrap();
                socket.send(WsMessage::Text(json!({
                    "type":"response.failed", "response":{"error":{"code":code, "message":"failure"}}
                }).to_string().into())).await.unwrap();
            }
        });
        let model = model(Host::new(), addr);
        for (code, retryable) in cases {
            let error = model.step(&request(), &mut |_| {}).await.unwrap_err();
            assert_eq!(super::super::is_retryable(&error), retryable, "{code}");
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn text_mode_omits_exec_tool_and_returns_prose() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let body: Value =
                serde_json::from_str(&socket.next().await.unwrap().unwrap().into_text().unwrap())
                    .unwrap();
            assert_eq!(body["input"][0]["role"], "developer");
            assert_eq!(body["input"].as_array().unwrap().len(), 2);
            assert_eq!(body["input"][1]["content"][0]["text"], "name this");
            socket.send(WsMessage::Text(json!({"type":"response.output_item.done","item":{"type":"message","id":"msg-1","content":[{"type":"output_text","text":"A name"}]}}).to_string().into())).await.unwrap();
            socket
                .send(WsMessage::Text(
                    json!({"type":"response.completed","response":{"usage":{}}})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
        });
        let host = Host::new();
        assert_eq!(
            model(host, addr)
                .text("instructions".into(), "name this".into())
                .await
                .unwrap(),
            "A name"
        );
        server.await.unwrap();
    }
}
