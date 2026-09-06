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
  the window. The boundary is at the fold, not at the socket: each host's
  IO already runs as a tokio task, and the model thread sits between the
  connection's event channel and the main thread (b8os, 6 Sep). The
  mirror writer thread that exists today is owned by the model thread.
  The model thread sends `Follow { since }` itself, from its own cursor,
  and decides "the copy started over" (seed mismatch, seq past the
  head), which it says with `Loaded` instead of `Changed`.
- The model is a plain struct, `ingest(host, ConnEvent) -> Vec<ModelMsg>`,
  and the thread is a loop around it. Tests call `ingest` inline and hand
  the messages to the workspace, so they stay synchronous with no
  test-only path through the product code.

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
(a daemon on `/tmp/rho-rig`, the 45 GB store copy, so the journal and the
agent count are the user's; the GUI in the isolated `/tmp/rho-slack-ux`
environment) showing the per-event cost it claims.
Each gets a landing note here.

The order is 1, 3, 4, 2. Slice 3 and slice 4 come before slice 2 because
a profile said so, twice. On the rig, 89% of the main thread was in
`sync_tree_dashboard` and the registry was 19 samples, so the registry
that slice 2 replaces is not what costs. A snapshot the user then took on
their own machine
(`gui-telemetry-1788709749665-0.json`, release aarch64, 9,744 rows) put a
number on it: a 6,350ms frozen frame, and every sample through it in
`ensure_headerless` calling `disable_headers_for_buffers` one buffer at a
time, 69% of the main thread. That is slice 3. Once it is gone, what is
left is `tree_dealer_queue` at 16%, quadratic for its own reason: per note
heading it walks every node again to find children and reads every note
buffer's text to build titles, on every desk sync. That is slice 4. Slice
2 is still worth doing, and it is still cheap to do after both.

1. **The model task and the change channel.** Connection, decode, fold and
   mirror writes move to the model thread; the main thread receives
   `Loaded`, `Changed`, `Rows`, `Live`. The screens still take the full
   list (the cascade stays), but it runs once per `Changed` and never
   during catch-up. The cursor invariant is fixed here. Proof: reconnect
   on the rig, main-thread samples during catch-up ≈ 0.

   *Landed.* `rho-gui/src/model.rs`: the fold, the journal cursor, the
   catch-up gate and the `agent-mirror.redb` writes run on a std thread
   named `rho-model`, with a channel each way. The socket stayed on the
   tokio runtime, as agreed: the boundary is the fold. The main thread
   receives `Loaded`, `Changed`, `Rows` and everything else forwarded
   unchanged; `Workspace::handle_event`'s `Log` arm is gone, and so are
   `mirror_hosts`, `MirrorCursor`, `journal_cursor` and `restore_mirror`.
   `AgentRegistry::told` takes folded agents where `tell` took rows;
   `tell` is now what the registry's own tests fold with.
   The cursor invariant is fixed: `mirror::write_log` takes the page's
   `seq` beside its rows and `apply` writes `StoredHost` for every page,
   including one whose rows all filtered out.
   `Model::ingest` is a plain function, so the tests drive it inline
   (`tests/story.rs::feed`) and stay in one thread.
   `Live` follows `Rows`: a tail for an agent no screen reads is dropped in
   the model, O(1), and never reaches the channel. Without that a connect
   cost one dashboard rebuild per agent: measured on the 2823-agent store,
   134s of block-map rebuilds on the main thread with the journal already
   caught up.
2. **`Agents` replaces the registry.** Map plus indexes, per-change
   O(log n); the deletions listed above; selection into the window.
   Proof: a `Changed` of one agent costs one index update, no walk.
