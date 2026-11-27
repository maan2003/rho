# chat

The user writes a project a message to put an idea to work. Each piece of work has one thread, with a subject and the messages so far. While the work runs, the project's latest message says what is happening now.

## bet A message is the record, a flash is the now
instead of: a chat stream where everything an agent says has the same weight, or mail where every word is kept.
serves: Keep the work moving.
because: one thread keeps one piece of work together. Its subject identifies the work, and the project speaks with one voice even when several agents do the work. What asks the user or hands them a result is a `Message`, kept and dealt. Everything else an agent says while it works is a `Flash`, seen if the user is there and gone in twenty minutes. Agents are trained to chat, and the flash is where that chat goes without a rule to learn.
costs: the user opens the transcript to see which agent did what, and a flash they did not see is gone.

An agent writes to another agent in a message in a thread under the project; the user can read it one level down and never gets a `deal.Card` from it.

## bet The project asks in a message, even when an agent stops
instead of: attention coming from a mixture of mentions from named agents and notices about their state.
serves: Know what wants them, and where they were.
because: the user only needs what asks for them or returns a result, not each agent's progress. A stop or a pending push approval sits right in the thread as another thing to answer, not a separate kind of screen to watch.
costs: a failure can still ask for the user when they have nothing to add.

## journey An idea becomes a merged result
goal: the user turns an idea into merged work.
starts when: the user has an idea.
steps:
1. the user writes to the project from where they are, `Write an idea`
2. the user sees the work start, in the thread row's `working` word in `window.Places` (or by filtering `working:`), or by the project's next message
3. the user opens the project's ask when it needs something, `deal.Clear what waits`
4. the user answers the ask in its thread, `Answer a message`
5. if merging pushes to `main`/`master` or needs ssh, the user inspects the staged refs/commits/diff right in the thread and approves (`space a`) or denies with words (`space x`), `agents.Approve a push or a fetch`
6. the user opens the result, `deal.Clear what waits`
7. the user looks into the work if they need to see how it was done, `agents.Look into the work`
8. the user reads the result and says done, `deal.Clear what waits`
done when: the work is merged and nothing waits.
day: Turn ideas into work.

## concept Project
purpose: give each project one address to write to, and one answer, asked once, to where, how, and on what model its agents work.
principle: the user sends an idea to `tide` (or `tide@lab` when two attached hosts share the name `tide`); the project starts the work at its place, in its mode, on its runtime, and sends its asks and results from that address.
sees: its name (such as `tide`, adding `@host` only when two attached hosts have a project of the same name); where it works, its mode, and its runtime when configuring it in `window.Commands` or filtering by it in `window.Places`. A project has no screen of its own.
has:
- many `Thread`
does:
- filter in `window.Places`: choosing a project in `window.Places` scopes the finder to that project's threads
- receive an idea: start the work at the project's place using the idea's `in:` mode (defaulting to the project's mode) and `on:` runtime (defaulting to the project's runtime for a new agent). `one` (`one agent`): the project's standing agent reads the idea in its new thread and keeps its current runtime unless the user explicitly changed `on:` (an untouched quiet default `on:` never reverts a manual `set agent runtime` choice); it starts on the project's runtime when none runs. `new` (`per idea`): a fresh agent starts in a fresh workset, a clone of the repository or the directory itself
- set project place, from `window.Commands` anywhere (inline or prompt): uses the current thread's project by default (overridable inline) or asks `project:` first; then asks `<project> works on:` and changes where the next idea (or retried refused start) starts; agents already running stay where they are
- set project mode, from `window.Commands` anywhere (inline or prompt): uses the current thread's project or asks `project:` first; changes between `one` and `per idea` for the next idea; agents already running stay as they are
- set project runtime, from `window.Commands` anywhere (inline or prompt): uses the current thread's project or asks `project:` first; asks `<project> runtime (sol-high):` with the host's runtimes as answers; the next new agent starts on it; agents already running stay as they are
- rename project, from `window.Commands` anywhere: renames the project cleanly across its threads
- retire project, from `window.Commands` anywhere: retires a finished or dead project so it leaves `to:` completion while keeping its historical threads searchable in `window.Places`
rejected:
- a narrowed `desk.Desk` screen per project · 2026-11-26 · Desk is one global glance of what waits; project filtering belongs in `window.Places`, dealer scoping belongs in `deal.Card.set dealer focus`, and project commands work globally from `window.Commands`
- the place read off the project's newest agent · 2026-11-21 · nothing could show it or change it
- the kind of place deciding the mode, a repository per idea and a directory shared · 2026-11-21 · one standing agent on a repository is useful too
- every project agent on `opus-high`, with the model chosen only through `start agent` · 2026-11-21 · that path asks the place again and never makes a project thread, so no project thread could start on another model

