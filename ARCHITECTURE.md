# rho architecture

`rho` is a Rust-local toolkit for building AI agents by composing crates rather
than by running a supervisor, extension protocol, or daemon process graph.

## Crate layering

- `rho-core` owns the shared vocabulary: transcript items, inference requests,
  inference events and responses, tool calls/results, usage, agent identities,
  roles and dispositions, message delivery and phases, and opaque
  provider items. It should stay policy-light.
- `rho-inference` translates `rho-core` requests and provider events, and its
  daemon-wide `Inference` handle owns the complete ChatGPT runtime: persisted
  enabled-account settings and current selection, quota polling/history,
  automatic account routing, and session creation. `Inference::new` opens that
  state from `RhoDb`. The private account manager makes selection decisions only
  when settings, quota, or rate-limit facts change; each new request snapshots
  the existing choice. Ordinary retries retain that choice and only explicit
  rate-limit failover replaces it. The
  daemon only projects safe settings/quota DTOs and merges Claude presentation.
  ChatGPT quota observations are attributed to that daemon-local namespace;
  `Inference` polls every enabled configured namespace and the GUI keeps each
  host/namespace history as an independent graph series.
  The explicit `eng-gemini` binding is a narrow exception: its deep session
  uses Antigravity `gemini-3.5-flash-low` with one manually configured profile,
  full transcript replay, and function tools only. It never enters ChatGPT
  account routing. Generated title/activity sidecars remain on ChatGPT.
- `rho-agent` owns the opinionated harness policy: queueing, retries/tool
  scheduling, streamed transcript handling, inference response block recording,
  and persistence hooks. Loading restores that logical state cheaply; the
  workspace-backed execution context (view, prompt, and tools) initializes
  lazily at first inference. It depends directly on the concrete
  `rho-inference` session. Its presentation sidecar derives a durable
  generated title and activity cache from committed text events. Native agents
  write those events directly; Claude commits individually bounded text from
  confirmed CLI messages into the same lineage and reconciles that mirror
  against the selected JSONL chain on load and rewind. It
  receives positions only after event persistence, validates every result's
  source position in the serialized agent loop, and rebuilds after a lineage
  fork. The same loop owns watched UI leases, source coalescing, cancellation,
  a 15-second request cadence, and result persistence; `AgentPool` only
  routes a lease to the loaded runtime. `AgentPool` also owns persistent
  agent-response subscription edges: terminal successes and failures are
  delivered to current subscribers as normal agent mail.
- `rho-claude-usage` is an isolated Claude Code subscription-quota adapter. It
  owns the hardened PTY process, `/usage` interaction, terminal emulation,
  parsing, polling cadence, and retry policy. The daemon only consumes parsed
  snapshots and persists them through its existing quota store.
- `rho-agent2` is an isolated experimental harness with only native inference,
  durable transcript/queue state, pull-based tool and peer-mail scheduling,
  streaming observation, and cancellation. It intentionally has no code mode,
  workspace/context discovery, collaboration, Claude runtime, or CLI/daemon/UI
  integration. Its architecture and governing decisions are recorded under
  `crates/rho-agent2/specs/`.
- `rho-workspaces` owns checkout materialization and filesystem views. A
  `Workspace` is one materialized checkout (a stable jj-managed bcachefs
  subvolume, the user's live checkout, a VCS-masked sandbox workspace, or a
  plain directory). Managed workspace ids are repository-local prefix ids
  rendered as `ws-<id>` and allocated by jj from a compact repository-local
  `(seed, next-counter)` registry; generated IDs are derived rather than
  stored individually. jj also dictates their stable per-repository paths,
  rematerializes missing checkouts, and garbage-collects stale materialized
  paths enumerated by jj's workspace store after snapshotting them. GC never
  scans the prefix-id counter range. Snapshotting is commit-cone scoped: jj
  snapshots the command workspace, workspaces sharing its working-copy commit,
  and materialized descendant workspaces, but not ancestors or siblings.
  Turn-boundary snapshots run `jj util snapshot` from the specific workspace
  checkout so the correct cone is selected. Every live `Workspace`
  holds a shared lock on its persistent sibling lease file; GC alone requests
  the nonblocking exclusive lock, so any number of Rho processes can inhibit
  collection without using the lease as a workspace-ownership mutex.
  Repository caches retain only weak references, while agents and views own
  live workspaces. Sandbox workspaces use the same managed checkout
  lifecycle while masking their `.jj` and colocated `.git` metadata from child
  commands, expose a separate synthetic Git baseline, and install a Landlock
  filesystem/network policy on every prepared child command. A `View` is one agent's world: a working
  set of workdir entries, fixed at spawn, realized as a private mount
  namespace with each entry's checkout mounted over its origin path. Entry 0 is
  the primary workdir (default cwd, prompt header).
  Agents joining a workspace share the `Workspace`; each agent has its own
  `View`. Each repository allocates its own managed workspace id, so a
  multi-repository agent's prompt lists a separate `ws-<id>` handle for every
  workdir.
  Sandbox and ordinary workspaces cannot be mixed in one view.
  Live-diff semantic barriers use the same vendored descendant-snapshot
  implementation through an embedded `jj-cli` API under the repo lock. The
  API returns the exact immutable repository epoch it wrote, so derived
  manifests never reload a racing operation head.
