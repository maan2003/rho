# The walk separates touched rows from walked tree items

Editor timing now reports two different quantities for every walk event:
`touched_rows` is the largest per-stage sum of the individual edits' row spans,
while `walked_items` is the sum of SumTree leaf items actually crossed by all
pipeline stages. The CLI prints both on each step, slow-frame finding, failure,
and run summary. Whole-document `old_rows`/`new_rows` remain only the value of
n used for the logarithmic allowance; they are no longer presented as work.

The profiling oracle bounds the event by summing, per stage, twice
`touched_rows + ceil(log2(n)) + 2`, then allowing 64 fixed work items. It rejects
a linear leaf walk hidden behind a one-row edit without multiplying document
size by the number of pipeline stages. Frame work reports rows actually drawn
separately as `drawn_rows`; `total_rows` remains document scale only.

`walked_items` deliberately counts leaf items crossed, including items consumed
by seek and slice, but not an untouched subtree shared wholesale. It therefore
finds accidental full-tree walks; it does not count internal-node probes and
cannot by itself distinguish one logarithmic seek from many logarithmic seeks.
