# The store: ids, properties, views

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

A fact is `(subject: Id, property)`: an id has properties, some of which
point at other ids (`parent`, `labeled`, `from_slack`) and some of which
are values (`state`, `handled_through`). The word is "property", not
"relation" (the user, 4 Sep): a cursor is a property of a thread, not a
relation to anything. The property is a typed enum whose variant carries
whatever data that property needs: an id, several
ids, a timestamp, text, or an id plus detail the id itself is too coarse
for. There is no separate object column; the payload is the object.

Two shapes of property, decided per variant:

- One per subject, last-writer-wins: the store key is the subject and
  the variant; a newer stamp replaces the payload. `Parent(Option<Id>)`,
  `Name(String)` (a label's name; on any other id the user's override of
  the derived title, so renaming an agent, a Slack unit, or a page is a
  store write that syncs, not a request to the daemon), `State(State)`,
  `DeferUntil(Timestamp)`,
  `Deadline(Timestamp)`, `PaceDays(u32)`, `SlackHandledThrough(Ts)` and
  `AgentHandledThrough(AgentEventPos)` (the verdict cursors, one per
  source, at that source's own position), `Deleted(bool)`,
  `CreatedAt(Timestamp)`.
- Many per subject, one boolean LWW cell per payload: the store key is the
  subject, the variant, and the payload; the cell says present or absent.
  `Labeled(Id::Label)` (the tag rule as today: opposing writes at the
  same version choose add). Any future property the user can have several
  of is this shape.
- `Body` (note) is the text CRDT, keyed by subject.
- The verdict log: `(Id, stamp) → VerdictEvent`, grow-only, merged by
  union. History and undo.

The payload is where detail lives that the id does not carry. Ids stop at
the unit (a Slack thread, an agent, a page), but a property can name the
exact thing inside it, and the variant is as specific as the fact:
`FromSlack { unit: SlackUnit, message: Ts }` on an agent records the very
message that led to spawning it, though no id exists for a message;
`FromPage { page: PageId, url: Url }` records the page and the exact
address that led to it. One variant per source, never a generic `From`
with an id that could be anything; a property may carry several ids where
one fact genuinely joins several things. The same rule bounds it: a
payload is typed, never a string that means something. The enum is the
whole schema: modelling a new fact is adding a variant that says exactly
what happened, with exactly the data it needs, and nothing has to be
generalised to fit an existing shape.

Gone from the cell vocabulary: `kind`, `agent_id`, `host`, `page_ref`,
`url`, `workspace`, `channel`, `thread_ts`, `repo`, `number`, `path`. Each
was either the identity (now in the id) or a source fact (now derived).
The merge rules, stamps, device ids, and sync-since-version are exactly
today's; what changes is the key: `(Id, property variant[, payload])`
instead of `(NodeId, Field)`, with `Id` typed and the value folded into
the property.

The machine writes nothing here. Not a reference node, not a parent, not
a title. The only writer is a user's key, through a verdict or an edit.
"The desk is 100% user-written" is now literally true of the store.

**Why:** the CRDT primitive was never the problem; per-field LWW plus a
grow-only log plus a text CRDT is already "a set of facts about an id".
Keeping it means slice 1's merge, stamps, and sync code stay. Restricting
the store to user facts means it can never disagree with a source, because
it never repeats one.

### Source facts are derived, never stored

The same shape, a property with its payload on a subject, computed at read time from the system that owns it,
in the GUI, where the sources already live:

- `spawned_by` (agent → agent), `on_host` (agent → host), `in_workdir`
  (agent → file): the registry.
- `in_channel` (Slack thread → channel), `in_workspace` (channel →
  workspace), `newest`, `newest_from_other`, `newest_author`: the Slack
  mirror.
- `opened_from` (page → page): the browser. A `ctrl-click` that opens a
  tab records where it came from, so the tab lands under its origin.
- `title`, everywhere: a note's first line, a label's name, the agent's
  label, the page's title, the Slack unit's subject; a stored `Name`
  overrides any of the derived ones.

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

Anything backed by a source is closed by a cursor at that source's own
position, never by a state: `d` on a Slack unit writes
`SlackHandledThrough(newest)`, `d` on an agent writes
`AgentHandledThrough(the position of its latest event)`. The card is
open again the moment the source has an event that wants the user past
the cursor (a reply from them, an agent turn ending on a question or a
tag), with a fresh wait; the user's own message to either never reopens
anything. No wall clock from one system is ever compared with another's.
`State(Done)` is for what nothing external can reopen: notes and labels.
`x` mute (the verdict formerly called discard; renamed 4 Sep because
what it does is stop the thing from raising its hand): the cursor,
plus `state := muted` so the thing stays out of Home even when it
speaks again until the user opens it, plus the
source's own silence where it has one (a thread unfollowed, a
conversation marked read). `s` snooze:
`defer_until`. `t` todo: as today, plus for Slack the cursor. `f` file:
`parent := the chosen id`, any id, picked with Find. `l` label:
`labeled += the chosen label`, created if new. `u` undo: the log entry
names the facts it changed and their old values; undo writes them back.
Every verdict is a log entry first.

