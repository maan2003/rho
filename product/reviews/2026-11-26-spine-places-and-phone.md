# Review: Window, Navigation, Non-Editor Places & Phone (high-level)

- **Date:** 2026-11-26
- **Commit:** `4a42786ad`
- **Device:** static (laptop + phone model & codebase audit)
- **Harness / captures:** static audit of `product/*.md` and `crates/rho-window`, `crates/rho-frame`, `crates/rho-browser`, `crates/rho-terminal`, `crates/rho-shell-view`, `crates/rho-desktop-client`, `crates/rho-notes`, `crates/rho-agents-view`

## Summary

The window spine has a genuinely strong core: a single O(1) cross-area navigation `History` that preserves surfaces in place, one Magit-style `Transient` menu primitive, a fast unified `Palette` with full-text search, and clean `Phone` layout detection at `<= 600 px`. However, **two structural seams are broken across the whole product**: (1) the four non-editor/raw places (`Browser`, `Terminal`, `Desktop`, `Shell`) each invent incompatible navigation, mode, and input-escape rules — including `Desktop` hijacking `space` so the user cannot type spaces into remote apps — and (2) the `Phone bar` + `PhoneTool` composition breaks down badly on non-editor and writing screens (cramming 12 equal-width buttons into a 360 px Terminal bar, burying `Write an idea` behind `more` on Desk, teleporting `menu` across slots, and leaving Shell's pager and Desktop's draw controls unreachable by thumb). In addition, a default `Subject::Own` bug turns `Browser` pages, `Usage` charts, and `Note`s into dealable cards that can be accidentally `done`d (closing Chrome tabs) or `muted` in the synced ledger.

## Findings (ranked by `runs × cost`)

### 1. Four incompatible navigation & escape schemes across raw places — and `Desktop` hijacks `space`
- **Kind:** product
- **Level:** high-level
- **Status:** decided (2026-11-26: universal dual-escape across all raw places — `Escape` enters rho `NOR`, `F9` forwards literal `Escape` to guest) → fixed (2026-11-26, Loop 2 branch `loop2/c4-dual-escape-idea`)
- **Where:** `window.Screen`, `browser.Page`, `agents.Terminal`, `agents.Desktop`, `agents.Shell`
- **Evidence:**
  - **Product state:** `product/index.md` promises *"The same keys reach a terminal, a desktop, and a page in the browser"* (`An editor, not an app`) and *"the way back is one key"* (`Moving between places costs nothing`). In reality, the product splits into four different exit/navigation schemes:
    1. **Editor screens & `Shell`:** `ctrl-k` (back), `ctrl-j` (forward/deal), `ctrl-shift-backspace` (close), `ctrl-shift-j` (close & deal), `tab` (desk), `q` (close). In `Shell`, `ctrl-k`/`ctrl-j` leave the screen even while typing, whereas `ctrl-c`/`ctrl-d` only work in Insert mode.
    2. **`Browser` (`Page`):** `ctrl-k`, `ctrl-j`, `ctrl-shift-backspace`, and `ctrl-shift-j` are disabled and replaced with `ctrl-shift-f9` (back), `ctrl-shift-f10` (forward), and `ctrl-shift-f11` (close). `CloseAndDeal` and `tab` have no bindings at all.
    3. **`Terminal` (typing mode):** `ctrl-k` and `ctrl-j` are suppressed for the PTY, `ctrl-shift-f9/f10/f11` do not work, and there is no one-key way back — the user must switch to read mode (`ctrl-shift-n` or `ctrl-\ ctrl-n`) and then press `ctrl-k`, `tab`, or `q`. Meanwhile `ctrl-shift-backspace` and `ctrl-shift-j` still fire directly from typing mode.
    4. **`Desktop` (live view):** `space` is bound to `VerdictMenu` directly on `RhoDesktop`, making it impossible to type a space into any remote app without popping up rho's verdict menu. Worse, on Linux `on_physical_key` forwards raw evdev keypresses straight to the remote compositor *before* GPUI action dispatch (unlike `Browser`, which buffers `pending_key` until `key_down`), so every reserved chord (`ctrl-shift-;`, `ctrl-k`, `ctrl-alt-m`, `space`) leaks into the remote app as well. And `ctrl-k`/`ctrl-j`/`ctrl-shift-backspace`/`ctrl-shift-j` still fire on `Desktop` even though `product/agents.md` (`Desktop`, `Work beside an agent`) says other keys go to the app and leaving uses `ctrl-shift-;`.
  - **Code refs:** `crates/rho-frame/src/keys.rs:95-117`, `crates/rho-frame/src/keys.rs:237-257`; `crates/rho-terminal/src/ui.rs:280-291`; `crates/rho-shell-view/src/ui.rs:269-280`; `crates/rho-desktop-client/src/ui/mod.rs:151-155`; `crates/rho-desktop-client/src/ui/view.rs:352-365`, `crates/rho-desktop-client/src/ui/view.rs:479`; `crates/rho-browser/src/view.rs:1464-1534`.
