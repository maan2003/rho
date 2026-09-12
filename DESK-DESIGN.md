# Notes and filing

Status: retired to its notes-and-filing half on 2026-09-03 (tree slice 4).
What used to live here has moved: the store and the shape of a node are
`STORE-DESIGN.md` (formerly `TREE-DESIGN.md`), the dealer and what it ranks are `HOME-DESIGN.md`, the
surface timeline and the deal are `HOME-DESIGN.md` too. The desk, the
inbox, and rooms are gone as concepts; this is what survives them, which is
how notes are written and how things get filed.

This is a design, not a spec. It records the *why* behind each decision.
Rho has exactly one user, so everything here is tuned to that one person and
can be changed the moment it stops fitting.

## Note text is 100% user-written

Every word of a note body is there because the user put it there. The
machine creates reference nodes, moves them when a verdict files them, and
sets state; it never writes a `body`.

**Why:** a note is a paged-out memory. A memory is only useful if it is
stable — if the system rewrites it, reading it no longer tells you what you
decided, it tells you what some process last did. Notes the system
co-writes become a feed you read instead of a memory you own. This rule is
structural: it is not "the system is careful", it is "the system cannot".

## A note is one text; its first line is its title

The body is multi-line and as long as the thought needs (the store caps it
at 4 MiB, which no thought reaches). The first line is what every other
view calls it: a path segment in find, a card's label, a row on the map. No
title is stored anywhere, so a note renamed by editing its first line is
renamed everywhere at once.

**Why:** a stored title is a second copy of the same fact, and two copies
drift. Deriving it also means writing a note costs one gesture: type, and
the first line has already named it.

## Capture costs nothing and decides nothing

One gesture, write the thought, back to what you were doing. No picking a
parent, no category, no naming: a capture is a note at the root. Filing
happens later, when the context makes the right parent obvious.

**Why:** every decision added at capture time is paid on every thought, and
eventually you stop capturing. The brain lets go of a thought only when it
trusts the system to bring it back (GTD).

## Placement is the labels a thing carries

The user's call, 8 Sep: a thing is placed by its labels and carries no
parent. `Parent` nests labels and nothing else. An agent, a note, a page, a
Slack unit or a file carries a set of `Labeled` cells and otherwise sits at
the root, and the label's own parent chain is what makes `rho/agent` count
as `rho`. `STORE-DESIGN.md` already writes this rule down under "Labels are
the filing; parent is only for labels"; what follows is what the code still
does instead, and what the placement gestures become.

**Why:** a parent is one place and a thing is in several. An agent working
in the rho checkout on a Slack thread's bug belongs under both, and the
parent forced a choice that was wrong half the time; the label set does
not. It also collapses two filing systems into one: `f` already writes
labels, and a place was a second thing it could write.

### What the store already gives

`Labeled { label, present }` is a set — the label is part of the key, so
two devices tagging at once do not fight, and the merge is add-wins. A
label is `Id::Label(uuid)` with a `Name` and its own `Parent`, so a label
path is minted and found by name (`label_path_writes`), listed as a path
(`label_paths`), and renamed in one cell. A label may carry a `Project`,
which is how a workdir is inherited: the chain walked is the label's, not
the thing's. The registry is already fed the labels of every agent and
nothing else about placement (`agent_filing` → `AgentFiling`), and Home
rows, cards and tabs now read them after the agent's name.

Nothing in the store has to change. The rule is a rule about what rho
writes, and the store already accepts both.

### What the draft already gives

`n a`, `n p`, `n n` all open the area picker first, so a new thing always
has somewhere to be. The picker offers every node on the desk plus `root`,
ranks the node in context first, and hands what was chosen to
`filing_property`, which writes `Labeled` for a label and `Parent(Some(…))`
for anything else. So the draft is already half of the new rule: choose a
label and a new thing is placed the way the rule says.

Three places wrote a parent on a thing, and they were the work. All three
are done:

- **`filing_property`** — a new thing filed under a note or an agent.
- **`file_under`** — the `f` picker's non-label rows ("anything else picked
  is a place, and a thing is in one place").
- **notes for this** — the note it creates is the child of the thing on
  screen, and it finds an existing one by `parent == the thing`.

The first two became label writes and their pickers stopped offering
places. The third was never filing at all: a note *about* the thing on
screen. `About(Id)` is the cell for that and it already existed, so the key
keeps its meaning and stops using the placement axis to say it — the note
is placed where the thing is, by the labels the thing carries, and `About`
is both the relation and how the second press finds the note again.

### The picker offers the smallest set that says where it is

A thing that carries `rho/agent` is under `rho` already, so the picker
neither shows nor writes `rho` beside it: the set kept is the minimal one,
with any label that another carried label already implies dropped. Adding
`rho` to a thing carrying `rho/agent` is a no-op the picker says nothing
about; adding `rho/agent` to a thing carrying `rho` takes `rho` off and
leaves the deeper one.

**Why:** without it every thing accumulates its own ancestry, the rows grow
a tail of labels that say the same thing, and "which labels is this under"
stops having one answer. The set is small enough to read on a Home row,
which is where it is now shown.