## Direction: the daemon shrinks to coordinator and agent runner

Not a decision yet, the user's read on 4 Sep, recorded so slice 1 does
not build against it. Once the store holds only the user's facts and
every source fact is derived where the source lives (Slack and the
browser already in the GUI, agent activity in the registry), the daemon
is left with two jobs: running agents on a host, and being the peer the
other devices sync the store through. Nothing about the store needs a
daemon: it is a set of cells with stamps and a version, and a GUI can
hold one and sync it peer to peer. So the store's API is written as a
store, not as "ask the daemon": the GUI reads and writes facts through
one interface whose one implementation today talks to the daemon, and
whose next one is local. The wire protocol carries cells, not desk
commands.

### Sync, later: the daemon as a holding relay

The user's read on 4 Sep, not built and not part of any slice yet. The
daemon should not need to read the graph, which may hold sensitive
text, and the clients are rarely online at the same time, so sync is
store-and-forward through the one always-on party the user already
runs. The model is logs and paths: every device owns an append-only log
of encrypted segments, each tagged (device, version), and a path is any
way of copying segments to another device. A direct iroh connection
streams them live when both are up; the daemon is a path that holds,
storing segments until the other device asks; a local network or
bluetooth path copies them when the devices are near; all the same two
operations, append a segment and read every segment since a (device,
version). No path has request-reply state, because the CRDT makes
segments idempotent, order-free, and safe to receive twice; the only
bookkeeping is the highest version seen per device, which the store
already keeps. Clients coalesce cell writes for a
few hundred milliseconds, encrypt the batch with a key only the clients
hold (entered once per device, or passed by QR), and append; a client
that is up receives the other's batches on the socket as they land, and
one that was away reads the log on connect. The daemon sees device ids,
versions, sizes, and timing, never contents. Losing the key loses sync,
not data; every client holds its own full copy. Consequence to decide
then: agents cannot read the graph either, so anything they should know
from notes is handed to them by the GUI on purpose. Chosen over an S3
log (no push, a request per batch) and over a git repo of batches
(fine for sessions, wrong for keystrokes); either is a second
implementation of the same two operations if the log should ever
outlive the host. Slice 1's store interface is what makes this a swap:
its one implementation today talks to the daemon in the clear.

