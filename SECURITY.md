# Security and reliability context

`rho` is a Rust toolkit and CLI for local AI-agent workflows. The main
production/runtime surfaces are local terminal use, local transcript/session
stores, local shell/apply-patch tools, and inference crates that talk to external
AI APIs.

## Trust boundaries

- Local users control prompts, session names/paths, inference auth setup/import,
  and tool inputs.
- Authenticated GUI clients may enable or disable OAuth namespaces.
  `rho-inference` exposes only those safe settings, namespace names, and the
  active namespace name; bearer and refresh tokens remain in credential files.
  Its persisted selection record is internal. An in-flight request may finish
  under the previous selection; subsequent inference, web search, and realtime
  requests observe the replacement. Authentication failures fail the request
  and never trigger automatic account failover.
- `rho-inference` owns the sole ChatGPT quota poller and provider-prefixed quota
  tables. It resolves every enabled configured namespace roughly every ten
  minutes.
  Only namespace names, percentages, and reset times are persisted or sent to
  clients; provider account identifiers remain memory-only.
- The explicit `eng-gemini` mode reads one separate Antigravity credential file
  under `auth.d/antigravity/`; it is never scanned as a ChatGPT namespace or
  considered by ChatGPT routing. Its refresh token, access token, and Google
  project id remain daemon-side. GenerateContent responses are capped at 8 MiB,
  HTTP/error text is bounded, and dropping or aborting the session cancels its
  request/retry task.
- Inference APIs and streamed inference events are remote, semi-trusted inputs and
  must be parsed defensively.
- Authenticated clients and view-aware tools may supply image files. Rho accepts
  at most 20 user images within the UI frame budget and 10 MiB per source,
  decodes each under fixed dimension and allocation limits, resizes to a bounded
  vision patch/pixel budget, and writes a fresh single-frame PNG before durable
  context or provider upload. This strips container metadata, color profiles,
  animation, and the source encoding. `view_image` detail `original` preserves
  resolution only within its separate 6,000-pixel/10,000-patch budget. In
  sandbox views, `view_image` uses the
  same checked in-process path mapping as patch writes and rejects paths outside
  the workdirs; ordinary views retain their documented ambient filesystem
  authority.
- A watched agent presentation sidecar sends a bounded (10 KiB total, 1 KiB
  per message), text-only recent transcript excerpt to Luna to derive a
  title/activity cache. Native agents commit source events directly; Claude
  commits individually bounded user and assistant text from confirmed CLI
  messages, then reconciles that mirror against the selected JSONL chain on
  load and rewind. XML wrapping is structural context, not a trust boundary:
  transcript text remains semi-trusted provider input. Requests are
  globally bounded, cancelled when the last UI watch is released, and never
  feed their output back into the agent transcript.
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
- The native GUI always retains fixed-size in-memory rings of GPUI frame timing
  and numeric editor/display-pipeline timing. An explicit
  `Ctrl-Alt-Shift-P` action sends a versioned JSON snapshot (at most 8 MiB) over
  the already authenticated connection to one active daemon. It contains
  precise timings, numeric window/thread IDs, edit counts, affected row ranges,
  map row totals, pending-batch counts, display flags, and embedded-browser
  scene/barrier IDs with production, coalescing, receipt, scheduling, paint,
  and frame-ack timing. Browser markers contain no URLs, pixels, or page
  content; snapshots contain no buffer text or filesystem paths. The daemon
  chooses a unique filename and writes a mode-0600 file under
  `dirs::state_dir()/rho/gui-telemetry` (normally
  `~/.local/state/rho/gui-telemetry`). There is no automatic upload, expiry, or
  deletion; users control retention of these local diagnostic files.
- The native GUI keeps a client-only append-only action journal under the rho
  state directory. It may contain agent/page identities, Desk locations,
  minibuffer prompts and input, navigation, and dealer decisions with precise
  timestamps. A dedicated local writer commits each event in its own redb
  transaction; the journal is
  never uploaded, analyzed, or used to adapt behavior automatically, and has no
  automatic retention, so users control it like other sensitive local state.