- `rho-context-config` owns bounded `AGENTS.md` loading plus local Markdown
  skill discovery/frontmatter parsing. Rho packages platform-owned skills under
  `$out/share/rho/skills`; the final package build embeds that immutable root in
  the binaries, below project and user skills in precedence. Results are cached per
  `rho-workspaces::Workspace` and merged across a view's workdirs;
  `rho-agent` owns system prompt rendering. Clients have no special skill or
  AGENTS.md command path. The native Rho inference loop and Claude Code use
  separate prompt compositions: Claude performs its own project and skill
  discovery and receives only Rho role/team and workspace context on
  top of Claude Code's own harness prompt.
- CLI and UI crates assemble concrete providers, tools, stores, and terminal
  rendering. They should not own inference protocol details. `rho-gui` uses
  `rho-touch-keyboard` for the browser client's reusable in-canvas keyboard
  core (layout, key dispatch, repeat, and local calibration telemetry).
  Each attached daemon is a named GUI host. Host-scoped settings and new-agent
  creation route through that identity explicitly; choosing a project never
  silently changes a draft's selected host.
  The Desk is one daemon-owned Zed CRDT text document per attached host. Its
  org-like headings derive structure rather than receiving structural RPCs.
  Visible agent-handle tags at the end of heading lines are the filing source
  of truth; `:project:` properties inherit down the heading tree. Every
  resolvable tag occurrence is an independent portal onto the exact named
  agent, so one agent may appear in several places. The normal presentation is
  a compact end-of-line hint; `g t` opens the complete runtime subtree as
  transient, per-occurrence display state. GUI clients retain one hidden CRDT source buffer per host
  and project shared read-only agent runtime buffers through occurrence-specific
  multibuffer rows, interleaved with writable prose and draft buffers.
  `space e` switches the Desk to a raw-source projection containing only those
  editable CRDT buffers, with all generated rows, hints, folds, and conceals
  removed until the user toggles the composed view back on.
  `rho-gui` supplies its context strip, theme mapping, and focus/show policy,
  while GPUI web owns only the immediate pointer-down routing region.
  Native web pages are client-local first-class resources owned by `rho-browser`.
  They use extension-generated UUID `PageId`s written in full as Desk tags.
  One embedded MV3 extension owns the durable page registry inside the implicit
  persistent Chromium profile: `chrome.storage.local` retains page metadata and
  one single-tab group titled `rho:<uuid>` attaches the ID to Chrome's restored
  tab and navigation history. Runtime tab and group IDs are never persisted.
  Rho sends only direct create/focus/close requests and does not mirror or
  receive an eager registry.
  All pages share one normal Chromium window and one private Smithay compositor
  surface, so exactly one page is visible. Switching a Rho page activates its
  tab. A generation-scoped handoff serializes extension focus requests and gates
  input until a DMA-BUF commit acknowledges a deliberately changed
  post-activation XDG configure. The previous atomic scene may remain visible
  but non-interactive during that handoff; Wayland ordering establishes frame
  readiness, not semantic pixel ownership by a Chrome tab.
  Valid compositor scenes continue to replace the displayed scene and receive
  presentation callbacks from a GPUI paint hook in the current outer frame
  while input is gated. This follows nested-compositor pacing and avoids both
  application-readiness deadlocks and an extra outer-refresh delay. The extension
  explicitly discards the previous tab, bounding loaded
  renderer and surface memory while Chrome retains per-page history metadata.
  The compositor issues one XDG activation token for the singleton toplevel and
  retains the unambiguous first-window fallback for stock Chromium builds that
  omit it. Extension native messaging reaches `rho-gui` through a tiny stdio
  relay and a mode-0600 Unix socket under `XDG_RUNTIME_DIR`; no TCP listener,
  CDP, remote debugging, content script, or website injection participates.
  Browser content is composed directly in GPUI/WGPU from an atomic Wayland
  surface-tree scene. Every DMA-BUF surface has explicit acquire/release
  synchronization, and GPUI retains each imported Vulkan image while its page
  model owns the lease; Smithay does not render or flatten the tree. Synchronized
  subsurface commits are reconciled only at their transaction anchor and the
  compositor copies lock-bound Smithay state into an immutable tree snapshot;
  the resulting bottom-to-top scene carries per-node position, viewport crop,
  destination size, and input region. Chromium SHM chrome and `xdg_popup`
  widgets use the bounded exception: the compositor validates and snapshots
  ARGB/XRGB rows into owned memory for GPUI/WGPU upload. Pointer hit-testing uses
  the same versioned scene, stacking order, geometry, and input regions. Wayland
  overlay delegation remains disabled so every visible client surface follows
  this single composition path.
  The compositor is wake-driven, advertises per-surface fractional scale and a
  viewporter while keeping its shared synthetic output stable, and forwards
  raw physical keys, pointer axes, and pinch phases to Chromium. It advertises
  `wp_cursor_shape_v1` and projects Chromium's named cursor requests onto the
  GPUI browser region, letting the outer display server render the native
  cursor rather than introducing a second cursor-surface renderer. `wl_shm`
  remains available only for ancillary Chromium surfaces; the root must remain
  an explicitly synchronized DMA-BUF.
  `rho-gui` only hosts the resulting GPUI page model/view. A full `:web-<uuid>:` tag
  on an ordinary Desk heading is a portal to the client-local page, just as an
  agent tag is a portal to an agent; selecting the heading uses the same
  right-hand preview card. Daemons do not own browser state.
  The native GUI
  exposes two deliberately separate daemon-owned process surfaces: an
  editor-native, Comint-style shell for ordinary commands and a raw terminal
  for programs that require a terminal screen. The editor shell starts a
  `rho-shell` sidecar inside the agent View. That sidecar embeds one persistent,
  serialized Brush evaluator retaining cwd, variables, functions, aliases,
  Bash-compatible configuration, history, and jobs. The process boundary keeps
  shell-global effects out of the daemon and preserves the View's namespace;
  the neutral bounded `rho-shell-proto` sideband supplies execution and lifecycle
  boundaries.
  Each execution receives a fresh PTY whose slave is Brush's stdin, stdout, and
  stderr. A relay reads that PTY's controller and tags every output byte with
  the daemon-assigned execution id; background descendants retain their
  originating PTY, so late output cannot be attributed to a newer execution.
  The PTY is not the persistent evaluator's controlling terminal. Programs
  requiring `/dev/tty`, persistent terminal job control, or a terminal screen
  belong in the raw terminal.
  Shell start/list/close operations use the main UI control stream, while each
  long-lived attachment uses its own Unix connection or iroh bidirectional
  stream, preserving transport-level prioritization. Closing an attachment
  leaves the explicitly started kernel running. The daemon is the sole owner of
  a bounded structured `ShellState`; each GUI projects that state into a
  read-only buffer beside a client-local writable draft, so pending edits never
  compete with shell-side state or leak between clients. Command-output SGR
  colors and attributes cross this boundary as bounded structured spans and are
  resolved against the client theme; prompts remain semantic client-themed text,
  and raw terminal control sequences never reach the editor buffer. Commands
  that opt into `$PAGER` use the sibling `rho-pager` helper. It relays ordinary
  output through the execution PTY but stops reading its input at bounded page
  boundaries, applying Unix pipe backpressure to the producer. A private Unix
  socket advertised through shell- and execution-scoped capability tokens
  carries only pause and credit actions between the helper and `rho-shell`;
  canonical pager state then crosses the existing sidecar and UI protocols so
  detached or newly attached clients observe the same paused execution. Paused
  pager records are retained independently of transcript blocks, so trimming an
  old execution cannot make its pager uncontrollable. When several background
  pagers pause concurrently, GUI shortcuts control the newest pause first.
