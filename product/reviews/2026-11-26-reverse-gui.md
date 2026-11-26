# Review: GUI & shared crates → `product/` (reverse)

- **Date:** 2026-11-26
- **Commit:** `561e6d43b` (main)
- **Device:** static
- **Harness / captures:** none (static `code → product/` walk of every crate under `crates/`, checked against `product/*.md` and the two earlier reviews in `product/reviews/`). Every claim below cites code read at this commit; nothing was built or run.

## Summary

The live spine matches `product/` well: one `History`, one Palette, one dealer, chat threads with `Flash`, Desk as a glance, and Pages, charts and notes are no longer cards. The dead weight sits in three places. First, retired flows that a user can still reach. Second, whole crates and protocols that only the agent host, the CLI or nobody uses. Third, legacy branches in the dealer and the journal. The single worst item is the **`start agent` / `send and start agent with options` menu, which every thread still offers in the Palette**. `product/` rejected that path twice: it bypasses the project's place, mode and runtime, and `agents.md` says no command has that name. Structurally, the code is still shaped around four retired models: a priority-curve dealer, a frame that owns chat's idea composer, a "forest of mail" in which a project is inferred from message shape, and agent-centric addresses where `product/` has worksets.

Tags: **zombie** = leftover from a retired design (`Kind: code`, delete). **undocumented** = live behaviour that `product/` does not describe (`Kind: product`, document it or cut it). **structure** = internal model that fights `product/`.

## Findings (ranked by `runs × cost`)

### 1. Zombie `start agent` menus are in every thread's Palette and bypass the project
- **Kind:** code
- **Level:** high-level (reverse)
- **Status:** open
- **Where:** `window.Palette` on any `chat.Thread`, and `desk.Desk` with a thread row selected; `chat.Project.receive an idea`
- **Evidence:**
  - Code: `AgentsSystem::rows` appends `start::rows(...)` on every thread point (`crates/rho-agents-view/src/system/mod.rs:1266-1269`). The menu is titled `start agent`, or `send and start agent with options` when the draft has text (`crates/rho-agents-view/src/system/start.rs:46-49`). Its rows are `new workset on a repository`, `here, in a directory on the host` and `join <subject>`, and each one then asks for a runtime (`start.rs:57-150`). The Palette flattens them into commands such as `start agent › here, in a directory on the host`. Behind them sit `rho-agents-client::create` (509 lines, still documented as a "start field" that Shift-Tab cycles, `crates/rho-agents-client/src/create.rs:21-31`) and the `StartFromMail`/`MailLaunch` RPC (`crates/rho-agents-client/src/protocol/worksets.rs:63, 82-95`).
  - Product: `chat.md:50` rejected *"the model chosen only through `start agent` · that path asks the place again and never makes a project thread"*. `chat.md:192` rejected *"two send keys, one that starts an agent and one that only sends"*. `agents.md:197` rejected *"telling the user to run `start agent here` · no command has that name"*, but the command does exist.
- **Cost:** It sits in the Palette that `Go to something by name` (20/day) and `Run a command I rarely use` (2/day) scan, and it ranks above window-wide commands because it is `Scope::Here`. Choosing it starts an agent outside the project's mode and runtime. That breaks `chat.an idea starts the work in its thread` and the value **Rare power, neatly tucked away**.
- **Direction:** Delete `system/start.rs`, `rho-agents-client/src/create.rs`, the `StartFromMail`/`MailLaunch` request and its host handler (`agent-host/rho-agent-host/src/agents.rs:545`). Keep `known_project_places` and move it next to `chat.Start a project`, which uses it (`mod.rs:201`).

