# Review: Core Work Loop — Desk, Deal, Chat, Agents (high-level)

- **Date:** 2026-11-26
- **Commit:** `4a42786ad`
- **Device:** static (`product/` + codebase, covering laptop & phone flows)
- **Harness / captures:** static Pass 1 review (no live Wayland captures; existing reference renders noted in `product/design/`)

## Summary

The core spine has a strong, coherent bet — one editor window, a single attention dealer (`Desk` + `Verdict`), threads speaking in the project's voice with ephemeral `Flash` progress, and `Agent` transcripts tucked one level down. However, the recent structural flips (`Thread` to newest-first + top draft, `Message` dropping headers and explicit reply targets, `Project` dropping its screen in favor of narrowed `Desk`, and `Idea` moving to an overlay) left deep seams broken across the 100/day core loop. The single worst problem is that **replying to a waiting thread fails to settle its card unless the thread was freshly opened through the Deal**, leaving answered asks stuck on the status line and Desk across the heaviest flow in the product (`chat.Answer a message`, 100/day).

## Findings (ranked by `runs × cost`)

### 1. Replying to a waiting thread does not settle its card unless freshly opened via Deal
- **Level:** high-level
- **Action:** fix
- **Status:** fixed (2026-11-26, Loop 2: `Surface::card() -> CardHere`, live `Frame::card_seen`)
- **Where:** `chat.Answer a message` (step 3) / `deal.a reply settles the card`
- **Evidence:**
  - Product model: `deal.md:72-73` (`a reply settles the card`: *"when the user replies to a `chat.Message` that waits, then its `Card` goes away"*) and `chat.md:201-206` (`Answer a message`: *"the user opens the thread by the deal or by name... the reply settles that card on this device"*).
  - Code: When sending a reply, `ThreadView::send_only` (`crates/rho-chat/src/ui/thread.rs:1236-1241`) resolves the card to settle via `FrameLink::with_dealt` (`crates/rho-frame/src/link.rs:56-63`) → `Frame::dealt_seen` (`crates/rho-frame/src/frame.rs:430-438`). `dealt_seen` returns `Some` **only** when the history entry is `ScreenEntry::Dealt(seen)`.
  - It returns `None` whenever the user reached the waiting thread via: (a) `ctrl-k` / `tab` back into history, (b) opening a recently visited thread from blank Palette (`crates/rho-frame/src/minibuffer.rs:593-597` calls `frame.open`, which stores `ScreenEntry::Opened`), (c) opening a Palette text search hit (`crates/rho-frame/src/frame.rs:1027`), or (d) any warm restart (`crates/rho-frame/src/place.rs:145` restores all entries as `ScreenEntry::Opened`).
  - Even on a freshly dealt card, because `deal.Verdict.reply` keeps the thread on screen (`deal.md:45`), any follow-up ask that arrives while the user stays in the thread keeps the stale `ScreenEntry::Dealt(old_seen)`. Replying re-settles the old message and leaves the new ask waiting.
  - Because `crates/rho-chat/src/cards.rs:86-90, 220-232` only settles what `thread_reply.settles` names (and never falls back to `legacy_read_through` when `thread_reply` is `Some`), `settles: None` leaves the card unsettled (`cards.rs:645-650`). Meanwhile, `Verdict::Done` uses `Frame::seen()` (`crates/rho-frame/src/deal.rs:193-201`), which falls back to the active card in `self.hand()`.
- **Cost:** `100 a day` (`chat.Answer a message`) × the user reads an ask right in front of them, sends an answer, yet `waits · …` stays on the status line and the same answered ask deals back to them until they also press `space d`. Fails `index.md` values **rho remembers, so the user does not** and **The system does what it can**, and breaks the core mental model that replying settles a card.
- **Direction:** Have thread replies settle via `Frame::seen()` (matching `Verdict::Done`), and update/capture the visible card observation when a new ask renders in an already-focused thread (or settle the card currently shown on screen) rather than relying on an invisible `Dealt` vs `Opened` history-entry distinction.

