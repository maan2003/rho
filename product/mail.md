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
2. the user sees the work start, in the thread row's `working` word in `window.Palette`, or by the project's next mail
3. the user opens the project's ask when it needs something, `deal.Clear what waits`
4. the user answers the ask in its thread, `Answer a mail`
5. the user opens the result, `deal.Clear what waits`
6. the user looks into the work if they need to see how it was done, `agents.Look into the work`
7. the user reads the proof on the result mail, `Answer a mail`
8. when there is nothing to add, the user says done, `deal.Clear what waits`
done when: the work is merged and nothing waits.
day: Turn ideas into work.

## concept Project
purpose: give each project one address to write to, and one answer, asked once, to where, how, and on what model its agents work.
principle: the user sends an idea to `tide@lab`; the project starts the work at its place, in its mode, on its runtime, and sends its asks and results from that address.
sees: its name and host as an address, such as `tide@lab`; where it works, its mode and its runtime, such as `on tide · one agent · sol-high`, in the top line of the Desk narrowed to it; open, the Desk narrowed to that project.
has:
- many `Thread`
does:
- open: narrow `desk.Desk` to the project through `window.Go to something by name`
- receive an idea: start the work at the project's place in its mode, on the project's runtime for a new agent unless the idea's `on:` line named another. `one agent`: the project's agent reads the idea in its new thread and keeps its runtime; it starts when none runs; an idea on another runtime switches it first, as `agents.Agent.set runtime` does. `per idea`: a fresh agent starts in a fresh workset, a clone of the repository or the directory itself
- works on…, from the palette on the project: ask `tide@lab works on:` and change where the next idea starts; agents already running stay where they are
- agents, from the palette on the project, as `agents: one` or `agents: per idea`: change the mode for the next idea; agents already running stay as they are
- runtime…, from the palette on the project: ask `tide@lab runtime (sol-high):` with the host's runtimes as answers; the next new agent starts on it; agents already running stay as they are
rejected:
- the place read off the project's newest agent · 2026-11-21 · nothing could show it or change it
- the kind of place deciding the mode, a repository per idea and a directory shared · 2026-11-21 · one standing agent on a repository is useful too
- every project agent on `opus-high`, with the model chosen only through `start agent` · 2026-11-21 · that path asks the place again and never makes a project thread, so no project thread could start on another model

## concept Thread
purpose: keep one piece of work and the whole exchange about it together.
principle: the user writes to `tide@lab` about a flaky test; the project asks about the test and later sends the result in the same thread, whatever agents did the work.
sees: its subject and project; on a row, what waits, or the project's latest words when nothing waits; open, the `Draft` first, then its mails newest first, oldest last
has:
- many `Mail`
- many `Thread`, each a tangent under a mail
- one `Draft` at the top of the thread
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
principle: the user is halfway through an answer when a card that matters comes in; they deal it and answer it; later, on the same device, they open the first thread, and the half-written answer is still at the top of the thread, even after a restart.
sees: the unsent text in `design.marked`, the user's own colour, with a thin bar of that colour in the left gutter, as the first lines of the thread, one blank line above the newest mail; when empty, a quiet `reply` right at the cursor where the words will go. When the draft answers a mail that is not the newest, a quiet line right under the draft names it, `answers eng-ka35 · 12:40 · Which clock needs fixing?`, cut at the window; when it answers the newest mail, nothing is said. When its last line is above the window, the draft becomes a `window.Edge-stuck excerpt` at the top edge, on a lighter surface; on the phone it is in the half above the keyboard.
does:
- edit `i`, `A`, `o`, or a tap on the draft: write in place; `escape` returns to Normal with the cursor still in the draft
- quote `enter` on a selection in one mail: insert those lines with `> ` in front, just above the last draft cursor; resume beneath the quote in Normal
- send `ctrl-enter`, or `send` on the phone bar while writing: the words become the newest mail, right below the empty draft; the point rests in the draft and nothing scrolls, then or when the project answers
- undo `u`, redo `shift-u`: the editor's undo while writing; on the phone, `undo` on the bar
- answer this mail, from the palette on a mail, or `r` on it in Normal mode: make the draft answer that mail; the quiet line changes or goes away
rejected:
- move the point to the project's next pinging mail after sending · 2026-11-21 · the user stays in the draft; the history moves as needed
- a separator line, `── reply to tide@lab · ctrl-enter sends`, between the history and the draft · 2026-11-20 · the draft is the next mail; a line of chrome on every thread costs more than it says
- a `to:` prompt or a one-line strip for a new idea · 2026-11-20 · see `Write an idea`

