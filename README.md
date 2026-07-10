# rho

rho is a Rust toolkit for building AI agents by changing Rust code instead of
swapping standalone processes.

It borrows useful implementation ideas from Tau, especially the Responses and
ChatGPT/Codex inference behavior and terminal rendering pieces, but deliberately
does not copy Tau's protocol, supervisor, extension runtime, socket API, or
process graph. The core bet is that many agent changes are simpler when they
are local edits to crates, structs, enums, futures, and streams.

## Shape

- `rho-core`: small shared item vocabulary for messages, tool calls, tool
  results, reasoning text, opaque provider items, inference requests, the
  streaming inference-service trait, inference updates/responses, ids, usage,
  and token usage.
- `rho-inference`: concrete inference provider integrations behind a
  provider-neutral public API, plus the auth-management CLI workflow. Its
  current private implementation provides WebSocket-only OpenAI Responses and
  ChatGPT/Codex inference support.
- `rho-agent`: an opinionated, forkable harness that owns queueing, retries,
  tool scheduling, persistence hooks, streamed transcript handling, and
  inference response block persistence while depending only on the core
  inference-service trait.
- `rho-context-config`: bounded `AGENTS.md` loading and local Markdown skill
  discovery support shared by workspace caches and agent prompts.
- `rho-cli`: an interactive terminal chat agent assembled from `rho-agent`, the
  inference service, shell/apply_patch tools, CBOR persistence, and copied Tau
  terminal rendering building blocks.
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
nix develop -c cargo run -p rho-cli -- auth add
nix develop -c cargo run -p rho-cli -- auth list
nix develop -c cargo run -p rho-cli -- auth remove default
nix develop -c cargo run -p rho-cli -- auth path --name default
nix develop -c cargo run -p rho-cli -- auth status --name default
nix develop -c cargo run -p rho-cli -- auth rate-limits --name default
nix develop -c cargo run -p rho-cli -- auth import --name default --file credentials.json
```

`auth add` runs the browser OAuth setup flow and saves credentials to a
file under the rho state directory. `auth list` and `auth remove` inspect
and delete those file-based credentials. `auth import` is available for copying
an existing file-based credential; `credentials.json` uses this JSON shape:

```json
{
  "access_token": "...",
  "refresh_token": "...",
  "expires_at_ms": 9999999999999
}
```

Sessions are append-only CBOR logs under the rho state directory unless
`--session-path` is supplied. Stored sessions also use the session name as the
inference prompt-cache key. Use `--no-store` for a disposable run that does not
read/write a transcript or send a inference prompt-cache key. The default tool
surface is intentionally small: `shell_command` and `apply_patch`.

In the interactive chat UI, `Enter` sends the prompt, `Shift-Enter` or
`Alt-Enter` inserts a newline, double `Ctrl-C` cancels the running response, and
`Ctrl-D` exits from an empty prompt.

Rho loads `AGENTS.md` instructions from user `~/.config/agents/AGENTS.md` and
the workspace repo root `AGENTS.md`, then includes them in the agent prompt with
file boundaries. User config instructions are listed before repo instructions.

Rho discovers Markdown skills from project `.agents/skills` plus user
`~/.config/agents/skills`. Skills are listed in the agent system prompt with
names, descriptions, and file paths; the model reads the referenced files with
normal shell tools when a task calls for them.

The daemon also runs an embedded Octo GitHub helper server for agent commands.
Install its GitHub token with `rho octo init`; the token is read from stdin and
kept in the daemon's sealed RAM-only platform secret store. Agent shell and
Claude commands receive the server path in `OCTO_SOCKET`, and can use the
vendored `oct` CLI.

For one-shot use:

```sh
printf 'summarize this repository\n' | nix develop -c cargo run -p rho-cli -- --prompt-stdin
```

## Tau Comparison

Current parity is strongest around Tau's ChatGPT/Codex Responses path:

- Responses API only for the initial inference surface.
- WebSocket transport only.
- Persistent ChatGPT/Codex WebSocket pool keyed by prompt-cache/thread id, with
  prewarming support.
- Item-only inference responses; no top-level messages, tool calls, or terminal
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
- No socket server, config system, site, or e2e harness.
- CLI/TUI support is intentionally direct and Rust-local: it uses copied Tau
  terminal rendering pieces, not Tau's harness protocol or daemon.
- No Chat Completions inference service.
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