- The daemon's structured Desk store accepts tree operations only from an
  allocated user replica and node-text operations only from allocated user or
  agent replicas. Client operations cannot create or structurally edit
  machine-owned nodes, and machine-node text is read-only. Individual node
  text retains the Desk's 4 MiB bounds; delete batches, text edit ranges,
  transactions, tags, and fractional-order depth are independently capped
  before persistence. The initial org import runs locally against the previous
  Desk snapshot and sends no content elsewhere.
- Opt-in GUI and daemon Dial9 profiles contain thread names, function symbols,
  local source paths, precise activity timing, and frontend marker metadata.
  GUI editor markers include numeric edit counts, affected row ranges, map row
  totals, pending-batch counts, and display-stage flags, but not buffer text or
  file paths.
  They do not intentionally include transcript data, but remain local
  diagnostic files whose destination and retention are the user's
  responsibility. Always-on timing collection does not enable Dial9 or CPU
  sampling. On Linux, Dial9 normally samples through `perf_event_open`;
  its clock-timer fallback owns process-global `SIGPROF`, installs a chained
  process-global `SIGSEGV` handler for safe stack reads, and samples only
  registered threads. The `SIGSEGV` handler is not restored, but profiling
  runs until process shutdown. The fallback must not run alongside another
  in-process profiler using `SIGPROF`. Perf sampling frequency is per
  inherited thread, and inherited child-process samples are collected before
  Dial9 discards them, so overhead can scale with process and subprocess
  parallelism. The single-file trace grows linearly with profiled CPU/frame
  activity.
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
  The driver sets an exact process-local marker for rho-browser's QA-only SHM
  root transport; ordinary GUI launches cannot silently select it. The path
  accepts only checked ARGB/XRGB buffers, copies validated rows into owned
  memory, releases the source immediately, retains the 16 MiB ancillary and
  32 MiB current-scene bounds, and preserves one-scene coalescing. It does not
  test the production DMA-BUF/fence invariants.
