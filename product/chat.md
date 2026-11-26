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
because: the user only needs what asks for them or returns a result, not each agent's progress. A stop is another message to answer, not another kind of thing to watch.
costs: a failure can still ask for the user when they have nothing to add.

## journey An idea becomes a merged result
goal: the user turns an idea into merged work.
starts when: the user has an idea.
steps:
1. the user writes to the project from where they are, `Write an idea`
2. the user sees the work start, in the thread row's `working` word in `window.Palette`, or by the project's next message
3. the user opens the project's ask when it needs something, `deal.Clear what waits`
4. the user answers the ask in its thread, `Answer a message`
5. the user opens the result, `deal.Clear what waits`
6. the user looks into the work if they need to see how it was done, `agents.Look into the work`
7. the user reads the proof on the result message, `Answer a message`
8. when there is nothing to add, the user says done, `deal.Clear what waits`
done when: the work is merged and nothing waits.
day: Turn ideas into work.

## concept Project
purpose: give each project one address to write to, and one answer, asked once, to where, how, and on what model its agents work.
principle: the user sends an idea to `tide` (or `tide@lab` when two attached hosts share the name `tide`); the project starts the work at its place, in its mode, on its runtime, and sends its asks and results from that address.
sees: its name (such as `tide`, adding `@host` only when two attached hosts have a project of the same name); where it works, its mode, and its runtime when configuring it or filtering by it in `window.Palette`. A project has no screen of its own.
has:
- many `Thread`
does:
- filter in `window.Palette`: choosing a project in `window.Palette` filters the palette to that project's threads
- receive an idea: start the work at the project's place in its mode, on the project's runtime for a new agent unless the idea's `on:` line named another. `one agent`: the project's agent reads the idea in its new thread and keeps its runtime; it starts when none runs; an idea on another runtime switches it first, as `agents.Agent.set runtime` does. `per idea`: a fresh agent starts in a fresh workset, a clone of the repository or the directory itself
- project › works on…, from `window.Palette` anywhere: uses the current thread's project when unambiguous, or asks `project:` first; then asks `<project> works on:` and changes where the next idea starts; agents already running stay where they are
- project › agents…, from `window.Palette` anywhere: uses the current thread's project or asks `project:` first; changes between `one` and `per idea` for the next idea; agents already running stay as they are
- project › runtime…, from `window.Palette` anywhere: uses the current thread's project or asks `project:` first; asks `<project> runtime (sol-high):` with the host's runtimes as answers; the next new agent starts on it; agents already running stay as they are
rejected:
- a narrowed `desk.Desk` screen per project · 2026-11-26 · Desk is one global glance of what waits; project filtering belongs in `window.Palette` and project commands work globally from `window.Palette`
- the place read off the project's newest agent · 2026-11-21 · nothing could show it or change it
- the kind of place deciding the mode, a repository per idea and a directory shared · 2026-11-21 · one standing agent on a repository is useful too
- every project agent on `opus-high`, with the model chosen only through `start agent` · 2026-11-21 · that path asks the place again and never makes a project thread, so no project thread could start on another model

## concept Thread
purpose: keep one piece of work and the whole exchange about it together.
principle: the user writes to `tide` about a flaky test; the project asks about the test and later sends the result in the same thread, whatever agents did the work.
sees: its subject and project; on a row, what waits, or the project's latest words when nothing waits; open, the `Draft` first, then its messages newest first, oldest last
has:
- many `Message`
- many `Thread`, each a tangent under a message
- one `Draft` at the top of the thread
does:
- open: from a `deal.Card` or by subject, open at the top of `Thread` (`Draft` and newest message); returning via `window.Screen.back` restores the view and point as left
- reply: write in the `Draft` and send with `Message.reply`
- rename, from `window.Palette` as `name this thread`: change the thread's subject
- fork: take a tangent into a thread under its message; the prompt line says when to fork

