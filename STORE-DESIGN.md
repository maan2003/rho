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
  the variant; a newer stamp replaces the payload. `Parent(Option<Id>)`
  (the GUI writes and placement reads it only from a label to another label
  or to none), `About(Id)` (used on a note), `Name(String)` (a
  label's name; on any other id the user's override of
  the derived title, so renaming an agent, a Slack unit, or a page is a
  store write that syncs, not a request to the daemon), `State(State)`,
  `DeferUntil(Timestamp)`,
  `Deadline(Timestamp)`, `PaceDays(u32)`, `SlackHandledThrough(Ts)` and
  `AgentHandledThrough(AgentEventPos)` (the verdict cursors, one per
  source, at that source's own position; nothing writes
  `SlackHandledThrough` since 8 Sep, and nothing reads it after the one
  seed that moved it into the Slack mirror -- the cells stay where they
  are and nothing deletes them), `Deleted(bool)`,
  `CreatedAt(Timestamp)`. `CreatedAt` is never zero; when creation time is
  unknown there is no cell.
- Many per subject, one boolean LWW cell per payload: the store key is the
  subject, the variant, and the payload; the cell says present or absent.
  `Labeled(Id::Label)` on things (the tag rule as today: opposing writes
  at the same version choose add). Labels use `Parent`, not `Labeled`. Any
  future property the user can have several
  of is this shape.
- `Body` (note) is the text CRDT, keyed by subject.
- The verdict log: `(Id, stamp) → VerdictEvent`, grow-only, merged by
  union. History and undo.

The payload is where detail lives that the id does not carry. Ids stop at
the unit (a Slack thread, an agent, a page), but a property can name the
exact thing inside it, and the variant is as specific as the fact:
`FromSlack { unit: SlackUnit, message: Ts }` on an agent records the very
message that led to spawning it, though no id exists for a message;
`FromPage { page: PageId, url: Url }`, also on an agent, records the page and
the exact address that led to it. `About(Id)` on a note records that the note was
written about a thing such as an agent or Slack unit. One variant per source,
never a generic `From`
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
  tab records where it came from. This is provenance, not placement.
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

- Place, for the map and for paths: labels alone. Labels nest through
  `Parent`; a thing appears under every label named by its `Labeled` cells
  and otherwise at the root. A label chain that cycles or reaches a deleted
  label is shown at the root.
- Matters, for what the map and Find show at all: an id with any user
  fact, or open by source facts, or a label-ancestor of one that is. So
  not every Slack channel, not every finished agent, not every tab.
- Home (`HOME-DESIGN.md`): dealable if open by source facts (an agent
  waiting, a Slack unit rho-slack says has attention, which is that
  crate's own join of its cursor and Slack's read mark) or by
  user facts (`defer_until` reached, with `pace_days`), filtered by
  `state`; curves per id kind as today.
- Notes for this: notes whose `About` names this id.
- Find: every id that matters, matched fuzzily against its title and its label
  paths, as today. `spawned_by` remains derived from the registry;
  it is never stored or used as placement.

**Why:** the user's words, 4 Sep: "the raw storage stays flexible and then
you convert it to visual with a set of rules". Every "where does this
live" question becomes a rule that can change without a migration.

### Labels are the filing; parent is only for labels

The user's revised call, 7 Sep: "why should they have parents, we should just
use labels for things right" and "convert archived things to done". Labels
carry all structure. A thing (note, agent, page, Slack unit, pull request, or
file) has no parent; it carries only `Labeled` cells and otherwise sits at the
root. Labels nest through `Parent`.
The store accepts `Parent` on any subject like every other cell, but rho never
writes one on a non-label and the placement rule never reads one. The user's
7 Sep correction: "no parent refusal must not happen at daemon. ok about is
reasonable."

