use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::time::Instant;

use anyhow::Result;
use rho_core::{InferenceEvent, InferenceRequest};
use senax_encoder::{Decode, Encode};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::DEFAULT_CHATGPT_BASE_URL;
use super::oauth::InferenceAuth;
use super::wire::{ResponseState, ResponsesRequest};
use super::ws::{self, WebSocketConnection};
use crate::config::InferenceProtectedConfig;

#[derive(
    Clone, Copy, Debug, Decode, Encode, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize,
)]
pub struct PromptCacheKey([u8; 8]);

impl PromptCacheKey {
    pub fn generate() -> Self {
        let mut bytes = [0; 8];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
        Self(bytes)
    }

    #[cfg(test)]
    pub(crate) const fn from_bytes(bytes: [u8; 8]) -> Self {
        Self(bytes)
    }

    pub(crate) fn to_wire_string(self, api_url: &str, client_secret: [u8; 32]) -> String {
        use std::hash::Hasher;

        fn fnv64(parts: &[&[u8]]) -> u64 {
            let mut hash = fnv::FnvHasher::default();
            for part in parts {
                hash.write(part);
            }
            hash.finish()
        }

        let hi = fnv64(&[
            b"rho-prompt-cache-key:v8:0",
            &self.0,
            api_url.as_bytes(),
            &client_secret,
        ]);
        let lo = fnv64(&[
            b"rho-prompt-cache-key:v8:1",
            &self.0,
            api_url.as_bytes(),
            &client_secret,
        ]);
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&hi.to_be_bytes());
        bytes[8..].copy_from_slice(&lo.to_be_bytes());
        bytes[6] = (bytes[6] & 0x0f) | 0x80;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        uuid::Uuid::from_bytes(bytes).to_string()
    }
}

/// A turn that has been requested and is being driven by `run`.
struct Turn {
    /// The original request, kept so a stale-`previous_response` failure can be
    /// rebuilt as a full replay.
    request: InferenceRequest,
    /// The envelope still waiting to be sent; `None` once it is on the wire.
    pending_send: Option<ResponsesRequest>,
    /// Translates streamed events into provider-neutral updates.
    response: ResponseState,
    /// Whether we already retried this turn as a full replay.
    replayed: bool,
    /// Whether we have announced `StreamingStarted` for the current attempt.
    streaming_started: bool,
}

pub struct InferenceSession {
    /// The session's warm WebSocket, kept alive across turns and owned outright
    /// (single owner, no lock). Reopened lazily when missing, stale, or dropped
    /// after a failure.
    connection: Option<WebSocketConnection>,
    /// The active turn, if one has been requested.
    turn: Option<Turn>,
    /// Updates parsed from a frame but not yet handed out by `run` (one frame
    /// can yield several updates, and `run` returns one at a time).
    buffered: VecDeque<InferenceEvent>,
    pub(crate) base_url: String,
    pub(crate) auth: InferenceAuth,
    pub(crate) config: InferenceProtectedConfig,
    pub(crate) prompt_cache_key: PromptCacheKey,
}

impl InferenceSession {
    pub fn new(
        auth: InferenceAuth,
        config: InferenceProtectedConfig,
        prompt_cache_key: PromptCacheKey,
    ) -> Self {
        Self {
            connection: None,
            turn: None,
            buffered: VecDeque::new(),
            base_url: DEFAULT_CHATGPT_BASE_URL.to_owned(),
            auth,
            config,
            prompt_cache_key,
        }
    }

    pub fn prompt_cache_key(&self) -> PromptCacheKey {
        self.prompt_cache_key
    }

    pub fn config(&self) -> &InferenceProtectedConfig {
        &self.config
    }

    /// Queue a turn. The work happens in `run`.
    pub fn request(&mut self, request: InferenceRequest) {
        let body = ResponsesRequest::from_inference_request(self, request.clone());
        self.buffered.clear();
        self.turn = Some(Turn {
            request,
            pending_send: Some(body),
            response: ResponseState::new(),
            replayed: false,
            streaming_started: false,
        });
    }

