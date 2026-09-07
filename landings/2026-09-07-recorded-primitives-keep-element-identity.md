# Recorded primitives keep their element identity

GPUI's test scene recorder already had an element path on every recorded
primitive, but ordinary `Drawable` painting updated only the window's element
stack. It now mirrors that stack into the test scene while painting. This
widens recorded scene owners from view and vertical band to the explicit
element IDs declared at the render site. A keyed connection label is therefore
removed as its own primitives instead of being paired by paint-order slot with
an unrelated unkeyed label that moved into the same band.

The production scene and renderer are unchanged. Upstream Zed did not need
this distinction because `recording_elements` is compiled only for tests or
the `test-support` feature, and no Zed production path reads it.

The warm 320-event walk gate takes about 43 seconds, or 134 ms per reported
event. Deterministic verification actually runs every event twice, so that is
about 67 ms per apply/draw phase. In a second warm run, the first pass's 320
recorded event draws averaged 18.2 ms and occupied about 22% of wall time after
allowing for the replay. The remainder includes ten app/workspace setups,
cold, warm, and baseline draws, event settling, scene hashing and comparison,
and reporting; it is not time attributed to one event path.