What the vendored stack needs for it (explored 4 Sep; `iroh` 1.0 and
`noq`, its QUIC, are under `vendor/`): no change to QUIC. Holding at the
packet level was looked at and rejected: the relay server
(`iroh-relay`, `server/clients.rs`) forwards QUIC datagrams of a live
connection between two connected endpoints, and a datagram held for
days belongs to a connection whose other end has long since restarted;
making that work means persisting connection state, keys, and loss
recovery across process restarts on both clients, a fork of the
protocol for no gain. The hold is an application protocol over a
second ALPN (`rho/sync/1`) on the daemon's existing endpoint: sealed
segments on streams, a redb table keyed (device, version), and the same
protocol served by each client so the direct path is the same code.
Three things change in the network layer: the GUI's iroh key becomes
stable (today `bind_ephemeral_iroh_client` generates one per process
and trusts it over SSH each launch), because segments are indexed by
device and clients must be able to reach each other; the daemon's
listener accepts two ALPNs instead of one; and the relay both sides use
for NAT traversal (`presets::N0`, n0's public servers today) can be the
vendored `iroh-relay` run by the daemon on its host, so no third party
sees even the metadata. Local discovery for the near path is iroh's own;
bluetooth is not, and waits. Assumed (the user, 4 Sep): two peers that
are both up can hole punch, so the relay's job is rendezvous, the
address exchange the punch needs, and the fallback for the rare pair of
networks that will not punch; it is not the data path. The daemon's
lasting sync role is the peer that holds while one side is away.

### Direction: what the daemon stores in the end

Recommended 4 Sep, corrected the same day by the user, not built. Per
agent, two things: the raw event log, and a head. The run config lives
in the log too (the user, 4 Sep): creation is the first event, carrying
role, runtime, workdirs, spawned by, and the name given at spawn when
one was (so no title is generated for it); a role change, a workdir
added, a rewind, a compaction are events after it. There is no agent
record. The head is the daemon's cache of the fold over the log: the
latest position, the current config, the generated title, and the
usage totals, nothing the daemon had to judge, rebuilt from the log if
ever lost. Whether an agent wants
the user is the client's derivation from the projected log's tail, and
the client keeps it in a cache of its own per agent (the user, 4 Sep),
so the daemon never computes attention; the head is what the old
record's activity, updated at, and last-message columns were for,
knowing where an agent is without loading its log. The daemon
publishes every head eagerly (that is the agents list) and the
projected log as "segments since the position you hold"; a client that
has never seen an agent takes its whole projected log in the background
(the user, 4 Sep: a one-time cost, and increments after that are tiny),
so the mirror is complete and nothing loads on open. A title or a cost
total never waits on a log. The lineage table folds into the log as well, since rewind is an
appended event. Daemon-wide: the machine identity
and iroh secret, the trust list, provider secrets, quota observations
from providers, the counters and format markers, and, later, the held
sync segments. Gone from the daemon: every `rho_desk_*` table once the
user has restarted on the slice 1 build; the agent record's display
name and parent (store facts: `Name`, `Parent`); the presentation
cache in its current form (the head and the projection replace it);
`view_config` (a client setting, store facts); `projects`, which become
ids: a project is `Id::File { host, path }` with a `Name` and a
`Project(true)` property, no new kind, and the host an agent runs on is
not stored anywhere because the client learns it from which daemon
published the agent.

### Direction: the agent API is log replication plus one focus stream

Decided 4 Sep with the user, not built. Today the daemon must load an
agent's runtime to render it, which is why the wire has per-agent
subscribe and unsubscribe, idle unload, attention broadcasts, and turn
reports. In their place: `Ready` carries every agent's head (position,
generated title, usage totals); one request carries the client's
position for every agent it knows, and the daemon streams only the
tails past those positions, agents the client has never seen whole,
served as range reads from the persisted log with no runtime loaded;
one connection-wide follow pushes new projected events for all agents
as they land; and `AgentStreamFocus` stays as the only per-agent thing,
the one agent on screen getting streaming deltas (partial text, a tool
in flight) ahead of the log, replaced by the log's tail when the turn
completes, so nothing durable travels only on the focus stream. Missing
data is found with a version vector, one position per agent, because
every log is append-only and totally ordered; the same vector, one
position per device, is how the store syncs. A tree of hashes over the
vector is the escalation if the vector itself ever costs something, not
before. Rewind is an appended event ("rewound to P"), never a
truncation, so positions only grow and the client drops its view past
P. The projected event carries today's `UiBlock` shapes, so client
rendering does not change; the projection is "which blocks did this
raw event produce". Gone from the wire: `SubscribeAgents`,
`UnsubscribeAgents`, `AgentSubscribed`, `AgentUnloaded`,
`AgentAttention`, `AgentTurnReport`, and the summary's attention, facts,
updated-at, and last-active fields.