### Create from here

On a Slack message, on an agent's transcript, on a note — the new-thing
verb takes the labels of what is on screen rather than asking. The picker
is still there for a thing that belongs somewhere else, but the common
case, "another agent for this same work", stops being a question the user
answers twice.

`here` is the picker's first row, so Enter alone is create-from-here. The
new thing takes every label the thing on screen carries — the same place,
without being asked for it a second time — and writes `About` naming that
thing. The labels are what put it on the map; `About` is only how it got
there, which is why one act writes both and neither stands in for the
other.

### Left to decide

- Whether every relation rho already derives (`FromSlack`, `FromPage`, an
  agent's spawner) reads the same way to the user as the `About` the two
  acts write. Nothing rho derives is stored as `About`; a derived relation
  stays derived.
- What the map draws for a thing with two labels: it is under both, and an
  outline draws each row once. Drawing it under each is the honest answer
  and the one that costs a reader nothing; a "primary label" would be the
  parent again under another name.
- Whether the existing parents on things are converted or left. They are
  the user's own filing, so leaving them unread is losing it; converting
  each to the label of the same name is a one-shot with the same shape as
  the outline conversion, and that one is now deleted rather than kept.

## One lifecycle for everything

Everything — a page, an agent, a thread, a note — is open, deferred, done,
or dismissed. Verdicts differ per kind: done on a page is a dismissal and
must cost nothing; done on an agent is accepting reviewed work and deserves
friction; deferring a parent mutes everything inside it.

A thing that lives somewhere else -- a Slack unit is the case that made
this -- is a virtual node: it is identified by that system's own ids, it
is dealt and filed and opened like anything else, and it becomes a real
node in the store only when a cell is written that the other system has no
place for, such as a snooze, a name, labels or About.

**Why:** one lifecycle means one mental model and one dealer for
everything. Tab hoarding is what humans do when tabs lack done/defer
semantics — the tab stays open because closing it loses the commitment.
Giving pages the same lifecycle as agents fixes that. But the *word* is
shared, not the meaning, so the gestures and the amount of friction differ
per kind on purpose.

## The map and the note surface

The map is the tree by kind and children: one editor over an outline, with
ordering derived rather than stored. It is a place you visit deliberately,
not persistent chrome. A note also has a surface of its own — the body,
with the node's children under it — which is what `enter` on a row opens
and where a long note is actually read.

**Why:** the outline is right for seeing where something sits and wrong for
reading a page of prose. Splitting them means neither has to compromise,
and both are the same text: editing a note on its surface and editing it on
the map are one edit on one CRDT.

## The desk mirror

The client holds a replica of the desk cells on disk, the way it holds a
replica of the agent log, and reads only the replica. The daemon is where
the store lives; the replica is what the client opens with, and what it is
never without.

**Why.** Today `DeskCells` is built empty at launch and filled by the first
`DeskSynced`. Between those two moments the client holds a desk that says
nothing, and an empty desk and a desk that has not arrived are the same
value with opposite meanings: the first says the user has said nothing, the
second says nobody has asked. Every reader that could not tell them apart
read the first meaning and was wrong. Two of them shipped: the dealer dealt
agents the user had snoozed, and Home listed them as running, for as long
as the first sync took on the user's own store. Those are patched by asking
`is_synced` at each reader, which is a guard that has to be remembered at
every new one. The replica removes the state instead of guarding it: there
is no window in which the client has no desk, because the desk is a file it
opens.

It also buys the cold open. A desk read from disk is drawn in the first
frame; the sync that follows is a delta, not the whole store.

**Resume, not reload.** The wire is already shaped for this.
`ClientMessage::DeskSync` carries `known: Version` — the per-device Lamport
map the client already holds — and the daemon answers
`desk_cells.sync_since(&known)`, which is `Store::since`: every cell and
verdict whose stamp is newer than the client's lane for that device. Within
one process this already works; a restart loses `confirmed` and so sends an
empty `known` and is answered with the whole store. Persisting `confirmed`
and its version is the whole of the client's half. Deletion needs nothing
extra: a delete is a cell with a stamp like any other, so it arrives in the
same delta and cannot be missed by resuming.

This is the agent mirror's shape. `agent-mirror.redb` keeps a `StoredHost`
per host with the journal `seq` it has read through, and `Follow { since }`
asks for the rest; the desk keeps a `StoredHost` per host with the
`Version` it has read through, and `DeskSync { known }` asks for the rest.
One is a scalar and one is a map per device, and that is the only
difference that matters.

**The file.** The client's own database, `rho-client.redb` in the state
directory that `main` resolves — a library never reaches for it, the rule
that already governs the agent mirror. The replica is
tables in it, beside the agent mirror's, the Slack mirror's, the journal's
and the inbox's; one file, opened once, each crate keeping its own names
and its own types. Tables: the
cells by id, the verdict events by `(id, stamp)`, the note bodies by id,
and one `StoredHost` per host holding the version and the store's identity.
The device id is a row in the same file. It has to be: the version is a
count of this device's writes, so a replica deleted while its id survived
would write stamps the daemon has already counted and lose them to
last-writer-wins without a word. Delete the file and the id goes with it,
and this is a new device with nothing behind it.

**Whose store is it.** The agent mirror asks the daemon for a
`machine_seed` and starts over when it is not the database this copy counts
in, or when the journal is shorter than the copy. The desk has no such
question on the wire: `DeskSynced` says nothing about which store answered.
Without it a client that has a version from one store and connects to
another — a restored backup, a different machine behind the same
name — sends a `known` the new store has never issued and is answered with
the cells it has not got, and the client goes on holding rows the daemon
does not have and calling them the user's desk. So both halves of the
handshake name the store. `DeskSync` carries `store: Option<DeviceId>`,
which is whose numbers `known` counts and is `None` when the client holds
no replica; `DeskSynced` carries the store that answered. A daemon given a
name that is not its own ignores `known` and sends the whole store, because
a difference taken from a number counted elsewhere is not a difference at
all. A client answered under a name it was not holding drops what it
held — the stores, the writes in flight, the map, the copy on disk — and
takes the answer as a first sync. One round trip, and at no point is the
user reading one desk made of two stores. This is the reason the daemon
commit is the sensitive one.

**What the replica holds.** Everything the client has written and
everything the daemon has sent, which since 9 Sep is the same store: a
write goes into the view and into the replica on disk in the same breath,
before the message goes out. It used to hold only what the daemon had
acknowledged, on the reasoning that a write in flight was the daemon's to
accept or refuse; the daemon does not refuse any more, so a write the
reader has been shown is theirs and a client that is closed a moment
later must open holding it.

**A verdict is complete when it is written here.** Not when a
`DeskMutationAccepted` comes back: that message is gone, and the queues
that waited on it with it. The undo is armed, the dealer is told, the
card leaves and the bar says so, all in the keystroke that made the
verdict. What goes out to the daemon is a copy for the other devices,
and nothing on this one waits for it. Offline, the desk works.

**The cold-open gate reads "the client's replica is loaded".** The rule
it guards is unchanged: nothing is dealt until the store has been read,
because a client that has not read it cannot say the user did not put
this agent away yesterday. What changed is where that reading comes
from. The replica is opened in `Workspace::new`, before a socket exists,
so Home's first draw is the user's own desk rather than a list waiting
on a daemon.

**Sync is two ways.** `DeskSync` asks for what the daemon has and answers
with what the client has: the daemon sends the cells above the client's
`known`, and the client sends the cells above the daemon's frontier,
which `DeskSynced`'s delta carries as its version. `ClientMessage::
DeskCellsApply` is that half, and the daemon merges it like anything
else. Without it a write is delivered exactly once or never: a verdict
taken while the daemon was down, or one lost on the wire, would sit on
one disk forever, because a write completes on the client and nothing
replays it. With it, the next handshake carries it. Ordinary syncs send
nothing back, since the cells the daemon just sent are the ones the
client would have offered.

**Bodies resume from the mirror.** `DeskSync` carries a `BodyVersion`
per note — the highest operation counter this client holds from each text
replica — and the daemon answers with the operations those lack and
leaves out the bodies with nothing new in them. A note the client has
never held is missing from the map and comes whole. The replica builds
each history up out of the pieces it is sent rather than replacing it,
and the versions it resumes with are read from the histories it holds.
Text typed on this client is written into the replica beside being sent:
the daemon never sends a client its own operations back, so a note
written here and only sent would read empty at the next cold open. That
write carries bodies alone and leaves the host's cell version vector
where the daemon's deltas set it — local typing is not a claim to have
seen anything of the store.

**The daemon does not refuse a mutation.** It merges every one it can
decode; last-writer-wins is the whole rule. Nothing on the desk waits for
permission, and there is no replay queue behind the view: a write is
never taken back out of the middle. The reasoning is in STORE-DESIGN.

**Bodies.** `DeskSynced` sends `desk_cells.bodies_since(&known)`: the
operations the client's own `BodyVersion` per note does not cover. A
text replica numbers its operations in order, so the highest counter
from each replica is the whole of "what I have", and one number per
replica per note is all the client has to say. It used to send every
note's text in full on every sync, so a delta of one cell carried the
whole desk's prose.

**Order.** Client first, daemon second. The client's half — persist,
open from the replica, send the version it holds — is correct against
today's daemon, which already answers `since`. The daemon's half is the
store identity on both messages and the drop it forces.

## Deferred on purpose

- **Agent help with filing** — an agent suggesting the parent for a note,
  or gardening the tree on request. Manual filing first.
- **Briefings** — an LLM summary of what happened while you were away.
  Generated from the logs, so it can be added any time.

## Symptoms to watch for

- **Notes turning into a feed:** if any future feature wants the machine to
  write into a body, the first decision above is the answer: no. Machine
  text belongs in a typed field.
- **A title stored rather than derived.**
- **A note that can only be reached from the map**, which means the thing
  it is about lost its link to it.