- **Cost:** Paid on every switch in or out of Slack/web (`browser.Read and reply on a page`, ~10/day) and agent inspection (`agents.Work beside an agent`, ~3/day). Violates `An editor, not an app` and `Moving between places costs nothing`, and breaks basic typing on `agents.Desktop`.
- **Direction:**
  - **Immediate bug fixes:** Remove `KeyBinding::new("space", VerdictMenu, Some("RhoDesktop"))` and mirror `Browser`'s `pending_key` buffer in `DesktopView` so reserved window chords never leak to the remote compositor.
  - **Shape decision (choose one unified raw-place contract):**
    1. **One universal reserved-chord layer across ALL raw places** (`Browser`, `Terminal` typing, `Desktop` live): the same window chords (e.g., `ctrl-shift-f9/f10/f11` or `alt`/super-free chords, plus `ctrl-shift-;` for Palette and `ctrl-alt-m` for Idea) always work identically on Browser, Terminal, and Desktop, while ordinary chords (`ctrl-k`, `ctrl-j`, `space`) always belong to the guest.
    2. **Two-tier model with explicit escape-to-normal** (like `Terminal`'s `ctrl-shift-n` or tmux prefix): one universal escape chord puts Browser/Terminal/Desktop into a frame-owned normal overlay where `ctrl-k`/`ctrl-j`/`tab`/`q`/`space`/`:` all work identically, while raw mode only reserves `ctrl-shift-;` and the escape chord.

---

### 2. `Phone bar` + `PhoneTool` composition breaks thumb ergonomics (12-button Terminal bar, teleporting `menu`, broken/missing `Undo`)
- **Level:** high-level
- **Action:** fix
- **Status:** open
- **Where:** `window.Phone bar` across `chat.Thread`, `agents.Agent`, `agents.Terminal`, `agents.Shell`, `chat.Write an idea`, `notes.Notes`
- **Evidence:**
  - **Product state:** `window.Phone bar` is meant to give one-thumb `56 px` targets (`design.thumb target`) that stay predictable across screens. Instead, the bar blindly appends surface-specific tools onto the default navigation/writing bar, and hardcodes inconsistent slot orders:
    1. **`Terminal` typing bar explodes to 12 buttons (~30 px wide each on a 360 px phone):** `TerminalSurface::mode()` returns `Insert`, so the frame generates `[Back, Desk, Menu, Undo]` (4 buttons — and `Undo` is completely dead because `Terminal` has no editor) and then appends all 8 `Terminal` tools (`ctrl`, `esc`, `tab`, `left`, `down`, `up`, `right`, `read`), contradicting `product/agents.md` (`Terminal`) which expects only the terminal keys while typing.
    2. **`Shell` typing bar crams 7–8 buttons** (`[Back, Desk, Menu, Undo, recall, run, cancel]` + `stop` when running), mixing navigation, editor `Undo`, and shell `cancel` in one overcrowded strip.
    3. **`menu` teleports between slots on the #1 drill-down transition:** on a `chat.Thread` card, the bar is `[back, menu, done, snooze, more]` (`menu` in slot 2); one tap down on `agents.Agent`, the bar is `[back, done, snooze, more, menu]` (`menu` jumps to slot 5).
    4. **Broken/missing `Undo`:** `Write an idea` omits `Undo` (`[Cancel, Send]`), contradicting `product/design.md` (`Phone bar`: `cancel, undo, and send`). On `notes.Notes`, typing in `find or start:` sets `mode = Insert`, so the bar shows `[back, desk, menu, undo, new]` with no `enter`/`done` button — and `Undo` dispatches to the read-only note list editor instead of the query editor, doing nothing.
  - **Code refs:** `crates/rho-frame/src/phone.rs:346-380`, `crates/rho-frame/src/phone.rs:634-719`, `crates/rho-frame/src/phone.rs:785-794`; `crates/rho-terminal/src/ui.rs:163-215`; `crates/rho-shell-view/src/ui.rs:166-196`; `crates/rho-notes/src/list.rs:440-453`; `product/design.md:155-162`; `product/agents.md:70`.
- **Cost:** Hits every phone session (`Clear what waits` 50/day, `Look into the work` 15/day, `Write an idea` 12/day, `Work beside an agent` 3/day). Violates `The phone is a full device`, `design.thumb target`, and muscle memory.
- **Direction:**
  - Let a surface declare whether its `PhoneTool`s **replace** the bar (for raw/typing toolbars like `Terminal` typing and `Shell` typing, capped at <= 5–6 tappable slots or two-row key strip for `Terminal`) vs **augment** a single slot (`in_desk_slot()`).
  - Lock `back` (leftmost) and `menu` (consistent slot — either slot 2 or rightmost) across both `OpenCard::Here` and `OpenCard::Related`.
  - Only show `Undo` when the focused editor is actually writable (and route it to the focused editor, fixing `NotesView::query`), and add `Undo` to `composing_idea` to match `product/design.md`.

---

### 3. `Browser`, `Usage`, and `Note` are treated as dealable Cards — allowing bogus verdicts that close Chrome tabs and mute notes/charts
- **Level:** high-level
- **Action:** fix
- **Status:** fixed (2026-11-26, Loop 2)
- **Where:** `window.Screen`, `deal.Verdict`, `browser.Page`, `usage.Chart`, `notes.Note`
- **Evidence:**
  - **Product state:** `product/deal.md` states *"Slack in the browser has no rho verdicts"*, `product/notes.md` states *"a note about nothing stays off the desk"*, and `product/usage.md` has no verdicts. Yet in the built app:
    - Pressing `space` on a `Usage` chart or a `Note` opens a `deal.Verdict` menu (`done`, `mute`, `snooze`) targeting that chart or note.
    - Opening `Palette` (`:` / `ctrl-shift-;`) on a `Browser` page, `Note`, or `Usage` chart injects `done`, `mute`, and `snooze` verdict commands for that screen.
    - Running `done` or `mute` writes a real `Fact::Settled` / `Fact::Mute` into the synced ledger, **closes the current screen** (for `Browser`, closing the user's live Chrome tab!), and deals the next card. Muting a `Usage` chart, `Note`, or `Browser` page stamps `muted` permanently on its status line.
    - Structurally, `render_status_line` also relies on this bug by accident: `PageView::title()` returns `<title>` without `web ›`, and only gets `web › <title>` because `own_card` is non-`None` and falls into `card_for(&address).heading()`. Similarly, `NoteView::title()` and `UsageSurface::title()` are bypassed on the status line.
  - **Code refs:** `crates/rho-frame/src/surface.rs:102-117` (`subject()` defaults to `Subject::Own`, `card()` delegates to `subject()`); `crates/rho-browser/src/ui/mod.rs:416-422` (`PageView::subject()` explicitly returns `Subject::Own`); `crates/rho-notes/src/view.rs:184-196` and `crates/rho-agents-view/src/usage/surface.rs:246-254` (inherit `Subject::Own`); `crates/rho-frame/src/frame.rs:393-416`, `crates/rho-frame/src/frame.rs:1092-1110`, `crates/rho-frame/src/frame.rs:1206-1234`, `crates/rho-frame/src/frame.rs:1479-1504`; `crates/rho-frame/src/deal.rs:226-263`.
- **Cost:** Paid whenever the user opens `Palette` or presses `space` on a `Page` (~10/day), `Note` (~2/week), or `Usage` chart (~2/week) — risking silent tab closure and bogus ledger facts. Violates `rho remembers, so the user does not` (corrupting the verdict ledger with non-cards) and `deal.Verdict` / `notes.a note about nothing stays off the desk`.
- **Direction:**
  - Change `Surface::card()` default to `Subject::None` (only surfaces that represent or list dealable cards — `ThreadView`, `DeskView`, etc. — opt in), and return `Subject::None` on `PageView`, `UsageSurface`, and standalone `NoteView` (unless a note explicitly has a card/deadline).
  - Have `PageView::title()`, `UsageSurface::title()`, and `NoteView::title()` return their true status-line headings (`web › <title>`, `usage › <chart>`, live first line) directly instead of piggybacking on `card_for().heading()`.

---

### 4. High-frequency flows are buried or unreachable on Phone (`Write an idea`, `Shell` pager/EOF, `Desktop` drawing, non-editor touch)
- **Kind:** product
- **Level:** high-level
- **Status:** on-hold (phone deferred to backlog)
- **Where:** `desk.Desk` (phone bar), `chat.Write an idea`, `agents.Shell`, `agents.Desktop`, `browser.Page`
- **Evidence:**
  - **Product state:** `product/index.md` commits to two core promises: *"An idea must become an agent at work in one short step, from either device"* (`day` #2: `Turn ideas into work`, 12/day) and *"Every flow is possible on the phone. Where the thumb makes a flow longer, that is a cost chosen on purpose and written down, never a flow that the phone cannot do."* (`The phone is a full device`). Yet:
    1. **`Write an idea` is buried behind `more` on Desk** (`[menu, open, more]` -> tap `more` -> tap `write an idea`, 2 taps + sheet), and `product/window.md:123` still marks the Desk phone bar as `not agreed`. From any screen other than Desk, `Write an idea` requires opening `menu` (Palette) and typing/finding it.
    2. **`Shell` pager and `ctrl-d` EOF are completely unreachable on Phone:** when command output pauses (`[pager … — Alt-Enter more · Alt-A all · Alt-Q stop]`), there are no `PhoneTool`s and no Palette commands for `more`/`all`/`stop` or `ctrl-d` — only `alt-enter`, `alt-a`, `alt-q`, and `ctrl-d` keyboard bindings.
    3. **`Desktop` annotation controls violate thumb ergonomics and lack touch support:** the draw bar (`DRAW  u undo  y copy  esc live`) is a `12 px` desktop strip with `3 px` vertical padding rather than `56 px` thumb targets or `PhoneTool`s, and neither `DesktopView` nor `PageView` implements GPUI `TouchEvent` handlers (`on_touch`/`capture_touch`) — relying on mouse/pointer-axis synthesis (while `Phone::scroll_edge` returns `Middle`, disabling flick navigation on Browser/Terminal/Desktop).
  - **Code refs:** `crates/rho-frame/src/phone.rs:356`, `crates/rho-frame/src/phone.rs:403-406`, `crates/rho-frame/src/phone.rs:771-783`; `crates/rho-shell-view/src/ui.rs:166-196`, `crates/rho-shell-view/src/ui.rs:277-280`; `crates/rho-shell-view/src/surface.rs:648`; `crates/rho-desktop-client/src/ui/view.rs:485-520`; `crates/rho-browser/src/view.rs:2158-2181`; `product/window.md:123`.
- **Cost:** Paid on every phone idea (~12/day total across devices), every long `Shell` output on phone, and any `Desktop`/`Browser` touch use. Violates `The phone is a full device` and `Turn ideas into work`.
- **Direction:**
  - **Fix immediately:** Add `more` / `all` / `stop` `PhoneTool`s (and Palette rows) on `ShellSurface` when `pagers` is non-empty, plus an `eof` Palette row; expose `undo` / `copy` / `live` as `PhoneTool`s on `DesktopSurface` when annotating.
  - **Decide on Desk's phone bar (`product/window.md:123`):** Compare (a) `[menu, open, idea, more]` (one tap to deal OR start an idea directly from Desk, matching `day` #1 and #2), vs (b) `[menu, open, idea]` with `back`/`undo` inside `menu` (Palette) so Desk has no redundant `more` sheet at all.

---

### 5. `Approve a push or a fetch` clobbers open prompts, drops concurrent approvals, mislabels the sender, and risks accidental approval while typing
- **Level:** high-level
- **Action:** fix
- **Status:** fixed (2026-11-26, Loop 2)
- **Where:** `agents.Approve a push or a fetch`, `window.Prompt`
- **Evidence:**
  - **Product state:** `product/agents.md` (`Approve a push or a fetch`) states that when two agents push at once, *"the second question waits for the first"*, and the prompt names *"the agent's name, `git asks`, ..."*. In the built product:
    1. **Concurrent approvals silently kill the first push:** `ConnEvent::GitTransportApproval` calls `open_prompt_cancellable`, which blindly overwrites `self.minibuffer` without calling the previous prompt's `on_cancel` or queueing. The first approval's `oneshot::Sender` is dropped immediately, failing the first agent's push instead of queuing behind it.
    2. **Destroys active user prompts & hijacks typing:** if the user is mid-query in `Palette`, `pair this device`, or `attach host`, an unsolicited git approval silently wipes out their prompt and steals keyboard focus. Worse, because the user may already be typing in a thread/terminal and press `y` + `enter` right as the prompt steals focus, a security-sensitive SSH approval (`AGENTS.md`: *"Nothing displayed acts without the user"*) can be triggered inadvertently.
    3. **Wrong identity & contradictory prompt copy:** `hosts.rs` formats `"{name}: git asks `{prompt}` · allow? (y/n):"` where `name` is the **host** name, not the agent's name — and `{prompt}` itself already ends in `Approve? [shift-Y/N]` (`product/agents.md:208`), producing two conflicting instructions on one line.
    4. **Phone friction:** inside a 60-second timeout, the phone user has to summon the soft keyboard, type `y`, and tap `enter` instead of tapping `allow` / `deny`.
  - **Code refs:** `crates/rho-frame/src/hosts.rs:113`, `crates/rho-frame/src/hosts.rs:143-175`; `crates/rho-frame/src/frame.rs:1272-1296`; `crates/rho-agent-hosts/src/connection.rs:523-538`.
- **Cost:** Runs ~3/day (and spikes whenever parallel agents push). Directly violates `agents.Approve a push or a fetch` (queueing + agent identity), `hosts.bet The window holds the secrets`, `The phone is a full device`, and `AGENTS.md` Security.
- **Direction:**
  - Queue incoming `GitTransportApproval` events in `Frame` (FIFO) so a second approval waits for the first and never clobbers an active `Prompt` (if the user is in a prompt, defer or show a status-line notice until it closes).
  - Include the requesting agent's name in `ConnEvent::GitTransportApproval`, remove the duplicate `Approve? [shift-Y/N]` vs `allow? (y/n):` suffix mismatch, and make approval require an unambiguous explicit choice (e.g., tappable `allow` / `deny` candidates on phone and `y`/`n` or explicit confirmation on desktop, never accidentally confirmed by a stray `enter`).

---

### 6. `Palette`, `Prompt`, and `Hosts` have broken promises, inconsistent interaction, and awkward command ranking
- **Level:** high-level
- **Action:** fix
- **Status:** fixed (2026-11-26, Loop 2: desktop Palette/Prompt clicks, window-wide `new note`, host copy/redial rows)
- **Where:** `window.Palette`, `window.Prompt`, `hosts.Host`, `hosts.Know my hosts are reachable`, `notes.Note`
- **Evidence:**
  - **Product state:** `Palette` and `Prompt` are the universal command/navigation backbone (`Go to something by name`, 20/day; `Run a command I rarely use`, 2/day), yet four rough edges fight the user's habits:
    1. **Documented query recall does not exist:** `product/window.md` (`Prompt`, lines 107 & 111) states twice that `up` recalls the last Palette query — directly contradicting `Palette` (line 52, `up`/`down` wrap rows) and the code, where `up` only wraps candidate selection and zero prompt history exists.
    2. **Desktop mouse clicks work in `Menu` and two special `Prompt`s, but NOT in `Palette` or ordinary `Prompt`s:** `Minibuffer::render` enables row clicks only for `CompletionInput::Address`, `Explicit`, or `Layout::Phone`. Clicking a Palette match on laptop does nothing, even though clicking a `Menu` row works.
    3. **`Hosts` has stale spec wording and no-op Palette rows:** `product/hosts.md:43` still describes a `hosts` menu (`1 · rho · online`) despite `product/window.md:55` explicitly rejecting a `hosts` menu. In code, `hosts_menu().into_commands()` flattens each host into a Palette command like `hosts › rho · online` whose entire action on Enter is echoing `"rho: online"` (repeating the row label). Meanwhile `hosts › attach` dumps a dense raw syntax spec into the prompt label (`name=local (this machine), name=iroh:<id>@<ssh host> (ssh), or name=iroh:<id> (security key):`), and an enrollment command printed in red on the status line (`product/hosts.md:91`) cannot be copied or selected.
    4. **Rare `new note` outranks `write an idea` on every screen:** `NoteSystem::rows()` registers `new note` as `Scope::Here` on every screen even though `product/index.md` classifies Notes as #5 ("Rare", 2/week), causing `new note` to rank above `write an idea` (`Scope::Window`, 12/day — ~30× more frequent) in the blank Palette.
  - **Code refs:** `crates/rho-frame/src/keys.rs:164-166`; `crates/rho-frame/src/minibuffer.rs:431-444`, `crates/rho-frame/src/minibuffer.rs:625-627`, `crates/rho-frame/src/minibuffer.rs:798-811`; `crates/rho-frame/src/frame.rs:1063-1076`, `crates/rho-frame/src/frame.rs:1114-1184`; `crates/rho-notes/src/lib.rs:433-451`; `product/window.md:52,55,107,111`; `product/hosts.md:43,91`.
- **Cost:** Paid across `Go to something by name` (20/day), `Run a command I rarely use` (2/day), and `Attach a host` / `Know my hosts are reachable`. Violates `Rare power, neatly tucked away` and `An editor, not an app` (consistent prompt/menu behavior).
- **Direction:**
  - Make all `Palette` and `Prompt` candidate rows clickable on Desktop too (matching `Menu`).
  - Resolve the `up` contradiction: remove the unbuilt `recall up` claim from `product/window.md` (or bind recall to a non-conflicting key like `alt-p` if actually desired).
  - Mark `new note` as `.window_wide()` so it stops outranking screen-local commands and `write an idea`.
  - Clean up `hosts`: update `product/hosts.md:43` to match the rejected-menu decision, make selecting a disconnected/unenrolled host row copy its enrollment command or trigger reconnect rather than echoing its own label, and simplify `hosts › attach` prompt copy.

---

### 7. The status line mode badge lies on `Browser` and `Desktop` (showing `NOR` while raw typing is active)
- **Level:** high-level
- **Action:** fix
- **Status:** fixed (2026-11-26, Loop 2 branch `loop2/c4-dual-escape-idea`: `PageSurface` and `DesktopSurface` report their own `Surface::mode`)
- **Where:** `window.Status line`, `browser.Page`, `agents.Desktop`
- **Evidence:**
  - **Product state:** `product/design.md` (`keys in motion`, `Status line`) defines the mode word at the top-right as telling the user *"how the keyboard will act"*. `TerminalSurface` correctly reports `INS` while typing and `NOR` while reading. However, neither `PageView` nor `DesktopSurface` implements `Surface::mode()`. Because neither has an `Editor`, `Frame::mode()` falls through to `self.mode_indicator.plain_mode()`, which displays **`NOR`** (or whatever stale vim mode the previous editor was in!) while every keystroke goes straight to Slack or the remote desktop app.
  - **Code refs:** `crates/rho-frame/src/frame.rs:1569-1602`; `crates/rho-browser/src/ui/mod.rs:415-443`; `crates/rho-desktop-client/src/ui/mod.rs:91-114`; `crates/rho-terminal/src/ui.rs:209-215`.
- **Cost:** Paid on every visit to `browser.Page` (~10/day) and `agents.Desktop`. Actively misleads the user into thinking `:` / `q` / `tab` / `j` / `k` will act on rho when keys are actually being typed into Slack or a remote window. Violates `An editor, not an app` and `design.Status line`.
- **Direction:** Implement `Surface::mode()` on `PageView` (returning `Some(Mode::Insert)`) and `DesktopSurface` (returning `Some(Mode::Insert)` when live, `Some(Mode::Normal)` when connecting/failed/annotating — or `DRAW` when annotating).

---

### 8. `Terminal`, `Shell`, and `Desktop` have asymmetric discoverability and orphan running terminals
- **Kind:** product
- **Level:** high-level
- **Status:** fixed (2026-11-26, Loop 2 Wave B C5 `b76c9b234`: flat peers with `Thread`, workset-centric transcript with agent filter, global Palette commands, running terminals/shells in `find`, `Shell` marked experimental)
- **Where:** `agents.Work beside an agent`, `agents.Terminal`, `agents.Shell`, `agents.Desktop`, `window.Palette`
- **Evidence:**
  - **Product state:** `window.Palette` is supposed to make any place *"one name away"* (`Moving between places costs nothing`), and `agents.md` treats an agent's `Terminal`, `Shell`, and `Desktop` as peer tools for working beside an agent. In the built app:
    1. **Cannot reach `Terminal`, `Shell`, or `Desktop` from a `chat.Thread`:** `AgentsSystem::rows()` offers `Steer an agent` commands (`cancel`, `rewind`, `set runtime…`) directly from the thread, but only offers `terminal`, `new terminal`, `shell`, `desktop…`, and `stop agent` when `point.surface` is already an `AgentAddress` (`Agent` transcript or one of its tools). The user must always take an extra hop into the transcript first.
    2. **Running terminals and shells are missing from `Palette` (`find()`):** `AgentsSystem::find()` registers `agents`, `desktops`, and `usage` charts as searchable places, but omits running terminals and shells. Once closed with `q` (which leaves them running on the host per `product/agents.md:70,88`), they cannot be found by name in `Palette`.
    3. **Secondary terminals become permanently orphaned:** `pick_terminal(running, new: false)` always picks `running.first()`. If the user starts `term 1` with `new terminal` and later closes its screen with `q` (leaving `term 0` and `term 1` running on the host), `terminal` will forever reopen `term 0` — there is no picker or Palette entry to ever reach `term 1` again!
    4. **Redundant `verdicts` Palette row:** `agent_rows` adds a `verdicts` row on every agent/terminal/shell/desktop even though `Frame::commands()` already puts `done`, `mute`, `snooze`, `next card` directly in the Palette.
  - **Code refs:** `crates/rho-agents-view/src/system/mod.rs:828-865`, `crates/rho-agents-view/src/system/mod.rs:1217-1286`, `crates/rho-agents-view/src/system/mod.rs:1324-1373`; `crates/rho-terminal/src/ui.rs:26-32`; `crates/rho-frame/src/frame.rs:1092-1110`.
- **Cost:** Paid on every `Work beside an agent` flow (~3/day) and whenever a user runs >1 terminal per agent. Violates `Moving between places costs nothing` and `rho remembers, so the user does not`.
- **Direction:**
  - **Fix:** Remove the redundant `verdicts` row from `agent_rows`, and when an agent has >1 running terminal, make `terminal` prompt which running terminal to open (matching `desktop…`'s exact behavior when an agent has >1 desktop).
  - **Decide:** Should `terminal` / `shell` / `desktop…` also be offered from a `chat.Thread` when a worker is unambiguous (matching `Steer an agent`), or intentionally stay one level down on `Agent` (`agents.bet An agent is labour, not a colleague`)? And should running terminals/shells be streamed into `Palette::find()` the same way desktops already are?

---

### 9. Warm restart and early `back` permanently erase host-backed screens from `History` (plus unresolved `not agreed` loading/error states)
- **Level:** high-level
- **Action:** fix
- **Status:** fixed (code part, 2026-11-26, Loop 2: `Opening::{Surface, Waiting, Gone}`; `product/` loading/error states remain `not agreed`)
- **Where:** `window.Screen`, `desk.a cold return opens the desk`, `desk.Desk` (states), `chat.Thread` (states), `agents.Desktop`
- **Evidence:**
  - **Product state:** `product/desk.md` (`a cold return opens the desk`) promises that returning within ten minutes leaves the user's `window.Screen` where they left it, and `product/window.md` promises the way back keeps earlier screens. In the built app:
    1. In `Frame::new`, `attach_host` spawns async network connections and **immediately** calls `return_to(saved)` synchronously before any host has connected or populated `AgentsSystem::registry`.
    2. `AgentsSystem::open` returns `None` for any `Agent`, `Terminal`, or `Shell` not yet in `registry.mirrored(agent)` — and `Frame::return_to` / `Frame::settle` immediately call `self.history.forget(&address)` and persist `self.moved()`, **permanently erasing** the user's active screen (and any screen stepped onto with `ctrl-k` before sync finishes) from `History`.
    3. For `Desktop`, `AgentsSystem::open` instead returns a static `DesktopSurface::unavailable("the agent's host is not attached")` or `"the agent no longer runs this desktop"` error surface that never upgrades to live once the host connects seconds later.
    4. This mirrors the unresolved `not agreed` holes across the model: `desk.Desk` (`loading`, `partial`, `error`), `chat.Thread` (`loading`, `partial`, `error`), and `window.Palette` (`error`).
  - **Code refs:** `crates/rho-frame/src/frame.rs:265-304`, `crates/rho-frame/src/frame.rs:330-349`; `crates/rho-agents-view/src/system/mod.rs:93-127`, `crates/rho-agents-view/src/system/mod.rs:1141-1182`; `product/desk.md:33-35`; `product/chat.md:130-132`; `product/window.md:69`.
- **Cost:** Paid on every restart within 10 minutes (or phone app resume) while on an agent/terminal/shell/desktop, or when stepping back while a host is reconnecting. Directly violates `rho remembers, so the user does not` (*"Does this change ever make that memory wrong, or lose it?"*).
- **Direction:**
  - Never call `history.forget(&address)` when a host is still connecting (`HostStatus::Connecting`/`Recovering`): render a lightweight `connecting to <host>…` placeholder surface (like `TerminalSurface`'s `Dialing` state) that automatically swaps in the real surface once the host syncs, and only forget an address once its host is online and confirms the resource no longer exists.
  - Settle the remaining `not agreed` loading/error wording in `product/desk.md`, `product/chat.md`, and `product/window.md` around this same reconnecting pattern.

---

### 10. `Notes`, `Browser` bookmarks, and `Usage` introduce awkward mini-UI seams and device gaps
- **Kind:** product
- **Level:** high-level
- **Status:** fixed (2026-11-26, Loop 2: ledger-synced bookmarks + `remove bookmark…`, Notes prompt keys, in-place usage switching)
- **Where:** `notes.Notes`, `browser.Bookmark`, `usage.Chart`
- **Evidence:**
  - **Product state:** Three smaller places diverge from rho's core editor/sync model in ways that create surprising friction:
    1. **`Notes` (`Find or start`) reinvents a half-baked `Prompt`:** `NotesView` embeds a separate `RhoNotesQuery` input bar at the top that overrides `/` (breaking standard in-buffer search) and binds only `enter`, `escape`, `up`, `down`, `ctrl-n`, `ctrl-p` — missing standard `Prompt` editing keys (`ctrl-w`, `ctrl-u`, `ctrl-a`) and breaking phone `Undo` (Finding #2).
    2. **`Browser` bookmarks are device-local and undeletable:** `bookmarks.rs` stores bookmarks in a local redb table rather than the synced ledger (`product/browser.md:93`), so bookmarks saved on laptop (notably Slack!) do not exist on the phone where typing URLs is hardest — conflicting with `product/index.md:17` (*"Both devices show the same places and the same hosts"*). There is also no command to remove/delete a bookmark once saved.
    3. **`Usage` splits 4×2 charts into 8 separate screens with no in-screen switching:** Opening `usage › rate limit · 7d` gives no key or Palette action (like toggling `7d` ↔ `30d` or stepping charts) on the screen itself; switching windows opens a brand-new history entry via the global Palette, piling up to 8 `usage:*` screens in `History`.
  - **Code refs:** `crates/rho-notes/src/list.rs:73-95`, `crates/rho-notes/src/list.rs:495-520`; `crates/rho-browser/src/ui/bookmarks.rs:11-12`, `crates/rho-browser/src/ui/bookmarks.rs:36-80`; `crates/rho-agents-view/src/usage/surface.rs:246-254`.
- **Cost:** Lower daily frequency (`Write something down` 2/week, `Check what is left` 2/week, `Bookmark` setup + daily Slack opening on phone), but compounds friction and erodes *"An editor, not an app"* and *"Both devices show the same places"*.
- **Direction:**
  - **Fix:** Add `remove bookmark` (at least on a bookmarked page or via Palette/prompt) and bind `ctrl-w`/`ctrl-u`/`ctrl-a` in `RhoNotesQuery` (or unify its keymap with `RhoMinibuffer`).
  - **Decide:**
    1. Sync `browser.Bookmark` through the sealed ledger (like verdicts and notes) so laptop bookmarks (Slack, PR dashboards) are immediately one tap away on phone.
    2. On `usage.Chart`, either add simple in-screen switching (`7`/`3` or a Palette `switch to 30 days` / next-chart action that replaces or updates the view without polluting history) or explicitly accept 8 separate palette places as intentional for a rare 2/week flow.
