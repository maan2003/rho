use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderMap;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::build_request::ResponsesRequest;
use crate::oauth::ResolvedAuth;
use crate::{
    DEFAULT_WEBSOCKET_EVENT_TIMEOUT_SECS, DEFAULT_WEBSOCKET_PING_INTERVAL_SECS,
    DEFAULT_WEBSOCKET_POOL_CHECKOUT_WAIT_MS, DEFAULT_WEBSOCKET_POOL_MAX_CONNECTION_AGE_SECS,
    DEFAULT_WEBSOCKET_POOL_MAX_CONNECTIONS, OPENAI_BETA_WS, ProviderSession, ResponsesUpdate,
    apply_response_event, responses_url,
};

pub(crate) async fn send_websocket(
    session: &ProviderSession,
    websocket_pool: &Arc<Mutex<WebSocketPool>>,
    body: ResponsesRequest,
    tool_names: &BTreeMap<String, String>,
    updates: &mpsc::UnboundedSender<Result<ResponsesUpdate>>,
) -> Result<()> {
    let auth = session.auth.clone();
    let resolved_auth = tokio::task::spawn_blocking(move || auth.resolve()).await??;
    if let Some(key) = WebSocketPoolKey::from_request(session, &body, resolved_auth.as_ref()) {
        return send_pooled_websocket(
            session,
            websocket_pool,
            key,
            resolved_auth.as_ref(),
            body,
            tool_names,
            updates,
        )
        .await;
    }

    let request = build_ws_request(
        session,
        body.prompt_cache_key.as_deref(),
        resolved_auth.as_ref(),
    )?;
    let (socket, _response) = connect_async(request).await?;
    let mut connection = WebSocketConnection::new(socket, resolved_auth.as_ref());
    connection.run_turn(body, tool_names, updates).await
}

async fn send_pooled_websocket(
    session: &ProviderSession,
    websocket_pool: &Arc<Mutex<WebSocketPool>>,
    key: WebSocketPoolKey,
    auth: Option<&ResolvedAuth>,
    body: ResponsesRequest,
    tool_names: &BTreeMap<String, String>,
    updates: &mpsc::UnboundedSender<Result<ResponsesUpdate>>,
) -> Result<()> {
    let mut connection = checkout_websocket_pool(session, websocket_pool, &key, auth).await?;
    match connection.run_turn(body, tool_names, updates).await {
        Ok(()) => {}
        Err(error) => {
            let mut pool = websocket_pool.lock().await;
            pool.release_busy(&key);
            return Err(error);
        }
    };

    let mut pool = websocket_pool.lock().await;
    pool.release(key, connection, DEFAULT_WEBSOCKET_POOL_MAX_CONNECTIONS);
    Ok(())
}

async fn checkout_websocket_pool(
    session: &ProviderSession,
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
                Duration::from_secs(DEFAULT_WEBSOCKET_POOL_MAX_CONNECTION_AGE_SECS),
            )
        };

        match checkout {
            WebSocketPoolCheckout::Ready(connection) => return Ok(*connection),
            WebSocketPoolCheckout::OpenNew => {
                let connection = async {
                    let request = build_ws_request(session, Some(&key.thread_id), auth)?;
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
                    DEFAULT_WEBSOCKET_POOL_CHECKOUT_WAIT_MS,
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
        body: ResponsesRequest,
        tool_names: &BTreeMap<String, String>,
        updates: &mpsc::UnboundedSender<Result<ResponsesUpdate>>,
    ) -> Result<()> {
        self.socket
            .send(WsMessage::Text(
                serde_json::to_string(&WsResponseCreate {
                    ty: "response.create",
                    body,
                })?
                .into(),
            ))
            .await?;

        let mut state = crate::ResponseState::with_tool_names(tool_names.clone());
        let mut completed = false;
        let event_timeout = Duration::from_secs(DEFAULT_WEBSOCKET_EVENT_TIMEOUT_SECS);
        let mut last_event_at = tokio::time::Instant::now();
        let mut ping_interval = (DEFAULT_WEBSOCKET_PING_INTERVAL_SECS > 0).then(|| {
            let interval = Duration::from_secs(DEFAULT_WEBSOCKET_PING_INTERVAL_SECS);
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
                    let (done, event_updates) = apply_response_event(&mut state, &event)?;
                    for update in event_updates {
                        let _ = updates.send(Ok(update));
                    }
                    if done {
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
        let _ = updates.send(Ok(ResponsesUpdate::Finished(response)));
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct WebSocketPoolKey {
    pub(crate) base_url: String,
    pub(crate) account_id: Option<String>,
    pub(crate) thread_id: String,
}

impl WebSocketPoolKey {
    pub(crate) fn from_request(
        session: &ProviderSession,
        body: &ResponsesRequest,
        auth: Option<&ResolvedAuth>,
    ) -> Option<Self> {
        Self::from_thread_id(session, body.prompt_cache_key.clone()?, auth)
    }

    fn from_thread_id(
        session: &ProviderSession,
        thread_id: String,
        auth: Option<&ResolvedAuth>,
    ) -> Option<Self> {
        Some(Self {
            base_url: session.base_url.clone(),
            account_id: auth.and_then(|auth| auth.account_id.clone()),
            thread_id,
        })
    }
}

pub(crate) struct WebSocketPool {
    idle: HashMap<WebSocketPoolKey, WebSocketConnection>,
    lru: VecDeque<WebSocketPoolKey>,
    busy: HashSet<WebSocketPoolKey>,
}

impl WebSocketPool {
    pub(crate) fn new() -> Self {
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

pub(crate) async fn next_ws_message<S>(
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
pub(crate) struct WsResponseCreate {
    #[serde(rename = "type")]
    pub(crate) ty: &'static str,
    #[serde(flatten)]
    pub(crate) body: ResponsesRequest,
}

pub(crate) fn build_ws_request(
    session: &ProviderSession,
    thread_id: Option<&str>,
    auth: Option<&ResolvedAuth>,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>> {
    let url = build_ws_url(session)?;
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

fn build_ws_url(session: &ProviderSession) -> Result<String> {
    let url = responses_url(&session.base_url);
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