3. **Map per-row updates.** `sync_tree` only at `Loaded` and reset;
   `refresh_desk_sources` per changed agent. Proof: one row changed, one
   buffer written, no display-map resync.

   *Landed.* Two loops were quadratic and both are gone.
   `Dashboard::ensure_headerless` disabled headers one buffer at a time and
   each call resynced the display map, so a build of n buffers was n
   resyncs of n rows; it now hands the editor every new buffer in one call.
   `AgentRegistry::set_agent_filing` rebuilt the whole registry once per
   agent, and the desk files them all at once; `set_agent_filings` takes
   the lot, rebuilds once, and says whether any filing moved so that what
   is derived from filing is made again only when it did.
   On top of that the scope is carried: `Workspace::schedule_desk_sync`
   takes the agents a `Changed` moved (`None` asks for the whole desk, and
   a whole one swallows the scopes it merges with), `sync_tree_rows` passes
   them to `refresh_desk_sources`, and that splices the named agents into
   the held source list through `agent_source` instead of rebuilding it
   from `known_agents`. `Loaded` and a host reset are what still ask for
   the whole desk.
   Measured on the rig (2,823 agents, debug build, llvmpipe), the warm-start
   `Loaded` frame gap went 163.4s -> 35.8s with the headerless batch ->
   4.50s with the filing batch. Editor stages across the whole gap are now
   0.157s of 4.50s: 2,073 block-map syncs, largest 0.337ms, largest row
   move 2 -> 10, where the same gap used to hold 126.4s of block-map sync
   and one 259ms sync of 1 -> 1,956 rows. The rest of the 4.50s is not the
   display map, and the trace on the user's own machine says what it is:
   slice 4.
   `one_agents_change_costs_no_display_map_resync` reads the editor's own
   timing ring and asserts the change side: a `Changed` for one agent costs
   zero block-map syncs over zero rows.
   Not done here, deliberately: `sync_tree`'s per-row decoration pass still
   builds inlays, end-of-line hints and highlights for every row rather
   than for the rows that moved. It costs about six display-map syncs per
   row, but over the rows actually composed, which is around ten on this
   store, so it does not show; splitting it means touching the anchor and
   fold code for no measured gain. It belongs with slice 4, which walks the
   same nodes.
   The passive profiler was fixed alongside, because it hid exactly this
   kind of stall. `CpuProfiler::snapshot_segments` took only sealed
   segments, and a segment is sealed by write activity, so a freeze left
   its own samples in the active file and the snapshot threw them away: in
   `gui-telemetry-1788709749665-0.json` that was the newest 3.17s, which
   swallowed a whole 2.17s stall. The unsealed tail now comes through,
   marked by `cpu_profile.tail_unsealed`, and `maximum_tail_gap_ms` reads 0
   when it is there; truncation costs nothing because the decoder stops at
   the last whole frame. The rolling window went from 10s to about 32s
   (`CPU_SNAPSHOT_SEGMENTS` 5 -> 16, the trace budget 2 MB -> 8 MB), so a
   six-second freeze no longer fills the history with itself. The snapshot
   schema is version 10.
