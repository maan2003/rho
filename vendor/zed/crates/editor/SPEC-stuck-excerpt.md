# SPEC-stuck-excerpt: An edge projection of an editable excerpt

## Record justification

Only the editor owns conversions between window pixels and document positions. A separate editor could not project the same excerpt without splitting selection, motion, search, and input.

## Contract

An editor can designate a path with one excerpt in its MultiBuffer. The excerpt is ordinary text while any of it is visible. When it has passed out of view past its designated edge, the projection paints and hit tests the same rows at that edge without reordering or adding display rows. A bottom-designated excerpt in a short document remains aligned with the lower edge. The projection takes no more than half the viewport, including any reserved row for a notice. A tall projection scrolls inside itself on selection motion or pointer scroll. Half-page viewport motion preserves point in the designated excerpt. Point motion and search retain document order. An optional notice clears when the excerpt comes inline and does not change the rows available to the draft or history.

This implements [window.Edge-stuck excerpt](../../../../product/window.md#concept-edge-stuck-excerpt).
