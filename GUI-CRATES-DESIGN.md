# The GUI as vertical crates: one per source, owning connection to screen

Agreed 6 Sep. This replaces slice 7 of `GUI-MODEL-DESIGN.md` (the window
split) and is where the usability work happens. `GUI-MODEL-DESIGN.md`
stays the record of the model layer and its rule (per-event work O(rows)
plus O(log n); per-frame work O(what is drawn); nothing on the main thread
but drawing). Proofs on a copy of the store only; never touch the live DB.

## Why

Usability broke for three reasons that no slice fixes:

1. The rules have no spec and no visibility. Dealing priority, Find's
   ranking and Slack's card rule were accreted one fix at a time; nothing
   tells the user why a card is in front of them, so a bug and a rule look
   the same.
2. QA is not the user's world. Tests run on a seeded rig with a handful of
   agents, driven by scripted keys and judged from screenshots. The user's
   desk has ~10k rows, ~2,800 agents and a flooded Slack mirror.
3. One object holds everything. `Workspace` is 10,685 lines and 106
   fields; dealing, Slack, Find, creation, the map, undo and telemetry
   reach into each other, so every fix touches it and only one engineer
   can work in it safely.

The boundary that lets engineers work apart is not a function boundary.
It is vertical: one crate per source, owning everything from the
connection to the screen, tested alone against its own fake server.

## It feels like Emacs

Ruling, 6 Sep, over every crate below. Rho is an editor the way Emacs is
one, and every screen a source crate builds obeys that:

- Every screen is a buffer. Agents, the map, a Slack conversation, a
  thread, Find's results, a draft: text in an editor, drawn with the
  editor primitives (buffers, inlays, the composition), with the point
  in it. The user moves through it, searches it, selects and copies from
  it, as in any buffer. There is no widget tree beside the editor; when
  a screen needs a primitive the editor lacks, the primitive is built
  from the ground up and the screen stays a buffer.
- Keys do everything, and a key means one thing per context. Each buffer
  kind has its key context; the same key does the same kind of thing in
  every buffer (open, act, back, next, previous). Nothing needs the
  mouse.
- The minibuffer asks and the echo line answers. A question to the user
  (Find, a name, a confirmation) is asked in the minibuffer; what just
  happened is said in the echo line, in words, never in a modal.
- Actions with choices are transients. A verdict, a filing, a reply with
  options: a transient shows the keys and their meanings, as Magit does,
  and goes away.
- Surfaces have a history. Back is always a key away and returns to the
  buffer as it was, point included.

`rho-window` owns these primitives; source crates use them and add no
others. A design that reaches for a different UI model for one screen is
wrong at the design, not at the polish.

## The crates

- **`rho-hosts`.** The daemon connection per host and its handshake,
  reachability (one status for the status line), the command channel and
  the event fan-out by kind, workdir labels, quota. Every crate that
  reaches a machine (agents, the DAG sync, file, diff, shell and terminal
  surfaces) goes through it; it owns no agent state. Cut out of
  `rho-agents` by eng-b8os as the first step of the move.

  *Landed.* `connection.rs`, `hosts.rs` and `realtime_client.rs` moved out
  of `rho-gui` whole, with `HostId`, `AttachTarget`, `HostSpec`, `HostPath`
  and the workdir and quota state that goes with them. The base crate names
  nothing above it: where the connection used to send onto the model
  thread's queue by name, it now sends through a `HostSink` trait the shell
  implements, and `attach` hands back the command channel rather than
  posting a `ModelCommand` itself. The two event kinds that existed only
  for tests went with that rule: `ConnEvent::Transcript` named a
  `rho-registry` type and is gone (a test seeds a transcript through the
  workspace instead), and `ConnEvent::Many` and the sent-command recorder
  sit behind a `test-support` feature. `rho-registry` re-exports
  `rho_hosts::HostId` rather than defining its own. Gate green: rho-gui 277,
  rho-hosts 14, rho-registry 14.