- Native embedded pages run pinned Brave Origin with the invoking user's full authority
  and the ordinary Brave Origin cookie/storage identity. Rho does not synthesize
  or mount browser policy. The NixOS Home Manager Brave module installs user-level
  policy for Brave Origin. That policy disables
  Rewards, Wallet/Web3, VPN, Leo, News, Talk, Tor, Playlist, Speedreader,
  Wayback integration, Sync, background mode, product analytics, usage pings,
  web discovery, metrics, command-line warnings, and default-browser prompts.
  It also enables Brave's maximum-savings Memory Saver mode; page unloading is
  performed by Brave's native eligibility policy rather than forced through the
  extension API.
  Rho installs its native-messaging manifest in Brave Origin's ordinary XDG config
  tree. Ordinary Brave and Rho must never run concurrently: Chromium's singleton
  is scoped to the user-data directory, and Rho terminates the process it starts.
  Brave retains its native process and renderer sandboxes.
  `RHO_CUSTOM_BRAVE_BIN` selects the locally built, Rho-patched Brave artifact;
  the NixOS Brave profile sets it to a configuration wrapper around its pinned
  package. There is no stock-browser fallback: `rho-browser` requires the
  component loader and private tab API from that build. The Nix wrapper adds the
  process-scoped tab-strip hiding switch. Rho accepts only bounded HTTP(S) launch
  URLs, holds an exclusive advisory lock on its runtime,
  and exposes Brave only to one private Wayland socket. One bundled MV3
  extension is registered by Rho's custom Brave build as a component extension
  from the isolated client-state directory. Updated worker and DOM-adapter code
  is therefore registered on browser restart without rebuilding Brave; its
  source is still supplied only by the installed `rho-gui` binary.
  The extension has `tabs`, `storage`, `clipboardWrite`, and `nativeMessaging`
  privileges. Clipboard writes occur only for explicit Vim copy commands
  handled synchronously from trusted keyboard input. The
  allowlisted `rhoPrivate.tabs` API stores UUID page identity in browser tab
  session data instead of exposing it through visible tab groups. Its bundled
  content script runs on HTTP(S) documents only. Its isolated-world,
  document-start `window` capture listener owns Vim modes and synchronously
  consumes matched commands, active prefixes/counts, and unmodified Hints-mode
  input; unmatched top-level keys and focused-control conflicts continue to the
  website as their original trusted events. The page agent performs nested
  native smooth scrolling, focus, visible-element label/text hints, and scroll
  marks locally. Find and Caret remain disabled pending native browser integration.
  Hint candidate text is not persisted or sent to the worker. An explicit `gB`
  command stores only the page origin in
  extension-local blacklist state.
  Browser history and
  reload requests contain only a fixed command name and are handled by the
  worker for the active sender tab. Hint activation currently uses DOM
  `element.click()`, which is not a trusted synthetic click, though it executes
  during the trusted key event's user-activation window. Brave's native Memory
  Saver discards eligible inactive tabs. Websites cannot read or modify the
  browser-owned UUID session metadata.
  URL-free lifecycle
  diagnostics sent over the
  native bridge contain page UUIDs, ephemeral tab IDs, and tab state booleans,
  but no URLs, titles, pixels, or page content. Brave native messaging starts a
  copy of the
  `rho-gui` executable in bounded stdio-relay mode; it connects to the existing
  GUI through `$XDG_RUNTIME_DIR/rho-browser.sock`, whose filesystem mode is
  0600. The socket is a same-user trust boundary and uses no additional
  authentication. No TCP listener, CDP/remote debugging, or arbitrary injected
  website script participates. The compositor binds only an
  exactly-one-pending-window/exactly-one-unbound-top-level pair; no activation
  token is issued or accepted. Additional or ambiguous top-levels fail closed.
  Browser content also fails closed unless GPUI and Brave share an
  importable DMA-BUF format on the selected DRM render node and explicit-sync
  eventfd support. Root SHM content, missing acquire/release points on any
  DMA-BUF surface, unsupported buffer transforms, and non-SHM/non-DMA-BUF
  ancillary buffers are rejected rather than displayed under a guessed mapping.
  The opt-in host-subsurface passthrough additionally requires the host to
  advertise the exact DMA-BUF format/modifier. Where the host lacks Wayland's
  legacy explicit-sync protocol (including niri), Rho imports Chromium's acquire
  sync file into every DMA-BUF plane's implicit reservation object before attach.
  At `wl_buffer.release`, it exports and waits for all implicit reader/writer
  fences before returning the buffer to Chromium. With explicit sync,
  fenced releases are not returned to Brave until their sync files signal.
  Synchronized commits are published as one versioned surface tree, preventing
  buffers or hit-test geometry from different Wayland transactions from mixing.
  Ancillary SHM buffers are the bounded exception: only ARGB8888 and XRGB8888
  rows with checked dimensions, stride, pool range, per-surface size, and total
  scene size are copied into owned memory and released immediately. GPUI/WGPU
  performs the only rendering; Smithay retains protocol, popup/grab, and input
  state but does not render browser pixels. With `RHO_BROWSER_PASSTHROUGH=1`, an
  eligible single-node DMA-BUF is instead sampled by the host compositor through
  a below-parent subsurface; all other scenes retain the GPUI/WGPU path.
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
- Octo's GitHub token reaches the daemon over the UI socket (`rho pr init`
  reads it from stdin) — never via argv, exec-time environment, or files. The
  daemon's `SecretStore` holds it in a sealed memfd
  and stashes/reclaims it via the systemd fd store (`FDSTORE=1`/`$LISTEN_FDS`),
  so the token never touches disk and survives daemon restarts but not reboots.
  Token values must not appear in logs or errors.
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
- `rho-pr-monitor` uses Octo only for bounded authenticated GitHub API calls
  behind `rho pr` on the normal daemon socket. Status and CI checks are
  stateless reads for any canonical HTTPS GitHub PR URL. PR commands need no
  agent identity;
  GitHub token permissions authorize mutations. Clients able to reach the
  privileged daemon socket already have equivalent control. The daemon stores
  no PR subscriptions or polling loop and never injects GitHub content into an
  agent conversation. `rho pr checks --watch` is client-local polling over
  independent stateless reads, not a stored daemon subscription. GitHub
  validates the PR/review-comment relationship for inline replies, which use
  an explicit numeric GitHub comment ID. GitHub permissions govern mutations
  and GitHub retains its edit history. Snapshots
  are capped at two pages per feedback surface, 100 CI
  records per API family, and 4 MiB per GitHub JSON response. CI log archives
  are stream-limited to 48 MiB on
  both socket hops; extraction permits at most 1,000 files, 16 MiB per entry,
  and 128 MiB total expanded data. GitHub comments, bot output, paths, links, and diff
  hunks remain prompt-injection-capable input;
  Engineers must validate claims against the repository before changing or
  executing code, and summarize meaningful milestones to their parent rather
  than forwarding raw review text.