## concept Message
purpose: carry an idea, an ask, or a result in the voice of the project, keep it as the record of the work, and let the user answer in the thread.
principle: the project asks a question; the user reads it on top of the thread with the project's earlier words one glance below, and replies without needing to know the writer's name.
sees: the words, with the voice told by colour alone: the user's own message in `design.marked`, the project's in reading text; a quiet name line above a message from anyone else; a quiet tag at the end of the first line, such as `edited` or `taken back`; what the message quotes
does:
- ping: every message from the project pings the user. It is dealt as a `deal.Card` and reads `waits · 2h` on its row. An agent names the kind of everything it sends, `kind="message"` or `kind="flash"`, with no default, so it decides each time whether the user is needed
- look into the work `enter` on the message: open the `agents.Workset` transcript at the step that produced its current words; a failed stop opens its error
- reply: the user's words go into the thread, and every agent in it reads them; there is no addressee. With two asks open, the user names the agent in words or deals the next card
- edit or delete: agents may change or take back their own messages, except that a message the user has answered stays as it was. A real edit reads `edited` and keeps the card as it is
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

Agents can search messages. A message written while the host is away waits on the device and goes out when the host is back. A submitted idea says `queued for` its project and can be found by name in `window.Palette` until the host's thread takes its place.

## concept Flash
purpose: let an agent say what it is doing or about to do, for the user who happens to be there, without a card and without a trace.
principle: an agent writes `I plan to drop the cache layer, say no in 20 min` and starts; the user who is in the thread replies `no`, and the agent changes course; the user who is away never sees it, and the thread does not grow.
sees: the words in `design.quiet` under the draft, newest first among the thread's messages, with the age at the end of the first line, `12m`; one open flash per agent, with its earlier fresh flashes folded under it as `+2 earlier`; on the project's row, the newest flash's first line as the project's latest words
does:
- post, by an agent with `kind="flash"`: show it in the thread for twenty minutes from its first post, longer while it is the thread's newest item from either side; it goes when the clock has run and something newer exists, a message or a flash from anyone; then it is gone from every device, not kept as history
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
purpose: keep what the user wrote in a thread and has not sent, so leaving the thread loses nothing.
principle: the user is halfway through an answer when a card that matters comes in; they deal it and answer it; later, on the same device, they open the first thread, and the half-written answer is still at the top of the thread, even after a restart.
sees: the unsent text in `design.marked`, the user's own colour, with a thin bar of that colour in the left gutter, as the first lines of the thread, one blank line above the newest message; when empty, a quiet `reply` right at the cursor where the words will go. When its last line is above the window, the draft becomes a `window.Edge-stuck excerpt` at the top edge, on a lighter surface; on the phone it is in the half above the keyboard.
does:
- edit `i`, `A`, `o`, or a tap on the draft: write in place; `escape` returns to Normal with the cursor still in the draft
- quote `enter` on a selection in one message: insert those lines with `> ` in front, just above the last draft cursor; resume beneath the quote in Normal
- send `ctrl-enter`, or `send` on the phone bar while writing: the words become the newest message, right below the empty draft; the point rests in the draft and nothing scrolls, then or when the project answers
- undo `u`, redo `shift-u`: the editor's undo while writing; on the phone, `undo` on the bar
rejected:
- move the point to the project's next pinging message after sending · 2026-11-21 · the user stays in the draft; the history moves as needed
- a separator line, `── reply to tide@lab · ctrl-enter sends`, between the history and the draft · 2026-11-20 · the draft is the next message; a line of chrome on every thread costs more than it says
- a `to:` prompt or a one-line strip for a new idea · 2026-11-20 · see `Write an idea`
- the draft as the last message at the end of the thread, stuck to the bottom while reading · 2026-11-21 · with the newest message first the draft leads, and no open or fill rule is needed

A thread has one draft on each device, not shared with other devices.