- `rho wayland` is an application-agnostic CLI surface for launching and
  controlling programs in isolated headless Sway sessions. It wraps the
  compositor's IPC plus `grim` and `wtype`; the Nix build embeds those tool
  paths and Mesa's software Vulkan driver rather than relying on the caller's
  environment.
- The daemon snapshots the user's login-shell environment and passes it
  explicitly to `rho-workspaces` for daemon-owned commands. Workspace-control
  subprocesses use that environment directly; agent execution shells and
  Claude processes add the primary project's environment through `direnv exec`.
  The GUI's Comint-style surface instead starts `rho-shell` through the agent
  View and lets Brush load normal Bash-compatible interactive configuration
  (`~/.bashrc`, `PS1`, and `PROMPT_COMMAND`), including a configured direnv Bash
  hook. Brush's `brush-v0.4.0` tag (commit `96a26d0c`) is imported under
  `vendor/brush` as a squashed Git subtree and linked only into the sidecar.
  Sandboxed agents remain refused until a sandbox-native startup policy can
  replace the intentionally empty sandbox HOME. The daemon treats the sidecar
  protocol as untrusted: it assigns execution ids, retains accepted command
  text, validates response ordering and bounds, sanitizes output, and exposes
  only canonical structured state to clients.
- `rho-rtc` owns only target-specific WebRTC media and audio devices: native
  libwebrtc plus microphone/playback, and browser WebRTC plus `getUserMedia` and
  HTML audio playback. They negotiate audio only and create no WebRTC data
  channel. Audio capture remains disabled until the daemon confirms that the
  provider sideband is connected.
  `rho-openai-realtime` separately owns the typed OpenAI realtime wire protocol
  and authenticated sideband WebSocket. The daemon resolves OAuth, exchanges
  SDP, extracts the call id, connects that sideband, retains the bounded
  role-bearing transcript snapshot, and routes delegation requests directly to
  Iris: a hidden persisted first-class `AgentRole::Iris` coordinator. Its prompt
  and typed tool schemas are built into `rho-agent`; the daemon hosts the
  stateful fleet operations. Additional requests steer the
  active Iris turn. Commentary and final assistant items are appended directly
  on the provider sideband using commentary or speakable channels; output
  without an active delegation uses session-level context append. Sideband
  failure is terminal. The dedicated GUI-daemon stream carries only SDP and
  lifecycle messages. Session startup includes a bounded visible-fleet
  snapshot. Iris is projected as a synthetic dashboard row, not an ordinary
  agent. Media flows directly between the GUI and provider
  and never traverses the daemon or `rho-core` transcript vocabulary.
