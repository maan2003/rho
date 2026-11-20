# mail

The user writes a project a mail to put an idea to work. Each piece of work has one thread, with a subject and the mails so far. While the work runs, the project's latest mail says what is happening now.

## bet A mail is the medium
instead of: separate conversations beside the project, or a project feed full of individually named agents.
serves: Keep the work moving.
because: one thread keeps one piece of work together. Its subject identifies the work, its latest mail tells the user what is happening now, and the project speaks with one voice even when several agents do the work.
costs: the user opens the transcript to see which agent did what.

An agent writes to another agent by mail in a thread under the project; the user can read it one level down and never gets a `deal.Card` from it.

## bet The project asks by mail, even when an agent stops
status: when an agent cannot launch, its stop mail explains how to get the work going. Further replies are saved without retrying the failed launch.
instead of: attention coming from a mixture of mentions from named agents and notices about their state.
serves: Know what wants them, and where they were.
because: the user only needs what asks for them or returns a result, not each agent's progress. A stop is another mail to answer, not another kind of thing to watch.
costs: a failure can still ask for the user when they have nothing to add.

## journey An idea becomes a merged result
status: a result carries no designated proof yet.
goal: the user turns an idea into merged work.
starts when: the user has an idea.
steps:
1. the user writes to the project from where they are, `Write an idea`
2. the user sees the work start, `desk.Glance at what moves`
3. the user opens the project's ask when it needs something, `deal.Clear what waits`
4. the user answers the ask in its thread, `Answer a mail`
5. the user opens the result, `deal.Clear what waits`
6. the user looks into the work if they need to see how it was done, `agents.Look into the work`
7. the user reads the proof on the result mail, `Answer a mail`
8. when there is nothing to add, the user says done, `deal.Clear what waits`
done when: the work is merged and nothing waits.
day: Turn ideas into work.

## concept Project
status: the project's own screen still lists its threads, with agent names and counts on its rows.
purpose: give each project one address to write to, without asking the user where its agents should work each time.
principle: the user sends an idea to `tide@lab`; the project starts the work where its agents work and sends its asks and results from that address.
sees: its name and host as an address, such as `tide@lab`; open, the threads of that project by subject.
has:
- many `Thread`
does:
- open: show the project's threads by name through `window.Go to something by name`
- receive an idea: use the place already remembered for the project's agents

## concept Thread
purpose: keep one piece of work and the whole exchange about it together.
principle: the user writes to `tide@lab` about a flaky test; the project asks about the test and later sends the result in the same thread, whatever agents did the work.
sees: its subject and project; on a row, what waits, or the project's latest words when nothing waits; open, its mails in order, oldest first, and the `Draft` as the next mail at the end
has:
- many `Mail`
- many `Thread`, each a tangent under a mail
- one `Draft` at the end of the thread
does:
- open: from a `deal.Card`, land on the mail that pings; otherwise open the thread by subject
- reply: write in the `Draft` and send with `Mail.reply`
- fork: take a tangent into a thread under its mail; the prompt line says when to fork

## concept Mail
status: a result does not yet carry designated proof. The built thread still shows a header with the writer and the time above each mail.
purpose: carry a brief, an ask, a result, or a word on the work's progress in the voice of the project, and let the user answer whoever wrote.
principle: after the user has been away, a project quotes just enough of the brief to ask a question that stands on its own; the user replies without needing to know the writer's name.
sees: the words, with the voice told by colour alone: the user's own mail in `design.marked`, the project's in reading text; a quiet name line above a mail from anyone else; a quiet tag at the end of the first line, such as `edited` or `taken back`; what the mail quotes; proof on a result; a `## recap` section while the thread is cold
does:
- ping: a mail either pings the user or it does not. A mail that pings is dealt as a `deal.Card` and reads `waits · 2h` on its row. A mail that does not ping stays in the thread with no card; it is how the project says what it is doing while it works, edited in place as the work moves
- look into the work `enter` on the mail: open the `agents.Agent` that wrote the mail at the step that produced its current words; a failed stop opens its error
- reply: send the user's words to whoever wrote the mail
- edit or delete: agents may change or take back their own mails, except that mail the user has answered stays as it was. An edit that turns the ping on deals the mail; an edit of a mail that already pings keeps its card as it is
- reply privately `alt-r`: write a remark directly beneath this mail, as in `notes.Reply privately`
rejected:
- kinds of mail, `ask`, `result`, and `fyi` · 2026-11-20 · one yes or no, whether the mail pings, does the job; a result waits like an ask
- a header with the writer and the time above each mail · 2026-11-20 · colour tells the voice, and the top line says how long the thread has waited
- a status line kept apart from the mails, replaced rather than kept · 2026-11-20 · a mail without a ping, edited in place, does the same job and stays as history

