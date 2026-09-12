# Each side of a widened edit names its own fold's start

`FoldMap::sync` widens an edit that lands inside a fold out to the fold's
start, because a fold has to be re-emitted whole. It used to widen both
sides by one common step, the larger of the two distances, on the premise
that the bytes before an edit are the same bytes before and after it.

That premise is false wherever there are inlays, and inlays are the case
that matters: an inlay is bytes the new side has and the old side does not.
When the new side sits inside a fold and the old side sits at the top of the
document, the two sides do name the same boundary - the top - and the
distance between them is inlay text standing in front of it. A common step
cannot say that.

The invariant is a statement about buffer offsets. Every fault this
answers made it in inlay offsets.

## What replaces what

The clamp landed this afternoon as `885076a1` is **replaced, not added to**.
The clamp kept the arithmetic safe - it stopped `delta` being subtracted
from a side that did not have it, which arrived as `edit.old
18446744073709551402..2674` and a seek asked to walk forward from the end of
the tree. It did not make the premise true. What it left behind was an edit
whose start was past its own end, `new 214..3` on the underflow document and
`new 210..0` on another. Better failure, same failure.

The rule now: each side is widened to the boundary its own fold starts at.
The boundary is named once, in buffer coordinates, by converting each side's
fold start with `to_buffer_offset` and taking the earlier of the two. It is
then said back on each side in that side's own coordinates. The two sides
may differ there, and only there, by the inlay bytes standing in front of
the boundary on that side.

Saying a buffer offset back on one side is ambiguous exactly at an inlay: a
buffer offset has one inlay offset in front of the inlay text and one behind
it. The choice is made by the **direction of travel** and never by the
inlay's own bias. A start travels backwards and takes the offset before the
inlay; the inlay finishes inside the widened edit, which is where it
belongs, because the layers above are being told this stretch is re-emitted
and an inlay just outside it is text they are not told changed.
`to_inlay_offset` will not do this on its own: it resolves the ambiguity by
the inlay's own bias, which is right for a caller asking where a buffer
position is and wrong for a caller asking what a widened edit covers.

## The step-over

The other half of the same cut. When an edit's coverage ran past the
transform the cursor was in, `sync` took that transform whole and widened
the edit to its end. On a document whose last transform is one long
isomorphic run, that is the whole document, every batch.

Two things wrong with it. It is a correctness hazard - the edit named rows
that had not changed and `BlockMap::sync` panicked with `display point out
of range` - and it is a cost-rule violation: O(rows composed so far) per
batch, on a rule that says per event O(rows the event touches) plus
O(log n). That is a candidate answer to the transcript getting slower the
longer it is, and it is the part of this the user would notice.

It now emits the undescribed remainder of an isomorphic transform from the
new snapshot and steps the cursor past it without widening the edit. Only a
fold is taken whole, which is the case the rule was written for. On the
drain document the edit stays `1615..2895` of a 4076-byte document, where
before it ran to the end; the new tree's input length equals the inlay
length at every step.

The merge that grows an edit's end and absorbs the later edits it now covers
is one routine with the invariant as its doc comment, called from the merge
loop and from the step-over, rather than two copies of one rule.

## What it answers

By construction:

- The unsigned wrap of `885076a1`. Neither side is asked for bytes it does
  not have, because neither side is moved by the other's distance.
- The inverted edit the clamp left behind. Both sides move back to one
  boundary, so a start cannot end up past its own end.
- `cannot seek backward` in `Cursor::slice` inside `FoldMap::sync`. **The
  crash the user hit on their live build, `fold_map.rs:683` reached through
  `reconcile_inlays` and `Editor::splice_inlays`, is this site, and this
  cut removes it.** Their build is an older main, so the line number is
  theirs and not today's; on the main this cut sits on it is
  `fold_map.rs:737:46`, the same `cursor.slice(&edit.old.start,
  Bias::Left)` and the same column. The held-back drain document reaches it
  there and does not reach it with this cut. Same shape, not a new one.

  The mechanism, since a batch is what makes it: the step-over widened the
  edit to the end of the transform it was in, which on a document ending in
  one long isomorphic run is the end of the document. The cursor was then
  past every later edit in the same batch, and the next slice was asked to
  go backward. `reconcile_inlays` hands over a batch, which is why that is
  where the user met it and why a single-edit document never does.
- The step-over's O(document) reach.

By test:

- `fold_widen_underflow.rs`, 8gpr's document, now asks for a widening
  record with nothing in it. It used to characterise three lines: two
  clamps and the inverted edit. It was the right assertion for a rule that
  no longer exists.
- `fold_widen_bias.rs`, two cases: an inlay of each bias standing on a
  boundary a start is widened to. The record reports an inlay left adjacent
  to a widened side rather than brought inside it. The left-biased case
  fails without the direction-of-travel step and passes with it; the
  right-biased one passes either way, because that is the half
  `to_inlay_offset` already gets right, and it is written out so the
  difference is visible.

## What it does not answer, and what is held back

The end of an edit is still widened by one common step. This cut changed
the start rule only, which is what was agreed. The symmetric change at the
end was tried and is worse: two ends inside one fold legitimately converge
on a single buffer offset when the fold is re-emitted whole, so the
gap-preserving statement that is true at the start is false at the end, and
accounting broke at every step rather than at one. It was reverted.

Held back with it:

- The two mirror bias cases on a widened end. The right-biased one fails
  today: `an inlay stands on the widened old end at 1467 and was left
  outside the edit`.
- `fold_widen_drain.rs`, which is red for a fifth fault this cut neither
  causes nor cures.

That fifth fault is at the fold map's own output. After consolidation, the
old output extent plus the net of the emitted edits should equal the new
output extent. It does not:

```text
UNACCOUNTED old_extent 3212 + net 6 = 3218, new_extent 3212     (drain)
UNACCOUNTED old_extent 2462 + net -8 = 2454, new_extent 2462    (underflow)
```

The fold map tells the layers above that the document grew by six bytes
while its own output extent did not change, and that six bytes arriving
upstairs is the `display point out of range` the drain document panics
with. It is on main - the second line above is main's `fold_map.rs` with
the accounting instrument and nothing else, on 8gpr's test, which passes
in both trees and has been carrying it silently since it was written. It is
not `Tail`-specific; the same document under `ElisionPolicy::Hidden` fails
at the block map's row assertion instead.

It is not what it first looked like. The reading offered was that the
inserted bytes fell inside a fold and stayed hidden, and the edit carried
their inlay extent rather than their fold-output extent of zero. The
instrument refutes it: on both documents the cursor sits on an isomorphic
transform when the end is computed, not on a fold, so the overshoot is
honest output. What is wrong is that the snapshot's total output extent did
not change, which means bytes were lost elsewhere with no edit emitted for
them - and on the underflow document the two ends stand eight apart, the
length of the inlay, which is the end rule leaving them at different
boundaries.

The accounting assertion goes under `wrap-test-support` beside the widening
record next, and the crash recipe gets it before anything else: the
transcript crash is 247 rows claimed against 233 yielded, which is the same
shape as six bytes announced upstairs that the output never grew by.