A thread has one draft on each device, not shared with other devices.

rejected:
- the draft as the last mail at the end of the thread, stuck to the bottom while reading · 2026-11-21 · with the newest mail first the draft leads, and no open or fill rule is needed

## screen Thread
answers: what is this piece of work waiting for from the user?
shows:
- the `Thread` subject and `Project` address in the `window.Status line`, with the card's words, `waits · 2h`, while it waits
- the `Draft` first, at the top of the document, or stuck to the top edge while the user reads history below
- the `Mail` newest first, one blank line between them, readable and copyable with the same motions and search as the `Draft`, but not editable; the user's own last mail, in `design.marked`, is the line between what is new since they wrote and the history below
- one `## recap`, in `design.recap`, before the body of the newest stretch opener that has one
- a `notes.Private reply` below its mail, editable there; tangents open as their own `Thread`, not folded into the main history
states:
- empty: the empty draft, then the opening mail; a thread begins with a mail
- loading: the thread's mail is not yet available; wording not agreed
- partial: only the available mail; wording not agreed
- error: the thread is unavailable; wording not agreed
- full: whether opened by the deal or by name, the window starts at the top of the document: the `Draft`, with the point in it where it was left, then the newest mail. Older mail is one scroll down. Returning to a place restores it as left.
keys:
- `enter` with no selection on a project's mail: `Mail.look into the work`
- `ctrl-u`, `ctrl-d`: scroll history by half a window without moving the point out of the `Draft`
- `gg`: go to the draft; `G`: go to the oldest mail; `i`, `A` or `o`: `Draft.edit`; `ctrl-enter` or `send`: `Draft.send`
- `enter` on a selection in one mail: `Draft.quote`
- `alt-r` on a mail: `notes.Private reply.reply privately`
- `u`, `shift-u` in the draft: undo and redo the last edit
- `space`: `deal.Verdict`, anywhere in Normal mode
- `:`: `window.Palette`, which also offers the agent's own commands here, as in `agents.Steer an agent`
- `ctrl-k`, `tab`, `q`: `window.Screen`
starts: `Answer a mail`, `agents.Look into the work`, `notes.Reply privately`
rejected:
- the mail that pings at the top of the window · 2026-11-21 · open on what the project has said since the user's last word, with the next reply ready in either way of opening
- oldest mail first, the draft at the end · 2026-11-21 · the first project mail since the user's last word could not be at the top and the window full at once; with the newest first and the draft leading, open is always the top and the user's own last mail separates new from history. `newest mail first` was itself rejected on 2026-11-20 as `the exchange reads in its own order`; a first reading now runs bottom up, and the recap covers the cold return
- the agent's transcript beside the mail in a second column · 2026-11-20 · one place in focus; the transcript is one level down
- the card alone, with the history folded · 2026-11-20 · a short history costs nothing and a long one is one scroll away
- the agent's work folded under each mail · 2026-11-20 · the transcript is `agents.Look into the work`
- a project split with the thread list at the left · 2026-11-20 · one screen at a time
- marking the mail under the point by brightness or a bar · 2026-11-20 · the cursor is enough
- a background tint behind the recap · 2026-11-20 · a slight text colour reads better

The point never rests on the blank line between two mails. A reply from a dealt card settles the card and stays on the thread; the next card waits for `deal.Card.deal`. On the phone, the draft is the tap target to write.

Built renders: [asking mail on the laptop](design/laptop-card.png) and [on the phone](design/phone-card.png), [an outcome with its proof](design/laptop-proof.png), [a cold recap above its mail](design/laptop-reading.png), and [the notice in a stuck phone draft](design/phone-reading-notice.png).

mock:
```text
clock@lab › Flaky clock test                                   waits · 2h   NOR
reply

## recap
The mock clock is shared between tests. One run in five lands on a DST boundary.

Is a separate clock per test enough, or do you want the suite isolated too?

Fix the clock test that fails one run in five.
It started after the DST change last week.
```