Agents can search mail. Mail written while the host is away waits on the device and goes out when the host is back.

## concept Draft
purpose: keep what the user wrote in a thread and has not sent, so leaving the thread loses nothing.
principle: the user is halfway through an answer when a card that matters comes in; they deal it and answer it; later, on the same device, they open the first thread, and the half-written answer is still at the end of the thread, even after a restart.
sees: the unsent text as the next mail at the end of the thread, after one blank line; when empty, a quiet `reply` where the words will go.
does:
- edit `i`, `A`, `o` at the end, or a tap anywhere below the last mail: write in place; `escape` returns to Normal with the cursor still in the draft
- quote `enter` on a selection in one mail: insert those lines with `> ` in front, just above the last draft cursor; resume beneath the quote in Normal
- send `ctrl-enter`, or `send` on the phone bar while writing: the words become the last mail; the point rests in the empty draft and moves to the project's next pinging mail when it arrives
- undo `u`, redo `shift-u`: the editor's undo while writing; on the phone, `undo` on the bar
rejected:
- a separator line, `── reply to tide@lab · ctrl-enter sends`, between the history and the draft · 2026-11-20 · the draft is the next mail; a line of chrome on every thread costs more than it says
- a `to:` prompt or a one-line strip for a new idea · 2026-11-20 · see `Write an idea`

A thread has one draft on each device, not shared with other devices.

## screen Thread
status: the built screen shows a header above each mail, a status line, and a separator before the draft, and opens scrolled to the end. The deal lands on the first waiting mail after the latest reply when there is one. An older unanswered mail still waits. An attempt to edit mail echoes `mail cannot be edited · write below the line`.
answers: what is this piece of work waiting for from the user?
shows:
- the `Thread` subject and `Project` address in the `window.Status line`, with the card's words, `waits · 2h`, while it waits
- the `Mail` in order, one blank line between them, readable and copyable with the same motions and search as the `Draft`, but not editable; the mail that pings is under the point when a `deal.Card` opens
- a `## recap` section of a mail only while the thread is cold, in `design.recap`; warm, it is not on screen at all
- the `Draft` as the next mail at the end
- a `notes.Private reply` below its mail, editable there; tangents open as their own `Thread`, not folded into the main history
states:
- empty: the opening mail, then the empty draft; a thread begins with a mail
- loading: the thread's mail is not yet available; wording not agreed
- partial: only the available mail; wording not agreed
- error: the thread is unavailable; wording not agreed
- full: the whole thread from the top when it fits the window; otherwise the mail that pings at the top of the window, with history above it and the draft below, all in the same buffer
keys:
- `enter` with no selection on a project's mail: `Mail.look into the work`
- `G`: go to the draft; `i`, `A` or `o`: `Draft.edit`; `ctrl-enter` or `send`: `Draft.send`
- `enter` on a selection in one mail: `Draft.quote`
- `alt-r` on a mail: `notes.Private reply.reply privately`
- `u`, `shift-u` in the draft: undo and redo the last edit
- `space`: `deal.Verdict`, anywhere in Normal mode
- `:`: `window.Palette`, which also offers the agent's own commands here, as in `agents.Steer an agent`
- `ctrl-k`, `tab`, `q`: `window.Screen`
starts: `Answer a mail`, `agents.Look into the work`, `notes.Reply privately`
rejected:
- newest mail first · 2026-11-20 · the exchange reads in its own order
- the agent's transcript beside the mail in a second column · 2026-11-20 · one place in focus; the transcript is one level down
- the card alone, with the history folded · 2026-11-20 · a short history costs nothing and a long one is one scroll away
- the agent's work folded under each mail · 2026-11-20 · the transcript is `agents.Look into the work`
- a project split with the thread list at the left · 2026-11-20 · one screen at a time
- marking the mail under the point by brightness or a bar · 2026-11-20 · the cursor is enough
- a background tint behind the recap · 2026-11-20 · a slight text colour reads better

The point never rests on the blank line between two mails. A reply from a dealt card settles the card and stays on the thread; the next card waits for `deal.Card.deal`. On the phone, the whole space below the last mail is the draft's tap target.

Built renders: [asking mail on the laptop](design/laptop-card.png) and [on the phone](design/phone-card.png), both of the built screen before this design.

