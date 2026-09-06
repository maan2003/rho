# The live tail, and the rest of the dumbing-down

Agreed 6 Sep after slice F landed (jj change `nzkrskxuxxmr`). This is
the destination; `AGENT-LOG-DESIGN.md` is the record of what landed
and why. Proofs on a `cp` of the store only; never touch the live DB.

## Principles

- The log is the truth. Anything derivable from rows is derived on
  the client, never carried on the wire a second time.
- Nothing on the wire is named `Ui*`. `rho-ui-proto` carries facts;
  render types live in `rho-registry`.
- The daemon does not diff, snapshot, or project. The loop says what
  changed as it changes it.
- Focus never loads. Commands load.

## The wire

One ordered feed per connection: journal rows and live deltas
interleaved in the order the daemon produced them. No per-agent
streams, no `AgentStreamOpened`, no generations, no decode budget.

`ServerMessage::Log { entries }` as now. `ServerMessage::Live {
agent_id, delta }` with:

```
enum Live {
    Requesting,                              // request went out; tail empty
    Item { index: u32, item: Item },         // first sight of an item, or a non-text change
    Appended { index: u32, text: String },   // append-only buffer grew by this
    Retrying { error: String },              // partial response dropped
    Waiting { until: Option<UnixMs> },       // tools running, or model asked to wait
    Idle,                                    // nothing in flight
}
enum Item {
    Text { text: String, phase: Option<MessagePhase> },
    Reasoning { text: String },              // summary joined, or raw content
    ToolCall { id, name, arguments: String },
}
```

No `Snapshot`, no `Clear`, no `Queue`, no `Status`. A joiner gets
`Requesting`, one `Item` per pending index, then the phase.

Ordering rule: the loop writes the row (`Replied`, `Sent`, `Turn`)
and then sends the next `Live` from the same task. The client applies
the row, then the tail. No position tag, no overlap window.

Live is server-wide per agent: the daemon unions `AgentStreamFocus`
across connections into one `live` set. Every connection forwards
every delta; clients ignore agents they are not holding. A dropped
connection drops its wants. Focus on an unloaded agent does nothing
until something loads it; then its deltas start.

Derived on the client from rows, never on the wire: the queue
(`Message` rows not covered by a later `Sent`/`QueueCleared`), running
tools (calls in `Replied` without a result in `Sent`), errors and
cancels (`Turn Ended(..)`), turn running, `context_used` (last
`Replied` usage).

`Detail` answers with `Item`s, not `UiBlock`s. `remote.rs` is deleted.

## Daemon

- The loop emits `Live` at its one publish site (`agent/mod.rs`
  `publish`) from what it just handled. `AgentState`, `AgentStateKind`,
  `PendingInferenceResponse` cloning in `snapshot()`, `ToolPreview`,
  `FailedInferenceResponse`, the `RwLock`+`Notify`, `subscribe()`,
  `state()`, `live_frame`, `in_flight_blocks`, all of `agent_ui.rs`'s
  `Ui*` go. `AStr::diff` gives `Appended` for free.
- Loaded agents in an LRU of 100. Touch on any command, mail, focus.
  Evict only when settled: idle, empty queue, no owed calls, not in any
  client's live set. Eviction drops the loop; the log is the truth.
- The loop keeps its `AgentHead` (config, role, parent, workdirs) in
  memory, updated by its own rows. Hot paths (`mcp_agent_tool`,
  mail, shell, terminal, `resolve_display_agent_id`) load the agent
  and read that. `get_agent` remains only inside `load`.
- Every command that names an agent loads it (`SendUserMessage` etc.
  currently error "agent is not loaded"; they must load).
- Presentation moves into the loop: after a turn ends the loop runs
  the title/activity pass itself and writes `Presented` rows. The
  sidecar, `watch_presentation`, and the daemon's other
  `agent.subscribe()` readers go.
- `agent_exists` is one key lookup (done). Cost series walks ids (done).

## Client

Superseded in shape by `GUI-MODEL-DESIGN.md` (6 Sep): no registry, no per-event rebuilds, the model off the main thread. What landed here is recorded in the step notes below.

- `UiAgentState`, `UiBlock`, `UiAgentStatus`, tool metadata types move
  from `rho-ui-proto::remote` to `rho-registry` as the render model,
  built from rows and `Item`s. `UiAgentUsage` goes (cost is the digest).
- Transcript fold becomes incremental: `TranscriptFold { state, next,
  open_calls: call_id -> block index, block_origin: Vec<AgentPos> }`
  with `tell(pos, event)` like the digest; `Rewound { to }` truncates
  blocks whose origin >= `to`. Live deltas append to the tail blocks;
  `Appended` is the incremental render case.
- Attention is derived, not stored: registry computes it from the
  digest plus `handled_through: AgentPos`, the only user fact, written
  in the same transaction as rows when it changes. The attention table
  goes.
- Mirror writer drains everything queued and commits once (catch-up
  batching). Tail stays one row per commit.
- Digest table carries a fold version; on mismatch at startup, refold
  every agent from its rows once.
- Active set of 4, LRU, focus = active set (done). Live for non-active
  agents is ignored.
