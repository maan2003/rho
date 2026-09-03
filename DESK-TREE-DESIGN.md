# The Desk as a Tree

A design for the desk's underlying primitive: what the document *is* once
the machine writes most of it. This continues DESK-DESIGN.md and keeps its
rules; it changes the store, not the experience.

This is a design, not a spec. It records the *why* behind each decision.
The engineer building it decides the mechanics, guided by these whys. Rho
has exactly one user, so nothing here needs to survive contact with anyone
else's habits.

## The problem

The desk began as a text file because text is the most freeform thing a
human can own. That was right when the human wrote every line. It is wrong
now, because the machine writes most of the desk:

- Agent rows, page rows, and every verdict (`:todo:`, `:deadline:`,
  `:defer:`, done, discarded) are machine edits into a human file. Each one
  is a CRDT splice, an undo entry, and a line the human did not type.
- Structure is *parsed* out of text: headings by stars, marks by property
  lines, agent bindings by trailing tags. Every feature that needs structure
  re-derives it from characters and hopes the parse agrees with the last one.
- Identity is a byte offset. A heading is "host + offset of its line", so an
  edit above it changes who it is. Skips, verdicts, folds, and the journal
  all key on something that moves.
- The desk's own principle, "the system never writes into the desk", is
  already false in practice and can only be kept by contortions (in-memory
  skips, tags that pretend to be text).

The design goal: keep exactly the experience the human has today, an org
outline edited with vim, and give the machine a store it can own honestly.

## Storage and sync contract

This section is the implementation contract used throughout this document.

### Cells and clocks

The daemon is the mandatory star hub. GUIs synchronize with the daemon and
never directly with one another. Structured state is a map from
`(node, field)` to `(stamp, value)`, where a stamp is
`{ device: DeviceId, version: u64 }`. Stamps compare by version first and use
device only as the tie-breaker. Each device uses a Lamport clock: its next
write is greater than every local or received version it has observed.

Every peer tracks a version vector `DeviceId -> u64`. A delta contains the
winning cells and verdict events whose device-local version is newer than the
receiver's vector, plus the sender's complete resulting vector. One ordered
writer connection owns a `DeviceId`, and the daemon rejects a second active
connection for it; this makes acknowledging a version mean that every earlier
write from that device was observed.

Tags are individual fields named `Tag(name)` with boolean values. They use
the same LWW rule as other cells, except that opposing values at the same
numeric version choose `true` regardless of the device tie-breaker. This is
not a causal add-wins set.

### Fields and kinds

All live nodes have these common fields:

- `Kind` (write once), `Parent`, `Deleted`, and `CreatedAt`;
- `State` (`Open`, `Done`, or `Dismissed`), `DeferUntil`, `Deadline`, and
  `PaceDays`;
- zero or more `Tag(name)` boolean fields.

New nodes supply all eight non-tag common fields in one mutation. Optional
timestamps are represented explicitly with `None`. `CreatedAt` is immutable;
it is also the sibling order key. Children sort by `(CreatedAt, NodeId)`, so
reordering existing siblings is deliberately unavailable in this slice.

An `Open` node is dealable only when it has a `Deadline`, or when its
`DeferUntil` has been reached. The post-wake curve is
`elapsed_since_defer - PaceDays`; an Open node with neither timestamp is a
plain Note/reference and is not a card. The migration mapping below keeps the
old ranking curves; only the displayed distinction between a woken Todo and a
woken Defer is intentionally lost.

Kinds add these typed fields:

| Kind | Required fields | Derived title |
| --- | --- | --- |
| `Note` | none; body is the node text CRDT | first/body display text |
| `Agent` | `AgentId`, `Host` | live agent summary |
| `Page` | `PageRef`, `Url` | live page metadata/URL |
| `Thread` | `Workspace`, `Channel`, `ThreadTs` | thread metadata |
| `PullRequest` | `Repo`, `PullRequestNumber` | repository and number |
| `File` | `Path` | file name from the path |

