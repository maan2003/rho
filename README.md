# Rho

Rho is an opinionated, forkable harness for local AI-agent workflows. Change
agent behavior by editing Rust code, rather than by assembling a separate
supervisor, protocol, and plugin runtime.

## Surfaces

- `rho-gui`, the native desktop application.
- Slack integration.
- The `rho` terminal CLI.
- A browser-based web UI.

## Profiling

`just profile-gui` runs an optimized GUI with unprivileged, all-thread CPU
sampling. Closing the GUI writes `rho-gui-profile.folded` plus
`rho-gui-profile.folded.frames.json`, which contains raw GPUI draw timings and
p50/p95/p99/max summaries.

`just profile-daemon` profiles an optimized daemon until SIGINT or SIGTERM and
writes `rho-daemon-profile.folded`. Both recipes accept an output path followed
by the normal executable arguments.
