# rho architecture

`rho` is a Rust-local toolkit for building AI agents by composing crates rather
than by running a supervisor, extension protocol, or daemon process graph.

## Crate layering

- `rho` owns the shared vocabulary: transcript items, inference requests and
  responses, tool calls/results, usage, roles, message phases, and opaque
  provider items. It should stay policy-light.
- Inference crates, currently `rho-inference-responses`, translate `rho` inference
  requests into provider-specific wire protocols and translate provider events
  back into `rho` items and updates.
- `rho-agent` owns the opinionated harness policy: queueing, retries/tool
  scheduling, streamed transcript handling, inference response block recording,
  and persistence hooks.
- CLI and UI crates assemble concrete providers, tools, stores, and terminal
  rendering. They should not own inference protocol details.
- Store crates own concrete persistence formats. Tool crates own concrete tool
  execution.

Dependencies should flow from higher-level assembly/policy crates toward lower
reusable crates. The shared `rho` crate must not depend on provider, agent,
store, tool, or CLI crates.

## Transcript and inference data ownership

`rho::ItemBlock` is the transcript unit passed between stores, agents, and
providers.

- `ItemBlock::Local` is local/user/tool/agent-owned transcript data.
- `ItemBlock::InferenceResponse` is provider-owned output plus the optional
  provider response id needed for provider-side chaining.
- Provider-specific data that must be replayed but is not part of the shared
  semantic vocabulary is carried as `rho::ProviderItem` with an opaque JSON
  payload and a coarse `ProviderItemKind`.

`rho-agent` is the canonical owner of the in-memory transcript during an agent
run and decides when to persist blocks. Inference crates may derive requests from
the transcript but should not mutate it directly.