`reply` is the empty draft's placeholder, in `design.quiet`, with the point on it. The project's newest mail follows in reading text, led by its recap in `design.recap` because that mail opened the latest stretch with a recap. The last two lines are the user's own mail, in `design.marked`: everything above it is new since they wrote, everything below it is history.

mock, phone:
```text
clock@lab › Flaky clock test…  waits · 3m  NOR
reply

Could you clarify which behaviour you want?

Fix the clock test that fails one run in five.

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
2. the user writes and edits an answer in the `Draft` at the top of the thread
3. the user sends the answer with `Mail.reply`; the reply settles the card
system: opens at the top with the point in the `Draft` and mail below it, newest first. The user's last mail separates what arrived since they wrote from earlier history. The thread keeps the draft if the user leaves, and sends the reply to whoever wrote the mail.
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
1. the user presses the idea key, `i` on a `desk.Desk` row or the idea key elsewhere; a `Draft` fills the window, with a `to:` line already addressed to the project picked from context, and an `on:` line with that project's runtime
2. the user writes, in the usual modes
   - if it is for a different project, the user edits the `to:` line with project completion, or `ctrl-t` asks for the project; a name with no match offers a `new project …` row, which is `Start a project`
   - if this one idea should think on another model, the user edits the `on:` line, with the host's runtimes as completions; `shift-tab` moves between the lines and the words; a runtime the host does not know is refused in the echo and the idea stays
3. the user sends with `ctrl-enter` or `send`; the echo names the idea and the project, and the screen before is back
system: picks the thread's project, or the last project the user wrote to; fills `on:` with that project's runtime, again when `to:` changes and the user has not touched `on:`; starts the work at that project's place, in its mode, on the `on:` runtime, as `Project.receive an idea`.
user: the words and, if different from the automatic choice, the project and the runtime.
cases: a multiline idea edited before sending; an unsent idea still present on this device after restart; the idea comes while reading a page; the current thread belongs to a different project; an idea on another runtime to a `one agent` project whose agent cannot change kind, refused as `set runtime` refuses it, with the idea kept.
mock:
```text
to: tide@lab
on: sol-high                                        sol-high
                                                    sol-medium
                                                    opus-high  project

idea
```
rejected:
- a one-line idea strip across every screen · 2026-11-20 · never agreed; a full window with the usual modes costs nothing when the window shows one place at a time
- a `to:` prompt before the draft opens · 2026-11-20 · the address is a line of the draft
- a draft tail at the bottom of the Desk · 2026-11-20 · the same draft is one key away, without a line of chrome on the Desk
- two send keys, one that starts an agent and one that only sends · 2026-11-20 · one wording and one key everywhere; the project decides what the mail starts
- a palette command `send on…` that asks the runtime · 2026-11-21 · it hides the default and adds a step; a line shows what the idea will run on before it goes
reviewed: 2026-11-20 · laptop · reviewer · 1 finding

## flow Start a project
runs: 1 a week
day: Turn ideas into work.
when: the user has an idea for work that no project holds yet
want: name the project and say once where, how, and on what model its agents work
so that: the idea starts now, and every later idea to that address starts with no question
steps:
1. in an idea, the user writes `to: tide@lab`; a name with no match offers `new project tide@lab`, and `enter` takes it
2. `tide@lab works on:` asks for the place, with the host's known repositories and directories as completions; a URL or a `git@` address is a repository, a path is a directory on `lab`
3. `tide@lab agents:` asks for the mode, `one` under the point and `per idea` below it
4. `tide@lab runtime:` asks for the model and effort, `opus-high` under the point and the host's other runtimes below it
5. the idea sends and the work starts, as in `Write an idea`
   - from the Desk palette, `new project` asks the same four lines and opens the empty project
system: the host keeps the project's name, place, mode and runtime, so every device sees the same answer; clones a repository from the host's mirror; names the project after the host in the address, so no host question.
user: the name, the place, the mode, and the runtime.
cases: the host is offline; an older project with no known place says `no place set` until `works on…` is answered; an older project with a place but no runtime reads and runs `opus-high` until `runtime…` is answered; the directory does not exist, so the thread keeps a `failed` stop as `agents.a refused start leaves mail`; a second project on the same repository; a project made from the Desk with no idea yet.
mock:
```
to: tide@lab                                        new project tide@lab
tide@lab works on: git@github.com:maan2003/tide     repository
tide@lab agents: one                                every idea goes to the same agent
                 per idea                           each idea gets a fresh agent in its own workset
