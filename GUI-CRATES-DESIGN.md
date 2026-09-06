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
  on disk, the agents map and its indexes (what remains of
  `rho-registry`), the transcript, creation, Find over agents, and the
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
