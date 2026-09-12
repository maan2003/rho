# The live tail, and the rest of the dumbing-down

Agreed 6 Sep after slice F landed (jj change `nzkrskxuxxmr`). This is
the destination; `AGENT-LOG-DESIGN.md` is the record of what landed
and why. Proofs on a `cp` of the store only; never touch the live DB.

## Principles

- The log is the truth. Anything derivable from rows is derived on
  the client, never carried on the wire a second time.
- Nothing on the wire is named `Ui*`. `rho-ui-proto` carries facts;
  render types live in `rho-agents::state` (they were `rho-registry`'s
  `render` until the map cut absorbed that crate).
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

Derived on the client from rows, never on the wire: the native
runtime's queue (`Message` rows not covered by a later
`Sent`/`QueueCleared`), running tools (calls in `Replied` without a
result in `Sent`), errors and cancels (`Turn Ended(..)`), turn running,
`context_used` (last `Replied` usage). A Claude agent's queue is not
rows at all (7 Sep): `Live::Queued { items }`, whole, whenever it
changes.

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
  from `rho-ui-proto::remote` to `rho-agents::state` as the render model,
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
  attention table is retired. `rho_agents::attention` is the one
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

The previous Rho loop wrote five variants the runtime no longer writes.
The migration `b1e40c93 -> 50351c18` rewrote them into the current
vocabulary (`Accepted`, `Sent`, `Replied`, `Presented`) by the old
replay's rules; a proof on a copy replayed every agent both ways and
held history, owed calls, context use and the queues equal (2824
agents, 1,054,194 rows translated). It ran on the live store on 6 Sep
and was removed with the old types in the landing after; nothing in
the tree reads or names the old world now.

## Rolling back (6 Sep)

`rho_agent::db::prepare` opens the store: when a migration is due it
takes a redb persistent savepoint first, in its own transaction before
any table is touched, records the id under the hop in
`recovery_savepoints`, then runs the migration. `rho debug rollback`
(daemon stopped) restores that savepoint and drops it; the store is
then at the old layout for an older build. `rho debug savepoints`
lists what the store holds. While a savepoint exists redb frees no
page it covers, so the file grows by what the migration rewrote;
`rho debug forget-savepoints` drops it once the new build is verified.
This stays for every migration to come; the 6 Sep one is gone.

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

## The Claude rows come from the stream (7 Sep)

Claude Code's session file (`~/.claude/projects/<cwd>/<session>.jsonl`)
is the Claude runtime's history. Before 6 Sep the loop wrote rows from
the stream (`ClaudePresentationSource` for text, `Replied` for calls,
`Sent` for results) and, on load, read the whole file and matched it
against those rows by uuid, speaker and a text prefix: two copies of one
truth, reconciled by heuristic. On 6 Sep (night) the file became the
only source of the rows, copied behind a cursor, with the stream as the
live tail and the bell to read the file. Measured on 7 Sep: Claude Code
writes a message's lines in one batch after the message and its tool
results, 90–340 ms after the stream told them and after `result` itself
(not the kernel or the runtime: every Bun write shows to another process
within 5 ms). A row from the file could only follow the stream, and
Rho's own rows (a turn end, a want, a delivery) could be ordered after
it only by waiting; the waits stalled the loop and mostly missed.

Now the stream is the source of the rows, and the file is read only
where Claude is handed a session to fork:

- **One row per finished block, from the stream.** Each `assistant`
  event is one content block with the message's id, usage, uuid and
  time; `assistant_row` makes `AgentEvent::Transcript { uuid, line, at }`
  of it, with `TranscriptLine::{User, Assistant, ToolResults,
  Compacted}` as before and the uuid the file gives the same line (a
  rewind forks the session there). Usage rides on the first block of a
  message only. A `user` event is the echo of a send (`Delivered` first,
  then the `User` row) or a call's results (`ToolResults`); a
  `compact_boundary` is `Compacted`. A subagent's blocks, synthetic
  messages, thinking-only blocks and Claude's command echoes make no
  row. `strip` tells the rows in the words a reader already knows
  (`ClaudeMessage`, `Replied`, and `Results`, which unlike `Sent` carry
  nothing out of the queue), so the client never learns which runtime it
  is looking at.
- **Row, then tail, from one task.** The block's row is appended and
  committed, then its streamed copy leaves the tail and the tail is told
  again without it (the teller empties a shorter tail on the client and
  says the rest). At `message_stop` whatever the tail still holds goes.
  Nothing waits on anything: the stream is the order things happened.
- **The queue is live, never rows.** Claude Code holds a Claude agent's
  queued messages in its process and nothing persists them: a restart
  loses them. So the loop writes no `Accepted` or `QueueCleared` row and
  tells `Live::Queued { items }` whole whenever its queue changes (the
  teller says it once per change and again for a joiner); the registry
  keeps it in the tail, after the streamed items. The echo of a send
  (its uuid) is when a message leaves the queue, and the echo's row is
  the message in the conversation. Older Claude logs were the copier's:
  the file's lines with `Accepted` and `QueueCleared` rows between
  them, some never confirmed by an echo. A one-off at daemon start
  (`rho_agent::rebuild`, 7 Sep), before any loop can append, rewinds
  each such log to its first conversation row and appends the file's
  active branch again in the file's order through the stream's
  projection: rows only, no queue, what the stream would have told.
  The client's mirror takes the `Rewound` and the rows like any other;
  its fold hides what the rewind took back. A log with no file keeps
  its rows and only a queue left open gets a `Cleared`.
- **Rho's own facts stay Rho's rows.** `Failed`, `Turn`, `Wants`,
  `Presented`, `Rewound`.
- **What the file is for.** A rewind hands Claude the uuid to fork at
  and reads the fork once to confirm it materialised. Sessions Rho did
  not run are not imported, and lines written while the daemon was not
  listening are not recovered: the log is what Rho witnessed. Rows from
  the one night of copying keep the file's `offset` in the store; the
  decoder skips it.

What is gone: the copier, `claude_transcript_cursors` (left in older
stores, never opened), `TranscriptTail`, the directory watch, the waits,
the reconciliation and its prefix matching, the in-memory `ContextBlock`
copy of the transcript, `Sent` and `Replied` writes from the Claude
loop, the whole-file read on every load. The one-off backfill of 7 Sep
(`backfill_claude_transcripts`) put one `Rewound` over the rows written
before 6 Sep and copied each file once; it ran and was removed.
`ClaudePresentationSource` stays in the enum, read-only, so older logs
still fold; dropping it needs a log-rewriting format hop, since the rows
stay in the log behind their `Rewound`.

Not done here: the user echo is still matched to its queued copy by text
on the client (`TranscriptFold`), the way it always was; the `Message`
wire event has no uuid yet. Tool-result bodies are stored whole in the
row rather than read back from the file at `offset` on demand; `offset`
is recorded so that can change without a migration.