- Store crates own concrete persistence formats. Tool crates own concrete tool
  execution.
- `rho-visualizations` owns opaque immutable visualization records, ids, and
  their independent RhoDB table. The daemon enforces only the per-record byte
  envelope while registering and retrieving those records; it does not parse
  or validate SVG. UI protocols carry artifacts only on explicit registration
  or one-shot lazy fetches. The daemon does not know the transcript
  `visualization` fenced-block syntax; `rho-gui` alone recognizes references
  and uses their required 1-to-50 `rows` field as the editor-block height. SVG
  capability safety belongs to GPUI's renderer; `rho-gui` does not layer a
  second SVG
  validator or resource policy over the stored bytes.
- `rho-profiling` owns the thin opt-in profiling lifecycle shared by the
  native GUI and daemon. Dial9 owns CPU sampling, buffering, symbolization, and
  the canonical binary trace; folded and Perfetto runtime exports are non-goals.
  `rho-profiling` contributes typed Rho domain events such as GPUI
  dirty-to-draw and draw durations on Dial9's monotonic timeline. Linux CPU
  coverage and stacks are best-effort and require frame pointers. Frontends
  must start profiling before creating threads they expect perf inheritance to
  cover, and own any domain-specific summary sidecars.
- `rho-tool-shell` owns Codex-compatible unified command sessions:
  `exec_command` yields a process session id when a command remains live and
  `write_stdin` writes to or polls that session. Command continuation state is
  per agent because each agent owns its `ShellTools` instance.