## concept Thread
purpose: keep one piece of work and the whole exchange about it together.
principle: the user writes to `tide` about a flaky test; the project asks about the test, stages any protected push right in the thread, and later sends the result in the same thread, whatever agents did the work.
sees: its subject and project; on a row, what waits, or the project's latest words when nothing waits (or `<host> away` when its host is away); open, the `Draft` first, any pending push/fetch approval item at the top, then its messages newest first, oldest last
has:
- many `Message`
- many `Thread`, each a tangent under a message (shown as a quiet `↳ N tangents` line beneath the parent message; `enter` opens them)
- one `Draft` at the top of the thread
does:
- open: one uniform rule everywhere — (1) opening a full-text match from `window.Places` jumps directly to that matched message/line; (2) if a new message, edit, or pending push arrived since the user last viewed the thread, open at the top (`Draft` and newest item); (3) otherwise restore the thread's scroll position and cursor as left
- reply: write in the `Draft` and send with `Message.reply`
- rename thread, from `window.Commands` (inline `:rename New title` or prompt): change the thread's subject
- fork thread, from `window.Commands` on a message: start a tangent `Thread` under that message, quoting it in the new draft

## concept Message
decided: 2026-11-26 · agent is primary unmarked reading voice (bright unread, softer read), user is muted teal with 2px left gutter bar, right-aligned relative ages (amber unread, quiet older), no date separators, no history folding
purpose: carry an idea, an ask, or a result in the voice of the project, keep it as the record of the work, and let the user answer in the thread.
principle: the project asks a question; the user reads it on top of the thread with the project's earlier words one glance below, and replies without needing to know the writer's name.
sees: the words, with the voice told by colour and gutter alone: the project's message as the unmarked primary reading voice (bright when unread, softer once read), the user's own message in muted teal (`design.marked`) with a `2 px` teal left gutter bar; a flush-right relative age on the first line (`attention` amber on unread turns, `quiet` on older turns); a quiet name line above a message from anyone else (or a quiet agent short code when more than one agent has a live message in the same thread); a quiet tag at the end of the text, such as `edited` or `taken back`; what the message quotes. Never inserts date separator lines or collapses older history.
does:
- ping: every message from the project pings the user. It is dealt as a `deal.Card` and reads `waits · 2h` on its row. An agent names the kind of everything it sends, `kind="message"` or `kind="flash"`, with no default, so it decides each time whether the user is needed
- look into the work `enter` on a project message or `Flash` (when cursor is not on a private reply or link): open the `agents.Workset` transcript at the step that produced its current words; a failed stop opens its error
- reply: the user's words go into the thread, and every agent in it reads them (mid-turn delivery is handled directly by the host harness, so the user does not need a separate cancel step); there is no addressee. With two asks open, the user names the agent in words. If no agent exists yet (a refused start), replying retries using the idea's recorded `in:` and `on:` overrides together with the project's latest place (falling back to project defaults if untouched); if an existing agent is `stopped` or `failed`, replying wakes it in its own workset on its own runtime (preserving any `set agent runtime` choice)
- edit or delete: agents may change or take back their own messages, except that a message the user has answered stays as it was. A real edit reads `edited` and deals the thread again (with a recovered system stop as the one clearing exception, see `agents.a stuck agent asks`)
- reply privately `alt-r`: write a remark directly beneath this message, as in `notes.Reply privately`
rejected:
- a required `## proof` section on every result message · 2026-11-26 · rigid templates fight different kinds of work; let agents back up results flexibly in prose, links, or captures, and use the workset transcript when deeper inspection is needed
- replacing conversational request-response threads with a rigid mutable casefile state machine · 2026-11-26 · back-and-forth brainstorming and follow-ups need real conversation
- kinds of message, `ask`, `result`, and `fyi` · 2026-11-20 · one yes or no, whether the message pings, does the job; a result waits like an ask
- a message that is kept but does not ping · 2026-11-21 · anything worth the record is worth a card, and agents used it to be polite; progress is a `Flash`
- a `## recap` section on a cold message, so it reads without the history · 2026-11-21 · with the newest message on top, the context is one glance below; the recap fold hid whole messages, and agents wrote it badly
- a reply with an addressee: the draft answers one message, says so with an `answers …` line, `r` retargets, and the host delivers the words to that writer only · 2026-11-21 · a chat message has no target; every agent in the thread reads it and the user names one in words when it matters. The deal still settles the card the user came from
- a header with the writer and the time above each message · 2026-11-20 · colour tells the voice, and the top line says how long the thread has waited
- a status line kept apart from the messages, replaced rather than kept · 2026-11-20 · a `Flash`, replaced as the work moves, does the same job without a second surface

