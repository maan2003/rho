use std::time::Duration;

use anyhow::{Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderMap;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use super::oauth::ResolvedAuth;
use super::wire::ResponsesRequest;
use super::{InferenceSession, OPENAI_BETA_WS, responses_url};

/// How long an active turn may go without any provider event before we treat
/// the socket as wedged and fail the turn. Not applied while idle.
pub(crate) const EVENT_TIMEOUT: Duration = Duration::from_secs(120);
const PING_INTERVAL: Duration = Duration::from_secs(25);
/// Margin under the server's ~60-minute hard cap: a connection this old is
/// reopened before sending, so a turn never dies mid-stream from the server
/// closing an aged socket.
pub(crate) const MAX_CONNECTION_AGE: Duration = Duration::from_secs(55 * 60);

type WebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One live WebSocket to the Responses endpoint, kept warm across turns. The
/// keepalive ping timer and the last-event clock live here so they persist
/// across the many short `run` calls that drive a single turn.
pub(crate) struct WebSocketConnection {
    socket: WebSocket,
    pub(crate) opened_at: tokio::time::Instant,
    pub(crate) bearer_token: String,
    pub(crate) client_secret: [u8; 8],
    ping_interval: tokio::time::Interval,
    last_event_at: tokio::time::Instant,
}

impl WebSocketConnection {
    pub(crate) fn new(socket: WebSocket, auth: &ResolvedAuth) -> Self {
        let now = tokio::time::Instant::now();
        Self {
            socket,
            opened_at: now,
            bearer_token: auth.bearer_token.clone(),
            client_secret: auth.client_secret,
            ping_interval: tokio::time::interval_at(now + PING_INTERVAL, PING_INTERVAL),
            last_event_at: now,
        }
    }

    /// Read the next frame, sending a keepalive ping if the ping timer fires
    /// first. `event_timeout` bounds the wait only while a turn is active;
    /// pass `None` when idle so a quiet socket is not treated as wedged.
    pub(crate) async fn next_message(
        &mut self,
        event_timeout: Option<Duration>,
    ) -> Result<Option<WsMessage>> {
        next_ws_message(
            &mut self.socket,
            event_timeout,
            &mut self.last_event_at,
            &mut self.ping_interval,
        )
        .await
    }

    /// Send a `response.create` envelope and reset the event clock so the first
    /// event of the new turn is timed from now, not from the previous turn.
    pub(crate) async fn send_envelope(&mut self, body: ResponsesRequest) -> Result<()> {
        let text = serde_json::to_string(&WsResponseCreate {
            ty: "response.create",
            body,
        })?;
        self.socket.send(WsMessage::Text(text.into())).await?;
        self.last_event_at = tokio::time::Instant::now();
        Ok(())
    }

    pub(crate) async fn pong(
        &mut self,
        payload: tokio_tungstenite::tungstenite::Bytes,
    ) -> Result<()> {
        self.socket.send(WsMessage::Pong(payload)).await?;
        Ok(())
    }
}

pub(crate) async fn next_ws_message<S>(
    socket: &mut S,
    event_timeout: Option<Duration>,
    last_event_at: &mut tokio::time::Instant,
    ping_interval: &mut tokio::time::Interval,
) -> Result<Option<WsMessage>>
where
    S: futures_util::Stream<
            Item = std::result::Result<WsMessage, tokio_tungstenite::tungstenite::Error>,
        > + futures_util::Sink<WsMessage, Error = tokio_tungstenite::tungstenite::Error>
        + Unpin,
{
    loop {
        let deadline = event_timeout.map(|timeout| *last_event_at + timeout);
        let timeout_sleep = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending().await,
            }
        };
        tokio::pin!(timeout_sleep);
        tokio::select! {
            _ = &mut timeout_sleep => {
                let secs = event_timeout.map(|t| t.as_secs()).unwrap_or_default();
                bail!("stream error: ws turn produced no events for {secs}s");
            }
            _ = ping_interval.tick() => {
                socket.send(WsMessage::Ping(Vec::new().into())).await?;
            }
            message = socket.next() => {
                if let Some(Ok(WsMessage::Text(_))) = message.as_ref() {
                    *last_event_at = tokio::time::Instant::now();
                }
                return Ok(message.transpose()?);
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
