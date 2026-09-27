//! The OpenAI Responses API, as ChatGPT serves it to Codex: one WebSocket
//! kept warm across steps, `response.create` in, events out until the response
//! ends.
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
const PING_INTERVAL: Duration = Duration::from_secs(25);
const MAX_CONNECTION_AGE: Duration = Duration::from_secs(55 * 60);
type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Connection {
    socket: Socket,
    selected: crate::SelectedAuth,
    auth: crate::ResolvedAuth,
    route: DialRoute,
    opened: tokio::time::Instant,
    cache_key: super::CacheKey,
    previous: Option<Previous>,
    ping: tokio::time::Interval,
}

struct Previous {
    id: String,
    lineage: uuid::Uuid,
    at: usize,
    carry: Carry,
    instructions: Arc<str>,
}

/// An idle connection still answers pings and observes close/route changes.
/// Taking it out before a request makes cancellation drop the in-flight socket.
pub(crate) struct Idle {
    take: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Option<Connection>>,
}
impl Drop for Idle {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Idle {
    async fn take(mut self) -> Option<Connection> {
        let _ = self.take.take()?.send(());
        (&mut self.task).await.ok().flatten()
    }
}
impl Connection {
    async fn next(
        &mut self,
        deadline: Option<tokio::time::Instant>,
    ) -> anyhow::Result<Option<WsMessage>> {
        loop {
            tokio::select! {
                _ = async {
                    match deadline {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => return Err(super::Retryable("the provider went quiet".into()).into()),
                _ = self.ping.tick() => self.socket.send(WsMessage::Ping(Vec::new().into())).await?,
                message = self.socket.next() => return Ok(message.transpose()?),
            }
        }
    }

    fn park(mut self, inference: Inference, model: InferenceModel, fast: bool) -> Idle {
        let (take, mut taken) = tokio::sync::oneshot::channel();
        let mut routes = inference.route_updates();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = &mut taken => return result.ok().map(|_| self),
                    changed = routes.changed() => {
                        if changed.is_err() || routes.borrow().for_model(model, fast, Some(&self.selected)) != self.route {
                            return None;
                        }
                    }
                    message = self.next(None) => match message {
                        Ok(Some(WsMessage::Ping(payload))) => {
                            if self.socket.send(WsMessage::Pong(payload)).await.is_err() { return None; }
                        }
                        Ok(Some(WsMessage::Pong(_))) => {}
                        Ok(Some(WsMessage::Text(text))) => {
                            if let Ok(event) = serde_json::from_str::<Value>(&text) {
                                if let Some(quota) = QuotaUpdate::from_event(&event) {
                                    inference.observe_quota(&self.selected, quota).await;
                                } else if matches!(event["type"].as_str(), Some("error" | "response.failed")) {
                                    return None;
                                }
                            }
                        }
                        _ => return None,
                    }
                }
            }
        });
        Idle {
            take: Some(take),
            task,
        }
    }
}

