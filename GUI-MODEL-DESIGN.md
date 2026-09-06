# The GUI's model layer: per-event work bounded, nothing on the main thread but drawing

Agreed 6 Sep after the performance report `gui-telemetry-1788696934561-0.json`
(captured 12:15:33, two minutes after a daemon restart). This document is
the destination for the GUI's model layer; `AGENT-LOG-DESIGN.md` and
`LIVE-TAIL-PLAN.md` remain the record of the log, the wire and the mirror.
Proofs on a copy of the store only; never touch the live DB.

## The problem, with numbers

The main thread was at 100% for six consecutive seconds. Every one of the
643 samples was inside `Workspace::handle_event`, handling the reconnect
catch-up one `Log` page at a time:

- 64%: `sync_tree_dashboard` → `Dashboard::sync_tree` → the vendored
  editor's `DisplayMap::disable_headers_for_buffers` → `BlockMap::sync`.
  The whole map rewritten, once per page. (Coalesced to one per frame after
  the head by 4eea6b06; still a full rewrite when it runs.)
- 28%: `AgentRegistry::rebuild` from `registry.tell`, once per page: a
  walk of every mirrored agent cloning its summary and rebuilding order,
  children and the tag index.
- The rest: dealer facts over every agent, desk `rebuild_view`, one Slack
  event sync.

The cause is not the volume of rows. It is that every handler does work
proportional to the whole world (all agents, all nodes, all cells, the
whole map text) instead of the thing that changed, and the wire delivers
thousands of things that changed. Cost = pages × world.

Also found while sizing: the on-disk mirror cursor is written from the
rows *kept* after filtering, so a page whose rows all fold to nothing
persists nothing, and every GUI start replays a longer tail than it
needs.

## The rule

- Per event (a `Log` page, a desk delta, a Slack event, a keystroke): work
  is O(rows in the event) plus O(log n) per index the rows touch.
- Per frame: work is O(what is drawn).
- Nothing walks all agents, all nodes or all cells except the load at
  startup and a host reset.
- The main thread never does the model's work. The connection, the
  decode, the fold and the mirror DB run on their own thread. The
  transcript view stays on the main thread: it is one agent's rows and the
  editor lives there.

## The layers

1. **Model task**, off the main thread. Owns the connection to each host,
   frame decode, the `MirroredAgent` fold, the `agent-mirror.redb` writer,
   the journal cursor and the catch-up. Speaks to the main thread in
   changes.
2. **Main-thread state.** An in-memory copy of every agent's identity and
   digest (a few MB at 20k agents, allowed so that search stays instant),
   three ordered indexes, the desk cells, the Slack facts, and one ranked
   set for dealing. Every one of them updated per change.
3. **Screens**, each holding the rows it draws: map, Home, Find,
   transcript, draft. A change updates the rows it touches. **Window**
   state (focus, which screen is up, key context, echo, shift tap,
   transients) is not about agents and holds none.

Flow is one direction: model → state → screens. Commands go back the same
road: a screen asks, the state applies optimistically where it already
does (desk `pending`), the model sends.

## The model task

Messages to the main thread:

```
Loaded  { host, agents: Vec<AgentSnapshot> }             // once, after load or host reset
Changed { host, agents: Vec<(AgentId, AgentSnapshot)> }  // per page, the agents it touched
Rows    { agent_id, rows: Vec<(AgentPos, MirrorEvent)> } // for followed agents only
Live    { agent_id, delta }                              // the tail, as today
```

`AgentSnapshot` is identity + digest, what the mirror already stores per
agent. Nothing else crosses: no summaries, no block lists, no states.

- Catch-up is silent. Pages are applied and written until `seq` reaches
  the head `Ready` named; then one `Changed` naming the agents that moved
  during it (a `Loaded` only when the copy started over). The main thread
  does not hear a page.
- The cursor invariant: the stored `seq` means "this client has seen the
  journal through here", never "kept a row at here". It is written for
  every page, including one that folds to no rows, and it is written by
  whatever consumed the page.
- `Rows` go only to agents the main thread follows (open transcript,
  active set). Everything else is a digest change.
- Desk messages (`DeskSynced`, `DeskCellsAvailable`, mutations, text ops)
  are forwarded to the main thread as today: the desk cells stay there
  because their row buffers are gpui entities. Applying them is made
  incremental below.
- The task is a std thread with a channel in each direction, not a gpui
  background task: it must not compete with rendering and must survive
  the window.

## Main-thread state

`Agents` replaces `AgentRegistry`:

```
agents:    BTreeMap<AgentId, AgentSnapshot>
by_host:   BTreeSet<(HostId, Reverse<UnixMs>, AgentId)>   // most recent first per host
by_parent: BTreeSet<(AgentId, AgentId)>                   // parent, child
by_name:   BTreeSet<(String, AgentId)>                    // title and id label, for Find and @targets
filing:    BTreeMap<AgentId, (bool, Vec<String>)>          // the user's, from the store
verdicts:  BTreeMap<AgentId, Verdict>                     // the user's, from the mirror
```

