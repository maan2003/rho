# The tree: everything in rho is a node

Status: decided with the user on 2026-09-03, building. Supersedes the
storage half of `DESK-TREE-DESIGN.md` and the inbox and desk-as-home parts of
`DESK-DESIGN.md`. `HOME-DESIGN.md` and `SLACK-DESIGN.md` sit on top of it.

## The problem

Rho has three stores for "things that can need attention": the desk tree
(daemon, CRDT), the inbox (GUI, redb), and per-source verdict tables (the
Slack mirror). Each has its own identity type, and the dealer, undo, the
journal, and the deal views carry an enum to bridge them. Verdicts on the
phone are invisible on the desktop because two of the three stores are
device-local. And the desk, which was home, is really a notes store.

## Decisions

### One tree of nodes; a node is a kind, a place, and attention state

A node is `id`, `kind`, `parent`, `deleted`, and typed fields. The parent is
the filing: a node's parent is its context. A note under a Slack thread is
"notes for this thread"; an agent under a pull request is the engineer on
it; a room is nothing special, only a node with children. `parent = none`
means the root, which is also what "unfiled" means. There is no stored
sibling order: every view sorts children by its own rule (score on Home,
time in notes, name on the map).

Kinds, and the fields each adds to the common ones:

- `note`: `body`, free-form text. The only kind with user-written text.
- `agent`: `agent_id`, `host` (the machine seed, u64, the durable machine
  identity; never a GUI attachment index).
- `page`: `page` (browser reference), `url`.
- `thread`: `workspace`, `channel`, `ts` (Slack; a channel is a thread
  with no parent message).
- `pull_request`: `repo`, `number` (arrives with the GitHub work).

