# DECISION-pull-based-sources: Sources are pulled, never pushed

Authority: inferred

## Decision

A source accumulates output on its own and is drained by the core at a moment
the core picks. No source may start a request, and a drain takes from every
source at once rather than from whichever one prompted it. The dedicated
[context-rotation preparation](../../../specs/DESIGN-context-rotation.md) exchange
is the exception: hold new user input, mail, and unrelated already-answered
tool output for the fresh window, while satisfying mandatory call results.

## Rationale

Pushing wakes one request per source, so a round of parallel tool calls, a typed
message and a peer's mail become four requests instead of one.

Pulling also puts the summarising where the knowledge is: a tool asked for its
output after five minutes hands back the relevant tail plus a count of what it
dropped, which no buffer owned by the core could have produced. Between
requests, a chatty tool costs nothing. Preparation deliberately excludes fresh
work so the last opportunity to preserve old context cannot be consumed by it.