## screen Thread
answers: what is this piece of work waiting for from the user?
shows:
- the `Thread` subject and `Project` address in the `window.Status line`, with the card's words (`waits · 2h`) while it waits, or `working` while an agent runs and nothing waits
- the `Draft` first, at the top of the document, or stuck to the top edge while the user reads history below
- the `Message` newest first, one blank line between them, readable and copyable with the same motions and search as the `Draft`, but not editable; the user's own last message, in `design.marked`, is the line between what is new since they wrote and the history below
- the fresh `Flash` of each agent in `design.quiet` with its age, in its place among the messages, with that agent's earlier fresh flashes folded under it
- a `notes.Private reply` below its message, editable there; tangents open as their own `Thread`, not folded into the main history
states:
- empty: the empty draft, then the opening message; a thread begins with a message
- loading: the thread's messages are not yet available; wording not agreed
- partial: only the available messages; wording not agreed
- error: the thread is unavailable; wording not agreed
- full: whether opened by the deal or by name, the window starts at the top of the document: the `Draft`, with the point in it where it was left, then the newest message. Older message is one scroll down. Returning via `window.Screen.back` restores it as left.
keys:
- `enter` with no selection on a project's message: `Message.look into the work`
- `ctrl-u`, `ctrl-d`: scroll history by half a window without moving the point out of the `Draft`
- `gg`: go to the draft; `G`: go to the oldest message; `i`, `A` or `o`: `Draft.edit`; `ctrl-enter` or `send`: `Draft.send`
- `enter` on a selection in one message: `Draft.quote`
- `alt-r` on a message: `notes.Private reply.reply privately`
- `u`, `shift-u` in the draft: undo and redo the last edit
- `space`: `deal.Verdict`, anywhere in Normal mode
- `:`: `window.Palette`
- `ctrl-k`, `tab`, `q`: `window.Screen`
starts: `Answer a message`, `agents.Look into the work`, `notes.Reply privately`
captures: [asking message on the laptop](design/laptop-card.png) and [on the phone](design/phone-card.png), [an outcome with its proof](design/laptop-proof.png), [an older thread with history](design/laptop-reading.png), and [the notice in a stuck phone draft](design/phone-reading-notice.png).
rejected:
- the message that pings at the top of the window · 2026-11-21 · open on what the project has said since the user's last word, with the next reply ready in either way of opening
- oldest message first, the draft at the end · 2026-11-21 · the first project message since the user's last word could not be at the top and the window full at once; with newest first and the draft leading, open is always the top and the user's own last message separates new from history
- the agent's transcript beside the message in a second column · 2026-11-20 · one place in focus; the transcript is a flat peer
- the card alone, with the history folded · 2026-11-20 · a short history costs nothing and a long one is one scroll away
- the agent's work folded under each message · 2026-11-20 · the transcript is `agents.Look into the work`
- a project split with the thread list at the left · 2026-11-20 · one screen at a time
- marking the message under the point by brightness or a bar · 2026-11-20 · the cursor is enough

The point never rests on the blank line between two messages. A reply from a waiting thread settles the visible waiting card whether the thread was opened by the deal or by name, and stays on the thread; the next card waits for `deal.Card.deal`.

## flow Answer a message
status: the user's Slack is in the browser, and the bot is not built.
runs: 100 a day
day: Keep the work moving.
when: a project or the Slack bot has something that asks for the user, or the user opens a result
want: read what arrived, including proof on a result, and answer in place if needed
so that: the person or project that needs the user's words gets them, and the message no longer waits
steps:
1. the user opens the thread by the deal or by name and reads what the project wrote since their last message
   - when there is nothing to say, the user gives `deal.Verdict.done` and stops here
2. the user writes and edits an answer in the `Draft` at the top of the thread
3. the user sends the answer with `Message.reply`; the reply settles the waiting card on this device even while it waits to be delivered. If the host refuses it, the card returns and the unsent answer stays in the thread.
system: opens at the top with the point in the `Draft` and message below it, newest first. The user's last message separates what arrived since they wrote from earlier history. The thread keeps the draft if the user leaves. The reply is a message in the thread that every agent in it reads.
user: the words of the answer, or done when there are none.
cases: a question answered after a day away, with the project's earlier words one glance below; a result with proof; a colleague reached through `slack.Bot`; replying after returning via `window.Screen.back` or `window.Palette`.

## flow Write an idea
runs: 12 a day
day: Turn ideas into work.
when: the user has an idea, wherever they are in the window
want: send it to the right project without losing their place
so that: work starts and the user can carry on where they were
steps:
1. the user presses the idea key (`i` on a `desk.Desk` row, `ctrl-alt-m` globally, or `write an idea` from `window.Palette`); a `Draft` fills the window, with a `to:` line already addressed to the project picked from context, and an `on:` line with that project's runtime
2. the user writes, in the usual Helix modes (with keys isolated from the screen underneath)
   - if it is for a different project, the user edits the `to:` line with project completion, or `ctrl-t` asks for the project; a name with no match offers a `new project …` row, which is `Start a project`
   - if this one idea should think on another model, the user edits the `on:` line, with the host's runtimes as completions; `shift-tab` moves between the lines and the words; a runtime the host does not know is refused in the echo and the idea stays
