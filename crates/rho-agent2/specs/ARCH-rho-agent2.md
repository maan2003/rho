# ARCH-rho-agent2: rho-agent2 harness

An agent harness built around one question, asked after every event: *should the
next request start now?*

## Shape

One `Agent` task owns all mutable state and runs a single loop — ask the
question, act on the answer, wait for the next event — until the last handle is
dropped. `AgentHandle` is the only outside view: commands in over an unbounded
channel, state out as a published `AgentSnapshot`.

Everything that produces transcript blocks is a **source**: the user queue, the
mail queue, and one entry per called tool. A source accumulates on its own and
reports plain facts — when something arrived, whether a call has been answered,
whether a tool has ended. It names no durations and starts no requests
([DECISION-pull-based-sources](DECISION-pull-based-sources.md)).

Being a source is a role, not a module. The two queues are plain vectors on the
`Agent`, and `SourceKind` — the facts a source reports — is declared in
`boundary.rs` beside the only code that reads it.

`boundary` is a pure function of every source's facts, the model's latest turn,
the current `Phase`, and the clock. It is the only part of the crate that
decides anything
([DECISION-boundary-is-the-only-decision](DECISION-boundary-is-the-only-decision.md));
the rest — spawning, draining, persisting, publishing — is mechanism.

`Phase` is what the agent is up to and the first thing the decision reads.
Either a request is in flight or it is not, and when it is not the only two
questions left are what the next request must open with (`owed`) and whether
anything has happened that bears on speaking (`standing`) — so `Phase::Idle`
carries both and nothing else has to. `standing` is facts, not a verdict, on the
same principle as a source: `Asked` says somebody asked for a request the sources
would not have made, `Cancelled`/`Failed` say what happened and when, and whether
either still stops the agent is `boundary`'s reading of them against the user
queue
([DECISION-stopped-agents-wait-for-a-person](DECISION-stopped-agents-wait-for-a-person.md)).
`boundary` never reads `owed`. Loading an agent is not its own state; it only
supplies `owed` differently ([SPEC-restart-recovery](SPEC-restart-recovery.md)).

## Boundaries and direction

The loop depends on `boundary`, the tools it has been given, `Store`, and an
`rho-inference` session. Nothing depends on `boundary`, and `boundary` reaches
nothing: no store, no provider, no task, no clock but the instant it is handed.
It is handed every source's facts at once and reads them together; no source
knows about another, or about the clock.

Tools come from the caller as a list of `Tool` implementations, keyed on the way
in by the name the model calls them by; a call in flight is a `ToolSession`. A
registry type would have been that map with pass-through methods, so there is
not one. The core says exactly one thing to a running tool — `cancel`, meaning
wind down — and still collects its parting output, so a tool has the last word
([DECISION-model-sets-the-pace](DECISION-model-sets-the-pace.md)).

A session hands its output over in two shapes, and the split is in the trait
rather than sorted out by the core: `first_output` is required and taken once,
`more_output` is optional and taken forever after. That is the provider's
one-result-per-call rule made unmissable
([REQ-provider-transcript-protocol](REQ-provider-transcript-protocol.md)). Both
carry the tool's words and only the tool's words
([DECISION-the-core-never-speaks-for-a-tool](DECISION-the-core-never-speaks-for-a-tool.md)),
which is also why the tool says when it may be forgotten: `done` is asked after
the drain beside it, so the last thing a tool has to say is always taken, and
the core never decides on its behalf that there is nothing more to hear.

## Persistence

`Store` is an append-only redb event log. History belongs to a *lineage* rather
than to an agent, and an agent points at the one it is currently on; loading
walks back to the root and replays forwards
([DECISION-history-only-branches](DECISION-history-only-branches.md)).
Instructions are deliberately not among the stored fields
([DECISION-instructions-are-code](DECISION-instructions-are-code.md)).

## Invariants

- The transcript has exactly one writer, so it has a total order.
- An accepted input reaches disk before it becomes live state, and a command's
  ack fires only after that command's own handling.
- The drain, the append and the send are one persisted event, so no crash can
  leave a queue drained into a transcript that never went out.
- No source names a duration. Every duration in the crate is in `boundary`.
- Nothing outside the core decides *when*; the core never decides *what* a
  source has to say.
- A drain's block order is the provider's, not the clock's, as constrained by
  [REQ-provider-transcript-protocol](REQ-provider-transcript-protocol.md).
