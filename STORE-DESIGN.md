# The store: ids, relations, views

Status: decided with the user on 2026-09-04, building. Replaces
`TREE-DESIGN.md`, which is kept only as the record of what its slices
landed. `HOME-DESIGN.md`, `SLACK-DESIGN.md`, and `CREATE-DESIGN.md` sit on
top of this.

## The problem

The tree stored copies of structure that other systems own. An agent's
spawner and host come from the registry, a Slack thread's channel from
Slack, a tab's origin from the browser; the tree copied each into a
`parent` cell on a node the machine created, and then had to keep the copy
honest with bind requests, reopen-on-rebind, and one-shot migrations. It
made a node per Slack ping, so a done thread came back when history loaded
(Slack checklist 2.17). It had one parent per thing, so an agent spawned by
an agent, a thread about a project, and a tab opened from a page each had
to choose between the place the user meant and the place the source knew.
And a Slack cursor kept beside the mirror would have been a second store
for the same kind of fact, unsynced.

## Core decisions and why

### An id names a thing in the system that owns it

`Id` is a typed enum: `Note(uuid)`, `Label(uuid)`, `Agent(AgentId)`,
`Host(seed)`, `Page(PageId)`, `Slack(SlackUnit)`, `PullRequest{repo,
number}`, `File{host, path}`. `SlackUnit` is `{workspace, channel,
thread: Option<ts>}`: a direct or group conversation, a channel, or a
followed thread; never a message. Rho mints ids only for notes and labels.
Every other id is the owning system's own identity, so nothing is ever
"created" in rho for an agent, a page, or a thread: it exists because its
source says so, and it is addressable in the store from the moment it
does. There is no `kind` field; the kind is the id.

**Why:** a second identity for a thing that already has one is where every
rebind, reopen, and duplicate-card bug this week came from.

### The store holds facts, and only the user's

A fact is `(subject: Id, relation)`, and the relation is a typed enum
whose variant carries whatever data that relation needs: an id, several
ids, a timestamp, text, or an id plus detail the id itself is too coarse
for. There is no separate object column; the payload is the object.

Two shapes of relation, decided per variant:

- One per subject, last-writer-wins: the store key is the subject and
  the variant; a newer stamp replaces the payload. `Parent(Option<Id>)`,
  `Name(String)`, `State(State)`, `DeferUntil(Timestamp)`,
  `Deadline(Timestamp)`, `PaceDays(u32)`, `HandledThrough(Ts)` (Slack,
  the verdict cursor), `Deleted(bool)`, `CreatedAt(Timestamp)`.
- Many per subject, one boolean LWW cell per payload: the store key is the
  subject, the variant, and the payload; the cell says present or absent.
  `Labeled(Id::Label)` (the tag rule as today: opposing writes at the
  same version choose add). Any future relation the user can have several
  of is this shape.
- `Body` (note) is the text CRDT, keyed by subject.
- The verdict log: `(Id, stamp) → VerdictEvent`, grow-only, merged by
  union. History and undo.

The payload is where detail lives that the id does not carry. Ids stop at
the unit (a Slack thread, an agent, a page), but a relation can name the
exact thing inside it, and the variant is as specific as the fact:
`FromSlack { unit: SlackUnit, message: Ts }` on an agent records the very
message that led to spawning it, though no id exists for a message;
`FromPage { page: PageId, url: Url }` records the page and the exact
address that led to it. One variant per source, never a generic `From`
with an id that could be anything; a relation may carry several ids where
one fact genuinely joins several things. The same rule bounds it: a
payload is typed, never a string that means something. The enum is the
whole schema: modelling a new fact is adding a variant that says exactly
what happened, with exactly the data it needs, and nothing has to be
generalised to fit an existing shape.

Gone from the cell vocabulary: `kind`, `agent_id`, `host`, `page_ref`,
`url`, `workspace`, `channel`, `thread_ts`, `repo`, `number`, `path`. Each
was either the identity (now in the id) or a source fact (now derived).
The merge rules, stamps, device ids, and sync-since-version are exactly
today's; what changes is the key: `(Id, relation variant[, payload])`
instead of `(NodeId, Field)`, with `Id` typed and the value folded into
the relation.

The machine writes nothing here. Not a reference node, not a parent, not
a title. The only writer is a user's key, through a verdict or an edit.
"The desk is 100% user-written" is now literally true of the store.

**Why:** the CRDT primitive was never the problem; per-field LWW plus a
grow-only log plus a text CRDT is already "a set of facts about an id".
Keeping it means slice 1's merge, stamps, and sync code stay. Restricting
the store to user facts means it can never disagree with a source, because
it never repeats one.

### Source facts are derived, never stored

The same shape, a relation with its payload on a subject, computed at read time from the system that owns it,
in the GUI, where the sources already live:

- `spawned_by` (agent → agent), `on_host` (agent → host), `in_workdir`
  (agent → file): the registry.
- `in_channel` (Slack thread → channel), `in_workspace` (channel →
  workspace), `newest`, `newest_from_other`, `newest_author`: the Slack
  mirror.
- `opened_from` (page → page): the browser. A `ctrl-click` that opens a
  tab records where it came from, so the tab lands under its origin.
- `title`, everywhere: a note's first line, a label's name, the agent's
  label, the page's title, the Slack unit's subject.

The daemon holds the store and syncs it; it never hears about Slack, the
browser, or agent activity. The join of store and sources happens in the
view.

**Why:** the source is the truth and is already in memory; a stored copy
can only be stale.