- Claude runtime stays on the same log and fold; if provider rewrites
  do not map onto `Rewound`, add one row `TranscriptReplaced { rows }`
  the fold treats as truncate-and-reapply. No second sync mechanism.

## Where step 1 left things (6 Sep)

Landed as planned, with these deviations and leftovers:

- `Item::Reasoning { text }` (summary parts joined with newlines, or
  the raw content when there is no summary) rather than `summary:
  Vec<String>`: one buffer to append to.
- `Item::Text.phase` is a proto twin `TextPhase`, since
  `rho_core::MessagePhase` has no `Pack`/`Unpack`.
- `AgentState`/`AgentStateKind` stay inside `rho-agent` for now: the
  teller reads the kind, and `presentation.rs`, the pool's working
  child count and the daemon's turn watcher still read the state.
  Step 3 removes them with the sidecar.
- `AgentPool::agent_handle` (the `role-id` label) still folds the log
  through `get_agent`; it is sync and called from seven places. Make it
  read the loaded head when step 3 touches the pool.
- The turn watcher task outlives an evicted loop (it sleeps on a notify
  that never fires); harmless, gone in step 3.
- An `Error` phase tells `Idle`; the partial response of a failed
  request is not shown after the failure. The error itself is the
  `Turn Ended` row. (Fixed below: the `Failed` row.)

## Where step 2 left things (6 Sep)

Landed as planned, with these deviations and leftovers:

- Attention needs one more user fact than `handled_through`: whether
  the agent is muted. `Verdict { handled_through, muted }` is what the
  registry keeps and the mirror stores (`gui_agent_verdict_v1`); the
  attention table is retired. `rho_registry::attention` is the one
  decision; the desk card and the registry both call it.
- A verdict is written in its own transaction, when it changes. It is
  the user's fact, not derived from rows, so nothing can diverge.
- `TranscriptFold::state()` still clones the block list for the store
  on every row, and the store's `FrameSummary` compares block lists.
  Per row that is O(blocks) memcpy, not O(session) decode and fold.
  (Fixed below: blocks are shared.)
- Claude rewinds already reach the mirror as `Rewound` rows through
  the presentation-source reconciliation, so no `TranscriptReplaced`.
- The digest version is on each stored snapshot (`version`, senax
  default 0 for what was written before); a mismatch refolds that
  agent from its rows at startup and writes the digest back.

## Where step 3 left things (6 Sep)

Landed with these deviations:

- The published state is `AgentStatus { kind, queued }`. `AgentState`
  and `AgentStateKind` stay in `rho-agent`: the Claude loop owns an
  `AgentState` as its private transcript-and-queue field, the teller
  and the turn boundary read the kind. Nothing outside the loops sees
  an `AgentState`.
- "Presentation into the loop" landed as gating, not a rewrite: the
  sidecar stays, the loop drives it, and the pool sets a watched flag
  on a loaded loop when it enters or leaves the live set. The counted
  `Watch` handles, `watch_presentation`, the daemon turn watcher and
  the pool's activation observer are gone.
- `agent_handle` reads the loaded head when it can take the agents
  lock without waiting, and folds the log otherwise (cold agent, or a
  caller already holding the lock).

## Where the leftovers went (6 Sep)

- Blocks are shared: `UiAgentState.blocks` is `Vec<Arc<UiBlock>>`, the
  fold hands out pointers to the blocks it keeps and copies one block
  out of its sharing only when a row changes it (`Arc::make_mut`). A
  row costs the blocks it adds; taking the state and the store's
  summary walk pointers, and `Arc`'s equality sees a shared block
  before comparing text.
- A failed request is a row. `AgentEvent::Failed { partial, error,
  retrying, at }` carries what the model had said (the
  `PendingInferenceResponse`, so `Detail` can hand back its items);
  `MirrorEvent::Failed { text, error, retrying, at }` is its strip.
  The Rho loop writes it on every temporary failure (`retrying`) and
  on a final one, before the phase moves; the Claude loop on a final
  one. The fold shows the text as an assistant block, a retry as a
  notice after it; a final failure's notice is the `Turn Ended` row
  that follows. `Live::Retrying` is gone (wire `rho/ui/12`): a retry
  tells `Requesting`, and the row precedes it. The variant id is a
  hash of its name, so older rows decode as before; no migration.

## The old loop's rows (6 Sep)

The previous Rho loop wrote five variants the runtime no longer
writes: `InferenceResponse`, `ToolResult`, `Queued`, `Dequeued` and
`PresentationUpdated`. The migration rewrites them into the current
vocabulary so nothing after it reads the old world: `Queued` becomes
`Accepted`; the `ToolResult` rows of a turn and the items a `Dequeued`
delivered become one `Sent` (a `NextTurn` item held back at a
`NextRequest` boundary is accepted again at the end of the log, as the
old replay queued it); `InferenceResponse` becomes `Replied`, carrying
the context the old replay would have shown (a compaction clears it,
`Some` overrides it, `None` keeps the last); `PresentationUpdated`
becomes `Presented`. Old rows carried no time, so a translated row
takes the last time seen on the lineage (a tool result's `finished_at`
counts), or the agent's creation. The old enum and its payload types
live only in `db/legacy_events.rs`, read through `SenAs` under the old
type name, and go with the migration in the cleanup landing; the
runtime `AgentEvent` has no legacy variant. The layout proof replays
every agent both ways, the old replay over the old rows and the
current one over the translation, and holds history, owed calls,
context use and the queues equal: on a copy (6 Sep) 2824 agents
replay the same context with 1,054,194 old-loop rows translated, in
37 s including the migration.

