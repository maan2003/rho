use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use rho_core::{InferenceEvent, InferenceRequest};
use senax_encoder::{Decode, Encode};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::DEFAULT_CHATGPT_BASE_URL;
use super::wire::{ResponseState, ResponsesRequest};
use super::ws::{self, WebSocketConnection};
use crate::config::{InferenceModel, InferenceProfile, ReasoningEffort};
use crate::inference::Inference;

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

    pub fn debug_file_stem(self) -> String {
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

#[derive(Clone, Copy)]
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

/// What a session asks for and where it asks, all of it settled by the caller
/// rather than by the wire.
///
/// Held by the handle, because callers read and revise it between turns without
/// awaiting anything, and copied to the task with every request so a body is
/// built from what was true when the request was made.
#[derive(Clone)]
pub(crate) struct SessionConfig {
    pub(crate) base_url: String,
    pub(crate) inference: Inference,
    pub(crate) mode: InferenceSessionMode,
    pub(crate) responses_config: ResponsesConfig,
    pub(crate) prompt_cache_key: PromptCacheKey,
}

/// A handle onto a session that runs in a task of its own.
///
/// Every method here is either a synchronous read of `config` or a message, so
/// nothing a caller does can interrupt the socket. That is the whole reason for
/// the split: a caller that drives [`InferenceSession::run`] from a `select!`
/// drops that future every time any other arm wins, and the I/O it was in the
/// middle of used to go with it — a half-written envelope, or a TLS handshake
/// started over from nothing. `run` is now a channel receive, which loses
/// nothing when it is dropped.
pub struct InferenceSession {
    pub(crate) config: SessionConfig,
    /// Spawned on the first request rather than at construction, so a session
    /// can be built and inspected outside a runtime.
    commands: Option<tokio::sync::mpsc::UnboundedSender<Command>>,
    events: tokio::sync::mpsc::UnboundedReceiver<(u64, InferenceEvent)>,
    events_tx: tokio::sync::mpsc::UnboundedSender<(u64, InferenceEvent)>,
    /// The turn this handle is waiting on, if any. Events are tagged with the
    /// epoch of the turn that produced them, so the ones already in the channel
    /// when a turn is abandoned are dropped rather than delivered as if they
    /// answered whatever came next — and `None` says that *everything* still
    /// arriving is stale, which is what an abort leaves behind.
    ///
    /// It is also the answer to [`InferenceSession::has_active_request`], there
    /// being no second place for that to be written down and disagree.
    awaiting: Option<u64>,
    /// Never reused, so an abandoned turn can never be mistaken for the one
    /// that replaced it.
    epochs: u64,
}

enum Command {
    Request {
        epoch: u64,
        config: SessionConfig,
        request: InferenceRequest,
    },
    Abort,
}

/// The half that owns the socket, and the only half that awaits it.
struct SessionTask {
    /// The session's warm WebSocket, kept alive across turns and owned outright
    /// (single owner, no lock). Reopened lazily when missing, stale, or dropped
    /// after a failure.
    connection: Option<WebSocketConnection>,
    /// The active turn, if one has been requested.
    turn: Option<Turn>,
    config: SessionConfig,
    debug_counter: u64,
    /// Where every event goes, tagged with the turn that produced it. Emitting
    /// straight into the channel is what lets one frame yield several updates
    /// without a queue in between: the channel is that queue.
    events: tokio::sync::mpsc::UnboundedSender<(u64, InferenceEvent)>,
    epoch: u64,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum InferenceSessionMode {
    Deep(InferenceProfile),
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
    fn deep(config: InferenceProfile, model: ResponsesModel) -> Self {
        let info = model.info();
        Self {
            // Responses Lite rejects server-side compaction requests. The
            // agent's explicit trigger policy works for both wire shapes.
            auto_compaction: (!model.use_responses_lite())
                .then_some(AutoCompaction::Threshold(info.auto_compact_token_limit)),
            model,
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
            model: ResponsesModel::Gpt56Luna,
            auto_compaction: None,
            reasoning_context: ReasoningContext::AllTurns,
            effort: ResponsesEffort::Medium,
            text_verbosity: TextVerbosity::Low,
            service_tier: ServiceTier::Priority,
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ResponsesModel {
    Gpt55,
    Gpt56Sol,
    Gpt56Luna,
    Gpt56Terra,
    #[cfg(test)]
    Test(String),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ResponsesModelInfo {
    context_window: u64,
    auto_compact_token_limit: u64,
}

impl From<InferenceModel> for ResponsesModel {
    fn from(model: InferenceModel) -> Self {
        match model {
            InferenceModel::Gpt55 => Self::Gpt55,
            InferenceModel::Gpt56Sol => Self::Gpt56Sol,
            InferenceModel::Gpt56Luna => Self::Gpt56Luna,
            InferenceModel::Gpt56Terra => Self::Gpt56Terra,
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
            Self::Gpt55 => false,
            #[cfg(test)]
            Self::Test(_) => false,
        }
    }

    fn info(&self) -> ResponsesModelInfo {
        match self {
            Self::Gpt56Sol | Self::Gpt56Luna | Self::Gpt56Terra => ResponsesModelInfo {
                context_window: 372_000,
                auto_compact_token_limit: 280_000,
            },
            Self::Gpt55 => ResponsesModelInfo {
                context_window: 272_000,
                auto_compact_token_limit: 232_560,
            },
            #[cfg(test)]
            Self::Test(_) => ResponsesModelInfo {
                context_window: 272_000,
                auto_compact_token_limit: 232_560,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ResponsesEffort {
    Low,
    Medium,
    Xhigh,
    High,
}

impl From<ReasoningEffort> for ResponsesEffort {
    fn from(effort: ReasoningEffort) -> Self {
        match effort {
            ReasoningEffort::Low => Self::Low,
            ReasoningEffort::Medium => Self::Medium,
            ReasoningEffort::Xhigh => Self::Xhigh,
            ReasoningEffort::High => Self::High,
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
    #[cfg(test)]
    CurrentTurn,
    AllTurns,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) enum AutoCompaction {
    Threshold(u64),
}

impl InferenceSession {
    pub(crate) fn new_deep(
        inference: Inference,
        config: InferenceProfile,
        model: InferenceModel,
        prompt_cache_key: PromptCacheKey,
    ) -> Self {
        Self::new(SessionConfig {
            base_url: DEFAULT_CHATGPT_BASE_URL.to_owned(),
            inference,
            mode: InferenceSessionMode::Deep(config),
            responses_config: ResponsesConfig::deep(config, model.into()),
            prompt_cache_key,
        })
    }

    pub(crate) fn new_title(inference: Inference, prompt_cache_key: PromptCacheKey) -> Self {
        Self::new(SessionConfig {
            base_url: DEFAULT_CHATGPT_BASE_URL.to_owned(),
            inference,
            mode: InferenceSessionMode::Title,
            responses_config: ResponsesConfig::title(),
            prompt_cache_key,
        })
    }

    fn new(config: SessionConfig) -> Self {
        let (events_tx, events) = tokio::sync::mpsc::unbounded_channel();
        Self {
            config,
            commands: None,
            events,
            events_tx,
            awaiting: None,
            epochs: 0,
        }
    }

    pub fn set_deep_config(&mut self, config: InferenceProfile, model: InferenceModel) -> bool {
        match &mut self.config.mode {
            InferenceSessionMode::Deep(current) => {
                *current = config;
                self.config.responses_config = ResponsesConfig::deep(config, model.into());
                true
            }
            InferenceSessionMode::Title => false,
        }
    }

    pub fn prompt_cache_key(&self) -> PromptCacheKey {
        self.config.prompt_cache_key
    }

    pub fn set_prompt_cache_key(&mut self, prompt_cache_key: PromptCacheKey) {
        self.config.prompt_cache_key = prompt_cache_key;
    }

    pub fn has_active_request(&self) -> bool {
        self.awaiting.is_some()
    }

    /// Report the advertised context window for a deep inference session.
    pub fn context_window(&self) -> Option<u64> {
        matches!(self.config.mode, InferenceSessionMode::Deep(_))
            .then(|| self.config.responses_config.model.info().context_window)
    }

    /// Report the context occupancy at which the client should explicitly
    /// request compaction.
    pub fn auto_compact_token_limit(&self) -> Option<u64> {
        matches!(self.config.mode, InferenceSessionMode::Deep(_)).then(|| {
            self.config
                .responses_config
                .model
                .info()
                .auto_compact_token_limit
        })
    }

    /// Queue a turn. The work happens in the task.
    pub fn request(&mut self, request: InferenceRequest) {
        self.epochs += 1;
        self.awaiting = Some(self.epochs);
        let events_tx = self.events_tx.clone();
        let config = self.config.clone();
        let commands = self.commands.get_or_insert_with(|| {
            let (commands, rx) = tokio::sync::mpsc::unbounded_channel();
            tokio::spawn(
                SessionTask {
                    connection: None,
                    turn: None,
                    config: config.clone(),
                    debug_counter: 0,
                    events: events_tx,
                    epoch: 0,
                }
                .drive(rx),
            );
            commands
        });
        // The task outlives every individual turn, so a closed channel here
        // means the runtime is going down and there is nothing to say about it.
        let _ = commands.send(Command::Request {
            epoch: self.epochs,
            config,
            request,
        });
    }

    /// Abort the active turn and drop the (now indeterminate) connection.
    pub fn abort(&mut self) {
        self.awaiting = None;
        if let Some(commands) = &self.commands {
            let _ = commands.send(Command::Abort);
        }
    }

    /// Take the next update for the active request.
    ///
    /// Cancel-safe: dropping this future loses nothing, because the work is in
    /// the task and this only takes what the task has already produced. Pends
    /// when there is nothing outstanding.
    pub async fn run(&mut self) -> InferenceEvent {
        loop {
            let Some((epoch, event)) = self.events.recv().await else {
                // Only reachable if the task panicked. Pending is the truthful
                // answer — nothing further is coming — and it keeps a caller in
                // a `select!` from spinning on the same news forever.
                std::future::pending::<()>().await;
                unreachable!()
            };
            // Whatever this belonged to has ended or been thrown away.
            if Some(epoch) != self.awaiting {
                continue;
            }
            if matches!(
                event,
                InferenceEvent::Finished { .. } | InferenceEvent::Failed { .. }
            ) {
                self.awaiting = None;
            }
            return event;
        }
    }
}

impl SessionTask {
    /// Live for as long as the handle does, which is what the task is: the
    /// socket has one owner and it is this.
    ///
    /// The only thing that interrupts `pump` is a command, and both commands
    /// throw the connection away when a turn was in flight — so the socket is
    /// never left half-written *and* kept. That is the invariant the whole
    /// split exists to hold, and it is why the task may have a `select!` where
    /// the caller may not.
    async fn drive(mut self, mut commands: tokio::sync::mpsc::UnboundedReceiver<Command>) {
        loop {
            tokio::select! {
                biased;
                command = commands.recv() => match command {
                    // The handle is gone, and with it anyone who could read an
                    // event or ask for a turn.
                    None => return,
                    Some(Command::Abort) => self.abort(),
                    Some(Command::Request { epoch, config, request }) => {
                        // Replacing a turn is an abort first: `pump` may have
                        // been mid-write when this arrived, and dropping the
                        // connection is what makes that harmless.
                        if self.turn.is_some() {
                            self.abort();
                        }
                        self.config = config;
                        self.epoch = epoch;
                        self.request(request);
                    }
                },
                () = self.pump() => unreachable!("the socket is never finished with"),
            }
        }
    }

    fn request(&mut self, request: InferenceRequest) {
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

    fn abort(&mut self) {
        self.turn = None;
        self.connection = None;
    }

    /// Hand one event to whoever is listening, tagged with the turn it belongs
    /// to. A closed channel means the handle is gone, which the command arm of
    /// `serve` notices for itself.
    fn emit(&self, event: InferenceEvent) {
        let _ = self.events.send((self.epoch, event));
    }

    /// Send what is queued and read what comes back, emitting as it goes.
    /// Never returns: with nothing to send it keeps any warm socket alive, and
    /// with no socket either it pends.
    async fn pump(&mut self) {
        loop {
            // 1. Send a queued envelope (connecting first if needed). Read
            // once: what the phase says is settled before anything below is
            // awaited, and re-reading it afterwards only invites a second
            // answer.
            if let Some(TurnPhase::Queued { replay, not_before }) =
                self.turn.as_ref().map(|turn| turn.phase)
            {
                if let Some(not_before) = not_before {
                    let now = Instant::now();
                    if not_before > now {
                        tokio::time::sleep(not_before - now).await;
                    }
                }
                if let Err(error) = self.ensure_connection().await {
                    self.connection = None;
                    self.fail_turn(error);
                    continue;
                }
                let debug_sequence = self.next_debug_sequence();
                let cached_response_id = self
                    .connection
                    .as_ref()
                    .and_then(|connection| connection.cached_response_id.as_deref())
                    .map(str::to_owned);
                let request = self.turn.as_ref().unwrap().request.clone();
                let mut body = ResponsesRequest::from_inference_request(
                    &self.config,
                    request,
                    (!replay).then_some(cached_response_id).flatten().as_deref(),
                );
                let connection = self.connection.as_ref().unwrap();
                body.prompt_cache_key = self
                    .config
                    .prompt_cache_key
                    .to_wire_uuid(&self.config.base_url, connection.client_secret);
                let turn = self.turn.as_mut().unwrap();
                turn.debug_sequence = Some(debug_sequence);
                turn.raw_events.clear();
                turn.phase = TurnPhase::InFlight {
                    replay,
                    used_previous_response_id: body.previous_response_id.is_some(),
                };
                self.maybe_debug_write_provider_request(debug_sequence, &body);
                if let Err(error) = self.connection.as_mut().unwrap().send_envelope(body).await {
                    self.on_socket_failure(error);
                    continue;
                }
                self.emit(InferenceEvent::RequestSent);
            }

            // 2. Read the socket. With no connection and nothing to send we are idle with
            //    nothing to keep warm, so pend.
            if self.connection.is_none() {
                std::future::pending::<()>().await;
            }
            // A turn bounds how long silence is allowed to last; an idle warm
            // socket may be quiet forever.
            let timeout = self.turn.is_some().then_some(ws::EVENT_TIMEOUT);
            // Read to a value first, so the borrow of the connection is over
            // before anything below reaches for it again.
            let read = self
                .connection
                .as_mut()
                .unwrap()
                .next_message(timeout)
                .await
                .and_then(|message| {
                    message.ok_or_else(|| {
                        anyhow::anyhow!("stream error: websocket ended before response.completed")
                    })
                });

            match read {
                Ok(WsMessage::Text(text)) => self.apply_text(text.as_ref()),
                Ok(WsMessage::Ping(payload)) => {
                    if let Err(error) = self.connection.as_mut().unwrap().pong(payload).await {
                        self.on_socket_failure(error);
                    }
                }
                Ok(WsMessage::Close(_)) => self.on_socket_failure(anyhow::anyhow!(
                    "stream error: websocket closed mid-stream"
                )),
                Ok(WsMessage::Binary(_) | WsMessage::Pong(_) | WsMessage::Frame(_)) => {}
                Err(error) => self.on_socket_failure(error),
            }
        }
    }

    /// Apply one text frame to the active turn's accumulator and emit whatever
    /// it produces — which may be nothing, or several updates at once.
    fn apply_text(&mut self, text: &str) {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(text) else {
            return;
        };
        // Ignore stray frames while idle (no response is in flight).
        if self.turn.is_none() {
            return;
        }
        let outcome = {
            let turn = self.turn.as_mut().unwrap();
            turn.raw_events.push(event.clone());
            turn.response.apply_event(&event)
        };
        match outcome {
            Err(error) => self.fail_turn(error),
            Ok((done, updates)) => {
                // Announce the first streamed content of the turn once.
                let turn = self.turn.as_mut().unwrap();
                let starting = !turn.streaming_started
                    && updates
                        .iter()
                        .any(|update| matches!(update, InferenceEvent::ContextItem { .. }));
                turn.streaming_started |= starting;
                if starting {
                    self.emit(InferenceEvent::StreamingStarted);
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
                // Quota is account-wide rather than a fact about this stream,
                // so hand it to the account on the way past.
                //
                // TODO: and then stop forwarding it. `InferenceEvent::Quota`
                // only exists because the wire parser has no handle on the
                // account, so it smuggles the observation through the update
                // stream to be picked up here. Once it has been, nothing
                // downstream needs it: `Inference::quota` is a watch that
                // yields immediately and on every change, and `latest_quota`
                // covers callers with nothing to await. Every consumer today
                // either ignores the event (rho-agent2, rho-agent's title
                // session) or copies it into a second place that then has to be
                // diffed against itself (rho-agent's `AgentState`, read by
                // rho-daemon). Dropping the variant means moving those to
                // `inference.quota()` and handing the parser a way to report it
                // that is not a public enum.
                for update in &updates {
                    if let InferenceEvent::Quota {
                        used_percent,
                        reset_at_unix,
                    } = update
                    {
                        self.config
                            .inference
                            .observe_quota(*used_percent, *reset_at_unix);
                    }
                }
                for update in updates {
                    self.emit(update);
                }
                if done {
                    // Re-read rather than hold a borrow across the emits above:
                    // the events go out first, and this runs once per turn
                    // rather than once per frame.
                    let debug = self.turn.as_ref().and_then(|turn| {
                        turn.debug_sequence
                            .map(|sequence| (sequence, turn.raw_events.clone()))
                    });
                    self.turn = None;
                    if let Some(connection) = self.connection.as_mut() {
                        connection.cached_response_id = response_id;
                    }
                    if let Some((sequence, raw_events)) = debug {
                        self.maybe_debug_write_provider_response(sequence, &raw_events, None);
                    }
                }
            }
        }
    }

    /// A read/write failure: replay or fail the active turn, or just drop the
    /// dead socket when idle.
    fn on_socket_failure(&mut self, error: anyhow::Error) {
        match self.turn.is_some() {
            true => self.fail_turn(error),
            // Nothing to fail, so the dead socket is the whole of it.
            false => self.connection = None,
        }
    }

    /// Say what a failed turn sounds like: a recoverable failure if it is being
    /// retried internally, and a final one if it is not.
    fn fail_turn(&mut self, error: anyhow::Error) {
        match self.on_turn_error(error) {
            ErrorAction::Retry { error, retrying_at } => {
                self.emit(temporary_failure(error, retrying_at))
            }
            ErrorAction::Fail(error) => self.emit(InferenceEvent::Failed {
                error: error.into(),
            }),
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
            "prompt_cache_key": self.config.prompt_cache_key.debug_file_stem(),
            "sequence": sequence,
            "kind": "request",
            "backend": "responses",
            "transport": "websocket",
            "model": &body.model,
            "body": body,
        });
        if let Err(error) = self.debug_write_json(sequence, "request", &metadata) {
            tracing::warn!(
                prompt_cache_key = %self.config.prompt_cache_key.debug_file_stem(),
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
            "prompt_cache_key": self.config.prompt_cache_key.debug_file_stem(),
            "sequence": sequence,
            "kind": "response",
            "backend": "responses",
            "transport": "websocket",
            "error": error,
            "raw_events": raw_events,
        });
        if let Err(error) = self.debug_write_json(sequence, "response", &metadata) {
            tracing::warn!(
                prompt_cache_key = %self.config.prompt_cache_key.debug_file_stem(),
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
        let path = dir.join(debug_file_name(
            self.config.prompt_cache_key,
            sequence,
            kind,
        ));
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
        let auth = self.config.inference.auth().clone();
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
                    self.config
                        .prompt_cache_key
                        .to_wire_uuid(&self.config.base_url, resolved.client_secret)
                        .to_string()
                });
            let request = ws::build_ws_request(&self.config, thread_id.as_deref(), &resolved)?;
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
            .field("base_url", &self.config.base_url)
            .field("inference", &self.config.inference)
            .field("mode", &self.config.mode)
            .field("responses_config", &self.config.responses_config)
            .field("prompt_cache_key", &self.config.prompt_cache_key)
            .finish()
    }
}
