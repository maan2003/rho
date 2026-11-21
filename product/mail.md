# mail

The user writes a project a mail to put an idea to work. Each piece of work has one thread, with a subject and the mails so far. While the work runs, the project's latest mail says what is happening now.

## bet A mail is the medium
instead of: separate conversations beside the project, or a project feed full of individually named agents.
serves: Keep the work moving.
because: one thread keeps one piece of work together. Its subject identifies the work, its latest mail tells the user what is happening now, and the project speaks with one voice even when several agents do the work.
costs: the user opens the transcript to see which agent did what.

An agent writes to another agent by mail in a thread under the project; the user can read it one level down and never gets a `deal.Card` from it.

## bet The project asks by mail, even when an agent stops
instead of: attention coming from a mixture of mentions from named agents and notices about their state.
serves: Know what wants them, and where they were.
because: the user only needs what asks for them or returns a result, not each agent's progress. A stop is another mail to answer, not another kind of thing to watch.
costs: a failure can still ask for the user when they have nothing to add.

## journey An idea becomes a merged result
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
purpose: give each project one address to write to, and one answer, asked once, to where and how its agents work.
principle: the user sends an idea to `tide@lab`; the project starts the work at its place, in its mode, and sends its asks and results from that address.
sees: its name and host as an address, such as `tide@lab`; where it works and its mode, such as `on tide · one agent`, in the top line of the Desk narrowed to it; open, the Desk narrowed to that project.
has:
- many `Thread`
does:
- open: narrow `desk.Desk` to the project through `window.Go to something by name`
- receive an idea: start the work at the project's place in its mode. `one agent`: the project's agent reads the idea in its new thread, and starts when none runs. `per idea`: a fresh agent starts in a fresh workset, a clone of the repository or the directory itself
- works on…, from the palette on the project: ask `tide@lab works on:` and change where the next idea starts; agents already running stay where they are
- agents, from the palette on the project, as `agents: one` or `agents: per idea`: change the mode for the next idea; agents already running stay as they are
rejected:
- the place read off the project's newest agent · 2026-11-21 · nothing could show it or change it
- the kind of place deciding the mode, a repository per idea and a directory shared · 2026-11-21 · one standing agent on a repository is useful too

## concept Thread
purpose: keep one piece of work and the whole exchange about it together.
principle: the user writes to `tide@lab` about a flaky test; the project asks about the test and later sends the result in the same thread, whatever agents did the work.
sees: its subject and project; on a row, what waits, or the project's latest words when nothing waits; open, its mails in order, oldest first, and the `Draft` as the next mail at the end
has:
- many `Mail`
- many `Thread`, each a tangent under a mail
- one `Draft` at the end of the thread
does:
- open: from a `deal.Card` or by subject, open at the same place in `Thread`; a return restores the view and point as left
- reply: write in the `Draft` and send with `Mail.reply`
- fork: take a tangent into a thread under its mail; the prompt line says when to fork

## concept Mail
purpose: carry a brief, an ask, a result, or a word on the work's progress in the voice of the project, and let the user answer whoever wrote.
principle: after the user has been away, a project quotes just enough of the brief to ask a question that stands on its own; the user replies without needing to know the writer's name.
sees: the words, with the voice told by colour alone: the user's own mail in `design.marked`, the project's in reading text; a quiet name line above a mail from anyone else; a quiet tag at the end of the first line, such as `edited` or `taken back`; what the mail quotes; proof on a result; a `## recap` section before the body when this mail carries the thread's recap
does:
- show proof: a result mail designates evidence under `## proof`. The heading and evidence stay visible on warm and cold threads.
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
sees: the unsent text as the next mail at the end of the thread, after one blank line; when empty, a quiet `reply` where the words will go. When the first line is below the window, the draft becomes a `window.Bottom-stuck excerpt`; on the phone it sits above the keyboard.
does:
- edit `i`, `A`, `o` at the end, or a tap anywhere below the last mail: write in place; `escape` returns to Normal with the cursor still in the draft
- quote `enter` on a selection in one mail: insert those lines with `> ` in front, just above the last draft cursor; resume beneath the quote in Normal
- send `ctrl-enter`, or `send` on the phone bar while writing: the words become the last mail; the point rests in the empty draft even when the project answers
- undo `u`, redo `shift-u`: the editor's undo while writing; on the phone, `undo` on the bar
rejected:
- a separator line, `── reply to tide@lab · ctrl-enter sends`, between the history and the draft · 2026-11-20 · the draft is the next mail; a line of chrome on every thread costs more than it says
- a `to:` prompt or a one-line strip for a new idea · 2026-11-20 · see `Write an idea`

A thread has one draft on each device, not shared with other devices.

