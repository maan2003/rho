# Landed: an edit widened into a fold keeps its own start

A transcript could not be opened. The GUI panicked on the frame that drew it,
in the fold map, with `cannot seek backward` — a cursor asked to walk forward
to an offset far behind where it already stood. The edit handed to that seek
had a start of 18446744073709551402, which is two to the sixty-fourth minus
214: an unsigned subtraction that wrapped.

This is a regression of `e8e76a50`, landed earlier the same afternoon. That
change made an edit reaching into a fold widen on both sides rather than one,
because widening a single side leaves the two sides naming different
boundaries and the map above adds up rows the snapshot beside it does not
have. It took the larger of the two sides' distances into their folds and
subtracted it from both starts, on the premise stated in its own comment:
the bytes before an edit are the same bytes before and after it, so the two
sides move by the same inlay bytes.

## Where the premise fails

It holds while whatever shifted the two sides apart sat in *front* of the
fold. Then the fold's own start moves by the same amount, both distances come
back equal, and the step is the room both sides have.

It fails when something shifted one side from *inside* the fold. An inlay
anchored within a fold pushes the new side out by its own length and leaves
the fold's start exactly where it was. The new side is then deeper into its
fold than the old side is from the top of the document, the larger of the two
distances is more than the old side has to give, and on unsigned offsets the
subtraction wraps rather than refusing.

The loop's own numbers on the case that reproduces it:

```text
old 2..2   new 2..216     old_delta 2   new_delta 2      fine
old 5..5   new 219..227   old_delta 5   new_delta 219    5 - 219
```

## The change

The step becomes the room both sides actually have: the larger distance,
clamped to each start. Where the clamp bites, the loop stops widening rather
than moving one side alone — the edit stays where the document allows it,
which is the property the original change was for.

## The test

`crates/rho-gui/src/tests/fold_widen_underflow.rs`, written by eng-8gpr and
handed over with the fault: a 24-row document, one fold anchored from offset
zero, and a single splice carrying a 214-byte inlay at offset 2 and an 8-byte
inlay at offset 5. It fails on the parent commit and passes here, in under
half a second, with no transcript and no rig.

Its module comment records the thing that is easy to get wrong twice: the
long inlay has to sit strictly inside the fold. Anchored at offset zero it
lands in front of the fold's start anchor, the fold moves with it, and the
document is symmetric again — the first attempt at this test passed for that
reason. The test asserts the map is coherent afterwards rather than only that
it did not panic, so a wrap that happens to survive still fails it.

## What this does not fix

The transcript that found it still does not open. Two further faults in the
same function are known and are not in this cut:

- `FoldMap::sync`'s step-over case, from `c0267db9`, widens one edit over
  ground that later edits in the same batch still name and does not bring
  them with it, so the cursor ends up past an edit that has not been sliced
  yet. Reproduced in a two-edit transaction across a `Tail` fold's placeholder
  boundary; its own cut, next.
- A display snapshot that claims 247 rows while its own chunk stream yields
  233, which is what the user's `element.rs` crash indexes past. Not
  explained by either of the above, and still open.

Numbers: no frame measurements are claimed here. This is a panic fix reached
and proved entirely in unit tests; the rig session that found it produced no
frame profile, because a session that panics never lands one.
