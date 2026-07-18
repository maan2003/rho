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

`just profile-gui` builds with frame pointers and runs the optimized GUI with
Dial9 CPU sampling. Closing the GUI writes `rho-gui-profile.0.bin.gz`, a
symbolized Dial9 trace containing timestamped CPU stacks and
typed `RhoGpuiLatencyV1` and `RhoGpuiDrawV1` events. It also writes `rho-gui-profile.bin.frames.json` with
raw GPUI draw timings and p50/p95/p99/max summaries.

`just profile-daemon` profiles the optimized daemon until SIGINT or SIGTERM
and writes `rho-daemon-profile.0.bin.gz`. Both recipes accept a trace base
path followed by the normal executable arguments. Inspect traces with
`dial9 serve --local-dir .` or Dial9's agent analysis toolkit. CPU stack
sampling is Linux-only. A custom-event-only trace is still written when CPU
sampling is unavailable, although the current Dial9 viewer requires CPU or
runtime events to open it. The normal Linux perf backend samples the thread that starts profiling and
threads subsequently created by it; Rho starts profiling before daemon and GUI
worker creation. If Dial9 falls back to clock timers, coverage is limited to
threads registered with the fallback sampler. Rho prefers a surviving raw
`*.0.bin` artifact and validates the normal symbolized `*.0.bin.gz` artifact
before reporting success; Dial9 may remove the raw trace when post-processing
fails. Dial9 currently displays these retrospective GPUI duration events as
instant ticks with duration fields, not selectable timeline ranges. Dial9 0.3
logs lost or dropped sampler records rather than storing those counters in the
trace. CPU profiling requires frame pointers across the Rust dependency graph;
use the profiling recipes rather than a normal packaged build for useful
stacks. Prebuilt native dependencies can still produce truncated stacks.