- `rho-web-search` owns the Codex-compatible client-side `web__run` tool and
  the bounded conversion of tool execution context into ChatGPT search input.
  `rho-agent` assembles it as a built-in tool and supplies the configured model,
  recent transcript, and output budget; the tool resolves the same ChatGPT
  OAuth credentials as inference and calls the first-party search endpoint.
- `rho-code-mode` is a tool crate: it runs model-authored JavaScript in an
  in-process V8 isolate (deno_core) and exposes the `exec`/`wait` tool pair.
  Nested tool calls made by scripts leave the crate through a `ToolDispatcher`
  trait implemented by the assembling harness. Each cell retains the immutable
  tool execution context from the `exec` call that created it, so nested tools
  cannot observe a later turn's context; the crate depends only on `rho-core`
  vocabulary. `rho-agent` exposes code mode as an optional runtime feature:
  daemon-side assemblers enable it, while `rho-ui-proto` disables it so native
  clients can share agent identifiers and wire-state projection without
  linking V8.

Claude Code MCP support follows the same boundary: `rho-claude` knows how to
set per-agent MCP environment, but the MCP server that exposes Rho multi-agent
operations lives at the CLI/daemon control boundary. Claude Code can launch a
globally configured `rho mcp-agent-tools` stdio MCP server; that server reads
`RHO_MCP_AGENT_ID` from the Claude process environment, relays tool calls to the
daemon, and the daemon executes parent-scoped spawn, agent mail, interrupt, and
wait against `AgentPool`. The MCP server must not reach into `rho-core` or
provider crates.

Claude turn cancellation uses Claude Code's streaming control protocol and
keeps a healthy child process alive; queued Rho-authored messages are cancelled
by UUID, with a bounded fallback that fully terminates the child before another
process may resume the session. Message-only rewind never restores workspace
files. It records a pending fork from the selected assistant UUID, projects the
truncated transcript immediately, and keeps the old session authoritative in
the database until Claude has durably materialized the fork. A crash-safe
pending descriptor reconstructs that view on load and rotates away from any
partial destination transcript before retrying.

