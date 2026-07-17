# Rho temporal sampling patch

This is pprof-rs 0.15.0, vendored from crates.io. Rho's narrow patch adds:

- a bounded, preallocated raw-stack ring written by the SIGPROF handler;
- an ordinary 100 ms drain thread that interns stacks and records timestamps;
- CLOCK_MONOTONIC timestamps, Linux OS thread ids, and dropped/truncated counts;
- ProfilerGuard::stop and ReportBuilder::build_temporal APIs.

The existing aggregate collector and public pprof APIs otherwise remain intact.
Keep the temporal handler path allocation- and I/O-free when updating upstream.
