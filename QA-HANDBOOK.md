# The QA handbook: the cases, and how a run closes one

Owner: eng-8gpr. The rig is `crates/rho-qa`; what it is and why is in
`GUI-CRATES-DESIGN.md` under "QA that is the user's world". This file is the
list of things that have broken or are likely to, each written so an agent can
run it without asking anyone what was meant, and so a run either closes it or
says exactly what it saw instead.

A case is closed by a run, not by a claim. "Closed by" carries a date, the
commit the binaries were built from, and the artefact — a screenshot, a journal
excerpt, a number. A case with an empty "Closed by" has never passed.

## Running anything

```sh
rho-qa snapshots                          # what state exists
rho-qa rig list                           # what rigs exist
rho-qa rig new --from user-2026-09-06 --name desk   # once; it refuses to overwrite
rho-qa build --binaries profiling         # the binaries a rig runs
rho-qa rig up desk                        # daemon, fakes, headless GUI, profiler
rho-qa rig down desk                      # stop; the state stays as the run left it
```

`rig up` ends by printing the line that drives the session it just started.

Driving the GUI, with the rig's own runtime dir:

```sh
export XDG_RUNTIME_DIR=/home/maan2003/src/rho-rigs/desk/run   # where the session lives
rho wayland --session desk key "ctrl+shift+p"                 # a chord
rho wayland --session desk input down:shift wait:400 up:shift # a held shift
rho wayland --session desk screenshot --output /tmp/case.png
rho wayland --session desk tree                               # the window tree
```

Three things the driver does not do, and what to do instead.

- **Resize.** There is no resize for a running session. Sway's own ipc socket
  is the way: the path is `ipc_socket` in the session's `session.json`, and
  `output HEADLESS-1 mode 1024x600@60Hz` through it takes effect at once and
  the client resizes. That is a real width change through `set_wrap_width`,
  which is how the wrap-on-resize case can be driven at all.
- **The clipboard.** `wl-copy --type image/png < file.png`, with
  `XDG_RUNTIME_DIR` pointed at the session's own `runtime` directory and
  `WAYLAND_DISPLAY=wayland-1`, puts an image where the GUI can paste it. In
  the GUI the paste is `ctrl+shift+v`; `ctrl+v` is visual block.
- **Its own binary.** `rho-qa build` builds what a rig runs, not `rho-qa`
  itself, so `target/profiling/rho-qa` can be older than main — an old one
  quietly writes no summary into the session. Run it through `cargo run -p
  rho-qa` when the line matters, and check `rho-qa --help` lists `profile`.

Three rules for every run.

1. **The desk is never reset.** `rho-qa rig new` refuses to overwrite one.
   State accumulates across sessions the way the user's does; a case that needs
   an empty desk is not a case, it is a unit test.
2. **Nothing touches the user's live state.** The live directory is read from
   by `rho-qa snapshot` and by nothing else, ever.
3. **All mocking is server side.** The fake daemon is a real daemon on a copied
   store; Slack is `rho-qa fake-slack`; the browser is
   `rho-browser/examples/fake_browser`. Nothing is stubbed inside the GUI.

## What the rig itself must be true before any case runs

These are not product cases. They are the rig lying to you, and they have
happened.

### R1. The daemon is alive, not just its socket

A daemon that dies opening the store leaves a socket on disk and every later
step reports success while the GUI sits on "reconnecting". `rig up` waits for
the previous daemon to exit, kills it if it will not, and checks the new one is
alive. If `rig up` ever prints "daemon up" and the GUI still says
"reconnecting", the check has regressed — read `logs/daemon.log` first, before
believing any case that ran after it.

The other half of R1 is the client. A daemon can be alive and its socket fine
while the GUI never gets past "connecting", and the reason is only in the
Wayland session's `application.log` (under the rig's `run/rho-wayland/<name>/`).
That is where a store-schema mismatch shows up: redb records the Rust path of a
table's value type, so a crate rename makes the client panic with
`TableTypeMismatch` on a table the daemon is perfectly happy with. Found on the
rig by eng-b8os during the map cut, which no unit test would have caught. If
the GUI says "connecting" and the daemon is up, read that log before anything
else.

*Closed by:* 6 Sep 2026, the fix itself; `DatabaseAlreadyOpen` reproduced and
then gone across four `rig up` cycles on `desk`.

