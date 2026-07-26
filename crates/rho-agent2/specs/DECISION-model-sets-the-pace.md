# DECISION-model-sets-the-pace: No foreground/background tool distinction

Authority: inferred

## Decision

Tools are not classified as foreground or background and a running tool has no
urgency of its own. How often its output reaches the model is set by the model's
latest turn and revised every turn. The only things a running tool may claim for
itself are that it ended, or that what it currently holds stands on its own.

A look-in lasts exactly one turn: the model is shown what its calls have once,
and to be shown again it has to ask, by calling something or by naming an
interval.

## Rationale

At the moment of the call `npm test` and `npm run dev` are the same call and
nothing can tell them apart, so any classification is a guess made at the worst
possible time. The model, by contrast, states what it is waiting on every turn
and can revise it.

Without the one-turn limit, an agent that answered "the build is going" would be
asked for another opinion every ten seconds for the length of the build.
