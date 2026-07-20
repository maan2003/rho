# rho architecture

`rho` is a Rust-local toolkit for building AI agents by composing crates rather
than by running a supervisor, extension protocol, or daemon process graph.

## Crate layering

- `rho-core` owns the shared vocabulary: transcript items, inference requests,
  inference events and responses, tool calls/results, usage, roles, message
  phases, and opaque provider items. It should stay policy-light.
- Inference crates, currently `rho-inference`, translate `rho-core` inference
  requests into provider-specific wire protocols and translate provider events
  back into `rho-core` items and updates.
- `rho-agent` owns the opinionated harness policy: queueing, retries/tool
  scheduling, streamed transcript handling, inference response block recording,
  and persistence hooks. Loading restores that logical state cheaply; the
  workspace-backed execution context (view, prompt, and tools) initializes
  lazily at first inference. It depends directly on the concrete
  `rho-inference` session.
- `rho-workspaces` owns checkout materialization and filesystem views. A
  `Workspace` is one materialized checkout (a jj pool slot, the user's live
  checkout, a VCS-masked sandbox workspace, or a plain directory). Sandbox
  workspaces use normal jj pool slots while masking their `.jj` and colocated
  `.git` metadata from child commands, expose a separate synthetic Git
  baseline, and install a Landlock filesystem/network policy on every prepared
  child command. A `View` is one agent's world: a working
  set of workdir entries, fixed at spawn, realized as a private mount
  namespace with each entry's slot mounted over its origin path. Entry 0 is
  the primary workdir (default cwd, prompt header).
  Agents joining a workspace share the `Workspace`; each agent has its own
  `View`. All jj workspaces created for one agent share one workspace id, so
  the agent's jj workspace name is the same in every repo it forks.
  Sandbox and ordinary workspaces cannot be mixed in one view.
- `rho-context-config` owns bounded `AGENTS.md` loading plus local Markdown
  skill discovery/frontmatter parsing. Rho packages platform-owned skills under
  `$out/share/rho/skills`; the final package build embeds that immutable root in
  the binaries, below project and user skills in precedence. Results are cached per
  `rho-workspaces::Workspace` and merged across a view's workdirs;
  `rho-agent` owns system prompt rendering. Clients have no special skill or
  AGENTS.md command path. The native Rho inference loop and Claude Code use
  separate prompt compositions: Claude performs its own project and skill
  discovery and receives only Rho role/team context on top of Claude Code's
  own harness prompt.
- CLI and UI crates assemble concrete providers, tools, stores, and terminal
  rendering. They should not own inference protocol details. The native GUI
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
  and raw terminal control sequences never reach the editor buffer.
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
- `rho-voice` is a provider-protocol crate outside the inference contract: it
  speaks the xAI realtime voice WebSocket (audio streams, voice tool calls)
  and deliberately never touches `rho-core` transcript vocabulary. Voice is a
  control surface over agents, assembled by the daemon, not an inference
  provider.
- Store crates own concrete persistence formats. Tool crates own concrete tool
  execution.
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
  vocabulary.

Claude Code MCP support follows the same boundary: `rho-claude` knows how to
set per-agent MCP environment, but the MCP server that exposes Rho multi-agent
operations lives at the CLI/daemon control boundary. Claude Code can launch a
globally configured `rho mcp-agent-tools` stdio MCP server; that server reads
`RHO_MCP_AGENT_ID` from the Claude process environment, relays tool calls to the
daemon, and the daemon executes parent-scoped spawn, agent mail, interrupt, and
wait against `AgentPool`. The MCP server must not reach into `rho-core` or
provider crates.

Collaboration creation is role-specific while communication is shared.
`spawn_engineer` always gives jj-backed workdirs isolated child workspaces;
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
The `eng-mini` tier uses the GPT-5.6 Luna Responses model with xhigh reasoning,
fast mode, and direct tools instead of code mode. Engineers spawned by an
`eng-mini` parent are also `eng-mini`.
PMs run with the normal direct tool surface (never code mode), coordinate
exclusively through collaboration tools, and do not receive shell command,
process-input, or patch tools. Their prompts omit repository `AGENTS.md` content
and skills as well as the working-directory Environment section; technical
requests are delegated to Engineers carrying the user's instructions verbatim.
PMs use judgment when routing follow-ups: they may reuse the responsible
Engineer, but spawn a fresh one when warranted or requested or suggested by the
user. Slack-bound PMs explicitly relay Engineer results and other user-facing
responses through `slack_reply` because final responses are not posted to Slack
automatically.
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
workflow-bearing Engineer/PM variants carry `AgentWorkflow`. Slack creates PMs
with the persistence-compatible `AgentWorkflow::PrFriendly` marker, and only
Engineers spawned directly by those PMs inherit it. The marker activates
`github-workflow` guidance without changing the visible role label or model
binding.

