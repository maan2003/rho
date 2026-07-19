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
  rendering. They should not own inference protocol details.
- `rho wayland` is an application-agnostic CLI surface for launching and
  controlling programs in isolated headless Sway sessions. It wraps the
  compositor's IPC plus `grim` and `wtype`; the Nix build embeds those tool
  paths and Mesa's software Vulkan driver rather than relying on the caller's
  environment.
- The daemon snapshots the user's login-shell environment and passes it
  explicitly to `rho-workspaces` for daemon-owned commands. Workspace-control
  subprocesses use that environment directly; agent shell and Claude processes
  add the primary project's environment through direnv.
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
- `rho-code-mode` is a tool crate: it runs model-authored JavaScript in an
  in-process V8 isolate (deno_core) and exposes the `exec`/`wait` tool pair.
  Nested tool calls made by scripts leave the crate through a `ToolDispatcher`
  trait implemented by the assembling harness; the crate depends only on
  `rho-core` vocabulary.

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

`octo-server` and its Git remote helper are vendored GitHub helper components.
Rho runs `octo-server` in-process on the fixed per-user Octo Unix socket. The
user- and agent-facing PR client is `rho pr` over the normal daemon socket;
Octo remains an internal authenticated API/Git transport. The daemon owns platform secret
installation and fd-store resume, so the server receives GitHub tokens only
through a RAM-only callback into the sealed platform secret store. The
`git-remote-octo` Git remote helper invokes a private
Nix-patched `git-remote-http` whose libcurl connection uses that Unix socket; it
does not replace Git globally. Octo proxies authenticated fetches for any
repository available to its token, and pushes after restricting destination
refs to `refs/heads/rho/*`.

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
standalone `octo` CLI is not installed; only `git-remote-octo` uses Octo's
private socket directly.

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
