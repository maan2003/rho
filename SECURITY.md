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
- Local/project Markdown skills are trusted prompt input when discovered or
  explicitly invoked. Treat them like AGENTS.md-style instructions: useful local
  guidance, not a sandbox or permission boundary.

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

## Skills

Rho skills are local Markdown files discovered from project `.agents/skills`
and user `~/.config/agents/skills`. Skills contribute names, descriptions, and
file paths to the agent system prompt; the model reads the referenced files
with normal shell tools when it needs their instructions.

Discovery uses bounded 64 KiB reads and rejects a skill whose YAML frontmatter
is truncated before the closing fence. Discovery follows
symlinked roots/directories/files while tracking canonical directories to avoid
cycles. Skill files are prompt input only; they do not restrict filesystem
access or grant tools.

## Future review notes

Future changes that add providers, credential storage, transcript persistence,
subprocess execution, filesystem writes, or background tasks must update this
file and document their primary trust boundaries, resource bounds, cancellation
behavior, and tests.
