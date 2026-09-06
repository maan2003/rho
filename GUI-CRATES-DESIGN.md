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

## The crates

- **`rho-agents`.** The daemon connection and handshake, the model
  thread (`Model::ingest`), the agent mirror on disk, the agents map and
  its indexes (what remains of `rho-registry`), the transcript, creation,
  Find over agents, and the agent screens. Tested end to end against a
  fake daemon. Owner: eng-b8os.
- **`rho-slack`, a real Slack client.** The session, socket and mirror
  that exist, plus what a client is: the channel and DM list with unreads,
  a thread view that reads well, compose and reply, reactions, mark read
  that sticks, search, and its own screens and keys. Tested against the
  fake Slack server (`fake.rs`) and against a copy of the user's real
  mirror. Read state: Slack's own cursor is the truth for reading and is
  written back when the user reads here; Rho's `SlackHandledThrough`
  stays the dealing cursor only. Owner: eng-bgwk.
- **`rho-dag`** (today `rho-desk`). The store is a global DAG of cells
  across hosts: notes, labels, parents, verdicts. The crate keeps the
  store and gains the map screen and the note views. The screen is
  called the map.
- **`rho-shell`.** Window state and nothing about any source: focus, key
  contexts, the echo line, surfaces and their history, transients and
  the minibuffer, the shift tap, telemetry, chime. (`rho-shell` the crate
  name is taken by the terminal shell; this one is `rho-window`.)
- **Dealing is composition, not a crate of its own.** Each source crate
  hands the dealer cards: the facts a card is ranked by and the reason
  it claims attention. The dealer ranks across sources with one visible
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