- Inter-agent mail activates parked recipients internally and waits for the
  recipient loop to accept the input; it never creates a GUI subscription.
  Native Rho acknowledges only after committing the queued event, so successful
  delivery survives daemon restart. Claude acknowledges only after writing the
  input to its live CLI process, but has no separate RhoDB mailbox; a daemon or
  CLI crash after that write but before Claude records the input may lose that
  rare message by design. Agent-response subscriptions use the same delivery
  path: native recipients acknowledge after queue persistence, while Claude
  recipients retain the weaker acceptance guarantee above. A crash before a
  response is queued, or a transient delivery failure, can lose that response;
  subscriptions are not an outbox and do not replay missed deliveries.

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
  reconnects from one endpoint. Once the QUIC handshake authenticates its
  endpoint key, a persistently or temporarily trusted client bypasses the
  64-permit enrollment semaphore but still completes the explicit first-stream
  confirmation and acknowledgement. At most 64 unknown-client enrollment
  exchanges run concurrently, and waiting for enrollment capacity is also
  bounded to ten seconds. Each connection permits at most 16 queued
  bidirectional streams before approval, and both client and server bound the
  auth exchange itself to ten seconds.
  Approved iroh clients receive 1024 bidirectional-stream credits and both
  peers extend the connection and path recovery window to ten minutes. This is
  an intentional trusted-client capability rather than a post-authentication
  denial-of-service boundary. The daemon's iroh secret key lives in the local
  rho database.
  The auth stream remains raw so unauthenticated input cannot invoke a
  decompressor. All later application directions use ALPN `rho/ui/4` and one
  streaming zstd frame with a 128 KiB maximum decoder window. Local Unix peers
  must first exchange the fixed, ten-second-bounded `RHO-STREAM-4` preface.
  Senax frame limits are enforced on declared decompressed lengths before
  allocating payloads; compression is not an authorization or integrity
  boundary.
  After authentication, each native GUI control connection explicitly
  subscribes agent state and may accept up to 1024 daemon-initiated
  unidirectional agent-state streams. Subscriptions are connection-local and
  may internally activate a parked runtime, but daemon activation alone never
  exposes a transcript to a GUI. Authorization and commands remain on the
  authenticated control session. Stream weights are sender-local scheduling
  metadata and are never trusted from the network. More than 1024 simultaneous
  subscriptions on one connection closes that connection rather than silently
  serving incomplete state.
  Agent frames retain the 64 MiB per-frame bound, and the native GUI reserves
  each declared payload against a connection-wide non-FIFO atomic byte budget before
  allocation, bounding concurrent length-prefix-driven frame allocations to
  128 MiB while allowing small frames to bypass a waiting large allocation.
  The reservation remains attached to the decoded GUI event until consumption,
  so slow UI handling cannot refill an unbounded queue of large agent frames.
  The web UI retains only one selected-agent subscription, advertises 16
  unidirectional stream credits for replacement overlap, and applies a 64 MiB
  aggregate decompressed-frame allocation budget. A malformed individual
  agent stream is discarded without tearing down unrelated control traffic.
  Setting `QLOGDIR` opts the process into writing a qlog file for every iroh
  connection. Qlog records transport metadata such as endpoint addresses,
  connection IDs, packet timing and sizes, stream IDs and offsets, loss, and
  congestion state, but not UI frame payload bytes or cryptographic secrets.
  Treat captures as sensitive diagnostics, use a private directory and bounded
  capture window, and remove them after analysis; rho does not rotate or cap
  their aggregate disk usage.
  Enrollment approval is also accepted from already
  trusted remote clients (they are fully privileged anyway).