A `Changed` for one agent removes its old index keys and inserts the new
ones: O(log n). `Loaded` builds all of it once. There is no `rebuild`, no
`summaries`, no `order`, no `AgentSummary`, no `AgentFacts`, no
`touch_agent`, no `set_activity`, no `deal_count_revision`. Readers take
`agents.get(id)` and read fields. Selection, the active pane and which
agents are live move to the window.

The ranked set replaces the dealer's walks:

```
ranked:  BTreeSet<(Reverse<Priority>, CardId)>
card_of: HashMap<CardId, Priority>
```

A change to an agent, a cell or a Slack unit recomputes that one card's
priority (the same `card_facts`, for one card) and replaces its key. Home
is the first N of `ranked`; the lamp is the first. A priority that
depends on time (a snooze ending, an age band) is refreshed by a timer set
to the next expiry, never by a per-frame or per-event re-rank. The dealer
signal task and its revision polling go.

## Screens

- **Map.** Keeps `node → row buffer`. A `Changed` for an agent rewrites the
  derived title of its node's buffer (one `write_derived_title`). A
  structural change (new node, reparent, removal) inserts, moves or
  removes one excerpt. `sync_tree` from scratch runs at `Loaded` and host
  reset only. `refresh_desk_sources` hands the store the source facts of
  the changed agents, not of all of them.
- **Home.** Draws the first N of `ranked`; re-renders when one of those N
  changes.
- **Find.** A prefix range over `by_name` plus the digest text of the
  agents in range; bounded by the results shown.
- **Transcript.** `Rows` append to the open fold and to the editor by the
  `Appended` path. The whole block list is never handed again;
  `TranscriptFrame::Fold(state)` goes with `refold_open_transcripts`.
- **Draft targets.** `by_name`.

## The desk store

`synced` applies the delta's cells to `confirmed` and to `view` in place,
O(cells in the delta); the pending mutations are replayed only when one is
rejected. `rebuild_view` goes. A new node gets its row buffer when it
appears, not on a reconcile walk. `DeskSynced` and `DeskMutationAccepted`
hand the map the node ids they touched, the way `Changed` hands it agent
ids; neither calls `sync_tree_dashboard`.

## Startup and reconnect

- Startup: the model task loads the digests from `agent-mirror.redb` and
  sends `Loaded` per host; the screens build once. Offline Home waits for
  it, as decided in AGENT-LOG-DESIGN slice E.
- Reconnect: the catch-up is silent, then one `Changed`. The screens
  update the rows it names.

## Slices, in landing order

Each slice lands on its own with the gate green and a profile on the rig
(`/tmp/rho-rig`, the 45 GB copy) showing the per-event cost it claims.
Each gets a landing note here.

1. **The model task and the change channel.** Connection, decode, fold and
   mirror writes move to the model thread; the main thread receives
   `Loaded`, `Changed`, `Rows`, `Live`. The screens still take the full
   list (the cascade stays), but it runs once per `Changed` and never
   during catch-up. The cursor invariant is fixed here. Proof: reconnect
   on the rig, main-thread samples during catch-up ≈ 0.
2. **`Agents` replaces the registry.** Map plus indexes, per-change
   O(log n); the deletions listed above; selection into the window.
   Proof: a `Changed` of one agent costs one index update, no walk.
3. **Map per-row updates.** `sync_tree` only at `Loaded` and reset;
   `refresh_desk_sources` per changed agent. Proof: one row changed, one
   buffer written, no display-map resync.
4. **The ranked set.** Home and the lamp read it; `dealer_hand`,
   `tree_dealer_queue`, the dealer signal task and the revision go;
   time-based priorities on a timer. Proof: a change re-ranks one card.
5. **Desk deltas in place.** `rebuild_view` goes; `DeskSynced` and
   `DeskMutationAccepted` touch the nodes they name. Proof: one verdict
   costs the cells it writes.
6. **Transcript rows append.** `refold_open_transcripts` hands deltas.
   Proof: a page for the open agent costs its rows.
7. **The window split.** `Workspace` (10,685 lines, 106 fields) becomes
   window state plus screens as entities. Its own document when reached.

`STORE-DESIGN.md` slice 5 (the client keeps the store) is unchanged and
queued after slice 5 here; it fits the desk store as described.

## Symptoms of the wrong shape

- A `for agent in all` or `for node in all` inside a message handler.
- `rebuild`, `sync` or `refresh` called from an event arm.
- A revision counter polled per frame.
- A screen reading another screen's state, or the registry.
- A whole state handed where a delta would do (`Fold(state)`,
  `set_tree_source`).
- A `&mut Workspace` inside the model task.

## Not in this document

Raw-log purity (`at` on raw events), the daemon's own per-event bounds
(LIVE-TAIL-PLAN), and the window split's detail.