Agents can search messages. A message written while the host is away waits on the device and goes out when the host is back. A submitted idea says `queued for` its project and enters `window.Places` MRU immediately (using its first line until titled) until the host's thread takes its place.

## concept Flash
purpose: let an agent say what it is doing or about to do, for the user who happens to be there, without a card and without a trace.
principle: an agent writes `I plan to drop the cache layer, say no in 20 min` and starts; the user who is in the thread replies `no`, and the agent changes course; the user who is away never sees it, and the thread does not grow.
sees: the words in `design.quiet` under the draft, newest first among the thread's messages, with the age at the end of the first line, `12m`; one open flash per agent, with its earlier fresh flashes folded under it as `+2 earlier`; on the project's row, the newest flash's first line as the project's latest words
does:
- post, by an agent with `kind="flash"`: show it in the thread for twenty minutes from its first post, longer while it is the thread's newest item from either side; it goes when the clock has run and something newer exists, a message or a flash from anyone; then it is gone from every device, not kept as history
- look into the work `enter` on a flash: open the `agents.Workset` transcript at the step that posted it
- reply: the user's reply is an ordinary `Message` to the thread; it does not keep the flash
- edit: the agent can change its words; the clock runs from the first post
rejected:
- one flash per agent, each replacing the last · 2026-11-21 · they vanish anyway; folding the earlier ones costs nothing and keeps a short plan readable
- an `ephemeral=True` flag with `False` as the default · 2026-11-21 · a default lets the model skip the choice; a required `kind` makes it pick one every time
- an `expires_in` parameter · 2026-11-21 · one clock is easier to trust than a number each agent tunes
- a flash that fades on its clock even while it is the newest thing in the thread · 2026-11-21 · the user who opens the thread an hour later still wants the last state of the work; the next message or flash replaces it
- a soft ask, a flash that pings and takes its card with it when it fades · 2026-11-21 · a card that vanishes breaks `nothing addressed to the user disappears unread`; what needs the user is a `Message`, so the agent names one of two kinds and nothing else
- a kept message that does not ping, for progress · 2026-11-21 · see `Message`

## concept Draft
decided: 2026-11-26 · empty thread draft has zero placeholder text (just the 2px teal left gutter bar and cursor on row 1)
purpose: keep what the user wrote in a thread and has not sent, so leaving the thread loses nothing.
principle: the user is halfway through an answer when a card that matters comes in; they deal it and answer it; later, on the same device, they open the first thread, and the half-written answer is still at the top of the thread, even after a restart.
sees: the unsent text in muted teal (`design.marked`), with a `2 px` teal bar in the left gutter, on row 1 at the top of the thread above a soft horizontal hairline seam; when empty in a thread, shows no placeholder word at all — just the teal left gutter bar and cursor on a clean empty line. When its last line is above the window, the draft becomes a `window.Edge-stuck excerpt` at the top edge, on a lifted surface; on the phone it is in the half above the keyboard.
does:
- edit `i`, `A`, `o`, or a tap on the draft: write in place; `escape` returns to Normal with the cursor still in the draft
- quote `>` on a selection in one read-only message: insert those lines with `> ` in front, just above the last draft cursor; resume beneath the quote in Normal (inside the editable `Draft`, `>` stays normal Helix indent)
- send `ctrl-enter`, or `send` on the phone bar while writing: the words become the newest message, right below the empty draft; the point rests in the draft and nothing scrolls, then or when the project answers
- undo `u`, redo `shift-u`: the editor's undo while writing; on the phone, `undo` on the bar
rejected:
- quoting on `enter` when selection width is `> 1` char · 2026-11-26 · Helix motions (`w`, `e`, `b`, `x`, `/`) leave multi-char selections, causing accidental quotes when pressing `enter` on links or messages; `>` is dedicated to quoting on read-only messages
- move the point to the project's next pinging message after sending · 2026-11-21 · the user stays in the draft; the history moves as needed
- a separator line, `── reply to tide@lab · ctrl-enter sends`, between the history and the draft · 2026-11-20 · the draft is the next message; a line of chrome on every thread costs more than it says
- a `to:` prompt or a one-line strip for a new idea · 2026-11-20 · see `Write an idea`
- the draft as the last message at the end of the thread, stuck to the bottom while reading · 2026-11-21 · with the newest message first the draft leads, and no open or fill rule is needed

