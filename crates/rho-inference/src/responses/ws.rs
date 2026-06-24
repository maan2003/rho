use std::collections::BTreeMap;
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

use super::oauth::ResolvedAuth;
use super::wire::{ResponseState, ResponsesRequest};
use super::{InferenceSession, InferenceUpdate, OPENAI_BETA_WS, responses_url};

const DEFAULT_WEBSOCKET_EVENT_TIMEOUT_SECS: u64 = 120;
const DEFAULT_WEBSOCKET_PING_INTERVAL_SECS: u64 = 25;
/// Margin under the server's ~60-minute hard cap: a connection this old is
/// reopened before sending, so a turn never dies mid-stream from the server
/// closing an aged socket.
const MAX_CONNECTION_AGE: Duration = Duration::from_secs(55 * 60);

/// Run one turn on the session's warm socket, reusing it when still valid and
/// reopening otherwise.
///
/// The session owns a single connection behind an async `Mutex`, so the guard
/// is held for the whole turn: a session runs its turns sequentially, and a
/// rare concurrent turn on the same session serializes here instead of opening
/// a duplicate socket. A failed turn drops the socket so the next turn starts
/// clean.
pub(crate) async fn send_websocket(
    session: &InferenceSession,
    connection: &Arc<Mutex<Option<WebSocketConnection>>>,
    body: ResponsesRequest,
    tool_names: &BTreeMap<String, String>,
    updates: &mpsc::UnboundedSender<Result<InferenceUpdate>>,
) -> Result<()> {
    let auth = session.auth.clone();
    let resolved_auth = tokio::task::spawn_blocking(move || auth.resolve()).await??;

    let mut slot = connection.lock().await;
    // Reuse the warm socket unless OAuth rotated the bearer or it is nearing the
    // server's age cap; otherwise open a fresh one in place.
    let reusable = slot.as_ref().is_some_and(|connection| {
        connection.bearer_token == resolved_auth.bearer_token
            && connection.opened_at.elapsed() < MAX_CONNECTION_AGE
    });
    if !reusable {
        let request = build_ws_request(session, body.prompt_cache_key.as_deref(), &resolved_auth)?;
        let (socket, _response) = connect_async(request).await?;
        *slot = Some(WebSocketConnection::new(socket, &resolved_auth));
    }

    let connection = slot.as_mut().expect("connection present after refresh");
    match connection.run_turn(body, tool_names, updates).await {
        Ok(()) => Ok(()),
        Err(error) => {
            // The socket is in an unknown state after a failed turn; drop it so
            // the next turn reconnects.
            *slot = None;
            Err(error)
        }
    }
}

pub(crate) struct WebSocketConnection {
    socket: WebSocket,
    opened_at: tokio::time::Instant,
    bearer_token: String,
}

type WebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

impl WebSocketConnection {
    fn new(socket: WebSocket, auth: &ResolvedAuth) -> Self {
        Self {
            socket,
            opened_at: tokio::time::Instant::now(),
            bearer_token: auth.bearer_token.clone(),
        }
    }

    async fn run_turn(
        &mut self,
        body: ResponsesRequest,
        tool_names: &BTreeMap<String, String>,
        updates: &mpsc::UnboundedSender<Result<InferenceUpdate>>,
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

        let mut state = ResponseState::with_tool_names(tool_names.clone());
        let mut completed = false;
        let event_timeout = Duration::from_secs(DEFAULT_WEBSOCKET_EVENT_TIMEOUT_SECS);
        let mut last_event_at = tokio::time::Instant::now();
        let ping = Duration::from_secs(DEFAULT_WEBSOCKET_PING_INTERVAL_SECS);
        let mut ping_interval = tokio::time::interval_at(tokio::time::Instant::now() + ping, ping);
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
                    let (done, event_updates) = state.apply_event(&event)?;
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
        let _ = updates.send(Ok(InferenceUpdate::Finished(response)));
        Ok(())
    }
}

pub(crate) async fn next_ws_message<S>(
    socket: &mut S,
    event_timeout: Duration,
    last_event_at: &mut tokio::time::Instant,
    ping_interval: &mut tokio::time::Interval,
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
    session: &InferenceSession,
    thread_id: Option<&str>,
    auth: &ResolvedAuth,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>> {
    let url = build_ws_url(session)?;
    let mut request = url.into_client_request()?;
    set_header(request.headers_mut(), "OpenAI-Beta", OPENAI_BETA_WS)?;
    set_header(
        request.headers_mut(),
        "Authorization",
        &format!("Bearer {}", auth.bearer_token),
    )?;
    if let Some(thread_id) = thread_id {
        set_header(request.headers_mut(), "session-id", thread_id)?;
        set_header(request.headers_mut(), "thread-id", thread_id)?;
    }
    if let Some(account_id) = auth.account_id.as_deref() {
        set_header(request.headers_mut(), "chatgpt-account-id", account_id)?;
    }
    Ok(request)
}

fn build_ws_url(session: &InferenceSession) -> Result<String> {
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
