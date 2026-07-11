# Security and reliability context

`rho` is a Rust toolkit and CLI for local AI-agent workflows. The main
production/runtime surfaces are local terminal use, local transcript/session
stores, local shell/apply-patch tools, and inference crates that talk to external
AI APIs.

## Trust boundaries

- Local users control prompts, session names/paths, inference auth setup/import,
  and tool inputs.
- Inference APIs and streamed inference events are remote, semi-trusted inputs and
  must be parsed defensively.
- Local filesystem state may contain transcripts and OAuth credentials;
  credential files are secrets.
- Provider debug logs under the rho state directory may contain full inference
  request bodies, tool results, and raw provider events; treat them like
  transcripts.
- Shell/apply-patch tools can affect the caller's workspace and must remain
  explicit user-facing capabilities.
- User/repo `AGENTS.md` files and local/project Markdown skills are trusted
  prompt input when discovered. Treat them as useful local guidance, not a
  sandbox or permission boundary.
- Messaging-platform/API secrets (e.g. Slack tokens and Octo's GitHub token)
  reach the daemon over the UI socket (`rho slack init --dir <coordinator-repo>`
  and `rho octo init` read them from stdin) — never via argv, exec-time
  environment, or files. `rho_slack::SecretStore` holds them in a sealed memfd
  and stashes/reclaims it via the systemd fd store (`FDSTORE=1`/`$LISTEN_FDS`),
  so tokens never touch disk and survive daemon restarts but not reboots. The
  explicit Slack coordinator repo is persisted in the local rho database, and
  Slack thread sessions always start PM agents there; cross-repo work
  is model-directed delegation to spawned agents with explicit repo paths, not
  first-message repo selection. Token values must not appear in logs or errors.
  Inbound Slack Socket Mode frames are remote, semi-trusted input: malformed or
  unexpected frames are skipped, never panicked on, and message text is handed to
  agents as untrusted user content. Slack replies are posted only when a
  Slack-mapped Rho agent explicitly calls its injected `slack_reply({text})`
  tool, which re-checks the persisted agent→thread mapping at dispatch time;
  completed-turn reports only clean up in-progress reactions. Slack-originated
  inputs are tagged with a private in-memory source id so the Slack relay can
  ignore its own accepted-input reports; other local-client user inputs to
  Slack-mapped agents are mirrored into the mapped thread with conservative
  attribution.
- The embedded Octo server listens only on a daemon-owned Unix socket whose path
  is passed to agent commands as `OCTO_SOCKET`. Octo uses the sealed platform
  secret store as its GitHub token source and has no token argv/env/file/admin
  import path in Rho. GitHub API responses and errors are remote, semi-trusted
  input and are returned to the calling command rather than panicking.

## Remote UI transports (iroh and web UI)

- With `rho daemon --iroh`, the daemon serves the full UI protocol over iroh
  (relay-backed QUIC). An enrolled client is fully privileged: everything a
  local UI client can do, including starting agents that run shell commands.
  Trust is per client endpoint key, persisted in the local rho database, and
  granted only by a local user running `rho iroh approve <code>` against the
  Unix socket; codes are 60-bit, single-use, expire after a minute, and bind
  the exact client key via a TLS exporter, so approval cannot be replayed or
  redirected. The daemon's iroh secret key lives owner-readable-only in the
  rho state directory. Enrollment approval is also accepted from already
  trusted remote clients (they are fully privileged anyway).
- The web UI (a static Leptos/wasm page in `webui/`, hostable anywhere) is
  an iroh client like any other: it connects to the same endpoint on its own
  ALPN and passes the same per-key enrollment before the daemon serves it.
  Its session is a reduced JSON command set (list/select agents, send
  message, new agent in a registered workdir, cancel turn) bridged onto a
  normal in-process UI protocol session; it grants nothing the full UI
  protocol does not. The page holds no secrets beyond the browser's own
  iroh key (kept in local storage, useless without enrollment) and sends
  only user-authored text.
- Inbound data on both ALPNs is remote, semi-trusted input: oversized UI
  protocol frames are rejected (`MAX_FRAME_LEN`), web UI JSON lines are
  length-bounded (`MAX_LINE_LEN`), malformed frames end the connection, and
  malformed browser JSON produces an error message, never a panic.


## Runtime assumptions

- Runtime code is primarily Tokio async Rust plus local CLI/TUI code.
- Network paths must have bounded waits or documented cancellation behavior.
- Queues and streams on inference/tool paths should provide backpressure or
  document accepted bounds.
- Production paths should not panic on malformed inference data, bad local input,
  missing files, or network failures.

## Realtime voice provider (`rho-voice`)

- `rho-voice` is a client for the xAI realtime voice WebSocket (Grok Voice
  Agent API). Server events are remote, semi-trusted input: parsing is
  defensive, unknown event types are preserved rather than rejected, and
  malformed payloads (bad base64 audio, missing fields) are errors, never
  panics.
- Authentication is OAuth-only: rho copies the Grok CLI login from
  `~/.grok/auth.json` into its own state auth file on first voice use, then
  refreshes through `auth.x.ai`. OAuth bearer tokens are never printed.
- The daemon runs at most one voice session, started only by an explicit
  client `VoiceStart` (`:voice` in the GUI) because microphone audio leaves
  the machine (to xAI) and sessions are billed per minute. The session stops
  on client request, on disconnect of the owning connection, and
  automatically after five minutes without detected speech.
- Voice tool calls are provider-controlled input executed against the agent
  registry; the tool surface is limited to the same operations UI clients
  already have (list/status/send/create/cancel/rename/move/archive), name
  resolution reports ambiguity instead of guessing, and tool errors are
  returned to the model as text, never panics.
- GUI audio: mic capture and playback ride the existing UI socket as raw
  PCM frames; the capture thread stops when the session ends or the
  connection channel closes.
- Bounded waits: socket reads take a per-event timeout, keepalive pings run
  every 25 seconds, and the smoke binary bounds its whole run by a
  per-event timeout.

## AGENTS.md

Rho loads `AGENTS.md` instructions from user `~/.config/agents/AGENTS.md` and
the workspace repo root `AGENTS.md`. These files are included in the agent
prompt with explicit file boundaries. They are trusted prompt input and do not
grant or restrict tool permissions.

AGENTS.md reads are bounded to 32 KiB per file and truncated with a diagnostic.
Rho follows symlinks with cycle detection for `AGENTS.md` files and does not
load legacy `~/.agents`, `.agents.local`, or `AGENTS.*.md` variants.

Claude-runtime agents keep Claude Code's `CLAUDE.md` discovery enabled. In
managed pool workspaces, Rho provides the rendered Rho prompt through a
generated temporary file that is file-bind-mounted over `~/.claude/CLAUDE.md`
inside the Claude process's private workspace mount namespace. If the bind
target does not exist, Rho creates an empty `~/.claude/CLAUDE.md` file first.
Rho does not write the generated prompt into the origin checkout or workspace
slot, and it removes the generated source file when the Claude process exits or
is cancelled. Loaded `AGENTS.md` content therefore has the same
external-provider exposure as other agent prompt text.

Claude Code MCP support is bound to the active Rho agent through
`RHO_MCP_AGENT_ID`, which Rho sets when spawning the Claude process. A globally
configured `rho mcp-agent-tools` stdio server inherits that environment and
treats tool calls as provider-controlled input: the daemon validates agent ids
and workspace choices, preserves the same spawn-depth/live-child limits as
in-process Rho tools, bounds wait operations, and returns tool errors as data
instead of panicking.

## Skills

Rho skills are local Markdown files discovered from project `.agents/skills`
and user `~/.config/agents/skills`. Skills contribute names, descriptions, and
file paths to the agent system prompt; the model reads the referenced files
with normal shell tools when it needs their instructions.

Discovery uses bounded 64 KiB reads and rejects a skill whose YAML frontmatter
is truncated before the closing fence. Discovery follows symlinks with cycle
detection for roots/directories/files. Skill files are prompt input only; they
do not restrict filesystem access or grant tools.

## Code mode (`rho-code-mode`)

- `rho-code-mode` runs model-authored JavaScript in an in-process V8 isolate
  (deno_core), one isolate per session on a dedicated thread. Scripts have full
  access to the host through the nested tool dispatcher — the same access the
  model already has through shell tools. Code mode is not a sandbox and adds no
  new privilege beyond the existing tool surface.
- Code mode is part of the opinionated PM agent role and is fixed at
  agent creation; the daemon rejects changing the role on a running agent. When
  on, the model-facing tools are only
  `exec`/`wait`, and
  shell plus multi-agent tools are dispatched from scripts on the agent's
  normal runtime through the same code paths as direct tool calls.
- Trust boundaries: script source is model-controlled input; nested tool calls
  leave the isolate through the `ToolDispatcher`, which forwards to the agent's
  normal tool path with its existing controls. The JS environment strips
  `console`, `Atomics`, `SharedArrayBuffer`, and `WebAssembly`, and exposes no
  I/O ops other than nested tool calls, `text`/`notify` output, and timers.
- `notify(...)` becomes a `ToolUpdate` attributed to the cell's originating
  `exec` call: it rides the agent's persisted input queue and enters model
  context at the next request boundary of the active turn. With no active
  turn the update is dropped, and leftover updates alone never start a turn,
  so script output cannot wake an idle agent.
- Resource bounds: exec/wait yield back to the model after a deadline (default
  10 s) while the script keeps running as a tracked cell; result text is
  middle-truncated to a token budget (default 10k tokens); a 100 ms heartbeat
  on the runtime thread detects synchronous busy loops.
- Cancellation: terminating a cell escalates from cancelling its pending tool
  ops (rejecting the promises it awaits), to `TerminateExecution` on the
  isolate if the heartbeat is stale (the isolate and other cells survive), to
  marking the cell an inert zombie whose ops are refused and output discarded.
  Dropping the session cancels all cells and shuts down the runtime thread.
- Tests: `crates/rho-code-mode/tests/session.rs` covers REPL state
  persistence, concurrent cells, yield/wait, terminate of both parked and
  busy-looping cells (with session survival), tool-failure propagation, and
  output truncation.

## Future review notes

Future changes that add providers, credential storage, transcript persistence,
subprocess execution, filesystem writes, or background tasks must update this
file and document their primary trust boundaries, resource bounds, cancellation
behavior, and tests.