## screen Thread
answers: what is this piece of work waiting for from the user?
shows:
- the `Thread` subject and `Project` address in the `window.Status line`, with the card's words, `waits · 2h`, while it waits
- the `Mail` in order, one blank line between them, readable and copyable with the same motions and search as the `Draft`, but not editable
- one `## recap`, in `design.recap`, before the body of the newest stretch opener that has one
- the `Draft` as the next mail at the end or stuck to the bottom while the user reads earlier history
- a `notes.Private reply` below its mail, editable there; tangents open as their own `Thread`, not folded into the main history
states:
- empty: the opening mail, then the empty draft; a thread begins with a mail
- loading: the thread's mail is not yet available; wording not agreed
- partial: only the available mail; wording not agreed
- error: the thread is unavailable; wording not agreed
- full: whether opened by the deal or by name, the point goes into the `Draft`, where it was left if it has text; the first project mail since the user's last mail starts at the top, or the latest mail if none followed theirs. If the tail is shorter than the window, history fills the space above it. If the whole thread fits, the spare space goes above the opening mail. It never scrolls to empty space below the draft. Returning to a place restores it as left.
keys:
- `enter` with no selection on a project's mail: `Mail.look into the work`
- `ctrl-u`, `ctrl-d`: scroll history by half a window without moving the point out of the `Draft`
- `G`: go to the draft; `i`, `A` or `o`: `Draft.edit`; `ctrl-enter` or `send`: `Draft.send`
- `enter` on a selection in one mail: `Draft.quote`
- `alt-r` on a mail: `notes.Private reply.reply privately`
- `u`, `shift-u` in the draft: undo and redo the last edit
- `space`: `deal.Verdict`, anywhere in Normal mode
- `:`: `window.Palette`, which also offers the agent's own commands here, as in `agents.Steer an agent`
- `ctrl-k`, `tab`, `q`: `window.Screen`
starts: `Answer a mail`, `agents.Look into the work`, `notes.Reply privately`
rejected:
- the mail that pings at the top of the window · 2026-11-21 · open on what the project has said since the user's last word, with the next reply ready in either way of opening
- newest mail first · 2026-11-20 · the exchange reads in its own order
- the agent's transcript beside the mail in a second column · 2026-11-20 · one place in focus; the transcript is one level down
- the card alone, with the history folded · 2026-11-20 · a short history costs nothing and a long one is one scroll away
- the agent's work folded under each mail · 2026-11-20 · the transcript is `agents.Look into the work`
- a project split with the thread list at the left · 2026-11-20 · one screen at a time
- marking the mail under the point by brightness or a bar · 2026-11-20 · the cursor is enough
- a background tint behind the recap · 2026-11-20 · a slight text colour reads better

The point never rests on the blank line between two mails. A reply from a dealt card settles the card and stays on the thread; the next card waits for `deal.Card.deal`. On the phone, the whole space below the last mail is the draft's tap target.

Built renders: [asking mail on the laptop](design/laptop-card.png) and [on the phone](design/phone-card.png), [an outcome with its proof](design/laptop-proof.png), [a cold recap above its mail](design/laptop-reading.png), and [the notice in a stuck phone draft](design/phone-reading-notice.png).

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

The first two lines are the user's, in `design.marked`; the project's mail follows in reading text. The recap leads the project's mail because that mail opened the latest stretch with a recap, in `design.recap`. The recap and body are separated by one blank line. `reply` is the empty draft's placeholder, in `design.quiet`.

mock, phone:
```text
clock@lab › Flaky clock test…  waits · 3m  NOR
Fix the clock test that fails one run in five.

Could you clarify which behaviour you want?

reply

    back     menu     done     snooze     more
```

## flow Answer a mail
status: the user's Slack is in the browser, and the bot is not built.
runs: 100 a day
day: Keep the work moving.
when: a project or the Slack bot has something that asks for the user, or the user opens a result
want: read what arrived, including proof on a result, and answer in place if needed
so that: the person or project that needs the user's words gets them, and the mail no longer waits
steps:
1. the user opens the thread by the deal or by name and reads what the project wrote since their last mail
   - when there is nothing to say, the user gives `deal.Verdict.done` and stops here
2. the user writes and edits an answer in the `Draft` at the end of the thread
3. the user sends the answer with `Mail.reply`; the reply settles the card
system: opens with the point in the `Draft` and the first project mail after the user's last mail at the top, or the latest mail if none followed theirs; fills any spare space with earlier history, keeps the draft if the user leaves, and sends the reply to whoever wrote the mail.
user: the words of the answer, or done when there are none.
cases: a cold mail with its recap, answered without the history; a result with proof; a colleague reached through `slack.Bot`.
reviewed: 2026-11-20 · laptop · reviewer · 2 findings

The historical review above was of Answer an agent. Answer Slack was walked separately:
> reviewed: 2026-11-20 · laptop · reviewer · 6 findings

## flow Write an idea
runs: 12 a day
day: Turn ideas into work.
when: the user has an idea, wherever they are in the window
want: send it to the right project without losing their place
so that: work starts and the user can carry on where they were
steps:
1. the user presses the idea key, `i` on a `desk.Desk` row or the idea key elsewhere; a `Draft` fills the window, with a `to:` line already addressed to the project picked from context
2. the user writes, in the usual modes
   - if it is for a different project, the user edits the `to:` line with project completion, or `ctrl-t` asks for the project; a name with no match offers a `new project …` row, which is `Start a project`
