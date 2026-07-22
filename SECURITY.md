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
- Client-side web search sends the configured model and a bounded recent
  transcript excerpt to ChatGPT's first-party search endpoint using the same
  OAuth identity as inference. Search responses are remote, semi-trusted tool
  output: HTTP bodies are capped at 4 MiB, parsed, and independently truncated
  to the tool output budget before entering context.
- Local filesystem state may contain transcripts and OAuth credentials;
  credential files are secrets.
- Provider debug logs under the rho state directory may contain full inference
  request bodies, tool results, and raw provider events; treat them like
  transcripts.
- Opt-in GUI and daemon Dial9 profiles contain thread names, function symbols,
  local source paths, precise activity timing, and frontend marker metadata.
  They do not intentionally include transcript data, but remain local
  diagnostic files whose destination and retention are the user's
  responsibility. On Linux, Dial9 normally samples through `perf_event_open`;
  its clock-timer fallback owns process-global `SIGPROF`, installs a chained
  process-global `SIGSEGV` handler for safe stack reads, and samples only
  registered threads. The `SIGSEGV` handler is not restored, but profiling
  runs until process shutdown. The fallback must not run alongside another
  in-process profiler using `SIGPROF`. Perf sampling frequency is per
  inherited thread, and inherited child-process samples are collected before
  Dial9 discards them, so overhead can scale with process and subprocess
  parallelism. GUI frame timings are retained in memory until shutdown, and
  the single-file trace grows linearly with profiled CPU/frame activity.
  Dial9 symbolization and compression materialize the whole segment in memory
  during shutdown; profiling is intended only for bounded diagnostic runs.
- Shell/apply-patch tools can affect the caller's workspace and must remain
  explicit user-facing capabilities.
- `rho wayland` sessions expose a private Wayland socket and Sway IPC
  socket below a mode-0700 runtime directory. Anyone able to access those
  sockets can observe or inject input into applications in that session. The
  driver never exposes them over the network, validates session names as a
  single path component, and records process start identities before sending
  signals during cleanup. Applications launched in a driver session are not
  sandboxed and retain the invoking user's authority.
- Long-running `exec_command` processes are retained only in their owning
  agent's in-memory command-session table. `write_stdin` requires that local
  numeric session id; waits are capped at five minutes and dropping the agent
  drops and kills its retained child processes.
- An agent's working set (its workdirs/mount-namespace view) is fixed at spawn,
  persisted on the agent record, and provides version isolation rather than
  access isolation: the namespace redirects entry paths to the agent's
  checkouts but does not restrict access to the rest of the filesystem.
  Isolated jj workdirs are stable bcachefs subvolumes. Each live Rho process
  holds a shared advisory lock on a persistent sibling lease file; jj's
  repository-local GC alone requests a nonblocking exclusive lock, rechecks
  its last-use timestamp, snapshots the working copy, and only then deletes
  the subvolume.
  The lock coordinates cooperating Rho/jj processes, not arbitrary same-user
  filesystem mutation. Managed workspaces require bcachefs; jj invokes the
  kernel's bcachefs subvolume ioctls directly rather than spawning a mutable
  executable from PATH.
  Apply-patch translates absolute paths inside any workdir to that workdir's
  checkout, so in-process file writes follow the same redirection as
  namespaced commands.
- Sandbox workspaces are a narrower, opt-in boundary for native agents. Rho
  creates a normal isolated jj-managed workspace, masks its original `.jj` and
  colocated `.git` metadata in the command mount namespace, and points Git at
  a separate synthetic baseline. Child commands receive a fail-closed Landlock policy: sandbox
  workdirs/home/temp/runtime directories are writable, explicit system and
  toolchain paths are read-only, other filesystem access is denied, and new
  TCP bind/connect operations are denied; a seccomp filter permits creation
  of Unix sockets only, covering UDP and other network families unavailable
  to Landlock ABI 7. The policy requires Landlock ABI 7. In-process patch
  writes separately reject paths outside the
  sandbox workdirs. Sandbox views never mix sandbox and ordinary workdirs.
  This is practical containment for evaluation workloads, not a hardened
  multi-tenant boundary: Landlock does not govern every metadata syscall or
  resource-exhaustion vector, and selected runtime paths remain readable.