- **`rho-agents`.** The model thread (`Model::ingest`), the agent mirror
  on disk, the agents map and its indexes (`rho-registry` is now part of
  this crate), the transcript, creation, Find over agents, and the
  agent screens. Tested end to end against a fake daemon. Selection and
  the active pane are not agent state and go to `rho-window`. Owner:
  eng-b8os.

  *Landed, the transcript (5).* `Transcripts` owns what a transcript is:
  the fold of an agent's mirror, the runtime's live tail, and the rendered
  state a screen draws. The workspace held two fields (`store` and
  `open_mirrors`) and four functions that walked between them; it now holds
  one field and says which agent. Which agents are open is answered here
  too, because it is the same question as which agents this client asks the
  model thread for rows about. What crosses out is a state and a summary of
  what changed — nothing above knows there is a fold underneath. Liveness
  stayed in the shell: it is the map's fact about an agent, not the
  transcript's. The mirror on disk (3) is still read by the workspace and
  the events handed in, so this cut does not drag the model thread with it;
  it follows with (4). Three tests run the crate alone. Gate green: rho-gui
  277, rho-agents 3, rho-hosts 14, rho-registry 14.

  *Landed, creation (6).* What a draft means is the crate's: the start
  modes and the base they stand for, the role names and the cycle through
  them, workdir resolution, and `parse_start` — the one place that decides
  which host a new agent lands on and refuses, in the reader's words, every
  way the four answers can fail to make one. The screen keeps its buffers
  and its labels and re-exports the vocabulary rather than defining it. The
  map's half of a base (which host an agent label is on, and the workspace
  it works in) is looked up by the shell and handed in as `StartBase`, so
  this cut does not pull the map in early; it goes when the map does. Seven
  tests run the crate alone, including the two-host refusal and the default
  base, which had no test before. Gate green: rho-gui 276, rho-agents 7,
  rho-hosts 14, rho-registry 14.

  *Landed, Find (7).* Which names an agent answers to is the crate's:
  `find::hit` gives the title a row shows, the names it also answers to
  (its label, and the last thing the user said to it when that is not the
  title already), and how recently it was used. Where an agent sits in the
  tree stays with the row that draws it, and the scorer and the prompt stay
  one thing across sources — Find is not per-source, only its answers are.
  A hit is its own type, as ruled: it answers a query where a card claims
  attention. The reason type is deferred with dealing composition, and the
  doc comment on `AgentHit` says so, so a hit gains one the day a card's
  reason becomes a type. Three tests run the crate alone, over a registry
  told an agent the way the model thread tells it one. Gate green: rho-gui
  276, rho-agents 10, rho-hosts 14, rho-registry 14.

  *Landed, the screens (8).* The agent screen and the transcript under it
  are the crate's: `agent_view` (the screen — the transcript multibuffer
  with a prompt buffer under it, the attachments, the status), `transcript`
  (the incremental composition: per-turn excerpts, the highlights, inlays
  and display elisions reconciled against every attached editor) and
  `render` (the pure projection from a block to styled spans, and the
  elision plans over it). `transcript.rs` from cut (5) became
  `transcript/store.rs` and is re-exported, so a transcript's model and the
  buffer that shows it are one module, as they read.

  Every screen here is a buffer and stayed one: the transcript is a
  multibuffer of per-turn excerpts with the point in it, the prompt is a
  buffer in the same multibuffer, the fold state is display elisions, the
  running-tool lines are inlays, and the status is an editor right prompt
  anchored at the prompt's end rather than a strip drawn beside it. Nothing
  in the move needed a widget tree. One thing to say rather than carry
  quietly: that right prompt is a primitive of the *vendored editor*, not
  of `rho-window` — the screen reaches it through `editor`, so today the
  window does not own every primitive its screens draw with.

  Two pieces went to `rho-window` rather than to `rho-agents`, because two
  source crates need them and a source crate must not name another:
  `markdown` (the Markdown grammars and what a Markdown buffer is
  configured as — the transcript uses it, and so does the Slack
  conversation through `configure_markdown`) and `languages` (the one
  language registry for the app, built on first ask and re-themed with the
  window; it was `zed_remote`'s `pub(crate)` global, and the file view, the
  diff view and the transcript all read from it). `rho-window` modules
  touched: `markdown` and `languages`, both new; the four chrome modules
  were not touched. Its one test only ever built because `rho-gui` was in
  the same invocation and turned on `gpui`'s `test-support` for the whole
  graph; `rho-window` now asks for it itself, so `cargo test -p rho-window`
  alone builds. The chrome cut's re-export line in `rho-gui/src/lib.rs`
  had no readers left and is gone with this cut, so the "no alias" claim in
  the note above is now true of the manifest as well as the use sites.

  What the screens still reach for is small and upward-free: the store's
  `FrameSummary`, `IncrementalUpdate` and `turn_open` come from
  `rho-registry` where they live, and `now_ms` from there too rather than
  from `Workspace`. `rho-gui` names `rho_agents::agent_view::AgentModel`
  and nothing else of the screens, and lost five dependencies it only had
  for them (`tree-sitter`, `tree-sitter-md`, `json-stream`, `languages`,
  `node_runtime`).

  Cost: no numbers, and this is why. The cut is a move — every code path,
  allocation and edit is byte-identical to what main ran, so a measurement
  here would measure main and be labelled as the screens'. The desk rig was
  held by another session's run while this landed, so I did not take one
  either. The transcript's own numbers are owed with the map (4), which is
  the crate held to the rule from its first line, and they will be taken on
  `user-2026-09-06` as `QA-HANDBOOK.md` sets out (`draw_ms` p99/max and
  `dirty_to_draw_ms` p99 from `frames.json`, each touched stage's
  `duration_ms` p99 against its `input_rows` from `editor.json`).

  Gate green: rho-gui 246, rho-agents 42 (the 32 transcript, render and
  screen tests moved with their modules; no test was added, dropped or
  rewritten), rho-window 1, rho-hosts 14, rho-registry 14; clippy
  `-D warnings` green over the three crates.

  *Landed, the map and its indexes (4).* `rho-registry` is gone; what it
  held is this crate's. The map is `AgentMap`: every agent the client
  knows, what each one is and what happened to it, the user's filing over
  the top, and the indexes the screens read it through. `fold`, `store`
  and `session` came with it unchanged; the old `render` module is `state`
  here, because this crate already has a `render` that turns a block into
  spans and the two are not the same thing — one is what a client shows of
  an agent, the other is how it is drawn.

  This is the first crate held to the cost rule from its first line, so
  the shape is the rule. What was there before made everything again on
  every event: one told agent rebuilt every row, cloned every summary into
  its host's snapshot, re-collected the parent and tag indexes, re-sorted
  them, and swept the verdicts — and inside that pass, `order.contains`
  per agent made a first sync quadratic. It is now one write per changed
  agent into indexes that are kept, never remade.

  What each event costs, for `k` changed in a map of `n`, all of it stated
  at the top of `map.rs` where the next person will read it:

  - told about agents (`told`, `restore`, `tell`): `k log n`, plus `k log
    k` to sort the ones this client has never seen into the front of the
    order.
  - the user's filing (`set_agent_filings`): `k log n`, and nothing at all
    when the filing offered is the filing already held.
  - a verdict, an activity, a touch, a life change: one lookup.
  - a host detaching or starting over: `k log n` in the agents that
    departed, and nothing walks the agents that did not. `detach_host` and
    `reset_host` now return the departed set rather than reaching into
    the window to move the point.

  And what each read a screen makes costs:

  - a row's own facts (`agent_facts`, `attention`, `agent_display_label`,
    `agent_human_name`, `agent_hidden`): one lookup each, so a frame costs
    the rows it draws. The dashboard is synced on events and drawn from
    its buffer, so a frame makes no pass over the map at all.
  - `agent_children`: the children. `agent_subtree`: the subtree it
    returns, plus sorting it.
  - `next_agent`: a lookup and the agents it steps over. The order is an
    index of keys handed out once — a newly discovered agent takes a key
    below every key in use, so it goes to the front without renumbering
    anyone, and stepping is a range on the visible set rather than
    collecting the visible agents to find a position in them.
  - `agent_by_tag`: a binary search of one host's agents of one role.
    `host_agents`: the host's agents, from the host index rather than a
    walk of the map asking each agent where it came from.

  The one read still proportional to the map is `agent_by_label`, which
  walks every agent asking what it is called; it answers what the user
  typed in the minibuffer, and it is written down in `map.rs` rather than
  indexed before anything is slow.

  Two answers rather than quiet carrying. `HostSnapshot::agents` — a full
  clone of every summary, per host, per event — was written and never
  read: residue, deleted. `next_attention_agent` had no caller anywhere;
  I built the attention index it would have read, found nothing reads it,
  and deleted both rather than keep an index for decoration. The attention
  reads the screens do make are per drawn row and per heading, and both
  are lookups. One thing for the user rather than for me: `AgentNext` and
  `AgentPrevious` have handlers and no key in the keymap, so the order
  index serves an action the fingers cannot reach. Either they get keys or
  they are residue.

  `rho-window` modules touched: `selection`, new — `ActivePane` and the
  point that goes with it are the window's, not the map's. The map no
  longer knows which agent is selected: `next_agent` takes it as an
  argument, and `Selection::forget` moves the point out of an agent that
  departed. The workspace holds the `Selection` beside the map it reads.
  `rho-window`'s other modules, `transient` included, were not touched.

  The rig caught what no test could. On the user's snapshot the client
  died on startup: redb records the Rust path of a table's value type, and
  `gui_agent_verdict_v1` was written as
  `rho-db::Sen<rho_registry::fold::Verdict>`. Renaming the crate renamed
  the type, and every existing mirror — the user's included — would have
  been refused with a `TableTypeMismatch` the first time the new client
  opened it. The table now names itself through `rho_db::SenAs` with the
  name it was written under, which is what that type exists for; the bytes
  on disk never changed. Nothing in the unit tests could have found this,
  because it only happens against a database written by an older build.

  Measured on `user-2026-09-06`, desk rig session 8, moving through the
  map on Home sixty times with the profiler on. The line `rig down`
  printed, verbatim:

  > 281 frames, draw p99 2.7 ms, 0 over 8 ms; worst gap 325 ms, p99 11 ms;
  > 21798 events, slowest stage buffer_edit p99 0.12 ms at 2 rows; 192
  > samples on rho-gui: `__syscall_cancel_arch_end` 5%,
  > `__memcpy_avx512_unaligned_erms` 4%, `eq` 4%

  Per frame: draw p99 2.7 ms, max 3.3 ms, nothing over the 8 ms bar. Per
  event: the slowest editor stage in 21,798 events is the one that edited
  two rows, and it cost 0.12 ms — time tracking what was touched and not
  the 2.5 million rows behind it, which is the shape the rule asks for.
  The worst gap of 325 ms is frame 4, the first sync of the whole desk
  after startup; the p99 is 11 ms and every gap after frame 4 is under
  5 ms. The map appears in the CPU profile only as B-tree searches for one
  agent's row — `search_tree` over `AgentSummary` and `MirroredAgent` —
  and never as a pass over the map. No map symbol is in the top three.

  One number that fails, and it is not the map's. Session 7 opened and
  closed an agent twenty times, one of them a 262k-token transcript: 281
  frames, draw p99 12.3 ms, 5 over 8 ms, worst gap 500 ms, and the slowest
  stage `wrap_map_update` p99 7,122 ms at 121,252 rows. That is the
  editor wrapping a whole transcript buffer when the screen opens — it
  predates this cut, nothing here touches it, and the wrap runs off the
  frame loop so the window kept drawing. It is the transcript screen's
  number and it fails the handbook's bar, so it is written down here
  rather than left in a profile nobody reads.

  Gate green: rho-agents 59 (the 14 that were rho-registry's, plus three
  new ones over the order, the filing and what the indexes say after a
  reparent and a host reset), rho-gui 243, rho-window 11 (`selection`
  brought one), rho-hosts 14; workspace clippy `-D warnings` green; the
  workspace has one crate fewer.
  *Landed, the transcript opens on its tail (9).* Session 7's failing
  number was this crate's: `wrap_map_update` p99 7,122 ms at 121,252 rows
  when a 262k-token transcript opened. Measured before designing, as
  ruled, and three things were true. The whole history was composed at
  open: `prepare_initial` rendered every block the agent had ever produced
  and `install_initial` put every one of them in the multibuffer before
  the first frame — 121,252 rows to show about 41. The transcript's own
  elisions could not help, because `DisplayElision` is a block-map
  construct and the block map sits *above* the wrap map (buffer, inlay,
  fold, tab, wrap, block), so eliding history hides rows that have already
  been wrapped. And the wrap is not a one-off: `WrapMap::set_wrap_width`
  rewraps the whole buffer on every width, so every resize paid the seven
  seconds again.

  Both halves are lazy now, in both senses the ruling asked for: history
  above the opening tail is neither rendered nor composed until a reader
  asks for it. The blocks themselves stay whole in memory — they are the
  fold's, shared by pointer, so the model's own list of them copies no
  text — and `records` and `buffers` cover `blocks[uncomposed..]`, growing
  upward. A transcript opens on the last 200 rows of it (`OPENING_ROWS`),
  which is a desk window twice over; reading upward composes another 400
  rows (`HISTORY_CHUNK_ROWS`) each time the reader comes within 40 rows of
  the top of what is composed. The separator a chunk's first block carries
  is the one it would have carried with all of history above it, so
  composing that history later leaves every chunk's text byte-identical —
  no excerpt is replaced, no anchor moves, and nothing below the new rows
  is laid out again.

  The three verbs the ruling named, and what each one costs:

  - **`gg`** composes everything, because the reader asked for everything,
    and the point lands when the top exists. Composition runs off the
    frame loop a chunk at a time — a step, then back to the window, which
    keeps drawing — and the echo line says "composing history" while it
    runs.
  - **`/`** is the buffer's search, so it searches the whole transcript:
    the history it has not composed yet is composed while the reader is
    still typing the query, and the search runs when it is all there.
    Worth saying plainly: `/` in a transcript did nothing at all before
    this cut. Vim emits `EditorEvent::SearchRequested` because this app
    has no zed pane, and the only listener was the dashboard's. The
    surface now hosts one, the same minibuffer search the dashboard has.
    `n` and `N` repeat it, landed straight after this cut: a search runs
    from the point rather than from the top of the buffer, wraps once and
    says so in the echo line when it does, and the query is the
    workspace's — one search register, as vim has one, so a query typed in
    a transcript repeats on the desk and the other way about. Vim's own
    `n` does nothing in this app, because it goes through a pane's search
    bar and there is no pane, which is the same reason `/` is the host's.
    A key means one thing per context, and a context is named: `n` and `N`
    are bound in `RhoTranscript` and `RhoDashboard`, the two surfaces that
    have a search, and the Zulip inbox and the Slack rooms keep them for
    the next unread by their own contexts rather than by being loaded
    later. A test asserts both halves; load order carrying a rule was the
    fragility it replaces.
  - **The point survives leaving and returning**, and it survives as a
    store position — which block, and how far into it — never a buffer
    offset. `AgentModel` remembers it whenever the point moves in the
    transcript, and remembers `None` while the point is in the prompt, so
    a surface left at the prompt comes back to the prompt and one left
    deep in history composes what it needs and comes back to the same
    block. A test asserts exactly that: open, `gg` to the top, close the
    surface, open it again, the point is on the block it was left on.

  The elision-covered fraction, measured because it was asked for even
  though folds are the fallback and not the plan: on the same transcript,
  off the rig's own mirror, 12,348 blocks, 121,114 rendered rows, 697
  elision plans covering 118,403 of those rows — 97.8 per cent. So the
  fallback would have worked, and it is still worth having later for a
  different reason than this cut: after a `gg` the whole transcript is
  composed and all 121k rows are wrapped again, and only a real fold, in
  the fold map below the wrap, takes them out of the wrap's input. That is
  the next thing to do here, not a thing this cut needed.

  One sentence worth keeping because it is what makes the whole thing
  safe: a chunk's first block is rendered with the separator it would have
  carried with all of history above it, so composing that history later
  leaves the chunk's text byte-identical.

  Owed, and not this cut's to fix: the editor rewrapping the whole buffer
  on every width change fails the cost rule on its own. Tail-first shrinks
  what it sees but does not fix it, and it is a vendored-editor primitive,
  so it goes on `rho-window`'s list beside the right prompt. And `y g g` —
  a yank with an operator pending — stays vim's own, so it copies what is
  composed rather than composing first; `gg` is bound only with
  `vim_operator == none`.

  Numbers, on the desk rig's `user-2026-09-06` snapshot, profiling
  binaries, one run per thing measured so each number belongs to one
  thing. Before, session 7 on the same agent: 281 frames, draw p99 12.3
  ms, 5 over 8 ms, worst gap 500 ms, `wrap_map_update` p99 7,122 ms at
  121,252 rows.

  - *Twenty opens and closes of the 262k agent* (session 24): 621 frames,
    draw p50 2.4 ms, p99 4.7 ms, max 8.1 ms, 1 frame over 8 ms and none
    over 16; worst gap 290 ms, p99 15 ms; `wrap_map_update` p99 7.9 ms
    over 432 input rows at its largest — the tail, not the history —
    and `block_map_sync` p99 0.02 ms. The seven seconds are gone because
    the rows are not there.
  - *Four width changes with the transcript open* (session 21): 83
    frames, draw p99 10.2 ms, 5 over 8 ms; the wrap still redoes the
    whole buffer on every width, but the whole buffer is now 432 rows,
    p99 7.5 ms. The primitive is still wrong and still owed; tail-first
    is what makes it survivable meanwhile. Driven with sway's own ipc
    socket (`output HEADLESS-1 mode WxH`), because `rho wayland` has no
    resize for a running session — worth knowing, and now in the QA
    handbook.
  - *`gg`, composing the whole history* (session 22): 265 frames, draw
    p99 16.3 ms, max 32.4 ms, 70 over 8 ms; worst gap 486 ms, p99 357
    ms. This one fails the frame bars while it runs, and it is the only
    one that does. Two things are true of it: the reader asked for
    everything, and the layer it costs is not the wrap — the wrap never
    saw more than 2,543 rows in one update, because composition arrives
    in chunks — but `block_map_sync`, p99 18.3 ms over a p95 of 50,045
    input rows. That is the argument for the real fold below the wrap
    being the next change here, and it says which layer to watch.
  - *A small agent, five opens* (session 23): draw p99 8.1 ms, wrap p99
    3.0 ms over 213 rows. Unchanged, which was the point of measuring it.

  Held by tests, not by the rig alone: a long transcript opens with
  history uncomposed, scrolling into it composes it, `gg` composes every
  row, a surface left deep in history returns to the same block, a search
  composes the history it looks through, and the cheap "does this block
  render to anything" predicate agrees with rendering it for every block
  there is.

  *Landed, the elisions are folds below the wrap map (10).* The tail-first
  cut left one number failing its bars: after `gg` the whole transcript is
  composed, and `block_map_sync` took a p95 of 50,045 input rows at p99
  18.3 ms. The elisions were display elisions, which are a block-map
  construct, and the block map sits above the wrap — so an elided turn was
  a turn already wrapped and then hidden. They are folds now: a fold is
  below the wrap, so an elided turn leaves the wrap's input and the block
  map's entirely. The visible tail of a turn is the fold map's own
  `ElisionPolicy::Tail`, the chip is the fold's placeholder — the same
  chevron and count the reader saw before — and each fold is tagged
  `HistoryFold`, so the thousands of concealment folds that live inside an
  elided range survive it. Folds are reconciled by diffing the model's
  specs against what an editor carries, skipping the common prefix and
  suffix, so composing history folds only the run that is new.

  A fold is gone once it is opened, so rho registers a crease beside every
  fold: `z o` opens an elided turn and `z c` closes it again, which is what
  a reader expects and what a display elision could not do at all.

  Two changes to vendor/zed's editor were needed, each its own commit
  before this one, under the user's ruling that we build editor primitives
  when we must:

  - **A fold in a multibuffer is a fold.** `z c`, `z o` and `toggle_fold`
    acted on the folds under the selection only in a singleton buffer; in
    a multibuffer they took the whole-buffer gesture instead, which is
    zed's project search, where a multibuffer is a list of files and
    folding one means folding that file. A transcript is a multibuffer
    with one excerpt, so no fold key reached an elision. The keys now act
    on the folds and creases under the point wherever they find them and
    keep the whole-buffer gesture when there is none — a strict widening;
    singleton behaviour is unchanged.
  - **Every path to the wrap map carries the row scales.** Found on the
    rig, not in a test: on the first open of a GUI process, `gg` composed
    history whose user messages then drew wider than the window and stayed
    that way. Measured off the pixels rather than guessed at — the row
    broke at 131 characters where it should have broken at 117, a ratio of
    1.12, which is `USER_MESSAGE_SCALE` exactly. A scaled row is wrapped at
    `wrap_width / scale` by the sync that writes it, and the wrap map takes
    the scales from the display map on each sync — but only
    `sync_through_wrap` handed them over. `DisplayMap::fold` is another
    path, and it consumes the buffer subscription itself, so when the
    transcript composes history and then folds it, the fold's own sync
    wraps the rows composition just wrote, at scales that do not yet name
    them. Nothing rewrites a row once wrapped, so they stayed too wide
    until a resize rewrapped the map. Sixteen call sites now go through one
    helper. This is why the fold change is three commits and not one: main
    is what the user builds at every commit, and the widening and the
    scales each regress nothing alone.

  The `Tail` policy had a bug of its own and it is fixed in the fold map
  rather than worked around here: a tail-eliding fold ended at the tail's
  first column, which swallowed the newline above it and glued the chip to
  the first line of the tail (`⋯echo`). The elided head now ends at the end
  of the row above the tail.

  What the QA of it taught, and it belongs here because it nearly cost an
  evening: a control that differs from the run in more than the variable is
  not a control. The first control run opened a different agent first, so
  the transcript under test was that process's *second* open — and the
  second open is the case that works. Two runs described the same way, "open
  the transcript and press `gg`", were two different runs.

  Numbers, desk rig, `user-2026-09-06`, profiling binaries, first open of a
  fresh GUI process then `gg`, the same drive on both builds:

  - *`gg` on the 262k agent* (sessions 33 and 36): `block_map_sync` input
    rows p95 55,144, max 128,710, 58.3M rows over the run at p99 21.97 ms
    before; p95 337, max 1,468, 265k rows at p99 2.61 ms after. That is the
    claim the cut was for: the rows are not there to be laid out. Draw p99
    21.1 ms with 100 frames over 8 ms before, 14.4 ms with 75 over after;
    `wrap_map_update` p99 29.2 ms at 2,564 rows before, 18.8 ms at 999
    after.
  - *Twenty opens and closes of the same agent* (session 37): 941 frames,
    draw p99 5.6 ms, none over 8 ms; `wrap_map_update` p99 2.65 ms at 223
    rows. Against the tail-first baseline of draw p99 4.7 ms and wrap p99
    7.9 ms at 432 rows, the draw p99 is 0.9 ms higher and the wrap does
    less. The 0.9 ms is reported rather than rounded away.

  Said plainly: `gg` still fails the frame bars — 75 frames over 8 ms is
  not a pass — and the layer to blame has moved. It is no longer the block
  map; it is the wrap map's own background rewrap, p99 18.8 ms a batch at
  999 rows, which is the whole-buffer rewrap primitive already owed to
  `rho-window`'s list. The reader asked for everything, and everything is
  what it costs.

  Held by tests: eliding history leaves fewer rows to the wrap than the
  buffer has, the rows a fold leaves still soft wrap, `z o` opens an elided
  turn and `z c` closes it again, the folds an editor carries survive a
  rebuild that does not touch their turns, and a fold that flushes a
  pending edit wraps the rows it flushes at their scale — 18 characters
  against 30 with the fix, 30 against 30 without it.

  *Landed, a width change wraps the reader's rows before the document
  (11).* The layer the elision cut left to blame. A width change threw the
  whole wrap map away and laid the document out again from row zero, in
  the background, publishing nothing until the last row was measured. On a
  262k transcript the rows in front of the reader — the only rows about to
  be painted — arrived behind rows nobody was looking at, and an edit
  arriving mid-pass waited for the document because `flush_edits` refuses
  to run while a rewrap is in flight.

  `set_wrap_width` now takes a `WrapPriority` beside the width. It is a
  parameter and not a setter because a range is meaningless without the
  width it applies to, and a parameter makes forgetting it a compile error
  at every caller rather than a stale range at one. A caller with a screen
  in front of it passes `ReaderRows`, the buffer rows in view with a
  screen of margin above and below; a caller with no screen — a measuring
  pass asking how tall the editor would be, a font change, a test — passes
  `DocumentOrder` and gets what the map did before. The reader's rows are
  laid out on the thread that asks, before the call returns, so the cost
  of the blocking part is the caller's and it is a screen. The rest of the
  document follows in background chunks of 500 tab rows spreading outward
  from the reader — down first, since that is where reading goes — with
  the map's pending edits flushed between chunks, so an edit that arrives
  during a width change waits for one chunk instead of a document. A
  chunk only starts when the map is settled (no pending edits, no
  interpolation), because a chunk works from a copy of the snapshot. When
  the reader scrolls clear of the run while the pass is still going, the
  next layout passes rows the run does not cover, and that call — same
  width, moved reader — wraps them at once and moves the run to follow.

  Rows not yet reached are never painted as if wrapped, and the mechanism
  is the ordering rather than anything the element does: a row is laid out
  at the current width before it can be drawn, because the element hands
  the rows it is about to paint to `set_wrap_width` in the same layout
  that paints them. A row outside the priority range is not on screen, and
  by the time a scroll brings it on screen it is either behind the pass or
  named by the next layout's priority. So there is no clipped row and no
  deferred row: `defer_paint_until_initial_wrap` is left as it was, and
  `is_rewrapping` now means what the element always wanted it to mean —
  the reader's rows are not laid out yet — with `is_backfilling` for the
  rest. The scroll needs no anchoring of its own: the editor's scroll
  anchor is a buffer position, so a chunk landing above the viewport
  changes what row the reader is on, not what text is under their eyes.

  The drag is deliberately not in this cut. A width change no longer
  blocks the reader or stalls an edit, which already turns a drag from a
  stall into spread work; whether it also wants a debounce is the
  element's business and its own change, after numbers say so.

  The measurement hole, and it is worth saying plainly because it made the
  previous note's blame wrong: **the rewrap was never a stage.** Every
  `wrap_map_update` number in this file is edit-driven work; the
  whole-buffer rewrap ran with no timing guard on it at all, so a resize's
  real cost landed in whichever stage happened to be on the stack — QA saw
  it as `multi_buffer_sync` p99 104 ms at 0 rows, which is a stage that
  had no rows in hand spending a tenth of a second. It is
  `wrap_map_rewrap` now, in the same commit, and it carries the rows it
  laid out.

  Numbers, desk rig, `user-2026-09-06`, profiling binaries, first open of
  a fresh GUI process in every run, three runs each side, main
  `0bb4ffc9` against the change:

  - *Four width changes with the 262k transcript open*, four runs each
    side (sessions 44 and 51 to 53 before, 47 to 50 after): draw p99 9.6,
    9.3, 9.6 and 9.1 ms with 3, 3, 1 and 2 frames over 8 ms before; 6.6,
    6.5, 6.5 and 6.6 ms with none over, in any run, after. Worst gap
    279–280 ms before and 279–300 ms after, in both cases the transcript's
    first compose and not a resize. The rewrap itself, now that it can be
    seen: p50 0.62 ms, p99 2.71 ms at 193 rows, eleven of them for four
    width changes — a priority pass and its chunks each side of the
    elisions.
  - *`gg` over the whole history*, three runs each side (sessions 43, 54
    and 55 before, 46, 56 and 57 after): draw p99 15.2, 14.5 and 14.2 ms
    with 76, 75 and 72 frames over 8 ms before; 14.4, 14.7 and 14.3 ms
    with 75, 73 and 75 over after. Unchanged, and
    the change is not what `gg` needs: with the rewrap finally on its own
    line it is 0.39 ms at 57 rows over that whole drive, while
    `wrap_map_update` — rows the composition edits, not rows a width
    change rewrites — is p99 18–25 ms at 999 rows. The previous note put
    `gg`'s blame on the rewrap; the rewrap was not the layer, and only
    instrumenting it could tell.

  So `gg` still fails the frame bars and this cut does not move it. What
  it moves is the width change: a resize of an open transcript now draws
  every frame inside 8 ms, and the reader's rows never wait for the
  document.

  Held by tests: a width change lays the reader's rows out at the new
  width before the call returns while rows they are not looking at keep
  the layout they had, and the background pass then reaches the rest and
  finishes — the second half of that is what fails without the change,
  because without a priority the whole document is rewrapped at once.

