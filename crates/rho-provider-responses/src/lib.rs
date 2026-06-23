//! OpenAI Responses API provider building blocks.
//!
//! The request-body shape and tool-name encoding are adapted from Tau's
//! Responses backend. Tau's protocol messages, event bus, VCR, WebSocket pool,
//! and HTTP loop are intentionally not copied into this crate; `rho-agent` or a
//! fork should own those runtime policies.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use futures::future::{BoxFuture, FutureExt};
use futures::stream::{self, BoxStream};
use futures_util::{SinkExt, StreamExt};
use rho::{
    ItemKind, Message, MessagePhase, ProviderItem, ProviderItemKind, ProviderRequest,
    ProviderResponse, ReasoningText, ReasoningTextKind, Role, TokenUsage, ToolCall, ToolCallId,
    ToolSpec, ToolType,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderMap;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

mod build_request;
pub mod oauth;
pub mod session;

use oauth::ResolvedAuth;
pub use oauth::{OAuthFile, ResponsesAuth, ResponsesOAuthCredentials};
pub use session::{
    ProviderSession, ReasoningEffort, ReasoningSummary, ServiceTier, ToolChoice, Verbosity,
};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api";
pub const DEFAULT_MODEL: &str = "gpt-5";
pub const DEFAULT_CONTEXT_WINDOW: u64 = 258_400;
pub const DEFAULT_WEBSOCKET_EVENT_TIMEOUT_SECS: u64 = 120;
pub const DEFAULT_WEBSOCKET_PING_INTERVAL_SECS: u64 = 25;
pub const DEFAULT_WEBSOCKET_POOL_MAX_CONNECTIONS: usize = 10;
pub const DEFAULT_WEBSOCKET_POOL_MAX_CONNECTION_AGE_SECS: u64 = 55 * 60;
pub const DEFAULT_WEBSOCKET_POOL_CHECKOUT_WAIT_MS: u64 = 50;
pub const OPENAI_BETA_WS: &str = "responses_websockets=2026-02-06";
pub const CHATGPT_CODEX_MODELS: &[&str] = &["gpt-5.5", "gpt-5.4", "gpt-5.4-mini", "gpt-5.3-codex"];

pub type ResponsesFuture = BoxFuture<'static, Result<ProviderResponse>>;
pub type ResponsesStream = BoxStream<'static, Result<ResponsesUpdate>>;
pub type ResponsesUpdateCallback = Box<dyn FnMut(ResponsesUpdate) + Send>;

#[derive(Clone)]
pub struct ResponsesProvider {
    config: ResponsesConfig,
    websocket_pool: Arc<Mutex<WebSocketPool>>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponsesSurface {
    OpenAi,
    ChatGptCodex,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsesConfig {
    #[serde(default = "default_surface")]
    pub surface: ResponsesSurface,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub auth: ResponsesAuth,
    #[serde(default = "default_context_window")]
    pub context_window: u64,
    #[serde(default)]
    pub supports_reasoning_effort: bool,
    #[serde(default)]
    pub supports_reasoning_summary: bool,
    #[serde(default)]
    pub supports_verbosity: bool,
    #[serde(default)]
    pub supports_phase: bool,
    #[serde(default)]
    pub supports_prompt_cache_key: bool,
    #[serde(default)]
    pub supports_encrypted_reasoning: bool,
    #[serde(default)]
    pub supports_compaction: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<ResponsesCompaction>,
    #[serde(default = "default_websocket_event_timeout_secs")]
    pub websocket_event_timeout_secs: u64,
    #[serde(default = "default_websocket_ping_interval_secs")]
    pub websocket_ping_interval_secs: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub websocket_pool_max_connections: usize,
    #[serde(default = "default_websocket_pool_max_connection_age_secs")]
    pub websocket_pool_max_connection_age_secs: u64,
    #[serde(default = "default_websocket_pool_checkout_wait_ms")]
    pub websocket_pool_checkout_wait_ms: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_body: BTreeMap<String, Value>,
}

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
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<TextRequest>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_cache_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context_management: Vec<ContextManagementRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_response_id: Option<String>,
    #[serde(flatten)]
    pub extra_body: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ReasoningRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<ReasoningContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) enum ReasoningContext {
    #[serde(rename = "all_turns")]
    AllTurns,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct TextRequest {
    pub verbosity: &'static str,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsesCompaction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_threshold: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct ContextManagementRequest {
    #[serde(rename = "type")]
    pub ty: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compact_threshold: Option<u64>,
}

impl Default for ResponsesConfig {
    fn default() -> Self {
        Self {
            surface: default_surface(),
            base_url: default_base_url(),
            auth: ResponsesAuth::None,
            context_window: DEFAULT_CONTEXT_WINDOW,
            supports_reasoning_effort: false,
            supports_reasoning_summary: false,
            supports_verbosity: false,
            supports_phase: false,
            supports_prompt_cache_key: false,
            supports_encrypted_reasoning: false,
            supports_compaction: false,
            compaction: None,
            websocket_event_timeout_secs: DEFAULT_WEBSOCKET_EVENT_TIMEOUT_SECS,
            websocket_ping_interval_secs: DEFAULT_WEBSOCKET_PING_INTERVAL_SECS,
            websocket_pool_max_connections: 0,
            websocket_pool_max_connection_age_secs: DEFAULT_WEBSOCKET_POOL_MAX_CONNECTION_AGE_SECS,
            websocket_pool_checkout_wait_ms: DEFAULT_WEBSOCKET_POOL_CHECKOUT_WAIT_MS,
            extra_body: BTreeMap::new(),
        }
    }
}

impl ResponsesConfig {
    pub fn chatgpt_codex(auth: ResponsesAuth) -> Self {
        Self {
            surface: ResponsesSurface::ChatGptCodex,
            base_url: DEFAULT_CHATGPT_BASE_URL.to_owned(),
            auth,
            context_window: DEFAULT_CONTEXT_WINDOW,
            supports_reasoning_effort: true,
            supports_reasoning_summary: true,
            supports_verbosity: true,
            supports_phase: true,
            supports_prompt_cache_key: true,
            supports_encrypted_reasoning: true,
            supports_compaction: true,
            compaction: None,
            websocket_event_timeout_secs: DEFAULT_WEBSOCKET_EVENT_TIMEOUT_SECS,
            websocket_ping_interval_secs: DEFAULT_WEBSOCKET_PING_INTERVAL_SECS,
            websocket_pool_max_connections: DEFAULT_WEBSOCKET_POOL_MAX_CONNECTIONS,
            websocket_pool_max_connection_age_secs: DEFAULT_WEBSOCKET_POOL_MAX_CONNECTION_AGE_SECS,
            websocket_pool_checkout_wait_ms: DEFAULT_WEBSOCKET_POOL_CHECKOUT_WAIT_MS,
            extra_body: BTreeMap::new(),
        }
    }

    pub fn chatgpt_codex_models() -> &'static [&'static str] {
        CHATGPT_CODEX_MODELS
    }

    pub fn with_compaction(mut self, compaction: ResponsesCompaction) -> Self {
        self.compaction = Some(compaction);
        self
    }
}

impl ResponsesSurface {
    pub fn responses_url(&self, base_url: &str) -> String {
        let base = base_url.trim_end_matches('/');
        match self {
            Self::OpenAi => format!("{base}/responses"),
            Self::ChatGptCodex => format!("{base}/codex/responses"),
        }
    }

    fn store_value(&self) -> bool {
        match self {
            Self::OpenAi => true,
            Self::ChatGptCodex => false,
        }
    }
}

impl ResponsesProvider {
    pub fn new(config: ResponsesConfig) -> Self {
        Self {
            config,
            websocket_pool: Arc::new(Mutex::new(WebSocketPool::new())),
        }
    }

    pub fn config(&self) -> &ResponsesConfig {
        &self.config
    }

    pub fn complete(&self, session: &ProviderSession, request: ProviderRequest) -> ResponsesFuture {
        self.complete_streaming(session, request, |_| {})
    }

    pub fn stream(&self, session: &ProviderSession, request: ProviderRequest) -> ResponsesStream {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let error_sender = sender.clone();
        let future = self.complete_streaming(session, request, move |update| {
            let _ = sender.send(Ok(update));
        });
        tokio::spawn(async move {
            if let Err(error) = future.await {
                let _ = error_sender.send(Err(error));
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
        let config = self.config.clone();
        let websocket_pool = Arc::clone(&self.websocket_pool);
        let prompt_cache_key = prompt_cache_key.into();
        async move {
            let auth = config.auth.clone();
            let resolved_auth = tokio::task::spawn_blocking(move || auth.resolve()).await??;
            let Some(key) =
                WebSocketPoolKey::from_thread_id(&config, prompt_cache_key, resolved_auth.as_ref())
            else {
                return Ok(false);
            };

            let connection =
                checkout_websocket_pool(&config, &websocket_pool, &key, resolved_auth.as_ref())
                    .await?;
            let mut pool = websocket_pool.lock().await;
            pool.release(key, connection, config.websocket_pool_max_connections);
            Ok(true)
        }
        .boxed()
    }

    pub fn complete_streaming(
        &self,
        session: &ProviderSession,
        request: ProviderRequest,
        on_update: impl FnMut(ResponsesUpdate) + Send + 'static,
    ) -> ResponsesFuture {
        let config = self.config.clone();
        let session = session.clone();
        let websocket_pool = Arc::clone(&self.websocket_pool);
        async move {
            let tool_names = tool_name_map(&request.tools);
            let responses_request =
                ResponsesRequest::from_provider_request(&config, &session, request.clone());
            let mut on_update: ResponsesUpdateCallback = Box::new(on_update);
            match send_websocket(
                &config,
                &websocket_pool,
                responses_request,
                &tool_names,
                &mut on_update,
            )
            .await
            {
                Ok(response) => Ok(response),
                Err(error) => {
                    if let Some(replay_request) =
                        stale_previous_response_replay_request(&config, &session, &request, &error)
                    {
                        send_websocket(
                            &config,
                            &websocket_pool,
                            replay_request,
                            &tool_names,
                            &mut on_update,
                        )
                        .await
                    } else {
                        Err(error)
                    }
                }
            }
        }
        .boxed()
    }
}

pub fn parse_response_events(
    events: impl IntoIterator<Item = impl AsRef<str>>,
) -> Result<ProviderResponse> {
    let mut state = ResponseState::default();
    let mut completed = false;
    for event in events {
        if apply_response_event_str(&mut state, event.as_ref(), &mut |_| {})? {
            completed = true;
            break;
        }
    }
    if !completed {
        bail!("response stream ended before response.completed");
    }
    Ok(state.finish())
}

pub fn parse_response_events_with_updates(
    events: impl IntoIterator<Item = impl AsRef<str>>,
    mut on_update: impl FnMut(ResponsesUpdate),
) -> Result<ProviderResponse> {
    let mut state = ResponseState::default();
    let mut completed = false;
    for event in events {
        if apply_response_event_str(&mut state, event.as_ref(), &mut on_update)? {
            completed = true;
            break;
        }
    }
    if !completed {
        bail!("response stream ended before response.completed");
    }
    let response = state.finish();
    on_update(ResponsesUpdate::Finished(response.clone()));
    Ok(response)
}

async fn send_websocket(
    config: &ResponsesConfig,
    websocket_pool: &Arc<Mutex<WebSocketPool>>,
    body: ResponsesRequest,
    tool_names: &BTreeMap<String, String>,
    on_update: &mut (dyn FnMut(ResponsesUpdate) + Send),
) -> Result<ProviderResponse> {
    let auth = config.auth.clone();
    let resolved_auth = tokio::task::spawn_blocking(move || auth.resolve()).await??;
    if let Some(key) = WebSocketPoolKey::from_request(config, &body, resolved_auth.as_ref()) {
        return send_pooled_websocket(
            config,
            websocket_pool,
            key,
            resolved_auth.as_ref(),
            body,
            tool_names,
            on_update,
        )
        .await;
    }

    let request = build_ws_request(
        config,
        body.prompt_cache_key.as_deref(),
        resolved_auth.as_ref(),
    )?;
    let (socket, _response) = connect_async(request).await?;
    let mut connection = WebSocketConnection::new(socket, resolved_auth.as_ref());
    connection
        .run_turn(config, body, tool_names, on_update)
        .await
}

async fn send_pooled_websocket(
    config: &ResponsesConfig,
    websocket_pool: &Arc<Mutex<WebSocketPool>>,
    key: WebSocketPoolKey,
    auth: Option<&ResolvedAuth>,
    body: ResponsesRequest,
    tool_names: &BTreeMap<String, String>,
    on_update: &mut (dyn FnMut(ResponsesUpdate) + Send),
) -> Result<ProviderResponse> {
    let mut connection = checkout_websocket_pool(config, websocket_pool, &key, auth).await?;
    let response = match connection
        .run_turn(config, body, tool_names, on_update)
        .await
    {
        Ok(response) => response,
        Err(error) => {
            let mut pool = websocket_pool.lock().await;
            pool.release_busy(&key);
            return Err(error);
        }
    };

    let mut pool = websocket_pool.lock().await;
    pool.release(key, connection, config.websocket_pool_max_connections);
    Ok(response)
}

async fn checkout_websocket_pool(
    config: &ResponsesConfig,
    websocket_pool: &Arc<Mutex<WebSocketPool>>,
    key: &WebSocketPoolKey,
    auth: Option<&ResolvedAuth>,
) -> Result<WebSocketConnection> {
    loop {
        let checkout = {
            let mut pool = websocket_pool.lock().await;
            pool.checkout(
                key,
                auth,
                Duration::from_secs(config.websocket_pool_max_connection_age_secs),
            )
        };

        match checkout {
            WebSocketPoolCheckout::Ready(connection) => return Ok(*connection),
            WebSocketPoolCheckout::OpenNew => {
                let connection = async {
                    let request = build_ws_request(config, Some(&key.thread_id), auth)?;
                    let (socket, _response) = connect_async(request).await?;
                    Ok::<_, anyhow::Error>(WebSocketConnection::new(socket, auth))
                }
                .await;
                match connection {
                    Ok(connection) => return Ok(connection),
                    Err(error) => {
                        let mut pool = websocket_pool.lock().await;
                        pool.release_busy(key);
                        return Err(error);
                    }
                }
            }
            WebSocketPoolCheckout::Busy => {
                tokio::time::sleep(Duration::from_millis(
                    config.websocket_pool_checkout_wait_ms,
                ))
                .await;
            }
        }
    }
}

struct WebSocketConnection {
    socket: WebSocket,
    opened_at: tokio::time::Instant,
    bearer_token: Option<String>,
}

type WebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

impl WebSocketConnection {
    fn new(socket: WebSocket, auth: Option<&ResolvedAuth>) -> Self {
        Self {
            socket,
            opened_at: tokio::time::Instant::now(),
            bearer_token: auth.map(|auth| auth.bearer_token.clone()),
        }
    }

    async fn run_turn(
        &mut self,
        config: &ResponsesConfig,
        body: ResponsesRequest,
        tool_names: &BTreeMap<String, String>,
        on_update: &mut (dyn FnMut(ResponsesUpdate) + Send),
    ) -> Result<ProviderResponse> {
        self.socket
            .send(WsMessage::Text(
                serde_json::to_string(&WsResponseCreate {
                    ty: "response.create",
                    body,
                })?
                .into(),
            ))
            .await?;

        let mut state = ResponseState::with_tool_names(tool_names.clone());
        let mut completed = false;
        let event_timeout = Duration::from_secs(config.websocket_event_timeout_secs);
        let mut last_event_at = tokio::time::Instant::now();
        let mut ping_interval = (config.websocket_ping_interval_secs > 0).then(|| {
            let interval = Duration::from_secs(config.websocket_ping_interval_secs);
            tokio::time::interval_at(tokio::time::Instant::now() + interval, interval)
        });
        while let Some(message) = next_ws_message(
            &mut self.socket,
            event_timeout,
            &mut last_event_at,
            &mut ping_interval,
        )
        .await?
        {
            match message? {
                WsMessage::Text(text) => {
                    let event: Value = match serde_json::from_str(text.as_ref()) {
                        Ok(event) => event,
                        Err(_) => continue,
                    };
                    if apply_response_event(&mut state, &event, on_update)? {
                        completed = true;
                        break;
                    }
                }
                WsMessage::Close(frame) => {
                    bail!(
                        "stream error: websocket closed mid-stream ({})",
                        frame
                            .map(|frame| format!("code={} reason={}", frame.code, frame.reason))
                            .unwrap_or_else(|| "no close frame".to_owned())
                    );
                }
                WsMessage::Ping(payload) => {
                    self.socket.send(WsMessage::Pong(payload)).await?;
                }
                WsMessage::Binary(_) | WsMessage::Pong(_) | WsMessage::Frame(_) => {}
            }
        }

        if !completed {
            bail!("stream error: websocket ended before response.completed");
        }
        let response = state.finish();
        on_update(ResponsesUpdate::Finished(response.clone()));
        Ok(response)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WebSocketPoolKey {
    surface: ResponsesSurface,
    base_url: String,
    account_id: Option<String>,
    thread_id: String,
}

impl WebSocketPoolKey {
    fn from_request(
        config: &ResponsesConfig,
        body: &ResponsesRequest,
        auth: Option<&ResolvedAuth>,
    ) -> Option<Self> {
        Self::from_thread_id(config, body.prompt_cache_key.clone()?, auth)
    }

    fn from_thread_id(
        config: &ResponsesConfig,
        thread_id: String,
        auth: Option<&ResolvedAuth>,
    ) -> Option<Self> {
        if config.websocket_pool_max_connections == 0
            || config.surface != ResponsesSurface::ChatGptCodex
        {
            return None;
        }
        Some(Self {
            surface: config.surface.clone(),
            base_url: config.base_url.clone(),
            account_id: auth.and_then(|auth| auth.account_id.clone()),
            thread_id,
        })
    }
}

struct WebSocketPool {
    idle: HashMap<WebSocketPoolKey, WebSocketConnection>,
    lru: VecDeque<WebSocketPoolKey>,
    busy: HashSet<WebSocketPoolKey>,
}

impl WebSocketPool {
    fn new() -> Self {
        Self {
            idle: HashMap::new(),
            lru: VecDeque::new(),
            busy: HashSet::new(),
        }
    }

    fn checkout(
        &mut self,
        key: &WebSocketPoolKey,
        auth: Option<&ResolvedAuth>,
        max_age: Duration,
    ) -> WebSocketPoolCheckout {
        if self.busy.contains(key) {
            return WebSocketPoolCheckout::Busy;
        }
        self.busy.insert(key.clone());

        let Some(connection) = self.idle.remove(key) else {
            self.remove_lru_key(key);
            return WebSocketPoolCheckout::OpenNew;
        };
        self.remove_lru_key(key);

        if connection.bearer_token != auth.map(|auth| auth.bearer_token.clone())
            || connection.opened_at.elapsed() >= max_age
        {
            return WebSocketPoolCheckout::OpenNew;
        }

        WebSocketPoolCheckout::Ready(Box::new(connection))
    }

    fn release(
        &mut self,
        key: WebSocketPoolKey,
        connection: WebSocketConnection,
        max_connections: usize,
    ) {
        self.busy.remove(&key);
        self.remove_lru_key(&key);
        self.idle.insert(key.clone(), connection);
        self.lru.push_back(key);
        self.enforce_limit(max_connections);
    }

    fn release_busy(&mut self, key: &WebSocketPoolKey) {
        self.busy.remove(key);
    }

    fn remove_lru_key(&mut self, key: &WebSocketPoolKey) {
        self.lru.retain(|candidate| candidate != key);
    }

    fn enforce_limit(&mut self, max_connections: usize) {
        while self.idle.len() > max_connections {
            let Some(oldest) = self.lru.pop_front() else {
                break;
            };
            self.idle.remove(&oldest);
        }
    }
}

enum WebSocketPoolCheckout {
    Ready(Box<WebSocketConnection>),
    OpenNew,
    Busy,
}

async fn next_ws_message<S>(
    socket: &mut S,
    event_timeout: Duration,
    last_event_at: &mut tokio::time::Instant,
    ping_interval: &mut Option<tokio::time::Interval>,
) -> Result<Option<S::Item>>
where
    S: futures_util::Stream<
            Item = std::result::Result<WsMessage, tokio_tungstenite::tungstenite::Error>,
        > + futures_util::Sink<WsMessage, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    loop {
        let timeout_secs = event_timeout.as_secs();
        let deadline = *last_event_at + event_timeout;
        let timeout_sleep = tokio::time::sleep_until(deadline);
        tokio::pin!(timeout_sleep);
        if let Some(ping_interval) = ping_interval.as_mut() {
            tokio::select! {
                _ = &mut timeout_sleep => {
                    bail!("stream error: ws turn produced no events for {timeout_secs}s");
                }
                _ = ping_interval.tick() => {
                    socket.send(WsMessage::Ping(Vec::new().into())).await?;
                }
                message = socket.next() => {
                    if let Some(Ok(WsMessage::Text(_))) = message.as_ref() {
                        *last_event_at = tokio::time::Instant::now();
                    }
                    return Ok(message);
                }
            }
        } else {
            tokio::select! {
                _ = &mut timeout_sleep => {
                    bail!("stream error: ws turn produced no events for {timeout_secs}s");
                }
                message = socket.next() => {
                    if let Some(Ok(WsMessage::Text(_))) = message.as_ref() {
                        *last_event_at = tokio::time::Instant::now();
                    }
                    return Ok(message);
                }
            }
        }
    }
}

#[derive(Serialize)]
struct WsResponseCreate {
    #[serde(rename = "type")]
    ty: &'static str,
    #[serde(flatten)]
    body: ResponsesRequest,
}

fn build_ws_request(
    config: &ResponsesConfig,
    thread_id: Option<&str>,
    auth: Option<&ResolvedAuth>,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>> {
    let url = build_ws_url(config)?;
    let mut request = url.into_client_request()?;
    set_header(request.headers_mut(), "OpenAI-Beta", OPENAI_BETA_WS)?;
    if let Some(auth) = auth {
        set_header(
            request.headers_mut(),
            "Authorization",
            &format!("Bearer {}", auth.bearer_token),
        )?;
    }
    if let Some(thread_id) = thread_id {
        set_header(request.headers_mut(), "session-id", thread_id)?;
        set_header(request.headers_mut(), "thread-id", thread_id)?;
    }
    if let Some(account_id) = auth.and_then(|auth| auth.account_id.as_deref()) {
        set_header(request.headers_mut(), "chatgpt-account-id", account_id)?;
    }
    Ok(request)
}

fn build_ws_url(config: &ResponsesConfig) -> Result<String> {
    let url = config.surface.responses_url(&config.base_url);
    if let Some(rest) = url.strip_prefix("https://") {
        Ok(format!("wss://{rest}"))
    } else if let Some(rest) = url.strip_prefix("http://") {
        Ok(format!("ws://{rest}"))
    } else {
        bail!("websocket base_url must start with http:// or https://")
    }
}

fn set_header(headers: &mut HeaderMap, name: &'static str, value: &str) -> Result<()> {
    headers.insert(name, value.parse()?);
    Ok(())
}

#[derive(Default)]
struct ResponseState {
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
    fn with_tool_names(tool_names_by_wire: BTreeMap<String, String>) -> Self {
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

    fn finish(self) -> ProviderResponse {
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
    on_update: &mut impl FnMut(ResponsesUpdate),
) -> Result<bool> {
    let data = data.trim_end();
    if data == "[DONE]" {
        return Ok(true);
    }
    let event: Value = match serde_json::from_str(data) {
        Ok(event) => event,
        Err(_) => return Ok(false),
    };
    apply_response_event(state, &event, on_update)
}

fn apply_response_event(
    state: &mut ResponseState,
    event: &Value,
    on_update: &mut (impl FnMut(ResponsesUpdate) + ?Sized),
) -> Result<bool> {
    match event["type"].as_str().unwrap_or_default() {
        "response.output_text.delta" => {
            if let Some(delta) = event["delta"].as_str() {
                let output_index = event["output_index"].as_u64().unwrap_or(0) as usize;
                state
                    .message_text_by_output_index
                    .entry(output_index)
                    .or_default()
                    .push_str(delta);
                on_update(ResponsesUpdate::TextDelta {
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
                on_update(ResponsesUpdate::OutputItem {
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
                on_update(ResponsesUpdate::ReasoningTextDelta {
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
                        on_update(ResponsesUpdate::ToolCall { output_index, call });
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
                    on_update(ResponsesUpdate::OutputItem {
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
                    on_update(ResponsesUpdate::OutputItem {
                        output_index,
                        item: ItemKind::ProviderItem(provider_item),
                    });
                }
                if item["type"].as_str() == Some("compaction") {
                    if event["type"].as_str() == Some("response.output_item.added") {
                        on_update(ResponsesUpdate::CompactionStarted { output_index });
                    } else if event["type"].as_str() == Some("response.output_item.done") {
                        let provider_item = ProviderItem {
                            kind: ProviderItemKind::Compaction,
                            payload: item.clone(),
                        };
                        state
                            .items_by_output_index
                            .insert(output_index, ItemKind::ProviderItem(provider_item.clone()));
                        on_update(ResponsesUpdate::OutputItem {
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
                    on_update(ResponsesUpdate::OutputItem {
                        output_index,
                        item: ItemKind::ProviderItem(provider_item),
                    });
                }
            }
        }
        "response.completed" | "response.done" => {
            state.usage = usage_from_event(event);
            if let Some(usage) = state.usage.clone() {
                on_update(ResponsesUpdate::Usage(usage));
            }
            state.provider_response_id = event
                .get("response")
                .and_then(|response| response["id"].as_str())
                .or_else(|| event["id"].as_str())
                .map(str::to_owned);
            if let Some(response_id) = state.provider_response_id.clone() {
                on_update(ResponsesUpdate::ResponseId(response_id));
            }
            return Ok(true);
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

    Ok(false)
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
    config: &ResponsesConfig,
    session: &ProviderSession,
    request: &ProviderRequest,
    error: &anyhow::Error,
) -> Option<ResponsesRequest> {
    if !is_stale_previous_response_error(error) {
        return None;
    }

    let sliced = ResponsesRequest::from_provider_request(config, session, request.clone());
    sliced.previous_response_id.as_ref()?;

    Some(ResponsesRequest::from_provider_request_full_replay(
        config,
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

fn default_surface() -> ResponsesSurface {
    ResponsesSurface::OpenAi
}

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_owned()
}

const fn default_context_window() -> u64 {
    DEFAULT_CONTEXT_WINDOW
}

const fn default_websocket_event_timeout_secs() -> u64 {
    DEFAULT_WEBSOCKET_EVENT_TIMEOUT_SECS
}

const fn default_websocket_ping_interval_secs() -> u64 {
    DEFAULT_WEBSOCKET_PING_INTERVAL_SECS
}

const fn default_websocket_pool_max_connection_age_secs() -> u64 {
    DEFAULT_WEBSOCKET_POOL_MAX_CONNECTION_AGE_SECS
}

const fn default_websocket_pool_checkout_wait_ms() -> u64 {
    DEFAULT_WEBSOCKET_POOL_CHECKOUT_WAIT_MS
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

#[cfg(test)]
mod tests;
