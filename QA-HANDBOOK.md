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
rho-qa snapshot --gui-state ~/state-from-the-laptop  # a second device's GUI half
rho-qa rig new --from user-2026-09-06 --name desk   # once; it refuses to overwrite
rho-qa build --binaries profiling         # the binaries a rig runs
rho-qa rig up desk                        # daemon, fakes, headless GUI, profiler
rho-qa rig down desk                      # stop; the state stays as the run left it
```

`rig up` ends by printing the line that drives the session it just started. It
refuses when the rig is already up, naming the session, whoever started it and
the daemon pid holding it; `--take` stops what is running and takes it. Two
agents drove the same desk twice in one evening, seconds apart in both
directions, so the refusal is not politeness — an overlapped run's numbers are
noise and neither party can tell from the screenshots.

Driving the GUI, with the rig's own runtime dir — or, better in a script,
with `--state-dir`, which names the session's directory outright and needs no
environment at all:

```sh
rho wayland --session desk --state-dir /home/maan2003/src/rho-rigs/desk/run/rho-wayland key "space"
export XDG_RUNTIME_DIR=/home/maan2003/src/rho-rigs/desk/run   # where the session lives
rho wayland --session desk key "ctrl+shift+p"                 # a chord
rho wayland --session desk input down:shift wait:400 up:shift # a held shift
rho wayland --session desk screenshot --output /tmp/case.png
rho wayland --session desk click 200 16                       # a tap, in logical pixels
rho wayland --session desk tree                               # the window tree
```

`click` and `move` are a real pointer, not sway's cursor commands: a headless
seat has no pointer device, so `seat seat0 cursor press` answers
`success: true` in the ipc and reaches no client. The driver creates a
`zwlr_virtual_pointer_v1` for the length of the tap, the way `wtype` creates a
virtual keyboard for the length of a keystroke, and `swaymsg -t get_seats`
shows the device appear and go: `capabilities: 0` with no devices at rest, `1`
with a `wlr_virtual_pointer_v1` while a tap is in flight. Coordinates are
logical pixels read off a screenshot, checked against the output's size *now*
rather than the size in `session.json`, which is the size the session started
at and wrong on any session that has been resized — which is every session the
phone is driven on.

That is what makes the phone drivable at all: its sheet opens from the ☰, the
deal card's header or the feed header, and none of them has a key. Keys do
arrive at phone width, so a screen that ignores one is ignoring it; a tap that
appears to do nothing is a different question, and the cursor answers it. Grab
the frame with the cursor in it — `grim -c` against the session's runtime dir
— and look at the shape: an I-beam over text and a hand over something
clickable is the client hit-testing what is under the pointer, which means the
tap landed and the screen chose to do nothing with it. That is how the phone's
first tap was found to be opening a menu nothing drew.

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
- **Its own binary.** `rho-qa build` builds `rho-qa` with the rig's binaries,
  so the tool that reads a run is never older than the run. It did not, once,
  and a stale copy wrote no summary line into desk sessions 23 to 25 while
  looking like a rig fault.

Three rules for every run.

1. **The desk is never reset.** `rho-qa rig new` refuses to overwrite one.
   State accumulates across sessions the way the user's does; a case that needs
   an empty desk is not a case, it is a unit test.
2. **Nothing touches the user's live state.** The live directory is read from
   by `rho-qa snapshot` and by nothing else, ever. Two allow lists say what a
   snapshot may copy — the daemon's half and, when `--gui-state` names another
   device's client directory, the GUI's half — and neither has ever named a
   credential: no `auth.d`, no `iroh-secret.key`, no `sessions`. A rig daemon
   is its own node. `rho-qa rig new` lays the GUI half over the daemon's state
   when the snapshot has one, because a rig runs one state directory.
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

No rig ever had one. `rig up` wrote `credentials.json` for a workspace it
named itself, `rig`, while the fake comes up as `acme`; the store is keyed by
workspace name, so the lookup missed and the client ran with no session on
every rig that has ever existed. Nothing said so: the fake was listening, the
daemon was up, the GUI came up and dealt, and the Slack rows simply never
closed. Any Slack row count taken on a rig before 6 Sep was taken without a
session.

The shape of the bug is worth more than the fix. A name written on both sides
of a lookup by different code is not checked by anything — the writer is
happy, the reader is happy, and the only symptom is a screen that looks
plausible. So the rig no longer names the workspace: it reads the name out of
what the fake printed and writes credentials for that, and if the fake never
printed one it refuses to start the GUI and says why.

*Closed by:* 6 Sep 2026, desk sessions 26 and 27. 26 printed ``slack   fake on
http://127.0.0.1:46871/api as workspace `acme` `` and left `credentials.json`
keyed by `acme`; 27 put the point on `#design › @Manmeet can you look at the
deploy before the release?`, took `d` from the verdict transient, and the row
took the verdict, left `next`, and was replaced by the next row of the flood.

### R3. The Slack mirror is the user's, not QA's

`slack.redb` in the snapshot taken 6 Sep holds only `acme`: 5 conversations,
211 messages, which is the *fixture's* workspace written there by earlier QA
runs. That is what a daemon-side copy is: the mirror is the client's file —
`rho-slack`'s session writes every arriving message into it under the state
directory of whichever device ran the GUI, and the daemon never touches it —
so the user's real Slack state is on their device, not on this machine.

The way to carry it is `rho-qa snapshot --gui-state <that device's state
dir>`: the mirror comes over on the GUI half and `rig new` lays it over the
daemon-side copy, so the rig reads the user's flood rather than the fixture.
Until a snapshot is taken that way, every Slack case below runs at fixture
scale and proves rendering, not flood behaviour. Say which one the run had
rather than reporting a green flood case.

*Closed by:* the flag exists; open until a snapshot of the user's own device
has been taken with it and a Slack case has run at flood scale.

### R4. The binary you are about to run is the one you built

A build that hits ENOSPC can leave a **truncated binary that cargo will not
replace**: the stale artefact is exactly the size cargo expects, so a rebuild
reuses it and reports success. The symptom is `Permission denied` from a file
that is `-rwxr-xr-x`, owned by you, on a filesystem mounted `rw` — with an
intact ELF header, because the head of the file was written before the disk
filled. `cp` reads it happily; only `execve` fails.

Touch a source file in the crate to force a genuine relink. If the rebuilt
file has the byte-identical size, cargo did not relink and the artefact is
still the bad one. It cost an hour on 2026-09-07 and was diagnosed as
everything but what it was.

The related trap is measurement. **On bcachefs, `df` immediately after a
delete is meaningless**: reclaim is asynchronous and free space kept climbing
for about eight minutes after each delete on the shared loop device (3.2 GB,
then 29, 96, 287). A deletion judged by the `df` that follows it will be read
as having freed almost nothing. Wait, or measure something else.

The other way a binary is not the one you built is the linker. On 2026-09-07
rho-gui's test binary crossed ~1.149 GB and stopped starting at all: SIGSEGV
before the first test, `--list` printing nothing. The cause was wild 0.10.0,
named by store path in the flake's shellHook — and the one-line check, from
eng-bgkw, is worth more than the story:

```
readelf -lW BIN | awk '$1=="DYNAMIC"{print $2}'
readelf -SW BIN | awk '$2==".dynamic"{print $5}'
```

Those two must be the same number. When they drift the loader reads the
dynamic array at the program header's offset, finds zeros, stops, and the
program never reaches `main`. The drift starts after the NOBITS `.tbss`, so
that is where to look if a future toolchain does it again. Fixed by moving to
mold; note that a shell started before that change still has the old linker's
store path in `CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS`, and a
target-specific RUSTFLAGS variable shadows `.cargo/config.toml` outright.
`direnv exec . cargo …` picks up the new one without restarting anything.

The same day gave the other half of that: `du` on a rig's state reports the
snapshot's full size because `rig new` clones with `cp -a --reflink=auto` and
`du` counts shared extents once per directory it walks. `filefrag -v` shows
the truth — the rig's file and the snapshot's at identical physical offsets,
flagged `shared`. A rig costs its divergence from the snapshot, not the
snapshot. Deleting rigs to free space frees very little; retiring the
snapshot is what frees the 43 GB, and only once no rig points into it.

### R5. An instrument can be faithful and still blind

The rule already here is to give the harness a question whose answer is known.
This is the sharper form of it, and it is eng-b8os's, from the fold cut: **an
instrument can measure correctly and still be incapable of seeing the fault.**
Their per-row invariant check was right about every row it examined and could
never have caught a row four billion columns wide, because every column in
such a row is inside it. The check passed, faithfully, and meant nothing.

So: **the check that "passes" is the one to distrust when you have not first
shown it can fail.** Before believing a green instrument, make it go red on
purpose — a deliberately wrong reference, a known-bad input, a fault you have
planted. The fold fix's zero rope errors was worth reporting only because the
same grep found 1,440 of them on the session before it. The inlay-map seek was
proven by a negative control in which a deliberately wrong reference failed
148 tests, which is what established the suite reached the function at all.

Two ways this has bitten in one week, both producing green:

- **Measuring nothing.** A cost test that fired a model event and asserted the
  map was not composed passed on the *unfixed* code, because the event was
  deferred to `on_next_frame` and the test never drew one. Every assertion held
  trivially over work that never happened. The fix is to assert first that the
  thing happened at all — a row was drawn again — before asserting what it cost.
- **Measuring faithfully the wrong thing.** The per-row check above.

The first is caught by a known-answer check. The second is only caught by
asking what the instrument *cannot* see, which is a question worth writing down
beside every new check.

### R6. The rig says which binaries it launched

`rig up` prints, and writes to `logs/rig.log`, the tree commit and every
binary's content hash, mtime and path. It **refuses to start** when any binary
is older than the newest file under `crates/`, `vendor/` or `Cargo.lock`.
`--allow-stale-binaries` overrides it, prints `STALE` on the ready line, and
records `stale_binaries` in the session so `rig status` says so afterwards.

This exists because on 2026-09-07 five consecutive sessions ran a GUI binary
three hours older than the tree — a rebuild had picked up `rho-cli` and
`rho-daemon` and not the GUI — and were reported as a commit that was never in
them. It withdrew a crash result and a whole table of frame numbers, including
one already sent onward. Nothing had ever checked, on a rig whose entire
purpose is numbers over commits.

The scoping to source that can affect a binary is deliberate: a doc-only edit
must not make every binary look stale, or the override becomes habit and the
check becomes noise.

Two rules follow, and they are the reason the check is not enough on its own:

- **A commit is a claim about the tree, not about the binaries.** Never
  attribute a number to a commit without the identity line from the same
  session that produced it.
- **A report carries the drive's name and step count beside the commit.** One
  commit produced 16%, 79% and 4.9% of frames over budget on three different
  drives; without the drive named, a table of such rows reads as a trend and
  is not one.

### R7. The drive is named, and its steps are counted

The second rule above is now the rig's job rather than the reader's. The
driver writes one line per thing it does — every `key`, `input`, `type`,
`click` and `move` — to `<session>-drive.log`, beside the wayland session
directory rather than inside it, because `stop` removes the directory and the
log is the part that has to outlive the run. `rho wayland --session <s> drive
"<name>"` names the drive that follows; the steps after it are counted against
it, and a later name starts the count again.

`rig down` reads that log, prints the drive and its step count, keeps the log
in `logs/` under the profile's own stem, and stores both on the session so
`rig status` says them afterwards. **A run with no drive named is reported as
having none**, in words, rather than as a blank: a number nobody can attribute
to a recipe is a number nobody can compare, and the report says so instead of
letting the row sit in a table looking like the others.

What this does not do is tell you two drives are the same drive. It counts
steps; it does not compare them. Two runs of "the 09:12 recipe" with different
step counts are two different drives whatever they are called, and the count
beside the name is what makes that visible.

**It used to not tell you a rig was idle.** A session nobody ever drove and a
session someone is using looked identical from the outside — same processes,
same directory, same `rig status`. One was found on the desk host with sway,
the profiling daemon and fake-slack up for 2h19m, and the only thing that
distinguished it from a live session was that the newest screenshot was from
the day before. That thread was thin because nothing read it.

`rig status` and the `rig up` refusal now read it. Both print a `touched`
line: **`last driven 2h19m ago (key j)`**, from the newest of the drive log's
last step and the newest screenshot, or **`never driven; nothing has been sent
to this session`** for a rig that has been standing since it came up. So the
refusal names the holder *and* says whether they are on it, which is the
question the next person actually has.

What it still cannot see is a reader looking at a screen without pressing
anything. Two minutes of that is indistinguishable from two minutes of
nothing, so **say in the session notes when you take a rig and when you are
done with it** — the rig now says a great deal more than it did, and not
that.

### R8. A binary that will not start: read its linker before anything else

If a rho-gui binary dies before the loader says anything — no panic, no
message, nothing from `main` — **check which linker built it, first, with one
command**:

```
strings -n 8 <binary> | grep '^Linker:'
```

If it says `Wild 0.10.0`, that is a known defect already written down in this
tree and nothing to do with your code. `.cargo/config.toml` describes it in
the comment above `[target.x86_64-unknown-linux-gnu]`, and GUI-CRATES-DESIGN
has it under "The linker, not the size": wild lays a big binary out so that
PT_DYNAMIC's `p_offset` lands sixteen bytes before the bytes of `.dynamic`, so
the loader stops on the first entry and the program never reaches `main`. It
is **the layout, not the size** — the binary that starts can be the larger of
the two. Confirm without running anything: `ld.so --list <binary>` faults on
its own, execve, brk, one mmap, then a fault at address 0x8.

**The trap is that this differs per shell, so "it works for me" proves
nothing.** `.cargo/config.toml` names mold, but a target-section rustflags is
*replaced*, not merged, by
`CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS`, which the devshell exports.
Two engineers on the same commit found different values in that variable on
the same afternoon — one mold, one wild — and only the wild one saw the
failure. Check your own before concluding anything about a tree:

```
echo $CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS
```

Two things not to try. The committed `-Clink-arg=-fuse-ld=mold` does **not**
work with the clang wrapper in this shell (`invalid linker name in argument`),
so the config as written would not save you if the variable were unset; the
working form is `--ld-path=<absolute path to mold>`. And appending flags with
`cargo rustc` does not beat the variable's own `--ld-path` — the variable
itself has to change, which is a devshell fix and not a code fix.

Found and diagnosed by eng-bgkw, who also corrected the first version of this
rule: it was written as "the workspace build and the per-package build are two
different artifacts, only one of which will not start". That framing was
wrong. The per-package binary flipped to failing as soon as another small edit
changed the layout, and the split was never the cause — which is itself R9,
one artifact standing in for the property being measured.

### R9. The measurement that comes to hand is not the measurement of the thing

Five of these happened in one afternoon, by three engineers, none of whom
noticed it was the same mistake until the fourth:

| what was read | what it was |
|---|---|
| `du` said 2.1T of cargo cache on a 1.4T device | `du` counts a reflinked extent once per file pointing at it, so it was not a space figure at all |
| `du -sh ~/.cargo` said 0 | `~/.cargo` is a **symlink** onto the build device; `du -sh` measured the link |
| a sweep's output file was empty and the disk had just filled | the run had **finished, exit 0, with results** — the whole run was piped through `awk`, which writes nothing until the pipeline ends |
| `df -h /` said 616G free while builds failed `No space left on device` | the build directory is a **separate loop device**, 100% full |
| a sweep counted markers with `grep -c '^SWEEP-ACCOUNTING$'` and found one | under `--nocapture` the harness prints `test tests::foo ... ` **with no newline**, so a test's own stderr continues that line and no anchored pattern sees it. Unanchored, the same file: four |

The fourth one is the one to lead with, because it is the only one with a
number on both sides: `du` said the desk rig was 64G, `df` before and after
deleting it said it returned **19.8 GiB**. An overcount of 3.2x, measured. An
argument that a figure might be wrong is worth much less than a figure that
was wrong by 3.2x and a second figure that says so.

The fifth is the sharpest, because the second instrument was not a different
tool at all — it was **the same log, read without the anchor**. One character
of regex stood between a right answer and a wrong one, and the wrong one was
used to retract a finding that was correct. The three tests it wrongly cleared
fail on main today.

It carries a second lesson the other four do not. The measurement that would
have settled it outright was to run the three tests and read the assertion,
which takes twenty seconds; instead a whole experiment was built on top of the
bad read. **When a direct measurement of the thing is available and cheap,
take it before building an experiment that infers it.** The experiment's own
result was true and proved nothing, because the other half of the comparison
had never been established directly.

**The rule is not "distrust your measurements", which nobody can act on. It
is: when a measurement is load-bearing, take a second one of a different kind
before you report it.** Every one of the five above had a second instrument
available that would have answered directly and cost seconds — `df` beside
`du`, `ls -l` beside `du -sh` on any path that might be a link, an exit code
or a `wc -l` beside an empty output file, the same grep without its anchor,
`bcachefs fs usage` beside all of them. Three minutes would have bought all
five.

Two specific corollaries worth naming, because they recur here:

- **On this filesystem, no `du` figure is a space figure.** jj workspaces are
  copy-on-write, so build artefacts share extents. The only way to learn what
  a candidate is worth is to delete it and re-read `df` — which is an argument
  for deleting one thing at a time, not for deleting the biggest `du` line.
- **The healthy-looking number is the one a person reaches for by reflex.**
  Nobody runs `df` on the build directory; they run it on `/`, see a large
  number, and go looking in the code for an hour. That is what made the loop
  device cost an afternoon rather than a minute.

Credit: eng-b8os found the reflink cause and the reflex point and set the
"second instrument of a different kind" rule; the 3.2x measurement is
eng-8gpr's.

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

*And a control, because the desk moves on its own.* Byte for byte is a proof
only when the buffer is the only thing that could have changed, and on a desk
with agents running it is not: a mirror row un-dims while you are pressing
keys, and that is a real difference in the picture that has nothing to do with
what you did. Take a no-input control — two frames the same span apart with no
keys between them — and read the round trip against it. If the control is
byte-identical, a difference in the round trip is yours; if the control moves,
the difference is only yours where the control did not move. Proving the root
menu's back on session 40 went that way: two of three open-and-dismiss round
trips byte-identical, the third differing by 1,167 pixels which were all one
agent's name in the running list going from muted to normal, and the control
byte-identical across the same span. Without the control that third frame is
either a defect or nothing, and there is no way to tell.

*Passes if* the row that was under the point before is still readable below the
block, moved down by the block's height, and no `BlockProperties` in
`rho-window` asks for `height: None`. *Fails if* the block is painted over the
row below, or the rows below shift by fewer rows than the block drew.

*No longer covers the transient.* On the user's ruling the menu left the
buffer: it is drawn at the bottom edge of the window, over the surface, so
there is no block and nothing below it to cover. What replaces the check for
the transient is the opposite one — the surface draws the same number of rows
with the menu open as without, which is what
`the_root_menu_opens_at_the_bottom_and_escape_retraces_it` asserts. The case
still covers every other thing drawn into a buffer: the draft's refusal and
attachment chip, the usage chart, a Slack image, a transcript gap, a
visualization.

*Closed by:* `style::refusal_block`. (It was closed by the verdict transient,
main 8601e048, until the transient stopped being a block.)

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
on, and `rig down` leaves four files in the rig's `profiles/`:

- `<name>.bin.frames.json` — per frame `draw_ns`, `prepaint_ns`, `paint_ns`,
  `finish_ns`, `dirty_to_draw_ns`, `invalidations`, and a summary with
  mean/p50/p95/p99/max for each. This is the per-frame side.
- `<name>.bin.editor.json` — per stage `count`, `duration_ms` percentiles and
  **`input_rows`**. `input_rows` against `duration_ms` is the per-event side:
  it is the evidence that work is O(touched) and not O(all).
- `<name>.bin.work.json` — main-thread work that happened **outside any
  frame**, per span and summarised per owner: `count`, `duration_ms`
  percentiles, `work_units` and the owner's total. The frame log accounts for
  time inside `Window::draw` and nothing else, so the work that makes the
  *next* frame late is in neither of the two above. This is the file that
  answers "what did one event cost the main thread", and its `work_units` is
  to a span what `input_rows` is to a stage: an owner whose span cost follows
  the desk rather than what the event named is O(all) again, wearing a
  different disguise. Until 2026-09-08 the ring behind it was readable only
  through a telemetry report the user sent, which is not something a rig can
  produce, so no rig run could see this at all.
- `<name>.0.bin.gz` — the CPU profile, for when the two above say something is
  wrong but not where. It is symbolized where it was written — the frames in it
  carry Rust names, not bare addresses — so reading it needs the trace decoder
  and not the binary it came from. `dial9 serve --local-dir .` opens the whole
  thing when you need the flame graph.

`rig down` reads all four and prints the line the run earned: frames drawn,
`draw_ms` p99, how many frames went over the 4 ms budget, the worst
dirty-to-draw gap and its p99, the editor stage with the worst p99 and the rows
it had in hand, the costliest owner of the work between frames with its spans,
total and percentiles, and where the GUI thread's samples landed. The same line goes
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
   the `input_rows` beside it. From `work.json`: for each owner the change
   touches, `duration_ms` p50 and p99 against `work_units`. A change to how an
   event is answered — a map patched instead of rebuilt, a source read instead
   of walked — shows up in the second file and in no other, because none of it
   happens inside a frame.
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

*Say which open a number came from.* A first open and a second open of the
same screen are different measurements: the first pays for what has never been
laid out and the second reads what is already there, and the gap between them
is large enough to swallow whatever you were trying to show. A number that
does not say which one it is cannot be compared with anything, including
itself a day later. So: name the order in the note — "first open, then `gg`,
on the same drive" — and drive both sides of a comparison the same way.

The same sentence covers what a session *is*. `rig.json` lists sessions and
they all look alike, but a session can be a control rather than a measurement —
main's binaries, or a build with the thing under test deliberately switched
off — and read as a measurement it says the opposite of what it means. Two
sessions minutes apart were once taken for a before-and-after when one of them
was a control on a different build (eng-b8os, desk sessions 29 and 33). Say in
the note, and in the message when you hand numbers to someone else, which
sessions were controls and what they are controls for.

*A slow stage with nothing against it is not a fast stage.* `input_rows` is
what makes O(touched) checkable, so a stage line with a large p99 and `0 rows`
beside it is not evidence of cheap work on nothing: it usually means the
expensive thing inside that stage has no stage of its own, and its cost is
being billed to whichever stage happened to be on the stack. Desk session 45
is the worked case — `multi_buffer_sync p99 104.32 ms at 0 rows`, which no
amount of reading multi-buffer syncing explains, because what the session
actually did was two resizes, and on main the whole-buffer rewrap they cause
was not a stage at all. eng-b8os measured it over desk sessions 43, 44 and 46
through 55, two drives a side and three runs of each, and the fix was to name
it: since `wrap_map_rewrap` exists, a resize's cost has its own line and its
own row count instead of hiding under a neighbour. So when a stage's p99 is
large and its rows are zero or implausible, do not report the stage — find
what ran inside it and give that its own stage first, then measure.

*A run that wrote a profile is not a run that did the thing.* The driver
finds the session under the rig's own runtime dir, so `rho wayland` from a
normal shell with neither `--state-dir` nor `XDG_RUNTIME_DIR` set finds no
session and the keystrokes go nowhere — and the run still comes up, still
profiles, still prints a session line that reads like a good result. Desk
session 66 is the worked case (eng-b8os, the rewrap cut): 312 frames, draw p99
2.4 ms, and no drive behind any of it. What gave it away was the row count,
not the stage name: 2114 rows touched across a whole run whose point was a
262,000-row transcript being composed. A run that did the thing has a row
count you could have predicted before it started, so predict it, and read it
first. Two habits from the same afternoon: drive with `--state-dir` rather
than an exported environment a subshell may not carry, and `trap` the `rig
down`, because session 64's `set -e` — its first keystroke failed outright
with "session is not available" — left the rig up and the runs behind it
refused as already held.

The batch is the other half of the lesson: it failed three different ways and
only the silent one produced a number. 64's keystroke failed loudly, runs in the same batch
refused loudly as already held, and 66 came up, drove nothing and wrote a
clean-looking profile. Loud failures cost minutes.
The quiet one is the one that gets reported.

The general form, and it is not only about profiles. *Predict the number or
the state you would get with the mechanism removed, and check that you do not
get it.* Two cases from the same day. eng-b8os believed the 144 rows laid out
by the shipped fill only after forcing the fill back to tail-first and
re-running the same assertion, which gave 961 — the contrast is the evidence,
not the 144. And a workspace test of mine asserted that back never lands on a
transcript whose daemon was detached; it passed, and it passed again with the
`forget` call it was meant to be testing commented out, because an agent's
context dies with the agent and there was no stack left to walk. A test that
passes with its mechanism removed and a profile written by a run that never
drove are the same failure: they look like evidence and cost nothing to
produce.

*What fails it.*

- Any frame above 4 ms, on any surface: the budget is 4 ms and the bar is
  zero over it. (It was 8 ms until the user set it to 4; reports printed by
  `rho-qa telemetry` carry the old count beside the new one for one release
  so an old report still compares.)
- `dirty_to_draw_ms` p99 above 50 ms: a visible lag between act and paint.
- A stage whose `duration_ms` grows with the snapshot while its `input_rows`
  does not: that is O(all) wearing O(touched)'s clothes, and it is the failure
  the rule exists to catch. The same reading applies to an owner in
  `work.json` against its `work_units`.
- Any main-thread sample inside ingest, dealing or the store in the CPU
  profile: the main thread does nothing but draw.

These thresholds are the current bar, not physics. Move them with a reason and
say so here.

## Reading a telemetry report

When the user says they sent telemetry, the report is a
`dev.rho.gui-performance-snapshot` JSON file and reading it by eye gets three
things wrong. `rho-qa telemetry <path>` exists so nobody has to.

```
cargo run -q -p rho-qa -- telemetry /tmp/rho-telemetry/<report>.json
```

*What it prints.* The frames broken out **by surface**, because slow is almost
never the whole GUI and the aggregate hides which window is paying; the editor
stages set against the window they were actually measured in; and the CPU
profile decoded, as a leaf leaderboard (what the thread was in), an on-stack
leaderboard (what it was under), and the callers of the top leaf.

*The three traps it exists to avoid.*

- **The stage ring is not the frame span.** `editor[]` is a 4,096-record ring.
  On a real report the frames cover minutes and the ring covers seconds, so a
  stage total and a frame total are not comparable and a stage that looks small
  against the whole session may be most of the window it was sampled in. The
  reader prints the ring's own span beside its totals for this reason.
- **A stage inside a draw is not a stage between draws.** The same stage name
  means different things depending on whether it ran under the frame or on a
  model event outside one, and only the second is invisible in the frame
  numbers. Attribute by `start_ns` against the frame spans, not by name.
- **The CPU profile is sixteen traces, not one.** `cpu_profile.segments[]` is
  16 **independent** base64'd `dial9-trace-v4` traces, each with its own header
  and symbol table, and **the last one is uncompressed** because the tail was
  unsealed when the report was written. Concatenating them and gunzipping once
  fails with "invalid gzip header"; so does `MultiGzDecoder`. Decode each
  segment separately, gunzip only those starting `1f 8b`, and merge the symbol
  tables — symbol indices are per-segment and mixing them silently mislabels
  every frame.

*What it found.* On the three reports of 2026-09-07 the leaf leaderboard put
45%, 48% and 56% of main-thread samples in `output_span_for_buffer_offset`, one
linear scan of a `SumTree`, reached 100% of the time through `refresh_dashboard`
-> `sync_tree` -> `splice_inlays` -> `InlayMap::splice`. That is the whole
reason the seek fix (93af55e4) exists, and no amount of reading the frame
numbers would have named it: the frames only said the transcript's draw was
slow.

*What the reports still do not say.* A frame does not record its own scale, so
the transcript's prepaint at p50 5.1 ms cannot be divided by anything. Until a
frame carries rows on screen, blocks, excerpts, inlays and cursors, and until
there are stages around `sync_tree` and `splice_inlays` with transform counts,
a report can say a frame was slow but not what it was slow per.

## Reading a rig's own profile

Every `rig up` leaves a profile behind and `rho-qa profile <path>` prints one
line from it: the frame gaps, the worst stage, what the main thread did
between frames, and the top three leaves of the CPU samples. That line is
what a landing note quotes.

*When to ask for the chains.* The line names leaves, and a leaf says what
the thread was in, not what put it there — `__syscall_cancel_arch_end` at 8%
is not an answer, it is a question. `--stacks` prints the whole callchain
behind each sample, most samples first:

```
cargo run -q -p rho-qa -- profile <path>.bin --stacks              # every chain
cargo run -q -p rho-qa -- profile <path>.bin --stacks memcpy       # only these leaves
```

It is off by default because a run of any length holds tens of thousands of
chains. Turn it on the moment a leaf is a library function, a syscall wrapper
or an allocator: those name a mechanism and never a path, and the path is
always the thing that can be changed. The 8% of GUI-thread CPU that this
answered on 2026-09-08 came back as `sendmsg < wl_connection_flush <
wl_display_flush` and `recvmsg < wl_connection_read < wl_display_read_events`
— the Wayland socket, per frame, nothing of rho's under it — which no leaf
leaderboard could have said.

*When the frames are addresses, check the reader's features first.* Only the
frames whose mapping symbolized come back as names, and a chain of addresses
is not an answer, so check that rho's own frames are named before drawing
anything from a chain that passes through them. In the profile that answered
the Wayland question every rho frame printed as an address and the symbolizer
said `failed to read ELF section with index 44`. Section 44 is
`.debug_abbrev`: the dev shell links with `-Wl,--compress-debug-sections=zstd`
and the symbolizer gave up on the whole mapping over it.

The temptation there is to blame the linker flag and change it, and that is
the wrong end. blazesym reads zstd; its decompressors are cargo features, and
the graph had zlib on and zstd off, so it was our own build of the reader
refusing a format it can support. `rho-profiling` now names blazesym with the
`zstd` feature so the sampler's copy has it. Read the features rather than
guessing at them:

```
cargo tree -i blazesym -e features
```

The rule this is an instance of: when a reader will not read a file, ask what
the reader was compiled to do before you change the file. The file was
telling the truth — `.symtab` sat uncompressed in the same binary with all
534,913 names in it the whole time.

*Read a chain structurally, and never a leaf's name alone.* A named frame is
not the same as a true one. These binaries are built with inlining on, so the
symbolizer attributes an address to whichever inlined function sits nearest
it, and near is not the same as responsible. The 4% third leaf on the GUI
thread came back as `runtime`, which is a real function name — a one-line
`ActionTiming::runtime` accessor in gpui's profiler — and the Wayland event
loop those samples were actually in does not call it. Two checks settled it in
minutes and neither needed a rerun: `nm --defined-only` on the binary found no
such symbol in 558,809, so the name came from debug info rather than the
symbol table; and every sibling leaf under the same parent was a calloop or
rustix internal, which says what the neighbourhood is. The same profile put
`is_some<FocusId>` above functions it cannot have called.

So read the frame you can defend. A chain is trustworthy where its shape is —
`prepaint`, `dispatch_events`, a syscall wrapper — and unreliable at the one
frame you most want to quote. When the answer has to be exact, spans beat
symbols: a span is recorded by the code that ran, and no amount of inlining
can move it.

## Adding a case

A new case starts every time the user reports something. Write it before
fixing: the case is what says the fix worked, and the list of cases is the only
record of what has been proven. Give it the next number in its section, say why
it is tricky in one paragraph, give the exact steps, and leave "Closed by"
empty until a run fills it in.

## Finding the layer a crash comes from

A crash in a stack of maps almost never comes from the layer that panics. The
transcript's display map is six of them - buffer, inlay, fold, tab, wrap,
block - and each hands the next a snapshot plus the edits since the last one.
The panic surfaces wherever some layer first notices that those two do not
describe the same document, which is usually well above where they stopped
agreeing.

So do not read the panicking layer. Ask every layer the same accounting
question and walk it down until one answers no: does this snapshot's row count
equal the last snapshot's row count plus the net of the edits handed over with
it. Add the check at each layer in turn, run the failing case, and read which
layer is the first to disagree. The layers above it are reporting faithfully
and have nothing wrong with them; the layers below it were never asked.

This is cheap and it is decisive. On the streaming crash the whole run had
exactly one disagreement, four layers below the panic, and it was the fatal
one: an edit whose net said one thing and whose snapshot said another. Two
readings that were plausible from the panic alone - a wrap-map interpolation
and an unsigned subtraction in `line_len` - were both wrong, and both had been
written down as findings before the accounting was run.

Two rules that come with it. Take the check off before landing; what stays is
the invariant that earns its place, not the scaffolding that found it. And a
control that passes with and without the fix is not a guard: say so and land
the test that fails without it instead.

### Prove the harness before you trust the run

A proof run has two things that can be wrong: the code under test, and the
harness driving it. A broken harness does not report that it is broken. It
reports that everything is fine, which is the answer you were hoping for, and
that is why it survives.

Both of these happened on the three runs that proved the fold fix, and both
would have produced a confident clean report:

The rig was never restarted. `rho-qa` had been handed the rig's own `HOME`,
because the wayland driver needs it, so it resolved the rig path underneath it
and `rig down` and `rig up` both failed with a path error that was being
discarded by a `tail -1` on the output. The script drove one nine-minute-old
session three times and printed three runs. The liveness check beside it was
passing on a pid that was not the GUI and no longer existed. Everything the
script said was green.

A key was read as not arriving. The first probe screenshot was taken with no
delay after the key, so it caught the frame before the redraw, and the screen
looked unchanged. The conclusion available from that - the transcript is not
taking input - is the one someone had already been wrong about that morning.

So, before a run counts:

Give the harness a question you already know the answer to. A key you know
reaches the workspace must show a changed screen; if it does not, the harness
is wrong, not the app. A grep for a fault must find that fault in a log known
to contain it - the fold fix's zero was worth reporting only because the same
grep returns 1,440 lines on the session from before the fix.

Do not discard the output of a step you depend on. `tail -1` on a command that
can fail is how a failure becomes a success.

Check the state moved, not that the command returned. The rig's own journal
counts its sessions; three runs is three new sessions, and if it is one, you
drove one session three times.

Check that the binary under test is the one you built. "I built it" and "the
thing that ran is the build I made" are two claims and only the first is easy.
A rig launches several binaries from several packages; a rebuild that names the
ones you were thinking about leaves the others at whatever they were. Five
sessions in a row were driven against a GUI four hours older than the tree, and
every one of them was reported as evidence about a tip it had never contained -
including a crash said to survive two fixes that were not in it. Timestamps are
worth checking and are not proof: the check that settles it is to put something
in the build that cannot be in the old one - a marker string in a log line the
run is certain to print - and confirm it appears. That is the known-answer
check again, aimed at the binary rather than the app.

And say so afterwards. A harness error found and corrected belongs in the
landing note beside the result, because the number means nothing without the
account of how it was taken.

Count a marker anywhere on the line, not at the start of one. Under
`--nocapture` the harness prints `test tests::name ... ` without a newline
and the test's own output continues that same line, so a marker a test
prints lands mid-line and a line-anchored pattern does not see it. A sweep
of the fold map's accounting record counted one violation with
`grep -c '^MARKER$'` and four with `grep -c MARKER` on the same log, and
the three it missed were the three that mattered - transcript tests
carrying the fault on the path the user sits in all day.

And read the assertion directly before retracting a finding. That miscount
was used to withdraw a correct result, and an experiment on another commit
was then built on the withdrawal; the experiment's answer was true and
proved nothing, because the other half of the comparison had never been
measured. Running the three tests and reading what they said took twenty
seconds. When a direct measurement of the thing is available and cheap,
take it before building an experiment that infers it.
