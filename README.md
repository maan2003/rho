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
p50/p95/p99/max summaries, and `rho-gui-profile.folded.trace.json`, a
Perfetto-compatible timeline containing timestamped CPU stacks and
`rho.gpui.latency.v1`/`rho.gpui.draw.v1` frame spans. Open the trace in
Perfetto and select a slow frame span to inspect CPU samples from that exact
interval. The trace reports dropped and truncated samples in `rhoProfile`.

`just profile-daemon` profiles an optimized daemon until SIGINT or SIGTERM and
writes `rho-daemon-profile.folded` and its timestamped `.trace.json` timeline.
Both recipes accept an output path followed by the normal executable arguments.
The folded files are aggregate compatibility views; the trace is the source of
truth for temporal attribution.