`f` opens the label picker. A label path (`rho/agent`) adds that label to the
thing (a thing can carry several; naming the same path again takes it off),
and the label's own `Parent` chain makes `rho/agent` count as `rho`
transitively. Find ranks filing over label paths alone. Relations do not
pretend to be structure: `FromSlack`, `FromPage`, and a note's `About(Id)` say
what happened, while agent `spawned_by` comes from the registry. The map shows
the label tree with things under their labels and unlabelled things at the
root. Workdir inheritance walks the label chain; a label may carry a `Project
{ host, path }` property, so a project is a label with a workdir rather than a
separate id. `f` writes only `Labeled`; it never writes `Parent` on a thing.
Built and landed as d8625568 (GUI plus the `Project` property in
rho-desk, so the daemon's profile moved there too): one picker on `f`,
label rows first ("label · enter takes it off" when the thing already
carries it), then things; `DashboardLabel` and `prompt_label_card`
gone; Find ranks over label paths (`rho/agent › title`); a Slack room
is findable because Slack says it exists, and its desk labels are
joined on by the room's unit; `area_workdir` inherits `Project` up the
label-parent chain.

### Labels are ids, not strings

`Label(uuid)` with a `name`, nested with its own `parent`. In the picker
the user types a name; `rho/agent` finds or creates label `agent` under
label `rho`. Renaming is one cell; two labels cannot drift into `rho` and
`Rho`; a label can carry notes and a defer like anything else. The user
never sees the id.

**Why:** the no-stringly-fields rule, and rename for free.

### Verdicts write facts

Anything backed by a source is closed by a cursor at that source's own
position, never by a state: `d` on an agent writes
`AgentHandledThrough(the position of its latest event)`. A Slack unit is
the same rule in a different file (8 Sep): its cursor is rho's own half of
a join with Slack's read mark, both halves live in the Slack mirror, and
`d` on a unit writes no cell at all, and a unit is a node here only while
it carries a cell Slack has no place for -- a filing, a name, labels,
About, a filing, a dismissal -- so the cursor cells older versions left
behind are read by nothing and shown by nothing. A mute on a unit is not
one of them (8 Sep): a channel or direct message is muted in Slack and a
thread is unfollowed there, so the `State(Muted)` cells older versions
wrote are left in the store unread too.
`SLACK-DESIGN.md`, "How a Slack unit sits in rho". The card is
open again the moment the source has an event that wants the user past
the cursor (a reply from them, an agent turn ending on a question or a
tag), with a fresh wait; the user's own message to either never reopens
anything. No wall clock from one system is ever compared with another's.
`State(Done)` means archived for every kind of thing. There is no archive
label and no `:archived:` stamp line in a note body.
`x` mute (the verdict formerly called discard; renamed 4 Sep because
what it does is stop the thing from raising its hand): the cursor,
plus `state := muted` so the thing stays out of Home even when it
speaks again until the user opens it. For a Slack unit there is no cell
at all (8 Sep): the mute is made where the source keeps it, a channel or
direct message muted in Slack and a thread unfollowed, and the card
closes because Slack has stopped asking. Undo is the same call the other
way, which is the only thing that brings the unit back. `s` snooze:
`defer_until`. `t` todo: as today, plus for Slack the cursor. `f` file: a
label path adds `Labeled(label)`, created if new, or takes it off if
already there. `u` undo: the log entry
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

### Direction: agents as logs

Moved to `AGENT-LOG-DESIGN.md` on 5 Sep, when the user chose to build
it: what the daemon stores per agent, the agent API as log replication
plus one focus stream, and transcripts as projected logs with attention
decided on the client. That document is the record; nothing about
agents is decided here.

### Direction: store sync is its own crate, mounted on a stream

The user's call, 4 Sep. The held store segments, the blind relay for
the user's CRDT, live in a crate of their own (`rho-sync`) that knows
nothing about agents or the daemon: a device id and append-only
encrypted segments with versions; the crate serves the version-vector
exchange, the tails, and the follow, over one stream. The daemon
mounts it: the connection is authenticated first by the existing iroh
trust handshake, as every stream already is, and then a sync stream is
opened like a terminal or a channel stream today, its first frame
selecting the handler, and the crate owns the stream from there.
Daemon code never touches store segments; the store's contents never
touch daemon code. The client side of the same crate is what the GUI
uses, and, later, what a GUI-to-GUI direct path uses unchanged. Agent
log replication is not this crate: it is the daemon's own protocol
over its own stream, since the daemon owns those logs and reads them
in the clear; the two share nothing.

### Direction: what the daemon offers, as capabilities

The user's calls, 4 Sep, on the simplification pass:

- Transports: the Unix socket is for the CLI in production; the GUI
  uses iroh in production and the Unix socket only in tests. Trust
  (enrollment codes, SSH trust-in-memory, approve and revoke) stays as
  it is for now; folding it into one list waits.
- Usage: the graphs stay, and the data behind them lives on the client,
  derived from the cost events in the mirrored agent logs; the per-agent
  and global usage requests go. Quota observations from providers stay a
  daemon request.
- Telemetry stays, as its own crate and capability mounted on its own
  stream the way store sync is, not intermingled with daemon code.
- Visualizations stay server-side: they are part of the agent
  capability, on their own path if that reads cleaner.
- Iris, the hidden coordinator agent, is disabled: its code stays in
  `rho-agent` but is not integrated at the protocol level; when
  coordination comes back it belongs on the client.
- Realtime (WebRTC) stays as client code, kept for later, out of the
  daemon protocol for now.
- Terminal and shell: one "process on the host" stream with a kind.
- Kept as they are: the land lease, git transport, diffs, PR commands.

## Browser tabs

A tab is `Page(PageId)`; it is never created in the store. The browser's
derived `opened_from` fact records its source page, but that relation is not
placement. Only labels file the page; an unlabelled page remains at the root.
Capture
(`CREATE-DESIGN.md`) is unchanged: a draft page carries the fields the
user typed, and the page exists when the browser opens it.

## Migration

### Third conversion: the parents become labels

A thing is placed by the labels it carries and carries no parent
(`DESK-DESIGN.md`). Rho stopped writing parents on the filing paths on
8 Sep; this reads the parents already in the store and says the same thing
as a label. One shot at daemon start behind the durable marker
`rho_desk_parent_labels_v1`, and then the code goes (the standing rule).

For every non-label carrying a `Parent`: the label that parent stands for is
minted or reused, named by the parent's `Name`, else the first line of its
body, else the agent log's title; that label is nested under the label the
parent's own parent stands for, so `rho/agent` is a path; the thing is given
`Labeled`, any label the new one is nested under comes off, and the `Parent`
is written `None`. A label's own `Parent` is left alone — that is what nests
labels. A parent nobody named cannot become a label, so that thing keeps its
parent rather than losing the only thing that says where it is. Label ids are
a hash of name-under-parent, so two runs of one store agree cell for cell.

Proved before landing on a copy of `user-2026-09-06`, which predates the
outline conversion and so still carries parents: 917 subjects, 18 labels,
341 parents, 7 things carrying a label and 7 label cells before; the run
reported 341 things carrying a parent, 134 labels minted and 3 reused, 340
things labelled, 0 shallower labels dropped, 340 parents cleared and 1 left
for want of a name; and the store read after it says 1051 subjects, 1 parent,
342 things carrying a label, 347 label cells, marker set.

### Second conversion: the outline recovered as labels (historical)

No copy of the Org text survived the 3 Sep cutover, so this conversion read
the outline back out of the Desk store itself: a heading was a note with at
least two non-stamp children, an Org agent tag was an agent parented under
that note, an archive mark was a `:archived:` stamp-note child, a bookmark
heading owned page children, and `:project:` was a file child plus a label's
`Project`. One shot at daemon start behind a durable marker, and then the
code goes (the standing rule).

What it left behind: headings became labels, parented to the nearest
enclosing heading-label and reused by equal name; agent entries gave their
labels, their Done state and their title-as-`Name` to the agents beneath
them; archives became Done; every remaining non-label `Parent` was dropped,
so nothing but a label places anything. The user ran it on 8 Sep and the
code was deleted the same day (this commit). On a converted clone of their
state it minted 30 named labels, and of 647 agent items 112 carry one; the
labels are their own vocabulary — nixos, rho, fedimint, jj, niri, work,
"Desk rework". The daemon's report of that run went nowhere, because the
daemon installs no tracing subscriber, which is a separate fault.

### First conversion from the native tree (historical)

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
   store and `rho desk cat/checkout` gone. Landed 4 Sep in two commits:
   a4fda1ce (the deletion, 5,124 lines, ALPN `rho/ui/8`, log epoch
   RUP8) and the Slack half after it. One property added on the way:
   `SlackSnoozedAt(ts)`, written by snooze beside `DeferUntil`, because
   "a message newer than the snooze" needs a Slack position to compare
   against and the store has no wall clock; the cursor stays untouched.
3. Labels: the label key, the picker with `rho/agent`, the map's label
   axis. Found while starting it (b8os, 4 Sep): a GUI that writes a
   verdict variant the running daemon does not know aborts that daemon
   at its next start (senax `UnknownVariantId` on the verdict log is a
   panic, not a skip). So any new verdict or property variant lands in
   two steps, the daemon first with the variant known and unknown
   variants made to fail soft (skip and log), then the GUI writing it;
   and a downgrade of the daemon under a newer GUI is not supported.
   Also open: a thing shown in two places on the map is one buffer in
   two excerpts, and anchors resolve to the first, so the second row
   needs per-excerpt anchors; and the key, since `l` on the map is
   vim's right motion (the user picks; the verdict transient is the
   natural home). Landed 4 Sep: the daemon half (bfe30d9d, 406081ea:
   the Label verdict, `Lenient<T>` reads of cells, the verdict log, the
   mutation replay, and note bodies, an edit to an unreadable body
   refused) and the GUI half (1fc7535a: the Labeled property, the
   `rho/agent` picker minting outer then inner, naming the same path
   again takes the label off, filing offers labels, the map's label
   axis with per-excerpt anchors from the public boundary API and depth
   per row rather than per id). The key is unbound until the user
   picks one.
