# Agents as logs

Decided with the user on 4 and 5 Sep; the three "Direction" sections
about agents in `STORE-DESIGN.md` moved here when the user chose to
build them ("lets do 2 and 3 now"). The store design stays the
reference for the user's facts; this is the daemon's side of the same
idea: an agent is an append-only log the daemon owns, and everything
else about it is derived.

## The problem

The daemon keeps five things per agent: an `AgentRecord` (role,
runtime, workdirs, spawn name, generated title, parent, display name,
labels, disposition, turn report, last-message snippet, activity,
timestamps), the raw `agent_events` log, an `agent_presentation_events`
cache, a `lineage_parents` table for rewinds, and usage aggregates in
three tables. The record and the log can disagree; half the record is
either a store fact now (`Name`, `Parent`, `Labeled`) or an attention
judgement the store design says the daemon must not make. To render an
agent the daemon loads its runtime, which is why the wire has
per-agent subscribe, attention broadcasts, turn reports, and a summary
with `attention`, `facts`, `updated_at`, `last_active`, `activity`,
`hidden`, `disposition`, and `last_user_message_text`. The GUI cannot
show an agent offline, cannot rank agents without the daemon's verdict,
and a title or cost total waits on a runtime.

## Decisions

### An agent is a raw log, a story log, and a head

Per agent the daemon keeps:

- **The raw log**, `agent_events`, as today: the runtime's own record,
  positions `AgentEventPos { lineage, seq }`, lineages forked on rewind
  through `lineage_parents`. This is the runtime's business and is not
  reshaped here; it gains the creation and config events below.
- **The story log**: a small typed append-only log per agent, the story
  a person reads, positions a plain `StoryPos(u64)` that only grows.
  Written by the daemon as things happen, beside the raw event for the
  Rho runtime and from the Claude stream for the Claude runtime. It is
  what clients mirror. No tool output, no diffs, no raw model exchange.
- **The head**: the daemon's cache of the fold over both logs: story
  position, current config, current lineage, generated title, activity,
  usage totals, whether a turn is running. Rebuilt from the logs if
  lost; never the source of anything. A field joins the head only once
  the logs can rebuild it: `turn_running` in slice B (with
  `TurnStarted`/`TurnEnded`), `usage_total` in slice C (with `Cost`);
  until then usage is read from the existing totals table so there is
  one number.

There is no `AgentRecord`. There is no `agent_presentation_events`
table: a generated title or activity is a story event
(`Titled`, `Activity`) and the head remembers the latest.

### Creation is the first event

`AgentEvent::Created { role, runtime, workdirs, spawned_by, spawn_name,
created_at }` is the first raw event of every agent; `RoleChanged`,
`WorkdirAdded`, `RuntimeRebound` (a Claude rewind landing on a new
session) follow as they happen. The head's config is the fold. A spawn
name stays in the creation event so no title is generated for it, as
today. `parent_agent` is not here: a parent is the store's `Parent`
fact, written by the client that spawned the agent (the daemon still
passes the parent id through on `NewAgent` for the runtime's mail, but
stores it only as `spawned_by`).

### What leaves the record, and where it goes

