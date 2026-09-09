# rho-inference security and reliability context

`rho-inference` is a library crate for inference provider integrations. Its
Responses module builds request bodies from `rho-core` inference requests,
opens ChatGPT/Codex WebSockets, parses streamed inference events, and manages
file-backed OAuth credentials. Its reduced Antigravity module sends bounded
GenerateContent requests for the explicit Gemini agent mode.

## Runtime and trust boundaries

- Callers provide `InferenceRequest`, tool specs, prompt-cache keys,
  model/session configuration, and a named auth credential.
- OpenAI OAuth/token endpoints and ChatGPT/Codex WebSocket messages are remote,
  semi-trusted inputs.
- OAuth credential JSON files contain bearer and refresh tokens and must be
  treated as secrets.
- Antigravity additionally stores a Google project id and uses embedded OAuth
  application credentials to refresh its manually supplied refresh token.
- Provider debug files under the rho state directory can contain full request
  bodies, tool results, and raw provider events. They must not include auth
  headers or OAuth tokens, but should still be treated as transcript-sensitive
  data.
- Inference event JSON must not be trusted to be well-formed, ordered, complete,
  or bounded in size.

## Concurrency and resource assumptions

- Streaming uses Tokio tasks and a WebSocket pool protected by
  `tokio::sync::Mutex`.
- WebSocket turns have an event-idle timeout and keepalive pings.
- OAuth credential refresh is synchronous and is run from async inference paths
  with `spawn_blocking`; auth-management commands are owned by `run_auth_cli`.
- `Inference` owns the sole periodic ChatGPT quota poller and provider-prefixed
  database tables. Persistence and public state contain only auth settings,
  namespace names, percentages, and reset times—never OAuth tokens or provider
  account identifiers. Account identifiers remain memory-only for alias
  deduplication. Session creation and account selection never request quota.
- Explicit non-production Responses endpoints suppress the ChatGPT usage
  poller, so an isolated QA daemon cannot make a live provider side request.
- Production inference owns one route-probe task. At most once per 30 minutes,
  it sends two bounded `generate: false` Luna/default requests over each of
  three fixed candidates (ordinary DNS and two direct TCP destinations). Probe
  scoring stops at the first `codex.rate_limits` event, while the protocol is
  drained through completion before another request is sent. Results remain
  scoped to the selected account. The winning account id and selection remain
  memory-only. Route, destination IP, validated Cloudflare colo, namespace,
  success state, and raw latency samples are retained for 30 days; failures do
  not persist remote error bodies. Direct routes retain `chatgpt.com` in the
  request URI, TLS SNI, and certificate verification; only the TCP destination
  differs.
- The current automatic account selection is persisted privately and read at
  request setup by sessions, web search, and realtime. Safe public state
  deliberately omits it. Authentication failures fail their request without
  changing selection; only rate limits and user disablement trigger failover.
- WebSocket pool entries are keyed by base URL, account id, and
  prompt-cache/thread id.

## Primary risks and safeguards

- Secret leakage: OAuth files should be created in private directories and
  written with private file permissions; tests should cover Unix credential-file
  mode when available.
- Hung inference/auth operations: OAuth HTTP calls and WebSocket connect/turn
  operations need explicit timeout/cancellation behavior.
- Unbounded memory/task growth: inference streams should apply backpressure and
  stop promptly when the returned stream is dropped.
- Antigravity HTTP requests have a five-minute timeout, an 8 MiB response cap,
  bounded error text, and cancellable task ownership. Transient transport,
  429, and 5xx failures use the existing bounded retry schedule.
- Transient provider/transport stream failures (for example overload, rate
  limit, and mid-turn WebSocket loss) are retried in the active turn for up to
  eight hours with jittered Fibonacci backoff capped at 30 minutes before
  surfacing a terminal failure.
- A structured `rate_limit_exceeded` failure switches the process-wide
  selection and may replay the active request through another enabled OAuth
  account. `TemporaryFailure` lets the agent retain the failed attempt's
  partial response while starting a clean pending response. Cross-account
  replay always drops the old socket and provider response chain.
- Responses protocol drift or malformed events: event parsing should ignore
  unknown/malformed non-terminal events, surface terminal error/incomplete
  events, and preserve provider items needed for replay.
- Direct edge candidates are opportunistic, not trusted certificate pins or
  guaranteed geographic locations. Probe and connection timeouts are bounded;
  an unusable selected route is demoted immediately to DNS. Route changes do
  not cancel in-flight turns. Replacing an idle connection deliberately clears
  its connection-bound response id, so the next request safely full-replays its
  transcript.
- Antigravity rejects images, custom tools, tool updates, and compaction before
  network I/O. Remote function names and ids pass through validated `rho-core`
  types, while thought signatures persist as tagged opaque provider data.

Future changes touching credentials, WebSocket pooling, stream task lifecycle,
event parsing, prompt-cache/thread ids, or replay behavior must update this file
and add/update focused tests for the affected primary risk.
