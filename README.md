# rho

rho is a Rust toolkit for building AI agents by changing Rust code instead of
swapping standalone processes.

It borrows useful implementation ideas from Tau, especially the Responses and
ChatGPT/Codex provider behavior, but deliberately does not copy Tau's protocol,
supervisor, extension runtime, CLI, TUI, socket API, or process graph. The core
bet is that many agent changes are simpler when they are local edits to crates,
structs, enums, futures, and streams.

## Shape

- `rho`: small shared item vocabulary for messages, tool calls, tool results,
  reasoning text, opaque provider items, provider requests/responses, ids, usage,
  and model parameters.
- `rho-provider-responses`: WebSocket-only OpenAI Responses and ChatGPT/Codex
  provider support.
- `rho-agent`: an opinionated, forkable harness that owns queueing, retries,
  tool scheduling, persistence hooks, streamed transcript handling, and
  previous-response chain state.
- `rho-tool-shell`: concrete `shell_command` and `apply_patch` tools.
- `rho-store-cbor`: an append-only CBOR transcript log.
- `rho-store-redb`: an append-only redb transcript log.

`rho-agent` is not a universal agent framework. It is a useful default harness
assembled from reusable lower crates. If a workflow diverges, patching or
forking `rho-agent` is expected.

## Tau Comparison

Current parity is strongest around Tau's ChatGPT/Codex Responses path:

- Responses API only for the initial provider surface.
- WebSocket transport only.
- Persistent ChatGPT/Codex WebSocket pool keyed by prompt-cache/thread id, with
  prewarming support.
- Item-only provider responses; no top-level messages, tool calls, or terminal
  events.
- Streaming updates for text, reasoning text, tool calls, output items,
  compaction, usage, response ids, and completion.
- ChatGPT/Codex OAuth credentials, file-based refresh, account id extraction,
  and standard-library file locking.
- Previous-response chaining, with stale chain recovery owned by the provider.
- Encrypted reasoning replay, compaction items, message phase support,
  prompt-cache keys, model-visible tool names, custom tool formats, model params,
  and usage.
- CBOR and redb transcript stores as concrete persistence building blocks.
- Tool surface is intentionally limited to `shell_command` and `apply_patch`.
  Shell command results use structured process metadata, including status,
  signal, timeout state, combined output, stdout, stderr, truncation, and UTF-8
  validity.
- Tau-style Nix, flakebox, selfci, treefmt, and nextest project setup.

Intentional differences from Tau:

- No process protocol or supervisor.
- No extension runtime or plugin protocol.
- No CLI/TUI, socket server, themes, config system, skills, site, or e2e harness.
- No Chat Completions provider.
- No HTTP or SSE Responses transport.
- No example crate or example files.

Known gaps that may be worth copying later:

- Debug/VCR capture.
- Release/build metadata and binary packaging, if rho grows a binary surface.

## Development

The repository uses Nix, flakebox, treefmt, nextest, and selfci.

Useful verification commands:

- `nix develop -c flakebox lint`
- `nix develop -c treefmt --ci`
- `nix develop -c cargo fmt --all --check`
- `nix develop -c cargo test --workspace`
- `nix develop -c cargo clippy --workspace --all-targets -- -D warnings`

## AI usage disclosure

[I use LLMs when working on my projects.](https://dpc.pw/posts/personal-ai-usage-disclosure/)