A thread has one draft on each device, not shared with other devices.

## screen Thread
decided: 2026-11-26 · title & project in Thread's top-left title row (no model name, no waits·age; muted / snoozed until … / approve push / working / stopped shown when active), Draft on row 1 with no placeholder, 128ch centered sheet with 80ch prose wrap and full-sheet code/diffs/tables/images, Mono + Balanced, 0px square corners
answers: what is this piece of work waiting for from the user?
shows:
- the `Thread` subject, `Project` address, and any active page marker (`muted`, `snoozed until …`, `approve push`, `working`, `stopped by you`, `stuck`) in Thread's top-left title row beside the notch (omitting redundant `waits · age` and model name, which lives in `agents.Transcript`)
- inside a `128 ch` centered sheet (`80 ch` wrap for prose/quotes/private replies; full `128 ch` width for code blocks, diffs, markdown tables, push cards, and images):
- the `Draft` first on row 1 (no `reply` placeholder, `2 px` teal left gutter bar, soft hairline below), or stuck to the top edge while the user reads history below
- any pending staged push/fetch approval right below the draft (naming repo, refs, force-push dropped count, commits, bounded diff, and `space a approves · space x denies (with draft reply) · space s snoozes`)
- the `Message` newest first, readable and copyable with the same motions and search as the `Draft`, but not editable; agent messages are the unmarked primary voice (bright unread, softer read), user messages are muted teal with a `2 px` left gutter bar, and relative ages sit flush-right (amber unread, quiet older); no date separators and no collapsed history
- the fresh `Flash` of each agent in `design.quiet` with its age flush-right, in its place among the messages, with that agent's earlier fresh flashes folded under it
- a `notes.Private reply` below its message, editable there; tangents appear as a quiet `↳ N tangents` line below their parent message and open as their own `Thread`
states:
- empty: the empty draft, then the opening message; a thread begins with a message
- loading: locally cached messages if available, or `connecting to <host>…` while syncing
- partial: available messages with quiet `<host> away` note in the status line when offline
- error: the thread is unavailable on an unreachable host
- full: follows `Thread.open` — full-text match jumps to that line; unseen message/edit/push opens at top `Draft`; otherwise restores scroll & cursor as left
keys:
- `enter` on a message/item always opens what is under the cursor regardless of Helix selection width: (1) `notes.Private reply` → edit in place; (2) tangent line (`↳ N tangents`) → open tangent; (3) link → `browser.Page.open`; (4) project `Message` or `Flash` → `Message.look into the work` (workset transcript at that step)
- `>` on a selection in one read-only message: `Draft.quote` (inside `Draft`, `>` stays Helix indent)
- `ctrl-u`, `ctrl-d`: scroll history by half a window without moving the point out of the `Draft`
- `gg`: go to the draft; `G`: go to the oldest message; `i`, `A` or `o`: `Draft.edit`; `ctrl-enter` or `send`: `Draft.send`
- `alt-r` on a message: `notes.Private reply.reply privately`
- `u`, `shift-u` in the draft: undo and redo the last edit
- `space`: silent `window.Screen` prefix (`space f` places, `space u` undo, plus `deal.Verdict` chords when waiting — or `space a` approve / `space x` deny / `space s` → `:snooze thread ` when a push/fetch awaits), anywhere in Normal mode
- `:`: `window.Commands`
- `space-j` (`ctrl-j`), `tab`, `q`: `window.Screen`
starts: `Answer a message`, `agents.Look into the work`, `agents.Approve a push or a fetch`, `notes.Reply privately`
captures: [asking message on the laptop](design/laptop-card.png) and [on the phone](design/phone-card.png), [an outcome with its proof](design/laptop-proof.png), [an older thread with history](design/laptop-reading.png), and [the notice in a stuck phone draft](design/phone-reading-notice.png).
rejected:
- the message that pings at the top of the window · 2026-11-21 · open on what the project has said since the user's last word, with the next reply ready in either way of opening
- oldest message first, the draft at the end · 2026-11-21 · the first project message since the user's last word could not be at the top and the window full at once; with newest first and the draft leading, open is always the top and the user's own last message separates new from history
- the agent's transcript beside the message in a second column · 2026-11-20 · one place in focus; the transcript is a flat peer
- the card alone, with the history folded · 2026-11-20 · a short history costs nothing and a long one is one scroll away
- the agent's work folded under each message · 2026-11-20 · the transcript is `agents.Look into the work`
- a project split with the thread list at the left · 2026-11-20 · one screen at a time
- marking the message under the point by brightness or a bar · 2026-11-20 · the cursor is enough

