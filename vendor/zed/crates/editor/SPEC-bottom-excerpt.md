# SPEC-bottom-excerpt: A bottom projection of an editable excerpt

## Record justification

Only the editor owns conversions between window pixels and document positions. A separately editable viewport over the same excerpt cannot be emulated by another editor without splitting selection, motion, search, and input.

## Contract

An editor can designate a path with one excerpt in its MultiBuffer. Project that excerpt at the viewport's lower edge only when its first display row is at or below the bottom of the ordinary viewport. The projection paints and hit tests real display rows and selections with a second viewport over the existing document. If the entire document fits, it sits at the bottom of the viewport. It never inserts or reorders display rows. Its height cannot exceed half the viewport; a taller excerpt scrolls internally on selection motion or pointer scroll inside it. Half-page viewport motion preserves a point in the excerpt, including when inline. Point motion and search retain ordinary document order. An optional label exists only while this projection is visible and clears when the excerpt comes inline. If it won't fit beside the text, its own row is already reserved within the projection's height limit.

`bottom_excerpt.rs` owns the geometry and internal scroll. `element/bottom_excerpt.rs` lays out and paints the projection using the editor's own text, cursor, and highlight code. PositionMap maps hits on the projection to its real display rows.

This implements [window.Bottom-stuck excerpt](../../../../product/window.md#concept-bottom-stuck-excerpt).