- An iroh host attached by `rho-gui`
  (`--attach <name>=iroh:<endpoint-id>@<ssh-dest>`) generates its client key
  in process memory and never persists it. Before connecting, it runs the user's
  OpenSSH client to execute `rho iroh trust-in-memory <endpoint-id>` on the
  daemon host, so no enrollment code or rejected connection is needed. An SSH
  destination is required for native iroh connections because an ephemeral GUI
  key cannot survive a manual approval/restart cycle. One GUI process binds a
  single client identity and reuses it for every daemon it attaches, so each
  daemon sees the same public key and each enrolls it separately over its own
  SSH login; no daemon gains anything from another's enrollment. The SSH host
  configuration and host-key verification are the authorization boundary and
  insecure fallback is not attempted. Existing legacy key files are ignored
  and left untouched. Once the GUI process exits, the daemon retains only the
  unusable public endpoint id until idle expiry or daemon restart.
  `--remote-rho <path>` selects the remote executable (default `rho`) and
  accepts only a nonempty shell-safe path alphabet; it is not an arbitrary
  remote shell command.
- The browser UI (a static GPUI/wasm page in `crates/rho-gui-web`, hostable anywhere) is
  an iroh client like any other: it connects on the native UI ALPN and passes
  the same per-key enrollment before the daemon serves it. Its session uses
  the same framed native UI protocol and therefore has the same privileges as
  the native GUI. The browser uses a user-verifying WebAuthn credential's
  PRF extension to derive a stable, daemon-specific iroh key on each connect;
  only the non-secret credential id and daemon id are kept in local storage.
  The PRF output and derived iroh key remain in browser memory and are never
  persisted. The hosting origin and all JavaScript it serves are fully trusted.
  On static hosts that cannot set response headers (including GitHub Pages),
  the page's same-origin COI service worker adds COOP and COEP after its first
  activation and reloads the page so threaded wasm can use `SharedArrayBuffer`:
  code running after the user approves the WebAuthn prompt can read the
  derived enrolled key and thereby gain persistent daemon access. Deploy the
  page on a dedicated origin without third-party scripts and treat its build
  and publishing pipeline as security-critical. The page refuses to run when
  framed and ships a restrictive meta CSP. GPUI background work runs in module
  workers created from same-page blobs; `worker-src` permits those blobs, while
  the locally carried `wasm_thread` bootstrap avoids JavaScript `eval` and
  imports only the build's same-origin wasm-bindgen shim. Production hosting
  must additionally send `Content-Security-Policy: frame-ancestors 'none'` as
  an HTTP header.
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
- Inbound data on the iroh ALPN is remote, semi-trusted input: oversized UI
  protocol frames are rejected (`MAX_FRAME_LEN`) and malformed frames end the
  connection.
  Raw Git tunnels have no total-byte bound because repository transfer sizes
  are intentionally data-dependent; their relay uses a fixed 16 KiB buffer,
  flushes the zstd writer after every chunk, and propagates half-close so small
  request/response exchanges cannot deadlock behind compressor buffering.
  Dropping a supervised typed channel cancels/resets its transport task;
  sender-driven graceful completion instead finishes the zstd frame and
  half-closes before the task joins.
