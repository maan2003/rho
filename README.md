# rho

rho is a Rust toolkit for building AI agents by changing Rust code instead of
swapping standalone processes.

It borrows useful implementation ideas from Tau, especially the Responses and
ChatGPT/Codex provider behavior and terminal rendering pieces, but deliberately
does not copy Tau's protocol, supervisor, extension runtime, socket API, or
process graph. The core bet is that many agent changes are simpler when they
are local edits to crates, structs, enums, futures, and streams.

## Shape

- `rho`: small shared item vocabulary for messages, tool calls, tool results,
  reasoning text, opaque provider items, provider requests/responses, ids, usage,
  and token usage.
- `rho-provider-responses`: WebSocket-only OpenAI Responses and ChatGPT/Codex
  provider support.
- `rho-agent`: an opinionated, forkable harness that owns queueing, retries,
  tool scheduling, persistence hooks, streamed transcript handling, and
  provider response block persistence.
- `rho-cli`: an interactive terminal chat agent assembled from `rho-agent`, the
  Responses provider, shell/apply_patch tools, CBOR persistence, and copied
  Tau terminal rendering building blocks.
- `rho-cli-term-raw`, `rho-term-screen`, `rho-blocking-notify-channel`:
  terminal UI building blocks adapted from Tau for normal-buffer interactive
  rendering with streaming output.
- `rho-tool-shell`: concrete `shell_command` and `apply_patch` tools.
- `rho-store-cbor`: an append-only CBOR transcript log.
- `rho-store-redb`: an append-only redb transcript log.

`rho-agent` is not a universal agent framework. It is a useful default harness
assembled from reusable lower crates. If a workflow diverges, patching or
forking `rho-agent` is expected.

## Using The CLI

The current binary target is provided by `rho-cli`:

```sh
nix develop -c cargo run -p rho-cli -- --session default
```

The CLI uses OAuth credentials from the rho state directory by default:

```sh
nix develop -c cargo run -p rho-cli -- provider add
nix develop -c cargo run -p rho-cli -- provider list
nix develop -c cargo run -p rho-cli -- provider remove default
nix develop -c cargo run -p rho-cli -- auth path --name default
nix develop -c cargo run -p rho-cli -- auth status --name default
nix develop -c cargo run -p rho-cli -- auth import --name default --file credentials.json
```

`provider add` runs the browser OAuth setup flow and saves credentials to a
file under the rho state directory. `provider list` and `provider remove` inspect
and delete those file-based credentials. `auth import` is available for copying
an existing file-based credential; `credentials.json` is a
`ResponsesOAuthCredentials` JSON object:

```json
{
  "access_token": "...",
  "refresh_token": "...",
  "expires_at_ms": 9999999999999
}
```

Sessions are append-only CBOR logs under the rho state directory unless
`--session-path` is supplied. Stored sessions also use the session name as the
provider prompt-cache key. Use `--no-store` for a disposable run that does not
read/write a transcript or send a provider prompt-cache key. The default tool
surface is intentionally small: `shell_command` and `apply_patch`.

In the interactive chat UI, `Enter` sends the prompt, `Shift-Enter` or
`Alt-Enter` inserts a newline, double `Ctrl-C` cancels the running response, and
`Ctrl-D` exits from an empty prompt.

For one-shot use:

```sh
printf 'summarize this repository\n' | nix develop -c cargo run -p rho-cli -- --prompt-stdin
```

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
  prompt-cache keys, custom tool formats, fixed Codex request shaping, and
  usage.
- CBOR and redb transcript stores as concrete persistence building blocks.
- Tool surface is intentionally limited to `shell_command` and `apply_patch`.
  Shell command results use structured process metadata, including status,
  signal, timeout state, combined output, stdout, stderr, truncation, and UTF-8
  validity.
- Tau-style Nix, flakebox, selfci, treefmt, and nextest project setup.

Intentional differences from Tau:

- No process protocol or supervisor.
- No extension runtime or plugin protocol.
- No socket server, config system, skills, site, or e2e harness.
- CLI/TUI support is intentionally direct and Rust-local: it uses copied Tau
  terminal rendering pieces, not Tau's harness protocol or daemon.
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
