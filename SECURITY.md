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
  under the previous selection. Worker inference requests observe account and
  credential replacements when the daemon's push arrives; requests started
  before delivery may use the previous snapshot. Expired or disconnected
  snapshots are unusable, and rate-limit retry acknowledgments fence delivery
  of the replacement. Web search and realtime resolve through daemon policy. Authentication failures fail the request
  and never trigger automatic account failover.
- `rho-inference` owns the sole ChatGPT quota poller and provider-prefixed quota
  tables. It resolves every enabled configured namespace roughly every ten
  minutes.
  Only namespace names, percentages, and reset times are persisted or sent to
  clients; provider account identifiers remain memory-only.
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
- Native and Claude agents may send at most 1 KiB of the first user/task
  message to Luna for a one-shot, text-only title. The attempt is durable before
  dispatch, globally limited to four concurrent requests, and bounded by a
  30-second timeout including queueing. Failure, cancellation, restart, and
  rewind do not retry it. Task text remains semi-trusted input; the output is
  restricted to a 30-character ASCII title and never enters agent context.
  Existing names win over late completion. Naming has no tools, evolving
  transcript access, activity classification, or UI-watch trigger.
- Client-side web search sends the configured model and a bounded recent
  transcript excerpt to ChatGPT's first-party search endpoint using the same
  OAuth identity as inference. Search responses are remote, semi-trusted tool
  output: HTTP bodies are capped at 4 MiB, parsed, and independently truncated
  to the tool output budget before entering context.
- Local filesystem state may contain transcripts and OAuth credentials;
  credential files are secrets.
- The Slack client mirror contains message history and unsent drafts. Draft
  attachment bytes are private local state and are uploaded only when the user
  submits that draft. A draft accepts at most 10 files, 25 MiB per file, and
  100 MiB total; path attachments are size-checked before reading. Composer
  text and attachment bytes use separate tables so an ordinary keystroke never
  decodes or rewrites file content. Failed and interrupted sends retain the
  durable draft for explicit retry; they are never replayed automatically.
  Slack archive links navigate natively only for the exact HTTPS archive host
  learned from RTM's workspace domain (and retained in the local mirror), with
  validated channel and message/thread timestamps. Other URLs stay browser links.
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
- A Desk connection binds one persisted random `DeviceId` to a daemon-assigned
  node/text namespace before writing. Cell mutations are bounded, atomic, and
  idempotent by stamp; they must advance that device's frontier without an
  unobservable clock jump. Clients can create only complete user-owned notes
  in their namespace. Raw kind cells enforce ownership even for deleted rows;
  machine nodes are read-only except for exact state/defer/parent changes
  recorded by a validated user verdict. Node text is namespace-bound,
  causally complete, and capped at 4 MiB; tags, cell strings, paths,
  transactions, verdict changes, and mutation write counts have independent
  bounds before persistence. Failed validation commits no frontier, cell,
  verdict, text, or mutation-log entry.
  The one-time native-tree V1 conversion is gone, with its marker and its
  frozen decoder: it ran on every daemon it was ever going to run on. A
  database that never took it has no cell state to read, and going back to
  it means the pre-upgrade database copy.
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
  GUI through a mode-0600 relay socket beside the selected local daemon
  socket. The socket is a same-user trust boundary and uses no additional
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
- An agent's place (its workset, working directory and mode) is fixed at
  spawn, persisted on the agent record, and provides version isolation rather
  than access isolation: the mount namespace presents the workset at `/src`
  and, in view mode, hides the rest of the host, but the process is the same
  user as the daemon (`WORKSET.md`). Worksets are plain directories under the
  state root; discarding one removes its directory, and the mirrors under
  `stores/` that clones borrow objects from are never removed.
  Apply-patch translates absolute paths inside the workset to the host
  directory, so in-process file writes follow the same mapping as
  namespaced commands.
