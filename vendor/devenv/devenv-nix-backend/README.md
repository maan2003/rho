# devenv-nix-backend

rho's Nix backend for `rho-devshell-builder`.
Talks to Nix through C bindings (the `nix-bindings-*` crates), against rho's Nix fork (`nix/patches/nix-*.patch`).

## What's in here

- `flake_env.rs` — `NixRuntime`: evaluates a flake's `devShells` into its `-env` output, recording what evaluation observed of local inputs; GC roots.
- `observations.rs` — those observations, observing local inputs again, and the rc script `nix develop` sources.
- `gc_root.rs` — GC root registration.
- `logger.rs` — bridges Nix's log messages into `tracing`.
- `umask_guard.rs` — scoped restrictive umask around C calls.

## Threads and the GC

Nix uses Boehm GC.
Any thread that touches Nix values has to be registered with it, or parallel marking races and crashes.

- `nix_init()` runs once per process (idempotent).
- `gc_register_current_thread()` registers the caller and stashes the guard in thread-local storage so it lives as long as the thread.
  Tokio worker threads call this from `on_thread_start`.
- `trigger_interrupt()` flips the process-global interrupt flag so an in-progress evaluation aborts on its next check.
