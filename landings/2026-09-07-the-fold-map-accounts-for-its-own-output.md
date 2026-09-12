# The fold map accounts for its own output

The layer-by-layer accounting question, asked of the fold map at its own
output: does this snapshot's output extent equal the last one's plus the
net of the edits handed over with it.

A sync that answers no is telling the layers above about a document that
does not exist. They find out later and somewhere else - as `display point
out of range` in `FoldPoint::to_offset` reached through `BlockMap::sync`,
or as rows a snapshot claims and the chunks decline to yield. That distance
between where the fault is made and where it is felt is the whole cost of
not asking.

## What this adds

Two records under `wrap-test-support`, beside the widening record:

- `FoldMap::take_accounting_violations` - every sync whose emitted edits
  did not account for the change in its own output extent. Asked after
  consolidation, because consolidation is part of what is handed over and a
  fault introduced there would be invisible before it.
- `FoldMap::take_end_convergences` - how many edits had both ends widened
  to the end of one and the same fold.

Both reach rho through `DisplayMap`. Neither changes what the fold map
does; they change what it can be asked.

## Why the second one is a count and not a record

Two ends of an edit meeting at one buffer offset is legitimate when it is
the end of one and the same fold on both sides: the fold is re-emitted
whole, so both ends move to its end and that end is the same text on both
sides. Ends that converge for any other reason - two different folds whose
ends happen to land on one offset, or a common step carrying one end
further than its own fold needed - are the end's version of the inverted
range the common step used to hide at the start. The folds are compared as
buffer ranges, which is the only coordinate the two sides' folds can be
compared in.

The faulty shape goes in the violation record. The legitimate shape is
counted, and that is eng-8gpr's point rather than mine: without the count,
a document whose ends meet legitimately and one whose ends never meet at
all produce the same silence, and "the record never fired" would be partly
a statement about the documents rather than about the rule.

It earned itself immediately by refuting the person who built it. I had
reported that the eight-byte accounting gap on the underflow document was
the end rule leaving the two ends at different boundaries. The count says
the ends converge, at buffer offset 1467, at the end of the fold,
legitimately, in the same sync where the accounting record fires. So the
fault is one step further in: the ends meet correctly and the output
offsets built from them do not, because each is the transform's output
start plus an overshoot counted in inlay bytes while the two sides'
transforms begin at different buffer positions.

## What the records say today

`fold_widen_underflow` is the deterministic reproducer and it asserts all
three things: the widening record empty, the accounting record saying
exactly the one known line, and the convergence count above zero so the
first two mean what they appear to.

```text
fold output accounting: ... net -8 ...
```

The accounting assertion is written as full characterisation on purpose,
and this is the opposite shape from the emptiness the same file asserts one
assertion earlier. The distinction is eng-8gpr's: emptiness is the
assertion when the rule is fixed, full characterisation is the assertion
while a known fault remains and you want the day it changes to be loud.
This line is meant to fail on the day the end rule is fixed rather than
guarded.

## What a full sweep says, and what it does not

The whole rho-gui suite, single-threaded, both branches instrumented:

```text
running 293 tests
test result: ok. 290 passed; 0 failed; 3 ignored; finished in 1050.01s

accounting violations          1   (fold_widen_underflow)
converged legitimately         2   (fold_widen_bias, both cases)
converged faultily             0
```

The coverage line is kept deliberately. An earlier run of this sweep
produced an empty output file at the same moment the disk filled, and I
reported it as a run killed by the disk; it had in fact finished, and the
file was empty because the whole run was piped through `awk`, which writes
nothing until the pipeline ends. A run whose extent is unknown cannot be
told from a run that did not happen.

On the convergence count: zero faulty across 290 tests is half of a bar
that was set before the number arrived, and the other half is not met. Two
legitimate convergences, both in tests written to exercise this exact
machinery, is consistent with the check only firing where it was pointed.
The instrument, the hypothesis and the two documents all have the same
author. So the honest line is **never observed faulty on the documents we
have, and the rule is unproven** - a finding about the instrument rather
than about the end rule, and the sixth investigation does not lean on it.

The accounting record is in the opposite evidential position and the two
should not be read as one result. It fires where nobody pointed it: before
`a83a1ce3` it fired on three transcript streaming tests written for
entirely unrelated reasons. Those are recorded in the guard cut that
follows this one.
