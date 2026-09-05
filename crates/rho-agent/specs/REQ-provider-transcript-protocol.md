# REQ-provider-transcript-protocol: Provider transcript protocol

Source: the inference provider wire protocols implemented by `rho-inference`.

A transcript sent to a provider must satisfy constraints the provider imposes
rather than ones this crate chose:

- Every `ToolCall` must be answered by exactly one `ToolResult` carrying its
  call id. A call left unanswered is rejected outright, and a second result for
  the same id is not accepted — so anything the agent has to say about a call
  after that point must be a `ToolUpdate`.
- Tool output must sit adjacent to the call it answers. A drain therefore emits
  tool results and updates ahead of mail and user messages regardless of the
  order things actually arrived in.
- A compaction trigger must be the final input item, so a queued compaction is
  stable-sorted to the back of a drain and history records the order the request
  was really sent in.

These are interoperability obligations with no flexibility inside this crate: a
change here follows a change in the providers `rho-inference` speaks to, not a
preference of the harness. Violating any of them fails the request rather than
degrading it.

The first requirement is what
[SPEC-restart-recovery](SPEC-restart-recovery.md) exists to meet, since a
process that stops mid-call leaves calls that nothing will ever answer.