`Host` is the daemon's persisted `machine_seed` as a `u64`. GUI `HostId` is a
per-process attachment index and must never be persisted; the GUI resolves a
stored machine seed to the current live `HostId` using the host data it
already receives in `Ready`.

Ownership is derived rather than stored: `Note` is user-owned and every other
kind is machine-owned. Ordinary GUI mutations can create and update notes but
cannot create machine kinds or edit machine fields. A validated verdict may
write only its declared `State`, `DeferUntil`, or `Parent` changes on a
machine node. The daemon alone creates, updates, and deletes machine nodes.
A newly created node id must use the node namespace assigned to that
connection.

A migrated page may temporarily lack `Url`; this is explicit migration debt
that the GUI backfills from its page registry. Newly bound pages always
supply a valid HTTP(S) URL. `PaceDays` preserves the legacy cadence value.

Materialization does not repair storage. Every non-deleted node with a kind
is independently reachable as a root when its requested parent chain is
missing, cyclic, or crosses a deleted node. Otherwise it appears beneath its
requested parent. This rule prevents a partial or corrupt parent chain from
hiding an otherwise live node.

### Verdicts and undo

Verdicts form a grow-only log keyed by `(node, stamp)`. An applied event is
`Applied { verdict, at, changes }`; `at` must equal the event's key and the
enclosing mutation stamp. Every change records its exact before and after
cell values and the same atomic mutation applies those after-values. An undo
is `Undone { of }` plus writes that reapply the referenced event's
before-values. It is accepted only while the current values still equal that
event's after-values. Neither operation erases history.

Verdict shape is fixed: done/dismiss change the logged node's `State`, defer
changes its `DeferUntil`, and file changes its `Parent`. Todo atomically
creates the declared Note child, sets `DeferUntil` to now and `PaceDays` to the
curve's default, and records all three logical changes. The absent
pre-creation node is treated as virtually `Deleted = true`,
`DeferUntil = None`, and `PaceDays = 0` for this check. Undoing Todo restores
those values, preserving its cells and text as an invisible tombstone rather
than trying to remove CRDT history.

### Text

Only notes have editable text. Each note body remains a separate Zed text
CRDT (`NodeTextSnapshot`); structure is never encoded into that text. A text
operation must use the connection's assigned node/text replica namespace,
must be causally complete against the daemon snapshot, and leaves the body no
larger than 4 MiB. Repeating the exact operation and transaction is a no-op;
reusing its timestamp with different content is rejected. Machine nodes have
derived read-only titles and no editable body.

### Wire handshake and messages

The GUI persists one random `DeviceId` in its client-state directory. On each
connection it sends:

```text
DeskSync { device, known: Version }
```

The daemon binds that connection to the device (rebinding it to another
device is an error), allocates or retrieves its stable `u16` node/text
namespace, and replies:

```text
DeskSynced { node_namespace, delta: Snapshot, texts: Vec<NodeTextSnapshot> }
```

`delta.version` is the new complete frontier. `texts` is the complete text
snapshot set; cell sync is incremental, while text operations after the
handshake arrive as `DeskTextApplied`. A reconnect or
`DeskResyncRequired` repeats `DeskSync`; clients merge the cell delta and
merge text histories by operation timestamp before resuming writes. They must
not replace a newer locally observed text event with an older handshake
snapshot, because the snapshot response and event fanout are independently
queued.

Structured writes use:

```text
DeskMutationApply { mutation: CellMutation }
DeskMutationAccepted { stamp }
DeskMutationRejected { stamp, reason }
```

The mutation contains one stamp, 1–4096 cell writes, and at most one verdict
event; the daemon commits all of it or none of it. The stamp device must match
the bound connection and a new mutation version must exceed the accepted
frontier for its own device. It cannot jump beyond one more than the daemon's
global maximum. This admits ascending offline writes that predate other
devices' activity without permitting an unobservable clock jump. An exact
retry at or below that device's accepted frontier is accepted idempotently;
the same stamp with different content is rejected. Creation must be complete,
note-only, and use the assigned namespace. Field types, ownership, verdict
before/after values, verdict undo target, and bounded counts are validated
before commit.