- Sandbox workspaces were a narrower, opt-in boundary for native agents
  (historical; no longer created). Rho created an isolated workspace, masked
  its original VCS metadata in the command mount namespace, and pointed Git
  at a separate synthetic baseline. Child commands receive a fail-closed Landlock policy: sandbox
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
- The embedded Octo server listens only on a Unix socket beside the daemon
  socket and
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
  Native Rho acknowledges after enqueueing the event for ordered replication;
  a crash can lose an unflushed accepted message. Claude acknowledges only after
  writing the input to its live CLI process, but has no separate RhoDB mailbox; a daemon or
  CLI crash after that write but before Claude records the input may lose that
  rare message by design. Agent-response subscriptions use the same delivery
  path: native recipients acknowledge after replication enqueueing, while Claude
  recipients retain the weaker acceptance guarantee above. A crash before a
  response is queued, or a transient delivery failure, can lose that response;
  subscriptions are not an outbox and do not replay missed deliveries.

## Slack media

- Slack message files use the authenticated workspace session because Slack's
  private file endpoints require it. Custom emoji and profile avatars are different:
  `emoji.list` and `users.info` may return public CDN or external URLs.
  Emoji and avatar asset GETs never carry the Slack
  bearer token or `d` cookie, including across redirects.
- Custom emoji metadata is capped at 10,000 definitions. A conversation
  reconciles at most 256 occurrences per changed message row. Avatars are looked
  up lazily, once per author per session. Both kinds of asset responses
  stream into a 512 KiB cap before persistence. Raster dimensions are capped at
  512×512, GIFs at 60 frames, and each conversation view retains at most 16 MiB
  of estimated decoded BGRA pixels. SVG is not decoded because its allocation
  cannot be bounded by the raster header check.
- Emoji and avatar cache entries are keyed by a hash of the URL and have explicit loading,
  ready, or failed state. A failed fetch or decode remains a readable shortcode
  and is not retried until a new Slack session; a missing avatar leaves the
  author's name intact. Conversation decorations are
  owned per message row; unchanged rows retain their anchors and decoded asset
  without rescanning the transcript or rereading the cache file.
- Tests exercise URL and alias parsing, credential-free public asset requests,
  response/dimension failure fallback, UTF-8 buffer offsets, active concealed
  folds plus fixed-cell image inlays, and preservation of shortcode text for
  copy and search.

## Slack app interactions

- App messages are remote content, not local commands. Buttons and selections
  dispatch only after explicit user activation, using the authenticated Slack
  API. Confirmation fields require a separate confirmation; modal submissions
  have an explicit Submit/Cancel step.
- Each dispatched app action uses a fresh 128-bit random correlation token.
  Dialog and view-opening events must consume a locally pending token within
  60 seconds; unsolicited, expired, and duplicate opening events are ignored.
  Modal state is submitted through Slack's views API, not to arbitrary URLs
  supplied in message content. URL buttons use the existing browser path only
  after user activation.

## Remote UI transport (iroh)

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
  decompressor. All later application directions use ALPN `rho/ui/8` and one
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
  A malformed individual agent stream is discarded without tearing down
  unrelated control traffic.
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
  can already open through the fully privileged UI protocol. A refresh runs
  under the workset's operation lock. The git-based snapshot behind it is
  not implemented yet; until it is, the request fails cleanly.
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
  but watcher/buffer invalidations cannot initiate manifest RPCs until that
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
  project configuration or a dev shell; timeout cleanup terminates and reaps the
  probe's process group.
- Internal workspace-management commands receive only that user environment.
  Agent shell commands, terminals, the shell sidecar and Claude Code
  additionally run in the dev shell of the nearest flake above their working
  directory, built by `rho-devshell-builder`. A project's flake and its
  `shellHook` are trusted local code with the same authority as the agent shell
  tools they configure. Evaluation is pure, but building the shell can realise
  derivations through the Nix daemon like any `nix develop`. Built shells are
  cached by the agent host for all of the owner's worksets, whose builders
  store entries and check them against their own checkouts. The cache's socket is in the
  shared cache directory that views bind, so any command in a view can store
  entries too; an entry is as trustworthy as those worksets, which already
  share the agents' `~/.cache`.
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
  `PROMPT_COMMAND`; any configuration reached from those hooks is
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
- The Rho-owned agent variable `RHO_AGENT_ID` is supplied explicitly to agent
  commands rather than copied incidentally from the daemon environment.
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