    /// Abort the active turn and drop the (now indeterminate) connection.
    pub fn abort(&mut self) {
        self.turn = None;
        self.buffered.clear();
        self.connection = None;
    }

    /// Drive the connection and return the next update for the active request.
    /// Pends, keeping any warm socket alive, when there is no active request.
    pub async fn run(&mut self) -> InferenceEvent {
        loop {
            // 1. Hand out anything already parsed.
            if let Some(update) = self.buffered.pop_front() {
                return update;
            }

            // 2. Send a queued envelope (connecting first if needed).
            if self
                .turn
                .as_ref()
                .is_some_and(|turn| turn.pending_send.is_some())
            {
                if let Err(error) = self.ensure_connection().await {
                    self.turn = None;
                    self.connection = None;
                    return InferenceEvent::Failed {
                        error: error.into(),
                    };
                }
                let mut body = self.turn.as_mut().unwrap().pending_send.take().unwrap();
                let connection = self.connection.as_ref().unwrap();
                body.prompt_cache_key = self
                    .prompt_cache_key
                    .to_wire_string(&self.base_url, connection.client_secret);
                if let Err(error) = self.connection.as_mut().unwrap().send_envelope(body).await {
                    match self.on_socket_failure(error) {
                        ControlFlow::Continue(()) => continue,
                        ControlFlow::Break(update) => return update,
                    }
                }
                return InferenceEvent::RequestSent;
            }

            // 3. Read the socket. With no connection and nothing to send we are idle with
            //    nothing to keep warm, so pend.
            if self.connection.is_none() {
                std::future::pending::<()>().await;
            }
            let active = self.turn.is_some();
            let timeout = active.then_some(ws::EVENT_TIMEOUT);
            let read = {
                let connection = self.connection.as_mut().unwrap();
                match connection.next_message(timeout).await {
                    Ok(Some(message)) => Read::Message(message),
                    Ok(None) => Read::Closed(anyhow::anyhow!(
                        "stream error: websocket ended before response.completed"
                    )),
                    Err(error) => Read::Failed(error),
                }
            };

            match read {
                Read::Message(WsMessage::Text(text)) => {
                    if let ControlFlow::Break(update) = self.apply_text(text.as_ref()) {
                        return update;
                    }
                }
                Read::Message(WsMessage::Ping(payload)) => {
                    if let Err(error) = self.connection.as_mut().unwrap().pong(payload).await
                        && let ControlFlow::Break(update) = self.on_socket_failure(error)
                    {
                        return update;
                    }
                }
                Read::Message(WsMessage::Close(_)) => {
                    if let ControlFlow::Break(update) = self.on_socket_failure(anyhow::anyhow!(
                        "stream error: websocket closed mid-stream"
                    )) {
                        return update;
                    }
                }
                Read::Message(WsMessage::Binary(_) | WsMessage::Pong(_) | WsMessage::Frame(_)) => {}
                Read::Closed(error) | Read::Failed(error) => {
                    if let ControlFlow::Break(update) = self.on_socket_failure(error) {
                        return update;
                    }
                }
            }
        }
    }

    /// Apply one text frame to the active turn's accumulator, buffering the
    /// updates it produces. `Break` carries an update to surface from `run`.
    fn apply_text(&mut self, text: &str) -> ControlFlow<InferenceEvent> {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(text) else {
            return ControlFlow::Continue(());
        };
        // Ignore stray frames while idle (no response is in flight).
        if self.turn.is_none() {
            return ControlFlow::Continue(());
        }
        let outcome = self.turn.as_mut().unwrap().response.apply_event(&event);
        match outcome {
            Err(error) => match self.on_turn_error(error) {
                ErrorAction::Retry(error) => ControlFlow::Break(temporary_failure(error)),
                ErrorAction::Fail(error) => ControlFlow::Break(InferenceEvent::Failed {
                    error: error.into(),
                }),
            },
            Ok((done, updates)) => {
                // Announce the first streamed content of the turn once.
                let turn = self.turn.as_mut().unwrap();
                if !turn.streaming_started
                    && updates
                        .iter()
                        .any(|update| matches!(update, InferenceEvent::ContextItem { .. }))
                {
                    turn.streaming_started = true;
                    self.buffered.push_back(InferenceEvent::StreamingStarted);
                }
                // The terminal `InferenceUpdate::Finished` is emitted by
                // `apply_event` itself, so here we just drop the finished turn.
                self.buffered.extend(updates);
                if done {
                    self.turn = None;
                }
                ControlFlow::Continue(())
            }
        }
    }