After accepting a cell mutation (including a daemon-authored machine write),
the daemon broadcasts `DeskCellsAvailable { frontier }`. This is a poke, not
the delta: each client answers with `DeskSync { device, known }`. Text writes
use `DeskTextApply { node_id, operation, transaction }`; accepted operations
are broadcast as `DeskTextApplied`. A lagged daemon event receiver gets
`DeskResyncRequired` and must repeat the handshake. Page filing retains its
request/result exchange, but `DeskPageBind` carries `parent`, `page_id`, and
`url` so the daemon can create a complete Page node.

The cell protocol is wire epoch 6 (`rho/ui/6`; protocol logs `RUP5`). The old
tree operation, batch, and sequence messages do not coexist in the shipped
epoch; they remain only in the integration workspace until the GUI cutover
replaces its call sites.

## Core decisions and why

### The truth is a tree of nodes; the text is a view

The desk document is a movable tree assembled from typed cells. A node has a
stable id and kind; parent, state, deletion, timestamps, cadence, tags, and
reference data are fields. Notes alone carry user text in their own CRDT.
Children are a derived view sorted by `(CreatedAt, NodeId)`, not a stored
sequence. Structure changes are atomic cell mutations; text changes are
per-note CRDT operations. The outline the human sees is a rendering of this
materialized tree.

**Why:** the machine's job is structural (place this agent here, mark this
node done, surface that one) and the human's job is textual (write what
you think). One primitive that serves both is a tree whose nodes carry text.
With the tree as truth, a mark is a field, a binding is a field, and moving a
subtree is one operation that cannot half-succeed. Nothing needs parsing
because nothing structural is ever encoded as characters.

### Notes keep prose freeform

A Note body can contain the heading text, paragraphs, blank lines, and rough
thoughts the user writes. Nested Notes provide outline structure; reference
children appear as derived read-only rows below the body. Legacy heading,
prose, and draft nodes all migrate to Notes rather than surviving as distinct
kinds.

**Why:** prose remains a real text buffer instead of becoming a form, while
node identity and hierarchy no longer depend on parsing stars or slicing one
global character stream. Moving a subtree changes one `Parent` cell rather
than performing text surgery.

### Node identity is the identity

Every consumer keys on node id: the dealer's cards and skips, verdicts,
folds, the journal, and the surface MRU. Agent and Page nodes also carry their
typed external identities, but neither display titles nor byte offsets are
node identities.

**Why:** a skip that lands on the wrong Note because a line was inserted
above is not a small bug; it is the dealer lying. Stable ids are also what
make the journal analyzable: the same node across weeks is the same node.

### Semantic rows preserve editor semantics

The editor exposes a rendered node row as semantic content rather than an
accidental line in one global text buffer. Linewise vim actions (`dd`, `yy`,
`p`, `o`/`O`, `>>`/`<<`) identify the opaque node and produce one atomic cell
mutation. Note-body edits continue through their native text CRDT. The editor
retains its external undo entry only after `DeskMutationAccepted`; rejection
restores the last merged cells. Undo emits inverse cell writes, and verdict
undo additionally appends `Undone { of }` under the rules above.

Deleting a Note writes only its `Deleted` cell. Descendants are not rewritten:
the materializer independently roots any live child whose effective parent
chain crosses the deleted node. Undoing the delete restores the cell, so those
children derive their original placement again. Machine nodes follow the same
derived-root rule without extra writes.

**Why:** display inlays and excerpt boundaries must not change what a day of
vim feels like. If the editor treats a rendered Note row as unrelated
characters, deleting or pasting a line can leave its node behind. A semantic
row hook keeps modal behavior generic in the editor, node identity private to
Desk, and one `u` equal to one user action.

### Ownership replaces "the system never writes"

The old rule was structural in spirit and impossible in practice. It becomes:
Note text is only ever edited by the user; the daemon creates, updates, and
removes reference nodes, while a user's validated verdict may change the
declared state/defer/parent cell on any dealt node. Machine nodes are otherwise
read-only in the editor.