- User/repo `AGENTS.md` files and local/project Markdown skills are trusted
  prompt input when discovered. Treat them as useful local guidance, not a
  sandbox or permission boundary.
- Rho's packaged skills are immutable package data at a store path embedded
  when the final binaries are built. They are trusted prompt input, not a
  security boundary.
- Messaging-platform/API secrets (e.g. Slack tokens and Octo's GitHub token)
  reach the daemon over the UI socket (`rho slack init --dir <coordinator-repo>`
  and `rho pr init` read them from stdin) — never via argv, exec-time
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
  attribution. Integration-internal control inputs (including PR-monitor
  wakeups) carry a process-local internal source tag and are not mirrored
  verbatim; the PM must relay an appropriate user-facing summary explicitly.
- The embedded Octo server listens only on its fixed per-user Unix socket and
  uses the sealed platform secret store as its GitHub API and constrained Git
  HTTP token source. It has no token argv/env/file/admin import path in Rho.
  Token-backed fetches are limited to standard GitHub remotes; receive-pack
  independently rejects every update outside `refs/heads/rho/*`. The helper
  routes any push batch containing another destination to client SSH and never
  retries an HTTP rejection. Without a token, its push listing is synthesized
  from at most 4,096 local remote-tracking refs and every push uses client SSH;
  destination plans are capped at 64 KiB. Remote-helper `cas` options and
  forced updates carry exact `--force-with-lease` expectations into the inner
  `git send-pack`, including expect-absent leases; when no observed old ref is
  available the routed path does not turn the update into an unconditional
  force.
  The token's actual fine-grained GitHub
  permissions still determine its authority and must be audited when setup
  guidance changes.
- SSH Git credentials stay on native GUI machines. Every native GUI
  automatically registers its connection as a provider. Requests expose the
  typed destination and repository to all registered GUIs; push requests also
  expose a bounded, validated destination-ref plan. The first user approval
  claims the credential-provider role. Every other recipient receives only an opaque `Done` for that
  request id, revealing neither winner nor outcome. With no provider the daemon
  rejects immediately; after 60 seconds without a claim it rejects the
  request. A winning GUI permits only hosts `github.com` and `git.sr.ht`, fixes
  the SSH user to `git`, and validates the port, normalized two-component
  repository path, service, and destination refs. The username is therefore
  omitted from the approval prompt. It asks before
  starting OpenSSH. For pushes it independently parses the actual bounded
  receive-pack command list and requires its destination-ref set to exactly
  match the approved plan. Any missing, additional, duplicated, or changed ref
  fails closed without a second prompt or any client-to-OpenSSH bytes. The
  approval and provider claim are one operation with a 60-second deadline. Ref names,
  repository fields, and prompts use components limited to ASCII alphanumeric
  characters, hyphens, underscores, and periods; prompt
  text replaces control and bidirectional formatting characters. The helper
  and GUI both enforce the same host, user, and repository rules. Push options,
  signed pushes, unknown framing, and unsupported object-id sizes fail closed.
  The daemon-side remote helper runs the same command parser, but the GUI never
  relies on that validation to protect its credential.
- SSH Git approval is session-only. No provider, a declined fetch, or a denied
  push means a fast failure for operations routed to SSH; PAT-backed GitHub
  fetch and `rho/*` push remain
  available without a GUI. At most eight requests wait in the daemon
  and each GUI runs one SSH transport at a time. A push is not failed over
  after an approved GUI claims it; retrying starts a new race.
  Streams are backpressured, SSH diagnostics are capped at 64 KiB, and
  cancellation or disconnect drops
  the stream and kills the GUI-owned OpenSSH child. OpenSSH config and host-key
  verification on the GUI machine remain part of the trust boundary. A lost
  connection after sending a receive-pack request has an ambiguous outcome;
  callers must inspect the remote ref before retrying.
- `rho-pr-monitor` uses Octo only for bounded authenticated GitHub API calls;
  subscription ownership and operations stay behind `rho pr` on the normal
  daemon socket. Agent invocations identify the subscriber with
  `RHO_AGENT_ID`; interactive invocations may use `--agent`. The daemon
  resolves that handle, requires an Engineer role, and generation-binds every
  subscription and feedback target. This is request scoping, not a new local
  authorization boundary: clients able to reach the privileged daemon socket
  already have equivalent control.
  Watches accept only canonical HTTPS GitHub PR URLs and bind replies to the
  Engineer that registered the stable repository-id/PR-number pair. Human
  feedback wakes an agent only for the PR author or OWNER/MEMBER/COLLABORATOR
  associations; bot feedback requires an exact per-watch login allowlist.
  Bodies and diff context are length-bounded and explicitly labeled untrusted
  before entering an Engineer prompt. Pending reviews are not delivered.
  Top-level comments require an active subscription owned by the calling
  Engineer. When replying to feedback, the event must also belong to that same
  PR and subscription generation.
  Outbound markers suppress input only when the comment author matches GitHub's
  authenticated viewer id. Event replies carry a reserved random operation
  marker for ambiguous retry recovery; reply commands revalidate subscription
  ownership and stored targets, limiting loops and preventing arbitrary GitHub
  comments through the CLI surface.
  Polling is capped at 16 active watches, two pages per feedback surface, 100
  CI records per API family, and 4 MiB per GitHub JSON response. It uses
  request timeouts and exponential error backoff, emits only state changes,
  and stops on merge/close. CI log archives are stream-limited to 48 MiB on
  both socket hops; extraction permits at most 1,000 files, 16 MiB per entry,
  and 128 MiB total expanded data. GitHub comments, bot output, paths, links, and diff
  hunks remain prompt-injection-capable input;
  Engineers must validate claims against the repository before changing or
  executing code, and summarize meaningful milestones to their parent rather
  than forwarding raw review text.

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
  After authentication, native GUI connections may accept up to 1024
  daemon-initiated unidirectional agent-state streams. These streams carry only
  framed UI state for agents already loaded by the daemon; authorization and
  commands remain on the authenticated control session. Stream weights are
  sender-local scheduling metadata and are never trusted from the network.
  If more than 1024 non-hidden agents are loaded, the daemon closes the native
  connection rather than silently serving incomplete state; the user must hide
  agents before reconnecting. Hidden agents are omitted from the warm stream
  set but get a stream if explicitly loaded/opened again.
  Agent frames retain the 64 MiB per-frame bound, and the native GUI reserves
  each declared payload against a connection-wide non-FIFO atomic byte budget before
  allocation, bounding concurrent length-prefix-driven frame allocations to
  128 MiB while allowing small frames to bypass a waiting large allocation.
  The reservation remains attached to the decoded GUI event until consumption,
  so slow UI handling cannot refill an unbounded queue of large agent frames.
  Setting `QLOGDIR` opts the process into writing a qlog file for every iroh
  connection. Qlog records transport metadata such as endpoint addresses,
  connection IDs, packet timing and sizes, stream IDs and offsets, loss, and
  congestion state, but not UI frame payload bytes or cryptographic secrets.
  Treat captures as sensitive diagnostics, use a private directory and bounded
  capture window, and remove them after analysis; rho does not rotate or cap
  their aggregate disk usage.
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
- An authenticated native UI client may request a diff for any workspace it
  can already open through the fully privileged UI protocol. A refresh is a
  persistent jj write: it snapshots that workspace and descendant workspace
  commits under the per-repo lock and may therefore rebase/materialize those
  descendants. Unrelated workspace branches are not scanned. The returned
  repository epoch is consumed under the same lock, avoiding mixed-operation
  manifests. The blocking job owns that lock, so an RPC timeout cannot admit a
  concurrent jj mutation while the timed-out worker finishes.
- Diff manifests expose repository-relative paths and bounded parent file
  contents to the requesting GUI; current-side contents stay on the existing
  Zed channel. Reads are limited per file, aggregate I/O, aggregate payload,
  and file count; both parent materialization and target text/binary probes
  charge the aggregate I/O budget. Dirty-path requests have count and path-byte
  limits. The headless Zed host enforces an 8 MiB limit while reading each live
  file on initial open, watcher/manual reload, and binary load, closing
  replacement/growth races after the metadata check; the GUI also caps
  aggregate live text before building diffs. Daemon loads have a semaphore and
  30-second wait, and use a low-priority one-shot iroh stream. Both encoded and
  raw frame writers enforce the same 64 MiB bound as readers.
- Hidden diff surfaces retain their Zed watch stream and local buffer identity,
  but watcher/buffer invalidations cannot initiate jj manifest RPCs until that
  model is shown in an active pane. Hidden changes coalesce; an already-started
  request may still finish after the surface is hidden.
- Checked Zed saves use a separate RPC, so an older host cannot ignore a new
  field and silently fall back to overwrite. Within one headless project,
  checked and unchecked saves for the same server buffer share a gate. A
  checked save performs no write when its immediate host-side metadata check
  sees path existence or exact mtime differ from the buffer's last
  saved/reloaded baseline; only an explicit overwrite/recreation response uses
  the unchecked RPC. This is not filesystem CAS: an external writer can race
  after the check, preserve/collide on metadata, or write through another host
  session or buffer identity. Rho therefore still does not focus-loss autosave.


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
- The GUI's editor-native shell is also a daemon-owned command surface with the
  agent workspace's authority. The daemon starts `rho-shell` through the agent
  View and gives it one private framed Unix socket as stdin. The sidecar makes a
  close-on-exec duplicate of that socket, replaces OS stdin/stdout/stderr with
  `/dev/null`, and gives Brush only explicit virtual descriptors backed by the
  current execution's PTY slave. Consequently, evaluated commands cannot
  accidentally inherit or redirect the protocol socket. The process boundary
  also keeps Brush and shell-global operations such
  as `exit` or `exec` out of the daemon process while retaining the View's mount
  namespace and filesystem authority.
  The daemon treats every sidecar frame as untrusted: decoding is bounded,
  response state and daemon-assigned execution ids are validated, command text
  remains daemon-owned, prompts/output are sanitized, and a violation terminates
  that shell session rather than being forwarded to a client. This is a protocol
  boundary, not an OS sandbox. Configuration and commands run as the daemon's
  user with workspace authority, so deliberately malicious same-user code may
  attack other local processes through ordinary operating-system facilities.
  Strong process isolation would require a separate sandbox or identity.
  `RHO_SHELL` and `RHO_PAGER` may override sibling/PATH executable lookup and
  are therefore trusted daemon-administrator input. `rho-shell` loads Bash-compatible
  interactive configuration from Brush, including `~/.bashrc`, `PS1`, and
  `PROMPT_COMMAND`; any configuration or `.envrc` reached from those hooks is
  trusted local code with the same authority. Sandboxed agents remain refused
  because their intentionally empty HOME has no trusted startup hook to activate
  the project environment.
- One serialized Brush evaluator persists per agent across client detach. A GUI
  explicitly starts or attaches to it; closing an attachment only detaches,
  while an explicit close gracefully stops the kernel and remaining jobs.
  Complete client-local drafts travel over the sideband protocol and are capped
  at 1 MiB; protocol frames are capped at 2 MiB. Each execution receives a fresh
  80x24 PTY whose slave supplies stdin, stdout, and stderr. Its controller has a
  dedicated relay tagged with the daemon-assigned execution id; background
  descendants retain their originating PTY and therefore their output
  attribution. EOF writes the PTY's configured VEOF byte only to the active
  execution. Interrupt sends SIGINT only to sidecar-session descendants with a
  standard descriptor still attached to the active PTY. This per-execution PTY
  is not the persistent evaluator's controlling terminal, so programs needing
  arbitrary interactive input, `/dev/tty`, persistent job-control terminal
  semantics, a terminal screen, or hidden password entry belong in the raw
  terminal.
- Pager-aware commands receive `rho-pager` through `PAGER` and `GIT_PAGER`.
  The sidecar binds one Unix socket below the user-private `XDG_RUNTIME_DIR`
  and requires both a random shell-lifetime token and a fresh random execution
  token from the pager's inherited environment. Pager frames are independently
  capped at 4 KiB, at most 64 connections may be active, and the sidecar maps
  the execution token to a daemon-assigned execution rather than accepting an
  execution id from the child. These capabilities prevent accidental or stale
  cross-shell attribution. An execution token remains valid until its
  originating PTY controller reaches EOF, allowing delayed background
  descendants to authenticate but rejecting them once that output scope closes.
  Pager actions are scoped to `(execution, pager, page)`, and the first valid
  action for a page wins. These controls are not isolation from deliberately
  malicious evaluated code or other same-user processes that can obtain its
  environment.
  Pager output still traverses the execution PTY and normal sanitizer. The
  helper pauses after the configured 1–1000 logical lines (24 by default) or a
  hard 64 KiB byte limit, stops reading so the producer receives pipe
  backpressure, and fails open to unpaged relay if its control socket
  disappears. Normal shutdown unlinks the socket; SIGKILL or a crash may leave
  its unreachable random pathname until `XDG_RUNTIME_DIR` is cleaned.
- The daemon is the canonical owner of bounded structured `ShellState`: accepted
  command text, prompt/cwd, execution status, and sanitized per-execution output.
  Output ANSI SGR colors and attributes are decoded into bounded structured style
  spans; prompt ANSI and all other control strings are discarded, and
  carriage-return/backspace edits are confined to the active output line. Slow or
  newly attached clients receive a full structured snapshot rather than a
  separate flat transcript, and the final canonical state and exit status bypass
  congested incremental queues.
  The shell runs in its own process session; normal exit sends TERM then KILL to
  all remaining members of that session, while task cancellation kills the
  session immediately. A command can intentionally create a new session and
  thereby outlive the shell, just as it can deliberately start a user service;
  this is accepted because editor-shell commands are trusted with the workspace's
  authority rather than sandboxed.
- Rho-owned agent variables (`RHO_AGENT_ID` and `RHO_MCP_AGENT_ID`) are supplied
  explicitly to agent commands rather than copied
  incidentally from the daemon environment.
- Rho forces all daemon-owned agent, terminal, and internal workspace
  subprocesses through process-local Git URL rewrites for the exact
  `git@github.com:`, `ssh://git@github.com/`, `git@git.sr.ht:`, and
  `ssh://git@git.sr.ht/` prefixes. It appends these
  entries to the captured `GIT_CONFIG_COUNT` environment without writing
  repository or user Git configuration; other hosts and GitHub SSH aliases
  keep their normal transport.
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
managed workspaces, Rho provides the rendered Rho prompt through a
generated temporary file that is file-bind-mounted over `~/.claude/CLAUDE.md`
inside the Claude process's private workspace mount namespace. If the bind
target does not exist, Rho creates an empty `~/.claude/CLAUDE.md` file first.
Rho does not write the generated prompt into the origin checkout or workspace
checkout, and it removes the generated source file when the Claude process exits or
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
- Code mode is used by GPT-5.6-backed roles except `eng-mini`, which uses the
  direct tool surface, and is fixed at agent creation; the daemon rejects
  changing the role on a running agent. When on,
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
