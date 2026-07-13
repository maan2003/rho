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
- Long-running `exec_command` processes are retained only in their owning
  agent's in-memory command-session table. `write_stdin` requires that local
  numeric session id; waits are capped at five minutes and dropping the agent
  drops and kills its retained child processes.
- An agent's working set (its workdirs/mount-namespace view) is version
  isolation, not access isolation: the namespace redirects entry paths to the
  agent's checkouts but does not restrict access to the rest of the
  filesystem. The set is fixed at spawn and persisted on the agent record.
  Apply-patch translates absolute paths inside any workdir to that workdir's
  checkout, so in-process file writes follow the same redirection as
  namespaced commands.
- User/repo `AGENTS.md` files and local/project Markdown skills are trusted
  prompt input when discovered. Treat them as useful local guidance, not a
  sandbox or permission boundary.
- Rho's packaged skills are immutable package data at a store path embedded
  when the final binaries are built. They are trusted prompt input, not a
  security boundary.
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
- The embedded Octo server listens only on its fixed per-user Unix socket.
  Clients derive that path from the same `octo-types` contract rather than an
  environment variable. Octo uses the sealed platform
  secret store as its GitHub token source and has no token argv/env/file/admin
  import path in Rho. Its Git remote-helper endpoint proxies smart-HTTP fetches
  for repositories readable by that token while retaining it in the daemon. It
  rejects every receive-pack command outside `refs/heads/rho/*`; command
  framing is bounded before pack data is streamed. GitHub API and Git protocol
  responses and errors are remote, semi-trusted input and are returned to the
  calling command rather than panicking.

## Remote UI transports (iroh and web UI)

- With `rho daemon --iroh`, the daemon serves the full UI protocol over iroh
  (relay-backed QUIC). An enrolled client is fully privileged: everything a
  local UI client can do, including starting agents that run shell commands.
  Trust is per client endpoint key. `rho iroh approve <code>` persists a
  pending enrollment in the local rho database; `rho iroh trust-in-memory
  <endpoint-id>` directly trusts a key in daemon memory, bounded to 4096 keys
  and 24 idle hours, and is intended for invocation through an existing SSH
  login. Every connection's
  first bi-stream is a bounded, ten-second auth-only exchange. The server
  explicitly returns approved, enrollment-required, or unavailable, and waits
  for a client acknowledgement before closing so the
  response cannot be discarded. Only approved connections may open later UI streams. After
  code approval, unknown clients reconnect with the same key. Both commands reach the daemon
  through its Unix socket. Codes are 50 bits displayed as ten lowercase
  Crockford Base32 characters, single-use, and expire after a minute. They are
  derived independently by server and client from both endpoint identities and
  the TLS exporter. The server registers its derivation but never sends it; the
  client displays its own derivation only after enrollment-required confirms
  registration succeeded, preventing cross-daemon code substitution.
  Active pending enrollments are capped at 10 and the five-minute
  recently-used collision cache at 4096 entries, including under repeated
  reconnects from one endpoint. At most 64 pre-auth exchanges run concurrently,
  each connection permits at most 16 queued bidirectional streams, and both
  client and server bound the auth exchange to ten seconds. The daemon's iroh
  secret key lives in the local rho database.
  Enrollment approval is also accepted from already
  trusted remote clients (they are fully privileged anyway).
- `rho-gui --endpoint <id>` generates its client key in process memory and
  never persists it. With `--ssh <destination>`, it runs the user's OpenSSH
  client to execute `rho iroh trust-in-memory <endpoint-id>` on the daemon host
  before connecting, so no enrollment code or rejected connection is needed.
  `--ssh` is required for native iroh connections because an ephemeral GUI key
  cannot survive a manual approval/restart cycle. The SSH host
  configuration and host-key verification are the authorization boundary and
  insecure fallback is not attempted. Existing legacy key files are ignored
  and left untouched. Once the GUI process exits, the daemon retains only the
  unusable public endpoint id until idle expiry or daemon restart.
  `--remote-rho <path>` selects the remote executable (default `rho`) and
  accepts only a nonempty shell-safe path alphabet; it is not an arbitrary
  remote shell command.
