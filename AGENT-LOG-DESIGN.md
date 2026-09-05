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
and the client hides its view past `to`. A projected segment is
`(AgentId, StoryPos)` and "since" means the same on both sides.

Tool output, diffs, reasoning, and the raw exchange stay on the host
and are fetched on demand when the user opens that call:
`AgentDetail { agent, story_pos }` answers with today's `UiTool` body
for that call, from the raw log, no runtime loaded.

### The wire is log replication plus one focus stream

- `Ready` carries every agent's head: `UiAgentHead { agent_id, story_pos,
  config, title, activity, usage_total, turn_running }`. That is the
  agents list; a title or a cost total never waits on a log.
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
table. On first start of the slice B build, for every agent:
build the story from the raw log (Rho runtime) or from the Claude
session file named by the runtime (Claude runtime), or write
`HistoryUnavailableBefore` when that file is gone, and drop
`agent_presentation_events`. Each migration runs once on the user's
real daemon after b8os has run it on a read-only copy of that store and
reported the counts; the migration code is deleted in the next landing
after the user has restarted on it (the standing rule; `desk_migration.rs`
from store slice 1 is deleted in slice A for the same reason).

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
B. **The story log and its replication.** `StoryEvent`, the story
   table written live for both runtimes, `Ready` heads, `AgentLogs` /
   `AgentStory` / `AgentHead`, the GUI mirror, attention derived on the
   client, `AgentHandledThrough(StoryPos)`, the deletions listed under
   the wire, the one-time GUI conversion of projects into `Project`
   labels and of view_config into the GUI's db, the transitional
   attention table deleted, the story migration. Daemon and GUI, epoch bump. The
   biggest slice; b8os may land the daemon half writing the story table
   first, behind no wire change, then the wire and GUI half.
C. **Usage from the mirror.** Graphs read `Cost` events; the usage
   requests and tables go. Daemon and GUI, epoch bump.
D. **On-demand detail.** `AgentDetail` for tool bodies and diffs from
   the raw log; the transcript surface reads the mirror and fetches
   bodies when a call is opened.

Each slice lands on its own with the tests of the slices before it
green; each daemon slice is proven on a read-only copy of the user's
store before the user restarts.

## Not in this document

`rho-agent2` is an isolated experimental harness with its own specs
under `crates/rho-agent2/specs/`; nothing here touches it. Store sync
(`rho-sync`) and the capability pass (transports, telemetry,
visualizations, Iris, realtime, terminal and shell) stay as Directions
in `STORE-DESIGN.md`.

## Symptoms to watch for

- A field on the head that the logs could not rebuild.
- A story event carrying tool output or a raw model message.
- The daemon computing whether an agent wants the user.
- A per-agent subscription reappearing on the wire.
- A migration file still present after the user has restarted on it.

## What done means

The daemon stores raw log, story log, and head per agent and nothing
else; the GUI lists, ranks, and reads every agent from its own mirror,
offline, with the daemon only streaming increments and the open agent's
live frame; usage graphs and attention are the client's.