### Direction: agent transcripts are logs too, and the client decides attention

The user's read on 4 Sep, not built and not a slice yet. An agent's
transcript is an append-only log owned by the daemon that runs it, the
same shape as a device's store log: segments tagged (agent, version),
copied to clients by the same paths, mirrored on the client the way
Slack is (tail first, older pages on demand, chunks with gap records,
never a background walk), so the transcript reads from disk before the
daemon answers and reads offline. "Waiting on you" stops being a fact
the daemon computes: the client derives it from the mirror, the way it
derives a Slack unit's state from the mirror, from the last event and
the agent-wants tags the transcript already carries (`<ask-human/>`
and the rest, `AGENT-WANTS-DESIGN.md`). The daemon then emits events
and takes commands (create, send, cancel, rewind, continue, compact)
and decides nothing about attention; Home for agents works offline from
the mirror; and there is one sync engine for store logs and agent logs
instead of a store protocol and a separate agent stream. The log is
not the raw transcript: the daemon projects the persisted `AgentEvent`
log (`rho-agent`, positions `AgentEventPos`) into a small typed event
log per agent, the story a person reads, and only that is mirrored. The
projection is a pure function of (position, event), so it can be
rebuilt from the raw log at any time, and a projected segment's version
is the `AgentEventPos` it reached, so "since" means the same thing on
both sides. Turn started and ended; the user's message; the agent's
visible reply; a tool call as its name and one typed line of what it
did (the file, the command, the search), never its output; an
agent-wants tag; cancelled, rewound, compacted, renamed; cost per turn.
Tool output, diffs, and the model's raw exchange stay on the host and
are fetched on demand when the user opens that call, the way a Slack
picture's bytes are. A projected log is small, so the client mirrors every
agent's whole projected log, in the background, once, and then only
increments; the head above and the client's own per-agent cache of
what it derived (wants the user, last speaker, wait) are what Home,
Find, and the map read. Every host is
then a peer that publishes its agents' event logs, and a GUI is a peer
that publishes its device log.

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
  cursor is slice 2's property, and done-ness on the old model was
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

1. Store: `Id`, `Property`, the cell key change, the migration, the
   views (map, Home, Find, notes-for-this, paths) reading through the join
   of store and sources, `DeskPageBind`, `DeskThreadBind`, and agent-node
   creation gone. `body` is the cells store's own text table re-keyed;
   the native tree's text and `Document`/`TreeOperation` wait for slice
   2. `parent` keeps an explicit none (un-filing is a write, not a
   delete); none and absent both read as root. Daemon change. Landed 4
   Sep (f31fdbd9): wire RUP7; `desk_migration.rs` runs once and drops
   the old tables; on a read-only copy of the user's real store it kept
   223 notes, 109 agents, 7 pages, 1 label, 221 bodies, 2381 facts, 6
   verdicts, and dropped 194 machine-made thread nodes and the 101
   verdicts on them; no Slack unit survived because none had been filed.
   The first run also dropped 12 file nodes that had a path and no host;
   fixed the same day (e0f86ac7) before the user restarted: a host-less
   file is on the daemon that stored it, and the 12 nodes are 7 files
   (five were second nodes for a path that already had one, merged by
   the id), all live and filed under notes. Two bugs found on the way: undo of a
   fact nobody had written read its before-value as none, fixed by one
   `unwritten()` definition shared by writer and checker; and redb
   records the Rust type names a table was created with, so the legacy
   decode needed `SenAs<T, N>` to answer to the recorded name, which no
   fresh-db test could have caught.
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
