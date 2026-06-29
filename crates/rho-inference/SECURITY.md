# rho-inference security and reliability context

`rho-inference` is a library crate for inference provider integrations. Its
current Responses module builds request bodies from `rho-core` inference
requests, opens ChatGPT/Codex WebSockets, parses streamed inference events, and
manages file-backed OAuth credentials.

## Runtime and trust boundaries

- Callers provide `InferenceRequest`, tool specs, prompt-cache keys,
  model/session configuration, and a named auth credential.
- OpenAI OAuth/token endpoints and ChatGPT/Codex WebSocket messages are remote,
  semi-trusted inputs.
- OAuth credential JSON files contain bearer and refresh tokens and must be
  treated as secrets.
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
- Responses protocol drift or malformed events: event parsing should ignore
  unknown/malformed non-terminal events, surface terminal error/incomplete
  events, and preserve provider items needed for replay.

Future changes touching credentials, WebSocket pooling, stream task lifecycle,
event parsing, prompt-cache/thread ids, or replay behavior must update this file
and add/update focused tests for the affected primary risk.