### 2. Flipping `Thread` to newest-first + top-draft broke both primary one-thumb phone interactions
- **Kind:** product
- **Level:** high-level
- **Status:** on-hold (phone ergonomics deferred to backlog; noting `Thread` feels too cursor/keyboard-centric on phone)
- **Where:** `chat.Thread` / `chat.Answer a message` (phone) / `window.Phone bar`
- **Evidence:**
  - Product model: `index.md:15, 52, 68` (*"used with one thumb and a bar of buttons along the bottom edge... On the phone, every step is a thumb's work, so the question is asked twice"*); `chat.md:154` (*"On the phone, the draft is the tap target to write"*); `window.md:129` (*"flick up from the end of a screen: forward, and from the newest screen deal the next card"*).
  - Code:
    1. **Top-edge reply reach:** On a phone card, the bottom bar shows `back | menu | done | snooze | more` (`crates/rho-frame/src/phone.rs:369`) — there is no `reply` button. Because `Draft` moved to line 0 at the top of `Thread` (`crates/rho-chat/src/ui/thread.rs:326`), starting a reply requires stretching one thumb to the very top edge of the screen to hit a single ~21px line (`reply`), violating `design.space.thumb target` (`56 px`) and putting the most common action (`Answer a message`, 100/day) at the least reachable spot on the phone.
    2. **Flick-up-to-deal blocked by scroll edge:** `ScrollEdge::permits` (`crates/rho-frame/src/phone.rs:180-188`) allows `PhoneFlickDirection::Up` (which deals the next card) **only** from `ScrollEdge::Bottom | ScrollEdge::Both`. That rule worked when `Thread` was oldest-first and opened at the bottom. Now that `Thread` opens at the top (`ScrollEdge::Top`), flicking up on any thread longer than one screen scrolls down into old history instead of dealing the next card.
- **Cost:** `100 a day` (every phone reply or card-skip on multi-screen threads). Directly fails `index.md` values **The system does what it can** and **The phone is a full device**.
- **Direction:** Prototype 2–3 phone options:
  1. Add a `reply` button to the phone bar on `Thread` (e.g. replacing `more` or `menu` slot, or pairing `reply` + `done` + `snooze` + `more` with `back`) so one thumb at the bottom starts typing immediately.
  2. Rethink the phone deal/skip gesture for top-anchored threads (e.g., either a top-edge / horizontal flick, or a `next` / `skip` action reachable without scrolling through old history).
  3. Re-evaluate whether the phone draft tap target should at least be a full `56 px` band when empty, even if a bar button is also added.

### 3. Cached `Thread` screens ignore `show_card` and `opened`, reopening dealt cards deep in stale history
- **Level:** high-level
- **Action:** fix
- **Status:** fixed (2026-11-26, Loop 2 branch `loop2/c2-thread-desk`)
- **Where:** `chat.Thread` (`states: full`) / `deal.Clear what waits` (step 1)
- **Evidence:**
  - Product model: `chat.md:133` (*"whether opened by the deal or by name, the window starts at the top of the document: the `Draft`, with the point in it where it was left, then the newest message... Returning to a place restores it as left"*); `deal.md:22` (*"put a message that wants the user in reach, without making them look through the whole thread"*).
  - Code: `Frame::open_with` (`crates/rho-frame/src/frame.rs:505-536`) reuses any `ThreadView` already cached in `self.surfaces` (everything visited this session unless explicitly closed with `q`) and calls `surface.show_card(card)` on Deal or `surface.opened()` on explicit open (`:` / `Enter`).
  - `ThreadView` (`crates/rho-chat/src/ui/thread.rs:1955-2267`) implements **neither `show_card()` nor `opened()`**, inheriting empty no-ops from `crates/rho-frame/src/surface.rs:134-138`.
  - If the user scrolled down into old history, left via `tab`/`ctrl-k`/`:`/`ctrl-j`, and later a new ask arrives and is dealt, the thread opens still scrolled deep into old history with the new ask hidden above the viewport.
