//! Deterministic provider-protocol server for full-stack Rho QA.
//!
//! Only the model is fake: callers use the same HTTP and WebSocket protocols
//! as production and can run the real agent host, agent loop, tools, and GUI.

pub const REAL_TOOL_ROUNDS: usize = 100;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use axum::Router;
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Json, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::serve::ListenerExt as _;
use bytes::Bytes;
use clap::ValueEnum;
use futures_util::{SinkExt as _, StreamExt as _};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::watch;

/// Whether event pacing uses Tokio's clock or sends events without waiting.
/// Tokio timing remains deterministic when a test starts paused and advances
/// virtual time; the binary opts into the same schedule on the wall clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimingMode {
    Immediate,
    Timed,
}

/// A deterministic, protocol-level behavior selected for a fake-model run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Scenario {
    /// Corpus-shaped normal traffic with the configured low-rate faults.
    #[default]
    Baseline,
    /// OpenAI: 100 sequential real shell commands, each output validated before
    /// continuing.
    RealToolRounds,
    /// Agent2: a deterministic status and human-visible chat reply from exec.
    Agent2Chat,
    /// Return a rate limit, then an overload, then allow retries to succeed.
    RateLimit,
    /// End the first stream during a text delta, then allow retries to succeed.
    StreamCut,
    /// Emit approximately one word-sized text chunk every 300 ms when timed.
    SlowTrickle,
    /// Cycle through a documented synthetic 100-result heavy-tail population.
    HugeToolOutput,
    /// Emit forty valid tool calls in one response.
    FortyToolCalls,
    /// Emit encrypted reasoning followed by a compaction item.
    ReasoningCompaction,
    /// Ask a question without invoking a tool, then end the turn.
    ClarifyingQuestion,
}

impl Scenario {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::RealToolRounds => "real-tool-rounds",
            Self::Agent2Chat => "agent2-chat",
            Self::RateLimit => "rate-limit",
            Self::StreamCut => "stream-cut",
            Self::SlowTrickle => "slow-trickle",
            Self::HugeToolOutput => "huge-tool-output",
            Self::FortyToolCalls => "forty-tool-calls",
            Self::ReasoningCompaction => "reasoning-compaction",
            Self::ClarifyingQuestion => "clarifying-question",
        }
    }
}

#[derive(Clone, Debug)]
pub struct StreamTiming {
    pub mode: TimingMode,
    pub first_token: Duration,
    pub between_chunks: Duration,
    pub burst_every: u32,
    pub stall_every: u32,
    pub stall: Duration,
}

