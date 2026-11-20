# mail

The user writes a project a mail to put an idea to work. Each piece of work has one thread, with a subject, the mails so far, and one line about what is happening now.

## bet A mail is the medium
instead of: separate conversations beside the project, or a channel full of individually named agents.
serves: Keep the work moving.
because: one thread keeps one piece of work together. Its subject identifies the work, its status tells the user what is happening now, and the project speaks with one voice even when several agents do the work.
costs: the user opens the transcript to see which agent did what.

An agent writes to another agent by mail in a thread under the project; the user can read it one level down and never gets a `deal.Card` from it.

## bet The project asks by mail, even when an agent stops
status: when an agent cannot launch, its stop mail explains how to get the work going. Further replies are saved without retrying the failed launch.
instead of: attention coming from a mixture of named agents' chat mentions and notices about their state.
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
status: the project's own screen is still the channel's list of threads, with agent names and counts on its rows.
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
sees: its subject and project; on a row, what waits or its `Status` if nothing waits; open, its mails in order, with history reached by scrolling and the status just above the draft separator.
has:
- many `Mail`
- many `Thread`, each a tangent under a mail
- one `Draft` at the end of the thread
does:
- open: from a `deal.Card`, land on the mail that asks; otherwise open the thread by subject
- reply: write at the end of the thread and send with `Mail.reply`
- fork: take a tangent into a thread under its mail; the prompt line says when to fork

## concept Mail
status: a result does not yet carry designated proof.
purpose: carry a brief, an ask, or a result in the voice of the project, and let the user answer whoever wrote.
principle: after the user has been away, a project quotes just enough of the brief to ask a question that stands on its own; the user replies without needing to know the writer's name.
sees: the user or the project's address as sender, the time, the words, what the mail quotes, `edited` after an edit, and proof on a result.
does:
- look into the work `enter` or a tap on the header: open the `agents.Agent` that wrote the mail at the step that produced its current words; a failed stop opens its error
- reply: send the user's words to whoever wrote the mail
- edit or delete: agents may change or take back their own mails, except that mail the user has answered stays as it was
- reply privately: keep a remark beside this mail, as in `notes.Reply privately`

Agents can search mail. Mail written while the host is away waits on the device and goes out when the host is back.

## concept Status
purpose: say in one glance what the work is doing now, without asking the user to read another mail.
principle: an agent reports `Checking the clock fix`, then another reports `Running the tests`; the user sees only the latter on the thread and on its row when nothing waits.
sees: one line about what the work is doing now, replaced by the newest line rather than added to the mail history.
does:
- update: the agents working in the thread set its status

## concept Draft
purpose: keep what the user wrote in a thread and has not sent, so leaving the thread loses nothing.
principle: the user is halfway through an answer when a card that matters comes in; they deal it and answer it; later, on the same device, they open the first thread, and the half-written answer is still below the separator, even after a restart.
sees: the unsent text below the separator, as it was left.
does:
- edit `i`, `A`, `o` at the tail, or tap the tail: write in place; `escape` returns to Normal with the cursor still in the draft
- quote `enter` on a visual selection in a mail: add those lines to the draft with `> ` in front
- send `ctrl-enter`, or `send` on the phone bar: move the words into the history and leave an empty draft

A thread has one draft on each device, not shared with other devices.

## screen Thread
status: the deal lands on the first waiting mail after the latest reply when there is one. An older unanswered mail still waits.
answers: what is this piece of work waiting for from the user?
shows:
- the asking `Mail` at the cursor when a `deal.Card` opens, with any quoted text or result proof
- the `Thread` subject and `Project` address in the `window.Status line`, with the card's words when it waits
- other `Mail` in order, readable and copyable with the same motions and search as the `Draft`, but not editable
- `Status`, then a quiet separator naming the recipient and send key, then the `Draft`, all in the same buffer
- a `notes.Private reply` beside the mail it answers; tangents open as their own `Thread`, not folded into the main history
states:
- empty: the opening mail and a blank line below the separator; a thread begins with a mail
- loading: the thread's mail is not yet available; wording not agreed
- partial: only the available mail; wording not agreed
- error: the thread is unavailable; wording not agreed
- full: the asking mail in view, with history and the draft tail reachable in the same buffer
keys:
- `enter` on an agent's mail: `Mail.look into the work`
- `G`: go to the draft; `i`, `A` or `o`: `Draft.edit`; `ctrl-enter` or `send`: `Draft.send`
- visual selection then `enter`: `Draft.quote`
- `space`: `deal.Verdict`, anywhere in Normal mode
- `:`: `window.Palette`
- `ctrl-k`, `tab`, `q`: `window.Screen`
starts: `Answer a mail`, `agents.Look into the work`, `notes.Reply privately`
mock:
```text
tide@lab › Flaky clock test                                     asks · 2h     NOR

you · 09:12
Fix the clock test that fails one run in five.

tide@lab · 09:52
> Fix the clock test that fails one run in five.
The mock clock is shared between tests. Is a separate clock per test enough?

Running the tests with a separate clock
── reply to tide@lab · ctrl-enter sends
Yes, use a separate clock.
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
2. the user writes and edits an answer in the `Draft` at the tail of the thread
3. the user sends the answer with `Mail.reply`; the reply settles the card
system: opens the asking mail, keeps the `Draft` if the user leaves, and sends the reply to whoever wrote the mail.
user: the words of the answer, or done when there are none.
cases: a cold mail that needs no prior reading; a result with proof; a colleague reached through `slack.Bot`.
reviewed: 2026-11-20 · laptop · reviewer · 2 findings

The historical review above was of Answer an agent. Answer Slack was walked separately:
> reviewed: 2026-11-20 · laptop · reviewer · 6 findings

## flow Write an idea
runs: 12 a day
day: Turn ideas into work.
when: the user has an idea, wherever they are in the window
want: send it to the right project without leaving their place
so that: work starts and the user can carry on where they were
steps:
1. the user writes in the `design.Draft tail` on `desk.Desk`, or opens the idea strip elsewhere with the idea key
2. on Desk, the user can return to Normal and edit the same text without hiding it
   - if it is for a different project, the user changes the address with project completion
3. the user sends the mail and stays where they were
system: picks the thread's project, or the last project the user wrote to; starts the work where that project's agents work.
user: the words and, if different from the automatic choice, the project.
cases: a multiline idea on Desk is edited before sending; an unsent idea is still present on this device after restart; the idea comes while reading a page; the current thread belongs to a different project.
reviewed: 2026-11-20 · laptop · reviewer · 1 finding

The historical review above was of Start a thread. Start an agent from a message was walked separately:
> reviewed: 2026-11-20 · laptop · reviewer · 4 findings

## rule the last mail stays live
when the project's last `Mail` in a `Thread` has no user reply or verdict, then it stays live; the project edits it as findings change instead of sending updates. Once the user replies, it is frozen. A reply or verdict earns new mail if there is more to say. A different question forks under the mail that raised it.
why: a thread of ten mails that each supersede the last is a feed; one live mail is a state the user can read in one look.

## rule a cold mail stands alone
when more than ten minutes have passed since the user had a `Thread` open, then it is cold, rho tells the `Project` once; the project edits live `Mail` to stand alone with a short exact quote instead of a recap. New cold mail also stands alone; warm mail is short.