- **Cost:** `50–100 a day` (re-dealing or reopening active threads throughout an 8–10 hour day). Breaks `chat.Thread`'s core answer (*"what is this piece of work waiting for from the user?"*) and `deal.Card`'s purpose.
- **Direction:** Implement `ThreadView::show_card` and `ThreadView::opened` to scroll to the top of the document and place point in the `Draft` (at `draft_point` if present, else draft start), while leaving `ctrl-k`/`ctrl-j` history traversal untouched so returning restores the previous scroll position as specified.

### 4. `Thread` never shows when an agent is `working`, and cannot `stop agent` without leaving the thread
- **Level:** high-level
- **Action:** fix
- **Status:** fixed (2026-11-26, Loop 2 branch `loop2/c2-thread-desk`)
- **Where:** `chat.Thread` (`shows`, `mock, with a flash`) & `agents.Steer an agent` (step 2)
- **Evidence:**
  - Product model: `chat.md:173` shows `working` in the `Thread` status line (`clock@lab › Flaky clock test   working   NOR`); `index.md:40` (*"what each agent is doing"*); `agents.md:44, 176` (`Steer an agent` from `chat.Thread` palette).
  - Code:
    1. `ThreadView` (`crates/rho-chat/src/ui/thread.rs:1955-2267`) does **not** implement `Surface::status()`, even though `HostChat` already receives live `ThreadWork::Working` (`crates/rho-chat/src/fold.rs:58, 243`) and `ThreadView` already reads `system.work(thread)` at line 760 for `StoppedByYou`.
    2. Even if `ThreadView::status()` existed, `Frame::render_status_line` (`crates/rho-frame/src/frame.rs:1479-1504`) takes the `if let Some(address) = own_card` branch for every `ThreadView` (since `ThreadView::card()` returns `Subject::Own`), and uses `card.label` (which is `""` when nothing waits) instead of falling back to `surface.status(cx)`.
    3. `AgentsSystem::rows` (`crates/rho-agents-view/src/system/mod.rs:1236-1286`) exposes `steer_rows` (`rewind turns`, `compact context`, `new cache key`, `set runtime…`, `cancel the request`) on `Thread`, but `stop agent` (`mod.rs:838, 1080`) lives only in `agent_rows` (only shown after drilling into an `Agent` surface).
- **Cost:** `100 a day` (every reply where the user stays on the thread and has zero confirmation that an agent picked up the work until a flash or reply arrives) + extra navigation to stop a runaway agent. Violates `index.md` value **rho remembers, so the user does not** and **The system does what it can**.
- **Direction:** Implement `ThreadView::status()` returning `"working"` when `ThreadWork::Working` (and `"stopped by you"` when appropriate), let `render_status_line` fall back to `surface.status(cx)` when `card.label` is empty, and include `stop agent` in `steer_rows` (also resolving its `"shift-c"` key collision with `cancel the request` at `surface.rs:479`).

