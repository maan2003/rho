//! One live realtime voice WebSocket session.
//!
//! Owns connect/auth, keepalive pings, and frame-to-event decoding. Policy —
//! what to do with events, reconnection with `conversation_id` resumption,
//! tool dispatch — belongs to the caller (the daemon's voice controller).

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderMap;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::auth::VoiceAuth;
use crate::wire::{ClientEvent, ServerEvent, encode_audio_append, parse_server_event};

const PING_INTERVAL: Duration = Duration::from_secs(25);

#[derive(Clone, Debug)]
pub struct VoiceConfig {
    /// `https://api.x.ai` in production; overridable for tests/proxies.
    pub base_url: String,
    pub model: String,
    pub auth: VoiceAuth,
    /// Resume a previous conversation's cached turns after a reconnect.
    pub conversation_id: Option<String>,
}

impl VoiceConfig {
    pub fn new(auth: VoiceAuth) -> Self {
        Self {
            base_url: "https://api.x.ai".to_owned(),
            model: "grok-voice-think-fast-1.0".to_owned(),
            auth,
            conversation_id: None,
        }
    }

    /// Uses the Grok CLI OAuth login (`~/.grok/auth.json`) as the seed for
    /// rho's local voice auth file.
    pub fn grok_cli() -> std::io::Result<Self> {
        Ok(Self::new(VoiceAuth::grok_cli()?))
    }
}

pub struct VoiceSession {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    ping_interval: tokio::time::Interval,
}

impl VoiceSession {
    pub async fn connect(config: &VoiceConfig) -> Result<Self> {
        let url = build_ws_url(config)?;
        let auth = config.auth.clone();
        let resolved_auth = tokio::task::spawn_blocking(move || auth.resolve())
            .await
            .context("join voice OAuth resolver")?
            .context("resolve Grok OAuth credentials")?;
        let mut request = url.into_client_request()?;
        set_header(
            request.headers_mut(),
            "Authorization",
            &format!("Bearer {}", resolved_auth.bearer_token),
        )?;
        // The public realtime endpoint accepts the OAuth bearer directly; the
        // Grok CLI and cli-chat-proxy also send this marker, and it is harmless
        // for the realtime path.
        set_header(request.headers_mut(), "X-XAI-Token-Auth", "xai-grok-cli")?;
        if let Some(version) = resolved_auth.grok_client_version.as_deref() {
            set_header(request.headers_mut(), "x-grok-client-version", version)?;
        }
        let (socket, _response) = connect_async(request)
            .await
            .context("connect realtime voice websocket")?;
        let now = tokio::time::Instant::now();
        Ok(Self {
            socket,
            ping_interval: tokio::time::interval_at(now + PING_INTERVAL, PING_INTERVAL),
        })
    }

    pub async fn send(&mut self, event: &ClientEvent) -> Result<()> {
        let text = serde_json::to_string(event).context("serialize client event")?;
        self.socket
            .send(WsMessage::Text(text.into()))
            .await
            .context("send client event")?;
        Ok(())
    }

    /// Base64-encodes and streams one chunk of PCM16 microphone audio.
    pub async fn send_audio(&mut self, pcm: &[u8]) -> Result<()> {
        self.send(&encode_audio_append(pcm)).await
    }

    /// Next decoded server event; `None` when the server closed the socket.
    /// `timeout` bounds the wait — pass `None` only when another mechanism
    /// (like a caller-side select) bounds it instead.
    pub async fn next_event(&mut self, timeout: Option<Duration>) -> Result<Option<ServerEvent>> {
        let deadline = timeout.map(|timeout| tokio::time::Instant::now() + timeout);
        loop {
            let timeout_sleep = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };
            tokio::pin!(timeout_sleep);
            let message = tokio::select! {
                _ = &mut timeout_sleep => {
                    let secs = timeout.map(|t| t.as_secs()).unwrap_or_default();
                    bail!("voice session produced no events for {secs}s");
                }
                _ = self.ping_interval.tick() => {
                    self.socket.send(WsMessage::Ping(Vec::new().into())).await?;
                    continue;
                }
                message = self.socket.next() => message,
            };
            match message.transpose()? {
                None | Some(WsMessage::Close(_)) => return Ok(None),
                Some(WsMessage::Text(text)) => return Ok(Some(parse_server_event(&text)?)),
                Some(WsMessage::Ping(payload)) => {
                    self.socket.send(WsMessage::Pong(payload)).await?;
                }
                Some(WsMessage::Pong(_) | WsMessage::Binary(_) | WsMessage::Frame(_)) => {}
            }
        }
    }
}

fn build_ws_url(config: &VoiceConfig) -> Result<String> {
    let rest = config
        .base_url
        .strip_prefix("https://")
        .map(|rest| format!("wss://{rest}"))
        .or_else(|| {
            config
                .base_url
                .strip_prefix("http://")
                .map(|rest| format!("ws://{rest}"))
        });
    let Some(base) = rest else {
        bail!("voice base_url must start with http:// or https://");
    };
    let base = base.trim_end_matches('/');
    let mut url = format!("{base}/v1/realtime?model={}", config.model);
    if let Some(conversation_id) = &config.conversation_id {
        url.push_str("&conversation_id=");
        url.push_str(conversation_id);
    }
    Ok(url)
}

fn set_header(headers: &mut HeaderMap, name: &'static str, value: &str) -> Result<()> {
    headers.insert(name, value.parse()?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> VoiceConfig {
        VoiceConfig::new(VoiceAuth::oauth_file("/tmp/rho-voice-test-auth.json"))
    }

    #[test]
    fn ws_url_swaps_scheme_and_carries_model() {
        let config = test_config();
        assert_eq!(
            build_ws_url(&config).unwrap(),
            "wss://api.x.ai/v1/realtime?model=grok-voice-think-fast-1.0"
        );
    }

    #[test]
    fn ws_url_appends_resumption_conversation_id() {
        let mut config = test_config();
        config.conversation_id = Some("conv_123".to_owned());
        assert!(
            build_ws_url(&config)
                .unwrap()
                .ends_with("&conversation_id=conv_123")
        );
    }

    #[test]
    fn ws_url_rejects_other_schemes() {
        let mut config = test_config();
        config.base_url = "ftp://api.x.ai".to_owned();
        assert!(build_ws_url(&config).is_err());
    }
}
