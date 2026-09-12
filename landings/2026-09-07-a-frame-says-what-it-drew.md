# Landed: a frame says what it drew

Two cuts have now made the transcript cheaper — the inlay map seeks instead of
walking, the dashboard patches instead of composing — and the transcript's own
prepaint, p50 5.1 ms in the user's reports and 26.4 ms of a 27.2 ms frame on
the rig, is still unexplained. It is unexplained because nothing in a report
could be divided by anything. A frame recorded how long it took and not how
much there was to do, so "slow" could be measured and never accounted for.

This is the instrumentation for that, in two commits: the vendored gpui
profiler API, then the recording and the reader.

### What is recorded

**A frame carries its scale.** `FrameWorkScale` — visible rows, total rows,
blocks, excerpts, inlays, cursors — accumulated from every element that
reported during the frame, so a window drawing two editors reports what the
window drew. The editor element fills it in prepaint from numbers it has
already computed, plus two new O(1) accessors: `MultiBufferSnapshot::
excerpt_count`, which reads the count already in the excerpt tree's summary,
and `InlaySnapshot::inlay_count`. Neither the excerpt tree nor the transform
tree has an O(1) length, and walking either once a frame would cost more than
the measurement is worth.

**A stage says what it worked on.** `EditorTiming` gains `transforms`,
`affected_start`, `affected_end` and `affected_offsets`. `InlayMap::splice`
fills them: the inlay count, the affected span, and the distinct offsets inside
it — which is the number a splice actually pays for, twice over, and is not the
same as the span's width. `EditorTimingKind` gains `SyncTree` and
`SpliceInlays`, typed rather than spelled, because a cost oracle matching on
strings breaks silently when a stage is renamed.

`transforms` is the map's inlay count rather than a node count, deliberately:
the two are proportional, the cost is in the inlays, and counting nodes would
mean the walk this whole line of work exists to remove.

**Work outside a frame has an owner.** `MainThreadWork` records a span with a
typed `MainThreadWorkKind` and its own `work_units`. `handle_model_events`
records the batch it reconciled and `sync_tree_rows` the agents it was told
about — both run outside every frame span, and both are what make the *next*
frame late. Before this, no report could see them at all.

**The rings say how much they dropped.** `frames_pushed`, `editor_pushed` and
`main_thread_work_pushed` are how many records were ever pushed, against
however many survive. This is the smallest change here and it corrects the
worst misreading of all: the editor ring holds 4,096 records, and on the user's
reports it had covered *seconds* while the frames covered *minutes*, so every
stage total set against a frame total was a comparison between two different
windows of time. The reader now says so in as many words when the ring dropped
anything.

Schema goes to version 11. Older reports are read exactly as before and the new
sections are simply absent rather than printed as zeros, which a test asserts
directly — a test that only checked the new report would pass equally against
code that printed the sections unconditionally.

### What it is aimed at

Two live faults, and it was written to answer both rather than in the abstract.

The transcript's prepaint. On the rig at ecdec381, opening a transcript costs
27.2 and 32.0 ms, **almost entirely prepaint, on a single invalidation** — so
it is not how much was dirtied. (That session was reported as ced14f55 and was
not: it ran a stale GUI binary, which is why `rig up` is about to start
printing the identity of every binary it launches. The observation is about
where the time goes and survives the correction; the commit label does not.) With the scale beside it that frame can now be
divided by its rows, its excerpts and its inlays, and the next report will say
which of the three it tracks.

The crash. `line_ix N out of bounds - row_infos.len(): N+1, line_layouts.len():
25` reproduces three times of three, and **the 25 does not move
while `row_infos` does** — a count that stopped rather than one that was
computed. A frame that records its own visible rows against its total rows is
pointed straight at that.

### Cost

Nothing is walked for any of it. Every entry point returns on the same
`frame_trace_enabled` check the existing rings use, which is one relaxed atomic
load and a cold-path return when tracing is off. The frame-work accumulator is
a single `spin::Mutex` on the same pattern as the rings beside it, cleared when
tracing is disabled.

### Vendored change note

Everything in gpui and the editor is additive: existing recorders leave the new
fields zero and no behaviour changes when tracing is off. `FrameWorkScale`, the
ring totals and `MultiBufferSnapshot::excerpt_count` would be useful upstream
and are worth sending; `SyncTree` names a rho layer upstream has no equivalent
of and is not. Upstream does not have this problem in this form — its editor
draws a file, where rows and excerpts are nearly the same question, whereas the
transcript is one multibuffer of thousands of excerpts with inlays spliced on
every model event.