### 5. `Idea` (`Write an idea`) is a half-baked overlay that breaks Helix editing, leaks keys to the hidden screen, and makes `to:`/`on:` pickers unreachable on phone
- **Level:** high-level
- **Action:** fix
- **Status:** open
- **Where:** `chat.Write an idea` (steps 1–3) / `design.Draft` / `window.Phone bar`
- **Evidence:**
  - Product model: `index.md:13, 28` (*"An idea must become an agent at work in one short step, from either device"*); `index.md` Value 6 (*"An editor, not an app"*) and Value 9 (*"The phone is a full device"*); `chat.md:221-225`; `design.md:160` (*"writing an idea: cancel, undo, and send"*).
  - Code:
    1. **Mode indicator lies & hidden screen takes Normal-mode keys:** `overlay.editor` is a full modal Helix editor (`crates/rho-frame/src/idea.rs:259-273`), yet `Frame::mode()` (`crates/rho-frame/src/frame.rs:1570-1576`) hardcodes `"insert"` whenever `idea_editor` has focus — pressing `Escape` switches to Normal mode while the status line stays `INS`. Worse, in Normal mode inside `Idea`, `q` calls `close_current` (`frame.rs:1721`) and closes the **hidden screen underneath**, `space` (`frame.rs:1728-1734`) opens the **hidden card's verdict menu**, `ctrl-j` deals a card behind the overlay, and `shift-u` (`frame.rs:1753-1762`) tries to redo on `current_surface()` instead of `idea_editor`.
    2. **Three disjoint editors break editor motions:** `to:` and `on:` are non-modal single-line editors (`idea.rs:274-311`) with `set_use_modal_editing(false)` — `j`/`k` cannot move between `to:`, `on:`, and the body (only one-way `shift-tab` cycles focus).
    3. **Phone `Write an idea` is crippled:** `buttons(BarState { composing_idea: true, .. })` (`crates/rho-frame/src/phone.rs:352-355`) renders only `[Cancel, Send]` — **dropping `Undo`** (`design.md:160`). Worse, `render_idea` (`idea.rs:736-759`) attaches **no click/tap handler** to `to:` or `on:`, and `PromptIdea` / `PromptIdeaRuntime` are keyboard-only (`keys.rs:154-156`) — so on a phone there is no way to open the project or runtime completion prompt (or `new project …`) by thumb! Finally, on phone Desk, `write an idea` is buried two taps deep inside `more` (`phone.rs:771-782`).
    4. **Undocumented laptop global shortcut:** Outside Desk, the idea key is `ctrl-alt-m` (`keys.rs:104`), which is never named in `chat.md:221` or `window.md`.
