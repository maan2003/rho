# DECISION-stopped-agents-wait-for-a-person: Only fresh user input revives a stopped agent

Authority: inferred

## Decision

A cancelled or failed agent starts no further requests on its own. Output from
tools still winding down reaches history at the next boundary, but it may not
itself cause one. Only fresh user input revives the agent; a peer agent's mail
does not.

Being stopped is derived rather than recorded: `Standing::Cancelled` and
`Standing::Failed` carry the instant it happened, and the agent counts as stopped
only while nothing the user queued is at least that recent. Nothing writes the
revival down when input arrives.

A failure is also not remembered across a restart. The log has no event for a
request that produced nothing, so a reloaded agent cannot tell that its last
request failed and does not inherit the stop.

## Rationale

An agent that carries on by itself is one nobody can call off. A cancelled
tool's dying words would wake it straight back up, and a request that failed
will fail the same way when the next tool ends. Both states are waiting on a
person, and another agent is not a person.

Persisting the failure would let an error from a process that is no longer
running keep the next one from trying. Coming back up is a fresh start, and
nothing needs writing down to make it one.

Deriving the revival keeps it in one place. A flag set as input goes past would
be a second record of something the queue already says, free to disagree with it,
and every path that empties the queue would have to remember to unset it.
