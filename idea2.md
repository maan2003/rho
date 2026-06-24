rho is a Rust toolkit for building AI agents, tuned for developers and tinkerers.

note: tau is in ~/src/tau
The main idea is to move Tau's process-based extensibility to Rust-level
extensibility. Tau swaps processes that speak a protocol; rho swaps/forks crates,
structs, enums, functions, futures, and streams. Because everything can live in one
Rust program, the compiler helps when you change the shape of things.

Forking is not a failure mode. It is expected. The goal is that small needs lead to
small, local changes in complexity: fork `rho-agent` to change the harness loop,
fork a provider crate to change provider behavior, fork a tool crate to change tool
behavior, without dragging the whole system along.

Crate boundaries exist to reduce tight coupling:

- `rho` / core types: small shared vocabulary like items, messages, tool calls,
  tool results, provider requests/responses, ids, and model params.
- provider crates: concrete provider adapters that produce futures/streams.
- tool crates: concrete tool implementations and helpers.
- persistence crates: optional storage such as cbor logs or redb.
- `rho-agent`: an opinionated harness/agent loop that depends on the building
  blocks.

`rho-agent` is intentionally tuned for one workflow. It can own the `tokio::select!`
loop, queue semantics, retries, tool scheduling, persistence hooks, and state enum.
Users can run it as-is, but are expected to fork it or keep patches on top when
their workflow diverges.

`rho-agent` is not an abstraction layer over all possible agents. It is a forkable
reference harness assembled from reusable lower crates. It is allowed to be
concrete and opinionated; forkability should come from understandable Rust code and
clear crate boundaries, not from abstracting every policy decision.

Prefer concrete Rust over framework abstraction. Avoid defining universal traits
until they are truly needed. `rho-agent` can assemble components with local enums:

```rust
enum AgentProvider {
    ChatCompletions(rho_provider_chat_completions::Provider),
    Responses(rho_provider_responses::Provider),
}

enum AgentState {
    Idle,
    ApiRequest { stream: ProviderStream },
    WaitingForTools { futures: ToolFutures, results: Vec<ToolResult> },
}
```

If a user wants a new provider or different behavior, they patch the enum and let
`cargo check` show the affected places. The abstraction boundary is often the crate
or source file, not a trait object.

Async type erasure is still fine when it keeps the harness simple. `BoxFuture`,
`BoxStream`, or enum stream wrappers may be useful implementation details. The goal
is not "never use dyn"; the goal is to avoid making universal trait hierarchies the
public design center.

Core types should be least-prescriptive but not meaningless. For example, a
`ToolSpec` should probably contain the stable essentials: name, description, and
input schema. More policy-heavy ideas like tool tags, repair examples, background
support, UI display state, or argument repair should live outside core unless they
prove broadly necessary.

Adding policy fields to core is easy; removing them later is hard. Provider, tool,
storage, UI, cancellation, backpressure, retry, queueing, and persistence choices
should stay in their owning crates or in `rho-agent` unless they become obviously
shared vocabulary.

Provider/tool/store crates should expose concrete capability structs rather than
requiring global traits:

```rust
ChatCompletionsProvider
ShellTools
CborStore
RedbStore
```

Those structs can create futures, streams, requests, responses, or helper values.
`rho-agent` decides how to assemble, poll, cancel, queue, persist, and display them.

Persistence should be optional and swappable. A harness may use no persistence,
append-only cbor logs, redb, or a custom store. Core types should not depend on any
one persistence model.

rho will borrow proven pieces from Tau, but not Tau's protocol, event bus,
supervisor, or harness machinery unless those pieces are useful at the Rust-library
level.

Design principles:

1. Rust-level extensibility instead of process/protocol extensibility.
2. Forkability is encouraged; compile errors are part of the workflow.
3. Building-block crates stay loosely coupled.
4. `rho-agent` is a useful default harness, not a universal framework.
5. Prefer structs/enums/functions/futures/streams over trait-heavy APIs.
6. Keep core types small: shared vocabulary, not policy.
7. Local policy changes should require local code changes.
8. Do not promise drop-in plugin compatibility when patching the harness is simpler.
9. Keep runtime policy in `rho-agent` or leaf crates, not in core.
