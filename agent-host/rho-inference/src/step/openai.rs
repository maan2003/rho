//! The OpenAI Responses API, as ChatGPT serves it to Codex: one WebSocket
//! kept warm across steps, `response.create` in, events out until the response
//! ends.
//!
//! Every model rho uses takes the Responses Lite shape: the tool and the
//! instructions are developer items at the head of the input, not top-level
//! fields.

use std::borrow::Cow;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use base64::Engine as _;
use futures_util::{SinkExt, StreamExt};
use rho_agent::inference::{CacheKey, Continuation, Retryable};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::{
    Call, CallResult, EXEC, Event, Image, Item, Observation, Request, Response, Step, Usage,
};
use crate::config::{InferenceModel, InferenceProfile, ReasoningEffort};
use crate::responses::{DialRoute, QuotaUpdate, ws};

/// Replay fragments must reach serde_json's writer without becoming Values.
#[derive(serde::Serialize)]
struct Body<'a> {
    #[serde(flatten)]
    fields: Value,
    input: Vec<Cow<'a, RawValue>>,
}
#[derive(serde::Deserialize)]
struct Incoming<'a> {
    #[serde(rename = "type", borrow)]
    kind: Cow<'a, str>,
    #[serde(default, borrow)]
    item: Option<&'a RawValue>,
}

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
    cache_key: CacheKey,
    previous: Option<Previous>,
    ping: tokio::time::Interval,
}

struct Previous {
    id: String,
    instructions: Arc<str>,
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
                } => return Err(Retryable("the provider went quiet".into()).into()),
                _ = self.ping.tick() => self.socket.send(WsMessage::Ping(Vec::new().into())).await?,
                message = self.socket.next() => return Ok(message.transpose()?),
            }
        }
    }
}
/// One task owns the socket for its entire lifetime, including idle keepalive.
#[derive(Clone)]
pub struct InferenceSession {
    commands: mpsc::UnboundedSender<Start>,
}

struct Start {
    request: Request,
    selected: crate::SelectedAuth,
    auth: crate::ResolvedAuth,
    tools: bool,
    events: mpsc::UnboundedSender<Event>,
}

struct Session {
    base_url: Arc<str>,
    model: InferenceModel,
    effort: ReasoningEffort,
    fast: bool,
    routes: watch::Receiver<crate::RouteSelection>,
    observations: mpsc::UnboundedSender<Observation>,
}

impl InferenceSession {
    pub fn new(
        base_url: Arc<str>,
        profile: InferenceProfile,
        model: InferenceModel,
        routes: watch::Receiver<crate::RouteSelection>,
    ) -> (Self, mpsc::UnboundedReceiver<Observation>) {
        let (commands, incoming) = mpsc::unbounded_channel();
        let (observations, reports) = mpsc::unbounded_channel();
        let session = Session {
            base_url,
            model,
            effort: profile.effort,
            fast: profile.fast_mode,
            routes,
            observations,
        };
        tokio::spawn(session.run(incoming));
        (Self { commands }, reports)
    }

    pub fn start(
        &self,
        request: Request,
        selected: crate::SelectedAuth,
        auth: crate::ResolvedAuth,
    ) -> Response {
        self.send(request, selected, auth, true)
    }

    pub fn text_start(
        &self,
        instructions: Arc<str>,
        input: String,
        selected: crate::SelectedAuth,
        auth: crate::ResolvedAuth,
    ) -> Response {
        self.send(
            Request::new(
                instructions,
                vec![Item::User {
                    text: input,
                    images: vec![],
                }],
                CacheKey::new(),
            ),
            selected,
            auth,
            false,
        )
    }

    fn send(
        &self,
        request: Request,
        selected: crate::SelectedAuth,
        auth: crate::ResolvedAuth,
        tools: bool,
    ) -> Response {
        let (events, response) = mpsc::unbounded_channel();
        let _ = self.commands.send(Start {
            request,
            selected,
            auth,
            tools,
            events,
        });
        response
    }
}