4. **The ranked set.** Home and the lamp read it; `dealer_hand`,
   `tree_dealer_queue` and the revision go; time-based priorities on a
   timer. Proof: a change re-ranks one card.
   *Landed, first half.* The dealer no longer walks the desk to answer a
   question about it. Each host's map carries an index built once when the
   source is set (id, children by parent, node by agent), which retires the
   walk of every node per heading, the search for an agent's node per
   agent, and the pass over every agent per agent in the spawn descent.
   Note titles come from the desk rather than from ropes: `HostDeskCells`
   keeps one per note and rereads only the buffers whose version moved, so
   the ten `note_title(&buffer.read(cx).text())` calls are gone from every
   sync path. Only the pickers read a buffer now, and only for derived
   rows; `tree_dealer_queue` and `dealer_hand` no longer take a `cx` at
   all, which is what makes that visible. The ranking is computed once and
   shared by Home, the lamp and the map's depth counter, which built it
   three times a frame; the map's copy was built with an empty interactions
   map, so the depth counter and the dealer were ranking differently, and
   they now agree. `deal_count_revision` is gone: field, fifteen bumps and
   accessor, with no reader outside its own tests. `sync_tree` writes a
   derived title only into the rows whose title moved.
   The dealer signal task survives, and only as the expiry timer: its wake
   is keyed to the soonest future `defer_until`, deadline or skip cooldown,
   floored at a second and ceilinged at a minute. The ceiling stays because
   a card's priority slides continuously with waiting, which no expiry
   names.
   Measured on the rig (49 GB store, debug, same journal position as the
   slice 3.5 run): the warm start's Loaded gap went from 4.65s to 4.27s,
   which is 8%. The dealer itself collapsed, from about 9% of main-thread
   samples to about 2%: `tree_dealer_queue` 56 -> 13,
   `dealer_hand` 27 -> 13, `evaluate_dealer_signals` 29 -> 5,
   `refresh_home` 15 -> 1, out of 626 and 597 samples.
   What did not move is the rest of the gap, and it is not the dealer's:
   `on_buffer_event` 144 -> 142, `colorize_brackets` 95 -> 86,
   `splice_inlays` 88 -> 96, `Composition::sync` 86 -> 87,
   `set_excerpts_for_path` 85 -> 86, `ensure_headerless` 26 -> 26. Every
   stack holding `on_buffer_event` has no rho-gui frame under it but the
   workspace closure: it is gpui dispatching the editor's own subscription
   to the multibuffer, deferred to the end of the update, raised by
   `sync_tree` replacing the whole composition. That residual is slices 5
   and 6, not this one. The decoration pass folded in here removed a rope
   read per machine row per sync but not the writes, because during a warm
   load the attention glyphs and activity text do change every sync.
   *Landed, second half.* The ranking is kept rather than made again.
   `DealerSet` holds the cards by the topic they are about, with an index
   from each host and each agent to what it owns, and a card is made only
   when the thing it is about changes: a `Changed` remakes exactly the
   agents it names, and a desk that arrived or changed shape remakes that
   host. `tree_dealer_queue`, the last pass over every node, is gone.
   The part of a card that moves with the clock is separated from the card
   as `PriorityCurve`, so a read brings a kept card up to the moment
   without making it again: a dated mark carries its mark, time and pace, an
   agent reply carries the agent it is about and reads its facts off the
   cursor already stored beside it, and a Slack unit carries nothing
   because its wait is the mirror's measurement, not the clock's. A card
   the curve takes under the floor leaves at the read.
   `dealer_hand` no longer takes a registry or the Slack facts, because a
   read consults neither; that is what makes it visible that reading the
   ranking cannot walk anything. A card is remade from the model event arm
   that carries the fact, not from the frame-deferred desk sync, because a
   fact moving is exactly when a card is made and every reader in between
   has to see it.
   Two curves do not key into bands, and neither needed to: the agent
   recency bonus and the pace of a not-yet-due deadline are both applied at
   read, along with every other card's slide, since Home renders the whole
   ranking and so a read is bounded by the answer's own size either way.
   The bounded thing is the making, and that is now what a change names.
   That read happens on a change or on the expiry timer and never on a
   frame: Home holds the rows it was handed and its render only draws
   them, and the three readers (`refresh_home`, the signal evaluation, and
   a pull) all hang off an event or a keypress. Top N and the two explicit
   sets, the recency window and the not-yet-due deadlines, layer on the
   day a screen stops rendering the whole hand; until then they would key
   nothing that is not already bounded.
   The sort gained the card's identity as a last tie-break: a set has no
   insertion order to fall back on, and two cards that tie on everything
   else must still come out the same way every read.
   Proof: `one_agents_change_makes_one_card` builds a desk of eight filed
   agents, each asking, and asserts that the build makes a card per asking
   agent and that a `Changed` for one of them makes exactly one more.
   On the rig, a warm start of the 49 GB store: the whole dealer is now
   3 samples of 417 in the busy block, all of them the one build at
   `Loaded`, against 26 for the two walks in the first half and about 9%
   of main before the slice. The block itself is 4.17 s against 4.27 s and
   4.65 s, so the second half buys almost nothing on the clock and is not
   meant to; the first half had already taken the dealer down, and what
   this half buys is that no read and no change walks anything. The rest
   of the block is where the first half left it, and is not the dealer's:
   `sync_tree` 234 samples, the editor's own buffer subscription 145,
   `splice_inlays` 91, `Composition` 88. That is the composition rebuild,
   slices 5 and 6.
