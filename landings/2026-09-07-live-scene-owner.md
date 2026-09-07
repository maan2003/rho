# Live scene owners replace the walk's Workspace allowlist

The host connection word now declares a typed `LiveOwner` with a one-second
cadence at its render site. The walk accepts changes during `AdvanceTime` only
when every changed scene owner carries such a declaration and has not changed
faster than its declared cadence. `Idle` remains strictly still.

Normal generated walks now create a workspace without a host. A focused
regression test opts into a detached host so its retry/status transition is the
only timer-driven behavior under test.

## Follow-up: primitive identity within an owner

The investigation also exposed a recorder limitation. Primitives are matched by
paint-order slot within a coarse owner and vertical band. Inserting or removing
the right-aligned connection word can therefore pair unrelated left- and
right-aligned glyphs and report apparent movement across roughly 3,400 pixels.
A robust fix is not just a diff heuristic: GPUI would need to propagate a stable
source element or text-run key into recorded primitives, then match by that key
before ordinal position. That is a moderate, separate change and should follow
in its own commit.
