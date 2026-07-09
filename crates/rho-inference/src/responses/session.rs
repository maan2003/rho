use std::collections::VecDeque;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use rho_core::{InferenceEvent, InferenceRequest};
use senax_encoder::{Decode, Encode};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::DEFAULT_CHATGPT_BASE_URL;
use super::oauth::InferenceAuth;
use super::wire::{ResponseState, ResponsesRequest};
use super::ws::{self, WebSocketConnection};
use crate::config::{DeepConfig, DeepEffort, DeepModel};

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

    pub(crate) fn to_wire_uuid(self, api_url: &str, client_secret: [u8; 32]) -> uuid::Uuid {
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
        uuid::Uuid::from_bytes(bytes)
    }

    fn debug_file_stem(self) -> String {
        let mut stem = String::with_capacity(self.0.len() * 2);
        for byte in self.0 {
            use std::fmt::Write as _;
            let _ = write!(&mut stem, "{byte:02x}");
        }
        stem
    }
}

/// A turn that has been requested and is being driven by `run`.
struct Turn {
    /// The original request, kept so a stale-`previous_response` failure can be
    /// rebuilt as a full replay.
    request: InferenceRequest,
    phase: TurnPhase,
    /// Translates streamed events into provider-neutral updates.
    response: ResponseState,
    /// Whether we have announced `StreamingStarted` for the current attempt.
    streaming_started: bool,
    /// Monotonic debug request/response sequence for the in-flight send
    /// attempt.
    debug_sequence: Option<u64>,
    /// Raw provider text frames observed for the in-flight send attempt.
    raw_events: Vec<serde_json::Value>,
    /// Transient provider/transport retry count for this turn.
    retry_attempts: u32,
}

