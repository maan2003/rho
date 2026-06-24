use std::collections::VecDeque;
use std::ops::ControlFlow;

use anyhow::Result;
use rho_core::{InferenceRequest, InferenceUpdate};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::oauth::InferenceAuth;
use super::wire::{ResponseState, ResponsesRequest};
use super::ws::{self, WebSocketConnection};
use super::{Compaction, DEFAULT_CHATGPT_BASE_URL, DEFAULT_MODEL};

/// A turn that has been requested and is being driven by `run`.
struct Turn {
    /// The original request, kept so a stale-`previous_response` failure can be
    /// rebuilt as a full replay.
    request: InferenceRequest,
    /// The envelope still waiting to be sent; `None` once it is on the wire.
    pending_send: Option<ResponsesRequest>,
    /// Accumulates streamed events into the final response.
    response: ResponseState,
    /// Whether we already retried this turn as a full replay.
    replayed: bool,
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
    buffered: VecDeque<InferenceUpdate>,
    pub(crate) base_url: String,
    pub(crate) auth: InferenceAuth,
    pub(crate) compaction: Option<Compaction>,
    pub(crate) model: String,
    pub(crate) prompt_cache_key: Option<String>,
}

impl InferenceSession {
    pub const DEFAULT_MODEL: &'static str = DEFAULT_MODEL;

    pub fn new(model: impl Into<String>, auth: InferenceAuth) -> Self {
        Self {
            connection: None,
            turn: None,
            buffered: VecDeque::new(),
            base_url: DEFAULT_CHATGPT_BASE_URL.to_owned(),
            auth,
            compaction: None,
            model: model.into(),
            prompt_cache_key: None,
        }
    }

    pub fn with_compaction(mut self) -> Self {
        self.compaction = Some(Compaction::Default);
        self
    }

    pub fn with_compaction_threshold(mut self, compact_threshold: u64) -> Self {
        self.compaction = Some(Compaction::Threshold(compact_threshold));
        self
    }

    pub fn with_prompt_cache_key(mut self, prompt_cache_key: impl Into<String>) -> Self {
        self.prompt_cache_key = Some(prompt_cache_key.into());
        self
    }

    pub fn prompt_cache_key(&self) -> Option<&str> {
        self.prompt_cache_key.as_deref()
    }

    /// Queue a turn. The work happens in `run`.
    pub fn request(&mut self, request: InferenceRequest) {
        let tool_names = super::tool_name_map(&request.tools);
        let body = ResponsesRequest::from_inference_request(self, request.clone());
        self.buffered.clear();
        self.turn = Some(Turn {
            request,
            pending_send: Some(body),
            response: ResponseState::with_tool_names(tool_names),
            replayed: false,
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
    pub async fn run(&mut self) -> Result<InferenceUpdate> {
        loop {
            // 1. Hand out anything already parsed.
            if let Some(update) = self.buffered.pop_front() {
                return Ok(update);
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
                    return Err(error);
                }
                let body = self.turn.as_mut().unwrap().pending_send.take().unwrap();
                if let Err(error) = self.connection.as_mut().unwrap().send_envelope(body).await {
                    match self.on_socket_failure(error) {
                        ControlFlow::Continue(()) => continue,
                        ControlFlow::Break(result) => return result,
                    }
                }
                continue;
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
                    if let ControlFlow::Break(result) = self.apply_text(text.as_ref()) {
                        return result;
                    }
                }
                Read::Message(WsMessage::Ping(payload)) => {
                    if let Err(error) = self.connection.as_mut().unwrap().pong(payload).await
                        && let ControlFlow::Break(result) = self.on_socket_failure(error)
                    {
                        return result;
                    }
                }
                Read::Message(WsMessage::Close(_)) => {
                    if let ControlFlow::Break(result) = self.on_socket_failure(anyhow::anyhow!(
                        "stream error: websocket closed mid-stream"
                    )) {
                        return result;
                    }
                }
                Read::Message(WsMessage::Binary(_) | WsMessage::Pong(_) | WsMessage::Frame(_)) => {}
                Read::Closed(error) | Read::Failed(error) => {
                    if let ControlFlow::Break(result) = self.on_socket_failure(error) {
                        return result;
                    }
                }
            }
        }
    }

    /// Apply one text frame to the active turn's accumulator, buffering the
    /// updates it produces. `Break` carries an error to surface from `run`.
    fn apply_text(&mut self, text: &str) -> ControlFlow<Result<InferenceUpdate>> {
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
                ErrorAction::Retry => ControlFlow::Continue(()),
                ErrorAction::Fail(error) => ControlFlow::Break(Err(error)),
            },
            Ok((done, updates)) => {
                self.buffered.extend(updates);
                if done {
                    let turn = self.turn.take().unwrap();
                    self.buffered
                        .push_back(InferenceUpdate::Finished(turn.response.finish()));
                }
                ControlFlow::Continue(())
            }
        }
    }

    /// A read/write failure: fail or replay the active turn, or just drop the
    /// dead socket when idle. `Break` carries the result to return from `run`.
    fn on_socket_failure(&mut self, error: anyhow::Error) -> ControlFlow<Result<InferenceUpdate>> {
        if self.turn.is_some() {
            match self.on_turn_error(error) {
                ErrorAction::Retry => ControlFlow::Continue(()),
                ErrorAction::Fail(error) => ControlFlow::Break(Err(error)),
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
            let tool_names = super::tool_name_map(&turn.request.tools);
            turn.pending_send = Some(body);
            turn.replayed = true;
            turn.response = ResponseState::with_tool_names(tool_names);
            // Replay on a clean socket.
            self.connection = None;
            ErrorAction::Retry
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
                .and_then(|body| body.prompt_cache_key.clone());
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
    Retry,
    Fail(anyhow::Error),
}

impl std::fmt::Debug for InferenceSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InferenceSession")
            .field("base_url", &self.base_url)
            .field("auth", &self.auth)
            .field("compaction", &self.compaction)
            .field("model", &self.model)
            .field("prompt_cache_key", &self.prompt_cache_key)
            .finish()
    }
}