- Iris starts when the user toggles voice. The dashboard row exposes that
  voice-session state. `rho-rtc` captures and plays audio using native devices.
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
managed workspaces, Rho provides a separately authored Claude integration
supplement covering the notebook interface, agent coordination, workspace
context, and the discovered skills catalogue—not either native role's full system prompt—through a generated temporary file that is file-bind-mounted over `~/.claude/CLAUDE.md`
inside the Claude process's private workspace mount namespace. If the bind
target does not exist, Rho creates an empty `~/.claude/CLAUDE.md` file first.
Rho does not write the generated prompt into the origin checkout or workspace
checkout. The mode-0600 generated source file remains alive while the agent loop
owns the Claude child, is rewritten in place before a cold Claude
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

Claude Code reaches Rho only through the in-process Python notebook server
that the workset runtime serves over Claude's control channel; every built-in Claude
tool is denied in the generated settings. The multi-agent host functions it
gets there are the same ones native roles use, with the same handle
validation, spawn-depth and live-child limits, and tool errors returned as
data instead of panicking.

Agent mail intentionally has no ownership or ancestry authorization: any agent
that knows another agent's unambiguous role-prefixed handle may inject mail into
its queue. This is a collaboration bus inside one trusted local pool, not a
team-isolation boundary. Self-messaging and ambiguous or mismatched handles are
rejected. Interrupt remains role-specific and separately validated.

Spawned Engineers join their parent's workset. Optional `workdir` selects an
existing absolute directory inside it, validated before child creation; omission
inherits the parent's directory. A parent that wants concurrent edits makes the
child a checkout of its own (a git worktree) first and passes its path as
`workdir`. The daemon creates no
checkouts for children, only the initial clone of a new agent's
repository. Advisors intentionally join their caller's directory and
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

## Papercut reports

The native `papercut` tool appends model-authored reports to a separate local
`papercuts` table, alongside the reporting agent id and timestamp. Descriptions
must be nonempty and at most 16 KiB; there is no aggregate quota or automatic
retention policy. Reports are opaque data, not instructions, and trigger no
notification, external submission, or background work. Success is returned only
after the database commit. Cancellation before acquiring the write lock leaves
no report; once writing starts, the short transaction completes atomically.
Tests cover validation, concurrent appends, and reopening the database.

## Python code mode (`rho-agent` `python` module)

Every role works in the Python notebook: native agents have it as their only
tool, and all Claude roles get it as an in-process MCP server with Claude's
own tools denied.

- Native and Claude runtimes admit at most one new Python exec per provider
  response. The provider call ID is the notebook execution ID; Claude's MCP
  transport IDs are not execution identities. Missing provider identity is
  rejected, not guessed. Claude admission commits before source evaluation. Native
  admission identities stay in worker memory, seeded from committed calls across
  all branches at startup; rewind does not clear them. A crash may lose native
  unflushed conversation and identity evidence. Retrying a transport must not replay admitted
  source; this does not claim external effects are transactional.
- Output reads lease a stable snapshot. Native requests transfer owned output
  into an ordered bounded replication queue before acknowledging leases or reaping
  cells. Request/response batches and response usage commit atomically in a background
  writer; the main loop stops on writer failure, without retrying uncertain writes.
  Rewind, profile changes, terminal publication and shutdown drain the writer.
  Claude output batches still commit before leases are acknowledged or cells reaped. Claude
  records transport handoff separately; after a crash in the handoff gap it may
  repeat an attributed report, never execute its source again. A successful write
  is not evidence of remote consumption. This output guarantee does not make
  ordinary Claude user/mail input durable.
- Exec timing describes provider and handoff milestones, not Python execution
  completion or proof of external effects.

- One workset process owns its agents, notebooks, jobs, terminals, shells, and
  provider transports. The daemon alone opens the shared database and owns
  account/quota/route policy, credential refresh, naming, and cross-agent mail.
  Tokens and provider-local credential backing can be present in workers;
  same-user execution is not a privilege boundary.
- Exactly one private Unix socketpair multiplexes all workset traffic. Startup
  separates its close-on-exec control descriptor from stdin before threads.
  Stdout/stderr are diagnostics. Logical Senax messages use bounded fragments,
  are reassembled before mutation, and retain per-port FIFO with fair writes.
  Memory bounds are not host-resource isolation. Readers never await runtime
  progress; disconnect rejects pending calls, and uncertain mutations are fatal
  rather than retried. Completion publication queues recipient delivery instead
  of awaiting reciprocal runtime acceptance.