impl Default for StreamTiming {
    fn default() -> Self {
        Self {
            mode: TimingMode::Immediate,
            first_token: Duration::from_millis(180),
            between_chunks: Duration::from_millis(18),
            burst_every: 4,
            stall_every: 19,
            stall: Duration::from_millis(650),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalOutcome {
    Complete,
    RateLimit,
    UsageLimit,
    Overloaded,
    Disconnect,
}

#[derive(Clone, Debug)]
pub struct PersonaDistribution {
    pub min_chunk_bytes: usize,
    pub max_chunk_bytes: usize,
    pub large_tool_body_bytes: usize,
    /// Calls emitted together when a persona chooses tools. Keep this at one
    /// for corpus-shaped traffic; raise it deliberately to stress batching.
    pub parallel_tool_calls: usize,
    pub forced_outcome: Option<TerminalOutcome>,
    /// Per-ten-thousand rates, sampled deterministically per request.
    pub rate_limit_bps: u16,
    pub usage_limit_bps: u16,
    pub overload_bps: u16,
    pub disconnect_bps: u16,
}

impl Default for PersonaDistribution {
    fn default() -> Self {
        Self {
            min_chunk_bytes: 12,
            max_chunk_bytes: 96,
            large_tool_body_bytes: 13_097,
            parallel_tool_calls: 1,
            forced_outcome: None,
            rate_limit_bps: 30,
            usage_limit_bps: 10,
            overload_bps: 30,
            disconnect_bps: 30,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FakeModelConfig {
    pub seed: u64,
    pub bind: SocketAddr,
    pub scenario: Scenario,
    pub real_tool_rounds: usize,
    pub timing: StreamTiming,
    pub distribution: PersonaDistribution,
}

impl FakeModelConfig {
    pub fn seeded(seed: u64) -> Self {
        Self {
            seed,
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            scenario: Scenario::Baseline,
            real_tool_rounds: REAL_TOOL_ROUNDS,
            timing: StreamTiming::default(),
            distribution: PersonaDistribution::default(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct MetricsSnapshot {
    pub requests: u64,
    pub completed_turns: u64,
    pub bytes_streamed: u64,
    pub active_requests: u64,
    pub peak_active_requests: u64,
    pub max_input_tool_output_bytes: u64,
    /// First request admission through latest completion (or now while active).
    pub request_window_us: u64,
    /// Union of request servicing intervals, not CPU time.
    pub busy_us: u64,
    /// No active model request within the request window.
    pub idle_us: u64,
    /// Most recent idle intervals; real-tool proofs retain their entire run.
    pub idle_gaps_us: Vec<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct RequestOrdinal(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ConversationKey(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum ProviderProtocol {
    OpenAiResponses,
    AnthropicMessages,
}

/// One boundary observation. Tests drain these between generated actions so
/// bytes from a turn are attributed to the action that caused them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Observation {
    pub request: RequestOrdinal,
    pub conversation: ConversationKey,
    pub protocol: ProviderProtocol,
    pub event_type: String,
    pub bytes: usize,
}

#[derive(Default)]
struct Metrics {
    requests: AtomicU64,
    completed_turns: AtomicU64,
    bytes_streamed: AtomicU64,
    active_requests: AtomicU64,
    peak_active_requests: AtomicU64,
    max_input_tool_output_bytes: AtomicU64,
    activity: Mutex<Activity>,
    gap_limit: usize,
}
#[derive(Default)]
struct Activity {
    first: Option<Instant>,
    last: Option<Instant>,
    busy_since: Option<Instant>,
    active: u64,
    busy: Duration,
    gaps: std::collections::VecDeque<u64>,
}

type ResponseTools = std::collections::VecDeque<(String, Vec<ProviderTool>)>;

#[derive(Clone)]
struct AppState {
    config: Arc<FakeModelConfig>,
    next_request: Arc<AtomicU64>,
    metrics: Arc<Metrics>,
    observations: Arc<Mutex<Vec<Observation>>>,
    response_tools: Arc<Mutex<ResponseTools>>,
}

/// A running fake server. Dropping it requests shutdown; `shutdown` also waits
/// for the listener task and should be preferred by tests.
pub struct FakeModel {
    address: SocketAddr,
    metrics: Arc<Metrics>,
    observations: Arc<Mutex<Vec<Observation>>>,
    stop: watch::Sender<bool>,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl FakeModel {
    pub async fn start(config: FakeModelConfig) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(config.bind)
            .await
            .with_context(|| format!("bind fake model on {}", config.bind))?;
        let address = listener.local_addr()?;
        anyhow::ensure!(config.real_tool_rounds > 0, "rounds must be positive");
        let metrics = Arc::new(Metrics {
            gap_limit: config.real_tool_rounds.saturating_add(2).max(256),
            ..Default::default()
        });
        let observations = Arc::new(Mutex::new(Vec::new()));
        let state = AppState {
            config: Arc::new(config),
            next_request: Arc::new(AtomicU64::new(0)),
            metrics: metrics.clone(),
            observations: observations.clone(),
            response_tools: Arc::default(),
        };
        let app = Router::new()
            .route("/health", get(|| async { "ok" }))
            .route("/metrics", get(metrics_handler))
            .route(
                "/backend-api/codex/responses",
                get(openai_ws).post(openai_http),
            )
            .route("/v1/messages", post(anthropic_messages))
            .with_state(state);
        let (stop, mut stopped) = watch::channel(false);
        let task = tokio::spawn(async move {
            axum::serve(
                listener.tap_io(|socket| {
                    // Immediate chunks must not wait for Nagle/delayed-ACK batching.
                    socket
                        .set_nodelay(true)
                        .expect("set fake-model TCP_NODELAY");
                }),
                app,
            )
            .with_graceful_shutdown(async move {
                while !*stopped.borrow() && stopped.changed().await.is_ok() {}
            })
            .await
            .context("serve fake model")
        });
        Ok(Self {
            address,
            metrics,
            observations,
            stop,
            task: Some(task),
        })
    }

    /// Base URL accepted by `rho-inference` (the endpoint suffix is appended by
    /// that client).
    pub fn openai_base_url(&self) -> String {
        format!("http://{}/backend-api", self.address)
    }

    /// Value to use for Claude Code's `ANTHROPIC_BASE_URL`.
    pub fn anthropic_base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn metrics(&self) -> MetricsSnapshot {
        snapshot(&self.metrics)
    }

    pub fn drain_observations(&self) -> Vec<Observation> {
        std::mem::take(&mut *self.observations.lock().expect("observation lock"))
    }

    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        let _ = self.stop.send(true);
        self.task
            .take()
            .expect("server task")
            .await
            .context("join fake model")?
    }
}

impl Drop for FakeModel {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

async fn metrics_handler(State(state): State<AppState>) -> Json<MetricsSnapshot> {
    Json(snapshot(&state.metrics))
}

fn snapshot(metrics: &Metrics) -> MetricsSnapshot {
    let activity = metrics.activity.lock().unwrap();
    let end = if activity.active > 0 {
        Some(Instant::now())
    } else {
        activity.last
    };
    let window = activity
        .first
        .zip(end)
        .map(|(start, end)| end - start)
        .unwrap_or_default();
    let busy = activity.busy
        + activity
            .busy_since
            .map(|start| end.unwrap() - start)
            .unwrap_or_default();
    MetricsSnapshot {
        idle_gaps_us: activity.gaps.iter().copied().collect(),
        request_window_us: window.as_micros() as u64,
        busy_us: busy.as_micros() as u64,
        idle_us: window.saturating_sub(busy).as_micros() as u64,
        requests: metrics.requests.load(Ordering::Relaxed),
        completed_turns: metrics.completed_turns.load(Ordering::Relaxed),
        bytes_streamed: metrics.bytes_streamed.load(Ordering::Relaxed),
        active_requests: metrics.active_requests.load(Ordering::Relaxed),
        peak_active_requests: metrics.peak_active_requests.load(Ordering::Relaxed),
        max_input_tool_output_bytes: metrics.max_input_tool_output_bytes.load(Ordering::Relaxed),
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiEnvelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(flatten)]
    request: OpenAiRequest,
}

#[derive(Clone, Debug, Default, Deserialize)]
struct OpenAiRequest {
    #[serde(default)]
    model: String,
    #[serde(default)]
    input: Vec<Value>,
    #[serde(default)]
    tools: Vec<ProviderTool>,
    #[serde(default)]
    previous_response_id: Option<String>,
    #[serde(default)]
    prompt_cache_key: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ProviderTool {
    #[serde(rename = "type")]
    kind: String,
    name: String,
}

async fn openai_ws(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| serve_openai_socket(socket, state))
}

async fn serve_openai_socket(mut socket: WebSocket, state: AppState) {
    while let Some(Ok(message)) = socket.next().await {
        let Message::Text(text) = message else {
            continue;
        };
        let Ok(envelope) = serde_json::from_str::<OpenAiEnvelope>(&text) else {
            let _ = socket.send(Message::Text(json!({"type":"error","error":{"message":"invalid response.create body","code":"invalid_request_error"}}).to_string().into())).await;
            continue;
        };
        if envelope.kind != "response.create" {
            continue;
        }
        let (request_number, request_guard) = begin_request(&state.metrics, &state.next_request);
        observe_input_tool_outputs(&state.metrics, &envelope.request);
        let events = openai_turn(&state, request_number, &envelope.request);
        for (index, event) in events.into_iter().enumerate() {
            delay(&state.config, index, &event).await;
            let bytes = event.to_string();
            state
                .metrics
                .bytes_streamed
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
            observe(
                &state,
                request_number,
                openai_conversation(&envelope.request),
                ProviderProtocol::OpenAiResponses,
                &event,
                bytes.len(),
            );
            if socket.send(Message::Text(bytes.into())).await.is_err() {
                return;
            }
            if outcome(&state.config, request_number) == TerminalOutcome::Disconnect
                && ((state.config.scenario == Scenario::StreamCut && is_text_delta(&event))
                    || (state.config.scenario != Scenario::StreamCut && index >= 2))
            {
                let _ = socket.close().await;
                end_request(request_guard, false);
                return;
            }
        }
        end_request(
            request_guard,
            outcome(&state.config, request_number) == TerminalOutcome::Complete,
        );
    }
}

async fn openai_http(
    State(state): State<AppState>,
    Json(request): Json<OpenAiRequest>,
) -> Response {
    let (request_number, request_guard) = begin_request(&state.metrics, &state.next_request);
    observe_input_tool_outputs(&state.metrics, &request);
    let terminal = outcome(&state.config, request_number);
    if terminal != TerminalOutcome::Complete && terminal != TerminalOutcome::Disconnect {
        end_request(request_guard, false);
        return openai_error_response(terminal, request_number);
    }
    let events = openai_turn(&state, request_number, &request);
    let config = state.config.clone();
    let metrics = state.metrics.clone();
    let observed_state = state.clone();
    let stream = async_stream::stream! {
        for (index, event) in events.into_iter().enumerate() {
            delay(&config, index, &event).await;
            let cut = terminal == TerminalOutcome::Disconnect
                && config.scenario == Scenario::StreamCut
                && is_text_delta(&event);
            let mut bytes = format!("data: {event}\n\n");
            if cut {
                bytes.truncate(bytes.len() * 3 / 4);
            }
            metrics.bytes_streamed.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            observe(&observed_state, request_number, openai_conversation(&request), ProviderProtocol::OpenAiResponses, &event, bytes.len());
            yield Ok::<Bytes, std::io::Error>(Bytes::from(bytes));
            if cut {
                yield Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "seeded stream cut"));
                end_request(request_guard, false);
                return;
            }
            if terminal == TerminalOutcome::Disconnect && index >= 2 {
                break;
            }
        }
        end_request(request_guard, terminal == TerminalOutcome::Complete);
    };
    let mut response = Body::from_stream(stream).into_response();
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    response
}

fn observe_input_tool_outputs(metrics: &Metrics, request: &OpenAiRequest) {
    let max = request
        .input
        .iter()
        .filter(|item| {
            matches!(
                item.get("type").and_then(Value::as_str),
                Some("function_call_output" | "custom_tool_call_output")
            )
        })
        .filter_map(|item| item.get("output"))
        .map(|output| match output {
            Value::String(text) => text.len(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .map(str::len)
                .sum(),
            _ => 0,
        })
        .max()
        .unwrap_or(0);
    metrics
        .max_input_tool_output_bytes
        .fetch_max(max as u64, Ordering::Relaxed);
}

fn openai_turn(state: &AppState, request_number: u64, request: &OpenAiRequest) -> Vec<Value> {
    let response_id = format!("resp_fake_{:016x}_{request_number}", state.config.seed);
    let terminal = outcome(&state.config, request_number);
    if terminal != TerminalOutcome::Complete && terminal != TerminalOutcome::Disconnect {
        let (message, code) = match terminal {
            TerminalOutcome::RateLimit => {
                ("Rate limit reached for requests", "rate_limit_exceeded")
            }
            TerminalOutcome::UsageLimit => (
                "Usage limit reached for this organization",
                "usage_limit_reached",
            ),
            TerminalOutcome::Overloaded => ("The server is overloaded", "server_is_overloaded"),
            _ => unreachable!(),
        };
        return vec![
            json!({"type":"response.created","response":{"id":response_id}}),
            json!({"type":"response.failed","response":{"id":response_id,"error":{"type":error_type(terminal),"message":message,"code":code}}}),
        ];
    }

    let mut events = vec![json!({"type":"response.created","response":{"id":response_id}})];
    let has_tool_result = request.input.iter().any(|item| {
        matches!(
            item.get("type").and_then(Value::as_str),
            Some("function_call_output" | "custom_tool_call_output")
        )
    });
    let asks_compaction = request
        .input
        .iter()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("compaction_trigger"));
    let mut tools = request_tools(request);
    {
        let mut remembered = state.response_tools.lock().unwrap();
        if tools.is_empty()
            && let Some(previous) = &request.previous_response_id
            && let Some((_, inherited)) = remembered.iter().find(|(id, _)| id == previous)
        {
            tools = inherited.clone();
        }
        if remembered.len() == 256 {
            remembered.pop_front();
        }
        remembered.push_back((response_id.clone(), tools.clone()));
    }
    if state.config.scenario == Scenario::Agent2Chat {
        if let Some(tool) = tools.iter().find(|tool| tool.name == "exec") {
            append_tool_call(
                &mut events,
                state,
                request_number,
                0,
                tool,
                Some(
                    "human.status('ready')\nhuman.send('fake model reply')\nawait human.reply()"
                        .into(),
                ),
            );
        } else {
            events.push(
                json!({"type":"response.failed","response":{"id":response_id,
                "error":{"type":"invalid_request_error","code":"invalid_request_error",
                "message":"agent2-chat requires exec"}}}),
            );
            return events;
        }
    } else if state.config.scenario == Scenario::RealToolRounds
        && (!tools.is_empty() || has_tool_result)
    {
        let outputs: Vec<_> = request
            .input
            .iter()
            .filter(|item| {
                matches!(
                    item.get("type").and_then(Value::as_str),
                    Some("function_call_output" | "custom_tool_call_output")
                )
            })
            .collect();
        let completed = outputs
            .last()
            .and_then(|item| {
                let output = item.get("output")?.to_string();
                output
                    .rsplit_once("rho-e2e-step-")?
                    .1
                    .split_once(":ok")?
                    .0
                    .parse::<usize>()
                    .ok()
                    .filter(|step| (1..=state.config.real_tool_rounds).contains(step))
            })
            .unwrap_or(0);
        if (!outputs.is_empty() && completed == 0) || tools.iter().all(|tool| tool.name != "exec") {
            events.push(
                json!({"type":"response.failed","response":{"id":response_id,
                "error":{"type":"invalid_request_error","code":"invalid_request_error",
                "message":"real-tool-rounds requires exec and a successful latest tool output"}}}),
            );
            return events;
        }
        if completed < state.config.real_tool_rounds {
            let step = completed + 1;
            let path = format!(".rho-fake-rounds-{}", state.config.seed);
            let init = if step == 1 {
                format!("Path({path:?}).write_text('0')\n")
            } else {
                String::new()
            };
            let cmd = format!(
                "test \"$(cat {path})\" = {completed} && printf {step} > {path} && printf 'rho-e2e-step-{step}:ok\\n'"
            );
            let source = format!("{init}await command({cmd:?})");
            let tool = tools.iter().find(|tool| tool.name == "exec").unwrap();
            append_tool_call(&mut events, state, request_number, 0, tool, Some(source));
        } else {
            append_text(&mut events, state, request_number, request, 0);
        }
    } else if state.config.scenario == Scenario::ReasoningCompaction {
        append_reasoning(&mut events, request_number, 0);
        append_compaction(&mut events, state, request_number, 1);
    } else if asks_compaction {
        append_compaction(&mut events, state, request_number, 0);
    } else if matches!(
        state.config.scenario,
        Scenario::ClarifyingQuestion | Scenario::SlowTrickle | Scenario::StreamCut
    ) {
        append_text(&mut events, state, request_number, request, 0);
    } else if !tools.is_empty()
        && (!has_tool_result
            || (matches!(
                state.config.scenario,
                Scenario::Baseline | Scenario::RateLimit
            ) && !mix(state.config.seed ^ request_number).is_multiple_of(20)))
    {
        append_reasoning(&mut events, request_number, 0);
        let tool = if matches!(
            state.config.scenario,
            Scenario::HugeToolOutput | Scenario::FortyToolCalls
        ) {
            tools
                .iter()
                .find(|tool| tool.name == "exec")
                .unwrap_or(&tools[0])
        } else {
            &tools[(mix(state.config.seed ^ request_number) as usize) % tools.len()]
        };
        let call_count = match state.config.scenario {
            Scenario::HugeToolOutput => 100,
            Scenario::FortyToolCalls => 40,
            _ => state.config.distribution.parallel_tool_calls.max(1),
        };
        for offset in 0..call_count {
            append_tool_call(&mut events, state, request_number, offset + 1, tool, None);
        }
    } else {
        append_reasoning(&mut events, request_number, 0);
        append_text(&mut events, state, request_number, request, 1);
    }
    events.push(json!({"type":"response.completed","response":{"id":response_id,"usage":{"input_tokens":request.input.len() * 31 + 17,"input_tokens_details":{"cached_tokens":if request.previous_response_id.is_some(){19}else{0}},"output_tokens":events.len() * 7 + 3}}}));
    events
}

fn request_tools(request: &OpenAiRequest) -> Vec<ProviderTool> {
    if !request.tools.is_empty() {
        return request.tools.clone();
    }
    request
        .input
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("additional_tools"))
        .filter_map(|item| item.get("tools").and_then(Value::as_array))
        .flatten()
        .filter_map(|tool| serde_json::from_value(tool.clone()).ok())
        .collect()
}

fn append_compaction(
    events: &mut Vec<Value>,
    state: &AppState,
    request_number: u64,
    output_index: usize,
) {
    let id = format!("cmp_{request_number}");
    events.push(json!({"type":"response.output_item.added","output_index":output_index,"item":{"type":"compaction","id":id}}));
    events.push(json!({"type":"response.output_item.done","output_index":output_index,"item":{"type":"compaction","id":id,"encrypted_content":format!("fake-compaction-{}-{request_number}",state.config.seed)}}));
}

fn append_text(
    events: &mut Vec<Value>,
    state: &AppState,
    request_number: u64,
    request: &OpenAiRequest,
    output_index: usize,
) {
    let text = scenario_text(state, request_number, request);
    let item_id = format!("msg_fake_{request_number}");
    events.push(json!({"type":"response.output_item.added","output_index":output_index,"item":{"type":"message","id":item_id,"phase":"final_answer"}}));
    for chunk in chunks(&text, &state.config, request_number) {
        events.push(
            json!({"type":"response.output_text.delta","output_index":output_index,"delta":chunk}),
        );
    }
    events.push(json!({"type":"response.output_item.done","output_index":output_index,"item":{"type":"message","id":item_id,"phase":"final_answer"}}));
}

fn append_tool_call(
    events: &mut Vec<Value>,
    state: &AppState,
    request_number: u64,
    output_index: usize,
    tool: &ProviderTool,
    source: Option<String>,
) {
    let custom = tool.kind == "custom";
    let item_type = if custom {
        "custom_tool_call"
    } else {
        "function_call"
    };
    let delta_type = if custom {
        "response.custom_tool_call_input.delta"
    } else {
        "response.function_call_arguments.delta"
    };
    let id = if custom {
        format!("ctc_fake_{request_number}_{output_index}")
    } else {
        format!("fc_fake_{request_number}_{output_index}")
    };
    let call_id = format!("call_fake_{request_number}_{output_index}");
    let arguments = source.unwrap_or_else(|| {
        tool_arguments(
            tool,
            state.config.distribution.large_tool_body_bytes,
            if state.config.scenario == Scenario::HugeToolOutput {
                output_index as u64 - 1
            } else {
                request_number ^ output_index as u64
            },
            state.config.scenario,
            state.config.seed,
        )
    });
    events.push(json!({"type":"response.output_item.added","output_index":output_index,"item":{"type":item_type,"id":id,"call_id":call_id,"name":tool.name}}));
    for chunk in chunks(&arguments, &state.config, request_number) {
        events.push(
            json!({"type":delta_type,"output_index":output_index,"item_id":id,"delta":chunk}),
        );
    }
    let argument_field = if custom { "input" } else { "arguments" };
    events.push(
        json!({"type":"response.output_item.done","output_index":output_index,"item":{
            "type":item_type,"id":id,"call_id":call_id,"name":tool.name,argument_field:arguments
        }}),
    );
}

fn append_reasoning(events: &mut Vec<Value>, request_number: u64, output_index: usize) {
    let id = format!("rs_fake_{request_number}");
    events.push(json!({"type":"response.output_item.added","output_index":output_index,"item":{"type":"reasoning","id":id}}));
    events.push(json!({"type":"response.reasoning_summary_text.delta","output_index":output_index,"summary_index":0,"delta":"Inspecting the current state and choosing the next bounded action."}));
    events.push(json!({"type":"response.output_item.done","output_index":output_index,"item":{"type":"reasoning","id":id,"encrypted_content":format!("fake-reasoning-{request_number}"),"summary":[{"type":"summary_text","text":"Inspecting the current state and choosing the next bounded action."}]}}));
}

fn tool_arguments(
    tool: &ProviderTool,
    size: usize,
    request_number: u64,
    scenario: Scenario,
    seed: u64,
) -> String {
    if tool.kind == "custom" || tool.name == "exec" {
        let body = "x".repeat(if scenario == Scenario::FortyToolCalls {
            0
        } else {
            size.saturating_sub(180)
        });
        let result_bytes = synthetic_result_bytes(scenario, seed, request_number);
        format!("const payload_{request_number} = {body:?};\ntext('x'.repeat({result_bytes}));")
    } else {
        json!({"cmd":format!("python3 - <<'PY'\nprint('x' * {size})\nPY"),"max_output_tokens":10000}).to_string()
    }
}

// This is deliberately a synthetic approximation, not an empirical fit. Its
// 100-value population is 89×227, 1×13,097, 9×22,328, and 1×170,448 bytes:
// exactly p50 227, mean 4,047, p90 13,097, and max 170,448.
fn synthetic_result_bytes(scenario: Scenario, seed: u64, request_number: u64) -> usize {
    if scenario == Scenario::HugeToolOutput {
        match seed.wrapping_add(request_number) % 100 {
            0..=88 => 227,
            89 => 13_097,
            90..=98 => 22_328,
            _ => 170_448,
        }
    } else {
        match mix(request_number) % 10_000 {
            0..=8_749 => 227,
            8_750..=9_849 => 13_097,
            _ => 170_448,
        }
    }
}

fn scenario_text(state: &AppState, request_number: u64, request: &OpenAiRequest) -> String {
    match state.config.scenario {
        Scenario::RealToolRounds => {
            format!(
                "Verified {} sequential real shell commands and their outputs.",
                state.config.real_tool_rounds
            )
        }
        Scenario::ClarifyingQuestion => {
            "Could you clarify which behavior you want me to implement?".to_owned()
        }
        Scenario::SlowTrickle => (0..200).map(|index| format!("token-{index} ")).collect(),
        _ => persona_text(state.config.seed, request_number, request),
    }
}

fn persona_text(seed: u64, request_number: u64, request: &OpenAiRequest) -> String {
    let lengths = [227usize, 227, 227, 620, 2_100, 4_047];
    let length = lengths[(mix(seed.wrapping_add(request_number)) as usize) % lengths.len()];
    let prefix = format!(
        "Completed deterministic turn {request_number} for model {}. The real agent loop supplied {} context items.\n",
        request.model,
        request.input.len()
    );
    let line = "Observed state is consistent; continuing through the real agent host, tools, journal, story, and GUI wire.\n";
    let mut text = prefix;
    while text.len() < length {
        text.push_str(line);
    }
    text.truncate(length);
    text
}

fn chunks(text: &str, config: &FakeModelConfig, request_number: u64) -> Vec<String> {
    if config.scenario == Scenario::SlowTrickle {
        return text
            .split_inclusive(char::is_whitespace)
            .map(str::to_owned)
            .collect();
    }
    let min = config.distribution.min_chunk_bytes.max(1);
    let max = config.distribution.max_chunk_bytes.max(min);
    let mut at = 0;
    let mut result = Vec::new();
    while at < text.len() {
        let wanted =
            min + (mix(config.seed ^ request_number ^ at as u64) as usize % (max - min + 1));
        let mut end = (at + wanted).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        result.push(text[at..end].to_owned());
        at = end;
    }
    result
}

fn outcome(config: &FakeModelConfig, request_number: u64) -> TerminalOutcome {
    match config.scenario {
        Scenario::RateLimit => {
            return match request_number {
                0 => TerminalOutcome::RateLimit,
                1 => TerminalOutcome::Overloaded,
                _ => TerminalOutcome::Complete,
            };
        }
        Scenario::StreamCut => {
            return if request_number == 0 {
                TerminalOutcome::Disconnect
            } else {
                TerminalOutcome::Complete
            };
        }
        Scenario::Baseline => {}
        _ => return TerminalOutcome::Complete,
    }
    if let Some(value) = config.distribution.forced_outcome {
        return value;
    }
    let draw = (mix(config.seed ^ request_number) % 10_000) as u16;
    let d = &config.distribution;
    if draw < d.rate_limit_bps {
        TerminalOutcome::RateLimit
    } else if draw < d.rate_limit_bps + d.usage_limit_bps {
        TerminalOutcome::UsageLimit
    } else if draw < d.rate_limit_bps + d.usage_limit_bps + d.overload_bps {
        TerminalOutcome::Overloaded
    } else if draw < d.rate_limit_bps + d.usage_limit_bps + d.overload_bps + d.disconnect_bps {
        TerminalOutcome::Disconnect
    } else {
        TerminalOutcome::Complete
    }
}

fn mix(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e3779b97f4a7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
    value ^ (value >> 31)
}

async fn delay(config: &FakeModelConfig, event_index: usize, event: &Value) {
    if config.timing.mode == TimingMode::Immediate {
        return;
    }
    if config.scenario == Scenario::SlowTrickle {
        if is_text_delta(event) {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        return;
    }
    let timing = &config.timing;
    let duration = if event_index == 0 {
        timing.first_token
    } else if timing.stall_every != 0 && event_index.is_multiple_of(timing.stall_every as usize) {
        timing.stall
    } else if timing.burst_every != 0 && event_index.is_multiple_of(timing.burst_every as usize) {
        Duration::ZERO
    } else {
        timing.between_chunks
    };
    tokio::time::sleep(duration).await;
}

fn is_text_delta(event: &Value) -> bool {
    event.get("type").and_then(Value::as_str) == Some("response.output_text.delta")
        || event
            .get("delta")
            .and_then(|delta| delta.get("type"))
            .and_then(Value::as_str)
            == Some("text_delta")
}

fn error_type(outcome: TerminalOutcome) -> &'static str {
    match outcome {
        TerminalOutcome::RateLimit => "rate_limit_error",
        TerminalOutcome::UsageLimit => "usage_limit_error",
        TerminalOutcome::Overloaded => "overloaded_error",
        _ => unreachable!("only HTTP error outcomes have error types"),
    }
}

fn openai_error_response(outcome: TerminalOutcome, request_number: u64) -> Response {
    let (status, message, code) = match outcome {
        TerminalOutcome::RateLimit => (
            StatusCode::TOO_MANY_REQUESTS,
            "Rate limit reached for requests",
            "rate_limit_exceeded",
        ),
        TerminalOutcome::UsageLimit => (
            StatusCode::PAYMENT_REQUIRED,
            "Usage limit reached for this organization",
            "usage_limit_reached",
        ),
        TerminalOutcome::Overloaded => (
            StatusCode::SERVICE_UNAVAILABLE,
            "The server is overloaded",
            "server_is_overloaded",
        ),
        _ => unreachable!("complete streams do not produce HTTP errors"),
    };
    let mut response = (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": error_type(outcome),
                "param": null,
                "code": code,
            },
            "request_id": format!("req_fake_{request_number}"),
        })),
    )
        .into_response();
    response
        .headers_mut()
        .insert("retry-after", HeaderValue::from_static("1"));
    response
}

fn begin_request(metrics: &Arc<Metrics>, ordinal: &AtomicU64) -> (u64, RequestGuard) {
    let mut activity = metrics.activity.lock().unwrap();
    let now = Instant::now();
    activity.first.get_or_insert(now);
    if activity.active == 0 {
        if let Some(last) = activity.last {
            if activity.gaps.len() == metrics.gap_limit.max(256) {
                activity.gaps.pop_front();
            }
            activity.gaps.push_back((now - last).as_micros() as u64);
        }
        activity.busy_since = Some(now);
    }
    activity.active += 1;
    metrics.requests.fetch_add(1, Ordering::Relaxed);
    let active = metrics.active_requests.fetch_add(1, Ordering::Relaxed) + 1;
    metrics
        .peak_active_requests
        .fetch_max(active, Ordering::Relaxed);
    (
        ordinal.fetch_add(1, Ordering::Relaxed),
        RequestGuard {
            metrics: metrics.clone(),
            complete: false,
        },
    )
}

struct RequestGuard {
    metrics: Arc<Metrics>,
    complete: bool,
}

fn end_request(mut request: RequestGuard, complete: bool) {
    request.complete = complete;
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        let metrics = &self.metrics;
        let complete = self.complete;
        let mut activity = metrics.activity.lock().unwrap();
        let now = Instant::now();
        activity.active -= 1;
        activity.last = Some(now);
        if activity.active == 0 {
            let start = activity.busy_since.take().unwrap();
            activity.busy += now - start;
        }
        metrics.active_requests.fetch_sub(1, Ordering::Relaxed);
        if complete {
            metrics.completed_turns.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn observe(
    state: &AppState,
    request: u64,
    conversation: ConversationKey,
    protocol: ProviderProtocol,
    event: &Value,
    bytes: usize,
) {
    state
        .observations
        .lock()
        .expect("observation lock")
        .push(Observation {
            request: RequestOrdinal(request),
            conversation,
            protocol,
            event_type: event
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            bytes,
        });
}

#[derive(Clone, Debug, Deserialize)]
struct AnthropicRequest {
    #[serde(default)]
    model: String,
    #[serde(default)]
    messages: Vec<Value>,
    #[serde(default)]
    tools: Vec<AnthropicTool>,
    #[serde(default)]
    stream: bool,
    #[serde(default)]
    metadata: Option<Value>,
}

#[derive(Clone, Debug, Deserialize)]
struct AnthropicTool {
    name: String,
}

async fn anthropic_messages(
    State(state): State<AppState>,
    Json(request): Json<AnthropicRequest>,
) -> Response {
    let (number, request_guard) = begin_request(&state.metrics, &state.next_request);
    let terminal = outcome(&state.config, number);
    if terminal != TerminalOutcome::Complete && terminal != TerminalOutcome::Disconnect {
        end_request(request_guard, false);
        let (status, kind, message) = match terminal {
            TerminalOutcome::RateLimit => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_error",
                "This request would exceed your rate limit.",
            ),
            TerminalOutcome::UsageLimit => (
                StatusCode::PAYMENT_REQUIRED,
                "billing_error",
                "Usage limit reached.",
            ),
            TerminalOutcome::Overloaded => (
                StatusCode::from_u16(529).expect("valid Anthropic overload status"),
                "overloaded_error",
                "Overloaded",
            ),
            _ => unreachable!(),
        };
        let mut response = (status, Json(json!({"type":"error","error":{"type":kind,"message":message},"request_id":format!("req_fake_{number}")}))).into_response();
        response
            .headers_mut()
            .insert("retry-after", HeaderValue::from_static("1"));
        return response;
    }
    if !request.stream {
        end_request(request_guard, true);
        let text = scenario_text(&state, number, &OpenAiRequest::default());
        return Json(json!({"id":format!("msg_fake_{number}"),"type":"message","role":"assistant","model":request.model,"content":[{"type":"text","text":text}],"stop_reason":"end_turn","usage":{"input_tokens":17,"output_tokens":31}})).into_response();
    }
    let events = anthropic_turn(&state, number, &request);
    let config = state.config.clone();
    let metrics = state.metrics.clone();
    let observed_state = state.clone();
    let stream = async_stream::stream! {
        for (index, (name, event)) in events.into_iter().enumerate() {
            delay(&config,index,&event).await;
            let cut = terminal == TerminalOutcome::Disconnect
                && config.scenario == Scenario::StreamCut
                && is_text_delta(&event);
            let mut bytes = format!("event: {name}\ndata: {event}\n\n");
            if cut {
                bytes.truncate(bytes.len() * 3 / 4);
            }
            metrics.bytes_streamed.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            observe(&observed_state, number, anthropic_conversation(&request), ProviderProtocol::AnthropicMessages, &event, bytes.len());
            yield Ok::<Bytes,std::io::Error>(Bytes::from(bytes));
            if cut {
                yield Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "seeded stream cut"));
                end_request(request_guard, false);
                return;
            }
            if terminal == TerminalOutcome::Disconnect && index >= 2 {
                break;
            }
        }
        end_request(request_guard, terminal == TerminalOutcome::Complete);
    };
    let mut response = Body::from_stream(stream).into_response();
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    response
}

fn openai_conversation(request: &OpenAiRequest) -> ConversationKey {
    ConversationKey(
        request
            .prompt_cache_key
            .clone()
            .unwrap_or_else(|| "openai-unkeyed".to_owned()),
    )
}

fn anthropic_conversation(request: &AnthropicRequest) -> ConversationKey {
    ConversationKey(
        request
            .metadata
            .as_ref()
            .and_then(|value| value.get("user_id"))
            .and_then(Value::as_str)
            .unwrap_or(&request.model)
            .to_owned(),
    )
}

fn anthropic_turn(
    state: &AppState,
    number: u64,
    request: &AnthropicRequest,
) -> Vec<(&'static str, Value)> {
    let id = format!("msg_fake_{number}");
    let mut events = vec![(
        "message_start",
        json!({"type":"message_start","message":{"id":id,"type":"message","role":"assistant","model":request.model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":request.messages.len()*31+17,"output_tokens":0}}}),
    )];
    let has_tool_result = request
        .messages
        .iter()
        .any(|message| message.to_string().contains("tool_result"));
    if !request.tools.is_empty()
        && !matches!(
            state.config.scenario,
            Scenario::ClarifyingQuestion | Scenario::SlowTrickle | Scenario::StreamCut
        )
        && ((state.config.scenario == Scenario::HugeToolOutput && number < 100) || !has_tool_result)
    {
        let tool = &request.tools[(mix(state.config.seed ^ number) as usize) % request.tools.len()];
        let call_count = if state.config.scenario == Scenario::FortyToolCalls {
            40
        } else {
            1
        };
        for index in 0..call_count {
            events.push(("content_block_start",json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":format!("toolu_fake_{number}_{index}"),"name":tool.name,"input":{}}})));
            let arguments = tool_arguments(
                &ProviderTool {
                    kind: "function".into(),
                    name: tool.name.clone(),
                },
                state.config.distribution.large_tool_body_bytes,
                number ^ index as u64,
                state.config.scenario,
                state.config.seed,
            );
            for chunk in chunks(&arguments, &state.config, number) {
                events.push(("content_block_delta",json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":chunk}})));
            }
            events.push((
                "content_block_stop",
                json!({"type":"content_block_stop","index":index}),
            ));
        }
        events.push(("message_delta",json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":71}})));
    } else {
        let text = scenario_text(state, number, &OpenAiRequest::default());
        events.push(("content_block_start",json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}})));
        events.push(("content_block_delta",json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Inspecting the current state."}})));
        events.push(("content_block_delta",json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":format!("fake-signature-{number}")}})));
        events.push((
            "content_block_stop",
            json!({"type":"content_block_stop","index":0}),
        ));
        events.push(("content_block_start",json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}})));
        for chunk in chunks(&text, &state.config, number) {
            events.push(("content_block_delta",json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":chunk}})));
        }
        events.push((
            "content_block_stop",
            json!({"type":"content_block_stop","index":1}),
        ));
        events.push(("message_delta",json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":text.len()/4}})));
    }
    events.push(("message_stop", json!({"type":"message_stop"})));
    events
}

#[cfg(test)]
mod tests;