4. Browser: `opened_from` from the embedded browser, tabs under their
   origin. Landed 4 Sep (GUI and extension): the extension adopts a tab
   that has an opener rho knows and carries the origin in the page's
   record, re-sent on every metadata event because the service worker
   restarts at Chrome's whim; a tab's place is its origin until filed,
   filing the origin carries the group in the view, nothing is written
   to the store. Which tabs matter (b8os's reading, accepted): a tab
   opened from a page, plus its origin transitively; a tab opened for
   its own sake stays off the map, which is what keeps ctrl-t from
   filling it. No rig screenshots: the rig host has no browser; a fake
   browser speaking the native-host protocol (the fake Slack pattern,
   via `RHO_CUSTOM_BRAVE_BIN`) is queued as its own task. A latent GUI
   panic when the browser was never set up (`list_pages_if_running`)
   was found and fixed on the way.
   The fake browser landed 4 Sep (b7630f85, `crates/rho-browser/
   examples/fake_browser.rs`, driven over `RHO_FAKE_BROWSER_CONTROL`)
   and found three client bugs, fixed the same day (5bcc3241,
   2f92d952): page metadata only notified and never reconciled the map,
   so a burst stayed invisible until an unrelated event; a row that
   exists only because a source says so had no buffer and drew nothing;
   and `f` and the shift tap adopted whatever card Home held, so over a
   page row they filed a Slack channel. The rule now: with the map open
   the map cursor row is the target, else the surface's own node, never
   a card from another source. And the map draws a thing under its
   label with its subtree and drops it from the root (a label hanging
   under the thing it labels keeps the root row, or nothing would reach
   it). Screens: `br-12-burst.png`, `br-26-map-filed.png`.

   This records the behavior that landed on 4 Sep. The revised 7 Sep rule
   above supersedes its placement semantics: `opened_from` is now provenance
   only, and labels alone place pages.

5. **The client keeps the store.** Decided 5 Sep, after slice B of
   `AGENT-LOG-DESIGN.md` showed that a cold GUI has agents but nothing
   to hang them on: the store's cells, verdicts, version and note bodies
   arrive by `DeskSync` every session and are kept nowhere on the client.
   The GUI keeps its own copy per host, on disk, beside the agent mirror
   (`crates/rho-gui/src/mirror.rs`, host by name, a doubted row dropped
   and asked for again): the confirmed `Store` (cells, verdicts, version)
   and every `BodySnapshot`'s operations and transactions, written by the
   same off-thread writer, replayed before any daemon answers, so Home,
   the map, Find and note bodies read from disk first and `DeskSync`
   asks with the real `known` version for the delta only. Two things
   the wire must give for that to hold: bodies since a version, not whole
   bodies on every sync (the client sends what it holds per body, the
   daemon answers with the operations it lacks; b8os finds the smallest
   honest shape), and pending mutations that survive a restart: the
   `pending` queue and any unsent text operations are on disk too and go
   out on connect in order, which is what makes a verdict or a note edit
   taken offline real rather than lost. That queue no longer runs into
   anything at the daemon: the stamp-jump rule, which an offline batch
   broke by design, is gone with the rest of the refusals (9 Sep). Namespaces for the text replica stay the
   daemon's per connection; operations kept from an earlier session keep
   the replica id they were made under. What this is not yet: encrypted,
   or a log the daemon cannot read. It is the device's copy that the
   rho-sync direction above turns into a device log later; the store
   interface from slice 1 is unchanged, so that swap stays a swap.
   Offline Home is claimed when this lands (the agent mirror alone gives
   heads and stories, `AGENT-LOG-DESIGN.md` slice E). Rig proof: GUI
   started with no daemon shows Home with breadcrumbs, the map with
   labels, a note body; a verdict and a note edit taken offline arrive
   at the daemon on reconnect and a second GUI sees them.

Each slice lands on its own with the tests of the slices before it green.

## A device is one GUI, and the newest window wins it

A device id names one writer in the CRDT: each device has its own
namespace and its own version counter, so two live connections writing
under one device id would mint the same versions for different writes.
The daemon therefore lets one connection hold a device at a time.

The first shape of that guard refused the *second* connection, and it was
wrong in exactly the case it mattered. The user's GUI panicked and
restarted, and the restarted one was told
`Desk device already has an active writer connection` — the hold belonged
to a connection nobody was on the other end of, and it is released only
when its handler loop ends, which over an iroh relay means waiting out
`rho_iroh_auth::AUTHENTICATED_IDLE_TIMEOUT`: **ten minutes** with no desk.

So the hold goes to the newest: a `DeskSync` for a device that is already
bound displaces the connection holding it. The one already there is dead
or stale — it is the same device, which is to say the same GUI — and the
window in front of the user is the one that should have it. The displaced
connection is told in words it can show ("The desk moved to a newer window
on this device"), its read loop is woken so it ends rather than sitting on
a socket nobody reads, and its session is marked displaced.

The guard itself is not weakened, only pointed the other way: a displaced
connection may not write. Its `DeskMutationApply` is refused with the same
sentence and its `DeskTextApply` errors, from the moment it is displaced
and whether or not its socket has noticed, so there is never a second live
writer in one device's namespace. A connection ending releases the device
only if the hold is still its own, so the window that took it keeps it.

## The daemon does not refuse a desk mutation

The user, 9 Sep: "it is not job of daemon, remove it!" The desk store is
the client's; the daemon holds a copy so that clients can sync through it
and catch up. A copy does not get a vote on what the user wrote.

So the daemon takes every mutation it can decode and merges it. Gone: the
verdict shape check, the before/after check against the cells, "not
applied by its mutation", "cannot remove a fact", and the stamp-jump
refusal. Last-writer-wins is the whole of the merge rule, and it needs no
frontier to enforce: a stamp the store has already counted merges as
itself, an older one loses to what beat it, a newer one wins. What is
left at the daemon is about the connection, not the verdict: a mutation
must carry this connection's device, a connection must sync before it
writes, and a displaced connection may not write at all (see above) —
each of which breaks the connection rather than answering it.

`DeskMutationRejected` is gone from the protocol, and with it the
client's rejection paths: the replay queue of pending mutations, the
view rebuilt from `confirmed`, and the taking-back of what a verdict's
answer promised. A mutation the store cannot decode at all is logged and
dropped; nobody is waiting for an answer to it.

Undo moves to the same footing: an entry whose before-values no longer
stand is nothing to put back, decided on the client. Undo returns what
the verdict took away, and it is not a way to reach past a write made
after it.

Why this was ever there: the daemon was the store and the client was a
view of it, so the daemon was the place that could say no. The direction
above (the daemon shrinks to coordinator) reverses that, and a refusal
in the middle only ever took back writes the client had already shown
the user.

### The desk lives on the client

The user's goal, 9 Sep, and the operative design: the desk store lives on
the client, and the daemon holds a copy only so that clients can sync
through it and catch up.

What that costs in code, taken with the refusal removal: a verdict is
complete the moment it is in the client's own replica, so the write goes
to the view and to the disk before the message goes out, and the undo,
the dealer, the card and the echo all happen there. `DeskMutationAccepted`
is gone from the protocol, along with the maps that held a verdict, an
undo and a paste's text until it came back. The gate that holds the
dealer until the store has been read now reads "the client's replica is
loaded", and the replica is opened in `Workspace::new` before a socket
exists.

Sync is two ways for the same reason: the daemon sends the cells above
the client's `known`, and the client sends the cells above the daemon's
frontier (`DeskCellsApply`). A copy that only ever received would lose
any write made while it was away, since a write is complete on the client
and nothing replays it.

What is still owed: a note's body does not resume from the mirror, so
every sync carries the whole desk's prose and the reader's text is the
one thing the replica cannot give them at open. Per-body versions are
the fix, and that is the next step on this path.

## Symptoms to watch for

- A fact in the store that a source could have answered.
- An id minted for anything but a note or a label.
- A title stored rather than derived.
- A verdict that changes facts without a log entry.
- A view rule enforced by rewriting storage.
- A restarted GUI refused its own device.
- The daemon judging what a client wrote rather than merging it.
- A verdict on the client waiting for a daemon to say it happened.

## What done means

One store of the user's facts, typed ids that are the sources' own,
verdicts and the Slack cursor visible on every device, typed relations such as
a note about anything or a page opened from another page, labels as the only
placement structure, and every "where is this shown" answer a rule in one
place.