mock:
```text
clock@lab › Flaky clock test                                   waits · 2h   NOR

Fix the clock test that fails one run in five.
It started after the DST change last week.

## recap
The mock clock is shared between tests. One run in five lands on a DST boundary.
Is a separate clock per test enough, or do you want the suite isolated too?

reply
```

The first two lines are the user's, in `design.marked`; the project's mail follows in reading text. The recap is on screen because the thread is cold, in `design.recap`. `reply` is the empty draft's placeholder, in `design.quiet`.

mock, phone:
```text
clock@lab › Flaky clock test…  waits · 3m  NOR
Fix the clock test that fails one run in five.

Could you clarify which behaviour you want?

reply

    back     menu     done     snooze     more
```

## flow Answer a mail
status: a result has no designated proof. The user's Slack is in the browser, and the bot is not built.
runs: 100 a day
day: Keep the work moving.
when: a project or the Slack bot has something that asks for the user, or the user opens a result
want: read what arrived, including proof on a result, and answer in place if needed
so that: the person or project that needs the user's words gets them, and the mail no longer waits
steps:
1. the user reads the mail the deal opened
   - when there is nothing to say, the user gives `deal.Verdict.done` and stops here
2. the user writes and edits an answer in the `Draft` at the end of the thread
3. the user sends the answer with `Mail.reply`; the reply settles the card
system: opens the asking mail, keeps the `Draft` if the user leaves, and sends the reply to whoever wrote the mail.
user: the words of the answer, or done when there are none.
cases: a cold mail with its recap, answered without the history; a result with proof; a colleague reached through `slack.Bot`.
reviewed: 2026-11-20 · laptop · reviewer · 2 findings

The historical review above was of Answer an agent. Answer Slack was walked separately:
> reviewed: 2026-11-20 · laptop · reviewer · 6 findings

## flow Write an idea
status: the built idea is a one-line strip and a draft tail on the Desk; the full-window draft with a `to:` line is not built.
runs: 12 a day
day: Turn ideas into work.
when: the user has an idea, wherever they are in the window
want: send it to the right project without losing their place
so that: work starts and the user can carry on where they were
steps:
1. the user presses the idea key, `i` on a `desk.Desk` row or the idea key elsewhere; a `Draft` fills the window, with a `to:` line already addressed to the project picked from context
2. the user writes, in the usual modes
   - if it is for a different project, the user edits the `to:` line with project completion, or `ctrl-t` asks for the project; `to:` never creates a project, and a name with no match offers a `new project …` row that makes the project first
3. the user sends with `ctrl-enter` or `send`; the echo names the idea and the project, and the screen before is back
system: picks the thread's project, or the last project the user wrote to; starts the work where that project's agents work.
user: the words and, if different from the automatic choice, the project.
cases: a multiline idea edited before sending; an unsent idea still present on this device after restart; the idea comes while reading a page; the current thread belongs to a different project.
rejected:
- a one-line idea strip across every screen · 2026-11-20 · never agreed; a full window with the usual modes costs nothing when the window shows one place at a time
- a `to:` prompt before the draft opens · 2026-11-20 · the address is a line of the draft
- a draft tail at the bottom of the Desk · 2026-11-20 · the same draft is one key away, without a line of chrome on the Desk
- two send keys, one that starts an agent and one that only sends · 2026-11-20 · one wording and one key everywhere; the project decides what the mail starts
reviewed: 2026-11-20 · laptop · reviewer · 1 finding

The historical review above was of Start a thread. Starting an agent from a mail was walked separately:
> reviewed: 2026-11-20 · laptop · reviewer · 4 findings

## rule the last mail stays live
when the project's current ask in a `Thread` has no user reply, then an update replaces its words in place, even if another agent writes them. It reads `edited` and the user's reply goes to the writer of the words shown. An unchanged ask does not deal again after `done`; changed words do. Once the user replies, the ask is frozen. A different question forks under the mail that raised it. Mail to the user stays in the thread but does not wake its agents.
why: a thread of ten mails that each supersede the last is a feed; one live mail is a state the user can read in one look.

## rule a cold mail carries a recap
when more than ten minutes have passed since the user had a `Thread` open, then it is cold. rho tells the `Project` once. The project's next pinging `Mail`, new or edited, carries a `## recap` section: enough that the user can answer without reading the history. When ten minutes pass with a pinging mail that has no recap, rho reminds the project once. Warm, the recap is not on screen; cold, it shows in `design.recap`. Warm mail is short.
why: the user comes back to a thread with no memory of it; the recap is for them, and the history stays whole for the times they want it.
