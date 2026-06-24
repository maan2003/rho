# rho-provider-responses architecture

`rho-provider-responses` provides OpenAI Responses / ChatGPT Codex provider
support for rho. It is the boundary between the shared `rho` transcript
vocabulary and the provider-specific Responses WebSocket protocol.

## Public API boundary

The public surface is intentionally small:

- `ProviderSession` configures provider access, prompt-cache/thread behavior,
  compaction, ChatGPT/Codex auth-file selection, auth-file management helpers,
  and owns the WebSocket pool.
- `ProviderSession::stream` accepts a `rho::ProviderRequest` and returns a
  stream of `ResponsesUpdate` values ending in `ResponsesUpdate::Finished`.
- `ResponsesUpdate` is the streaming update contract consumed by `rho-agent` and
  UIs.
- Raw OAuth helpers, credential DTOs, credential files, token refresh, and
  recorded-event parsers are internal implementation details. CLI and UI crates
  should use the `ProviderSession` facade instead of importing provider auth
  plumbing directly.

The crate should not own agent policy such as tool scheduling, retries beyond
provider-chain replay, transcript persistence, terminal rendering, or CLI
configuration.

## Internal components

- `build_request.rs` converts `rho::ProviderRequest` and `ProviderSession`
  settings into the provider request JSON shape.
- `lib.rs` owns event parsing, `ResponseState`, streaming update production,
  stale `previous_response_id` fallback, and public re-exports.
- `session.rs` owns session configuration and shared WebSocket-pool state.
- `ws.rs` owns WebSocket request construction, connection checkout/release,
  event-loop timeouts/pings, and pool keying.
- `oauth.rs` owns private credential files, OAuth token exchange/refresh,
  account id extraction, and file locking behind the `ProviderSession` facade.

## Request and replay model

`ProviderRequest.input` is the source of truth for the provider request body. The
request builder:

- extracts system/developer messages into `instructions`;
- sends user/assistant/tool-call/tool-result items as Responses input items;
- encodes local tool names to provider-safe wire names and maps them back when
  parsing tool calls;
- includes encrypted reasoning provider items and compaction provider items when
  they need to be replayed;
- prefers `previous_response_id` chaining when a prior provider response block is
  usable;
- falls back to full transcript replay when the provider reports a stale or
  missing previous response;
- trims input before the latest compaction item when replaying compacted history.

Provider request bodies use `store: false`; durable transcript persistence is
owned by the agent/store layer, not by the provider service.

## Streaming and response state

`ResponseState` is the canonical in-flight accumulator for one provider turn. It
indexes text, reasoning summaries, tool calls, opaque provider items, usage, and
provider response id by provider output index. Streaming updates are emitted as
events arrive, and the final `ProviderResponse` is built from the accumulated
state.

`ResponsesUpdate::Finished` is the only successful terminal update. Consumers
should treat a stream ending before `Finished` as an error.

## WebSocket pool ownership

Each `ProviderSession` owns an `Arc<tokio::sync::Mutex<WebSocketPool>>`. Pooled
connections are keyed by base URL, optional ChatGPT account id, and
prompt-cache/thread id. Connections are checked out for a single turn, then
returned to the pool if the turn completed successfully and the connection is
still valid. Failed turns release the busy marker and do not return the failed
connection.

Runtime ownership and cancellation must remain explicit: dropping a consumer
stream or aborting the caller's task must not leave an unobserved provider turn
running in the background or return that turn's connection to the reusable pool.

## OAuth credential ownership

The provider auth file is the source of truth for persisted ChatGPT/Codex
credentials. It stores access-token, refresh-token, expiry, and optional account
id JSON under the rho auth directory or an explicit auth directory, protects
refresh/save with a sibling lock file, and writes credentials with private
filesystem permissions where supported.