Collaboration creation is role-specific while communication is shared.
`spawn_engineer` always gives jj-backed workdirs isolated child workspaces.
Explicit child revsets resolve in the parent's corresponding workspace context
(or the user checkout for a repository outside the parent's working set), so
workspace-relative symbols and snapshot scope follow the spawning agent.
Sandboxed parents create only sandboxed child workdirs;
detailed delegation and integration guidance lives in the
`delegate-engineering` skill rather than every Engineer prompt. Engineers can
use `ask_advisor` to create an advisory session; PMs cannot. `message_agent` is
an unrestricted bidirectional
mail bus for any known role-prefixed handle, including Advisor context requests;
`wait_agent` waits for incoming mail. Each agent record stores whether it was
created directly, by a PM, or by an Engineer so prompt ownership context is an
immutable creation-time fact rather than inferred later. Advisors retain normal
shell/patch capabilities plus messaging/waiting but cannot spawn or interrupt.
User-facing handles remain `eng-*`, `pm-*`, and `adv-*` over `AgentId`.
Mail delivery is an internal daemon operation, not a UI protocol lifecycle.
It activates a parked recipient when necessary and awaits a per-delivery
acceptance channel. Native Rho acknowledges after its queued event is committed;
Claude acknowledges after its process-local input queue accepts the message,
which intentionally may be lost if the daemon restarts before Claude records
it.
The `eng-mini` tier uses the GPT-5.6 Luna Responses model with xhigh reasoning,
fast mode, and direct tools instead of code mode. Engineers spawned by an
`eng-mini` parent are also `eng-mini`; Engineers spawned by an `eng-alt`
parent are `eng-cheap`. An `eng-cheap` parent spawns `eng-cheap` Engineers and
`advisor-cheap` Advisors; `advisor-cheap` uses GPT-5.6 Terra with xhigh
reasoning.
PMs run with the normal direct tool surface (never code mode), coordinate
exclusively through collaboration tools, and do not receive shell command,
process-input, or patch tools. Their prompts omit repository `AGENTS.md` content
and skills as well as the working-directory Environment section; technical
requests are delegated to Engineers carrying the user's instructions verbatim.
PMs use judgment when routing follow-ups: they may reuse the responsible
Engineer, but spawn a fresh one when warranted or requested or suggested by the
user.
PMs do not receive `wait_agent`: they end their turn after delegation and agent
mail wakes them for the next request. Their prompt states this asynchronous
delegate, acknowledge, end-turn, wake-on-mail, and relay flow explicitly.

The database also stores a global project registry, distinct from each agent's
fixed execution `workdirs`. Projects are keyed by local repository path and
carry a UI-only name plus a description. PM prompts receive only project paths
and descriptions so they can route Engineers without repository access of
their own; UI clients retain names for display and selection.

`AgentRole` also carries a persistence-compatible workflow distinction:
existing Engineer/PM variants are the default workflow, while appended
workflow-bearing Engineer/PM variants carry `AgentWorkflow`. The
`AgentWorkflow::PrFriendly` marker activates `github-workflow` guidance
without changing the visible role label or model binding.

`octo-server` is the daemon's authenticated GitHub API and constrained Git
HTTP component. Rho runs it
in-process on the fixed per-user Octo Unix socket. The user- and agent-facing
PR client is `rho pr` over the normal daemon socket. The daemon owns platform
secret installation and fd-store resume, so Octo receives the GitHub token
only through a RAM-only callback into the sealed platform secret store.

`git-remote-octo` routes each operation by token availability and destination
ref. With a GitHub token, standard GitHub fetches and pushes wholly below
`refs/heads/rho/*` use the private Nix-patched `git-remote-http` through Octo's
Unix-socket smart-HTTP proxy. A push batch containing any other destination is
performed by `git send-pack` over a raw Git-protocol stream instead; no HTTP
push is attempted first. Without a token, and always for SourceHut, fetches use
Git's raw `connect` capability while receive-pack connection attempts fall back
to the helper's `push` capability. The helper reports local remote-tracking
refs for `list for-push`, learns the requested destination refs, and sends every
push through GUI-backed SSH. The inner `git send-pack` performs the authoritative
remote negotiation.

Every connected native GUI registers as a client-held SSH Git transport
provider. For each operation the daemon snapshots the live providers and fans
out the same request. Fetch prompts contain the typed destination; push prompts
also contain the helper's planned destination refs. The first user approval
claims the request and opens a dedicated GUI stream. Every other recipient
receives an outcome-neutral `Done` message carrying only the request id. With
no registered GUI the helper fails immediately, and with no provider claim it
fails after 60 seconds. There is no mid-operation failover.

The winning GUI launches the user's local OpenSSH for a typed host, user, port,
repository, and upload-pack/receive-pack service. Every operation is approved
before OpenSSH starts. For pushes, the GUI independently parses the bounded
receive-pack command list and forwards it only when its destination-ref set
exactly matches the approved plan; any missing, additional, duplicated, or
changed destination denies the operation without another prompt. Rho injects
process-local Git `insteadOf` entries into every agent,
terminal, and internal workspace-management subprocess. Standard
`git@github.com:OWNER/REPOSITORY.git` and
`ssh://git@github.com/OWNER/REPOSITORY.git` remotes then select
`git-remote-octo` without changing repository or user Git configuration.
SourceHut's `git@git.sr.ht:~USER/REPOSITORY` and equivalent SSH URL are also
rewritten, but are never PAT-eligible, so both fetches and pushes require GUI
approval. Explicit `octo://` URLs are restricted to these two hosts, SSH user
`git`, and normalized two-component repository paths: `OWNER/REPOSITORY` for
GitHub and `~USER/REPOSITORY` for SourceHut. Repository components contain ASCII
alphanumeric characters, hyphens, underscores, and periods; traversal components
are rejected. An input `.git` suffix is removed before validation. There is
no failover after an approved GUI claims an operation; retrying starts a fresh
provider race.

`rho-pr-monitor` provides stateless pull-request operations while Octo remains
the authenticated GitHub API boundary. `rho pr status` fetches the current PR,
CI, review, and feedback snapshot for any canonical GitHub PR URL. Monitoring
is caller-owned polling: the daemon stores no subscription and never injects
monitor updates into an agent conversation. The standalone `octo` CLI is not
installed.

The normal UI protocol carries request-id-scoped `rho pr` commands and their
text or bounded log-archive results. PR operations need no agent identity;
GitHub token permissions remain the mutation authority.

The daemon's UI protocol (`rho-ui-proto`) is served over the local Unix socket
and iroh connections from clients enrolled through `rho-iroh-auth` (`rho
daemon --iroh`; approval via `rho iroh approve` stays on the Unix socket).
`rho-rpc` owns the transport below that vocabulary: the raw bounded iroh
authentication exchange, the versioned Unix preface, whole-stream zstd,
bounded Senax framing, Unix/iroh dialing, supervised bounded typed channels,
and flush-aware raw relays. Its authenticated listener classifies completed
iroh handshakes, bounds unknown-client enrollment, and exposes only approved
connections to `rho-daemon`. It also owns the persisted endpoint identity,
endpoint construction, qlog setup, congestion controller, and QUIC credit
transitions. `rho-iroh-auth` owns the trusted-client table, temporary trust,
and pending enrollments in `rho-db`; the daemon supplies only the application
ALPN and retains approve/revoke command routing. Each post-authentication application direction is
one streaming zstd frame with a 128 KiB maximum history window; Senax length
limits apply to decompressed payloads. The iroh ALPN is `rho/ui/3` and Unix
peers exchange the fixed `RHO-STREAM-3` preface before compression.
The protocol crate owns only wire types, limits, and state diffs; `rho-daemon`
projects the richer `rho-agent` runtime state into that wire shape. Consequently UI
clients do not depend on the agent runtime or inherit its optional features.
Daemon startup reads lightweight agent summaries but does not restore every
transcript. Runtime activation is internal daemon policy and serialized per
pool so concurrent UI subscriptions, mail, and integrations cannot construct
duplicate loops. Before returning a newly activated runtime, the pool awaits a
daemon-installed observer that consumes its initial state and arms the
attention watcher; integrations start only after that observer is installed.
Native and Claude runtimes publish non-coalescing successful-completion events
for integrations and separately report settled execution from inside their
state machines. `AgentPool` flushes usage and persists top-level disposition
only when no newer queued turn took over, or when a terminal failure prevents
queued work from proceeding. Attention watchers only project current runtime
state plus that durable disposition and never infer completion from snapshots.
User-input disposition changes are likewise committed by the serialized native
or Claude runtime loop when it accepts the input, not by the calling daemon
connection, so an older completion cannot overwrite a newer queued input.
Each UI control connection independently subscribes and
unsubscribes agent state; activation does not imply observation by any GUI.
`Ready` derives parked-agent attention from persisted disposition alone.
Error and unfinished-turn states are live runtime detail rather than durable
parked-agent attention, so an unloaded pending agent remains pending.
The native GUI initially subscribes its retained selection and up to ten
recently active visible top-level agents. It then
keeps a generous 128-entry transcript LRU; opening another agent beyond that
bound releases the least recently viewed subscription. The daemon confirms a
released or idle-evicted stream with `AgentUnloaded`, and only that server
notice changes retained transcript status to `UiAgentStatus::Unloaded`.
Unix sessions multiplex control and subscribed agent state on one byte stream.
Native iroh sessions keep commands and lifecycle events on a high-priority
bidirectional control stream (exactly one per physical connection); the daemon
opens one unidirectional stream per agent subscribed by that connection, up to
1024, avoiding cross-agent head-of-line blocking. The focused stream has weight
200 and background streams weight 1 within their lower-priority class. Focus
changes travel over the control stream and update transport weights without
reopening streams.
Authentication upgrades both peers from the standard QUIC idle timeout to a
ten-minute same-connection recovery window and raises the daemon's incoming
bidirectional stream credit from its pre-authentication limit. Iroh already
sends five-second transport heartbeats; after ten seconds without receiving an
authenticated QUIC datagram, the native GUI presents a temporary bottom
recovery strip until the same connection responds or finally closes. The
browser client speaks the same native UI protocol over the same iroh ALPN, so
both clients share one wire vocabulary and agent policy. It retains only its
selected-agent subscription, accepts 16 concurrent daemon-initiated streams,
and reserves decompressed frames against a 64 MiB aggregate allocation budget.
The page is a static GPUI/wasm bundle (`crates/rho-gui-web`, its own Cargo
workspace) which boots the portable `rho-gui` dashboard and connects as an iroh
client from the browser.

Native GUI file and diff surfaces share one GUI-local remote-workspace registry
per workspace, and therefore one Zed `language::Buffer` identity and dirty state
per relative path. A dedicated rho-native file channel carries only bounded
open/reload/save requests and coalesced filesystem notifications; editor edits
and undo history never cross the wire. The daemon scopes every operation to the
checkout directory descriptor, uses opaque content revisions for checked saves,
and enforces an 8 MiB per-file limit on reads, writes, and conflict payloads.
Clean buffers reload after watcher events, dirty buffers retain their local text
until checked save reports a conflict, and watcher overflow asks the GUI to
rescan every open path. Syntax parsing remains GUI-local; the daemon runs no
headless GPUI/Zed project.
A diff refresh is a semantic barrier, not a second watcher snapshot: the daemon
persists the requested jj workspace/descendant closure and returns a bounded
manifest containing the exact operation and working-copy commit, parent-side
text, and bounded target type/size/mode descriptors. Current-side text is
deliberately omitted and always comes from the live GUI buffer. The GUI unions
dirty shared-buffer paths into each manifest request so unsaved-only files get
their immutable parent side and survive reconciliation. Conditional one-shot
requests suppress unchanged manifests when that dirty-path set is stable.

Each diff surface has one shared GUI `DiffModel`; split panes create independent
editors over its multibuffer. Stable repository-path keys and subscriptions to
buffers, buffer diffs, and workspace-file events let refreshes reconcile paths
and recalculate hunks without replacing the surface. Manifest invalidations are
lazy: a hidden diff stays subscribed to its retained file channel but cannot
start a jj manifest request. Returning it to an active pane coalesces all hidden
changes into one refresh; a request already started while visible may finish
after it becomes hidden.

The daemon file channel owns save conflict detection. GUI saves are serialized
per path and checked against the content revision from the last open, reload, or
save. Writes use a same-directory durable temporary file, revalidate the target,
atomically replace it while preserving its mode, and verify the installed
revision. The GUI renders modified/deleted results: overwrite or recreation omits
the revision only after explicit confirmation, while discard reloads the daemon
contents into the same buffer entity. Focus loss does not save because arbitrary
external writers can still race the content check and write.

Rho imports iroh as a managed jj subtree and patches its `noq` transport
dependencies to vendored copies. The local extensions preserve strict stream
priorities, add relative send-stream weights within each equal-priority
fair-scheduling class, and allow an application-authenticated pair to
coordinate a post-handshake idle-timeout override. Weight 1 retains upstream
behavior; higher weights receive proportionally more packet-writing turns
without changing anything on the QUIC wire. The fork's default and relay path
idle limits are ten minutes as well; this default also applies to custom paths
created through rho's iroh build. Transport scheduling owns
connection bandwidth allocation, while application-level stream selection and
coalescing remain UI protocol policy. Native GUI and daemon endpoints enable
noq's qlog instrumentation when `QLOGDIR` is set, writing `rho-gui-*` and
`rho-daemon-*` traces respectively for transport-level diagnosis. Iroh uses
CUBIC congestion control by default; `rho-rpc` selects BBR3 for
daemon-to-client traffic when `RHO_IROH_BBR3=true`, without requiring the
client to use the same controller.


Dependencies should flow from higher-level assembly/policy crates toward lower
reusable crates. The shared `rho-core` crate must not depend on provider, agent,
store, tool, or CLI crates.

## Transcript and inference data ownership

`rho_core::ItemBlock` is the transcript unit passed between stores, agents, and
providers.

- `ItemBlock::Local` is local/user/tool/agent-owned transcript data.
- `ItemBlock::InferenceResponse` is provider-owned output plus the optional
  provider response id needed for provider-side chaining.
- Provider-specific data that must be replayed but is not part of the shared
  semantic vocabulary is carried as `rho_core::ProviderItem` with an opaque JSON
  payload and a coarse `ProviderItemKind`.

`rho-agent` is the canonical owner of the in-memory transcript during an agent
run and decides when to persist blocks. Inference crates may derive requests from
the transcript but should not mutate it directly.