## Rolling back (6 Sep)

`rho_agent::db::prepare` opens the store: when a migration is due it
takes a redb persistent savepoint first, in its own transaction before
any table is touched, records the id under the hop in
`recovery_savepoints`, then runs the migration. `rho debug rollback`
(daemon stopped) restores that savepoint and drops it; the store is
then at the old layout for an older build. `rho debug savepoints`
lists what the store holds. While a savepoint exists redb frees no
page it covers, so the file grows by what the migration rewrote; drop
it with the migration once the new build has run.

Proven on a copy (release build, 6 Sep): the migration rewrites 2824
agents and 1.11M rows in 16s and commits in 4s, 21s in all; rollback
restores the old format, heads and the exact table set in 60ms; the
layout proof then replays every agent unchanged from the rolled-back
store.

Found on the way: the live store holds ten persistent savepoints
(ids 11 to 29) left by migrations of older builds whose code was
removed without dropping them. Each pins every page freed since it
was taken, which is why a store with a few GB of rows is a 47 GB
file. `rho debug drop-stale-savepoints` (daemon stopped) drops the
ones no migration recorded; redb then reuses the pages, and
`Database::compact` would shrink the file if that is ever wanted.

## Landing order

1. Wire and daemon: `Live`/`Item` types, `remote.rs` gone, loop emits
   deltas, union live set, single feed, commands that load, LRU 100,
   head in memory. Client updated only enough to compile and pass.
2. Client: incremental transcript fold, derived attention, batched
   writes, digest version, render types moved.
3. Presentation into the loop.

Each step: fmt, workspace check, all suites, update
`AGENT-LOG-DESIGN.md`, `jj describe`.

## Accepted, not fixing

- Live for an agent goes to every client; fine at 4 active per client.
- Cold client pulls the whole journal once.
- Mirror rows are kept for every agent; nothing prunes.

## Store size and compaction (6 Sep)

The live store was 47 GB for 3.6 GB of stored rows (`agent_log` 3.58 GB,
every other table under 35 MB; `rho debug stats` prints this). The rest was
freed pages: ten persistent savepoints from older migrations (ids 11–29,
removed with their migration code in c792014 but never deleted) pinned every
old version of every table, so nothing freed was ever reused.

`rho debug drop-stale-savepoints` frees them (57–70 s on a copy, most of it
redb processing the freed tree). After that, `rho debug compact` should give
the space back, but redb 4.1 stalled at 13–30 GB with 5.4 GB allocated: the
region tracker's "has a free block of this order" bits were never cleared
for the large orders when small frees merged, so whole empty regions were
skipped and 8 MiB pages at the end of the file had nowhere to go. redb 4.2
fixes the free path; `vendor/redb` (4.2.0, see its RHO-FORK.md) adds a
fallback that checks the regions themselves when the tracker says none has
room, which repairs files 4.1 wrote. With that, a copy went from 30.1 GB to
5.44 GB in 11.9 s. `RhoDb::compact` also calls redb's `compact()` in a loop,
because redb trims one region tail per commit.

Crash recovery on the copy, release build: 13 s to open after a kill -9 with
the stale savepoints present, 5 s after they were dropped and the file
compacted (a mid-migration kill, then `rho debug rollback` in under a second
and the layout proof replaying all 2824 agents unchanged). Quick-repair mode
was considered and rejected: commits would pay for it every time.

Order for the live store, daemon stopped: `drop-stale-savepoints`, start the
daemon (it migrates, ~20 s, behind savepoint), verify, `forget-savepoints`,
`compact`. `rho debug savepoints` lists what is pinned at any point.

## After the first restart (6 Sep, evening)

Seen on the live store after the migration, both in the GUI, neither in
the daemon or the wire (the qlog showed both connections got the whole
catch-up in 20 s, then idled):

- A resync rebuilt the whole desk once per `Log` page (`sync_tree_dashboard`
  plus the dealer, ~2170 pages of 512 rows), and every steady-state row did
  the same, which is what made sending feel slow. Now one rebuild per frame
  per host, and none while the follow is short of the journal head `Ready`
  named. The rebuild itself is still a full walk of every agent, facts and
  node, three times over in places, with the editor torn down and recomposed;
  that is the next rethink, not a patch.
- A Claude loop that starts a new message within one request (its rows now
  carrying the last one) told nothing, so the client kept the old tail under
  the rows and showed the message twice. The teller now empties the tail
  (`Requesting`) whenever the pending response has fewer items than were told.

Standing rule for what comes after: work done per event, on either side, is
bounded by something like `O(log(agents × events))`. A row must not walk every
agent, every fact, or every node; dealing and the tree sync are where that is
still broken.