- The workset creates its identity user namespace and mount view before Tokio
  or notebook threads. Normal execution inherits this view. No namespace
  descriptors or mount-change RPCs cross the daemon channel. Claude child
  launchers privately clone the workset mount namespace and apply preopened
  account/projects and generated prompt/settings overlays with syscall-only
  pre-exec operations. These mounts isolate topology, not shared backing data.
- Agent retirement requires serialized runtime permission, closes admission,
  drains jobs and owned children, and waits for daemon handlers before reusing
  the ID. A cancellation-safe lock spans retirement. A stuck retirement kills
  and reaps the whole workset. Agent unload and GUI detach do not terminate
  retained terminals or shells. Workset mode changes require settled agents
  and no live sessions and replace the whole execution process.
  Unexpected death loses all workset interpreters and sessions and has weaker
  descendant cleanup: parent-death signals are best effort. Neither process
  death nor recovery rolls back external effects or resumes code automatically.
  There are no cgroups or process-tree rollback guarantees.
- Notebooks run on embedded CPython (PyO3, one interpreter per worker process)
  with ordinary host access: `pathlib`, `open`, `os`, the whole standard library
  and native extension modules work directly. `pathlib` and `Path` are prebound;
  imports remain ordinary Python imports. Each notebook has its own globals, its
  own event loop and a dedicated thread that unshares its filesystem attributes
  and sets its initial cwd, while retaining the inherited workset mount namespace.
  Python `chdir` therefore affects that notebook, not the daemon or sibling
  notebooks; it remains shared between its live cells. Imported modules and
  other interpreter-wide state (`sys.path`, logging, warnings) are shared by every
  notebook in the worker; host objects such as `agents` are per notebook.
- Python is explicitly **not a sandbox**. Workspace mount mapping provides path
  correctness, not capability isolation. Unlike managed shell commands, native
  Python file operations are not Landlock-restricted. Process-global environment
  mutations, signals, descriptor operations, and process exit retain their normal
  behavior inside the worker. `ctypes` and native extensions can read and write
  the worker's memory, including credential and route snapshots it holds.
  Ordinary interpreter failure does not exit the daemon, but same-user hostile
  operations and host resource exhaustion are not contained. Python code must
  still be trusted. Automatic Python signal-handler installation is disabled so
  imports do not replace the host's Ctrl-C handler; explicit Python signal
  changes still retain their normal semantics.
  Commands should use `command()` when Rust-managed
  lifetime and automatic output are wanted; ordinary Python subprocesses do not
  acquire that managed lifecycle automatically.
- Each notebook runs the stock asyncio selector loop. Rust messages wake it
  through an eventfd inbox; Python objects never cross threads. A cell's
  ownership is a context variable that asyncio copies into its tasks and
  callbacks and that threads inherit; the loop counts each cell's live tasks,
  callbacks, timers, descriptor watchers and threads, and the cell finishes
  when its code has returned and that count is zero. Host tools are PyO3
  classes and functions that check Python arguments themselves. What a tool
  starts is an operation of the running cell: registered before Python
  continues and run on the worker's Tokio runtime to completion. Awaiting it
  is optional and cancelling the awaitable does not stop it; cancelling the
  cell does.
  Asyncio networking and subprocesses have ordinary unsandboxed Python access,
  not the managed lifecycle of `command()`.
  Real Python threads are enabled and share one interpreter lock with every
  notebook in the worker. Notebook-created threads inherit cell context unless
  the caller supplies an explicit context; executor workers belong to the pool,
  and submitted work keeps its cell alive through the task awaiting it.
  Cancelling an asyncio future does not imply its thread has stopped. A host
  call that returns an awaitable, including `command()`, must be made on the
  notebook event loop, not a worker thread.
  PyYAML and HTTPX are supplied from the Nix-pinned package closure alongside
  the pinned CPython.
  Cancelling a cell cancels its asyncio tasks and callbacks only. Synchronous
  Python is never interrupted: a notebook stuck in synchronous or native code
  blocks its own loop, and other notebooks only through the shared interpreter
  lock; recovery is restarting the workset worker. Python can exhaust memory,
  catch cancellation, or block in native computation. Rust command and
  nested-tool cancellation do not depend on Python cooperation.