### R2. The GUI has a Slack session

A client with no session shows every Slack row the desk ever held as Open, and
no rule can close them. The fake is that session. `rig up` must refuse to start
the GUI when the fake did not come up as the workspace, rather than start it
anyway — a GUI in that state looks exactly like a dealing bug.

*Closed by:* not yet — the refusal is not built.

### R3. The Slack mirror is the user's, not QA's

`slack.redb` in the snapshot taken 6 Sep holds only `acme`: 5 conversations,
211 messages, which is the *fixture's* workspace written there by earlier QA
runs. The user's real Slack state lives on their device with the GUI, not on
this machine. Until a snapshot carries the device's mirror, every Slack case
below runs at fixture scale and proves rendering, not flood behaviour. Say so
in the run rather than reporting a green flood case.

*Closed by:* not yet — waiting on the device's GUI state dir.

## The cases

### C1. Dealing after a restart

*Why it is tricky.* Everything the dealer knows is rebuilt on start: the mirror
catches up from the journal, cards are made again, the ranking is applied. A
card that is dealt only because of something held in memory disappears, and a
card that is dealt from stale state comes back after being answered.

*Run.* Note the top three rows of Home and their reasons. `rho-qa rig down
desk`, then `rho-qa rig up desk`. Wait for the GUI to settle (the application
log says it is following the host). Screenshot Home.

*Passes if* the same rows are in the same order with the same reasons, minus
anything whose deadline passed while it was down. *Fails if* a row appears that
was answered before the restart, or an expected row is missing, or the order
differs with no fact behind the difference.

*Closed by:* —

### C2. A verdict on a Home row

*Why it is tricky.* The verdict lands on the card the transient was opened
over, which is the row under the point and not the surface in view. Getting the
wrong subject is silent and looks like the verdict was ignored.

*Run.* Move the point to a Home row that is not the first. Tap `shift` to open
the verdict transient. Press the verdict key. Screenshot before and after.

*Passes if* the verdict lands on the row the point was on, that row leaves or
changes reason, and no other row moves. *Fails if* the first row takes it, or
two rows change, or the row stays with no echo line saying why.

*Closed by:* —

### C3. A Slack thread marked read that comes back

*Why it is tricky.* There are two cursors: Slack's own, which is the truth for
reading, and Rho's `SlackHandledThrough`, which is the dealing cursor only. A
thread marked read here must write back to Slack's cursor, and a thread the
dealer has handled must not re-deal on the next poll.

*Run.* Open an unread thread in the rig's Slack, read to the end, leave it.
Note the conversation. `rho-qa rig down desk` and `rig up desk`. Look at Home
and at the conversation list.

*Passes if* the thread is not unread and is not dealt again. *Fails if* it is
back as a card, or shows unread after being read, or is read here but the fake
never received a `conversations.mark`.

*Closed by:* —

### C4. An agent that stops appearing

*Why it is tricky.* The desk has ~2,800 agents; an agent that falls out of an
index rather than out of the store is invisible and there is nothing on screen
to say so. This is the case that most needs the real snapshot: it does not
reproduce at fixture scale.

*Run.* Pick an agent visible on Home. Note its id. Give it a verdict, or let it
finish. Find it again by id, by title, and from the map.

*Passes if* all three find it and agree on its state. *Fails if* any one of
them cannot, which localises the bug to that index.

*Closed by:* —

### C5. Creation into a managed workspace

*Why it is tricky.* Creation decides which host an agent lands on and refuses a
cross-host base and workdir; the workspace it creates is a real jj workspace on
disk. The rig must never create into the user's own source tree.

*Run.* Use the fixture repo at `/home/maan2003/src/rho-qa-rig` as the base.
Create an agent into it from the GUI. Check `jj workspace list` in the fixture.

*Passes if* the workspace exists in the fixture, the agent's workdir label
points at it, and nothing was written under `/home/maan2003/src/rho` or any
other real checkout. *Fails if* anything is created outside the fixture — stop
and report immediately, this one can damage the user's work.

*Closed by:* —

### C6. Find an agent by what the user last said to it

*Why it is tricky.* The ranking is over the names an agent also answers to and
how recently it was used, not over its title alone. At 2,800 agents a scorer
that is nearly right returns something plausible and wrong.

