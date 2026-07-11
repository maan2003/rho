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
  checkout, or a plain directory). A `View` is one agent's world: a working
  set of workdir entries, fixed at spawn, realized as a private mount
  namespace with each entry's slot mounted over its origin path. Entry 0 is
  the primary workdir (default cwd, prompt header).
  Agents joining a workspace share the `Workspace`; each agent has its own
  `View`. All jj workspaces created for one agent share one workspace id, so
  the agent's jj workspace name is the same in every repo it forks.
- `rho-context-config` owns bounded `AGENTS.md` loading plus local Markdown
  skill discovery/frontmatter parsing. Results are cached per
  `rho-workspaces::Workspace` and merged across a view's workdirs;
  `rho-agent` owns system prompt rendering. Clients have no special skill or
  AGENTS.md command path. The native Rho inference loop and Claude Code use
  separate prompt compositions: Claude performs its own project and skill
  discovery and receives only Rho role/team context on top of Claude Code's
  own harness prompt.
- CLI and UI crates assemble concrete providers, tools, stores, and terminal
  rendering. They should not own inference protocol details.
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
`delegate-engineering` skill rather than every Engineer prompt. `ask_advisor`
creates an advisory session. `message_agent` is an unrestricted bidirectional
mail bus for any known role-prefixed handle, including Advisor context requests;
`wait_agent` waits for incoming mail. Each agent record stores whether it was
created directly, by a PM, or by an Engineer so prompt ownership context is an
immutable creation-time fact rather than inferred later. Advisors retain normal
shell/patch capabilities plus messaging/waiting but cannot spawn or interrupt.
User-facing handles remain `eng-*`, `pm-*`, and `adv-*` over `AgentId`.

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

`octo`/`oct` are vendored GitHub helper crates from `~maan2003/claude`. Rho
runs Octo as an in-process daemon Unix-socket server and exposes its socket path
to agent commands via `OCTO_SOCKET`; the `oct` CLI is the agent-facing client.
The daemon, not Octo, owns platform secret installation and fd-store resume, so
Octo receives GitHub tokens only through a RAM-only callback into the sealed
platform secret store. The `git-remote-octo` Git remote helper invokes a private
Nix-patched `git-remote-http` whose libcurl connection uses that Unix socket; it
does not replace Git globally. Octo proxies authenticated fetches for any
repository available to its token, and pushes after restricting destination
refs to `refs/heads/rho/*`.

The daemon's UI protocol (`rho-ui-proto`) is served over two transports, all
through the same per-connection handler: the local Unix socket, and iroh
bi-streams from clients enrolled through `rho-iroh-auth` (`rho daemon
--iroh`; approval via `rho iroh approve` stays on the Unix socket). The same
iroh endpoint carries a second ALPN for the web UI: newline-delimited JSON
(`rho-webui-messages`, shared with the browser as a wasm-safe crate) bridged
through an in-process duplex pipe onto a normal UI protocol session, so the
daemon's webui module only translates the JSON vocabulary and owns no agent
policy. Its new-agent command carries the selected topic, registered workdir,
role, and isolated-versus-user-checkout start choice. The web UI page itself
is a static Leptos/wasm app (`webui/` at the
repo root, its own cargo workspace, hostable anywhere) that connects as an
iroh client from the browser.


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