    /// A read/write failure: replay or fail the active turn, or just drop the
    /// dead socket when idle. `Break` carries the update to return from `run`.
    fn on_socket_failure(&mut self, error: anyhow::Error) -> ControlFlow<InferenceEvent> {
        if self.turn.is_some() {
            match self.on_turn_error(error) {
                ErrorAction::Retry(error) => ControlFlow::Break(temporary_failure(error)),
                ErrorAction::Fail(error) => ControlFlow::Break(InferenceEvent::Failed {
                    error: error.into(),
                }),
            }
        } else {
            self.connection = None;
            ControlFlow::Continue(())
        }
    }

    /// Decide whether a failed active turn should be replayed in full (a stale
    /// `previous_response_id` we can drop) or surfaced to the caller.
    fn on_turn_error(&mut self, error: anyhow::Error) -> ErrorAction {
        let replayable = matches!(&self.turn, Some(turn) if !turn.replayed)
            && super::is_stale_previous_response_error(&error)
            && self.turn_has_previous_response_id();
        if replayable {
            let request = self.turn.as_ref().unwrap().request.clone();
            let body = ResponsesRequest::from_inference_request_full_replay(self, request);
            let turn = self.turn.as_mut().unwrap();
            turn.pending_send = Some(body);
            turn.replayed = true;
            turn.streaming_started = false;
            turn.response = ResponseState::new();
            // Replay on a clean socket.
            self.connection = None;
            ErrorAction::Retry(error)
        } else {
            self.turn = None;
            self.connection = None;
            ErrorAction::Fail(error)
        }
    }

    fn turn_has_previous_response_id(&self) -> bool {
        match &self.turn {
            Some(turn) => ResponsesRequest::from_inference_request(self, turn.request.clone())
                .previous_response_id
                .is_some(),
            None => false,
        }
    }

    /// Ensure a usable connection, reopening when missing, when OAuth rotated
    /// the bearer, or when nearing the server's age cap.
    async fn ensure_connection(&mut self) -> Result<()> {
        let auth = self.auth.clone();
        let resolved = tokio::task::spawn_blocking(move || auth.resolve()).await??;
        let reusable = self.connection.as_ref().is_some_and(|connection| {
            connection.bearer_token == resolved.bearer_token
                && connection.opened_at.elapsed() < ws::MAX_CONNECTION_AGE
        });
        if !reusable {
            let thread_id = self
                .turn
                .as_ref()
                .and_then(|turn| turn.pending_send.as_ref())
                .map(|body| body.prompt_cache_key.clone());
            let request = ws::build_ws_request(self, thread_id.as_deref(), &resolved)?;
            let (socket, _response) = connect_async(request).await?;
            self.connection = Some(WebSocketConnection::new(socket, &resolved));
        }
        Ok(())
    }
}

enum Read {
    Message(WsMessage),
    Closed(anyhow::Error),
    Failed(anyhow::Error),
}

enum ErrorAction {
    /// The turn is being replayed internally; surface a recoverable failure
    /// carrying the error that triggered it.
    Retry(anyhow::Error),
    /// The turn is dead; surface a terminal failure.
    Fail(anyhow::Error),
}

fn temporary_failure(error: anyhow::Error) -> InferenceEvent {
    InferenceEvent::TemporaryFailure {
        error: error.into(),
        retrying_at: Instant::now(),
    }
}

impl std::fmt::Debug for InferenceSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InferenceSession")
            .field("base_url", &self.base_url)
            .field("auth", &self.auth)
            .field("config", &self.config.config())
            .field("prompt_cache_key", &self.prompt_cache_key)
            .finish()
    }
}