*Run.* Pick an agent the user gave a distinctive instruction to. Open Find,
type a phrase from that instruction.

*Passes if* the agent is in the first three hits and the row says why it
matched. *Fails if* it is absent, or present with no reason, or ranked below
agents with no relation to the phrase.

*Closed by:* —

### C7. Shift held versus tapped

*Why it is tricky.* A tap opens the verdict transient; a hold is a modifier for
whatever comes next; a second tap reaches Home. The difference is 300 ms and a
release, and the virtual keyboard makes it easy to send something that is
neither.

*Run.*

```sh
rho wayland --session desk input down:shift wait:100 up:shift          # a tap
rho wayland --session desk input down:shift wait:600 key:d up:shift    # a hold
rho wayland --session desk input down:shift wait:100 up:shift wait:150 down:shift wait:100 up:shift
```

*Passes if* the tap opens the verdict transient, the hold sends `shift-d` and
opens no transient, and the double tap reaches Home. *Fails if* a held shift
opens the menu, or the menu swallows the letter typed while it is held.

*Closed by:* —

### C8. The map after a reparent

*Why it is tricky.* The store is a DAG across hosts. A reparent changes an
edge; the map's index and any open note view have to agree afterwards, and a
cached subtree is easy to leave behind.

*Run.* Open the map, reparent a cell under another, then walk from the new
parent to the child and back. Restart the rig and walk it again.

*Passes if* the child appears under the new parent and nowhere else, before and
after the restart. *Fails if* it appears in both places, or the old parent
still counts it, or the walk differs after the restart.

*Closed by:* —

### C9. A block under the point moves the rows below it

*Why it is tricky.* Anything the GUI draws into a buffer — a transient menu, a
refusal, an inline note — is an editor block, and a block is only measured and
given rows when it has a height to start from (`Block::has_height` is
`height.is_some()`; with `None` the editor resizes nothing and the block keeps
zero rows forever). A block with no height still paints, on top of whatever is
below it. Every unit test passes: the point did not move, the block is in the
editor, the strip is empty. Only a picture shows the row underneath being
covered instead of pushed down.

The half of this that hides the defect: a block that is *last* in its buffer
looks the same either way, because nothing is under it to cover. The draft's
refusal is one of those — it sits at the end of the body, and the attachment
chip that shares its anchor has the lower priority, so it takes the row above.
Two builds of `refusal_block`, one with `Some(1)` and one with `None`, driven
the same way, produce byte-identical screenshots. So a block with no height is
a defect waiting for a neighbour, and the way to find it is to read the code
for `height: None`, not to wait for a picture.

*Run.* On the desk rig, open each thing that draws under the point — the
verdict transient on a Home row, a refused draft with an image attached — and
screenshot the frame. Where a picture has to settle it, build the same drive
twice, once with the block's height forced to `None`, and compare the two PNGs
byte for byte.

*Passes if* the row that was under the point before is still readable below the
block, moved down by the block's height, and no `BlockProperties` in
`rho-window` asks for `height: None`. *Fails if* the block is painted over the
row below, or the rows below shift by fewer rows than the block drew.

*Closed by:* the verdict transient (main 8601e048) and `style::refusal_block`.

## Does it feel like Emacs

From the user's ruling. These run over whatever case you were already running —
they are not separate sessions, they are what you check while doing C1 to C9.

### E1. The point survives back

Move the point somewhere non-obvious in a buffer, go to another surface, come
back. *Passes if* the buffer is as it was with the point where it was. *Fails
if* the point is at the top, or the scroll position was lost.

### E2. The same key means the same thing everywhere

Take a key with an obvious meaning in one buffer and press it in three others.
*Passes if* it means the same kind of thing in each, or is unbound. *Fails if*
it means something unrelated in another buffer — that is the bug, not a
convenience.

### E3. Nothing needs the mouse

Do the whole case with `key` and `input` only. *Passes if* every step is
reachable. *Fails if* anything can only be done by clicking; record what.

### E4. No modal appears

*Passes if* every question came through the minibuffer and every answer through
the echo line, and the buffer behind stayed readable. *Fails if* anything
blocked the window or dimmed what was behind it.

### E5. A transient takes one key and closes