- **`rho-slack`, a real Slack client.** The session, socket and mirror
  that exist, plus what a client is: the channel and DM list with unreads,
  a thread view that reads well, compose and reply, reactions, mark read
  that sticks, search, and its own screens and keys. Tested against the
  fake Slack server (`fake.rs`) and against a copy of the user's real
  mirror. Read state: Slack's own cursor is the truth for reading and is
  written back when the user reads here; Rho's `SlackHandledThrough`
  stays the dealing cursor only. Owner: eng-bgkw.
  Ruling, 6 Sep: the Slack screens stay editor based. The conversation,
  the thread, the list and compose are drawn with the editor primitives
  the rest of Rho draws with (buffers, inlays, the composition), never a
  separate widget tree beside them; where the editor lacks a primitive a
  Slack screen needs, the primitive is built from the ground up and the
  screen keeps using the editor.
  Order, set 6 Sep after the inventory: the crate already had the list, the
  thread view, compose and reply, and reactions on screen, so the work is
  (1) mark read that sticks, (2) the card rule into the crate as Slack's own
  notion of attention, which is the flood fix, (3) adding reactions, (4)
  search, (5) the keys and the card-handing out of `rho-gui/src/slack.rs`,
  once `rho-window` exists.
  *1 landed.* Mark read that sticks. The fake first, because none of the
  four client bugs could be caught without the server behaviour to catch
  them with: `conversations.mark` works the badge out again from what is
  left above the cursor and pushes the frame Slack sends every client the
  user is signed in on; `subscriptions.thread.mark` keeps the thread's own
  cursor, serves it back through `getView`, and pushes `thread_marked`;
  `activity.feed` is newest-first and paged; `/control` gained `mark`, which
  is the user reading on their phone. Then the client: the cursor rises and
  never falls, so a reconnect cannot re-badge a conversation read a second
  earlier; a thread is marked as a thread, so reading one no longer marks
  the channel around it read; the cursor is written to the mirror wherever
  it moves and read back at startup, so the unread rule is in place before
  the network answers and at all when offline, with Slack's cursor still
  overtaking it the moment the counts land; and a surface takes the first
  cursor it is offered rather than only the one that existed when it was
  built, which is why a restart used to show no rule at all. The read-state
  rule above is unchanged and is now what the code does: Slack's cursor is
  the truth, the mirror is a head start and never a second opinion, and
  `SlackHandledThrough` was not touched. Six proofs against the fake, one
  of them replacing a test that had been asserting the bug. Gate green:
  rho-slack 144.
  *2 landed.* The card rule, in the crate, as Slack's own notion of
  attention. `Model::attention` is the one place the question is answered
  and it asks Slack's own read state, not rho's dealing cursor: a unit is a
  card when it is a DM or group DM with something unread, a mention, a reply
  in a followed thread since the reader last looked, or unread traffic in a
  channel the reader opted into. A channel with plain unreads is in the list
  with its count and is never a card. Every card carries the fact of why —
  `Attention`, not a sentence — and the words are made at draw time by
  `reason_text` out of the roster's current label, so a conversation named
  late reads `mentioned in #design` rather than a line written when the
  message landed. `slack_thread_facts` no longer decides what a card is; it
  carries the crate's answer, and `desk_view::slack_card` closes what the
  crate says nothing for. The desk's two cursors are untouched.
  On the record, because it reverses a documented behaviour: reading is a
  fact about a message and Slack's cursor is the truth for it, so a card
  read on the phone leaves. A verdict is the reader's key alone, and the
  desk's cursors are untouched. The test that asserted the old behaviour is
  rewritten to assert the new one and says why.
  The opt-in is rho's own fact, not Slack's, so it lives in rho's own file:
  a typed `rho_slack_watched_v1` table in `slack.redb`, written where the
  reader opts in with `w` on the row in the list, read back at startup, and
  shown as the word `watched` on the line it was made on.
  Under the cost rule. The rule keeps `asking`, the set of units currently
  a card, maintained by every event that can change it — a message, a mark,
  a mute, a follow, an opt-in, Slack's counts — each touching only the units
  it names; a channel-level event reaches its own units by a range scan over
  one channel's keys and no further. Drawing the cards costs the cards. And
  a start no longer reads history: the per-unit facts live in a typed
  `rho_slack_units_v1` table, written on the event that moves them, so a
  start is one range scan of one row per unit. History is walked exactly
  once, on a mirror written before the table existed, and the mirror says
  so afterwards so no later start pays it again.
  Numbers, from `cargo run --example card_rule` over a reflinked copy of the
  snapshot's `slack.redb` (5 conversations, 211 messages, 4 units): start
  reading messages 3.20 ms, start reading units 156 µs — 20× on a fixture,
  and the gap grows with history because one side is O(messages) and the
  other O(units). One mark 1.3 µs. One draw of the cards 4.8 µs. One draw
  of the conversation list 4.2 µs for 5 rows, which is the O(n log n) per
  draw that change 2b removes. Card counts old rule 4, new rule 4: on this
  fixture every unit is a mention or a DM, so there is no plain-traffic
  channel for the new rule to drop, and the fixture cannot show the flood.
  The real-mirror numbers wait for a snapshot taken after the mirror
  persists — the file we have holds only the QA fixture, because no Slack
  workspace is registered on the machine, so no session has ever run to
  write it. Proven instead against the fake, which badges the way Slack
  badges: `only_what_slack_would_badge_is_handed_over` serves three unread
  conversations and gets two cards.
  Found while looking for the flood, and left for the dealing composition
  rather than patched here: **a Slack row can be shown Open by a client
  that has no session**. `Sources` is in memory, so at a start with no
  session it is empty; `slack_card` has no source to derive from and the
  node falls back to the state the store holds, which no rule can then
  move. The composition must make that impossible: a Slack card exists
  while `rho-slack` asks and not otherwise, verdicts stay the desk's, and
  the map never decides a source's attention. `Model::attention` and the
  `asking` set are what it composes. This is also why the machine looked
  flooded with no session running: the user's GUI runs on their own device
  and its state, with the real credentials and the real mirror, is there
  rather than on the devbox the snapshot was taken from. rho now says as
  much at startup instead of deciding in silence that it has no Slack.
  Wanted from `rho-window`, for change 3: a transient buffer — opens under
  the point, lists keys and their meanings, takes one key, closes, leaves
  the surface behind it undisturbed, and back returns to it. Reactions and
  search are shaped around it and it is not built in `rho-slack`.
  Gate green: rho-slack 153 (119 lib, 8 mirror, 26 transport), rho-gui 276.
  **Change 2b, the conversation list.** The list is an index, not a sort.
  `order` is a `BTreeMap<RowKey, ConversationRow>` whose own order is the
  order on screen, `placed` remembers the key each conversation currently
  has, and an event takes one row out and puts it back: no pass over the
  list, no comparator run n log n times a draw. A first draw is O(n) once.
  After it the buffer is never rebuilt; the model hands over a log of what
  moved and the view edits those lines, one line out and one line in, as
  rope edits at a `Point`.
  An edit names conversations, never places. The line to take out is the
  one the view put that conversation on, and the line to put back is the
  one its new neighbour is on — `before`, the next key in the order, which
  the tree finds in its own depth. This is not a detail: the first cut of
  2b reported places as numbers, and a number costs the distance down the
  list to count. Measured, that made every event O(position) and the
  roster landing O(n²) — 678 ms at 20 000, startup-visible, exactly what
  the cost rule exists to stop. Naming the neighbour instead is what the
  numbers below are.
  The point follows the conversation, not the line number. It is read
  before the edits as the conversation it was on and put back on that
  conversation's line afterwards, so rows arriving above the reader do not
  move the reader. `rows_moving_above_the_point_leave_the_point_on_its_conversation`
  in `rho-gui` asserts it against the fake, through a real session.
  Narrowing stays model-side, so `/`, `n`, `G` and yank see the whole list.
  A narrowed listing is not the model's list, so the log cannot be applied
  to it and the view rebuilds; the same for a first draw, a health banner
  appearing, and a log that reached its cap.
  The journey this is measured against is **J5**, read and answer a Slack
  conversation, whose rule is that mark read sticks and *the list row
  moves once per event, not the list*. The second half is what this change
  is: one event edits the lines it touches and leaves the rest of the
  buffer alone. The first half landed in change 1. J5's `today` is still
  pending measurement on the rig, and the rig's Slack numbers only became
  meaningful at 9efbed8d, so the keystroke count is QA's to take, not
  this note's to claim.
  Numbers, `cargo run --release --example list_cost`, this machine:

  | | 5 000 | 20 000 |
  | --- | --- | --- |
  | the first draw, once | 721 µs | 4.74 ms |
  | one draw, sorted per draw (what it did) | 1.34 ms | 5.11 ms |
  | one draw, a screenful of 50 | 3.03 µs | 3.20 µs |
  | one badge, row unmoved | 1.04 µs | 910 ns |
  | one row, bottom to top | 1.18 µs | 1.44 µs |
  | one message arriving | 1.78 µs | 2.62 µs |
  | the roster landing, once a session | 6.43 ms | 30.9 ms |

  Read the shape, not the absolutes: a badge is flat in n, a move and a
  message grow like log n, and the roster landing — the one event that
  legitimately renames every row — grows like n log n and asks for the
  list again rather than logging an edit per row, because n edits cost
  more to replay than one draw. Before the neighbour fix the same three
  rows read 31.7 µs, 16.5 µs and 35.7 µs at 20 000, growing with the list.
  What is still O(n) per refresh and said so: `paint` re-anchors every
  highlight in the buffer, and the view's `drawn` vector shifts on an
  insert. Both are one pass over lines already in memory with no
  allocation, and neither is on the frame path — but neither is O(log n)
  either, and the next pass at this file is where they go.
  Gate green on b6cb0dbe: rho-slack 157 (123 lib, 8 mirror, 26 transport)
  and rho-gui, this change's live view test among them; clippy
  `-D warnings` and `cargo fmt --check` clean across the workspace.
  The live view test waits for the row to move rather than for the gpui
  executor to park. The frame crosses a real socket into the session's own
  tokio loop and comes back, and parking says nothing about whether that
  has happened — under a loaded suite it had not, and the test's own
  `assert_ne!` guard caught it. A loaded machine can now only make the
  test slower, never make it lie.
  Two things seen on the way in and now gone, recorded because they cost
  a gate run each: on 2ac218a7 rho-gui's lib test binary segfaulted at
  load, before enumerating a test, under both codegen backends; on
  9efbed8d clippy stopped on `echo_text_for_test`, used only from
  `src/tests.rs` and so dead to the plain lib build. Both reproduced on a
  clean checkout of main with nothing of this change in the tree, and
  both are fixed on 0bb4ffc9.
