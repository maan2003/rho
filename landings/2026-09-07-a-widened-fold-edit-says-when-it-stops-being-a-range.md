# Landed: a widened fold edit says when it stops being a range

Two fold-map faults were found on the desk rig this week, and neither of
them said what it was. One is an unsigned underflow — an edit widened out to
a fold that begins at offset zero, asked to give up more bytes than it has
in front of it — which in a release build wraps to about eighteen
quintillion and surfaces later as a seek that cannot go backward. The other
strands an edit behind a cursor that stepped over it, and surfaces as the
same message at a different line.

In both, the edit is made wrong in one place and the panic happens in
another, far enough apart that the message names the wrong layer. Whoever
reads that panic has to work back through the widening loops to find out
what was actually asked for.

## What it does

`FoldMap::sync` now records, under `wrap-test-support`, every widened edit
that stopped describing a range:

- inside the widening loop, **where the clamp bites** — the two sides were to
  move by the same number of bytes and one of them could not, which is the
  premise in that loop's own comment failing. Recorded before the `delta == 0`
  break, because a side with nothing in front of it clamps to zero and would
  otherwise leave by the quiet door, which is precisely the shape the rig
  crash had;
- after the widened edit is assembled, a range whose start is past its own
  end, or whose end is past the extent of the tree it is a range of.

`DisplayMap::take_fold_widening_violations` hands them over. Empty is what a
healthy sync gives.

The predicate is deliberately narrow. It does not know what the right answer
is, only what cannot be one. Neither check needs a model of the document.

**The clamp is why this is worth having rather than redundant with it.**
eng-b8os's clamp keeps the arithmetic safe; it does not make the premise
true. An edit widened by less than the fold it was widened to still names a
fold that the layers above will be told about in full, and after the clamp
that disagreement leaves no trace at all — the subtraction simply takes less
than it asked for and says nothing. This is the record of it.

## The instrument is shown to work, both halves

The vendored editor is not a workspace member and cannot run its own tests
here, so a check written into it would otherwise be an instrument nobody had
ever seen produce a reading.

- The predicate is exported under the same feature and `fold_widening_check.rs`
  takes **both** of its answers on all three cases — a range that starts after
  it ends, a range past the tree, and healthy ranges including the empty one
  at the very end that would be the first false positive.
- The in-loop record is asserted by `fold_widen_underflow.rs`, the test that
  used to panic here: with the clamp in, that document now reaches the end of
  the sync, and the test reads the record back and requires the clamping to
  have been reported. The case that produced the crash is the case that
  proves the instrument.

An earlier draft of this change recorded "a side asked for more than it has",
which the clamp had already made unreachable — the check would have compiled,
never fired, and read as evidence of health. Recording where the clamp bites
rather than where the subtraction would have wrapped is the difference
between a live instrument and a decoration.

## Whose

The faults are eng-b8os's, from their two fold cuts, and the fixes are
theirs. This is the instrument they asked for, in the file they own, with
their say-so; the shape — assert where the edit is built, not where the seek
trips over it — is theirs as well.