3. the user sends with `ctrl-enter` or `send`; the idea queues durably, the draft clears immediately so another idea can be written even while offline, the echo names the idea and the project, and the screen before is back
system: picks the thread's project, or the last project the user wrote to; fills `on:` with that project's runtime, again when `to:` changes and the user has not touched `on:`; starts the work at that project's place, in its mode, on the `on:` runtime, as `Project.receive an idea`.
user: the words and, if different from the automatic choice, the project and the runtime.
cases: a multiline idea edited before sending; an unsent idea still present on this device after restart; writing a second idea while the first is still queued offline; the idea comes while reading a page; the current thread belongs to a different project; an idea on another runtime to a `one agent` project whose agent cannot change kind, refused as `set runtime` refuses it, with the idea kept.
captures: [empty idea with the project runtime](design/laptop-idea-empty.png), [address chooser on laptop](design/laptop-idea.png) and [on phone](design/phone-idea-runtime.png).
rejected:
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
4. `tide runtime:` asks for the model and effort, `opus-high` under the point and the host's other runtimes below it
5. if the user started from Send, the idea sends and the work starts; if they were editing `to:` with `ctrl-t`, the new project fills that line and the draft stays open until they Send
   - from `window.Palette` anywhere, `project › new` asks the same four lines
system: the host keeps the project's name, place, mode and runtime, so every device sees the same answer; clones a repository from the host's mirror; takes the host from the address when more than one is attached, so no host question.
user: the name, the place, the mode, and the runtime.
cases: the host is offline; an older project with no known place says `no place set` until `project › works on…` is answered; an older project with a place but no runtime reads and runs `opus-high` until `project › runtime…` is answered; the directory does not exist, so the thread keeps a `failed` stop as `agents.a refused start leaves a message`; a second project on the same repository; a project created from `window.Palette` with no idea yet.
captures: [project mode prompt](design/laptop-project-mode.png) and [project runtime prompt](design/laptop-project-runtime.png).
rejected:
- a menu `where this project's agents work` with two rows and a prompt behind each · 2026-11-21 · three kinds of UI for one answer, and the place stayed invisible
- an `on host:` prompt · 2026-11-21 · the address names the host
- the mode asked only for a repository · 2026-11-21 · several agents in one directory are fine too
- a second question at the second idea · 2026-11-21 · a surprise later costs more than one more line now
- no runtime question, every project on `opus-high` · 2026-11-21 · a project thread could not start on another model at all

## rule the last message stays live
when a project agent has already sent a `Message` since the user's last message in the `Thread`, then rho refuses another from that agent, tells them which message to edit, and points them at `kind="flash"` for anything that does not need the user. Other agents can each send one message. A `Flash` never occupies the live message slot. A message that was deleted or taken back no longer occupies that place. A real edit reads `edited`. An unchanged ask does not deal again after `done`; changed words do. A reply is read by the thread's agents and settles the thread through the latest message visible when the user replied, freezing the messages already answered. A different question forks under the message that raised it. A message to the user stays in the thread but does not wake its agents.
why: a thread of ten messages that each supersede the last is a feed; each agent has one place for current words, and the flash is where the rest goes.
rejected:
- a second send merges in place · 2026-11-21 · the agent has to choose to edit its existing message; another send must not silently change it
- a cold message carries a `## recap`, and rho reminds the project when it does not · 2026-11-21 · see `Message`

## rule new messages land below the draft
when new `Message` arrives, the user's own or the project's, or the newest message is edited, then it takes its place right below the `Draft` and the point does not move. With the window at the top, the user sees it at once and older messages move down. With the draft stuck to the top edge, the view stays where it is, and for a project message the stuck draft shows a quiet `↑ new` at its right end until the user scrolls back to the top.
rejected:
- history pushes up to keep the draft in place at the tail, `↓ new` for a project message when stuck at the bottom · 2026-11-21 · the mirror of this rule for the oldest-first order