enum TurnPhase {
    /// A request is waiting to be sent. `replay` means the previous attempt hit
    /// a stale `previous_response_id`, so this send must be a full replay.
    Queued {
        replay: bool,
        not_before: Option<Instant>,
    },
    /// A request is on the wire and we are reading its response.
    InFlight {
        replay: bool,
        used_previous_response_id: bool,
    },
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
    pub(crate) mode: InferenceSessionMode,
    pub(crate) responses_config: ResponsesConfig,
    pub(crate) prompt_cache_key: PromptCacheKey,
    debug_counter: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum InferenceSessionMode {
    Deep(DeepConfig),
    Title,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ResponsesConfig {
    pub model: ResponsesModel,
    pub auto_compaction: Option<AutoCompaction>,
    pub reasoning_context: ReasoningContext,
    pub effort: ResponsesEffort,
    pub text_verbosity: TextVerbosity,
    pub service_tier: ServiceTier,
}

impl ResponsesConfig {
    fn deep(config: DeepConfig, model: ResponsesModel) -> Self {
        let context_window = model.context_window();
        Self {
            model,
            auto_compaction: Some(AutoCompaction::Threshold(
                context_window * 95 / 100 * 90 / 100,
            )),
            reasoning_context: ReasoningContext::AllTurns,
            effort: config.effort.into(),
            text_verbosity: TextVerbosity::Low,
            service_tier: if config.fast_mode {
                ServiceTier::Priority
            } else {
                ServiceTier::Normal
            },
        }
    }

    fn title() -> Self {
        Self {
            model: ResponsesModel::Gpt54Mini,
            auto_compaction: None,
            reasoning_context: ReasoningContext::CurrentTurn,
            effort: ResponsesEffort::Medium,
            text_verbosity: TextVerbosity::Low,
            service_tier: ServiceTier::Normal,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ResponsesModel {
    Gpt55,
    Gpt56Sol,
    Gpt56Luna,
    Gpt56Terra,
    Gpt54Mini,
    #[cfg(test)]
    Test(String),
}

impl From<DeepModel> for ResponsesModel {
    fn from(model: DeepModel) -> Self {
        match model {
            DeepModel::Gpt55 => Self::Gpt55,
            DeepModel::Gpt56Sol => Self::Gpt56Sol,
            DeepModel::Gpt56Luna => Self::Gpt56Luna,
            DeepModel::Gpt56Terra => Self::Gpt56Terra,
        }
    }
}

impl ResponsesModel {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Gpt55 => "gpt-5.5",
            Self::Gpt56Sol => "gpt-5.6-sol",
            Self::Gpt56Luna => "gpt-5.6-luna",
            Self::Gpt56Terra => "gpt-5.6-terra",
            Self::Gpt54Mini => "gpt-5.4-mini",
            #[cfg(test)]
            Self::Test(model) => model,
        }
    }

    /// gpt-5.6 models use the Responses Lite wire shape: tools and base
    /// instructions ride the input timeline as developer items instead of
    /// top-level request fields, and the request is flagged via
    /// `client_metadata`.
    pub(crate) fn use_responses_lite(&self) -> bool {
        match self {
            Self::Gpt56Sol | Self::Gpt56Luna | Self::Gpt56Terra => true,
            Self::Gpt55 | Self::Gpt54Mini => false,
            #[cfg(test)]
            Self::Test(_) => false,
        }
    }

    fn context_window(&self) -> u64 {
        match self {
            Self::Gpt56Sol | Self::Gpt56Luna | Self::Gpt56Terra => 372_000,
            Self::Gpt55 | Self::Gpt54Mini => 272_000,
            #[cfg(test)]
            Self::Test(_) => 272_000,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ResponsesEffort {
    Low,
    Medium,
    Xhigh,
}

impl From<DeepEffort> for ResponsesEffort {
    fn from(effort: DeepEffort) -> Self {
        match effort {
            DeepEffort::Low => Self::Low,
            DeepEffort::Medium => Self::Medium,
            DeepEffort::Xhigh => Self::Xhigh,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ServiceTier {
    Priority,
    Normal,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum TextVerbosity {
    Low,
    #[cfg(test)]
    Medium,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ReasoningContext {
    CurrentTurn,
    AllTurns,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum AutoCompaction {
    Threshold(u64),
}

impl InferenceSession {
    pub fn new_deep(
        auth: InferenceAuth,
        config: DeepConfig,
        model: DeepModel,
        prompt_cache_key: PromptCacheKey,
    ) -> Self {
        Self {
            connection: None,
            turn: None,
            buffered: VecDeque::new(),
            base_url: DEFAULT_CHATGPT_BASE_URL.to_owned(),
            auth,
            mode: InferenceSessionMode::Deep(config),
            responses_config: ResponsesConfig::deep(config, model.into()),
            prompt_cache_key,
            debug_counter: 0,
        }
    }

    pub fn new_title(auth: InferenceAuth, prompt_cache_key: PromptCacheKey) -> Self {
        Self {
            connection: None,
            turn: None,
            buffered: VecDeque::new(),
            base_url: DEFAULT_CHATGPT_BASE_URL.to_owned(),
            auth,
            mode: InferenceSessionMode::Title,
            responses_config: ResponsesConfig::title(),
            prompt_cache_key,
            debug_counter: 0,
        }
    }

    pub fn set_deep_config(&mut self, config: DeepConfig, model: DeepModel) -> bool {
        match &mut self.mode {
            InferenceSessionMode::Deep(current) => {
                *current = config;
                self.responses_config = ResponsesConfig::deep(config, model.into());
                true
            }
            InferenceSessionMode::Title => false,
        }
    }

    pub fn prompt_cache_key(&self) -> PromptCacheKey {
        self.prompt_cache_key
    }

    pub fn has_active_request(&self) -> bool {
        self.turn.is_some()
    }

    /// Queue a turn. The work happens in `run`.
    pub fn request(&mut self, request: InferenceRequest) {
        self.buffered.clear();
        self.turn = Some(Turn {
            request,
            phase: TurnPhase::Queued {
                replay: false,
                not_before: None,
            },
            response: ResponseState::new(),
            streaming_started: false,
            debug_sequence: None,
            raw_events: Vec::new(),
            retry_attempts: 0,
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
                .is_some_and(|turn| matches!(turn.phase, TurnPhase::Queued { .. }))
            {
                let not_before = self.turn.as_ref().and_then(|turn| match turn.phase {
                    TurnPhase::Queued { not_before, .. } => not_before,
                    TurnPhase::InFlight { .. } => None,
                });
                if let Some(not_before) = not_before {
                    let now = Instant::now();
                    if not_before > now {
                        tokio::time::sleep(not_before - now).await;
                    }
                }
                if let Err(error) = self.ensure_connection().await {
                    self.connection = None;
                    match self.on_turn_error(error) {
                        ErrorAction::Retry { error, retrying_at } => {
                            return temporary_failure(error, retrying_at);
                        }
                        ErrorAction::Fail(error) => {
                            return InferenceEvent::Failed {
                                error: error.into(),
                            };
                        }
                    }
                }
                let debug_sequence = self.next_debug_sequence();
                let cached_response_id = self
                    .connection
                    .as_ref()
                    .and_then(|connection| connection.cached_response_id.as_deref())
                    .map(str::to_owned);
                let (replay, request) = {
                    let turn = self.turn.as_ref().unwrap();
                    let replay = match turn.phase {
                        TurnPhase::Queued { replay, .. } => replay,
                        TurnPhase::InFlight { .. } => unreachable!("checked above"),
                    };
                    (replay, turn.request.clone())
                };
                let mut body = ResponsesRequest::from_inference_request(
                    self,
                    request,
                    (!replay).then_some(cached_response_id).flatten().as_deref(),
                );
                let connection = self.connection.as_ref().unwrap();
                body.prompt_cache_key = self
                    .prompt_cache_key
                    .to_wire_uuid(&self.base_url, connection.client_secret);
                let turn = self.turn.as_mut().unwrap();
                turn.debug_sequence = Some(debug_sequence);
                turn.raw_events.clear();
                turn.phase = TurnPhase::InFlight {
                    replay,
                    used_previous_response_id: body.previous_response_id.is_some(),
                };
                self.maybe_debug_write_provider_request(debug_sequence, &body);
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
        let outcome = {
            let turn = self.turn.as_mut().unwrap();
            turn.raw_events.push(event.clone());
            turn.response.apply_event(&event)
        };
        match outcome {
            Err(error) => match self.on_turn_error(error) {
                ErrorAction::Retry { error, retrying_at } => {
                    ControlFlow::Break(temporary_failure(error, retrying_at))
                }
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
                let response_id = updates.iter().find_map(|update| match update {
                    InferenceEvent::Finished {
                        provider_response_id,
                        ..
                    } => provider_response_id
                        .as_ref()
                        .map(|id| id.as_str().to_owned()),
                    _ => None,
                });
                self.buffered.extend(updates);
                if done {
                    let debug = turn
                        .debug_sequence
                        .map(|sequence| (sequence, turn.raw_events.clone()));
                    self.turn = None;
                    if let Some(connection) = self.connection.as_mut() {
                        connection.cached_response_id = response_id;
                    }
                    if let Some((sequence, raw_events)) = debug {
                        self.maybe_debug_write_provider_response(sequence, &raw_events, None);
                    }
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
                ErrorAction::Retry { error, retrying_at } => {
                    ControlFlow::Break(temporary_failure(error, retrying_at))
                }
                ErrorAction::Fail(error) => ControlFlow::Break(InferenceEvent::Failed {
                    error: error.into(),
                }),
            }
        } else {
            self.connection = None;
            ControlFlow::Continue(())
        }
    }

    /// Decide whether a failed active turn should be retried or surfaced to the
    /// caller. Stale `previous_response_id` failures are retried once as a full
    /// replay; transient provider/transport failures are retried with bounded
    /// exponential backoff.
    fn on_turn_error(&mut self, error: anyhow::Error) -> ErrorAction {
        if let Some(turn) = &self.turn
            && let Some(sequence) = turn.debug_sequence
        {
            let error_message = error.to_string();
            self.maybe_debug_write_provider_response(
                sequence,
                &turn.raw_events,
                Some(error_message.as_str()),
            );
        }
        let stale_previous_response = matches!(
            &self.turn,
            Some(Turn {
                phase: TurnPhase::InFlight { replay: false, .. },
                ..
            })
        ) && super::is_stale_previous_response_error(&error)
            && self.turn_has_previous_response_id();
        if stale_previous_response {
            let turn = self.turn.as_mut().unwrap();
            turn.phase = TurnPhase::Queued {
                replay: true,
                not_before: None,
            };
            turn.streaming_started = false;
            turn.debug_sequence = None;
            turn.raw_events.clear();
            turn.response = ResponseState::new();
            // Replay on a clean socket.
            self.connection = None;
            ErrorAction::Retry {
                error,
                retrying_at: Instant::now(),
            }
        } else if self.turn.is_some()
            && is_transient_turn_error(&error)
            && self
                .turn
                .as_ref()
                .is_some_and(|turn| turn.retry_attempts < MAX_TRANSIENT_RETRIES)
        {
            let turn = self.turn.as_mut().unwrap();
            turn.retry_attempts += 1;
            let delay = transient_backoff(turn.retry_attempts);
            let retrying_at = Instant::now() + delay;
            let replay = match turn.phase {
                TurnPhase::Queued { replay, .. } | TurnPhase::InFlight { replay, .. } => replay,
            };
            turn.phase = TurnPhase::Queued {
                replay,
                not_before: Some(retrying_at),
            };
            turn.streaming_started = false;
            turn.debug_sequence = None;
            turn.raw_events.clear();
            turn.response = ResponseState::new();
            self.connection = None;
            ErrorAction::Retry { error, retrying_at }
        } else {
            self.turn = None;
            self.connection = None;
            ErrorAction::Fail(error)
        }
    }

    fn next_debug_sequence(&mut self) -> u64 {
        self.debug_counter = self.debug_counter.saturating_add(1);
        self.debug_counter
    }

    fn maybe_debug_write_provider_request(&self, sequence: u64, body: &ResponsesRequest) {
        let metadata = serde_json::json!({
            "prompt_cache_key": self.prompt_cache_key.debug_file_stem(),
            "sequence": sequence,
            "kind": "request",
            "backend": "responses",
            "transport": "websocket",
            "model": &body.model,
            "body": body,
        });
        if let Err(error) = self.debug_write_json(sequence, "request", &metadata) {
            tracing::warn!(
                prompt_cache_key = %self.prompt_cache_key.debug_file_stem(),
                sequence,
                "failed to write provider request debug log: {error}",
            );
        }
    }

    fn maybe_debug_write_provider_response(
        &self,
        sequence: u64,
        raw_events: &[serde_json::Value],
        error: Option<&str>,
    ) {
        let metadata = serde_json::json!({
            "prompt_cache_key": self.prompt_cache_key.debug_file_stem(),
            "sequence": sequence,
            "kind": "response",
            "backend": "responses",
            "transport": "websocket",
            "error": error,
            "raw_events": raw_events,
        });
        if let Err(error) = self.debug_write_json(sequence, "response", &metadata) {
            tracing::warn!(
                prompt_cache_key = %self.prompt_cache_key.debug_file_stem(),
                sequence,
                "failed to write provider response debug log: {error}",
            );
        }
    }

    fn debug_write_json(
        &self,
        sequence: u64,
        kind: &str,
        metadata: &serde_json::Value,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let Some(dir) = provider_debug_dir() else {
            return Ok(());
        };
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(debug_file_name(self.prompt_cache_key, sequence, kind));
        std::fs::write(path, serde_json::to_vec_pretty(metadata)?)?;
        Ok(())
    }

    fn turn_has_previous_response_id(&self) -> bool {
        matches!(
            &self.turn,
            Some(Turn {
                phase: TurnPhase::InFlight {
                    used_previous_response_id: true,
                    ..
                },
                ..
            })
        )
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
                .filter(|turn| matches!(turn.phase, TurnPhase::Queued { .. }))
                .map(|_| {
                    self.prompt_cache_key
                        .to_wire_uuid(&self.base_url, resolved.client_secret)
                        .to_string()
                });
            let request = ws::build_ws_request(self, thread_id.as_deref(), &resolved)?;
            let (socket, _response) = connect_async(request).await?;
            self.connection = Some(WebSocketConnection::new(socket, &resolved));
        }
        Ok(())
    }
}

pub(crate) fn provider_debug_dir() -> Option<PathBuf> {
    Some(
        dirs::state_dir()?
            .join("rho")
            .join("debug")
            .join("provider-requests"),
    )
}

pub(crate) fn debug_file_name(
    prompt_cache_key: PromptCacheKey,
    sequence: u64,
    kind: &str,
) -> String {
    format!(
        "{}-{sequence:04}-{kind}.json",
        prompt_cache_key.debug_file_stem()
    )
}

enum Read {
    Message(WsMessage),
    Closed(anyhow::Error),
    Failed(anyhow::Error),
}

enum ErrorAction {
    /// The turn is being replayed internally; surface a recoverable failure
    /// carrying the error that triggered it.
    Retry {
        error: anyhow::Error,
        retrying_at: Instant,
    },
    /// The turn is dead; surface a terminal failure.
    Fail(anyhow::Error),
}

const MAX_TRANSIENT_RETRIES: u32 = 5;
const TRANSIENT_INITIAL_DELAY_MS: u64 = 200;
const TRANSIENT_BACKOFF_FACTOR: f64 = 2.0;

pub(crate) fn transient_backoff(attempt: u32) -> Duration {
    let exp = TRANSIENT_BACKOFF_FACTOR.powi(attempt.saturating_sub(1) as i32);
    let base = (TRANSIENT_INITIAL_DELAY_MS as f64 * exp) as u64;
    let jitter = rand::Rng::gen_range(&mut rand::thread_rng(), 0.9..1.1);
    Duration::from_millis((base as f64 * jitter) as u64)
}

pub(crate) fn is_transient_turn_error(error: &anyhow::Error) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    [
        "server_is_overloaded",
        "slow_down",
        "overloaded",
        "rate_limit_exceeded",
        "service_unavailable",
        "server_error",
        "internal_server_error",
        "temporarily unavailable",
        "try again",
        "timed out",
        "timeout",
        "websocket ended before response.completed",
        "websocket closed mid-stream",
        "connection reset",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn temporary_failure(error: anyhow::Error, retrying_at: Instant) -> InferenceEvent {
    InferenceEvent::TemporaryFailure {
        error: error.into(),
        retrying_at,
    }
}

impl std::fmt::Debug for InferenceSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InferenceSession")
            .field("base_url", &self.base_url)
            .field("auth", &self.auth)
            .field("mode", &self.mode)
            .field("responses_config", &self.responses_config)
            .field("prompt_cache_key", &self.prompt_cache_key)
            .finish()
    }
}