pub struct OpenAi {
    pub(crate) inference: Inference,
    pub(crate) model: InferenceModel,
    pub(crate) effort: Effort,
    pub(crate) fast: bool,
    pub(crate) idle: tokio::sync::Mutex<Option<Idle>>,
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
        self.exchange(request, true, stream).await
    }

    pub(crate) async fn text(
        &self,
        instructions: Arc<str>,
        input: String,
    ) -> anyhow::Result<String> {
        let request = Request::new(
            instructions,
            vec![Item::User {
                text: input,
                images: Vec::new(),
            }],
            super::CacheKey::new(),
        );
        let response = self.exchange(&request, false, &mut |_| {}).await?;
        if response.call.is_some() {
            bail!("text completion returned a tool call");
        }
        Ok(response.prose)
    }

    async fn exchange(
        &self,
        request: &Request,
        tools: bool,
        stream: &mut (dyn FnMut(Stream<'_>) + Send),
    ) -> anyhow::Result<Step> {
        // Hold admission, not the connection, in the mutex while in flight.
        // An aborted caller drops its local connection and cannot reuse a partial turn.
        let mut idle = self.idle.lock().await;
        let (mut selected, resolved) = self.inference.select_resolved().await?;
        selected.account_id = resolved.account_id.clone();
        let route = self
            .inference
            .route_for_model(self.model, self.fast, Some(&selected));
        let connection = match idle.take() {
            Some(parked) => parked.take().await.filter(|c| {
                c.selected == selected
                    && c.auth.bearer_token == resolved.bearer_token
                    && c.auth.client_secret == resolved.client_secret
                    && c.auth.account_id == resolved.account_id
                    && c.route == route
                    && c.cache_key == request.cache_key
                    && c.opened.elapsed() < MAX_CONNECTION_AGE
            }),
            None => None,
        };
        let mut connection = match connection {
            Some(c) => c,
            None => {
                self.connect(request.cache_key, selected, resolved, route)
                    .await?
            }
        };
        let previous = connection.previous.take().filter(|p|
            tools && p.lineage == request.lineage
            && (Arc::ptr_eq(&p.instructions, &request.instructions) || p.instructions == request.instructions)
            && matches!(request.items.get(p.at), Some(Item::Step(carry)) if carry.same_response(&p.carry))
            && !p.carry.has_compaction());
        let mut body = self.body_from(request, previous.as_ref().map(|p| p.at + 1).unwrap_or(0));
        if let Some(previous) = previous {
            body["previous_response_id"] = Value::String(previous.id);
        }
        body["prompt_cache_key"] = request
            .cache_key
            .wire_uuid(
                self.inference.responses_base_url(),
                connection.auth.client_secret,
            )
            .to_string()
            .into();
        if !tools {
            body["input"].as_array_mut().unwrap().remove(0);
        }
        connection
            .socket
            .send(WsMessage::Text(body.to_string().into()))
            .await?;
        let result = self.read_response(&mut connection, stream).await?;
        let (step, response_id, complete_replay) = result;
        if tools && complete_replay && !step.carry.has_compaction() {
            connection.previous = response_id.map(|id| Previous {
                id,
                lineage: request.lineage,
                at: request.items.len(),
                carry: step.carry.clone(),
                instructions: request.instructions.clone(),
            });
        }
        *idle = Some(connection.park(self.inference.clone(), self.model, self.fast));
        Ok(step)
    }

    async fn connect(
        &self,
        cache_key: super::CacheKey,
        selected: crate::SelectedAuth,
        resolved: crate::ResolvedAuth,
        mut route: DialRoute,
    ) -> anyhow::Result<Connection> {
        crate::ensure_crypto_provider();
        let request = ws::request(
            self.inference.responses_base_url(),
            Some(
                &cache_key
                    .wire_uuid(self.inference.responses_base_url(), resolved.client_secret)
                    .to_string(),
            ),
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
                Some(
                    &cache_key
                        .wire_uuid(self.inference.responses_base_url(), resolved.client_secret)
                        .to_string(),
                ),
                &resolved,
            )?;
            connected = ws::connect(request, route).await;
        }
        let (socket, _) = match connected {
            Ok(connection) => connection,
            Err(error) => {
                if websocket_status(&error) == Some(429) {
                    if self.inference.mark_rate_limited(&selected).await {
                        return Err(super::Retryable(error.to_string()).into());
                    }
                    bail!("provider quota exhausted: {error}");
                }
                return Err(error).context("connecting to the Responses endpoint");
            }
        };
        Ok(Connection {
            socket,
            selected,
            auth: resolved,
            route,
            opened: tokio::time::Instant::now(),
            cache_key,
            previous: None,
            ping: tokio::time::interval_at(
                tokio::time::Instant::now() + PING_INTERVAL,
                PING_INTERVAL,
            ),
        })
    }

    async fn read_response(
        &self,
        connection: &mut Connection,
        stream: &mut (dyn FnMut(Stream<'_>) + Send),
    ) -> anyhow::Result<(Step, Option<String>, bool)> {
        let mut deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
        let mut items = Vec::new();
        let mut streaming: Option<(String, Value)> = None;
        loop {
            let message = connection.next(Some(deadline)).await?.ok_or_else(|| {
                super::Retryable("the provider closed the connection mid-response".into())
            })?;
            let text = match message {
                WsMessage::Text(text) => text,
                WsMessage::Ping(payload) => {
                    connection.socket.send(WsMessage::Pong(payload)).await?;
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
            deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
            let event: Value = serde_json::from_str(&text)?;
            if let Some(quota) = QuotaUpdate::from_event(&event) {
                self.inference
                    .observe_quota(&connection.selected, quota)
                    .await;
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
                    let count = items.len();
                    let answer = step(items, &event["response"]["usage"]);
                    let complete_replay = answer
                        .carry
                        .prepared()
                        .is_some_and(|p| p.items.len() == count);
                    return Ok((
                        answer,
                        event["response"]["id"].as_str().map(str::to_owned),
                        complete_replay,
                    ));
                }
                "response.incomplete" => bail!(
                    "response incomplete: {}",
                    event["response"]["incomplete_details"]["reason"]
                ),
                "response.failed" | "error" => {
                    let error = if event["type"] == "error" {
                        event.get("error").unwrap_or(&event)
                    } else {
                        &event["response"]["error"]
                    };
                    let stale = error.to_string().to_ascii_lowercase();
                    let retryable = if stale.contains("previous_response")
                        || stale.contains("previous response")
                        || stale.contains("response not found")
                    {
                        true
                    } else if is_rate_limit(error) {
                        self.inference.mark_rate_limited(&connection.selected).await
                    } else {
                        ["code", "type"].iter().any(|key| {
                            matches!(
                                error[key].as_str(),
                                Some(
                                    "previous_response_not_found"
                                        | "previous_response_id_not_found"
                                        | "server_error"
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

    #[cfg(test)]
    fn body(&self, request: &Request) -> Value {
        self.body_from(request, 0)
    }

    fn body_from(&self, request: &Request, start: usize) -> Value {
        let mut input = if start == 0 {
            vec![
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
            ]
        } else {
            Vec::new()
        };
        // Context already discarded everything before its latest compaction.
        // Cached metadata also handles a compaction within one response.
        let mut compaction_requested = false;
        for item in request.items.iter().skip(start) {
            match item {
                Item::Step(carry) => match &*carry.0 {
                    Inner::OpenAi { .. } => {
                        let prepared = carry.prepared().unwrap();
                        input.extend(
                            prepared
                                .items
                                .iter()
                                .skip(prepared.compaction.unwrap_or(0))
                                .cloned(),
                        );
                    }
                    Inner::ScriptedCompaction => {}
                    Inner::Scripted { call } => {
                        if let Some(call) = call {
                            input.push(call_item(call));
                        }
                    }
                },
                Item::CompactionTrigger => compaction_requested = true,
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
        carry.push(replay);
    }
    Step {
        call,
        prose,
        carry: Carry::from_openai_values(carry),
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
        stable: std::sync::atomic::AtomicBool,
        replacement: std::sync::atomic::AtomicBool,
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
                stable: false.into(),
                replacement: true.into(),
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
                let nth = if self.stable.load(std::sync::atomic::Ordering::Relaxed) {
                    1
                } else {
                    nth
                };
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
                self.replacement.load(std::sync::atomic::Ordering::Relaxed)
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
        Request::new(
            "answer with exec".into(),
            vec![Item::User {
                text: "run".into(),
                images: vec![],
            }],
            super::super::CacheKey::from_u128(19),
        )
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
            idle: Default::default(),
        }
    }

    #[test]
    fn request_contains_one_exec_tool_and_replays_results() {
        let body = local_model().body(&Request::new(
            "instructions".into(),
            vec![
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
            super::super::CacheKey::from_u128(17),
        ));
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
        let request = Request::new(
            "instructions".into(),
            vec![
                Item::User {
                    text: "discard".into(),
                    images: vec![],
                },
                Item::Step(Carry::from_openai_items(vec![
                    json!({"type":"compaction","id":"old","encrypted_content":"old-key"})
                        .to_string(),
                ])),
                Item::Step(Carry::from_openai_items(vec![
                    json!({"type":"reasoning","id":"discard"}).to_string(),
                    latest.to_string(),
                    json!({"type":"message","id":"keep"}).to_string(),
                ])),
                Item::User {
                    text: "keep".into(),
                    images: vec![],
                },
                Item::CompactionTrigger,
            ],
            super::super::CacheKey::from_u128(17),
        );
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
        let Inner::OpenAi { items, .. } = &*answer.carry.0 else {
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
                            request.headers()["thread-id"]
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
    type TestSocket = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

    async fn envelope(socket: &mut TestSocket) -> Value {
        loop {
            match socket.next().await.unwrap().unwrap() {
                WsMessage::Text(text) => return serde_json::from_str(&text).unwrap(),
                WsMessage::Ping(payload) => socket.send(WsMessage::Pong(payload)).await.unwrap(),
                other => panic!("expected request, got {other:?}"),
            }
        }
    }

    async fn complete(socket: &mut TestSocket, id: &str, compacted: bool) {
        let item = if compacted {
            json!({"type":"compaction","id":id,"encrypted_content":"summary"})
        } else {
            json!({"type":"custom_tool_call","name":"exec","id":id,"call_id":format!("call-{id}"),"input":"pass"})
        };
        for event in [
            json!({"type":"response.output_item.done","item":item}),
            json!({"type":"response.completed","response":{"id":id,"usage":{}}}),
        ] {
            socket
                .send(WsMessage::Text(event.to_string().into()))
                .await
                .unwrap();
        }
    }

    async fn infer(model: &crate::step::Model, context: &super::super::Context) -> Step {
        tokio::time::timeout(
            Duration::from_secs(5),
            model.step(
                &context.request("instructions".into(), super::super::CacheKey::from_u128(19)),
                &mut |_| {},
            ),
        )
        .await
        .unwrap()
        .unwrap()
    }

    #[tokio::test]
    async fn warm_socket_uses_suffix_and_compaction_replays_without_reconnecting() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (pong_tx, pong_rx) = tokio::sync::oneshot::channel();
        let header = Arc::new(Mutex::new(String::new()));
        let server_header = header.clone();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_hdr_async(
                tcp,
                |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                    *server_header.lock().unwrap() =
                        request.headers()["session-id"].to_str().unwrap().to_owned();
                    Ok(response)
                },
            )
            .await
            .unwrap();
            let first = envelope(&mut socket).await;
            assert_eq!(first["input"].as_array().unwrap().len(), 3);
            assert!(first.get("previous_response_id").is_none());
            assert_eq!(
                first["prompt_cache_key"].as_str().unwrap(),
                &*header.lock().unwrap()
            );
            complete(&mut socket, "r1", false).await;
            // Server ping must be serviced while no step future is being polled.
            socket
                .send(WsMessage::Ping(vec![9, 4].into()))
                .await
                .unwrap();
            assert_eq!(
                socket.next().await.unwrap().unwrap(),
                WsMessage::Pong(vec![9, 4].into())
            );
            pong_tx.send(()).unwrap();
            let next = envelope(&mut socket).await;
            assert_eq!(next["previous_response_id"], "r1");
            assert_eq!(next["input"].as_array().unwrap().len(), 2);
            assert_eq!(next["input"][0]["type"], "custom_tool_call_output");
            assert_eq!(next["input"][1]["content"][0]["text"], "second");
            complete(&mut socket, "compact", true).await;
            let replay = envelope(&mut socket).await;
            assert!(replay.get("previous_response_id").is_none());
            assert_eq!(replay["input"].as_array().unwrap().len(), 4);
            assert_eq!(replay["input"][2]["type"], "compaction");
            assert_eq!(replay["input"][3]["content"][0]["text"], "third");
            complete(&mut socket, "r3", false).await;
        });
        let host = Host::new();
        host.stable
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let model = model(host, addr);
        let mut context = super::super::Context::default();
        context.push(Item::User {
            text: "first".into(),
            images: vec![],
        });
        let answer = infer(&model, &context).await;
        context.push(Item::Step(answer.carry));
        tokio::time::timeout(Duration::from_secs(5), pong_rx)
            .await
            .unwrap()
            .unwrap();
        context.push(Item::Result {
            call_id: CallId::new("call-r1"),
            text: "output".into(),
            images: vec![],
        });
        context.push(Item::User {
            text: "second".into(),
            images: vec![],
        });
        let answer = infer(&model, &context).await;
        context.push(Item::Step(answer.carry));
        context.push(Item::User {
            text: "third".into(),
            images: vec![],
        });
        infer(&model, &context).await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn stale_continuation_is_reported_then_retry_replays_on_new_socket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            envelope(&mut socket).await;
            complete(&mut socket, "r1", false).await;
            let next = envelope(&mut socket).await;
            assert_eq!(next["previous_response_id"], "r1");
            socket
                .send(WsMessage::Text(
                    json!({"type":"error","code":"previous_response_not_found"})
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            let (tcp, _) = listener.accept().await.unwrap();
            let mut retry = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let full = envelope(&mut retry).await;
            assert!(full.get("previous_response_id").is_none());
            assert_eq!(full["input"].as_array().unwrap().len(), 5);
            assert_eq!(full["input"][3]["call_id"], "call-r1");
            complete(&mut retry, "r2", false).await;
        });
        let host = Host::new();
        host.stable
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let model = model(host, addr);
        let mut context = super::super::Context::default();
        context.push(Item::User {
            text: "first".into(),
            images: vec![],
        });
        let answer = infer(&model, &context).await;
        context.push(Item::Step(answer.carry));
        context.push(Item::Result {
            call_id: CallId::new("call-r1"),
            text: "result".into(),
            images: vec![],
        });
        let request = context.request("instructions".into(), super::super::CacheKey::from_u128(19));
        let error = tokio::time::timeout(Duration::from_secs(5), model.step(&request, &mut |_| {}))
            .await
            .unwrap()
            .unwrap_err();
        assert!(super::super::is_retryable(&error));
        infer(&model, &context).await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rebuilt_context_and_changed_instructions_do_not_chain() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            for id in ["r1", "r2", "r3"] {
                let body = envelope(&mut socket).await;
                assert!(body.get("previous_response_id").is_none());
                assert_eq!(body["input"][0]["type"], "additional_tools");
                complete(&mut socket, id, false).await;
            }
        });
        let host = Host::new();
        host.stable
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let model = model(host, addr);
        let mut original = super::super::Context::default();
        original.push(Item::User {
            text: "original".into(),
            images: vec![],
        });
        let first = infer(&model, &original).await;
        let mut rebuilt = super::super::Context::default();
        rebuilt.push(Item::User {
            text: "different prefix".into(),
            images: vec![],
        });
        rebuilt.push(Item::Step(first.carry));
        let second = infer(&model, &rebuilt).await;
        rebuilt.push(Item::Step(second.carry));
        tokio::time::timeout(
            Duration::from_secs(5),
            model.step(
                &rebuilt.request("changed".into(), super::super::CacheKey::from_u128(19)),
                &mut |_| {},
            ),
        )
        .await
        .unwrap()
        .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_stream_drops_socket_and_never_replays_it_internally() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (admitted, wait) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            envelope(&mut socket).await;
            socket.send(WsMessage::Text(json!({"type":"response.output_item.added","output_index":0,"item":{"type":"custom_tool_call","id":"i","name":"exec","call_id":"c"}}).to_string().into())).await.unwrap();
            socket.send(WsMessage::Text(json!({"type":"response.custom_tool_call_input.delta","item_id":"i","delta":"print(9)"}).to_string().into())).await.unwrap();
            let (tcp, _) = listener.accept().await.unwrap();
            let mut fresh = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let body = envelope(&mut fresh).await;
            assert!(body.get("previous_response_id").is_none());
            complete(&mut fresh, "fresh", false).await;
        });
        let host = Host::new();
        host.stable
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let model = Arc::new(model(host, addr));
        let running = model.clone();
        let task = tokio::spawn(async move {
            let mut admitted = Some(admitted);
            running
                .step(&request(), &mut |event| {
                    if matches!(event, Stream::Code(_)) {
                        if let Some(tx) = admitted.take() {
                            let _ = tx.send(());
                        }
                    }
                })
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .unwrap()
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), model.step(&request(), &mut |_| {}))
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();
    }
    #[tokio::test]
    async fn pong_traffic_cannot_extend_the_active_event_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let host = Host::new();
        let crate::step::Model::OpenAi(openai) = model(host.clone(), addr) else {
            unreachable!()
        };
        let connecting = tokio::spawn(async move {
            let (mut selected, resolved) = openai.inference.select_resolved().await.unwrap();
            selected.account_id = resolved.account_id.clone();
            let connection = openai
                .connect(
                    super::super::CacheKey::from_u128(1),
                    selected,
                    resolved,
                    DialRoute::Dns,
                )
                .await
                .unwrap();
            (openai, connection)
        });
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let (openai, mut connection) = connecting.await.unwrap();
        tokio::time::pause();
        let start = tokio::time::Instant::now();
        let reading =
            tokio::spawn(async move { openai.read_response(&mut connection, &mut |_| {}).await });
        tokio::task::yield_now().await;
        tokio::time::advance(EVENT_TIMEOUT / 2).await;
        socket.send(WsMessage::Pong(vec![1].into())).await.unwrap();
        // Give the received frame a chance to be processed before the deadline.
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(EVENT_TIMEOUT / 2 + Duration::from_secs(1)).await;
        let error = reading.await.unwrap().unwrap_err();
        assert!(super::super::is_retryable(&error));
        assert!(error.to_string().contains("went quiet"));
        assert!(start.elapsed() <= EVENT_TIMEOUT + Duration::from_secs(1));
    }

    #[tokio::test]
    async fn changing_cache_key_reopens_socket_and_quota_without_replacement_stops() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let first = envelope(&mut socket).await;
            complete(&mut socket, "r1", false).await;
            let (tcp, _) = listener.accept().await.unwrap();
            let mut fresh = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let next = envelope(&mut fresh).await;
            assert!(next.get("previous_response_id").is_none());
            assert_ne!(first["prompt_cache_key"], next["prompt_cache_key"]);
            fresh.send(WsMessage::Text(json!({"type":"response.failed","response":{"error":{"code":"usage_limit_reached"}}}).to_string().into())).await.unwrap();
        });
        let host = Host::new();
        host.stable
            .store(true, std::sync::atomic::Ordering::Relaxed);
        host.replacement
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let model = model(host, addr);
        let mut context = super::super::Context::default();
        context.push(Item::User {
            text: "first".into(),
            images: vec![],
        });
        let answer = infer(&model, &context).await;
        context.push(Item::Step(answer.carry));
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            model.step(
                &context.request("instructions".into(), super::super::CacheKey::from_u128(99)),
                &mut |_| {},
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(!super::super::is_retryable(&error));
        server.await.unwrap();
    }
}