tide@lab runtime: opus-high                         default
                  sol-high
                  sol-medium
```
rejected:
- a menu `where this project's agents work` with two rows and a prompt behind each · 2026-11-21 · three kinds of UI for one answer, and the place stayed invisible
- an `on host:` prompt · 2026-11-21 · the address names the host
- the mode asked only for a repository · 2026-11-21 · several agents in one directory are fine too
- a second question at the second idea · 2026-11-21 · a surprise later costs more than one more line now
- no runtime question, every project on `opus-high` · 2026-11-21 · a project thread could not start on another model at all

The historical review above was of Start a thread. Starting an agent from a mail was walked separately:
> reviewed: 2026-11-20 · laptop · reviewer · 4 findings

## rule the draft answers what the user came for
when the user opens a `Thread` from a `deal.Card`, then the `Draft` answers the dealt mail, even when newer mail sits between it and the draft; when the user opens the thread by name, the draft answers the newest mail that pings, or the newest mail when none does. The thread says which mail that is whenever it is not the newest, and the user can change it with `Draft.answer this mail`.
cases: two agents ask in one thread and the deal opens the older ask; the user opens by name while two asks wait; the user quotes a line of an older mail, which does not change the target.
rejected:
- the draft always answers the newest mail · 2026-11-21 · the deal brought the user to an older ask, and the reply would go to the wrong writer
- the draft silently answers the dealt mail · 2026-11-21 · the words under the draft were another agent's, and the reply went to the one above without a sign

## rule the last mail stays live
when a project agent has already sent a `Mail` since the user's last mail in the `Thread`, then rho refuses another from that agent and tells them which mail to edit, unless the agent explicitly opts in with `replace_last=True`. That option edits its live mail, preserving its ping unless set, and returns the same id. If it has no live mail in the thread, it sends and returns a new one. Other agents can each send one mail. A mail that was deleted or taken back no longer occupies that place. A real edit reads `edited`, and the user's reply goes to the writer of the words shown. An unchanged ask does not deal again after `done`; changed words do. Once the user replies, the ask is frozen. A different question forks under the mail that raised it. Mail to the user stays in the thread but does not wake its agents.
why: a thread of ten mails that each supersede the last is a feed; each agent has one place for current words.
rejected:
- a second send merges in place · 2026-11-21 · the agent has to choose to edit its existing mail; another send must not silently change it

## rule a cold mail carries a recap
when project `Mail` arrives or is edited in a `Thread` more than ten minutes after the user's last mail or verdict there (or with neither), then it opens a cold stretch. The newest stretch opener with a `## recap` shows that section in `design.recap` before its body. Other recap sections are hidden when there are words outside the recap. A mail consisting only of a recap stays visible as ordinary mail, with its heading. An answer within ten minutes does not move or hide the chosen recap.
why: after a long gap the user can answer without reading the history, and the same context stays visible while they work.
rejected:
- cold from when the user last had the thread open · 2026-11-21 · reading on either device must not change the recap; the user's actual words and decisions mark the return

rho tells the project once when the thread goes cold. The project's next pinging mail, new or edited, carries a recap that lets the user answer without the history. When ten minutes pass with a pinging mail that has no recap, rho reminds the project once.

## rule new mail lands below the draft
when new `Mail` arrives, the user's own or the project's, or the newest mail is edited, then it takes its place right below the `Draft` and the point does not move. With the window at the top, the user sees it at once and older mail moves down. With the draft stuck to the top edge, the view stays where it is, and for project mail the stuck draft shows a quiet `↑ new mail` at its right end until the user scrolls back to the top.
rejected:
- history pushes up to keep the draft in place at the tail, `↓ new mail` for project mail when stuck at the bottom · 2026-11-21 · the mirror of this rule for the oldest-first order