Open a transient, press one key. *Passes if* it runs and the transient is gone,
the point unmoved. An item declared an infix may stay — that is the only
exception, and it must be declared. *Fails if* an ordinary item leaves the menu
up, or the point moved. (Pending: the transient buffer is not built yet; until
it is, this checks today's bottom-strip menu.)

## Scale proofs

The cost rule, from the user: **per event O(touched) + O(log n), per frame
O(drawn)**, in every crate, proven on the user's snapshot, with the numbers in
the landing note. This section is how to get those numbers without the dial9
dance.

*Which snapshot.* `user-2026-09-06` or newer: 42.8 GiB in seven databases,
2,583,116 rows across 55 tables. A number from a smaller state proves nothing about the rule —
the rule is about growth, and growth only shows at size. Say which snapshot in
the note.

*What the rig already writes.* Every `rig up` runs the GUI with the profiler
on, and `rig down` leaves three files in the rig's `profiles/`:

- `<name>.bin.frames.json` — per frame `draw_ns`, `prepaint_ns`, `paint_ns`,
  `finish_ns`, `dirty_to_draw_ns`, `invalidations`, and a summary with
  mean/p50/p95/p99/max for each. This is the per-frame side.
- `<name>.bin.editor.json` — per stage `count`, `duration_ms` percentiles and
  **`input_rows`**. `input_rows` against `duration_ms` is the per-event side:
  it is the evidence that work is O(touched) and not O(all).
- `<name>.0.bin.gz` — the CPU profile, for when the two above say something is
  wrong but not where. It is symbolized where it was written — the frames in it
  carry Rust names, not bare addresses — so reading it needs the trace decoder
  and not the binary it came from. `dial9 serve --local-dir .` opens the whole
  thing when you need the flame graph.

`rig down` reads all three and prints the line the run earned: frames drawn,
`draw_ms` p99, how many frames went over the 8 ms budget, the worst
dirty-to-draw gap and its p99, the editor stage with the worst p99 and the rows
it had in hand, and where the GUI thread's samples landed. The same line goes
into the rig's session entry in `rig.json`, so a landing note quotes the run
instead of re-deriving it. `rho-qa profile <name>.bin` prints it again for any
session, including an old one.

*What to measure for a new screen or rule.*

1. Open it on the snapshot, do the thing it exists for twenty times, `rig
   down`.
2. From `frames.json`: `draw_ms` p99 and max, and `dirty_to_draw_ms` p99 — the
   second is the one the user feels, because it is the wait between something
   changing and the pixels moving.
3. From `editor.json`: for each stage the change touches, `duration_ms` p99 and
   the `input_rows` beside it.
4. Repeat on a snapshot half the size, or with half the rows in view. Both
   numbers should move with what is drawn or touched, not with what exists.

*A worked example of measuring off the screen.* Not everything slow is
measured in frames. The transcript of the desk's largest agent wraps in
7,122 ms at 121,252 rows, and display elisions do not help it: measured
offline against a copy of the rig's own client mirror with the crate's own
planner — 12,348 blocks, 121,114 rendered rows, 697 elision plans covering
118,403 rows, 97.8 per cent — the elisions sit in the block map, which is
above the wrap map, so all 121k rows are laid out and then 98 per cent of them
are hidden. Two habits to copy from it: measure the layer that does the work
rather than the one that shows it, and copy the mirror before reading it,
because opening a rig's live mirror takes a write transaction on it
(eng-b8os, the transcript cut).

*What fails it.*

- `draw_ms` p99 above 8 ms, or max above 16 ms: a frame that misses at 60 Hz.
- `dirty_to_draw_ms` p99 above 50 ms: a visible lag between act and paint.
- A stage whose `duration_ms` grows with the snapshot while its `input_rows`
  does not: that is O(all) wearing O(touched)'s clothes, and it is the failure
  the rule exists to catch.
- Any main-thread sample inside ingest, dealing or the store in the CPU
  profile: the main thread does nothing but draw.

These thresholds are the current bar, not physics. Move them with a reason and
say so here.

## Adding a case

A new case starts every time the user reports something. Write it before
fixing: the case is what says the fix worked, and the list of cases is the only
record of what has been proven. Give it the next number in its section, say why
it is tricky in one paragraph, give the exact steps, and leave "Closed by"
empty until a run fills it in.