**Why:** the point of the old rule was that reading the desk tells you what
*you* decided, not what some process last did. Ownership keeps that promise
precisely: your text is yours, the machine's rows are visibly the machine's,
and every field the machine sets on your node traces back to a key you
pressed and a journal line that says so.

### Structure is never parsed from text, with one deliberate exception

No document parse exists. The one recognition kept is line-local: typing
`* ` at the start of a line can create a Note at that parent and place the
remaining typed content in its body. Org-style keys (new sibling, new child,
promote, demote) produce the same Note creation or `Parent` mutation without
typing stars. Existing text is never reparsed to discover structure.

**Why:** hands trained on org-mode type stars; taking that away costs more
than it saves. But the recognition is a keystroke-time convenience on one
line, reversible, and never re-run over the document. It is the opposite of
parsing: it decides once, at the moment of intent, and records a node.

### Native in the editor, not a projection beside it

The tree view is something Zed's Editor hosts directly, with the vim and
helix layers working across the whole outline: motions, search, visual mode,
text objects inside Note bodies, folding by subtree, and structure keys as
editor actions. Machine nodes render as read-only rows with today's styling:
bullets, end-of-line hints for marks, no ids anywhere.

**Why:** the current desk is a composition of excerpts spliced between
generated rows, and everything that touches structure has to reach around
the editor to do it. A third-class document is one that every new feature
has to special-case. If the desk is rho's home surface, its document must be
as native to the editor as a file is. The cost is paid once, in the vendored
editor, and every later feature (scopes, queries, bulk edits) inherits it.

### The daemon owns the document; sync is the same shape as today

The daemon stores winning cells, the grow-only verdict log, accepted mutation
records for idempotence, and per-note text histories. Desktop and phone merge
version-vector cell deltas with that hub. Concurrent parent writes converge by
LWW; missing, cyclic, and deleted-crossing chains derive roots. Sibling order
is always `(CreatedAt, NodeId)` and is not separately writable.

**Why:** the daemon already owns the desk and the phone already edits it
offline. Changing the primitive must not change who is responsible for it.

### One migration, no runtime backcompat

The first upgraded daemon start decodes and replays native-tree V1 through a
frozen decoder, converts it once, and atomically commits all V2 tables plus a
completion marker. Headings, prose, and drafts become notes; agent, page, and
file bindings become their corresponding typed nodes; typed marks become the
common state/defer/deadline/pace fields. Unchanged per-node Zed text histories
are transcoded structurally, not replayed through rendered org text. Raw
tombstones and parent candidates are copied, including missing/cyclic parent
chains; materialization applies the rooting rule later. No runtime path writes
V1 after cutover.

Legacy headings with file bindings retain their Note identity and text. The
migration creates a typed File child for the active file binding in a reserved
migration namespace; this preserves the old relationship without putting a
machine-only `Path` field on a user-owned Note. Historical superseded or
cleared bindings are not replayed.

For each destination field, the latest active legacy mark wins:

| Legacy mark | `DeferUntil` | `Deadline` | `PaceDays` |
|---|---|---|---|
| Todo | Todo date | unchanged | Todo pace |
| Defer | Defer date | unchanged | `0` |
| Reminder | Reminder date | unchanged | Reminder pace |
| Deadline | unchanged | Deadline date | Deadline pace |

`PaceDays` uses the latest active mark among these four kinds. This retains
the pace Deadline's curve reads on both sides of its date; only Defer is
deliberately zeroed because its legacy curve did not read pace.

The migration logs counts by new kind, nodes rooted by the chain rule, marks
dropped (normally zero), warnings, and pages awaiting URL backfill. Validation
must also be run on a copy of the real user `rho.redb` before release; the
original is never used as a test input. V1 tables are retained untouched for
rollback rather than serving as a second truth. If the first upgraded start
fails, the V2 transaction and marker are absent and the old binary can read
the unchanged V1 tables. After V2-only writes begin, rollback requires the
pre-upgrade database copy because an old binary cannot see those writes.

#### Real-database validation (2026-09-03)

