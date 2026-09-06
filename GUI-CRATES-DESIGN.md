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
    What it does not yet have is `n`/`N` to repeat, which is vim's and
    needs the same treatment; it is written down here rather than
    discovered.
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
  device, so no Slack case runs at flood scale yet; and `rig up` still starts
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