5. **Desk deltas in place.** `rebuild_view` goes; `DeskSynced` and
   `DeskMutationAccepted` touch the nodes they name. Proof: one verdict
   costs the cells it writes.

   *Landed.* The map is kept rather than made. The desk cells hold the
   ordered nodes and where each id is drawn, and a delta says two things:
   the ids it named, and whether the shape can have moved. Only four
   properties move a shape — where a row is filed, what labels it carries,
   whether it exists and how old it is — so a verdict never does, and a
   delta that keeps the shape patches the rows it named and walks nothing.
   `synced` merges the delta into the confirmed store and the view and
   stops there: the replay was a walk that answered a question the merge
   had already answered, and it survives only on the rejection path, which
   is the one case that takes a write out of the middle. `reconcile_buffers`
   likewise survives only where the shape moved; a row a delta brings gets
   its buffer from the delta, which is what "a new node gets its buffer
   when it appears" means. `DeskMutationAccepted` now does nothing to the
   map at all: the cells went in when this client wrote them, and the
   daemon agreeing is not news the map has to be told.
   On the drawing side the dashboard keeps what each row was drawn as —
   where it sits, the marker in front of it, the hint after it — so a
   redraw is of that row. A verdict composes nothing: the excerpts, the
   folds, the highlight ranges and the row depths are the same rows in the
   same order. What moves is the hint at the end of the row, the marker in
   front of an agent row when its attention moves, and the derived title of
   a machine row. The cards move with it: `DealScope::Nodes` makes the
   cards of the rows a delta named, and of the agents those rows lend a
   place to, which the dealer set now indexes by heading so that finding
   them is not a pass over the hand.
   One thing this slice does not take, and it is the editor's shape rather
   than the map's: `highlight_text` and `set_eol_hints` take their whole
   set at once, and `HighlightKey` is a closed enum, so a row's hint cannot
   be replaced without handing back the hints of the rows that did not
   move. Inlays are already incremental and are spliced per row. So one
   verdict costs its own row plus a pass that clones the kept hints; no
   anchor is computed, no snapshot is scanned, nothing is composed, and no
   buffer is read. Making that last pass a delta needs an incremental
   decoration API in the editor, which is not this document's to write.
   Proof: `one_verdict_costs_its_own_row` builds a desk of seventeen rows,
   marks one note done, and asserts that the map is composed no further
   times and exactly one row is drawn again.
   What this does to the Loaded gap: nothing, and it was not going to. On
   the rig the busy block is 411 samples against slice 4's 417 and 427,
   which is noise — a `Loaded` is still one build, and the one build is
   still `sync_tree` at 232 samples of 411, the buffer subscription at 143,
   `splice_inlays` at 91 and `Composition` at 89. What did leave the warm
   start is the walking either side of it: `rebuild_view` is gone from the
   trace entirely, `reconcile_buffers` is down to 2 samples and the map is
   built 4. The gap this slice moves is the one no warm start shows, the
   verdict, and the test is what says so.
   `DeskTextApplied` still composes. A body edit from another device does
   not move a row's place, but it does move the words a card's breadcrumb
   is made of, and following that through is slice 6's work with the
   transcript rows rather than a fifth path bolted on here.
6. **Transcript rows append.** `refold_open_transcripts` hands deltas.
   Proof: a page for the open agent costs its rows.

   *Landed.* `TranscriptFold` remembers the lowest index of the composed
   transcript that has moved since a delta was last taken, and hands that
   index with the blocks from there on. Everything before it is the same
   pointer it already was, so the reader replaces a suffix. The store
   applies the suffix in place and composes from the same index, and the
   summary it answers with is that index rather than the result of
   comparing two block lists. A transcript is handed whole exactly once,
   when the reader opens an agent and there was nothing to append to.
   Before this, one row of an open agent's mirror cost the whole
   transcript three times over: the fold cloned every block into a state,
   the store cloned it again to compose, and the summary walked the shared
   prefix to find out that only the end had moved.
   `DeskTextApplied` follows the map's delta path now. A body edit from
   another device moves that note's words and the breadcrumbs made of
   them, which is its subtree and nothing outside it; where the rows sit
   does not move, so nothing is composed.
   Proof: `a_page_for_the_open_agent_costs_its_rows` folds sixty-four
   messages, takes the whole transcript once, appends one row, and asserts
   the delta starts where the transcript already ended, carries one block,
   and that the store renders from there rather than from the top.
   Fixed in passing: slice 5's test was inserted above
   `one_agents_change_makes_one_card` and took its doc comment with it.
   Both tests have their own again.
7. **The window split.** Replaced by `GUI-CRATES-DESIGN.md` (6 Sep): the
   GUI becomes vertical crates by source, and the window keeps only
   window state.

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

The daemon tells every agent's tail at connect (`pool.tell_tails`, then one
`Live` per agent). Once the client drops the tails it does not read, that is
O(agents) on the wire for nothing: on a store of 2823 agents it is 2823
messages a client throws away. It belongs to LIVE-TAIL-PLAN's live set, where
a tail is told only for agents some client holds. Daemon-side, later, and it
needs a daemon restart to take effect.