Common fields: `created_at`, `state` (`open`, `done`, `dismissed`),
`defer_until`, `deadline`, `tags`. Titles are derived per kind (a note's
first line, the agent's name, the page's title, the thread's subject), never
stored, so the structure store holds no free text but note bodies.

### Fields merge by CRDT, chosen per field

Modelled on cr-sqlite's "a row is a map of CRDTs" and Figma's multiplayer:

- Last-writer-wins register for `parent`, `kind`, `deleted`, `state`,
  `defer_until`, `deadline`, and every reference field. Each cell carries a
  stamp `(device, version)`; newest wins, device id breaks ties.
- One boolean LWW cell per tag. Opposing writes at the same numeric version
  choose add (`true`) regardless of the device tie-breaker; writes with
  different versions follow ordinary LWW. This is not a causal add-wins set.
- Grow-only log for verdicts: `(node, stamp) → VerdictEvent`, merged by
  union. `state` is the current value; the log is history and undo.
- Text CRDT for `body`: the editor's buffer already is one; its operations
  persist keyed by node. This is the one field two devices genuinely edit
  concurrently, so LWW is not acceptable there.

Known and accepted: LWW silently drops one of two concurrent writes to the
same field of the same node. For one user that is the right trade
everywhere except `body`.

### Cycles are handled in rendering, never fixed in storage

Two devices moving A under B and B under A offline produce a cycle after
merge. It is rare enough that the handling is the dumbest thing that loses
nothing: when materializing, a node whose parent chain never reaches the
root is shown at the root. The stored cells are not touched; the user
refiles when they notice. No move algorithm, no edge maps.

### Deletes are a flag

`deleted` is an LWW boolean. A live child of a deleted parent is shown at
the root. Undelete is setting it back. Nothing is ever removed from the
store by the machine.

### Sync is "cells since your version"

Every device keeps a version counter; every cell records the stamp that
wrote it; every device remembers the last version it saw from each peer.
Sync is the set of cells newer than that, in either direction, over any
channel. For now the daemon is mandatory and is the hub: every GUI syncs
with the daemon only, never with another GUI, and the daemon's store is
the one every device converges to. The GUI talks to it through the
existing protocol carrying cells instead of tree operations. The planned
peer-to-peer future swaps the transport, not the model.

### The membrane

The machine creates reference nodes (`agent`, `page`, `thread`,
`pull_request`), moves them when a verdict files them, and sets `state`.
It never writes a `body`. "The desk is 100% user-written" becomes "note
text is 100% user-written".

### The dealer deals nodes

Card identity is `NodeId` everywhere: dealer, skips, undo, journal, deal
views. Curves are per kind as today; `defer_until` and `deadline` are the
tuning knobs, typed. An `open` node is dealable only if `defer_until` (once
reached) or `deadline` is set; a note with neither is a plain note and is
never dealt. After `defer_until` the curve is `elapsed - pace_days`, which
is today's todo curve, and today's defer curve when `pace_days` is 0. The
old todo and defer marks both become `defer_until` (todo keeps its pace,
defer gets 0), so every existing card ranks exactly as before; the one
loss, accepted on 3 Sep, is the word: a woken todo and a woken defer read
the same in the bar. There is no `todo_at` field and no `todo` tag. A verdict appends to the log and sets fields:
`done`/`dismiss` set `state`; `defer` sets `defer_until`; `file` sets
`parent`; `todo` creates a `note` child. Undo reads the log entry and
restores the fields it changed. Capture is "create a note at the root";
the inbox stops existing as a concept.

Node volume rule: a `thread` node exists only once something made it
matter (a ping, a reply, a mark, the user opening it from Home), never one
per message. The mirror stays the bulk store.

### Views

- Home (`HOME-DESIGN.md`): the dealer's ranking of nodes.
- The map: the tree by kind and children, replacing the desk view; the
  same editor over an outline, ordering derived.
- The note surface: an editor over `body`, with the node's children below.
- One key on any surface opens or creates the note under that node.
- Every other kind keeps its surface.
- Find: a minibuffer over every node, matched fuzzily against its full
  path (`nixos › poco on linux`, `#design › release date`, an agent's
  label under its parent). Matching is subsequence with word-boundary and
  path-segment bonuses, fzf-style, so `nixpoco` finds it; ranking by match
  quality, then recency of use. `enter` opens the node's surface as a
  normal open (transcript, page, conversation, note). Asked for by the
  user on 2026-09-03 for agents first; built over the desk tree's paths
  now, behind one function that yields `(path, target)`, and moved onto
  `NodeId` in slice 2 without changing the prompt. Built 3 Sep: `ctrl-shift-f`
  anywhere (or `shift-f` on the root transient), scorer in
  `crates/rho-gui/src/find.rs`, recency from what already records a use
  (agent activity, conversation latest, thread verdict key) until the tree
  carries a per-node last-opened field.

## Migration

One shot at daemon start, like the desk cutover: desk headings become
`note` nodes (heading text as the first line of `body`, children kept,
marks to `state`/`defer_until`/`deadline`); inbox items become `note`
nodes at the root or `thread` nodes; the Slack verdict table becomes
verdict log entries on `thread` nodes. The old stores are read once and
left in place until the next release.

Once the user's daemon has run the migration, the migration goes: the
frozen V1 decoder, the V1 tables and their fixture, the rollback text, and
the migrated marker, all deleted in one change so nothing carries a
compatibility facade (the user's standing rule, 3 Sep). Queued for 5pha
the moment the upgrade is confirmed.

Deleted 3 Sep, after confirming read-only that the live daemon had run it:
the daemon binary the running process was started from carries the
conversion, and `initialize` runs on every start, so a daemon that is up
and answering has cell state and had written the marker. Gone with it:
`desk_tree_v1.rs`, the `desk-tree-v1-real.redb` fixture (the cell tests
now seed their own note), the `MigrationReport` and its marker table, and
the rollback paragraph in SECURITY. A database with no cell state now
starts empty instead of converting one. What is left of V1 is the native
tree store itself (`desk_tree.rs`, the `DeskTree*` protocol messages, and
`desk_org_migration*`, which only it uses): nothing writes to it, and the
one reader is `rho desk cat`/`checkout`, the agent-facing skill, which
still shows the pre-cutover tree. That view has to move onto cells or go,
and it is the user's call which.

## Slices, in landing order

1. Storage: cells, LWW and add-wins merge, verdict log, materializer with
   cycle-to-root and deleted-to-root, versions and sync-since, migration of
   the desk tree, the desk view kept working over it. Replaces the
   order-key and move machinery in `rho-desk` and the daemon.
2. Identity: `NodeId` through the dealer, undo, journal, and deal views;
   the inbox deleted outright (store, surface, kinds, source references,
   journal events, the word itself), the user's call on 3 Sep; captures
   as root notes, with a one-shot on first run that turns existing capture
   items into root notes and is deleted with the V1 migration after the
   upgrade; Slack inbox items are not carried, they re-derive from the
   mirror. Every agent gets a root
   `agent` node at creation, quick-spawn included, made by the daemon.
   Creation itself follows `CREATE-DESIGN.md` (3 Sep): `n` from anywhere,
   the area always asked with context first, the draft page carrying the
   agent fields; `DeskPageBind.parent` optional so the new-page flow lands
   in the tree, since the inbox it landed in is gone.
   Pulled in from slice 3 on 3 Sep so Slack never stops being dealt: a
   `thread` node is created when a ping or a reply to the user would have
   made an inbox item, through a `DeskThreadBind` request from the GUI to
   the daemon (the shape of `DeskPageBind`), and the dealer deals the node;
   Slack verdicts live in its log from then on (Slack checklist 2.10).
3. Slack: the mirror's verdict table removed; anything left that still
   deals from the mirror rather than from nodes. Landed 3 Sep: slice 2's
   cutover had already moved every decision onto thread nodes, so there
   was no verdict table left to delete and nothing dealing from the
   mirror. What this slice removed is what was left pointing the old way:
   `Model::obligations`, the dealer's entry point into the Slack store,
   dead outside its own tests; `ThreadCard::verdict_key`, renamed
   `latest`, because the newest message is a fact and the verdict keyed on
   it is the node's; and the design's claim that the mirror is the home
   for verdict state. A test pins the rule: a done thread is closed by its
   node whatever Slack still says, and is still findable so a newer
   message can rebind it.
4. Notes: multi-line `body` on the text CRDT, the note surface, "notes for
   this" from any surface, the map over the tree; `DESK-DESIGN.md` retired
   to notes-and-filing. Landed 3 Sep. The body was already multi-line in
   the store (the 4 MiB cap and the newline-free text path were there from
   slice 1); what was missing is that everything downstream read a whole
   body as a title, so `note_title` — the first line, trimmed — is now the
   one place a note is named, and paths, cards, rooms, and pickers all go
   through it. The note surface is `crates/rho-gui/src/note_view.rs`: one
   composition whose section is the node's own body buffer and whose tail
   is a generated row per child, so editing a note there and editing it on
   the map are one edit on one CRDT. `enter` on a row — on the map, in the
   finder, or on a child row of a note — opens what the row is, which for
   a note is its surface; staffing a note with an agent stays `r`.
   "Notes for this" is `ctrl-shift-n` (and `space shift-n`): the child note
   of the node the surface is about, created on the first press and
   reopened on every one after. Two keymap bugs stood in the way of a body
   ever having a second line and were fixed here: `enter` in insert mode
   fell through to the transcript prompt's `SubmitPrompt`, which ate the
   key, and on the map the mode-less `RailOpen` binding outranked both, so
   `enter` while typing opened the row instead. The map also pads the
   second and later lines of a body to its bullet's column, so a note
   reads as one block. No daemon change: the GUI already creates notes in
   its own namespace, the 4 MiB cap and the text path were there from
   slice 1, and the daemon never derived a title.
5. Home on the tree.

Each slice lands on its own with the tests of the slices before it green.

## Symptoms to watch for

- A field stored as text that should be typed.
- A title stored rather than derived.
- A `thread` node per message.
- A verdict that changes fields without a log entry.
- Any code path that repairs a cycle in storage.

## What done means

One store, one identity, verdicts visible on every device, a note under
anything, the desk gone as a concept, Home and the phone feed dealing from
the same tree.