### 2. The dealer still models a priority curve where `product/` has a FIFO queue and two time rules (structure + undocumented rules)
- **Kind:** product + code
- **Level:** high-level (reverse)
- **Status:** fixed
- **Where:** `deal.Card` order, `window.Status line` lamp and chime, `deal.Verdict.snooze`
- **Evidence:**
  - Order: the card order is `DealOrder::{Overdue, Waiting(since), Other}` (`crates/rho-dealer/src/rank.rs:266-284`). The `f64` priority now only breaks ties among `Overdue` and `Other`, and nothing current produces either (see Finding 8). Yet every claim still carries a `Curve::Waiting { head_start, since }` with a slope of 12/day (`crates/rho-dealer/src/curve.rs:14-16`), a recency `bonus` (`crates/rho-chat/src/cards.rs:187-193`), and a `Curve::Fading` that nothing produces (`curve.rs:56-61`).
  - Lamp and chime: these are the only remaining readers of the floats (`crates/rho-frame/src/deal.rs:439-461`). With `head_start = 1.0` and `LAMP_THRESHOLD = 0.5`, every waiting card lights the lamp at once. With `CHIME_THRESHOLD = 1.2`, a card chimes after **24 minutes** unseen, or **at once** if the user wrote in the thread in the last ~6 minutes (`AGENT_RECENCY_BONUS = 1.5`, `curve.rs:27-42`). `product/` says only *"something important waits"* and *"crosses a higher line"* (`window.md:42, 46`; `design.md:59, 197`), so the real rules live as tuned floats.
  - Snooze: `REPLY_BREAKTHROUGH` lets a project reply that comes within 1 h of the user's own message break through a snooze (`curve.rs:22`, `cards.rs:190-192`, `rank.rs:359-366`, case Z4 in `crates/rho-dealer/cases.md`). `deal.md:49` says snooze *"hides the card until then"*, so this is an **undocumented** exception.
  - The policy floats are also journalled every session as `DealerPolicySnapshot`, including a fixed `fyi_reply_pace_days: 0.0 // Results no longer fade` (`deal.rs:503-516`).
- **Cost:** Lamp and chime run all day, and `Clear what waits` runs 50/day. The user cannot predict the chime from `product/`. A change to the rules means a change to floats whose meaning is three steps removed. This breaks the `index.md:40` value that *"The defaults are decided once, in the product"*.
- **Direction (product):** Write the two rules into `window.Status line` (for example `lamp: any waiting card elsewhere`, `chime: once when a card has waited 24 min, or at once when the project answers within minutes of the user's message`). Then decide whether the Z4 snooze breakthrough stays. If it stays, write it under `deal.Verdict.snooze`.
- **Direction (code):** Replace `Curve`/`priority`/`bonus` with `Card { waiting_since, user_spoke_at }` and two named time rules. Delete `Fading`, `Deadline` (Finding 8) and `DEAL_QUEUE_FLOOR`. Options weighed: (a) keep the floats and only document them, rejected because the docs would describe constants and not intent; (b) time rules in `rho-dealer`, recommended; (c) time rules in `rho-frame::deal`, rejected because the dealer owns "what wants the user".

### 3. `rho-frame` is a god object that also owns chat's idea composer, and the `System` trait carries chat-only hooks (structure)
- **Kind:** code
- **Level:** high-level (reverse)
- **Status:** open
- **Where:** `chat.Write an idea`, `window.*`, `desk.*`, `deal.*`, `hosts.*`
- **Evidence:**
  - `Frame` has 25 fields (`crates/rho-frame/src/frame.rs:100-134`). They cover host connections and git questions, the ledger and pairing (`attention`), the dealer signals, history and the surface cache, Desk, the idea composer, the minibuffer, menu and echo, search, phone gestures, place persistence and cold return. `frame.rs` is 2,862 lines with 129 functions, and `rho-frame` is 13,090 lines. It implements five `product/` modules: `window`, `desk`, `deal`, `hosts` (approvals, pairing) and `chat.Write an idea`.
  - The `System` trait (`crates/rho-frame/src/system.rs:83-211`) has 9 hooks, and each has exactly one implementor. `idea_projects`, `idea_project`, `last_idea_project`, `idea_project_hosts`, `create_idea_project`, `send_idea`, `idea_pending`, `desk_context` and `shown_in` are implemented only by `ChatSystem` (`crates/rho-chat/src/ui/mod.rs:977-1075`). `agent_name` is implemented only by `AgentsSystem` (`crates/rho-agents-view/src/system/mod.rs:1291`). The frame drives a chat flow through generic plumbing (`crates/rho-frame/src/idea.rs`, 968 lines, with types named `IdeaMail` and `NewIdeaProject`).
  - **Duplicated home for one fact:** the idea draft persists in the frame (`gui_frame_idea_draft_v2`, `crates/rho-frame/src/place.rs:24-26`). Thread drafts persist in chat (`chat3_draft`, `crates/rho-chat/src/cache.rs:36`). `design.md` says both are the same `Draft` part.
  - `system.rs:7` claims *"a system never names another"*. In fact `rho-agents-view` depends on `rho-chat`, and `rho-gui` wires chat↔notes↔agents through `Rc` callbacks (`crates/rho-gui/src/main.rs:223-254`). One of those comments is stale: *"Chat ranks a thread by the agents still in it"*, but claims never read participants.