- An authenticated native UI client may request a diff for any workspace it
  can already open through the fully privileged UI protocol. A refresh is a
  persistent jj write: it snapshots that workspace and descendant workspace
  commits under the per-repo lock and may therefore rebase/materialize those
  descendants. Unrelated workspace branches are not scanned. The returned
  repository epoch is consumed under the same lock, avoiding mixed-operation
  manifests. The blocking job owns that lock, so an RPC timeout cannot admit a
  concurrent jj mutation while the timed-out worker finishes.
- Diff manifests expose repository-relative paths and bounded parent file
  contents to the requesting GUI; current-side contents stay in the GUI's
  local editor buffers. Reads are limited per file, aggregate I/O, aggregate payload,
  and file count; both parent materialization and target text/binary probes
  charge the aggregate I/O budget. Dirty-path requests have count and path-byte
  limits. The workspace file channel enforces an 8 MiB limit on every live-file
  read, write, and conflict payload; the GUI also caps
  aggregate live text before building diffs. Daemon loads have a semaphore and
  30-second wait, and use a low-priority one-shot iroh stream. Both encoded and
  raw frame writers enforce the same 64 MiB bound as readers.
- Hidden diff surfaces retain their workspace watch stream and local buffer identity,
  but watcher/buffer invalidations cannot initiate jj manifest RPCs until that
  model is shown in an active pane. Hidden changes coalesce; an already-started
  request may still finish after the surface is hidden.
- Workspace file requests accept only normalized relative paths and resolve
  them from the authorized checkout directory with Linux `openat2`
  `RESOLVE_BENEATH|RESOLVE_NO_MAGICLINKS`; absolute paths, traversal, final
  symlinks, and checkout escapes are rejected. Payloads are byte-preserving and
  invalid UTF-8 is rejected by the GUI instead of being rewritten. Checked saves
  compare a SHA-256 content revision, write and sync a same-directory temporary
  file, revalidate, atomically rename, sync the parent, and verify the installed
  revision; existing permission modes are preserved. Only an explicit user
  overwrite/recreation response omits the revision. This is not filesystem CAS:
  an external writer can still race between revalidation and rename or replace
  the path immediately after verification. Rho therefore still does not
  focus-loss autosave.


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
- The daemon's centralized Claude quota poller is a deliberate exception to
  project-scoped Claude startup. The isolated `rho-claude-usage` crate runs
  Claude Code directly in a dedicated, empty `0700` state directory, with safe
  mode, tools, hooks, plugins, MCP, and transcript history disabled. It
  automatically accepts Claude's trust prompt only for that verified-empty
  directory. The bounded PTY probe uses the snapshotted user environment and
  configured PATH overrides so it shares Claude's user auth without loading
  project configuration or `direnv`; timeout cleanup terminates and reaps the
  probe's process group.
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
- Pager-aware commands receive `rho-pager` through `PAGER`, `GIT_PAGER`, and
  `JJ_PAGER`.
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

## Realtime voice provider (`rho-rtc` / `rho-openai-realtime`)

- Native and browser Iris start when the user toggles voice. The dashboard row
  and both clients' controls expose that voice-session state. `rho-rtc`
  captures and plays audio using target-specific native or browser facilities.
  Encoded media flows directly between the GUI-owned WebRTC peer and ChatGPT,
  never through the daemon. Audio capture stays disabled until sideband
  readiness. Rho creates no WebRTC data channel; all provider control traffic
  uses the daemon sideband.
- The OAuth bearer token remains daemon-side. A GUI sends a bounded SDP offer
  over a dedicated authenticated UI stream. The daemon resolves ChatGPT OAuth,
  calls the realtime signaling endpoint under a timeout, validates the returned
  `rtc_*` call id, and returns only the bounded SDP answer. Delegation payloads
  and responses never traverse the client link.