- The web UI (a static Leptos/wasm page in `webui/`, hostable anywhere) is
  an iroh client like any other: it connects to the same endpoint on its own
  ALPN and passes the same per-key enrollment before the daemon serves it.
  Its session is a reduced JSON command set (list/select agents, send
  message, new agent in a registered workdir, cancel turn) bridged onto a
  normal in-process UI protocol session; it grants nothing the full UI
  protocol does not. The browser uses a user-verifying WebAuthn credential's
  PRF extension to derive a stable, daemon-specific iroh key on each connect;
  only the non-secret credential id and daemon id are kept in local storage.
  The PRF output and derived iroh key remain in browser memory and are never
  persisted. The hosting origin and all JavaScript it serves are fully trusted:
  code running after the user approves the WebAuthn prompt can read the
  derived enrolled key and thereby gain persistent daemon access. Deploy the
  page on a dedicated origin without third-party scripts and treat its build
  and publishing pipeline as security-critical. The page refuses to run when
  framed and ships a restrictive meta CSP; production hosting must additionally
  send `Content-Security-Policy: frame-ancestors 'none'` as an HTTP header.
  Besides user-authored text, the page sends bounded agent creation choices
  (topic, registered workdir, role, base revset, and workspace mode).
  A compromised origin can register a persistent service worker as well as
  steal an unlocked key, so recovery requires revoking the endpoint, clearing
  the origin's browser site data, verifying the deployment, and enrolling a
  new identity.
  `rho iroh revoke <endpoint-id>` removes persistent and in-memory trust through
  the local daemon socket; already-established connections are not forcibly
  closed and must be disconnected (or the daemon restarted) during compromise
  recovery. In-memory trust is always lost when the daemon exits.
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

### Daemon subprocess environments

- The daemon captures a user environment once from a clean `bash -lc` at
  startup. Every daemon-owned subprocess clears the daemon environment before
  applying that snapshot, so service credentials and other incidental daemon
  variables are not inherited.
- Internal workspace-management commands receive only that user environment.
  Agent shell commands and Claude Code additionally run through `direnv exec`
  in their project directory. Project `.envrc` files are trusted local code and
  have the same authority as the agent shell tools they configure.
- Rho-owned agent variables (`RHO_AGENT_ID` and `RHO_MCP_AGENT_ID`) are supplied
  explicitly to agent commands rather than copied
  incidentally from the daemon environment.
- When present, `XDG_RUNTIME_DIR` is seeded into the login shell alongside the
  basic identity and shell variables so user-scoped runtime sockets remain
  reachable from agent subprocesses.
- CLI-local subprocesses, including land and selfci jobs, retain the invoking
  CLI's environment; they are outside the daemon subprocess boundary.

`rho debug render-prompt <role>` performs local context discovery in the
current workdir and prints the resulting prompt and model-facing Rho tool
specifications. Its output may contain repository instructions and user skill
metadata; it performs no inference and creates no agent or workspace.

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

Registered project paths and descriptions are included in PM prompts and are
therefore disclosed to the configured inference provider. Project UI names are
not included in model context. Treat descriptions as prompt input rather than
trusted instructions.

Claude Code MCP support is bound to the active Rho agent through
`RHO_MCP_AGENT_ID`, which Rho sets when spawning the Claude process. A globally
configured `rho mcp-agent-tools` stdio server inherits that environment and
treats tool calls as provider-controlled input: the daemon validates
role-prefixed handles and Engineer workdir choices;
preserves the same spawn-depth/live-child limits as
in-process Rho tools, bounds wait operations, and returns tool errors as data
instead of panicking.

Agent mail intentionally has no ownership or ancestry authorization: any agent
that knows another agent's unambiguous role-prefixed handle may inject mail into
its queue. This is a collaboration bus inside one trusted local pool, not a
team-isolation boundary. Self-messaging and ambiguous or mismatched handles are
rejected. Interrupt remains role-specific and separately validated.

Spawned Engineers always receive isolated jj workspaces; the model cannot opt
them into a shared jj checkout. Plain directories cannot be isolated and remain
shared. Advisors intentionally join their caller's workdirs and keep shell and
patch tools for read-oriented investigation and scratch experiments. They may
message other agents and wait for replies, but cannot spawn or interrupt, and
are instructed not to implement changes.

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
- Code mode is used by every GPT-5.6-backed role and is fixed at agent
  creation; the daemon rejects changing the role on a running agent. When on,
  the model-facing tools are only
  `exec`/`wait`, and
  shell plus multi-agent tools are dispatched from scripts on the agent's
  normal runtime through the same code paths as direct tool calls.
- Nested command calls return structured JSON values to JavaScript (including
  process session ids), while direct command calls render the equivalent
  Codex-style status headers as text. Other nested tools return JSON strings;
  tool errors reject the JavaScript promise rather than becoming values.
- `spawn_engineer` is installed in the nested runtime registry and listed by
  `ALL_TOOLS`, but its full declaration and delegation guidance live in the
  dynamically discovered `delegate-engineering` skill instead of every code
  mode prompt. Runtime authorization and spawn validation are unchanged.
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