impl Session {
    async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Start>) {
        let mut connection: Option<Connection> = None;
        loop {
            tokio::select! {
                biased;
                start = commands.recv() => {
                    let Some(start) = start else { return };
                    // A queued request may have been abandoned before admission.
                    if start.events.is_closed() { continue; }
                    let result = tokio::select! {
                        biased;
                        _ = start.events.closed() => {
                            connection = None;
                            continue;
                        }
                        result = self.exchange(&mut connection, &start) => result,
                    };
                    let event = match result {
                        Ok(event) => event,
                        Err(error) => {
                            connection = None;
                            Event::Failed(error)
                        }
                    };
                    if start.events.send(event).is_err() {
                        // Caller did not accept completion; never continue its turn.
                        connection = None;
                    }
                }
                changed = self.routes.changed() => {
                    if changed.is_err() { return; }
                    if connection.as_ref().is_some_and(|c|
                        self.routes.borrow().for_model(self.model, self.fast, Some(&c.selected)) != c.route
                    ) { connection = None; }
                }
                message = async { connection.as_mut().unwrap().next(None).await }, if connection.is_some() => {
                    let conn = connection.as_mut().unwrap();
                    match message {
                        Ok(Some(WsMessage::Ping(payload))) => {
                            if conn.socket.send(WsMessage::Pong(payload)).await.is_err() { connection = None; }
                        }
                        Ok(Some(WsMessage::Pong(_))) => {}
                        Ok(Some(WsMessage::Text(text))) => {
                            if let Ok(event) = serde_json::from_str::<Value>(&text) {
                                if let Some(quota) = QuotaUpdate::from_event(&event) {
                                    self.quota(&conn.selected, quota).await;
                                } else if matches!(event["type"].as_str(), Some("error" | "response.failed")) {
                                    connection = None;
                                }
                            }
                        }
                        _ => connection = None,
                    }
                }
            }
        }
    }

    async fn quota(&self, selected: &crate::SelectedAuth, quota: QuotaUpdate) {
        let (done, acknowledged) = oneshot::channel();
        let _ = self.observations.send(Observation::Quota {
            selected: selected.clone(),
            quota,
            done,
        });
        let _ = acknowledged.await;
    }

    async fn exchange(
        &self,
        slot: &mut Option<Connection>,
        start: &Start,
    ) -> anyhow::Result<Event> {
        let request = &start.request;
        let tools = start.tools;
        let mut selected = start.selected.clone();
        let resolved = start.auth.clone();
        selected.account_id = resolved.account_id.clone();
        let route = self
            .routes
            .borrow()
            .for_model(self.model, self.fast, Some(&selected));
        if slot.as_ref().is_some_and(|c| {
            c.selected != selected
                || c.auth.bearer_token != resolved.bearer_token
                || c.auth.client_secret != resolved.client_secret
                || c.auth.account_id != resolved.account_id
                || c.route != route
                || c.cache_key != request.cache_key
                || c.opened.elapsed() >= MAX_CONNECTION_AGE
        }) {
            *slot = None;
        }
        if slot.is_none() {
            *slot = Some(
                self.connect(request.cache_key, selected, resolved, route)
                    .await?,
            );
        }
        let connection = slot.as_mut().unwrap();
        let previous = connection.previous.take().filter(|p| {
            tools
                && request.previous_response_id.as_ref() == Some(&p.id)
                && p.instructions == request.instructions
        });
        if request.previous_response_id.is_some() && previous.is_none() {
            return Ok(Event::NeedsContext);
        }
        let mut body = self.body_from(request, previous.is_some());
        if let Some(previous) = previous {
            body.fields["previous_response_id"] = Value::String(previous.id);
        }
        body.fields["prompt_cache_key"] = super::wire_uuid(
            request.cache_key,
            &self.base_url,
            connection.auth.client_secret,
        )
        .to_string()
        .into();
        if !tools {
            body.input.remove(0);
        }
        connection
            .socket
            .send(WsMessage::Text(serde_json::to_string(&body)?.into()))
            .await?;
        let (mut step, response_id, complete_replay) =
            self.read_response(connection, &start.events).await?;
        if tools && complete_replay && !step.carry.has_compaction() {
            step.continuation = response_id.clone().map(Continuation::new);
            connection.previous = response_id.map(|id| Previous {
                id,
                instructions: request.instructions.clone(),
            });
        }
        Ok(Event::Completed(step))
    }

    async fn connect(
        &self,
        cache_key: CacheKey,
        selected: crate::SelectedAuth,
        resolved: crate::ResolvedAuth,
        mut route: DialRoute,
    ) -> anyhow::Result<Connection> {
        crate::ensure_crypto_provider();
        let request = ws::request(
            &self.base_url,
            Some(&super::wire_uuid(cache_key, &self.base_url, resolved.client_secret).to_string()),
            &resolved,
        )?;
        let mut connected = ws::connect(request, route).await;
        if let Err(error) = &connected
            && route != DialRoute::Dns
            && !matches!(websocket_status(error), Some(401 | 403 | 429))
        {
            let (done, acknowledged) = oneshot::channel();
            let _ = self.observations.send(Observation::RouteFailed {
                route,
                selected: selected.clone(),
                done,
            });
            let _ = acknowledged.await;
            route = DialRoute::Dns;
            let request = ws::request(
                &self.base_url,
                Some(
                    &super::wire_uuid(cache_key, &self.base_url, resolved.client_secret)
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
                    return Err(super::RateLimited(error.to_string()).into());
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
        events: &mpsc::UnboundedSender<Event>,
    ) -> anyhow::Result<(Step, Option<String>, bool)> {
        let mut deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
        let mut items = Vec::new();
        let mut streaming: Option<(String, Value)> = None;
        loop {
            let message = connection.next(Some(deadline)).await?.ok_or_else(|| {
                Retryable("the provider closed the connection mid-response".into())
            })?;
            let text = match message {
                WsMessage::Text(text) => text,
                WsMessage::Ping(payload) => {
                    connection.socket.send(WsMessage::Pong(payload)).await?;
                    continue;
                }
                WsMessage::Close(frame) => {
                    return Err(Retryable(format!(
                        "the provider closed the connection: {frame:?}"
                    ))
                    .into());
                }
                _ => continue,
            };
            deadline = tokio::time::Instant::now() + EVENT_TIMEOUT;
            let incoming: Incoming<'_> = serde_json::from_str(&text)?;
            if incoming.kind == "response.output_item.done" {
                items.push(
                    incoming
                        .item
                        .ok_or_else(|| anyhow::anyhow!("completed output item missing"))?
                        .to_owned(),
                );
                continue;
            }
            if matches!(
                incoming.kind.as_ref(),
                "response.completed" | "response.done"
            ) {
                #[derive(serde::Deserialize)]
                struct Completion {
                    response: Completed,
                }
                #[derive(serde::Deserialize)]
                struct Completed {
                    id: Option<String>,
                    #[serde(default)]
                    usage: Value,
                }
                // item.done is the source of items; do not parse/copy response.output.
                let completed: Completion = serde_json::from_str(&text)?;
                let count = items.len();
                let answer = step(items, &completed.response.usage);
                let complete_replay = super::prepared(&answer.carry).items.len() == count;
                return Ok((answer, completed.response.id, complete_replay));
            }
            let event: Value = serde_json::from_str(&text)?;
            if let Some(quota) = QuotaUpdate::from_event(&event) {
                self.quota(&connection.selected, quota).await;
            }
            match event["type"].as_str().unwrap_or_default() {
                "response.output_item.added" if streaming.is_none() && is_call(&event["item"]) => {
                    let item = &event["item"];
                    streaming = Some((
                        item["id"].as_str().unwrap_or_default().to_owned(),
                        event["output_index"].clone(),
                    ));
                    let _ = events.send(Event::Call {
                        carry: super::bare(Call::new(
                            item["call_id"].as_str().unwrap_or_default(),
                            String::new(),
                        )),
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
                    let _ = events.send(Event::Code(
                        event["delta"].as_str().unwrap_or_default().to_owned(),
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
                    if is_rate_limit(error) {
                        return Err(super::RateLimited(format!("provider error: {error}")).into());
                    }
                    let retryable = ["code", "type"].iter().any(|key| {
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
                                    | "service_unavailable_error"
                                    | "websocket_connection_limit_reached"
                                    | "slow_down"
                            )
                        )
                    });
                    if retryable {
                        return Err(Retryable(format!("provider error: {error}")).into());
                    }
                    bail!("provider error: {error}");
                }
                _ => {}
            }
        }
    }

    #[cfg(test)]
    fn body(&self, request: &Request) -> Value {
        serde_json::from_str(&serde_json::to_string(&self.body_from(request, false)).unwrap())
            .unwrap()
    }

    fn body_from<'a>(&self, request: &'a Request, continuation: bool) -> Body<'a> {
        let input = if !continuation {
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
        let mut input: Vec<Cow<'a, RawValue>> = input
            .into_iter()
            .map(|item| Cow::Owned(serde_json::value::to_raw_value(&item).expect("request item")))
            .collect();
        // Full requests discard everything before their latest compaction.
        // Cached metadata also handles a compaction within one response.
        let mut compaction_requested = false;
        for item in request.items.iter() {
            match item {
                Item::Step(carry) => {
                    let prepared = super::prepared(carry);
                    input.extend(
                        prepared
                            .items
                            .into_iter()
                            .skip(prepared.compaction.unwrap_or(0))
                            .map(Cow::Borrowed),
                    );
                }
                Item::CompactionTrigger => compaction_requested = true,
                Item::Result(CallResult {
                    id: call_id,
                    function,
                    text,
                    images,
                }) => input.push(Cow::Owned(serde_json::value::to_raw_value(&json!({
                    "type": if *function { "function_call_output" } else { "custom_tool_call_output" },
                    "call_id": call_id.as_str(),
                    "output": content(text, images, true),
                })).expect("tool output"))),
                Item::User { text, images } => input.push(Cow::Owned(serde_json::value::to_raw_value(&json!({
                    "type": "message",
                    "role": "user",
                    "content": content(text, images, false),
                })).expect("user message"))),
            }
        }
        if compaction_requested {
            input.push(Cow::Owned(
                serde_json::value::to_raw_value(&json!({ "type": "compaction_trigger" })).unwrap(),
            ));
        }
        Body {
            input,
            fields: json!({
                "type": "response.create",
                "model": self.model.as_str(),
                "instructions": "",
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
            }),
        }
    }
}

fn is_call(item: &Value) -> bool {
    item["type"] == "custom_tool_call" && item["name"] == EXEC
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

/// Extract execution/display facts, but keep every completed item verbatim.
fn step(items: Vec<Box<RawValue>>, usage: &Value) -> Step {
    let mut prose = String::new();
    for raw in &items {
        let meta: super::ItemMeta<'_> =
            serde_json::from_str(raw.get()).expect("completed item metadata");
        if meta.kind == "message" {
            #[derive(serde::Deserialize)]
            struct Message {
                #[serde(default)]
                content: Vec<Part>,
            }
            #[derive(serde::Deserialize)]
            struct Part {
                text: Option<String>,
            }
            let message: Message = serde_json::from_str(raw.get()).expect("completed message");
            for part in message.content {
                if let Some(text) = part.text {
                    prose.push_str(&text);
                }
            }
        }
    }
    let carry = super::from_raw_items(items, true);
    let call = carry.display_calls().into_iter().next();
    Step {
        continuation: None,
        call,
        prose,
        carry,
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

    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::accounts::SelectedAuth;
    use crate::inference::{Inference, InferenceConfig, PolicyReply, PolicyRequest};
    use crate::responses::{InferenceAuth, RouteSelection};

    #[derive(Debug)]
    struct Host {
        selected: SelectedAuth,
        calls: Mutex<Vec<&'static str>>,
        route: watch::Sender<RouteSelection>,
        credentials: watch::Sender<crate::CredentialSnapshot>,
        stable: std::sync::atomic::AtomicBool,
        replacement: std::sync::atomic::AtomicBool,
    }
    impl Host {
        fn snapshot(&self, nth: usize) -> crate::CredentialSnapshot {
            crate::CredentialSnapshot {
                revision: nth as u64,
                state: crate::CredentialState::Ready {
                    selected: self.selected.clone(),
                    auth: crate::ResolvedAuth {
                        bearer_token: format!("ephemeral-{nth}"),
                        account_id: Some("account-9".into()),
                        client_secret: [0; 32],
                    },
                    refresh_at: u64::MAX,
                },
            }
        }

        fn new() -> Arc<Self> {
            let (route, _) = watch::channel(RouteSelection::default());
            let (credentials, _) = watch::channel(crate::CredentialSnapshot {
                revision: 0,
                state: crate::CredentialState::Pending,
            });
            let host = Arc::new(Self {
                selected: SelectedAuth {
                    auth: InferenceAuth::oauth_file("/nonexistent/host-only"),
                    namespace: Some("test".into()),
                    account_id: None,
                },
                calls: Mutex::new(vec![]),
                route,
                credentials,
                stable: false.into(),
                replacement: true.into(),
            });
            host.credentials.send_replace(host.snapshot(1));
            host
        }

        fn policy(self: &Arc<Self>, addr: std::net::SocketAddr) -> Inference {
            let (calls, mut receiver) = mpsc::channel::<crate::inference::PolicyCall>(32);
            let host = self.clone();
            tokio::spawn(async move {
                while let Some(call) = receiver.recv().await {
                    let reply = match call.body {
                        PolicyRequest::RateLimited(selected) => {
                            assert_eq!(selected.account_id.as_deref(), Some("account-9"));
                            host.calls.lock().unwrap().push("limited");
                            PolicyReply::RateLimited(
                                host.replacement.load(std::sync::atomic::Ordering::Relaxed),
                            )
                        }
                        PolicyRequest::Quota { selected, quota } => {
                            assert_eq!(selected.account_id.as_deref(), Some("account-9"));
                            assert_eq!(quota.weekly_used_percent, 17);
                            assert_eq!(quota.routing_used_percent, 83);
                            host.calls.lock().unwrap().push("quota");
                            if !host.stable.load(std::sync::atomic::Ordering::Relaxed) {
                                let nth = host.credentials.borrow().revision as usize + 1;
                                host.credentials.send_replace(host.snapshot(nth));
                            }
                            PolicyReply::Done
                        }
                        PolicyRequest::RouteFailed { .. } => panic!("unexpected route failure"),
                        PolicyRequest::SelectAccount | PolicyRequest::ResolveAuth(_) => {
                            panic!("transport must use pushed credential selection")
                        }
                    };
                    let _ = call.reply.send(Ok(reply));
                }
            });
            let (_closed, closed) = watch::channel(false);
            Inference::from_worker(
                calls,
                self.credentials.subscribe(),
                self.route.subscribe(),
                closed,
                InferenceConfig::with_responses_base_url(format!("http://{addr}")).unwrap(),
            )
        }
    }

    struct TestInferenceSession {
        session: InferenceSession,
        policy: Inference,
    }
    impl TestInferenceSession {
        async fn step(
            &self,
            request: &Request,
            stream: &mut (dyn FnMut(Event) + Send),
        ) -> anyhow::Result<Step> {
            let (selected, auth) = self.policy.select_resolved().await?;
            let mut response = self.session.start(request.clone(), selected.clone(), auth);
            while let Some(event) = response.recv().await {
                match event {
                    Event::Completed(step) => return Ok(step),
                    Event::NeedsContext => return Err(anyhow::anyhow!("needs context")),
                    Event::Failed(error) => {
                        if self.policy.retryable(&error, &selected).await {
                            return Err(Retryable(error.to_string()).into());
                        }
                        return Err(error);
                    }
                    event => stream(event),
                }
            }
            bail!("session closed")
        }
        async fn text(&self, instructions: Arc<str>, input: String) -> anyhow::Result<String> {
            self.policy.text(instructions, input).await
        }
    }

    fn session(host: Arc<Host>, addr: std::net::SocketAddr) -> TestInferenceSession {
        let policy = host.policy(addr);
        TestInferenceSession {
            session: policy.session(Default::default(), InferenceModel::Gpt61Sol),
            policy,
        }
    }

    fn request() -> Request {
        Request::new(
            "answer with exec".into(),
            vec![Item::User {
                text: "run".into(),
                images: vec![],
            }],
            CacheKey::from_u128(19),
        )
    }

    fn local_model() -> Session {
        let (_routes, routes) = watch::channel(crate::RouteSelection::default());
        let (observations, _) = mpsc::unbounded_channel();
        Session {
            base_url: "http://127.0.0.1:1".into(),
            model: InferenceModel::Gpt61Sol,
            effort: ReasoningEffort::Low,
            fast: false,
            routes,
            observations,
        }
    }

    #[tokio::test]
    async fn item_done_bytes_survive_persistence_and_actual_wire_replay() {
        const ITEMS: [&str; 5] = [
            r#"{ "type" : "reasoning", "id":"rs", "encrypted_content":"opaque\u002b", "summary":[], "future":1e+9999 }"#,
            r#"{"type":"message", "id":"msg", "role":"assistant","phase":"commentary","content":[{"type":"output_text","text":"hi\u0021","annotations":[{"type":"future","v":3.00}]},{"type":"refusal","refusal":"unchanged"}]}"#,
            r#"{ "type":"function_call","id":"fc","call_id":"function-7","name":"exec","arguments":"print(7)","status":"completed","future":{"z":0,"a":-0.0} }"#,
            r#"{"type":"future_item", "unrecognized":[1,  2],"text":"\u0061"}"#,
            r#"{ "type":"custom_tool_call","id":"extra","call_id":"not-executed","name":"exec","input":"must_not_run()" }"#,
        ];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            envelope(&mut socket).await;
            for item in ITEMS {
                socket
                    .send(WsMessage::Text(
                        format!(r#"{{"type":"response.output_item.done","item":{item}}}"#).into(),
                    ))
                    .await
                    .unwrap();
            }
            // Completed output is deliberately different. item.done owns item bytes.
            socket.send(WsMessage::Text(r#"{"type":"response.completed","response":{"id":"raw-r1","usage":{},"output":[{"type":"message","content":[{"text":"wrong source","unknown":1e+9999}]}]}}"#.into())).await.unwrap();
            let wire = loop {
                match socket.next().await.unwrap().unwrap() {
                    WsMessage::Text(text) => break text,
                    WsMessage::Ping(bytes) => socket.send(WsMessage::Pong(bytes)).await.unwrap(),
                    _ => {}
                }
            };
            #[derive(serde::Deserialize)]
            struct Input {
                input: Vec<Box<RawValue>>,
            }
            let sent: Input = serde_json::from_str(&wire).unwrap();
            assert_eq!(sent.input.len(), 7); // tools, instructions, four retained items, result
            for (actual, expected) in sent.input[2..6].iter().zip(&ITEMS[..4]) {
                assert_eq!(actual.get(), *expected, "replayed item bytes changed");
            }
            let result: Value = serde_json::from_str(sent.input[6].get()).unwrap();
            assert_eq!(
                result,
                json!({"type":"function_call_output","call_id":"function-7","output":"seven"})
            );
            assert!(
                !wire.contains("not-executed"),
                "never replay an unanswered extra call"
            );
            complete(&mut socket, "raw-r2", false).await;
        });
        let host = Host::new();
        host.stable
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let session = session(host, addr);
        let answer = infer(&session, full(vec![user("first")])).await;
        assert_eq!(answer.prose, "hi!");
        assert_eq!(answer.call.as_ref().unwrap().code, "print(7)");
        assert!(
            answer.continuation.is_none(),
            "filtered response cannot use server continuation"
        );
        let entry = rho_agent::entry::Entry::Step {
            at: rho_agent_types::UnixMs(9),
            exec: Some("print(7)".into()),
            prose: answer.prose,
            carry: answer.carry,
            usage: None,
        };
        let bytes = senax_encoder::encode(&entry).unwrap();
        let decoded: rho_agent::entry::Entry = senax_encoder::decode(&mut bytes.as_ref()).unwrap();
        let rho_agent::entry::Entry::Step { carry, .. } = decoded else {
            panic!()
        };
        let stored = super::super::replay(&carry);
        assert_eq!(
            stored.items.len(),
            ITEMS.len(),
            "store every done item, even unexecuted calls"
        );
        for (actual, expected) in stored.items.iter().zip(ITEMS) {
            assert_eq!(actual.get(), expected, "persisted item bytes changed");
        }
        let results = super::super::reply(&carry, "seven", &[]);
        let mut replay = vec![Item::Step(carry)];
        replay.extend(results.into_iter().map(Item::Result));
        infer(&session, full(replay)).await;
        server.await.unwrap();
    }

    #[test]
    fn compaction_selects_raw_items_without_rewriting_them() {
        const OLD: &str = r#"{"type":"message","id":"old"}"#;
        const SUMMARY: &str =
            r#"{ "type":"compaction", "encrypted_content":"a\u002bb", "future":2e3 }"#;
        const TAIL: &str = r#"{ "type":"future", "payload": [1,  4] }"#;
        let carry = super::super::from_raw_items(
            [OLD, SUMMARY, TAIL]
                .into_iter()
                .map(|s| RawValue::from_string(s.into()).unwrap())
                .collect(),
            true,
        );
        let request = full(vec![user("evicted"), Item::Step(carry)]);
        let model = local_model();
        let wire = serde_json::to_string(&model.body_from(&request, false)).unwrap();
        #[derive(serde::Deserialize)]
        struct Input {
            input: Vec<Box<RawValue>>,
        }
        let sent: Input = serde_json::from_str(&wire).unwrap();
        assert_eq!(sent.input.len(), 4);
        assert_eq!(sent.input[2].get(), SUMMARY);
        assert_eq!(sent.input[3].get(), TAIL);
        assert!(!wire.contains("evicted"));
        assert!(!wire.contains("\"old\""));
    }

    #[test]
    fn request_contains_one_exec_tool_and_replays_results() {
        let body = local_model().body(&Request::new(
            "instructions".into(),
            vec![
                Item::Result(CallResult {
                    id: "call-1".to_owned(),
                    function: false,
                    text: "result".into(),
                    images: vec![],
                }),
                Item::User {
                    text: "next".into(),
                    images: vec![],
                },
            ],
            CacheKey::from_u128(17),
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
                Item::Step(super::super::from_openai_items(vec![
                    json!({"type":"compaction","id":"old","encrypted_content":"old-key"})
                        .to_string(),
                ])),
                Item::Step(super::super::from_openai_items(vec![
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
            CacheKey::from_u128(17),
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
            ]
            .iter()
            .map(|item| serde_json::value::to_raw_value(item).unwrap())
            .collect(),
            &json!({"input_tokens":10,"output_tokens":3,"input_tokens_details":{"cached_tokens":4}}),
        );
        assert_eq!(answer.call.unwrap().display_id(), "first");
        assert_eq!(answer.prose, "prose");
        assert_eq!(answer.usage.cached_tokens, 4);
        assert_eq!(super::super::call_ids(&answer.carry), ["first".to_owned()]);
        let items = super::super::prepared(&answer.carry).items;
        assert_eq!(items.len(), 3);
        assert_eq!(
            serde_json::from_str::<Value>(items[2].get()).unwrap()["call_id"],
            "first"
        );
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
        let session = session(host.clone(), addr);
        for _ in 0..2 {
            let mut stream = Vec::new();
            let result = session
                .step(&request(), &mut |event| match event {
                    Event::Call { carry } => stream.push(format!(
                        "id:{}",
                        super::super::with_code(&carry, String::new()).display_id()
                    )),
                    Event::Code(code) => stream.push(code),
                    _ => unreachable!(),
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
        assert_eq!(*host.calls.lock().unwrap(), ["quota", "quota"]);
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
        let error = session(host.clone(), addr)
            .step(&request(), &mut |_| {})
            .await
            .unwrap_err();
        assert!(error.to_string().contains("weekly quota"));
        assert!(super::super::is_retryable(&error));
        server.await.unwrap();
        assert_eq!(*host.calls.lock().unwrap(), ["limited"]);
    }

    #[tokio::test]
    async fn provider_failure_codes_control_retryability() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let cases = [
            ("server_error", true),
            ("service_unavailable_error", true),
            ("websocket_connection_limit_reached", true),
            ("previous_response_not_found", true),
            ("invalid_request_error", false),
        ];
        let server = tokio::spawn(async move {
            for (code, _) in cases {
                let (stream, _) = listener.accept().await.unwrap();
                let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
                socket.next().await.unwrap().unwrap();
                socket.send(WsMessage::Text(json!({
                    "type":"response.failed", "response":{"error":{"code":code, "message":"previous response not found"}}
                }).to_string().into())).await.unwrap();
            }
        });
        let session = session(Host::new(), addr);
        for (code, retryable) in cases {
            let error = session.step(&request(), &mut |_| {}).await.unwrap_err();
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
            session(host, addr)
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

    async fn infer(session: &TestInferenceSession, request: Request) -> Step {
        tokio::time::timeout(Duration::from_secs(5), session.step(&request, &mut |_| {}))
            .await
            .unwrap()
            .unwrap()
    }

    fn full(items: Vec<Item>) -> Request {
        Request::new("instructions".into(), items, CacheKey::from_u128(19))
    }

    fn delta(previous: &str, items: Vec<Item>) -> Request {
        Request::continuation(
            "instructions".into(),
            items,
            CacheKey::from_u128(19),
            previous.into(),
        )
    }

    fn user(text: &str) -> Item {
        Item::User {
            text: text.into(),
            images: vec![],
        }
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
        let session = session(host, addr);
        let answer = infer(&session, full(vec![user("first")])).await;
        assert_eq!(
            answer
                .continuation
                .clone()
                .map(|id| id.into_token())
                .as_deref(),
            Some("r1")
        );
        // Caller discards the old response; continuation needs only its ID.
        drop(answer);
        tokio::time::timeout(Duration::from_secs(5), pong_rx)
            .await
            .unwrap()
            .unwrap();
        let answer = infer(
            &session,
            delta(
                "r1",
                vec![
                    Item::Result(CallResult {
                        id: "call-r1".to_owned(),
                        function: false,
                        text: "output".into(),
                        images: vec![],
                    }),
                    user("second"),
                ],
            ),
        )
        .await;
        assert!(
            answer.continuation.is_none(),
            "compaction requires full replay"
        );
        infer(
            &session,
            full(vec![Item::Step(answer.carry), user("third")]),
        )
        .await;
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
        let session = session(host, addr);
        let answer = infer(&session, full(vec![user("first")])).await;
        let suffix = vec![Item::Result(CallResult {
            id: "call-r1".to_owned(),
            function: false,
            text: "result".into(),
            images: vec![],
        })];
        let request = delta("r1", suffix.clone());
        let error = session.step(&request, &mut |_| {}).await.unwrap_err();
        assert!(super::super::is_retryable(&error));
        // A retry cannot send just the suffix on the replacement connection.
        let error = session
            .step(&request, &mut |_| panic!("no code before replay"))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "needs context");
        let mut replay = vec![user("first"), Item::Step(answer.carry)];
        replay.extend(suffix);
        infer(&session, full(replay)).await;
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
        let session = session(host, addr);
        let first = infer(&session, full(vec![user("original")])).await;
        // Rewind/rebuild explicitly sends full context, even on a warm socket.
        let second = infer(
            &session,
            full(vec![user("different prefix"), Item::Step(first.carry)]),
        )
        .await;
        let mut changed = delta(
            second
                .continuation
                .clone()
                .map(|id| id.into_token())
                .as_deref()
                .unwrap(),
            vec![user("next")],
        );
        changed.instructions = "changed".into();
        let error = session
            .step(&changed, &mut |_| panic!("must load full context"))
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "needs context");
        changed.previous_response_id = None;
        infer(&session, changed).await;
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
        let session = Arc::new(session(host, addr));
        let running = session.clone();
        let task = tokio::spawn(async move {
            let mut admitted = Some(admitted);
            running
                .step(&request(), &mut |event| {
                    if matches!(event, Event::Code(_)) {
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
        tokio::time::timeout(
            Duration::from_secs(5),
            session.step(&request(), &mut |_| {}),
        )
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
        let mut openai = local_model();
        openai.base_url = format!("http://{addr}").into();
        let connecting = tokio::spawn(async move {
            let mut selected = host.selected.clone();
            let crate::CredentialState::Ready { auth: resolved, .. } = host.snapshot(1).state
            else {
                unreachable!()
            };
            selected.account_id = resolved.account_id.clone();
            let connection = openai
                .connect(CacheKey::from_u128(1), selected, resolved, DialRoute::Dns)
                .await
                .unwrap();
            (openai, connection)
        });
        let (tcp, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let (openai, mut connection) = connecting.await.unwrap();
        tokio::time::pause();
        let start = tokio::time::Instant::now();
        let reading = tokio::spawn(async move {
            let (events, _response) = mpsc::unbounded_channel();
            openai.read_response(&mut connection, &events).await
        });
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
        let session = session(host, addr);
        let answer = infer(&session, full(vec![user("first")])).await;
        let mut request = delta(
            answer
                .continuation
                .clone()
                .map(|id| id.into_token())
                .as_deref()
                .unwrap(),
            vec![user("next")],
        );
        request.cache_key = CacheKey::from_u128(99);
        let error = session.step(&request, &mut |_| {}).await.unwrap_err();
        assert_eq!(error.to_string(), "needs context");
        request.previous_response_id = None;
        let error = session.step(&request, &mut |_| {}).await.unwrap_err();
        assert!(!super::super::is_retryable(&error));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn actor_runs_without_polling_and_skips_cancelled_queued_requests() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (started, start_seen) = oneshot::channel();
        let (finished, finish_seen) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            assert!(envelope(&mut socket).await.to_string().contains("first"));
            started.send(()).unwrap();
            // No completion: dropping the response receiver must close this
            // active socket before another request can run.
            loop {
                match socket.next().await {
                    Some(Ok(WsMessage::Ping(bytes))) => {
                        socket.send(WsMessage::Pong(bytes)).await.unwrap()
                    }
                    Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                    other => panic!("unexpected active message: {other:?}"),
                }
            }
            let (tcp, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let body = envelope(&mut socket).await;
            assert!(body.to_string().contains("third"));
            assert!(!body.to_string().contains("abandoned"));
            assert!(body.get("previous_response_id").is_none());
            complete(&mut socket, "r3", false).await;
            finished.send(()).unwrap();
            // Dropping the last session handle also terminates the idle owner.
            loop {
                match socket.next().await {
                    Some(Ok(WsMessage::Ping(bytes))) => {
                        socket.send(WsMessage::Pong(bytes)).await.unwrap()
                    }
                    Some(Ok(WsMessage::Close(_))) | Some(Err(_)) | None => break,
                    other => panic!("unexpected idle message: {other:?}"),
                }
            }
        });
        let host = Host::new();
        let test = session(host, addr);
        let (selected, auth) = test.policy.select_resolved().await.unwrap();
        let first = test
            .session
            .start(full(vec![user("first")]), selected.clone(), auth.clone());
        // No step future or receiver poll is required to drive the socket.
        tokio::time::timeout(Duration::from_secs(5), start_seen)
            .await
            .unwrap()
            .unwrap();
        let abandoned = test.session.start(
            full(vec![user("abandoned")]),
            selected.clone(),
            auth.clone(),
        );
        drop(abandoned);
        drop(first);
        let mut third = test
            .session
            .start(full(vec![user("third")]), selected, auth);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(5), third.recv()).await.unwrap(),
            Some(Event::Completed(Step {continuation: Some(id), ..})) if id.clone().into_token() == "r3"
        ));
        finish_seen.await.unwrap();
        drop(third);
        drop(test);
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }
}