- **Cost:** This is the root of the open idea bugs in `2026-11-26-spine-core-loop.md` #5 and #6. The Idea overlay is not a `Surface`, so it leaks `q`, `space` and `ctrl-j` to the hidden screen, and its draft and pending state sit in two crates. It runs 12/day, on day #2 *Turn ideas into work*.
- **Direction:** Make the idea a `chat` surface: a `Draft` with `to:`/`on:` lines that chat opens and persists next to thread drafts. Then delete the seven `idea_*` hooks and `IdeaBox`. Move `draft.rs`, an editor primitive (`window.Edge-stuck excerpt`), to `rho-window`. Consider splitting `deal` and `attention` out of `Frame` into an entity of their own, which `Frame` reads. Options weighed: (a) a chat `Surface` for the idea, recommended; (b) keep the idea in the frame with a narrower `IdeaHost` trait, rejected because it keeps two draft homes; (c) a generic "composer" in `rho-window`, rejected because no second user exists.

### 4. Chat is still a "forest of mail": a project is inferred from message shape, and the wire can say states that `product/` rejected (structure)
- **Kind:** code
- **Level:** high-level (reverse)
- **Status:** open
- **Where:** `chat.Project`, `chat.Thread`, `chat.Message.ping`, `chat.Flash`
- **Evidence:**
  - **The project has no type of its own.** `HostChat::is_project` holds when *"a root message with no mentions"*, and *"a root that was itself addressed to the user is mail, not a project"* (`crates/rho-chat/src/fold.rs:258-263`). "Thread" has three definitions: `thread_of`, `subthread_of` and `root_of` (`fold.rs:299-344`, with comments *"a root is a stream"*). `ThreadId` names projects, threads and messages alike, so every caller re-checks `is_project` (`ui/mod.rs:820, 981, 1125, 1195`).
  - **Two fields must agree.** Ping is `MessageKind::Kept` plus `Mention::User { ping: true }` (`crates/rho-agent-types/src/chat.rs:78-105, 372-374`). A flash is `Flash` plus `User { ping: false }`. `Kept` plus `User { ping: false }` is still representable, settable (`set_ping`, `chat.rs:377`) and built in fixtures (`agent-host/rho-cli/src/agent.rs:2655-2663`). `chat.md:78` rejected exactly that: *"a message that is kept but does not ping"*.
  - **Addressees survive.** `Mention::Direct(agent)` is *"mail to this agent alone, without delivery to thread members"* (`chat.rs:86`). Combined with `reply_to` (*"a historical addressee"*) and `legacy_read_through` (`crates/rho-chat/src/cards.rs:218-232`), it contradicts `chat.md:73, 80`: *"there is no addressee"*, and *"a chat message has no target"*.
  - **Naming.** The `mail` vocabulary has about 550 occurrences in `rho-chat` and about 200 in `rho-agents-view`. Examples are `MailEvent`, `MailStream`, `MailAudience::Direct` (*"wrote direct mail"* in the transcript), `IdeaMail`, `StartFromMail` and the `mail6_log`/`mail6_cursor` tables. `rho-chat/src/lib.rs:1` opens with *"Mail: a forest of messages per host."*