The point never rests on the blank line between two messages. A reply from a waiting thread (without a pending push/fetch) settles the visible waiting card whether the thread was opened by the deal or by name, and stays on the thread; the next card waits for `deal.Card.deal`.

## flow Answer a message
status: the user's Slack is in the browser, and the bot is not built.
runs: 100 a day
day: Keep the work moving.
when: a project or the Slack bot has something that asks for the user, or the user opens a result
want: read what arrived and answer in place if needed
so that: the person or project that needs the user's words gets them, and the message no longer waits
steps:
1. the user opens the thread by the deal or by name and reads what the project wrote since their last message
   - when there is nothing to say, the user gives `deal.Verdict.done` and stops here
2. the user writes and edits an answer in the `Draft` at the top of the thread
3. the user sends the answer with `Message.reply`; the reply settles the waiting card on this device even while it waits to be delivered. If the host refuses it, the card returns and the unsent answer stays in the thread, marked `not sent`; `write reply again` in `window.Commands` takes it back into the empty `Draft` to change and send again.
system: opens at top `Draft` when new messages arrived (else restores view), with messages below newest first. The user's last message separates what arrived since they wrote from earlier history. The thread keeps the draft if the user leaves. The reply is delivered by the host harness to every agent in the thread, waking any stopped/failed agent in its own workset & runtime (or retrying a refused start using the project's latest place & runtime).
user: the words of the answer, or done when there are none.
cases: a question answered after a day away, with the project's earlier words one glance below; a result with proof; a colleague reached through `slack.Bot`; replying after returning via `window.Places`; replying while the host is away (queued offline).

## flow Write an idea
runs: 12 a day
day: Turn ideas into work.
when: the user has an idea, wherever they are in the window
want: send it to the right project without losing their place
so that: work starts and the user can carry on where they were
steps:
1. the user presses the idea key (`i` anywhere on `desk.Desk`, `ctrl-alt-m` globally, or `write idea` from `window.Commands`); a `Draft` fills the window (reopening any unsent idea draft on this device, which is also listed in `window.Places`), with `to:` addressed to the project picked from context (including transcripts/tools), `in:` quietly pre-filled with the project's mode (`one` or `new`), and `on:` quietly pre-filled with that project's runtime
2. the user writes, in the usual Helix modes (with keys isolated from the screen underneath)
   - if it is for a different project, the user edits `to:` with project completion, or `ctrl-t` asks for the project; a name with no match offers a `new project …` row, which is `Start a project`
   - if this one idea should use a fresh agent in a `one agent` project (or the standing agent in a `per idea` project), the user edits `in:` (`one` or `new`)
   - if this one idea should think on another model, the user edits `on:`, with the host's runtimes as completions; `shift-tab` moves between the header lines and the words; an untouched quiet `on:` leaves a standing agent's current runtime unchanged; a runtime the host does not know is refused in the echo and the idea stays
3. the user sends with `ctrl-enter` or `send`; the idea queues durably (recording its explicit `in:` and `on:` on the opening message so any retry after a refused start honors them), enters `window.Places` using its first line until titled, the draft clears immediately so another idea can be written even while offline, the echo names the idea and the project, and the screen before is back. If a queued offline idea is later refused by the host before a host thread exists, it surfaces locally as a `not sent` card with `write idea again` in `window.Commands`
system: picks the thread's or workset's project, or the last project the user wrote to; quietly fills `in:` and `on:` from the project (updating untouched defaults when `to:` changes); starts the work as `Project.receive an idea`.
user: the words and, if different from the defaults, the project, mode, or runtime.
cases: a multiline idea edited before sending; an unsent idea still present on this device after restart; writing a second idea while the first is still queued offline; sending a fresh-agent idea inside a `one agent` project via `in: new`; an idea on another runtime to a `one agent` project whose agent cannot change kind, refused as `set agent runtime` refuses it, with the idea kept.
captures: [empty idea with the project runtime](design/laptop-idea-empty.png), [address chooser on laptop](design/laptop-idea.png) and [on phone](design/phone-idea-runtime.png).
rejected:
- branching an idea off an existing workset in `in:` · 2026-11-26 · the user tells the existing agent to spawn a child directly rather than picking worksets in the idea header
- a one-line idea strip across every screen · 2026-11-20 · never agreed; a full window with the usual modes costs nothing when the window shows one place at a time
- a `to:` prompt before the draft opens · 2026-11-20 · the address is a line of the draft
- a draft tail at the bottom of the Desk · 2026-11-20 · the same draft is one key away, without a line of chrome on the Desk
- two send keys, one that starts an agent and one that only sends · 2026-11-20 · one wording and one key everywhere; the project decides what the message starts
- a palette command `send on…` that asks the runtime · 2026-11-21 · it hides the default and adds a step; a line shows what the idea will run on before it goes