- **Cost:** `12 a day` (Day #2 priority: **Turn ideas into work**). Fails `index.md` values **An editor, not an app**, **The phone is a full device**, and **Moving between places costs nothing**.
- **Direction:** Make `Idea` a real `Surface` (or properly isolate every frame action `q`/`space`/`ctrl-j`/`shift-u` and read `idea_editor`'s actual Vim mode), add `Undo` to the phone bar for Idea, make tapping the `to:` and `on:` rows open their respective completion prompts on phone (and click on laptop), document the global idea chord in `chat.md`/`window.md`, and decide whether phone Desk's bottom bar should expose `idea` directly instead of hiding it under `more`.

### 6. One queued idea locks out writing any second idea while offline or waiting on a host
- **Level:** high-level
- **Action:** fix
- **Status:** open
- **Where:** `chat.Write an idea` / `chat.Message` (offline queueing, `chat.md:83`)
- **Evidence:**
  - Product model: `index.md:13` (*"Ideas come to them anywhere, at the laptop or on the go. An idea must become an agent at work in one short step, from either device"*); `chat.md:83` (*"A message written while the host is away waits on the device and goes out when the host is back. A submitted idea says `queued for` its project and can be found by name in `window.Palette` until the host's thread takes its place"*).
  - Code: `Frame::send_idea` (`crates/rho-frame/src/idea.rs:659-678`) queues the idea in `Outbox` under `client_id` and closes the overlay, **without clearing `self.idea`**, waiting for `Frame::finish_idea` (`idea.rs:683-715`) to clear it only after the host logs it. Meanwhile, `Frame::prompt_idea` (`idea.rs:233-240`) blocks opening the Idea composer whenever `self.idea.client_id` is still pending in the outbox, echoing `"idea: waiting for the host"`.
  - Additionally, when a host refuses an idea's runtime (`PostIdeaReply::RuntimeRefused`), `finish_idea` (`idea.rs:709-713`) only flashes a 4-second echo (`"idea: ... · draft kept"`) without reopening the draft or surfacing it as a waiting/refused item.
- **Cost:** `12 a day` (especially on the go / phone when disconnected or when a host takes a few seconds to create a workset). Completely blocks capturing a second idea on the train/plane or back-to-back while a host is unreachable — violating `index.md` value **rho remembers, so the user does not** and Day #2 (**Turn ideas into work**).
- **Direction:** Once `send_idea` durably persists the idea in `Outbox` (where it is already findable as a `PendingIdea` in Palette), clear `IdeaBox` immediately so the user can capture the next idea right away. On `RuntimeRefused`, retain the refused `PendingIdea` (just like `OutgoingState::Refused` for thread replies) or reopen it into `IdeaBox` when the user opens it from the refusal/Palette.

### 7. `Agent` transcript breaks `space u` (undo) and `space n` (next card) right after settling a card or opening by name
- **Level:** high-level
- **Action:** fix
- **Status:** fixed (2026-11-26, Loop 2)
- **Where:** `agents.Agent` (`keys: space then u`, `space`) / `deal.Verdict.undo` / `deal.Card.deal`
- **Evidence:**
  - Product model: `agents.md:113-114` (*"`space` then `u`: undo the last verdict without leaving the transcript. `space`: `deal.Verdict` on the card the user came from; the transcript stays open. Opening the agent by name does not choose a card"*); `deal.md:50-51`.
  - Code: `Frame::verdict_menu` (`crates/rho-frame/src/frame.rs:1193-1203`) short-circuits with `"nothing waits here"` whenever `inspects_card()` is true and the target is either `None` or no longer in `self.hand().cards`.
  - Because `Frame::verdict` (`crates/rho-frame/src/deal.rs:255-261`) intentionally leaves the transcript open after `space d` (`done`), the card immediately leaves `self.hand().cards`. If the user then presses `space u` to undo an accidental `done`, or `space n` to deal the next card, `verdict_menu` refuses to open! The same dead-end happens on any `Agent` opened by name, even though every other non-card surface opens `"no card here"` with `u undo` and `n next card` (`frame.rs:1213-1215`).
  - Minor related clutter: `AgentsSystem::agent_rows` (`crates/rho-agents-view/src/system/mod.rs:835`) pushes a duplicate `"verdicts"` Palette row even though `Frame::commands` (`frame.rs:1092-1110`) already adds verdict commands.
- **Cost:** `15 a day` (`agents.Look into the work`). Breaks undo at the exact screen where a user just judged work, and breaks `space n` deal-continuation on every inspected transcript. Fails `index.md` value **Moving between places costs nothing** and `agents.Agent`.
- **Direction:** Remove the early return in `Frame::verdict_menu` so `verdict_rows` falls through to its standard `"no card here"` menu (`u undo`, `n next card`) whenever an inspection surface has no waiting card target, and remove the duplicate `"verdicts"` Palette row in `agent_rows`.

### 8. `Agent` transcript traps the `brief` in a fixed `line_clamp(4)` header outside the editor
- **Level:** high-level
- **Action:** fix
- **Status:** fixed (2026-11-26, Loop 2 branch `loop2/c2-thread-desk`)
- **Where:** `agents.Agent` (`shows: the brief once, then the transcript in order`)
- **Evidence:**
  - Product model: `agents.md:110` (*"the brief once, then the transcript in order... nothing the agent was given is shortened, since this screen is for finding out what happened"*); `index.md` Value 6 (*"Every screen is text the user moves through with the same keys"*) and `non-goal Fixed controls`.
  - Code: `AgentSurface::render` (`crates/rho-agents-view/src/system/surface.rs:597-609`) mounts the brief in a fixed GPUI `div()` **above** the transcript `Editor`, clamped with `.line_clamp(4)`.
  - Consequences: (1) Any brief longer than 4 lines is silently truncated with no way to expand or read it on the Agent screen; (2) the brief cannot be searched (`/`), selected, or yanked (`y`); (3) it permanently consumes ~5 lines of vertical viewport (~25% of a phone screen) while reading deep in the transcript.
- **Cost:** `15 a day` (`agents.Look into the work`), highest on phone and on multi-paragraph ideas. Directly fails `index.md` values **An editor, not an app**, **One place, in focus**, and `agents.Agent`.
- **Direction:** Render the full, untruncated `brief` as the top block inside the transcript `Editor` buffer (followed by its rule), so it scrolls naturally out of the way, is searchable/copyable with Helix motions, and never clips what the agent was given.

### 9. Phone tap-to-inspect (`Look into the work`) is an invisible 21px first-line hit target after message headers were removed
- **Kind:** product
- **Level:** high-level
- **Status:** on-hold (phone deferred to backlog; stale message-header refs removed from `agents.md`)
- **Where:** `agents.Look into the work` (step 2) / `chat.Thread` (phone)
- **Evidence:**
  - Product model: `agents.md:39, 157` still says *"tap on a message header"* to open the agent transcript, even though `chat.Message` removed message headers on 2026-11-20 (`chat.md:80`: *"colour tells the voice"*).
  - Code: `ThreadView::tap_mail` (`crates/rho-chat/src/ui/thread.rs:1663-1669`) inspects the agent **only** when `self.item_row(&Key::Message(mail.id), cx) == Some(cursor.row)` — i.e., tapping the exact **first body line** of a project message (~21px tall, less than half of `design.space.thumb target` = `56 px`, and visually identical to lines 2–N of the same message). Tapping any other line of the message does nothing, and if line 1 contains a URL, `open_inline` (`thread.rs:1658`) wins with no visual distinction between inspect vs link.
- **Cost:** `15 a day` (`agents.Look into the work`, phone). Fails `index.md` value **The phone is a full device** and leaves `agents.md` contradicting `chat.md`.
- **Direction:** Decide between two clean models and update `agents.md` to match:
  1. Make `look into the work` a direct action on the phone's `more` sheet (or dedicated bar/menu entry for the dealt message's writer) so one thumb never hunts an invisible 21px line; and/or
  2. Allow tapping anywhere on a non-link line of a project message (or a quiet per-message inspect affordance) — being careful not to conflict with scrolling/selection.

### 10. Narrowed `Desk` (`Project`) leaks scope on skip/next, hides project settings from threads, and leaves a dead `Project` list screen in code and `design.md`
- **Kind:** product
- **Level:** high-level
- **Status:** decided (2026-11-26: killed narrowed Project Desk completely; Palette filters by project and exposes global project commands; code cleanup queued for Loop 2) → fixed (2026-11-26, Loop 2 branch `loop2/c2-thread-desk`)
- **Where:** `desk.Desk` (`narrow`) / `chat.Project` / `agents.a refused start leaves a message` / `design.Row`
- **Evidence:**
  - Product model: `desk.md:24` & `chat.md:37` state *"a project has no screen of its own"* (opening a project narrows `Desk`), and `chat.md:242` rejected *"two send keys, one that starts an agent and one that only sends"*.
  - Code & model friction:
    1. **Narrowed Desk loses its project scope mid-deal:** `Frame::deal_next` (`crates/rho-frame/src/deal.rs:367-378, 495-501`) reads `self.desk_project(cx)`, which only returns `Some` when `self.desk_in_view()` is true. So pressing `space` on a narrowed Desk (`tide@lab`) opens `tide@lab`'s top card — but pressing `space n` / `ctrl-j` (or replying and then dealing next) runs with `desk_in_view() == false`, silently escaping `tide@lab` into cards from other projects.
    2. **Project settings are unreachable from inside its threads:** `ChatSystem::project_rows` (`crates/rho-chat/src/ui/projects.rs:112-182`) only offers `works on…`, `agents:`, and `runtime…` when `point.surface` is the project root (narrowed Desk). Yet when a start fails, the host posts inside the thread: *"reply to retry, or change where it works from the project's palette"* (`agent-host/rho-agent/src/host/pool/projects.rs:381`, `agents.md:220`). From that thread, the user cannot run `works on…` without leaving the thread, opening Palette, finding the project, opening narrowed Desk, running `works on…`, and returning to the thread!
    3. **Dead `Project` list UI & two-send-key code lingers:** `Frame::make_surface` (`crates/rho-frame/src/frame.rs:313-318`) intercepts attached project roots with `self.desk.clone()`, stranding ~400 lines of `ThreadView::is_list()` (`crates/rho-chat/src/ui/thread.rs:322-327, 754-783, 1086-1174, 2247-2266`, `SendOnly` on `ctrl-shift-enter` in `crates/rho-frame/src/thread.rs:46-50`) and stale `project list` specs in `design.md:76, 79, 81, 82`.
- **Cost:** `30 a day` (`desk.Glance at what waits` / `Switching projects`) + high friction on every failed start recovery. Violates `index.md` values **The system does what it can** and **Rare power, neatly tucked away**.
- **Direction:**
  - Fix: Delete the dead `ThreadView::is_list()` / `SendOnly` code path and stale `design.md` `project list` specs; preserve narrowed-project deal scope across `space n` / `ctrl-j` while dealing from a narrowed Desk.
  - Decide: Offer `works on…` / `agents:` / `runtime…` (scoped to the thread's project, e.g. `tide@lab › works on…`) directly from a thread's Palette too — especially when a `StartFailed` stop is in the thread — or keep them exclusively on narrowed Desk and give the `StartFailed` message a 1-step jump (`enter` or palette action) to fix the place.

### 11. Constitution & cross-module contradictions: device-local drafts, broken chime suppression on FIFO deal, and stale rejected-feature references
- **Kind:** product
- **Level:** high-level
- **Status:** fixed (2026-11-26: drafts stay device-local, `index.md:17` amended, stale refs fixed; chime part fixed in Loop 2 with per-card chime set & Desk suppression)
- **Where:** `index.md` (`user`) vs `chat.Draft` (`chat.md:115`) / `deal.Card` & `window.Status line` (chime) / `deal.md` & `agents.md`
- **Evidence:**
  1. **Draft sync contradiction:** `index.md:17` promises *"Their verdicts, notes, and drafts are the same on both [devices]"*, while `chat.md:115` states *"A thread has one draft on each device, not shared with other devices"* (matching code: `crates/rho-chat/src/ui/mod.rs:349-356` and `crates/rho-frame/src/place.rs:25-26` store drafts strictly locally, never in `rho-ledger`).
  2. **Chime logic broken after FIFO deal switch:** `Frame::evaluate_signals` (`crates/rho-frame/src/deal.rs:446-488`) tracks a single boolean `chime_above` (`max_priority >= CHIME_THRESHOLD = 1.2`) and does **not** suppress `chime_card` when `self.desk_in_view()` is true (so the chime can ring while looking straight at Desk). Worse, because any card waiting >24m stays above `1.2` (`crates/rho-dealer/src/curve.rs:28-38`), `chime_above` stays latched `true` — completely suppressing `AGENT_RECENCY_BONUS` instant completion chimes for an agent the user *just* replied to whenever any older card sits waiting. Meanwhile `AGENT_RECENCY_BONUS` (`cards.rs:189`) has zero effect on `DealOrder::Waiting(since)` (`rank.rs:472-478`), making it dead weight for ranking and broken for chiming.
  3. **Stale model contradictions:** `deal.md:15` cites deleted flow `desk.Glance at what moves` (rejected 2026-11-21); `chat.md:147` defends newest-first with *"the recap covers the cold return"* even though `## recap` was rejected the same day (`chat.md:78`); `agents.md:158, 177` still reference *"an explicitly chosen reply target"* (rejected 2026-11-21, `chat.md:79`).
- **Cost:** Cross-cutting trust and predictability across laptop↔phone handoff and 8–10h/day focus (`index.md` values **rho remembers, so the user does not** and **One place, in focus**).
- **Direction:**
  - Decide on **drafts**: either sync drafts sealed through `rho-ledger` (fulfilling `index.md:17`), or explicitly amend `index.md:17` and add a `bet` explaining why drafts stay per-device.
  - Fix the **chime**: suppress chime on `Desk` (matching lamp), and trigger chime per-card crossing (or per newly-ripe recency card) rather than one latched hand-wide boolean.
  - Fix the three stale `product/*.md` references during the next model alignment pass.
