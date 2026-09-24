# DECISION-history-only-branches: History branches, and is never rewritten

Authority: inferred

## Decision

Stored events are append-only. A rewind mints a new lineage that records the
position in its parent it branched from; nothing is rewritten or deleted, and
the abandoned branch stays readable.

## Rationale

Truncating is the obvious implementation of a rewind, and it is the one that
destroys the transcript somebody may be trying to get back to. Branching costs a
row per fork and one walk back to the root at load.