## flow Start a project
runs: 1 a week
day: Turn ideas into work.
when: the user has an idea for work that no project holds yet
want: name the project and say once where, how, and on what model its agents work
so that: the idea starts now, and every later idea to that address starts with no question
steps:
1. in an idea, the user writes `to: tide`; a name with no match offers `new project tide`, or, with more than one host attached, a row per host such as `new project tide@lab`, and `enter` takes it; `to: tide@lab` offers only that host
2. `tide works on:` asks for the place, with the host's known repositories and directories as completions; Enter takes the place as typed unless the user chooses a completion. A URL or a `git@` address is a repository, a path is a directory on `lab`
3. `tide agents:` asks for the mode, `one` under the point and `per idea` below it
4. `tide runtime:` asks for the model and effort, pre-selecting the draft's `on:` runtime if set or `opus-high` otherwise, with the host's other runtimes below it
5. if the user started from Send, the idea sends and the work starts; if they were editing `to:` with `ctrl-t`, the new project fills that line and the draft stays open until they Send
   - from `window.Commands` anywhere, `new project` asks the same four lines
system: the host keeps the project's name, place, mode and runtime, so every device sees the same answer; clones a repository from the host's mirror; takes the host from the address when more than one is attached, so no host question.
user: the name, the place, the mode, and the runtime.
cases: the host is away; an older project with no known place says `no place set` until `set project place` is answered; an older project with a place but no runtime reads and runs `opus-high` until `set project runtime` is answered; the directory does not exist, so the thread keeps a `failed` stop as `agents.a refused start leaves a message`; a second project on the same repository; a project created from `window.Commands` with no idea yet.
captures: [project mode prompt](design/laptop-project-mode.png) and [project runtime prompt](design/laptop-project-runtime.png).
rejected:
- a menu `where this project's agents work` with two rows and a prompt behind each · 2026-11-21 · three kinds of UI for one answer, and the place stayed invisible
- an `on host:` prompt · 2026-11-21 · the address names the host
- the mode asked only for a repository · 2026-11-21 · several agents in one directory are fine too
- a second question at the second idea · 2026-11-21 · a surprise later costs more than one more line now
- no runtime question, every project on `opus-high` · 2026-11-21 · a project thread could not start on another model at all

## rule the last message stays live
when a project agent has already sent a `Message` since the user's last message in the `Thread`, then rho refuses another from that agent, tells them which message to edit, and points them at `kind="flash"` for anything that does not need the user. Other agents can each send one message. A `Flash` never occupies the live message slot. A message that was deleted or taken back no longer occupies that place. A real edit reads `edited`. An unchanged ask does not deal again after `done`; changed words do (with a recovered system stop as the one clearing exception). A reply is read by the thread's agents and settles the thread through the latest message visible when the user replied, freezing the messages already answered. A different question forks under the message that raised it. An agent-to-agent message stays in the thread without lighting the deal.
why: a thread of ten messages that each supersede the last is a feed; each agent has one place for current words, and the flash is where the rest goes.
rejected:
- a second send merges in place · 2026-11-21 · the agent has to choose to edit its existing message; another send must not silently change it
- a cold message carries a `## recap`, and rho reminds the project when it does not · 2026-11-21 · see `Message`

## rule new messages land below the draft
when new `Message` arrives, the user's own or the project's, or the newest message is edited, then it takes its place right below the `Draft` and the point does not move. With the window at the top, the user sees it at once and older messages move down. With the draft stuck to the top edge, the view stays where it is, and for a project message the stuck draft shows a quiet `↑ new` at its right end until the user scrolls back to the top. Until then the user has not seen it: a reply or `deal.Verdict.done` leaves it waiting.
rejected:
- history pushes up to keep the draft in place at the tail, `↓ new` for a project message when stuck at the bottom · 2026-11-21 · the mirror of this rule for the oldest-first order
