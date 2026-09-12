# Scene recordings include coarse paint time per owner

GPUI's test scene recorder now attributes paint CPU time to each typed
`SceneOwner` in a frame. Timing starts immediately before root painting and
ends after the final painted element. The interval before each primitive is
charged to that primitive's owner, and work after the last primitive is charged
to the final owner. Replayed primitives are timed in the current frame rather
than carrying timing from the frame they reuse.

This is intentionally coarse. It identifies whether an expensive paint interval
lands in a transcript band instead of merely counting that band's primitives,
but computation which emits no primitive is charged to the next primitive (or
the final owner at frame end), and prepaint/layout is not included. Collection
exists only under GPUI test support, where scene recording already retains typed
primitive metadata; production drawing is unchanged.