3. the user sends with `ctrl-enter` or `send`; the echo names the idea and the project, and the screen before is back
system: picks the thread's project, or the last project the user wrote to; starts the work at that project's place, in its mode, as `Project.receive an idea`.
user: the words and, if different from the automatic choice, the project.
cases: a multiline idea edited before sending; an unsent idea still present on this device after restart; the idea comes while reading a page; the current thread belongs to a different project.
rejected:
- a one-line idea strip across every screen · 2026-11-20 · never agreed; a full window with the usual modes costs nothing when the window shows one place at a time
- a `to:` prompt before the draft opens · 2026-11-20 · the address is a line of the draft
- a draft tail at the bottom of the Desk · 2026-11-20 · the same draft is one key away, without a line of chrome on the Desk
- two send keys, one that starts an agent and one that only sends · 2026-11-20 · one wording and one key everywhere; the project decides what the mail starts
reviewed: 2026-11-20 · laptop · reviewer · 1 finding

## flow Start a project
runs: 1 a week
day: Turn ideas into work.
when: the user has an idea for work that no project holds yet
want: name the project and say once where and how its agents work
so that: the idea starts now, and every later idea to that address starts with no question
steps:
1. in an idea, the user writes `to: tide@lab`; a name with no match offers `new project tide@lab`, and `enter` takes it
2. `tide@lab works on:` asks for the place, with the host's known repositories and directories as completions; a URL or a `git@` address is a repository, a path is a directory on `lab`
3. `tide@lab agents:` asks for the mode, `one` under the point and `per idea` below it
4. the idea sends and the work starts, as in `Write an idea`
   - from the Desk palette, `new project` asks the same three lines and opens the empty project
system: the host keeps the project's name, place and mode, so every device sees the same answer; clones a repository from the host's mirror; names the project after the host in the address, so no host question.
user: the name, the place, and the mode.
cases: the host is offline; an older project with no known place says `no place set` until `works on…` is answered; the directory does not exist, so the thread keeps a `failed` stop as `agents.a refused start leaves mail`; a second project on the same repository; a project made from the Desk with no idea yet.
mock:
```
to: tide@lab                                        new project tide@lab
tide@lab works on: git@github.com:maan2003/tide     repository
tide@lab agents: one                                every idea goes to the same agent
                 per idea                           each idea gets a fresh agent in its own workset
```
rejected:
- a menu `where this project's agents work` with two rows and a prompt behind each · 2026-11-21 · three kinds of UI for one answer, and the place stayed invisible
- an `on host:` prompt · 2026-11-21 · the address names the host
- the mode asked only for a repository · 2026-11-21 · several agents in one directory are fine too
- a second question at the second idea · 2026-11-21 · a surprise later costs more than one more line now

The historical review above was of Start a thread. Starting an agent from a mail was walked separately:
> reviewed: 2026-11-20 · laptop · reviewer · 4 findings

## rule the last mail stays live
when a project agent has already sent a `Mail` since the user's last mail in the `Thread`, then rho refuses another from that agent and tells them which mail to edit, unless the agent explicitly opts in with `replace_last=True`. That option edits its live mail, preserving its ping unless set, and returns the same id. If it has no live mail in the thread, it sends and returns a new one. Other agents can each send one mail. A mail that was deleted or taken back no longer occupies that place. A real edit reads `edited`, and the user's reply goes to the writer of the words shown. An unchanged ask does not deal again after `done`; changed words do. Once the user replies, the ask is frozen. A different question forks under the mail that raised it. Mail to the user stays in the thread but does not wake its agents.
why: a thread of ten mails that each supersede the last is a feed; each agent has one place for current words.
rejected:
- a second send merges in place · 2026-11-21 · the agent has to choose to edit its existing mail; another send must not silently change it

## rule a cold mail carries a recap
when project `Mail` arrives or is edited in a `Thread` more than ten minutes after the user's last mail or verdict there (or with neither), then it opens a cold stretch. The newest stretch opener with a `## recap` shows that section in `design.recap` before its body. All other recap sections are hidden. An answer within ten minutes does not move or hide the chosen recap.
why: after a long gap the user can answer without reading the history, and the same context stays visible while they work.
rejected:
- cold from when the user last had the thread open · 2026-11-21 · reading on either device must not change the recap; the user's actual words and decisions mark the return

rho tells the project once when the thread goes cold. The project's next pinging mail, new or edited, carries a recap that lets the user answer without the history. When ten minutes pass with a pinging mail that has no recap, rho reminds the project once.

## rule new mail follows the draft at the tail
when new project `Mail` arrives or the latest mail is edited, then history moves as needed to keep the `Draft` in place if it is inline, without moving the point. If it is stuck, history stays where it was and the draft shows a quiet `↓ new mail` at its right end until the user scrolls down to the tail.