- `rho-openai-realtime` connects an authenticated daemon-side WebSocket bound
  to that call id. Sideband text messages are remote, semi-trusted input:
  WebSocket frames and decoded events are capped at 1 MiB, known delegation and
  transcript forms decode into tagged Rust types, unknown top-level events are
  ignored, and malformed known events terminate the session without panicking.
  Binary frames are rejected. Provider commands are typed, split on UTF-8
  boundaries into at most 500-byte chunks, and sent under a timeout. Sideband
  connection or closure is terminal; there is intentionally no WebRTC
  data-channel fallback.
- The daemon retains a bounded 16 KiB role-bearing conversation snapshot and a
  bounded visible-fleet startup snapshot, then routes typed delegation text to
  the hidden persisted `AgentRole::Iris` coordinator. `rho-agent` gives that
  role only its built-in Iris schemas and dispatches them to the daemon's typed
  fleet-control host. Only one backend turn is active per realtime call; later
  delegations steer it and are acknowledged directly on the sideband. Iris
  output is capped at 16 KiB per active handoff before provider append.

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
checkout. The mode-0600 generated source file remains alive while the agent loop
owns its persistent mount namespace, is rewritten in place before a cold Claude
respawn, and is removed when that loop is dropped. A successful soft turn
cancellation keeps the process and its private prompt mount alive for later
turns; a failed cancellation terminates the process while retaining the prompt
source for a later respawn. Loaded
`AGENTS.md` content therefore has the same
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
shared for ordinary agents. Spawn revsets are resolved and snapshotted from the
parent's corresponding workspace, not the user's root checkout. Sandboxed
parents create sandboxed owned workdirs even for repositories outside their
working set; sharing an outside ordinary checkout or spawning into a plain
directory is refused. Advisors intentionally join their caller's workdirs and
keep shell and patch tools for read-oriented investigation and scratch
experiments. They may message other agents and wait for replies, but cannot
spawn or interrupt, and are instructed not to implement changes.

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

## Visualization artifacts

- Storage boundary: visualization content is model-authored opaque input. The
  daemon enforces a 4 MiB per-record byte limit but does not parse, sanitize, or
  validate SVG and does not impose record-count or aggregate-byte quotas.
  Content-addressed ids deduplicate registrations; records are immutable and
  retained indefinitely. The independent artifact table does not change the
  agent database format.
- Render boundary: the GUI passes stored SVG bytes to GPUI without structural
  or resource validation. Model-authored SVG is allowed to exhaust GUI render
  resources, and a malicious or compromised daemon exhausting a GUI client is
  likewise accepted. GPUI's SVG renderer disables usvg's string image-href
  resolver at the source, preventing artifact-supplied filesystem paths from
  being loaded; this is a renderer capability restriction, not a work bound.
- Presentation: artifact content travels only on an explicit one-shot lazy
  fetch. The daemon has no knowledge of transcript reference syntax.
  `rho-gui` recognizes dedicated `visualization` fenced blocks and uses the
  model-supplied `rows` value from 1 through 50 as their block height.
  GPUI's SVG renderer performs rasterization; a failed fetch or raster displays
  a non-interactive error placeholder.
- Tests: `rho-visualizations` covers immutable round trips, deduplication,
  opaque invalid content, and the per-record byte limit.
  `rho-ui-proto` covers visualization wire round trips; `rho-gui` covers marker
  parsing (including required row sizing and rejection of malformed or
  nested fences), inline replace-block insertion, removal when the transcript
  reference changes, and GPUI SVG rasterization.

## Future review notes

Future changes that add providers, credential storage, transcript persistence,
subprocess execution, filesystem writes, or background tasks must update this
file and document their primary trust boundaries, resource bounds, cancellation
behavior, and tests.