`rho-slack` is the in-process Slack surface. `SlackManager` is handed the
daemon's `AgentPool` and `RhoDb` and owns everything Slack: sealed-memfd
secret storage (`SecretStore`), the Socket Mode reconnect loop, the persisted
Slack coordinator repository and Slack-thread → agent-session mapping, and a
Slack-bound `slack_reply` tool extension for mapped coordinator agents. It also
subscribes to generic accepted-input reports and mirrors non-Slack local user
inputs into mapped Slack threads, using a private opaque source id to avoid
echoing Slack-originated inputs. The daemon validates and installs Slack setup,
resumes secrets from the systemd fd store on startup, and publishes generic
agent turn-completion and accepted-input reports through `AgentPool`; Slack uses
completed-turn reports for reaction cleanup, not automatic final-answer posting,
and the daemon does not own Slack routing policy.

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
GitHub and `~USER/REPOSITORY` for SourceHut. Repository components are ASCII
alphanumeric, and an input `.git` suffix is removed before validation. There is
no failover after an approved GUI claims an operation; retrying starts a fresh
provider race.

`rho-pr-monitor` owns long-lived pull-request policy while Octo remains only
the authenticated GitHub API boundary. Engineers create or adopt PRs through
`rho pr`, which persists the stable GitHub repository id and PR number,
registration generation, subscriber Engineer, feedback revisions, CI/check
state, mergeability, and constrained reply targets in `rho-db`. A daemon task
polls at most 16 open watches every two minutes, filters pending reviews and
untrusted human authors, wakes the subscribed Engineer on meaningful CI,
review, mergeability, or terminal changes, and keeps watching after CI turns
green until merge/close.
The Engineer handles repository work and GitHub replies directly, then sends
concise milestones to its parent so Slack-bound PMs can relay them. The
standalone `octo` CLI is not installed.

The normal UI protocol carries request-id-scoped `rho pr` commands and their
text or bounded log-archive results. Agent-side commands identify the
subscriber from `RHO_AGENT_ID`; the daemon resolves and validates the Engineer
before calling `rho-pr-monitor`. The CLI process never owns a polling loop:
subscribe/create return immediately, while the daemon later loads and wakes
the persisted Engineer through `AgentPool`.

The daemon's UI protocol (`rho-ui-proto`) is served over the local Unix socket
and iroh connections from clients enrolled through `rho-iroh-auth` (`rho
daemon --iroh`; approval via `rho iroh approve` stays on the Unix socket).
Unix sessions multiplex control and agent state on one byte stream. Native
iroh sessions keep commands and lifecycle events on a high-priority
bidirectional control stream (exactly one per physical connection); the daemon
opens one unidirectional stream per non-hidden loaded agent, up to 1024, so
state remains warm in the GUI cache without cross-agent
head-of-line blocking. The focused stream has weight 200 and background streams
weight 1 within their lower-priority class. Focus changes travel over the
control stream and update transport weights without reopening streams. The
same iroh endpoint carries a second ALPN for the web UI: newline-delimited JSON
(`rho-webui-messages`, shared with the browser as a wasm-safe crate) bridged
through an in-process duplex pipe onto a normal UI protocol session, so the
daemon's webui module only translates the JSON vocabulary and owns no agent
policy. Its new-agent command carries the selected topic, registered workdir,
role, and isolated-versus-user-checkout start choice. The web UI page itself
is a static Leptos/wasm app (`webui/` at the
repo root, its own cargo workspace, hostable anywhere) that connects as an
iroh client from the browser.

Rho patches iroh's `noq` transport dependencies to vendored copies. The local
extension preserves strict stream priorities and adds relative send-stream
weights within each equal-priority fair-scheduling class. Weight 1 retains
upstream behavior; higher weights receive proportionally more packet-writing
turns without changing anything on the QUIC wire. Transport scheduling owns
connection bandwidth allocation, while application-level stream selection and
coalescing remain UI protocol policy. Native GUI and daemon endpoints enable
noq's qlog instrumentation when `QLOGDIR` is set, writing `rho-gui-*` and
`rho-daemon-*` traces respectively for transport-level diagnosis. Iroh uses
CUBIC congestion control by default; `rho daemon --iroh-bbr3` (or
`RHO_IROH_BBR3=true`) selects BBR3 for daemon-to-client traffic without
requiring the client to use the same controller.


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