Validation used a physical copy of the user's 42 GiB `rho.redb`, taken while
the daemon was stopped with `SIGSTOP` by a detached, trap-guarded script and
resumed unconditionally with `SIGCONT`. The copy took 40 seconds; the live
database was not opened or modified by the test. Because the copy represents
a crash-time snapshot, redb recovery plus a committed empty write took about
350 seconds. That recovery cost is separate from the migration measurement.

The frozen decoder successfully read and replayed all four V1 Desk tables
(`state`, tree ops, text ops, and batch ops). A clean atomic V1-to-V2 migration
then completed in 228 ms with this report:

| Result | Count |
|---|---:|
| Note | 219 |
| Agent | 108 |
| Page | 7 |
| File | 12 |
| Rooted by chain rule | 0 |
| Dropped marks | 0 |
| Pages awaiting URL backfill | 7 |
| Heading/file bindings split into typed File children | 12 |

The post-migration redb table scan reported these Desk tables (bytes are
logical table statistics, not the 42 GiB database file's allocated size):

| Table | Rows | Stored bytes | Metadata bytes | Fragmented bytes |
|---|---:|---:|---:|---:|
| `rho_desk_tree_state_v1` | 1 | 123,060 | 8 | 8,004 |
| `rho_desk_tree_ops_v1` | 0 | 0 | 0 | 0 |
| `rho_desk_node_text_ops_v1` | 69 | 17,563 | 564 | 18,737 |
| `rho_desk_batch_ops_v1` | 25 | 13,629 | 280 | 10,667 |
| `rho_desk_cells_v2` | 3,025 | 593,067 | 47,904 | 538,677 |
| `rho_desk_node_text_v2` | 336 | 59,736 | 3,980 | 50,972 |
| `rho_desk_cell_meta_v2` | 1 | 135 | 8 | 3,953 |
| `rho_desk_tree_migrated_v2` | 1 | 898 | 8 | 3,190 |
| `rho_desk_mutations_v2` | 0 | 0 | 0 | 0 |
| `rho_desk_verdicts_v1` | 0 | 0 | 0 | 0 |

The initial validation run was useful rather than green: it found the real
heading/file shape absent from the checked-in fixture. Migration now handles
that shape as specified above, and the clean rerun succeeded. The V1 tables
remain in the migrated copy, so a pre-V2-write rollback can still use them;
the physical pre-upgrade copy remains the rollback boundary after V2 writes.

**Why:** rho has one user and the desk is small. A frozen one-way decoder and
an explicit backup provide recovery without carrying a legacy compatibility
facade in the live protocol or write path.

## What stays the same for the human

- The outline looks and edits exactly as now: stars, indentation, vim.
- Folding, jumping, the overview, dealing, verdict letters, marks shown as
  end-of-line hints.
- Writing anywhere. A node with no marks and no bindings is just a note.

## What changes for the machine

- Cards, skips, verdicts, folds, journal entries, and the MRU key on node id.
- Verdicts set fields; nothing is written into text.
- Agent and page rows are nodes the system owns, placed under the Note
  that binds them, updated in place, removed when archived.
- The dealer reads marks as fields; the curve library takes typed marks.
- The journal records node ids, so week-over-week analysis is possible.

## Deliberately deferred

- Scopes (a top-level node as the dealer's and the MRU's filter).
- Tags as queries: runtime groupings by field, not by hierarchy.
- Richer fields (estimates, people, links) on nodes.
- Bulk textual editing of structure (wdired-style): with the tree as truth
  it becomes a reconciler over a rendered view, and is not needed for v1.
- Multiple desk documents.

## Symptoms to watch for

- The engineer reaching for a parse of rendered text to answer a structural
  question. That is the old model leaking back.
- Machine writes appearing in user nodes' text for convenience.
- Ids surfacing in the UI.
- Note text that can no longer be written somewhere it could be written before.
- Edits in the editor that feel slower or lossier than a plain buffer.

## What done means

The org desk is imported, the text parser is gone, the dealer and journal
use node ids, machine rows are nodes, the phone and desktop sync the tree,
and a day of ordinary use in vim feels no different from the text desk.