- **Cost:** This touches every message, and `Answer a message` runs 100/day. Each new rule must handle shapes that `product/` removed. The review bug class "settles the wrong card" (`spine-core-loop` #1) came from exactly this sort of dual `reply_to`/`thread_reply` bookkeeping.
- **Direction:** In `rho-agent-types`, give `Project` its own identity (`ProjectId` already exists for config). Replace `kind` plus `Mention::User { ping }` with one enum such as `Voice::{Message, Flash, ToAgents}`. Retire `Mention::Direct` and `reply_to` from the user-facing path. **Compatibility cost:** the chat log is host-owned and synced, so this is a host and GUI wire change. Old records need a one-hop fold on the host (`temp-migration`), and a GUI older than the change cannot read a new host.

### 5. Agent-centric addressing and transcripts where `product/` has worksets, and the Palette lists every agent (structure)
- **Kind:** code (product already admits the status at `agents.md:30`)
- **Level:** high-level (reverse)
- **Where:** `agents.Workset`, `agents.Terminal`/`Shell`/`Desktop`, `window.Palette`, `index.md` non-goal *An agent dashboard*
- **Evidence:**
  - Every tool is addressed under one agent: `agent:<id>/term/<n>`, `agent:<id>/shell` and `agent:<id>/desktop/<name>` (`crates/rho-agents-view/src/system/address.rs:10-21`). This holds even though the comment says terminal `n` is *"of the agent's workset"*. `agent_view.rs:1` reads *"One model per agent"*.
  - `AgentsSystem::find` lists every known agent as a Palette place, with `host · working`/`idle` words (`system/mod.rs:1296-1314`). `window.md:55` lists `agents.Workset transcripts` and not agents, and `index.md:77` forbids a list of agents.
  - Tool commands appear only when the point is already on an `AgentAddress` (`system/mod.rs:842-874`, `1186-1190`). That breaks `window.md:69` (*"commands are never hidden merely because of which screen is open"*).
- **Cost:** `Look into the work` (15/day) and `Work beside an agent` (3/day). A second agent in a workset gets its own transcript and its own `term 0`. One terminal of the workset therefore has one address per agent, so history can hold the same terminal twice.
- **Direction:** Introduce `workset:<id>` addresses that own the transcript (filtered by agent) and the tools. `agent:<id>` then resolves to its workset with a filter. Make `find` list worksets. **Compatibility cost:** saved history (`gui_frame_place_v1`) holds `agent:` addresses, so map them once on load.

### 6. Zombie key schemes for raw places are still bound (cross-reference)
- **Kind:** code
- **Status:** open (the decision is logged as `spine-places-and-phone` #1, "decided"; this finding lists the exact code to delete)
- **Evidence:** `ctrl-shift-f9/f10/f11` on `RhoBrowser` (`crates/rho-frame/src/keys.rs:114-119`), plus the page-only Palette rows `go back`, `go forward` and `close page` bound to them (`crates/rho-browser/src/ui/mod.rs:470-481`). `space → VerdictMenu` on live `RhoDesktop` (`crates/rho-desktop-client/src/ui/mod.rs:148`). `ctrl-shift-n` and `ctrl-\ ctrl-n` for terminal Normal mode (`crates/rho-terminal/src/ui.rs:286-287`). Nothing binds `f9`. All of these were rejected in `window.md:31` and `browser.md:19`.
- **Cost:** About 10 visits/day to Pages and 3/day to Terminal or Desktop. A space typed into a remote app opens rho's verdict menu.
- **Direction:** Delete these bindings and rows when the dual-escape model lands. Do not keep them as aliases.

### 7. Dead crates, modules, tables and RPCs that the GUI no longer reaches
- **Kind:** code
- **Status:** open
- **Evidence (each verified to have no GUI caller):**
  - **`rho-files`** (1,018 lines): the GUI never enables its `client` feature, which only its own dev-deps turn on. The host refuses the protocol: *"the workspace file protocol is no longer served"* (`agent-host/rho-agent-host/src/lib.rs:1343`). The only user is `rho-cli`'s protocol describer (`agent-host/rho-cli/src/lib.rs:100`). The diff view and buffers left the GUI with the Zed workspace.
  - **`rho-rtc` client half** (`session.rs` and `session/native.rs`, 547 lines of WebRTC and audio): no crate enables `client`. The GUI dropped voice in `c71816dce` (2026-11-15). The host still serves `Protocol::Voice` (`rho-agent-host/src/realtime.rs`), so voice has no client at all. `product/` never mentions voice, so it is **undocumented** if anyone means to keep it.
  - **`rho_window::image_view`** (52 lines): no user since Slack and the Zed workspace left (`crates/rho-window/src/lib.rs:15`).
  - **`rho_hosts_last_placed_v1`** with `load_last_placed`/`save_last_placed` (`crates/rho-agent-hosts/src/saved.rs:17, 169-185`): no caller. `chat_last_project_v1` replaced it.
  - **Chat RPCs `Reparent` and `Leave`** (`crates/rho-chat/src/protocol.rs:44-48`): no client caller in the GUI or the CLI. Only the host handles them, and agents have their own IPC equivalents. Moving messages and removing agents from a thread are not in `product/`. **`ResolveProject`** is used only by `rho-cli`.
  - **`rho_rpc::Protocol::{Agents, Ledger, Workspace}`** (`crates/rho-rpc/src/protocol/mod.rs:169, 178, 186`): the host bails on all three (`rho-agent-host/src/lib.rs:1324-1343`). Removal depends on how senax tags enum variants (by name or by index). Check that before you delete them.
  - **Stale crate docs:** `rho-frame/src/lib.rs:15-19` lists the systems as *"chat, agents, notes, files"* and says a system *"lists what has moved for the desk"* (rejected at `desk.md`, 2026-11-21). `rho-frame/src/thread.rs:2` says *"whether a channel or a thread"*. `rho-agents-view/src/system/mod.rs:1193` mentions *"The `with` line"*.
- **Cost:** No per-day user cost. Each item is build time, review surface and a false map for the next agent that reads the code.
- **Direction:** Delete them. Fix the stale docs in the same change.

### 8. Legacy dealer paths are still live, and one can deal a card that cannot be settled
- **Kind:** code
- **Status:** open
- **Evidence:**
  - **Deadlines:** no build writes `Fact::Deadline` (the only writer is a test, `crates/rho-chat/src/cards.rs:1174`). Yet `rank_into` turns any deadline in the ledger into a card on **any** address, notes included (`crates/rho-dealer/src/rank.rs:306-318`). A passed deadline gets `DealOrder::Overdue` with priority 1e6, so it deals first and keeps the lamp on (`curve.rs:79-86`). `NoteView` and `NotesView` have `CardHere::None`, so on a note that card cannot take a verdict. This contradicts `notes.md` (*"A standalone note is never a dealable `deal.Card`"*). `rho-notes/src/lib.rs:17-19` still says *"a bare reminder is a note with a deadline"*. **Conditional:** this bites only if some device's ledger still holds an unsettled `Deadline` fact from an older build. I did not inspect a real ledger.
  - **Project-level hold (`Claim::under`):** a snooze on the project root holds its threads (`cards.rs:176-186`, `rank.rs:355-360`). No surface can target a project root any more, because a project has no screen (`chat.md:37`). The comment *"A snooze or mute on the project…"* is also wrong, since mute ignores `under`.
  - **`Fact::Named` and `NodeMarks.name`** (`crates/rho-dealer/src/marks.rs:24`): written by no build and read by no code. `name this thread` goes to the host `Name` RPC instead.
  - **Legacy `Seen` variants** (`Agent`, `Topic`, `Thread{stopped}`) and the `cases.md` sections P1, T1, T3, T6, N2 and Z7 describe piles, todos, deadlines and channels.
- **Cost:** This adds risk to the lamp and chime and to `Clear what waits` (50/day) for any user with old facts.
- **Direction:** Keep the `Fact` variants so that old entries still decode. Drop the read paths for `Deadline`, `Named`, `under` and the legacy `Seen` variants, and treat them as no-ops in `Marks::apply`. Prune `cases.md` to match.

### 9. Undocumented live features: document them or cut them
- **Kind:** product
- **Status:** open
- **Evidence and options:**
  - **`name this thread`** (alias `rename`), a Palette command with the host `Name` RPC (`crates/rho-chat/src/ui/mod.rs:891-933`). `chat.Thread` has a subject but no way to change it. Options: add `rename` under `chat.Thread.does`, or cut it.
  - **`f21`/`f20` → back/forward and `f16` → close and deal** (`crates/rho-frame/src/keys.rs:103-105`). These are probably hardware or remapped keys. Options: name them in `window.Screen` as alternate keys, or cut them.
  - **`quit`**, a window-wide Palette command (`frame.rs:1240`). Add one line to `window.Palette`, or leave it out as an OS concern.
  - **The snooze breakthrough and the lamp/chime timings** are covered in Finding 2.
  - **Voice** is covered in Finding 7.
- **Cost:** This is small per use. The issue is that `product/` stops being the full list of what the user can do.

### 10. The action journal is an undocumented instrument, half of it is dead, and one comment states adaptive intent
- **Kind:** code (with a product note)
- **Status:** open
- **Evidence:** Of 35 `rho_journal::Event` variants, 18 are never recorded by any crate: `OverviewOpened`, `AgentOpened`, `AgentSelected`, `DeskRawModeToggled`, `Find`, `Created`, `CaptureCarryover`, `WrongCard`, `Key`, `AgentMessageSent`, `Told`, `AgentChanged`, `Click`, `Scroll`, `DealMode`, `SurfaceClosed`, `HistoryAppended` and `WindowFocusChanged` (`crates/rho-journal/src/lib.rs:438-621`). `Event::Find` is documented as *"a miss signal for future dealing"* (`lib.rs:505`). That conflicts with `index.md:40`: *"Nothing adapts on its own."* The one-off secret scrub (`SCRUBBED`, `lib.rs:793-830`, from 2026-09-25) is past its window.
- **Direction:** Keep the variants for decode order, but mark them retired, remove their writers' types, and delete the scrub. Rewrite the `Find` comment so that it states the journal is inert. `product/` needs no entry, because this is a developer instrument that is never uploaded.

### 11. Temporary migrations are past their window
- **Kind:** code
- **Status:** open
- **Evidence:** These are kept "until every device has opened this build". Per the `temp-migration` skill, they should usually last 1–2 commits:
  - `retire_slack.rs` (256 lines, 2026-11-20)
  - `gui_channel_place_v1` with the `ImportProject` RPC (`crates/rho-chat/src/ui/projects.rs:569-600`, 2026-11-20)
  - `gui_frame_idea_draft_v1` (2026-11-20)
  - `gui_agent_rows_v2` and `gui_chat_message_v1`/`gui_chat_outbox_v1`/`gui_topic_draft_v1` (`crates/rho-agents-client/src/cache.rs`, 2026-10-31 to 2026-11-15)
  - `ledger_logs_v2` (`crates/rho-ledger/src/store.rs:13-28`, 2026-09-25)
  - The `gui_browser_bookmark_v1` import is from today (`4fda8a9f0`), so it should stay for now.
- **Direction:** Ask the owner to confirm that the laptop and the phone have each run a current build. Then delete them in one commit.

### 12. Two incremental-transcript engines
- **Kind:** code
- **Status:** open
- **Evidence:** `rho-transcript` (keyed items, bucketed highlights) drives Desk, Thread and the Notes list (`crates/rho-frame/src/desk.rs:21`, `crates/rho-chat/src/ui/thread.rs:31`, `crates/rho-notes/src/list.rs:20`). `rho-agents-view/src/transcript` (2,507 lines) reimplements bucketed, anchor-preserving projection for the agent transcript. `AGENTS.md` says new UI is built from shared editor primitives.
- **Direction:** Do not merge them now. Record the choice for whoever builds the workset transcript in Finding 5, and build it on `rho-transcript`.

## Orphan inventory (Goal 1)

| Item | Tag | Kind | Finding |
|---|---|---|---|
| `start agent` / `send and start agent with options` menu, `create.rs`, `StartFromMail` | zombie | code | 1 |
| `Curve::Fading`, `Curve::Deadline`, `DealOrder::Overdue`, `DEADLINE_LEAD_DAYS` | zombie | code | 2, 8 |
| Recency `bonus` and float thresholds as the lamp/chime rule | undocumented | product | 2 |
| Snooze breakthrough (Z4) | undocumented | product | 2 |
| `System::idea_*`, `desk_context`, `shown_in`, `agent_name` (single implementor) | structure | code | 3 |
| `IdeaBox` and idea draft in `rho-frame` | structure | code | 3 |
| `Mention::User{ping}` with `MessageKind`; `Mention::Direct`; `reply_to`; `legacy_read_through` | zombie/structure | code | 4 |
| `agent:<id>/…` tool addresses; Palette lists agents | structure | code | 5 |
| `ctrl-shift-f9/f10/f11`, `go back`/`go forward`/`close page` rows, desktop `space`, terminal `ctrl-shift-n` | zombie | code | 6 |
| `rho-files` crate | zombie | code | 7 |
| `rho-rtc` client session; host voice | zombie / undocumented | code / product | 7 |
| `rho_window::image_view` | zombie | code | 7 |
| `rho_hosts_last_placed_v1` table | zombie | code | 7 |
| Chat RPCs `Reparent`, `Leave` | zombie | code | 7 |
| `Protocol::{Agents, Ledger, Workspace}` | zombie | code | 7 |
| `Claim::under` project hold; `Fact::Named` read; `NodeMarks.name`; legacy `Seen` | zombie | code | 8 |
| `name this thread`, `f21/f20/f16`, `quit` | undocumented | product | 9 |
| 18 unwritten journal events; secret scrub | zombie | code | 10 |
| Action journal, `rho-gui journal dump`, `--cpu-profile` | undocumented (developer instrument) | none | 10 |
| `retire_slack`, `gui_channel_place_v1`, `gui_frame_idea_draft_v1`, `gui_agent_rows_v2`, `gui_chat_*_v1`, `ledger_logs_v2` | zombie (temporary migration) | code | 11 |
| Second transcript engine in `rho-agents-view` | structure | code | 12 |

I checked the following and found them consistent with `product/`, so the table leaves them out:
- `rho-gui` telemetry (`upload performance report` = `window.Report a slow window`)
- `rho-notes`
- bookmarks, which now go through the ledger
- `usage` charts with in-place switching
- the git-approval queue
- the `waiting` placeholder surface
- `rho-browser`/`rho-browser-wayland`/`native_host`, which is the infrastructure behind `browser.Page`
- `rho-desktop-*`, `rho-shell-view` and `rho-terminal`, apart from the key schemes in Finding 6
- `rho-ledger`, `rho-iroh-auth`, `rho-db`, `rho-rpc`, `prefix-id`, `json-stream`, `octo-types` and `senax-encoder`, which are shared plumbing
- `rho-profiling`

## Not verified

- Whether senax tags enum variants by name or by index. This decides whether the `Protocol::*` variants in Finding 7 and the dead `Event` variants in Finding 10 can be deleted or must stay.
- Whether any real ledger holds an unsettled `Fact::Deadline` (Finding 8).
- `agent-host/` was read only where `crates/` code pointed into it.