### Views are rules over facts

Nothing in storage says where a thing is shown, whether it is dealt, or
what it is called. Each view is a rule set, and changing a rule moves
nothing in storage.

- Place, for the map and for paths: the user's `parent` if set; else the
  derived edge into its source context (`spawned_by`, then `on_host`;
  `in_channel`, then `in_workspace`; `opened_from`); else the root. A
  parent chain that never reaches the root (a cycle, a deleted ancestor)
  is shown at the root, as today.
- Labels are a second axis: a thing also appears under every label it
  carries. The map is therefore a DAG drawn as a tree; a thing in two
  places appears twice, and that is the truth, not a bug.
- Matters, for what the map and Find show at all: an id with any user
  fact, or open by source facts, or a place-ancestor of one that is. So
  not every Slack channel, not every finished agent, not every tab.
- Home (`HOME-DESIGN.md`): dealable if open by source facts (an agent
  waiting, a Slack unit with `newest_from_other > handled_through`) or by
  user facts (`defer_until` reached, with `pace_days`), filtered by
  `state`; curves per id kind as today.
- Notes for this: notes whose `parent` is this id, any id.
- Find: every id that matters, matched fuzzily against its place path and
  its labels, as today.

**Why:** the user's words, 4 Sep: "the raw storage stays flexible and then
you convert it to visual with a set of rules". Every "where does this
live" question becomes a rule that can change without a migration.

### Labels are ids, not strings

`Label(uuid)` with a `name`, nested with its own `parent`. In the picker
the user types a name; `rho/agent` finds or creates label `agent` under
label `rho`. Renaming is one cell; two labels cannot drift into `rho` and
`Rho`; a label can carry notes and a defer like anything else. The user
never sees the id.

**Why:** the no-stringly-fields rule, and rename for free.

### Verdicts write facts

`d` done: `state := done`; for a Slack unit, `handled_through := newest`.
`x` discard: `state := dismissed`, plus the source's own silence where it
has one (a thread unfollowed, a conversation marked read). `s` snooze:
`defer_until`. `t` todo: as today, plus for Slack the cursor. `f` file:
`parent := the chosen id`, any id, picked with Find. `l` label:
`labeled += the chosen label`, created if new. `u` undo: the log entry
names the facts it changed and their old values; undo writes them back.
Every verdict is a log entry first.

## Browser tabs

A tab is `Page(PageId)`; it is never created in the store. Its place is
its `opened_from` origin until the user files it, so a burst of
`ctrl-click`s from a search page reads as a group under that page on the
map, and filing the page under a project carries the group with it in the
view (the children's place still derives from the page). Capture
(`CREATE-DESIGN.md`) is unchanged: a draft page carries the fields the
user typed, and the page exists when the browser opens it.

## Migration

One shot at daemon start, and then the code goes (the standing rule):

- `note` nodes → `Note(uuid)` with `body`, `parent`, `state`,
  `defer_until`, `deadline`, `pace_days`, tags as `labeled` to labels
  minted from the tag names.
- `agent` nodes → `Agent(agent_id)`: `parent` kept only where the user
  filed it (a parent that is not its spawner), `state` and `defer_until`
  kept; the node's own id is dropped. Notes under it re-parent to the
  agent id.
- `page` nodes → `Page(page_ref)` the same way; `file` → `File`.
- `thread` nodes: one the user filed (a parent that is not the root) or
  that has notes under it becomes `Slack(unit)` with `parent` and
  `defer_until` kept and its notes re-parented. Every other thread node
  was machine-made and leaves nothing behind, verdicts included: the
  cursor is slice 2's relation, and done-ness on the old model was
  already lost on every restart (Slack checklist 2.17), so nothing the
  user still has is dropped. `DeskThreadBind` goes in slice 1 with them;
  between slices 1 and 2 a Slack card has no verdict state, which is the
  state it was effectively in.
- The verdict log re-keyed to the new ids.

Wire epoch bump; profile upgraded; the user restarts the daemon.

## Slices, in landing order

The user's call, 4 Sep: this comes first, ahead of every short-term bug
and ahead of the verdict transient, because it changes what everything
else is built on. The transient lands after slice 2.

1. Store: `Id`, `Relation`, the cell key change, the migration, the
   views (map, Home, Find, notes-for-this, paths) reading through the join
   of store and sources, `DeskPageBind`, `DeskThreadBind`, and agent-node
   creation gone. `body` is the cells store's own text table re-keyed;
   the native tree's text and `Document`/`TreeOperation` wait for slice
   2. `parent` keeps an explicit none (un-filing is a write, not a
   delete); none and absent both read as root. Daemon change.
2. Slack on the store (checklist 2.18): the unit model, `handled_through`
   as a fact, cards from the join, `DeskThreadBind` gone, the native tree
   store and `rho desk cat/checkout` gone.
3. Labels: the `l` key, the picker with `rho/agent`, the map's label axis.
4. Browser: `opened_from` from the embedded browser, tabs under their
   origin.

Each slice lands on its own with the tests of the slices before it green.

## Symptoms to watch for

- A fact in the store that a source could have answered.
- An id minted for anything but a note or a label.
- A title stored rather than derived.
- A verdict that changes facts without a log entry.
- A view rule enforced by rewriting storage.

## What done means

One store of the user's facts, typed ids that are the sources' own,
verdicts and the Slack cursor visible on every device, a note under
anything, a tab under the page it came from, and every "where is this
shown" answer a rule in one place.
