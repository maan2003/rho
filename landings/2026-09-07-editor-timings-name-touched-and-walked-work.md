# Editor timings distinguish touched rows from walked work

Editor pipeline timings now report two costs that the existing outer-span and
document-size fields could not answer:

- `touched_rows` sums each edit independently, taking the larger of its old
  and new row extents. Point-coordinate edits include their row; half-open row
  ranges use their length and count an empty point edit as one.
- `walked_items` counts repeated pipeline items actually visited. SumTree
  cursors count leaf items crossed by next/prev/filter/seek/slice, but not items
  inside an untouched subtree reused wholesale. Display stages add the
  non-tree chunks or decorations they explicitly scan, and MultiBuffer adds
  buffer states and changed paths to its excerpt and diff-transform cursor
  work.

This keeps a one-row edit in a roughly 2,000-item tree visibly small while
still explaining MultiBuffer syncs which emit no text edits: those records
correctly have zero touched rows, but now name the buffer/path scan that did
the work.

Fold, tab, wrap sync and async update, block, inlay, and MultiBuffer sync all
populate the fields. The cursor control verifies that a seek across 2,000
items reuses internal subtrees rather than claiming a full walk, that one
subsequent item increments work once, and that reset does not erase cumulative
work. The GPUI ring test verifies both fields survive recording.

The one-row wrap regression control models four chunks plus three cursor
crossings and requires the reported total to remain 7; this specifically
guards against replacing accumulated chunk work with the cursor subtotal.

## Finding: inlay rebuild still scans every inlay

`append_transforms_from` filters every inlay and resolves every valid anchor
for each edited rebuild suffix. That remains O(inlays log n) per edit; this
change measures it and does not attempt to fix it.

Upstream Zed did not expose this distinction: its span-based ztracing records
stage duration but has no numeric per-event bound in terms of touched rows.
The cursor counter added here is a saturating u64 incremented once per crossed
leaf item. A seek or slice which reuses a shared subtree contributes zero for
that subtree, rather than pretending it visited all of its leaves.