- Managed commands use watched flake dev shell generations and a native Bash
  supervisor with immutable environment. Cache invalidation covers discovery,
  symlinks, replacement, and every source read the Nix evaluator reported, not
  files read only by `shellHook` at activation. Resolution failure does not
  reuse stale values.
  Up to five pristine pre-forked children wait for one command each. They receive
  cwd and stdio only at admission; no child that ran user code is reused. Idle
  children have parent-death protection and are killed/reaped at supervisor shutdown.
  The single-threaded Bash supervisor warms variables and builtins but never
  runs command bodies or startup files. Each child refreshes process identity,
  cwd, timing and random state, initializes job control with its own stdio,
  and runs normal startup.
  Only three stdio descriptors reach the child; lifecycle traffic is separate.
  Cancellation kills the command's process group and waits for its leader.
  Disconnect fails pending work without replay; detached descendants and
  external effects are not contained or rolled back.
- Commands, stdin writes, and nested tools share eager Rust-owned registration;
  awaiting a Python result is not what starts or owns the work. Their source remains
  attached to the actual provider `exec` call until evaluation and attached work
  finish and final output is drained. Internal jobs never invent provider calls.
  Output previews are token-budgeted; explicit reads use an independent cursor
  over a private temporary file, including after command completion. Each job
  retains its first 8 MiB with explicit overflow counts. At most 64 job records
  are retained, evicting oldest completed, delivered records; temporary files
  disappear with their records. Up to 32 image references are retained.
- Notebook bridge payloads are capped at 1 MiB, live cells at 128, and pending
  host requests and registered tasks at 1,024 each. Input admission is unbounded:
  one FIFO wakes the interpreter directly, which drains at most 64 messages per
  callback and re-wakes for the remainder. Cancellation and shutdown flags bypass
  backlog. These queue semantics do not bound total admitted bytes. Python-to-Rust callbacks commit synchronously and have no
  deferred event consumer. These bounds do not cap arbitrary Python allocations.
- Commands and internal tool calls inside Python are independent scheduling sources. Their output remains
  attached to the originating `exec` call; command IDs identify the work, not
  notebook cell IDs. The core tracks each command's first drain separately.
- `notify` marks meaningful output; `text` and captured stdout/stderr mark
  ordinary progress. Standard streams expose no daemon file descriptors. Both
  become output on the originating call at the core's next request boundary;
  neither starts inference directly. `set_checkin` conveys a model-authored
  one-turn interval and tool-wakeup policy, not a Python sleep or a tool-selected timeout. It updates
  its execution's shared Rust state synchronously. Only the execution from the
  latest model response controls check-ins; old settings need no mutation or
  stale-setter warnings. A quiet successful setter-only
  completion is not news that immediately defeats its own interval; failures,
  command completion, and meaningful output retain normal wake/batching rules
  unless `wake_on_tools=False` suppresses tool deadlines for that turn. Suppression
  includes the execution itself, does not cancel work or discard buffered output,
  and does not disable the timer, user input, or agent mail.
- Python source can execute complete top-level units before its provider response
  finishes. The agent validates one stable, append-only custom `exec` identity,
  bounds total source to 1 MiB, and orders unit admission and settlement in memory.
  Executing a unit never waits for a database admission or settlement write. Transport loss never
  closes the compiler as EOF: unadmitted source is discarded, while an admitted
  unit and its commands continue as ordinary sources on the accepted `exec`
  call. The next request respects their normal completion, batching, and check-in
  rules rather than a forced retry deadline. Failures before admission retain
  bounded backoff. Fresh context reports completed, running, or failed statements
  rather than automatically replaying them. Only coherent conversation boundaries
  are saved. After restart, recent source and execution may be absent; external
  side effects may remain. Recovery never reconstructs interpreter progress or
  automatically replays interrupted work.
- Notebook state and live jobs are ephemeral and do not survive restart. The
  existing transcript recovery rules apply. The `rho-code-mode` V8 crate remains
  the JavaScript runtime for all other code-mode roles.

## Native tool-history eviction