| record field | goes to |
| --- | --- |
| role, runtime, workdirs, spawned_by, created_at, binding | `Created` and the config events |
| display_name | `Created.spawn_name` (it was also what `RenameAgent` wrote, so no agent loses its name; a given name still beats a generated title; the store's `Name` overrides both) |
| labels, parent_agent | already store facts (`Labeled`, `Parent`); dropped |
| generated_title, activity | story events; head caches the latest |
| updated_at, last_turn_ended, last_user_message, last_user_message_text, disposition, turn_report, user_interacted | slice A: a transitional table named for its deletion (`agent_attention_until_slice_b`), because raw events carry no wall clock and these are judgements; slice B: the times become a fold over story events (every `StoryEvent` has `at`), the judgements become the client's attention cache and the table is deleted |
| claude_rewind | a raw event (`RuntimeRebound` pending, then confirmed) |
| current_lineage | the head |

`projects` goes too: a project is `Project { host, path }` on a label
(`STORE-DESIGN.md`), and `ProjectSet`, `ProjectRemove`, and
`Ready.projects` leave the wire. `view_config` goes: it is a client
setting and lives in the GUI's own db. Both wait for slice B, because
the user's project paths were never converted and the Workdir field's
completions read them: in B the GUI converts `Ready.projects` once into
labels carrying `Project { host, path }` (the user's own data, written by
their GUI, not the daemon) and moves view_config into its own db, and
only then does the daemon drop the tables.

### The story events

One enum, `StoryEvent`, every variant typed, no strings but the ones a
person wrote or the model said:

- `Created { role, runtime_kind, workdirs, spawned_by, spawn_name, at }`
- `UserMessage { text, at }`, `AgentMail { from: AgentId, text, at }`
- `TurnStarted { at }`, `TurnEnded { at, outcome: Completed | Cancelled | Errored { message } }`
- `Reply { text, at }` — the agent's visible message text, whole, once the turn wrote it
- `ToolCall { name: ToolName, what: ToolLine, at }` — one typed line: the path, the command, the query; never the output
- `Wants(AgentWant)` — the tag the reply ended with (`AGENT-WANTS-DESIGN.md`), when there was one
- `Titled { title }`, `Activity { label: Option<String> }`
- `Cost { usage: AgentUsageBucket }` per turn
- `Rewound { to: StoryPos }`, `Compacted { at }`, `RoleChanged { role }`, `WorkdirAdded { workdir }`
- `HistoryUnavailableBefore` — the first event of a migrated Claude agent whose session file is gone

Rewind is an appended `Rewound { to }`; positions never go backwards
and the client hides its view past `to`. The daemon finds `to` through
a side table of its own, `agent_story_source: (AgentId, StoryPos) →
AgentEventPos`, written for the events told from the raw log, so the
event clients copy carries no raw position. A projected segment is
`(AgentId, StoryPos)` and "since" means the same on both sides.

`Reply` is whole for both runtimes. Rho replies always were; Claude
replies had been capped at 1024 bytes because the only durable copy rho
kept was the mirror that feeds the title sidecar, deliberately capped so
Claude's transcript would not become a second unbounded local copy. The
story is that copy now, by decision (5 Sep): 105 Claude agents made
25 MiB with the cap on, and a reply is bounded by the model's output
limit. The sidecar mirror keeps its cap. Not told yet: Claude
compactions, because the stream has no mapping for them.

Tool output, diffs, reasoning, and the raw exchange stay on the host
and are fetched on demand when the user opens that call:
`AgentDetail { agent, story_pos }` answers with today's `UiTool` body
for that call, from the raw log, no runtime loaded.

### The wire is log replication plus one focus stream

- `Ready` carries every agent's head: `UiAgentHead { agent_id, story_pos,
  role, runtime_kind, workdirs, spawned_by, parent, spawn_name,
  generated_title, activity, turn_running, created_at }`. That is the
  agents list; a title or a workdir never waits on a log.
- `AgentLogs { known: Vec<(AgentId, StoryPos)> }`, sent once after
  `Ready`: the client's version vector, one position per agent it holds.
  The daemon answers with `AgentStory { agent_id, from: StoryPos, events }`
  for every agent past the client's position, agents the client has
  never seen from zero, served as range reads from the story table with
  no runtime loaded. The whole first mirror of an unseen daemon is a
  background copy, once; after that increments are tiny.
- One connection-wide follow, implicit after `AgentLogs`: every new
  story event on any agent is pushed as `AgentStory` with one event.
  Heads are pushed as `AgentHead` when they change.
- `AgentStreamFocus` stays as the only per-agent thing: the one agent on
  screen gets today's `AgentRemoteFrame` deltas (partial text, a tool in
  flight) ahead of the story, and the story's tail replaces them when
  the turn completes, so nothing durable travels only on the focus
  stream.
- Commands stay: `NewAgent`, `SendUserMessage`, `CancelTurn`,
  `RewindAgent`, `ContinueTurn`, `CompactAgent`, `ChangeAgentRole`,
  `ChangePromptCacheKey`. `RenameAgent` and `AgentLabel` go (store
  facts). `SetAgentDisposition` goes (client cache).
- Gone: `SubscribeAgent`, `AgentSubscribed`, `AgentAttention`,
  `AgentTurnReport`, `UiAgentSummary` and everything in it, `AgentUsage`
  and the global usage requests (below), `ProjectSet`, `ProjectRemove`,
  `ViewConfigSet`, `Ready.projects`, `Ready.view_config`,
  `Ready.iris_agent` (Iris is disabled; `STORE-DESIGN.md` capabilities).

Every wire change here bumps the epoch and the iroh ALPN, so an old GUI
fails to connect rather than to decode.

### Where the wire shape met the code (b8os, 5 Sep; all accepted)

- Live frames are a set, not one agent: `AgentStreamFocus { agent_ids }`
  replaces `SubscribeAgents` / `UnsubscribeAgents` (a split shows two
  agent panes) as well as the singular subscribe.
- `Wants` has a producer from day one: `StoryEvent::Wants { want, summary,
  at }` written by the Luna turn-report sidecar that exists today
  (`report_needs_you` → `Ask`, `report_fyi` → `Show`); `AgentWant` is
  named after the tags in `AGENT-WANTS-DESIGN.md` so the tag parser
  replaces the producer later without a client change. Without this,
  deleting the turn report would have left Home nothing to rank agents
  on (131 agents carried a report on the copy).
- Hidden and snoozed are the user's verdicts and live only in the
  transitional table: a one-time daemon conversion writes them into the
  desk store as verdict-log entries on the agent id (hidden →
  `State::Muted`, snoozed → `DeferUntil`; 151 and 379 on the copy),
  counts printed on the migrating start, the table dropped after, the
  conversion code deleted after the user's restart. This is the one
  case of the daemon writing to the store, and it is a migration of the
  user's own data, like store slice 1.
- Offline, a `ToolCall` shows its one line and no result until slice D;
  the live frame still has results for a focused agent.
- `StoryEvent` is a daemon type; the wire carries `UiStoryEvent`, a twin
  converted in the daemon the way `UiBlock` and `AgentUsageBucket` are,
  because `rho-ui-proto` builds for wasm and does not depend on
  `rho-agent`.
- `AgentUsage` and the global usage requests stay until slice C, or
  Home's cost column would go blank in between.
- The parent id was nowhere in the log: `Created` carries a spawned-by
  kind, and the 2349 parent ids lived only in the transitional table,
  which the GUI nests delegated work on and the daemon routes mail by.
  `StoryEvent::Parented { parent, at }` is told at creation (and once
  for migrated agents by the backfill), the head folds it, `UiAgentHead`
  carries it. The spawner is a source fact the daemon owns and is never
  written into the store; the store's `Parent` is the user's filing and
  wins in the view: an agent shows under its store `Parent` if any, else
  under its spawner from the head, else at the root, one rule in
  `desk_view`.
- `usage_total` is not in `UiAgentHead` until slice C: folded from `Cost`
  it would read zero for all history while the usage tables still hold
  the truth.
- The transitional table comes off the wire in this slice and is deleted
  in the landing after the restart, with the conversion code that reads
  it.

### The client mirrors the story and decides attention

The GUI keeps every agent's story log in its own redb, the way it keeps
the Slack mirror: tables `agent_story (agent, pos) → StoryEvent`,
`agent_head`, and a per-agent attention cache. Attention is derived, on
the client, from the story tail: the last speaker, whether a turn is
running, the `Wants` tag of the last reply, how long since, and the
user's own verdicts in the store (`AgentHandledThrough(StoryPos)`,
`State`, `DeferUntil`, `Labeled`). "Wants you" is that cache, a
`UiAttention`-shaped enum computed in one function in `desk_view`, the
same place the Slack card is derived. Home, Find, and the map read the
mirror and the cache; the transcript surface renders the story from
disk before the daemon answers, and offline; `AgentStreamFocus` layers
the live frame on top when the agent is open.

`AgentHandledThrough` is a `StoryPos` from now on; a card is open again
when a `Reply` with a `Wants` tag, or a `TurnEnded` with `Errored`,
lands past the cursor, and the skip cursor (`HOME-DESIGN.md`) is the
same position.

### Usage lives on the client

The graphs stay. Their data is the `Cost` events in the mirrored story
logs, summed on the client per agent and per time bucket; the
`AgentUsage` and global usage requests and the three usage tables on
the daemon go. Quota observations from providers stay a daemon request.

### Migration, once

On first start of the slice A build, for every `AgentRecord`: write
`Created` from its fields as the agent's new root lineage (one event at
seq 0, the old root's parent pointer set to it, so the replay is
`Created` then the old events unchanged and every existing position and
fork stays valid; b8os proves this byte-for-byte on a copy), fold the
record into the head and the transitional table, and drop the `agents`
table. The slice B build backfills the story in the
background rather than at start: sizing on a copy (b8os, 5 Sep) put the
whole history at about 843k story events and 350 MiB for 2823 agents,
not large, but decoding a million raw events and parsing 553 MiB of
Claude transcripts would block a restart for tens of minutes. So the
daemon starts and serves as before; each head says whether its story
is built; a background job builds agents most recently touched first,
one agent per transaction, resumable across restarts; an agent loaded
before its turn (a turn, a subscribe) is built synchronously first so
live writes never land ahead of history; the 76 Claude agents whose
session file is gone get `HistoryUnavailableBefore` at once. Whole
history, no cutoff (a 30-day cut would have kept 60% of the events for
52% of the agents, which buys little). `agent_presentation_events` is
dropped once every agent is built. Each migration runs once on the user's
real daemon after b8os has run it on a read-only copy of that store and
reported the counts; the migration code is deleted in the next landing
after the user has restarted on it (the standing rule; `desk_migration.rs`
from store slice 1 is deleted in slice A for the same reason).

## Revision, 6 Sep: the mirror is a pure function of the raw log

The user's calls on 6 Sep, after slice B's wire and GUI half had landed
and the rho-agent2 loop had become the only Rho runtime. Everything
below supersedes the sections above where they differ; the sections
above stay as the record of what landed and why.

### What was wrong with what landed

- The story was a second, hand-written log. The loop told a story
  event beside each raw event, a backfill told the same story again
  from old raw events, a side index (`agent_story_source`) mapped story
  positions back to raw ones so a rewind could be told, and the head
  folded both logs. Three writers of one truth.
- The transcript existed twice: as the fold over the story, and as the
  live `AgentRemoteFrame` snapshot plus block diffs. Opening an agent
  sent a whole snapshot that replaced the story-made view, and tool
  results existed only on the live side.
- Resync was a version vector of one position per agent (2823 pairs on
  every `Ready`), a per-agent cursor per connection on the daemon, and a
  gap on one agent sent `AgentHead`, made the client re-send the whole
  vector, and made the daemon abort and restart the follow. A broadcast
  lag sent a whole `Ready`. Correct, but healing by restart.
- The first copy pushed the whole history into an unbounded channel.

The user's read: the one-time copy is fine (it is one time, and the
wire is zstd-compressed; nothing about it needs to be clever), the
mechanism is not; the story fold is too complicated; the mirror should
be a pure function of the raw event, tool output stripped.

### The mirror event is `strip(raw event)`, nothing else

One function, per event, no state carried between events:
`strip: AgentEvent -> MirrorEvent`. The variants are the raw log's
variants with bodies removed: tool results, images, reasoning and the
raw model exchange are gone; `Sent` keeps its call names and one typed
line each (the path, the command, the query); `Replied` keeps its
visible text, its calls, its usage and `context_used`. Because it is
pure, the mirror is rebuildable from the raw log at any time, on either
side, and there is nothing to backfill: no story table, no
`agent_story_source`, no `story_built`, no head table.

Transcript blocks, heads (config, title, activity, parent, turn
running), attention and cost are one fold over mirror events, in a
crate that builds for wasm and that the GUI runs. The daemon keeps no
derived agent state: at load it reads its own agent's config while
`replay` walks that agent's log anyway; a parent is the child's
`Created` at position zero, one point read; a listing for the CLI is a
scan of position zero of every agent, rare and cheap. The daemon can
call the fold for tests, and does not need it to run.

Tool bodies stay on the host and come on demand, `Detail { agent, pos }`,
from the raw log, no runtime loaded, as slice D said.

### The raw log carries everything a reader needs

Four raw events join the loop's `persist` path, which already owns
every append: `Turn { Started | Ended(outcome), at }`, `Presented
{ title, activity, at }` from the sidecar, `Wants { want, summary, at }`
from the turn-report sidecar (later the tag parser), and `Rewound { to,
at }`. The Claude runtime writes the same events from its stream.

`Replied` carries `usage: Option<AgentUsageBucket>` beside
`context_used`. Why there: usage is provider-reported per model
response (input, cache read, cache write, output) and cannot be
recomputed from text, since the tokenizer is the provider's and a cache
hit is only knowable from what they report; `Replied` is the one raw
event per model response, so it is the only event the numbers can
describe; per response and not per turn because a turn is many
responses when the model loops over tools, and cache read against cache
write per request is exactly the signal the spend work needed; not a
separate `Usage` event because it would follow every `Replied` one to
one and every reader would have to pair them. Today `Replied` carries
only `context_used`, itself derived from these numbers, and the bucket
goes to the usage tables and a `Cost` story event.

Every raw event written from now on carries `at`. Only `Accepted` and
`Created` did; "how long has it been" is the reader's first question
and the story answered it by stamping every event on the way out. Old
events read `at` as zero, which the fold treats as unknown and fills
from the nearest stamped neighbour.

Rewind stops forking. Today `AgentEventPos` is `(lineage, seq)` and a
rewind forks the agent onto a new lineage whose parent points at the
cut (`lineage_parents`), which is why the story needed its own index to
say where a reader's view stops. From now on an agent has one lineage,
`AgentEventPos` is a dense per-agent position that never moves, and a
rewind appends `Rewound { to }`; `replay` hides the range for the
runtime and the fold hides it for the reader, with one rule. The
migration flattens each existing fork once: the abandoned tail stays
where it is, followed by the `Rewound` that hides it. Prompt-cache and
history behave as before. Forking an agent, if it is ever wanted, is a
new agent whose `Created` names another agent's `(agent, pos)` as the
history it starts from; the user's read, 6 Sep: possibly useful later,
not needed now, and nothing here stands in its way.

`AgentEventPos` stays, and stays per agent. The agent log is
self-contained on purpose: resuming one agent is one range read over
`(agent, pos)` and never touches anything daemon-wide. The user's call,
6 Sep: "agent log being separate is very important".

### Cost is derived on the client

The fold multiplies `Replied.usage` by the price of the model in force,
known from `Created` and `RoleChanged` since a role carries its model
binding, from a price table by model and date on the client. The
`Cost` event, `record_agent_usage`, the three usage tables and the
usage requests go. History: old `Replied` events carry no usage and the
tables hold buckets per agent per time bucket, not per response, so the
graphs read the tables for the past and the fold for the future until
the past no longer matters, and the tables are dropped then.

### One journal beside the agent logs, one cursor per client

A second table, `journal: seq -> (agent, pos)`, appended in the same
write transaction as the agent's raw event. The agent log stays primary
and holds the payload under `(agent, pos)`, so an agent's range read is
contiguous; the journal is 24 bytes per entry and only the follow reads
it. No new contention: every append already takes redb's single write
transaction. `Created` is position zero and gets a seq like any event,
so the agent list on the client is a fold and needs no message.

Why one global sequence and not a version vector: there is one writer.
A client's whole knowledge of a host is one integer, the last seq it
holds; a gap is impossible by construction; resume, lag and a cold
start are all "send from my seq".

Not sliding sync: server-side sorted windows exist for a client that
cannot hold the set. The client holds the whole mirror on disk, so
sorting is local. The only ordering the daemon ever chooses is the
order of the one-time copy, and that is journal order, oldest first,
accepted as is. The one knob if a cold start ever hurts is a second
cursor going backwards, `Backfill { before: seq }`, so a cold client
follows live from `journal_head` and fills history newest first. Not
built.

### The wire

```
client -> daemon
  Follow { since: Seq }              everything after since, every agent,
                                     contiguous by seq, forever; a cold
                                     client sends 0
  AgentStreamFocus { agent_ids }     which agents this client is looking
                                     at; never loads one
  Detail { agent, pos }              a tool call's body, on demand
  commands unchanged: NewAgent, SendUserMessage, CancelTurn, RewindAgent,
  ContinueTurn, CompactAgent, ChangeAgentRole, ChangePromptCacheKey;
  every one that names an agent loads it

