# SPEC-stuck-excerpt: An edge projection of an editable excerpt

## Record justification

Only the editor owns conversions between window pixels and document positions. A separate editor could not project the same excerpt without splitting selection, motion, search, and input.

## Contract

An editor can designate a path with one excerpt in its MultiBuffer. The excerpt is ordinary text while any of it is visible. When it has passed out of view past its designated edge, the projection paints and hit tests the same rows at that edge without reordering or adding display rows. A bottom-designated excerpt in a short document remains aligned with the lower edge. A top projection keeps the text and gutter at their inline origin and reserves a fixed band beneath the text, with a seam and a shadow over the history. Its gutter bar runs through the band to the seam, and any notice sits in the band. The projection takes no more than half the viewport, including any reserved band or notice row. A tall projection scrolls inside itself on selection motion or pointer scroll. Half-page viewport motion preserves point in the designated excerpt. Point motion and search retain document order. An optional notice clears when the excerpt comes inline and does not change the rows available to the draft or history.

This implements [window.Edge-stuck excerpt](../../../../product/window.md#concept-edge-stuck-excerpt).