`eng-high-notes` removes older completed tool exchanges from active context before
falling back to provider compaction. Manual Compact always uses provider
compaction. Eviction targets 40,000 estimated tokens remaining, while protecting
the recent 40,000-token suffix and live/unanswered exchanges. If eligible
exchanges cannot reclaim enough, Rho discards the eviction plan and requests
provider compaction without evicting any exchanges. Eviction and compaction
are mutually exclusive for a request. No notes are written or preparation
responses requested.

- Evictions are append-only typed transcript items. Inference removes paired
  calls/results and their updates, invalidates pre-eviction continuations, and
  retains prose and reasoning. Recent and live/unanswered exchanges are protected.
  Full original transcripts remain durable; role changes do not reopen evictions.
- Python `transcript` is a lazy, read-only snapshot of the agent's original transcript
  per execution. Its `text` exposes bounded tool output and tool-call source
  (`arguments` aliases the latter), not separately retained full tool output.
  It may contain private user content, tool arguments and results,
  image data and provider reasoning metadata. It has the same trust and disclosure
  boundaries as the transcript, not privileged instruction authority. Do not send
  it to external destinations merely to search it.
- Eviction and compaction preserve live Python and jobs. Restart loses execution
  and never automatically replays effects. Historical rotation/preparation events
  remain readable, but no unfinished preparation is resumed. Existing notes files
  are left untouched; the runtime no longer creates or inventories them.

## Headless evaluations (`rho eval`)

The CLI runs the production agent loop with the selected native engineer role
(default `eng-high` / GPT-6 Astra), configured provider credentials, and isolated
temporary agent state. It neither connects to nor replaces the running daemon.
The default workdir is temporary; `--workdir` deliberately grants ordinary live
workspace tool authority and does not roll back writes. Evaluations make real
provider requests. JSONL output contains assistant/tool transcripts and usage,
not provider reasoning or image bytes; it remains potentially sensitive.
Timeout/interruption cancels the agent. Expected final substrings and required
observed tool calls are CLI evaluation criteria, not a security boundary.

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

## Agent desktop transport

Desktop processes and their applications run with the invoking user's authority;
they are not a sandbox. Session descriptors live in private `XDG_RUNTIME_DIR`
directories. Desktop control and MoQ listeners use Linux abstract Unix sockets
and reject peers whose kernel-reported UID differs from the desktop process.
Advertisements contain the owning agent ID and session name and live under
`rho-desktop/agents/<agent>/` in that runtime directory. Workers list live
advertisements without opening a media subscription. Advertisements are
same-user metadata, not an authentication boundary.
The worker resolves a selected session within that agent's directory; the daemon
connects to that endpoint directly. No unauthenticated network desktop listener
is exposed.

Remote desktop opening is permitted only after the existing Iroh authentication.
The GUI's existing connection demultiplexes media streams by a reserved 0xff
prefix and per-viewer identifier; ordinary compressed RPC keeps its framing.
Each connection admits at most 32 media sessions, with bounded incoming stream
queues and header deadlines. Closing a viewer closes its media route, not the
authenticated connection or sibling viewers.

Desktop-open requests one fixed VP9 video track. Both hops send MoQ lite-05
GROUP data directly, with microsecond timestamps and subscription id zero;
there is no media setup, announcement, metadata, or subscription negotiation.
A separate stream signals viewer lifetime without gating frame delivery on a
reply. Each receiver admits at most 32 concurrent group handlers; cancellation
aborts unfinished groups and releases the upstream subscription. Local desktop
and remote UI protocol versions reject binaries using the old media handshake.

Desktop JSON headers are capped at 64 KiB; dimensions are capped at 4096 in each
axis and VP9 packets at 16 MiB. Raw/encoded queues are bounded. The MoQ cache
uses a 32 MiB target and short retention; this target is not a hard process-memory
limit. The decoder still processes compressed data from an authorized desktop
using libvpx. External decoder allocations reject requests above 256 MiB each;
retained planes are range-checked against their backing allocation. Buffers are
reused only when neither libvpx nor a displayed/frozen frame owns them. GPUI
validates plane lengths/strides and checks the GPU texture size limit before
upload. These per-allocation checks are not a hard process-memory limit.
Same-user desktop clients can control applications and capture
their contents; annotations copied or attached to a prompt disclose the exact
frozen image selected by the user.