- **`rho-dag`** (today `rho-desk`). The store is a global DAG of cells
  across hosts: notes, labels, parents, verdicts. The crate keeps the
  store and gains the map screen and the note views. The screen is
  called the map.
- **`rho-shell`.** Window state and nothing about any source: focus, key
  contexts, the echo line, surfaces and their history, transients and
  the minibuffer, the shift tap, telemetry, chime. (`rho-shell` the crate
  name is taken by the terminal shell; this one is `rho-window`.)
  Its primitives are designed one at a time in `RHO-WINDOW-DESIGN.md`, held
  against the user's ruling. First is the transient buffer, which `rho-slack`
  needs for reactions: the note's finding is that today's transient has the
  right data shape and the wrong everything else — it draws into the bottom
  strip rather than under the point, its actions are `&mut Workspace`
  closures, and 791 of its 2,101 lines are agent quota and cost charts that
  are not a window primitive at all. The shape is kept, the primitive is
  replaced. Owner: eng-8gpr, after eng-b8os's chrome cut creates the crate.

  *Landed, the chrome (first cut of `rho-window`).* The vocabulary every
  screen is drawn with, moved verbatim and nothing else: `style` (the
  classes a span carries, the regions, the gutter and chip colours, the
  attachment and refusal blocks), `highlights` over a multibuffer,
  `editor_config` (what a buffer is opened as), and `visualization`. None
  of the four reached into `Workspace`: they knew buffers, editors and the
  theme and nothing above, which is why the move is verbatim and why the
  cut is a manifest and a set of imports rather than a redesign. Every use
  site names `rho_window::` now rather than `crate::`; no alias was left
  behind, because a re-export would have let a screen keep believing the
  chrome is its own. The rest of the window — focus, surfaces, history,
  transients, the minibuffer, selection and the active pane — follows here.
  Gate green: rho-gui 275 and rho-window 1 (the style test moved with its
  module), workspace clippy `-D warnings` green.

  *Landed, the transient buffer (`rho_window::transient`).* The primitive,
  built the way the design note settled it and touching no other module in the
  crate. A `Transient<A>` is a title and rows of key, meaning and an optional
  value; `A` is the caller's own action type, so the crate names nothing above
  it and a source crate puts an item in a menu without naming the workspace.
  A press is answered rather than performed: `press` returns run this item,
  take this digit as a count, dismiss, or nothing is bound, and the caller does
  the doing — which is what lets the menu be tested without a window and drawn
  by anything. It opens as a measured block under the point's anchor, with the
  editor's own text style, the way `style::refusal_block` already does: buffer
  text under the row the reader is on, not a strip at the bottom of the window.
  One presentation path, `items()`; there is no by-index second API for the
  phone. Applicability is at open — an item with nothing to act on is not in
  the menu rather than in it and failing when pressed. `escape` and `ctrl-g`
  dismiss, because they mean that everywhere else. An unbound key keeps the
  menu: a mistype is not a reason to lose it.
  The rule is one key and closed. `Kind::Infix` exists so an item can declare
  itself a toggle and the press says `closes: false`, but no menu declares one
  yet and none will until the user rules on the open question in
  `RHO-WINDOW-DESIGN.md` — the mechanism is there, the chaining is not.
  Cost: a press is one pass over the rows on screen, a draw is O(rows in the
  menu); nothing behind either grows with the desk. Numbers come with the
  wiring, because a primitive nothing opens has no frames to measure.
  Not yet wired: `rho-gui`'s 19 menus still run through its own
  `transient.rs`. The verdict menu moves first, on its own, and that is the
  change the rig proves.
  Gate green: rho-window 11 tests (10 new), clippy `-D warnings` clean,
  `cargo fmt --check` clean.

  *Landed, the verdict menu on it (`rho-gui`'s `transient` and `workspace`,
  `rho_window::transient`).* The first menu wired, and the one a tap of
  `shift` opens. `verdict_menu` and `verdict_snooze_menu` are now data over a
  `VerdictAction` enum — done, mute, snooze, the room, todo, file, undo, pull,
  and a unit — and `Workspace::run_verdict` is the only place in the program
  that knows what those mean. The menu is a block under the point in the
  active editor: the bottom strip holds nothing but the keyboard while it is
  up, an element with the focus on it and no size of its own. `s` replaces the
  menu with the snooze units over the same row and `escape` goes back to the
  verdicts rather than out, so back returns here too; a second `escape` leaves.
  The other 17 menus are untouched and still draw in the strip.
  Two things the rig caught that no test could. The block was created with
  `height: None`, and the menu painted over the rows below instead of moving
  them down — `Block::has_height` is what turns the editor's measuring on, so
  a block with no height starts at zero rows and stays there; it wants
  `Some(1)` and the editor resizes it to what the element drew. (The same is
  true of `style::refusal_block`, which is still `None`: that is a real defect
  in the chrome, filed here rather than fixed in this change.) And the Magit
  column layout was drawing as one column however it was chunked, so the
  chunking went: one item per row, which is what a buffer is anyway.
  Also removed: moving counts into the menu left `Workspace::transient_count`,
  `take_transient_count` and `Transient::counted`/`takes_count` with no reader,
  since the verdict menus were the only counted ones. The path is deleted
  rather than left write-only, the count digit in the strip's render with it.
  Proven on the desk rig, session 12, on `user-2026-09-06`: `shift` over a
  running agent opens the verdicts under that row with the rest of Home
  readable below them, `s` swaps in the units, `escape` `escape` leaves the
  point exactly where it was, and `x` mutes the row the point was on and says
  so in the echo line. Emacs-feel checks: the point survives back, nothing
  needed the mouse, no modal appeared and nothing dimmed, one key ran and
  closed. The rig-down line for that session:
  `113 frames, draw p99 4.1 ms, 0 over 8 ms; worst gap 161 ms, p99 8 ms;
  12909 events, slowest stage multi_buffer_sync p99 0.11 ms at 0 rows; 126
  samples on rho-gui: __memcpy_avx512_unaligned_erms 9%,
  __syscall_cancel_arch_end 5%, eq 5%`. Opening the menu costs one block
  insert and one measured element of eight rows; a press is one pass over
  those rows. Nothing here grows with the desk.
  Gate green: rho-gui 245 passed and 3 ignored (248 total, against main's
  246), rho-window 11, clippy `-D warnings` clean, `cargo fmt --check` clean.
  *Landed, the root menu and the three under it (`rho-gui`'s `transient` and
  `workspace`).* The second batch of the seventeen, and the one that turned
  the verdict menu's private plumbing into the window's one way of showing a
  menu. `root_menu`, `slack_menu`, `hosts_menu` and `projects_menu` are now
  data over the same primitive: `MenuAction` is `Open(MenuId)`, `Verdict(..)`
  or `Command(..)`, and `Workspace::run_command` is the single match that
  knows what a menu item means, beside `run_verdict` which already did.
  `VerdictBuffer` became `MenuBuffer` and `open_menu` is the way in for any
  menu; `space` no longer touches the bottom strip at all.
  Two shapes came out of doing the root menu rather than another leaf, both
  written up in `RHO-WINDOW-DESIGN`. An item names a menu rather than opening
  one, which is what lets the seventeen move in batches — the root menu says
  "hosts", the workspace decides where hosts is drawn, and the four menus
  still reached by name (`input`, `agent`, `new`, `status`) open in the strip
  from the same arm until their batch. And back is a stack rather than a
  parent: the verdicts were one deep so one parent sufficed, but `space a s`
  is three, and an escape that leaves from the third step instead of
  returning to the second is exactly what the design says never happens.
  Also removed, because the primitive does it: `Transient::item_when`,
  `retain_applicable` and the strip item's `when` field. Applicability is at
  build time now — `root_menu(&subject)` — which is the same moment as before
  and one fewer pass. `Workspace::echo_text_for_test` was missing its
  `#[cfg(test)]`, so `clippy --all-targets -D warnings` on rho-gui failed on
  main; the attribute is added here since the gate has to be green.
  Proven on the desk rig, session 30, on `user-2026-09-06`: `space` on Home
  with the point on a running agent's row opens `rho` as a block under that
  row, with the rows below it moved down and the bottom strip empty; `h`
  replaces it with `hosts` over the same row; `escape` comes back to `rho`
  and `escape` leaves, the point on the row it started on. The screenshot
  taken after the first `escape` is byte for byte the one taken when the
  root menu first opened — back returns the buffer as it was, and two
  identical frames is the strongest way to say so.
  Two more pictures for the two claims that are not about one menu. `space
  s` from the same row draws `status` in the bottom strip, which is the
  mixed state working: a menu that has moved and a menu that has not, one
  keystroke apart. And `space` on a transcript opens the same `rho` menu
  under the point in that buffer, this time with `a agent…` and `d changes`
  in it — the same key, the same menu, applicability answered by the surface
  rather than by the menu. `l` there ran the message log and the menu closed:
  one key, ran, closed. Emacs-feel checks: the point survives back, the same
  key means the same thing in both buffers, nothing needed the mouse, no
  modal appeared and nothing dimmed.
  The rig-down line for that session:
  `187 frames, draw p99 9.1 ms, 2 over 8 ms; worst gap 746 ms, p99 22 ms;
  17875 events, slowest stage wrap_map_update p99 2.43 ms at 220 rows; 295
  samples on rho-gui: __syscall_cancel_arch_end 14%,
  __memcpy_avx512_unaligned_erms 4%, runtime 4%`. The two frames over budget
  are the block insert and the rewrap it causes when a twenty-six row menu
  opens into a 121k-row Home; opening a menu is one block insert and one
  measured element, and a press is one pass over the rows on screen. Nothing
  here grows with the desk, but a menu on screen is a real edit to the block
  map and does not belong in a sample being measured for something else.
  Gate green: rho-gui 259 passed and 3 ignored (against main's 258), clippy
  `-D warnings` clean, `cargo fmt --check` clean.

  *Landed, the draft's menus and the phone's one way in (`rho-gui`'s
  `transient`, `workspace` and `workspace_phone`).* `new`, `input`,
  `status`, `agent`, `snooze`, `phone_root_menu` and the phone's snooze
  sheet: twelve of the seventeen have moved, and the five that have not are
  the usage charts, which carry their series and want a screen of their own
  rather than a mechanical move.
  The part worth reading is the phone. The primitive's fifth complaint
  about the old transient was that the phone had a second way in —
  `phone_rows` and `action_at`, by index into a private `Vec`. The sheet
  now draws from the same `Transient<A>` through `items()`, and a tap runs
  the item a key would have run through the same `run_menu_action`. It is
  not the same picture — a thumb needs a target, not a row — and that is
  the point: one menu, one set of actions, two drawings.
  Which made the block optional rather than conditional at the call site.
  A menu on the phone is open the same way it is open on the desk; it just
  has no block, so nothing in the buffer moves to make room for a sheet
  that is drawn over the surface. `MenuBuffer::block` is an `Option`, and
  the one place that decides is `show_menu`.
  Proven on the desk, sessions 40 and 41, rebased onto main `1e04ecff` and
  rebuilt, so the pictures are of b8os's fold trio with this batch on top.
  On the desktop: `space` on Home with the point on the `eng-8gpr` row draws
  the root menu as a block directly under the point, `n` replaces it with
  `new` — **a** agent…, **p** page…, **n** note… — the rows below pushed
  down and the bottom strip empty, and two escapes give Home back in a frame
  byte-identical to the one the menu opened over. Three consecutive
  open-and-dismiss round trips: two byte-identical, and the third differing
  by 1,167 pixels which are all the `eng-b8os` name going from muted to
  normal — that agent's own mirror row moving while they work, not the
  buffer. A no-input control over the same span is byte-identical, which is
  how the live row and the buffer were told apart.
  The phone half could not be photographed, and the reason is the rig, not
  the change. Every way into the sheet is a tap — the ☰, the deal card's
  header, the empty feed's header — and the rig's headless seat has no
  pointer: sway reports `capabilities: 0` with no devices, so
  `seat seat0 cursor press` succeeds in the ipc and reaches no client, while
  `wtype`'s virtual keyboard is a device and does. Keys do arrive at phone
  width (`ctrl-shift-f` opens the minibuffer there), but nothing bound to a
  key opens the phone's menu: `space` is bound for editors and the deal card
  focuses none, and the one command that would leave the card for the
  dashboard is itself an item in the menu being opened. So the phone half is
  proven by `the_phone_sheet_is_the_same_menu_as_the_block` — 400x800, sheet
  title `menu`, rows Map/Slack/Agents/Status, no block and no strip, `Status`
  opening a submenu with a back, two dismissals to close — and not by a
  picture. Giving `rho wayland` a virtual pointer is the rig's next item and
  the handbook says so; until then the phone is a test, not a photograph, and
  this note says which.
  The block-insert re-measure asked for after the fold trio is session 41 and
  nothing else: ten open-and-dismiss round trips on Home, `115 frames, draw
  p99 5.8 ms, 0 over 8 ms; worst gap 290 ms, p99 8 ms; 13100 events, slowest
  stage block_map_sync p99 0.08 ms at 1 rows`. Against the two frames over
  8 ms and the rewrap of what was around the block measured before the trio,
  the block insert now costs the block: one row of block-map work, and no
  frame over budget on the whole run.
  Removed, and named here so it can be asked for back. `phone_desk_menu`
  was the Map screen's own sheet on the phone — **cycle folds**, **edit
  notes**, **new** — and nothing opened it: the phone's ☰ opens the root
  menu whichever root is showing, so the Map sheet had never been
  reachable. A menu nothing opens is residue by the rule every dead item
  here has been answered with, so it goes rather than being carried over as
  data. If the map's own sheet is wanted on the phone it comes back as a
  `Transient` over `MenuAction` like the rest, with one line in the bottom
  bar, not as a strip. `phone_cycle_dashboard_folds` went with it, having
  had no other caller; the desktop's fold cycling is untouched.
  The block-insert cost from the batch above came off `rho-window`'s owed
  list with the number above: the fold trio changed what Home's block map
  holds, and a menu open is one row of it now. What is left on that list
  beside the width-change rewrap is the wrap map's own whole-buffer rewrap,
  which is eng-b8os's next task, not this crate's.
  Gate green on the rebase: rho-gui 262 passed and 3 ignored — one test
  added for the phone sheet and one removed with the menu it was about, so
  the count is main's, which the fold trio moved from 259 to 262. Clippy
  `-D warnings` clean, `cargo fmt --check` clean.

  *Landed, the sheet the change above opened and never drew (`rho-gui`'s
  `workspace` and `workspace_phone`).* The overlay at the bottom of
  `render` matched on the bottom strip's `transient` alone, so once the
  phone's menus stopped being strips the phone opened a menu into an empty
  screen: `menu_buffer` was set, the sheet had a title and rows, and nothing
  put them on the glass. The test that shipped with the change asked the
  workspace what the sheet said and got the right answer, which is the
  whole lesson — *open* and *drawn* are different claims, and a test that
  reads state proves only the first. What found it is the rig's new
  pointer: the first real tap on the phone's header did nothing, and the
  cursor turning into a hand over that same header is what said the tap was
  landing and the screen was empty on purpose.
  The fix is one arm — a menu on the phone draws the same sheet — and two
  corrections that came with it: the backdrop's keys go to `menu_key` when
  the sheet is a menu, and a tap outside closes the menu rather than a
  strip that is not there. The new test taps the bottom of a 400x800 window
  where the last row lands and asserts the row ran; it fails without the
  arm, which is the only kind of test that could have caught this one.

  *Landed, the rig can tap (`rho-cli`'s `wayland` driver, `QA-HANDBOOK`'s
  driving notes).* `click` and `move` were sway ipc `seat cursor` commands,
  which move a cursor belonging to a pointer device the headless seat does
  not have: the ipc answered `success: true` and no client ever saw a thing.
  The driver now creates a `zwlr_virtual_pointer_v1` on the session's own
  seat for the length of the tap, which is the shape `wtype` already had for
  keys — `swaymsg -t get_seats` shows `capabilities: 0` with no devices at
  rest and `1` with a `wlr_virtual_pointer_v1` while a tap is in flight, and
  the seat is bare again afterwards, so no run changes what the next run
  finds. Coordinates are logical pixels read off a screenshot and checked
  against the output's size *now*, from `get_outputs`, not the size in
  `session.json`: a resize goes through sway's ipc without touching that
  file, and every session the phone is driven on has been resized. That is
  its own unit test.
  Proven on desk session 45, first open of the session, at 800x1600 with
  scale 2 — a phone. A tap on the note card's header at (200, 16) drew the
  sheet: **menu** with **close**, and Map / Slack / Agents / Status, the
  same menu the desk draws as a block under the point. A tap on Status at
  (100, 772) ran that row and the sheet became **status** with a **back** —
  upload GUI performance snapshot, usage…, version. A tap on back gave a
  frame byte-identical to the sheet before it, and a tap on the backdrop
  closed it into a frame byte-identical to the one before the first tap:
  the surface underneath never moved, which is what the sheet drawing with
  no block is for. A no-input control over the same span is byte-identical,
  so those are the buffer and not the desk. Keys are unharmed: back at
  2560x1664, `space` still opens the root menu as a block.
  The session line is `179 frames, draw p99 24.6 ms, 15 over 8 ms; worst gap
  307 ms, p99 135 ms; 15620 events, slowest stage multi_buffer_sync p99
  104.32 ms at 0 rows`, and it is not a menu number: that session is two
  resizes and a card pull, and a resize is the whole-buffer rewrap on the
  owed list. The menu's own cost is session 41's, ten round trips and
  nothing else, none over 8 ms.

- **The rig proof for surfaces and history, and what `S` is** (`rho-window`
  `history` doc; `RHO-WINDOW-DESIGN.md` the section). Desk session 84, first
  open of a fresh process on the user's snapshot at 2560x1664 scale 2, main
  0caf8671. Frames compared byte for byte by md5; every claim below has a
  no-input control taken over the same span on the same surface, because a
  frame of Home is not equal to itself — the agent ages tick and the row
  moves between "next" and "running" while you watch.
  **The question first, because it decides whether the rest matters.**
  eng-en1p read `back` dropping the surface it leaves and asked whether that
  destroys the view, which would break the Emacs rule on the ordinary path:
  leaving a buffer never kills it. It does not. `S` is a handle — every
  `SurfaceView` variant is a refcounted `Entity`, and the context's buffer
  list (`surfaces: HashMap<ContextId, Vec<Surface>>`) holds a clone that
  `make_surface` returns rather than building a second view — so dropping the
  entry drops a handle. The machine's doc now says `S` must be a handle and
  why, and the design section says it in the same words. The one path that
  really destroys a view is eviction, `release_agent`, and it already has the
  warm store: `warm_surface` rebuilds the transcript if back reaches it.
  **Proven on the rig, three pairs, each against its control.**
  (1) A desk node with the point set, the usage screen opened over it, back:
  the frame on the node is byte-identical to the frame before leaving
  (`5c4ebc6b`… twice; the usage frame in between `2e3f345a`). Control: the
  node frame is stable over 5 s with no input.
  (2) The same node scrolled twenty-five rows down, left for the usage
  screen, back: byte-identical again, same digest.
  (3) A transcript — 275k, eng-en1p's, scrolled eighty rows up into history
  with a fold opened so the display map is not its default — left for the
  usage screen in the same context and returned to: byte-identical
  (`f39e9e01`… twice), control clean.
  **And the case en1p added: the surface back drops, reopened.** The node was
  dropped by a second back, then reopened from Home. 1,512 pixels of
  4,259,840 differ, 0.036%, in exactly two bands eighteen pixels wide and one
  text row tall: the cursor leaving the row it was on and arriving on the row
  Home named. The scroll — twenty-five rows into the document — is identical,
  which is the whole answer: a rebuilt view would have started at the top.
  The point moves because opening from Home *is* "go to this node", so the
  open path places it; history did not lose it.
  **The one case not run, and why.** "The thing behind B is deleted while it
  is in the stack" is not reachable from the keyboard now that history is per
  context: an agent's transcript lives in that agent's context, and
  `prune_contexts` drops the whole context when the agent goes, so there is
  no stack left to walk. I checked that rather than assuming it — the
  workspace test for it passes with the `forget` call commented out. The
  invariant is proven on the machine instead, where a key can be forgotten
  under a stack that outlives it: `a_forgotten_surface_is_never_shown_again`
  and `forgetting_removes_every_entry_for_that_key`.
  **Cost.** Nothing in the profile belongs to the history machine, which is
  what O(1) with nothing walked at draw time looks like from outside. The
  session's own numbers, reported as they came: 1,404 frames, draw p99 9.4 ms
  with 29 frames over 8 ms, dirty-to-draw p99 82 ms and a worst gap of
  275 ms, 88,507 editor events. That fails the handbook on both counts and
  the drive is why — it is a first open of a 275k transcript and of the whole
  desk tree with eighty-key scroll bursts, not a controlled comparison. One
  number in it is worth someone's time and is not mine: `wrap_map_update`,
  30 calls, `input_rows` p50 and p99 both 1,048, `duration_ms` p50 2.84 ms
  and max 1,519 ms. A 500× spread at constant input size is not the row
  count, and b8os's `wrap_map_rewrap` puts 500 rows at 2.1 ms, so it is a
  cold or contended path inside the update rather than work proportional to
  anything. Filed for cut B's owner rather than fixed here.
  **Two drive details for whoever runs the next one.** `ctrl-k` does not
  reach the application through the rig's driver — `f21`, bound to the same
  `SurfaceBack`, does, and every back above was driven with it. And the
  leader menu draws past the bottom of the screen: `space` then `s` shows
  entries clipped by the window edge, which is a defect for the sweep, not
  for this note.

- **Landed, surfaces and history are one machine** (`rho-window` module
  added: `history`; `rho-gui` `pane` cut down to `SurfaceKey`, `workspace`,
  `workspace_phone`, and `create` and `slack` where they read the viewport;
  `RHO-WINDOW-DESIGN.md` the section this cuts against).
  There were two histories and that was the defect: `Pane<S>`'s per-context
  `Vec<S>`, whose `show` ran `retain` over the whole stack, and the
  workspace's global `surface_history: Vec<WarmSurface>` with a
  `history_cursor`, which is the one the keys actually reached. A push was
  O(entries) twice over in two places that had to agree, and closing a
  surface or forgetting an agent scanned both. Both are gone. What replaces
  them is `rho_window::history::History<K, S>` — generic over the key the way
  `Pane<S>` was, naming no source type — held once per context in
  `contexts: HashMap<ContextId, SurfaceHistory>`. Per context is eng-en1p's
  ruling under the Emacs rule and is named in the design section so the user
  can reverse it by name.
  The shape, and its cost. Entries are appended and never removed from the
  middle; a `HashMap<Key, usize>` holds each key's newest index, so showing a
  surface that is already in the stack leaves the older entry stale instead
  of paying a scan and a memmove, and back skips any entry the map no longer
  points at. Each stale entry is skipped at most once in the life of the
  stack, which is the amortised bound. The numbers the tests pin, at the
  size the note quotes: at 1,000 entries a push touches one entry and
  rebuilds nothing, a back touches one, a forget touches none at all — only
  the map — and 1,000 distinct pushes rebuild zero times. The rebuild is the
  only O(n) event; the worst case for staleness, two surfaces alternating
  over 2,000 pushes, rebuilds 142 times, once per fourteen pushes, and
  leaves twelve entries for three reachable surfaces. Nothing walks the
  stack at draw time: the viewport draws `current()`.
  Death is one call. The four paths — close, discard a draft, forget an
  agent's transcript, forget a detached daemon's agents — hand a key to
  `forget`, and the invariant that back never shows a dead entry is two
  tests on the machine rather than four places agreeing about a cursor. The
  workspace test for the daemon case asks only for the result and says so:
  it is kept twice over, once by `forget` and once because an agent's
  context dies with the agent, and no path has to know which saved it.
  **Two behaviour changes, both deliberate, both flagged for the user.** `q`
  on a standalone draft now goes back one surface instead of to Home — it is
  a close like any other close, and Home was the old global cursor's answer.
  And there is no forward step: a stack walked past is shorter, which is
  what "history is a stack" means and what the old cursor did not do.
  Eviction is not a death: `release_agent` leaves the entry alone and
  `warm_surface` rebuilds the transcript if back reaches it, which is what
  the old code did in the path that was live.
  Gate green: rho-window 24 passed (the machine's own tests, from 11),
  rho-gui 259 passed and 3 ignored, rho-agents 68 passed, clippy
  `-D warnings` clean on both crates, `cargo fmt --check` clean.
  **Owed, and not in this note:** the rig proof — A, B, back byte-identical
  to A against a no-input control, again scrolled with a fold closed, again
  after the thing behind B is deleted while it is in the stack, first open
  named. The desk is with b8os for Cut A; the numbers follow in their own
  note when the seat frees.

- **Landed, the usage charts are a screen** (`rho-agents` module added:
  `usage`; `rho-gui` modules touched: `usage` (new), `transient`,
  `workspace`, `workspace_phone`, `pane`, `telemetry`, `journal`). The five
  chart items under `space s u` were the last thing the bottom strip was
  for, so they became plain `MenuAction`s that open a screen and the strip's
  element tree went with them: `Transient`, `TransientRun`, `TransientItem`,
  `quota_usage`, `active_auth_namespaces`, `global_usage`,
  `agent_cost_usage`, `usage_days`, the workspace's `transient` and
  `transient_stack`, and the phone's `phone_transient_action`. `transient.rs`
  is 2115 lines down to 601 and holds menus, which are values and nothing
  else.
  The summary lives in `rho-agents`' `usage` and the painting in `rho-gui`'s.
  The line between them is that the reduction takes a *count* of columns, not
  a pixel: nothing in it is about gpui, so it sits with the rest of what the
  client knows about agents and is unit-tested without a window. `usage.rs`
  in the GUI only paints. The screen is a buffer — the title and the totals
  are its text and the chart is a block below them, the way a menu is a block
  below its row — so `:buffer`, `escape`, history and search work because
  there is nothing new for them to work on. `SurfaceKey::Usage` is one
  screen: `c` after `r` redraws it rather than opening a second place to be,
  which is what the new test asserts.
  The cost rule is met by where the arithmetic happens, not by how fast it
  is. A summary is built when a series arrives, once per arrival, and reduced
  there to the chart's width, so a frame walks at most one point per column
  per line and the percentile buckets are already bucketed. `render()`
  computes nothing over samples. A window that gets wider than the summary
  was reduced to rebuilds it — that is a resize, not a frame, and the same
  goes for a window that gets taller, since the chart's height follows the
  viewport now.
  **The before, on the desk snapshot's own rows.** The two defects were that
  the strip recomputed its summary inside `render` from every sample it held,
  and that the transient carried the series to do it with. What that walked
  here, counted against `/home/maan2003/src/rho-rigs/desk/state/rho/rho.redb`
  with the daemon down: 279 Claude quota samples over seven days (124 opus,
  155 fable) and 807 over thirty, and 23041 gpt samples in
  `chatgpt_quota_observations`, which keeps thirty days and nothing older.
  So a frame of the thirty-day rate limit chart on main recomputed over
  roughly twenty-four thousand samples, and the seven-day one over some
  thousands of them; after the change a frame walks at most the chart's
  width per line, which the unit test pins at 833 points for a week of
  minute-by-minute polls on an 832-column chart.
  **The after, and it is not a win per frame.** Both runs are the first open
  of a fresh session on the same desk snapshot at 2560x1664 scale 2, driven
  by the same twenty opens of the rate limit chart (`escape`, `space`, `s`,
  `u`, `r`). Session 58 on main 98ca1835: 206 frames, draw p50 1.93, p95
  3.65, p99 4.27 ms, prepaint p99 2.77, paint p99 1.68, 0 frames over 8 ms.
  Session 62 on this change: 296 frames, draw p50 3.08, p95 5.92, p99 6.14
  ms, prepaint p99 4.02, paint p99 2.39, 0 over 8 ms. The change costs about
  1.8 ms of p99 draw and it stays inside the budget.
  The obvious suspect was the picture's size — the strip drew 1280x240 and
  the screen draws 1280x682 — and it is not the cause. Session 63 is the
  control for it: the same binaries with the chart pinned to the strip's 240
  px and the identical drive script, 263 frames, draw p99 6.07 ms, prepaint
  p99 4.03. Holding the picture's height changes nothing, so what is left is
  that a screen redraws the viewport where a strip redrew a strip. That is
  the cost the change takes on, in exchange for the bound above and for the
  charts being a place you can go rather than a strip that appears. Sessions
  62 and 63 ran the identical script; session 58's pauses I cannot swear were
  the same to the millisecond, so the frame *counts* between main and the
  change are not like for like and the per-frame percentiles are what the
  comparison rests on.
  Two things the screenshots caught that no assertion here would have. The
  block went in as `BlockStyle::Fixed`, which the editor measures at
  min-content: the chart drew across 60% of the screen with the rest blank,
  and the test asserting the block exists passed the whole time. It is `Flex`
  now. And a chart sized for a strip on a screen of its own is a quarter of
  the glass in use, so the height is the viewport's less the header, floored
  at the strip's 220. Inserted, drawn, and drawn at the right size are three
  different claims.
  The docs that named the wrong crate are fixed in the same commit:
  RHO-WINDOW-DESIGN.md lines 83 and 143 and this file above said the charts
  were "drawn by `rho-visualizations`". They are not and were never going to
  be — that crate is the daemon-side store of immutable SVG blobs an agent
  recorded, and a live quota chart through it would be O(events) per refresh
  plus a store round trip for a series the GUI already holds.
  Gate green: rho-gui 258 passed and 3 ignored (main's 263 less the eight
  tests that moved to `rho-agents`, plus two pchip tests in the GUI's
  `usage` and one new test for the screen), rho-agents 68 passed (eight
  moved and two added for the reduction), rho-window 11 passed, clippy
  `-D warnings` clean, `cargo fmt --check` clean.
  One housekeeping line, since the restart truncated three of these files to
  zero bytes mid-change: `workspace.rs`, `usage.rs` and `tests.rs` were
  restored by reading operation `52a22986d0df`'s working-copy snapshot with
  `jj --at-op … file show`, nothing was written to the store, and after the
  gate passed they were re-read against that operation. They differ from it
  by exactly the six edits deliberately re-applied on top and nothing else.

- **Landed, the refusal block is measured too** (`rho-window` module touched:
  `style`; `QA-HANDBOOK` C9 and the driving notes). `style::refusal_block` was
  the other `height: None` in the chrome, filed in the change above and fixed
  here with the same word: `Some(1)`, and the editor resizes it to the two or
  three rows a jj failure actually draws.
  What the rig said about it is worth more than the fix. The picture asked for
  — a refused draft pushing the rows below it down — cannot exist on that
  surface. The refusal anchors at the end of the draft body, so nothing is
  under it; the attachment chip that shares the anchor has the lower priority
  and takes the row above. I drove the same script twice on the desk, once on
  a build with `height: None` restored (session 19) and once on the fix
  (session 25) — new agent, three-line body, a workdir the daemon refuses, an
  image pasted so the chip is there too — and the two screenshots are
  byte-identical. So this defect is invisible until something is drawn below
  the block: the verdict transient had rows under it and showed it at once,
  this one would have waited for whatever is added under a refusal next. That
  is now C9 in the handbook, with the rule that finds it — read `rho-window`
  for `height: None` — rather than the picture that does not.
  Three driving facts came out of taking it, and are in the handbook's
  "Running anything": `rho wayland` has no resize for a running session, so
  sway's own ipc socket and `output HEADLESS-1 mode 1024x600@60Hz` is the way
  (eng-b8os, who measured the resize case with it); an image reaches the
  clipboard with `wl-copy --type image/png` against the session's `runtime`
  dir, and the paste is `ctrl+shift+v` because `ctrl+v` is visual block; and
  `rho-qa build` does not build `rho-qa`, so a stale `target/profiling/rho-qa`
  silently writes no summary into the session — sessions 23 to 25 have none
  for that reason.
  The rig-down line for session 25, the fixed build:
  `969 frames, draw p99 2.6 ms, 0 over 8 ms; worst gap 278 ms, p99 8 ms;
  53355 events, slowest stage buffer_edit p99 0.12 ms at 2 rows; 458 samples
  on rho-gui: __memcpy_avx512_unaligned_erms 8%, __syscall_cancel_arch_end 4%,
  compare 2%`. A refusal costs one block insert and one measured element;
  nothing here grows with the desk.
  Gate green on main dca5c374 (rebased onto b8os's transcript tail): rho-gui
  250 passed and 3 ignored, rho-window 11, clippy `-D warnings` clean,
  `cargo fmt --check` clean.
- **Landed, the sweep for blocks that are never measured** (`rho-window`
  modules touched: `style`, `transient`). The rule the refusal left behind
  — read `rho-window` for `height: None` — read once, and then made
  something a reader does not have to remember. The sweep found nothing to
  fix: the three blocks the chrome hands out (the attachment chips, the
  refusal, a transient's menu) all start at `Some(1)` and are resized to
  what the element draws, and no `BlockProperties` built anywhere in
  `crates/` is missing a height. So the change is two tests rather than a
  fix. Each asserts that the block a constructor returns starts with a
  height, which is the whole of the defect: `Block::has_height` is
  `height.is_some()`, and a block without one is painted over the rows
  below instead of moving them down — invisible on a surface with nothing
  under it, which is why the refusal's took a rig session and a reading to
  find. Proven by putting `refusal_block` back to `None` and watching the
  test fail. Gate: rho-window 13 passed, `cargo fmt --check` clean.
- **Dealing is composition, not a crate of its own.** Each source crate
  hands the dealer cards: the facts a card is ranked by and the reason
  it claims attention. A Find hit shares the reason type with a card but
  is its own type: a card claims attention, a hit answers a query. The
  map never touches another crate's store: a verdict on an agent card
  is a command routed by the card's source to `rho-agents`, and a filing
  of an agent is told to `rho-agents` the same way. The dealer ranks across sources with one visible
  rule set, Home and the lamp read it, and a verdict goes back to the
  crate that owns the card. Every card carries its reason to the screen.
- `rho-browser` already has this shape and stays.

## What a source crate promises the window, and nothing more

- Its screens (gpui entities) and the key bindings they own.
- Its cards, with facts and a reason, and the handlers for verdicts on
  them.
- A fake server, and tests that run the crate alone against it.
- Its own state on disk under the state dir, in its own file.

Nothing else crosses. No crate reads another crate's state; the window
holds no source state; `&mut Workspace` appears in no crate.

### The cost rule holds in every crate, from the first line

Ruling, 6 Sep. The rule of GUI-MODEL-DESIGN is not the agents crate's
rule, it is every source crate's, and it is designed in rather than
fixed after: per event, O(rows the event touches) plus O(log n) to place
them; per frame, O(rows drawn); never a pass over a crate's whole mirror,
list or set on an event, a keypress or a frame. Agents were built the
other way and cost a 163 s start; Slack is not built that way at all.

What it means when a crate is written:

- The mirror is indexed for every question a screen asks (by
  conversation, by time, by thread, by read cursor), so an answer is a
  lookup and a bounded scan, not a walk.
- Counts, badges, the unread rule and the ranked list are maintained on
  the event that changes them and read when a screen draws; nothing is
  recomputed from the mirror to draw.
- The card rule is incremental: an event moves the cards it touches and
  no others. A rule that needs the whole mirror to decide is the wrong
  rule.
- Screens draw the rows in view and fold the rest; a conversation of
  fifty thousand messages opens as fast as one of fifty.
- Every one of these is proven, not assumed, on the user's snapshot in
  the rig, with the per-event and per-frame numbers in the landing note.
  A landing note without the numbers is not a landing.

## QA that is the user's world

Owner: eng-8gpr, first deliverable, because the other two prove their
work on it.

- **The snapshot.** On demand, a copy of the user's live state: store,
  agent mirror, Slack mirror, and the GUI's own files, taken while the
  daemon runs (a proof does not need a quiesced copy). Kept under a name
  and a date; a bug report becomes "this snapshot, this action".
- **The rig runs on the snapshot.** The daemon on the copied store with
  its own XDG dirs, the fakes for Slack and the browser fed from the
  copied mirrors, the GUI headless in the isolated Wayland session, the
  same release binaries the user runs, with the profiler on.
- **Accumulated state, never empty.** The QA desk is persistent: it
  starts from the user's snapshot and every QA session adds to it (agents
  created, verdicts given, notes filed, threads read), the way the user's
  own state accumulates. A fresh empty state is not a test of anything.
- **Agent QA with a handbook.** A QA agent drives the GUI through the rig
  (keys, screenshots, the journal), following a handbook of the tricky
  cases: dealing after a restart, verdicts on Home rows, a Slack thread
  marked read that comes back, an agent that stops appearing, creation
  into a managed workspace, Find for an agent by what the user last said
  to it, shift held versus tapped, the map after a reparent. The handbook
  grows with every bug the user reports; a case is closed by a run, not
  by a claim.
- **Proof numbers come from here.** Per-event and per-frame costs, frame
  gaps and main-thread samples, on the user's data.

### Landed

- **The pictures for the transient at the bottom and the usage charts**
  (rig session 93, main `d671995e`, frames in
  `rho-rigs/sweep/frames/s93-*.png`). Owed with the two commits before this
  one; the desk was held for the panic repro when they landed.
  *The round trip.* On Home, with the point on a running agent's row: a
  no-input control two frames 3 s apart is byte-identical, and `space` then
  `escape` is byte-identical before to after, three times over. Two deep —
  `space h`, `escape`, `escape` — is byte-identical too, against its own
  control taken the same way. So the menu leaves nothing behind, and the
  desk was still enough for that to mean something.
  *What the open changes.* Against the frame before it, opening the `hosts`
  menu differs in exactly two bands: rows 1446–1663, which is the menu, and
  rows 350–391, which is the point's cursor block ceasing to be drawn while
  the transient holds the keyboard. Every Home row is unmoved to the pixel.
  That is the ruling's invariant in a measurement: the buffer above is
  untouched. The row highlight stays, so the reader can still see where they
  are; the cursor itself does not, which is what focus moving means here and
  was as true of the block as it is of the bottom.
  *The picture.* `s93-menu-hosts.png` is the one the ruling asked for: five
  rows of `hosts` against the bottom edge, the Home buffer above exactly
  where it was, the point's row visible mid-buffer with a screenful of empty
  space between the two. `s93-open-1.png` is the root menu, 25 rows, which
  reaches further up but still sits on the bottom edge. Neither is clipped:
  the last row's ink ends 15 px above the window's bottom.
  *The charts.* All four — rate limit, model cost, usage share, agent cost —
  read by eye and by count. Not one pixel in any of the four is darker than
  the background: axis labels, end labels, legend and the p50/p90/p99 labels
  are all light on dark and all in the buffer's face. The header is buffer
  text and always was.

- **The usage screen's words get a colour and a font.** Two user reports:
  text drawing black on the dark background, and fonts wrong in several
  places. One cause under both. The charts are a block under the header, and
  a `div` inside a block is not inside the editor's text: it inherits gpui's
  defaults, which are black and not the buffer's face. The header — the
  title and the totals — was never affected, because it is buffer text on
  purpose; everything inside the block was.
  Drawing black: the axis value labels (`100%`/`50%`/`0%`, `full`/`½`/`0%`,
  and the agent-cost ticks) and the two end labels under every chart
  (`−{days}d` and `now`). The legend entries and the `p50`/`p90`/`p99`
  labels set their own series colour and were never black — which is why
  only some of the text looked wrong.
  In the wrong face: all of those, the coloured ones included.
  The fix is one place rather than five. `render_chart` now takes the
  editor's text style and wraps every chart in a root that sets the family,
  the weight, the size and the colour, so the labels inherit them and a
  child that wants its own colour still overrides its parent. That is what
  makes it true of the next label somebody adds, not just of these. The axis
  keeps its explicitly smaller size — an axis is read past, not read — and
  now wears it in the buffer's face.
  The transient had the same defect from the same cause and loses it in the
  commit before this one: its rows had no colour of their own, and at the
  bottom of the window it inherits the strip's, which is the editor's.

- **The transient goes back to the bottom of the window.** The user's ruling,
  and it overturns eng-en1p's earlier one: the menu was drawn as a block in
  the buffer under the point, and Magit's transient sits at the bottom of the
  frame with the point where it was. What the earlier specification got wrong
  is that it made the menu something that happens to the reader's text —
  opening one reflowed the surface, and near the bottom of the viewport it
  drew off the edge of the screen with nothing scrolling it into view (the
  defect that started this, first on the sweep's list).
  Only the drawing changed. `Transient<A>`, the items, the keys, the count,
  the back stack and the phone sheet are exactly as they were.
  `Menu::block` is gone and `Menu::render` takes its place: an element in the
  editor's text style, with no opinion about where it goes. The desk pins it
  to the bottom edge — `absolute().bottom_0()` over the window, not another
  row in the column — so nothing above it reflows, and it wears the same
  chrome as the minibuffer (`bottom_strip`) because it is the same piece of
  furniture in the same place. If the ruling meant no shared chrome either,
  that is a one-line change.
  `MenuBuffer` no longer carries a block id, a weak editor or an anchor: it
  is the menu, the count and the way back, and it now says nothing at all
  about where the menu is drawn. `show_menu` cannot fail, so it returns
  nothing; `reinsert_menu_block` and `remove_menu_block` are gone (a count
  is a redraw, a close is a `None`).
  The test that read the block's height back is gone with the block.
  `the_root_menu_opens_at_the_bottom_and_escape_retraces_it` now measures the
  surface's drawn rows before, during and after: opening the menu adds no row
  to the buffer, which is the invariant the ruling is about, and the point is
  unmoved at every step as before.

- **The snapshot and the rig** (`crates/rho-qa`). `rho-qa snapshot` copies the
  live state while the daemon runs and verifies the copy by opening it and
  counting rows; the live directory is read from and never written, never
  opened by a database. The first snapshot, `user-2026-09-06`, is 42.8 GiB and
  holds 2,582,646 rows across 38 tables of store, plus the agent mirror, the
  action journal, the inbox and the Slack mirror. What is not copied is an
  allow list with reasons: no `auth.d` or iroh key, because a rig daemon is its
  own node and runs without `--iroh`; no logs; no `sandboxes`, which is dead
  bubblewrap scaffolding the user does not use.
  `rho-qa rig new` clones a snapshot into a runnable rig — a reflink clone on
  bcachefs, so 42.8 GiB costs 16s and no disk, and the base snapshot stays
  pristine. `rho-qa rig up` stands the rig up: the daemon on the copied store
  under the rig's own XDG dirs, the fake Slack with its API base read back from
  its log, the fake browser as the client's Brave, and `rho-gui` headless in
  the `rho wayland` session with the CPU profiler on, from `target/profiling`
  by default or the nix binaries the user runs with `--binaries nix`.
  The desk accumulates: `rig new` refuses to overwrite a rig, nothing resets
  one, and every `rig up` appends a session line to its `rig.json`.
  `rho-qa build` builds the five binaries a rig runs in one command, with the
  shell's own rustflags; `RHO_QA_LD` replaces the linker when a dev shell pins
  one that cannot link an optimised binary.
  Proven end to end on the desk: daemon, fake Slack, headless GUI and profiler
  up on the 42.8 GiB copy, Home drawing the user's real agents and desk cells
  with the fake's Slack cards beside them, and a CPU profile and frame log
  written on the way down.

- **The fake Slack fed from a real mirror.** `rho-qa fake-slack --mirror` reads
  a copy of a `slack.redb` and hands the fake what it holds — roster,
  conversations with their kinds, each history, the threads under it, Slack's
  own read cursor — using the same `add_*` calls the fixture uses, so `fake.rs`
  is untouched. `rig up` feeds the fake from the rig's own mirror whenever it
  has one and falls back to the fixture with a line in the log. The user's own
  id is remapped onto the fake's `ME`, so "me" stays "me"; the mirror is copied
  before it is opened, because the rig's GUI holds that same file. `rho-slack`
  gains one additive accessor, `Mirror::workspaces`.
  What this turned up: the snapshot's Slack mirror holds only the fixture
  workspace — 5 conversations, 211 messages, the fake's own `acme` — so the
  flood the QA premise names is not in the mirror on disk. The loader is right
  and the data is not there yet; a snapshot taken after a real Slack session
  will carry it.
  Also fixed here: `rig up` started its daemon before the last one had let go
  of the store, so the new daemon died with `DatabaseAlreadyOpen` after its
  socket was already on disk and the GUI sat on "reconnecting". It now waits
  for the old process to exit, kills it if it will not, and checks the new one
  is alive rather than trusting the socket.

- **The handbook** (`QA-HANDBOOK.md`). Every case in the bullet above, written
  so an agent runs it without asking what was meant: why it is tricky, the
  exact keys and commands, what passes, what fails, and a "Closed by" line that
  is empty until a run fills it in. Three sections beyond the cases. The rig's
  own preconditions, because a rig that lies to you invalidates everything that
  ran after it — the daemon alive and not just its socket, the GUI holding a
  Slack session, and the mirror being the user's rather than QA's. The
  Emacs-feel checks, run over whatever case is already running: the point
  survives back, the same key means the same thing in every buffer, nothing
  needs the mouse, no modal appears, a transient takes one key and closes. And
  the scale proofs: which snapshot (`user-2026-09-06`, 42.8 GiB, 2,583,116 rows
  across 55 tables), which numbers (`draw_ms` and `dirty_to_draw_ms` p99 from
  the frame log, `duration_ms` against `input_rows` per stage from the editor
  log — that pair is the per-event O(touched) evidence), and what fails: 8 ms
  p99 draw, 50 ms p99 dirty-to-draw, any stage whose time grows with the
  snapshot while its `input_rows` does not, any main-thread sample in ingest,
  dealing or the store.
  Two limits are recorded in the handbook rather than left to be rediscovered:
  the snapshot is daemon-side only, because the user's GUI runs on their own
  device, so no Slack case runs at flood scale yet (answered since by
  `--gui-state`, below); and `rig up` still starts
  the GUI when the fake did not register as the workspace, which looks exactly
  like a dealing bug. Both have a case with an open "Closed by".
  Also learned while writing it: the CPU profile is symbolized where it is
  written — the frames in `.0.bin.gz` carry Rust names, not addresses — so the
  worst-frame-gap summary line on `rig down` needs the trace decoder only, not
  the binary the profile came from.

- **The summary line on rig down** (`crates/rho-qa/src/profile.rs`, and the
  session entry in `rig.rs`). Every landing note from here needs numbers off a
  run, and getting them meant standing a viewer up over a directory of
  sidecars. `rig down` now stops the GUI, waits for what it writes on the way
  out, reads all three files and prints one line: frames drawn, `draw_ms` p99,
  how many went over the 8 ms budget, the worst dirty-to-draw gap and its p99,
  the editor stage with the worst p99 with the rows it had in hand — the pair
  that is the per-event side of the cost rule — and where the GUI thread's
  samples landed, three symbols with their share. The same line is written into
  the rig's session entry in `rig.json`, so a note quotes the run instead of
  re-deriving it, and `rho-qa profile <name>.bin` prints it again for any
  session, including the ones already on disk. `rig status` shows the last.
  The CPU profile needs no binary and no `addr2line`: Dial9 symbolizes each
  segment against its own `/proc/self/maps` before compressing it, so the names
  are in the file. A sample is attributed to its leaf frame, and the thread is
  the GUI's own (`rho-gui`) rather than the profiler's flush and worker
  threads, which are always in the file and never the answer.
  A session with no profile says nothing rather than summarizing nothing.
  Proven on the desk, session 5, driving Home with the keys and nothing else:
  `81 frames, draw p99 3.6 ms, 0 over 8 ms; worst gap 11 ms, p99 11 ms; 11471
  events, slowest stage buffer_edit p99 0.04 ms at 2 rows; 90 samples on
  rho-gui: __memcpy_avx512_unaligned_erms 13%, parse.constprop.0 3%, runtime
  3%`. That is the user's own 42.8 GiB desk with the fake's Slack rows beside
  the real agents, and the cost rule holds on it: the worst editor stage is
  40 µs against the two rows it touched, and no frame came near the budget.
  Reading a profile is O(events in it) once per `rig down`, off the GUI's own
  path entirely — the rig reads what the run already wrote.

- **A snapshot of two devices** (`rho-qa snapshot --gui-state <dir>`, with
  `paths`, `rig new` and the handbook). The limit the handbook recorded — the
  snapshot is daemon-side only, because the GUI runs on the user's own device
  — is now a flag. `--gui-state` names a client's state directory and copies
  its half beside the daemon's, into `gui-state/rho/` in the snapshot, with
  its own manifest entries (`gui_source`, `gui_files`, `gui_databases`) and
  its own verification: every copied database is opened and its rows counted,
  and a torn file is copied once more before the snapshot is called failed.
  It is a second allow list with reasons, not a second deny list. What it
  takes is what a screen reads: the agent mirror, the inbox, the action
  journal so undo survives a restart, the desk device because a rig that lies
  about which device it is deals the wrong hand, the client's own store, and
  the Slack mirror. The mirror is on this list because it is the client's
  file: `rho-slack`'s session writes every arriving message into it under the
  client's state directory and the daemon never touches it, so the real flood
  is on whichever device ran the GUI and the copy beside a daemon is whatever
  that box happens to hold — on this one, QA's own `acme` fixture. It stays on
  the daemon list as the fallback for a snapshot taken without `--gui-state`,
  and the overlay in `rig new` is what makes the client's copy win.
  What it refuses is what it refused before: no `auth.d`, no `iroh-secret.key`,
  no `sessions` — a rig is never the user, on any device — and no `rho.redb`,
  the daemon's store, which a client never has. Two tests hold both rules to
  the lists themselves rather than to a comment.
  `rig new` lays the GUI half over the daemon's state after the clone, because
  a rig runs one state directory and the GUI reads its files from the same
  place the daemon does; it says which files came from the other device.
  Proven end to end on copies, never on live state: a snapshot of a fabricated
  two-device pair copied and read back 1,197,254 agent-mirror rows on both
  halves, copied neither of the two credentials planted as decoys in the GUI
  source (an `auth.d/token` and an `iroh-secret.key`), listed as
  `1198457 rows + gui 1198141 rows from …`, and a rig cloned from it came up
  with the six GUI files overlaid — including a Slack mirror that differed on
  the two sides, where the rig's copy is byte for byte the client's and not
  the daemon-side one.
  Also here, and the reason it is here: `rho-qa build` now builds `rho-qa`
  itself alongside the rig's binaries. A stale copy in `target/profiling`
  wrote no summary line into desk sessions 23 to 25 and looked like a rig
  fault; the tool that reads a run can no longer be older than the run.
  Gate green: rho-qa 4 tests (2 new), clippy `-D warnings` clean,
  `cargo fmt --check` clean.
- **The rig's GUI has a Slack session, and a rig that is up says who holds
  it** (`crates/rho-qa`, `rig.rs`). No rig's GUI has ever had a Slack
  session. `rig up` wrote `credentials.json` for a workspace it named
  itself, `rig`, while the fake comes up as `acme`, and the credential
  store is keyed by workspace name — so the lookup missed and the client
  ran sessionless on every rig that has existed. Nothing said so: the fake
  was listening, the daemon was up, the GUI dealt, and Slack rows simply
  stayed Open forever, which reads as a dealing bug. `rig up` now reads
  the workspace out of what the fake printed and writes credentials for
  that name, and refuses to start the GUI at all when the fake never
  reports one, saying that a sessionless client shows every row as Open
  and no rule can close them. Proven on desk sessions 26 and 27: `slack
  fake on … as workspace `acme``, credentials keyed by `acme`, and a
  `#design` row taking a `done` verdict — leaving `next` and being
  replaced by the next row of the flood, which no rig could do before.
  This closes handbook R2.
  The other half: `rig up` refuses when the rig is already up and names
  the session, whoever started it (`RHO_MCP_AGENT_ID`, falling back to
  `RHO_AGENT_ID` then `USER`) and the daemon pid that holds it, with
  `--take` to stop what is running and take it. The lock is the live
  daemon pid recorded in the last `rig.json` session, not a poll. Two of
  us drove the same desk twice in one evening, seconds apart, in both
  directions; an overlapped run's numbers are noise and the screenshots
  do not show it.
  Gate green: rho-qa 6 tests (2 new), clippy `-D warnings` clean,
  `cargo fmt --check` clean.
- **`gg` gives the reader the top, not the transcript** (`crates/rho-agents`,
  `transcript/mod.rs`, `transcript/gap.rs`, `agent_view.rs`). Going to the
  top used to compose every block between the tail and the first one before
  the point could land: the whole history wrapped, folded and blocked to
  show one screen. Composition now serves the end the reader asked for. The
  transcript keeps a head and a tail with a gap between them — `blocks[..head]`
  composed for a reader at the top, `blocks[uncomposed..]` the tail it opened
  on — and both edges splice into the same records and buffers, which the
  multibuffer already allowed because its excerpts are keyed by start block
  and so stay ordered whatever order they arrive in.
  The fill is reading-aware, like the wrap: it closes from the head downward,
  because that is where reading goes, and a reader who reaches the gap is
  served their own edge instead of the head's.
  The step is sized to the frame budget, and the unit that matters is not the
  one the code counts: `HISTORY_CHUNK_ROWS` is buffer rows, but a step costs
  display rows, and on the desk snapshot at 1280 logical columns a transcript
  buffer row is about 3.4 display rows once wrapped. 400 buffer rows was a
  step of some 1,400 wrapped rows; 40 is a median step of 171 rows and 3.1 ms.
  Desk sessions 71–73 against 79–81, `gg` on the longest transcript on the
  snapshot (eng-2cjj's, 235k tokens), three runs a side, first open of a
  fresh process, 2560x1664 scale 2:
  before, the reader waits 5.7 s for 85 steps of 486 rows, step p50 10.3 ms,
  draw p99 15.5 ms with 36 frames over 8 ms of 458;
  after, the first step is 2.8 ms and it *is* the top — the point lands on
  block 0 there — and the gap closes 9.6 to 10.3 s later over 143 to 162
  steps, step p50 3.1 ms, draw p99 7.9, 8.0 and 8.8 ms with 7, 7 and 13
  frames over 8 ms of about 730.
  Two of the three runs are inside the budget and the third is not, and what
  is left over is one thing: a single history block larger than the whole
  step, up to 2,300 rows and 34 ms, which the composition cannot divide
  because a block is its unit. That is what cut B removes by eliding before
  the wrap sees the rows, and it is the reason this note does not claim the
  budget is met.
  The inlay map's share of the fill fell from 1,054–1,114 ms to 242–313 ms,
  not because that defect is fixed — it is on rho-window's owed list, 3.2 to
  13.3 µs a row as the buffer grows — but because it is no longer handed a
  history to walk.
  The gap says so in the buffer: one block, `height: Some(1)`, above the
  tail's first block, "N blocks still composing", gone when the gap closes.
  It sits above a block that does not move while the head grows towards it,
  so a fill step that serves a reader at the top rewrites nothing, and the
  count is read at paint time so a closing gap counts down without touching
  the buffer. Nothing blocks scrolling.
  Proven three ways, because they are three claims: a test asserts the block
  is inserted in the gap and removed when it closes; a screenshot shows it
  drawn, one row, "2444 blocks still composing"; and the drawn shot needed a
  rig-only build with the step slowed to 300 ms to catch it at all, because
  on the real build the gap is gone in ten seconds — which is also why the
  mid-fill shot of a normal run shows the top of the transcript and no
  marker.
  The failing-without test is rows laid out, not blocks composed: `escape g g`
  on a 200-turn history, summing the wrap map's sync traces at the moment the
  point lands. 961 rows before, 144 after.
  Main-side runs were on 79142ea6 and the change is on 8ac59765; the
  difference between them is the usage axis label and two handbook entries,
  nothing the editor or the transcript touches.
  Gate green: rho-gui 260 tests (1 new), rho-agents 68, `cargo fmt --check`
  clean.

## Order

1. eng-8gpr: the snapshot rig and the accumulated QA desk, so it exists
   before the crates land on it. Then the handbook and the QA agent.
   Then `rho-window`, taking the shell out of the workspace.
2. eng-b8os: `GUI-MODEL-DESIGN` slice 6 (small), then `rho-agents`. The
   public surface is the list of what the workspace touches today; the
   move is first mechanical, then the reach is cut.
3. eng-bgwk: `rho-slack` as a client, on the fake server first, then on a
   copy of the user's mirror. The flood is fixed inside the crate with
   client rules (DMs, mentions, threads the user took part in, channels
   opted into become cards; the rest is readable, never dealt).
4. Dealing composition once `rho-agents` and `rho-slack` both hand cards;
   then the rules, one source at a time, with the user's data on the
   table.

Each engineer works serially in their own crate. Every change starts with
`jj new main` on the current main commit, not on whatever the workspace
was last on. Land through eng-en1p: gate green, `cargo fmt --check`
clean, a landing note in this document under the crate.

## Symptoms of the wrong shape

- A source's state read from the workspace or from another crate.
- A screen that needs the workspace to render.
- A test that seeds an empty state and calls it a QA run.
- A card without a reason.
- A fix proven on the seeded rig only.
