# DECISION-boundary-is-the-only-decision: All scheduling judgment lives in `boundary`

Authority: inferred

## Decision

Every scheduling rule and every duration lives in the `boundary` function. No
source, tool, or handle may name a delay, a priority, or a reason to send;
sources report facts, and even "this is not worth sending" is `boundary`'s
answer to give.

`SourceKind`, the shape of those facts, is declared in `boundary.rs` too — it
exists to be read by the decision and by nothing else.

## Rationale

A duration chosen by one source in isolation is chosen without seeing what else
is waiting, and the bug that follows always has the same shape: something that
looked urgent alone quietly demotes something that mattered more.

Keeping the rules in one place is also what makes them testable. `boundary`
touches no store, no provider and no task, so it runs against a fabricated clock
and fabricated sources.