daemon -> client
  Ready { auth, machine_seed, agent_counter, journal_head: Seq }
  Log { entries: Vec<(Seq, AgentId, Pos, MirrorEvent)> }
                                     pages of at most 512 in catch-up,
                                     one entry at a time live
  Live { agent, live: Live }         one delta: Requesting | Item | Appended
                                     | Retrying | Waiting | Idle
  Detail { agent, pos, body }        body is Results or Response(Vec<Item>)
```

`Live` and `Log` are one ordered feed per connection. A loop writes its
row, the commit hook puts the row on the feed, and the same task then
puts the next `Live` on it; a client applies the row, then the tail.
The live set is server-wide, the union of every connection's focus;
every connection forwards every delta and a client ignores agents it
is not holding. A joiner is told `Requesting`, one `Item` per index,
then the phase; an `Appended` for an index it does not hold is dropped.

Gone: `AgentLogs`, `AgentStory`, `AgentHead`, `Agent { frame }`, the
snapshot and the block diff, `AgentSubscribed`, `AgentAttention`,
`AgentTurnReport`, `AgentUsage`, `Ready.agents`, `UiAgentHead` on the
wire. Every wire change bumps the epoch and the iroh ALPN.

Completeness is one rule per side. Follow: the client holds every seq
up to its cursor, or it re-sends `Follow` from the last one it has.
Lag: the daemon reads the journal from the last seq it sent and carries
on; no `Ready`, no restart. Head changes need no message: title,
activity, turn running, parent and role arrive as `Log` entries.

Live is ephemeral only: the response in flight, item by item and
append by append, and the phase. It is layered on the mirror's tail by
the client and replaced by the `Log` entries when the response lands,
so nothing durable travels only in `Live`. Everything else a reader
wants is a row: the queue is `Message` rows no `Sent` has carried, a
call runs until a `Sent` answers it, a turn ends with a `Turn` row.
No snapshot on focus: the transcript is the fold, once.

### The client

The mirror is keyed `(host, agent, pos) -> MirrorEvent` with one
`(host, seq)` cursor, in the GUI's redb as today; the story rows are
replaced by mirror rows. Attention, heads, transcript and cost come
from the fold crate; `AgentHandledThrough` stays an agent position.
`Ready` no longer carries agents, so on a first connect Home fills as
`Created` and `Presented` entries arrive, complete when the copy is; on
a warm connect nothing changes.

## Slices, in landing order

A. **Config in the log, no record.** `Created` and the config events,
   the head table, the transitional attention table, the `agents` table
   gone, `desk_migration.rs` gone, the record→log migration. Daemon
   change; the GUI keeps `UiAgentSummary` for now, filled from the head,
   so no epoch bump; `projects` and `view_config` stay until B. Lands
   with a profile upgrade and a restart. Found on the way (b8os, 5 Sep):
   the record table's recorded redb type name is the old module path, so
   the migration reads it through `SenAs`, the same escape hatch store
   slice 1 needed; the unit tests could not see it because they write
   and read from one module, which is why every daemon migration runs
   on a copy of the user's store first.
   Landed 5 Sep (088e88e3, daemon only). On a copy of the user's real
   store: 2823 agents, 2645 with a spawn name, 105 Claude runtimes, 2
   pending Claude rewinds; after migration every agent replays as
   `Created` followed by its old events equal value by value, no
   position changed. The store held 0 agent `Name` facts, so
   `Created.spawn_name` carrying the record's display_name is what kept
   2645 agents named. `create_agent` now takes the role, so no log opens
   with a pointless `RoleChanged`; `append_agent_event` steps past an
   occupied position so a config event mid-turn is not overwritten.
   Migration files (`record_to_log_migration.rs`, `record_to_log_proof.rs`,
   the `AGENT_DB_MIGRATIONS` entry) come out once the user has restarted.
B. **The story log and its replication.** `StoryEvent`, the story
   table written live for both runtimes, `Ready` heads, `AgentLogs` /
   `AgentStory` / `AgentHead`, the GUI mirror, attention derived on the
   client, `AgentHandledThrough(StoryPos)`, the deletions listed under
   the wire, the one-time conversion of projects into `Project`
   labels and of view_config into the GUI's db, the transitional
   attention table deleted, the story migration. Daemon and GUI, epoch bump. The
   biggest slice; b8os may land the daemon half writing the story table
   first, behind no wire change, then the wire and GUI half.
   Daemon half landed (b8os, 5 Sep): the story is written live for both
   runtimes, `Titled`/`Activity` replace `agent_presentation_events` as
   the source with the head caching the latest, `turn_running` is the
   fold over `TurnStarted`/`TurnEnded`, `Cost` is told per model
   response, and a rewind tells `Rewound { to }` using a daemon-only
   `agent_story_source` index of which raw event each story event came
   from. The backfill runs in the background rather than holding a
   restart: `story_built` on the head, most-recently-touched agents
   first, one transaction each, resumable, and a load builds its own
   agent's story first. On a copy of the user's store it built 2823
   agents and 629k events in 17 s while the daemon answered an agent
   list in 25-62 ms. Known gap: a Claude compaction is not told, because
   the stream carries no event this can be mapped from; a Rho one is
   (`Compacted`).
   Wire and GUI half, change one (b8os, 5 Sep): epoch RUP9, ALPN
   `rho/ui/9`. `Ready` carries `UiAgentHead` only; `AgentLogs` /
   `AgentStory` / `AgentHead` and the connection-wide follow replace the
   per-agent subscription, with `AgentStreamFocus` left as the whole
   focus set, replaced wholesale rather than added to one agent at a
   time. The follow keeps a per-agent cursor per connection: an event in
   step goes as `AgentStory`, one past the cursor as `AgentHead` so the
   client re-asks with `AgentLogs`, and broadcast lag falls back to
   `Ready`. That makes lag and the background backfill self-healing with
   no new message. Attention is decided in one function, `agent_card` in
   `desk_view`, beside the Slack card, and pushed into the registry so
   every rail reads one answer.
   Three places the shape did not survive contact:
   - The spawner's id lived only in the transitional attention table, so
     the story learns it: `Parented { parent, at }`, written at creation,
     inserted after `Created` by the backfill, and told once for
     already-built stories. It folds into `AgentHead.parent`.
   - Publishing a story event needed every `append_agent_story` call
     site to carry a channel. Instead rho-db grew two small mechanisms:
     one type-erased observer slot per database, and an after-commit
     effect queue, so the append publishes itself once the transaction
     is durable and never while the write lock is held.
   - The projects conversion could not be the GUI's: `Ready.projects`
     is gone in the same epoch, so there is nothing client-side left to
     convert from. It is a daemon conversion like the dispositions one,
     writing one label per project with `Name` and `Project { host, path
     }`, the label id derived from the path so a second run recognises
     it. A project's description is dropped; nothing read it.
   Change two is the redb mirror in place of the in-memory fold, the
   transcript surface rendering from it with the live frame layered on,
   and then the daemon dropping `projects`, `view_config` and the
   transitional attention table.
   The mirror is `agent-mirror.redb` in the client state directory:
   the head with the name of the host it was heard from, one row per
   story event keyed agent then position, and the attention the card
   decided. It is written on the events that carry those things and
   read back before any daemon answers, so `AgentLogs` on `Ready` asks
   only for what came after. Two rules keep it honest: a row whose host
   is not attached this session is dropped, because host ids are handed
   out in attach order and mean nothing across a restart; and a story
   that is not contiguous from position zero is thrown away and asked
   for again rather than folded with a hole in it.
   A card also stopped needing a note. Filing is the user's labelling
   and placement, never a precondition: an agent whose story ends
   asking for the user is a card whether or not anyone filed it, ranked
   at the root with no breadcrumb, with its store node consulted only
   for the user's own verdict on it (muted, deferred) when there is
   one. That is what lets Home rank from the mirror alone.
   Opening such a card reads the agent from the card's own id
   rather than from a node, which is what was wrong the first time: a
   card with nothing filed behind it opened an empty page.
   The transcript reads the mirror too. Opening an agent shows the
   story folded into blocks straight away instead of waiting on a
   load, and shows it with the daemon down. Subscribing makes the
   daemon send a snapshot first, so the live transcript replaces the
   story-made one whole and nothing is merged. What the story does not
   carry it does not invent: a tool call is its name and its one line,
   never its output, and the status is never `Streaming`, because a
   turn that was running when the client last heard is the daemon's to
   report again. Bodies come on demand in slice D.
   `view_config` moved nowhere: nothing had read it since July, so the
   table is deleted rather than mirrored.
   Home is not offline yet, and this change does not claim it. The
   Desk's rows still come from the daemon by `DeskSync` every session,
   so a cold client has agents and no notes, no filing and no
   breadcrumbs. The client's own copy of the store is the `rho-sync`
   direction in `STORE-DESIGN.md` and gets its own slice after this
   one; offline Home is claimed when that lands.
C. **Usage from the mirror.** Graphs read `Cost` events; the usage
   requests and tables go. Daemon and GUI, epoch bump.
D. **On-demand detail.** `AgentDetail` for tool bodies and diffs from
   the raw log; the transcript surface reads the mirror and fetches
   bodies when a call is opened.
E. **The Desk mirror.** Found while proving change two on the rig
   (b8os, 5 Sep): the agent mirror alone gives a cold GUI heads, stories
   and attention, but the store's cells, frontier and bodies arrive by
   `DeskSync` every session and are not kept, so nothing offline has a
   note, a label or a breadcrumb to hang a card on. The client keeping
   its own copy of the store is the client half of store sync
   (`STORE-DESIGN.md`, rho-sync direction) and gets its own design; the
   frontier and body snapshots must come back exactly or the next sync
   is wrong. Offline Home is claimed only when this lands, not with B.
   Designed as `STORE-DESIGN.md` slice 5, "The client keeps the store".

F. **The pure mirror and the journal** (the 6 Sep revision). In three
   landings:
   1. Daemon, no wire change: `Turn`, `Presented`, `Wants`, `Rewound`
      and `at` on every raw event; usage on `Replied`; the journal table
      appended in the same transaction and built once for existing logs
      in creation order; lineages flattened into `Rewound`; `strip` and
      the fold crate, with a test that the fold over stripped raw events
      matches today's story transcript on the fixture. Proven on a copy
      of the user's store, counts reported, before the restart.
   2. Wire and GUI, epoch bump: `Follow` / `Log` / `Live` / `Detail`,
      `Ready` shrunk, the mirror rekeyed, attention and heads from the
      fold; the story table, `agent_story_source`, the backfill,
      `story_wire`, the head table, `AgentRemoteEncoder` and the
      registry's story fold deleted.
   3. Usage: the graphs read the fold for events that carry usage and
      the tables for the rest; the tables and requests go when the past
      no longer matters.
   Slices C and D fold into this: C is the fold's cost, D is `Detail`.

   Landings 1 and 2 landed together, 6 Sep, as one migration
   `b1e40c93 -> 50351c18`. Found on the way: the user's store was at
   the slice A layout (`b1e40c93`: raw events by lineage, a folded
   head per agent, the presentation record, the transitional attention
   table; slice B's story never ran on it), not the pre-A layout the
   first draft of the migration read; a proof on a `cp` of the store
   said so (0 agents found) before any restart, which is what the
   proof is for. The migration reads the heads, lays each agent's
   lineages out as one log with forks as `Rewound`, weaves in the rows
   only a story carried (turn edges, titles, activity labels, wants)
   after the raw row each followed when a store has one, and tells the
   head's title and activity again at the end when the rows had not.
   The heads, the presentation record, the attention table and any
   story tables are dropped; cost rows are not carried over, the usage
   tables still answer for the past (landing 3). The five variants the
   previous Rho loop wrote (`InferenceResponse`, `ToolResult`,
   `Queued`, `Dequeued`, `PresentationUpdated`) are rewritten as
   `Replied`, `Sent`, `Accepted` and `Presented` by the old replay's
   rules, so the runtime enum has no legacy variant; the old enum
   lives in `db/legacy_events.rs` and leaves with the migration. A title the sidecar
   gave an agent with a spawn name stays in the log now, where the old
   head dropped it; a reader prefers the spawn name on its own. On a
   `cp` of the store: 2824 agents, 1,109,932 rows, 110 forks, 146 heads
   told again, 21 s in one write transaction; every agent's visible
   history replayed value by value, unchanged. What the landing leaves
   open, in the order it will bite:
   - `Detail` is served but not asked for: the transcript folded from
     the mirror shows a tool call as its line and its status, never its
     output, and the GUI has no request wired to a tool being opened.
     That is the client half of D, still to do.
   - `Log` rides the main stream and `Live` the uni streams, so they are
     unordered: a reply that has just landed in the log may show twice
     for a moment, once in the fold and once in the live tail, until the
     next live frame drops it from the tail.
   - Every `Log` entry re-folds each open transcript from its whole
     mirror, O(session) per entry per open agent; an incremental fold
     is the fix when a long session streams.
   - `get_agent` folds the agent's whole log on every call; the daemon's
     hot paths (mail routing, parents, subscribers) want a config-only
     projection, not yet written.
   - A client that holds none of a host's agents at `Ready` picks its
     first subscriptions only when its cursor reaches the `journal_head`
     `Ready` named, so a cold client's rail fills before any transcript
     is asked for.
   - The GUI tests feed whole transcript states; the diff frames went
     with the encoder and no test describes a transcript as diffs.

   Landed 6 Sep, after it: the client and the daemon made dumber. The
   client holds every agent as a digest (identity plus what the rails
   read, folded incrementally) and at most four agents whole: their
   events, the transcript folded from them, and the live tail. The
   digests go to disk in the same transaction as the rows that made
   them (`gui_agent_digest_v1`), so a restart reads them back instead of
   folding every event again; the rows are read only when a transcript
   opens. The four are the focus set every host is told, replaced whole
   when one joins or leaves; leaving drops the events, the transcript,
   the live tail and the view unless a pane still shows it. Gone with
   that: the warm set seeded at `Ready`, the resubscribe of retained
   transcripts on reconnect (the focus set is simply sent again after
   `Follow`), the frame queue that coalesced live frames across draws,
   `AgentUnloaded` (the daemon sends one empty `Live` when an agent
   leaves the focus set, and nothing else about loading), and the six
   tests that described retention. The daemon answers whether an agent
   exists with one key lookup instead of folding every head, and the
   cost series walks agent ids instead of heads. Still open:
   - `Detail` unwired on the client, as above.
   - `Log` and `Live` unordered, as above.
   - The transcript of an active agent is re-folded whole on every
     `Log` entry; the digest is incremental, the transcript is not.
   - `get_agent` still folds one agent's whole log per call on the
     daemon's mail and tool paths; nothing loaded keeps its config in
     memory yet.
   - The mirror's per-agent rows are kept for every agent, not only
     the active four; nothing prunes them.

   Landed 6 Sep, after that: the live tail as deltas, step 1 of
   `LIVE-TAIL-PLAN.md` (wire `rho/ui/11`). `LiveFrame` and everything
   named `Ui*` left the wire; `Live` is one of `Requesting`, `Item`,
   `Appended`, `Retrying`, `Waiting`, `Idle`, and `Detail` answers a
   response with the same `Item`s. The loop says what changed at its
   one publish site through a teller that remembers what it last told;
   `AStr::diff` makes an append an `Appended`. Rows and deltas ride
   one broadcast feed in the daemon (the journal observer carries
   both), so a connection forwards them in the order they happened and
   the row always precedes the tail that follows it: `Log` and `Live`
   are ordered now. The per-agent iroh uni streams, their weights and
   decode budget, `AgentStreamOpened` and the stream generations are
   gone. Focus never loads: the pool unions every connection's focus
   into one live set, a loop tells only while it is in it, and a
   loaded agent entering it is told to say its tail whole; a follower
   asks the same after its catch-up and after a lag, and drops deltas
   until the first whole tell arrives. Every command that names an
   agent loads it. Loaded agents sit in an LRU of 100; past it the
   least recently used one that is idle, has nothing queued and nobody
   is looking at is dropped, which ends its loop (the created event
   and the turn watcher no longer hold handles). Each loop keeps its
   `AgentHead` in memory, updated when it changes its own profile, and
   the daemon's tool, mail, shell and terminal paths load the agent
   and read that instead of folding the log. The render types moved to
   `rho-registry::render`; the client's store keeps the tail from the
   deltas. Still open, in `LIVE-TAIL-PLAN.md`: the incremental
   transcript fold, derived attention, batched mirror writes, the
   digest fold version (step 2); presentation into the loop, and with
   it `AgentState`, the sidecar and the turn watcher (step 3);
   `agent_handle` still folds a log for its label.

   Landed 6 Sep, after that: step 2 of `LIVE-TAIL-PLAN.md`, the client.
   The transcript is folded incrementally: `TranscriptFold` takes one
   row at a time like the digest and gives the store its blocks; a
   `Log` entry for an active agent no longer refolds its events. The
   attention table is gone; attention is `rho_registry::attention`
   over the digest's facts (turn running, errored past, wants past)
   and one user verdict (`handled_through`, `muted`) kept in
   `gui_agent_verdict_v1` and written when it changes. The desk card
   and the registry make the same call. The mirror writer drains
   everything queued and commits once, so catch-up is one transaction
   per batch rather than one per row. Each stored digest carries the
   fold version; a mismatch at startup refolds that agent from its
   rows and writes the digest back. Claude rewinds reach the mirror as
   `Rewound` rows already, so no `TranscriptReplaced` row was needed.

   Landed 6 Sep, after that: step 3 of `LIVE-TAIL-PLAN.md`. What a
   loop publishes is an `AgentStatus` (its kind and how many inputs
   wait), not the whole `AgentState`; the Rho loop builds it from its
   own fields without cloning history, and the Claude loop owns its
   `AgentState` as a private field. `subscribe()`, the `Notify` and
   the daemon's turn watcher are gone: both loops already settle the
   turn with the pool, which flushes usage. Presentation is gated by
   the live set: the pool tells a loaded loop it is watched when it
   enters the set and unwatched when it leaves (an idempotent flag,
   no counted `Watch` handles), and the sidecar makes titles and
   activity only while watched; the turn report at every turn end is
   unchanged. The pool's activation observer is gone with the watcher.
   `agent_handle` reads the loaded loop's head and folds a log only
   for a cold agent. Left as it was: the sidecar itself, since the loop
   drives it and its `Presented` rows are already the loop's own.

   Landed 6 Sep, after that: two leftovers closed. Blocks are shared
   (`Vec<Arc<UiBlock>>`): the fold hands out pointers to what it keeps
   and copies a block out of its sharing only when a row changes it,
   so a `Log` row costs the blocks it adds and the store's summary
   walks pointers. A failed request is a row, `Failed { partial,
   error, retrying, at }`, written by the Rho loop on every temporary
   failure and by both loops on a final one, before the phase moves;
   its strip carries the text the model had said, the fold shows it,
   and a retry is a notice after it. `Live::Retrying` is gone with it
   (wire `rho/ui/12`); a retry tells `Requesting` after its row. The
   variant id is a hash of the name, so older logs decode unchanged.

   With it, a way back: `db::prepare` takes a redb persistent savepoint
   before a due migration and records its id; `rho debug rollback`
   restores it with the daemon stopped. The savepoint pins every page
   it covers, so it goes with the migration once the new build has run.

Each slice lands on its own with the tests of the slices before it
green; each daemon slice is proven on a read-only copy of the user's
store before the user restarts.

## Not in this document

The Rho runtime loop itself (`crates/rho-agent/src/agent/`, specs under
`crates/rho-agent/specs/`): it is the sole writer of a Rho agent's raw
log, in `Accepted`, `Sent` and `Replied` events, and tells the story
beside each one; how it decides when to send is its own business. Store sync
(`rho-sync`) and the capability pass (transports, telemetry,
visualizations, Iris, realtime, terminal and shell) stay as Directions
in `STORE-DESIGN.md`.

## Symptoms to watch for

- A field on the head that the logs could not rebuild.
- A story event carrying tool output or a raw model message.
- The daemon computing whether an agent wants the user.
- A per-agent subscription reappearing on the wire.
- A source that adds rows but only refreshes facts: a story event or a
  page's metadata makes a row exist, so the tree the dealer reads has to
  be made again, not only the sources under it.
- A surface borrowing a card it does not stand for: a why or a label
  read on a list, a log or a picker belongs to whatever the map's
  cursor last left behind, and a verdict pressed there takes it.
- A migration file still present after the user has restarted on it.
- A story event written by hand beside a raw event, or a mirror event
  that is not `strip` of exactly one raw event.
- A table on the daemon that the agent logs and the journal could
  rebuild.
- A per-agent position in a client's request, or a head on the wire.

## What done means

The daemon stores the raw log per agent and one journal, and nothing
else; the mirror is `strip` of the raw log and a client's knowledge of
a host is one seq; the GUI lists, ranks, and reads every agent from its
own mirror, offline, with the daemon only streaming `Log` from the
client's cursor and the focused agents' live frames; usage graphs,
heads and attention are the client's fold.

**Store size (6 Sep).** The 47 GB file held 3.6 GB of rows; the rest was
pages pinned by ten stale persistent savepoints. `rho debug
drop-stale-savepoints`, `forget-savepoints`, `compact` and `stats` handle it;
redb moved to 4.2.0 vendored with one allocator fix so compaction reaches
the empty regions (copy: 30.1 GB to 5.44 GB in 12 s). Details in
LIVE-TAIL-PLAN.md.
