# mail

The user writes a project a mail to put an idea to work. Each piece of work has one thread, with a subject, the mails so far, and one line about what is happening now.

## bet A mail is the medium
instead of: separate conversations beside the project, or a channel full of individually named agents.
serves: Keep the work moving.
because: one thread keeps one piece of work together. Its subject identifies the work, its status tells the user what is happening now, and the project speaks with one voice even when several agents do the work.
costs: the user opens the transcript to see which agent did what.

An agent writes to another agent by mail in a thread under the project; the user can read it one level down and never gets a `deal.Card` from it.

## bet The project asks by mail, even when an agent stops
instead of: attention coming from a mixture of named agents' chat mentions and notices about their state.
serves: Know what wants them, and where they were.
because: the user only needs what asks for them or returns a result, not each agent's progress. A stop is another mail to answer, not another kind of thing to watch.
costs: a failure can still ask for the user when they have nothing to add.

## journey An idea becomes a merged result
status: ideas are written in channels, and a result carries no proof yet.
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
sees: its subject and project; on a row, what waits or its `Status` if nothing waits; open, its mails in order, with history reached by scrolling and the status pinned above the reply box.
has:
- many `Mail`
- many `Thread`, each a tangent under a mail
- one `Draft` in the reply box
does:
- open: from a `deal.Card`, land on the mail that asks; otherwise open the thread by subject
- reply: write in the box and send with `Mail.reply`
- fork: take a tangent into a thread under its mail; the prompt line says when to fork

## concept Mail
status: a result does not yet carry designated proof.
purpose: carry a brief, an ask, or a result in the voice of the project, and let the user answer whoever wrote.
principle: after the user has been away, a project quotes just enough of the brief to ask a question that stands on its own; the user replies without needing to know the writer's name.
sees: the user or the project's address as sender, the time, the words, what the mail quotes, `edited` after an edit, and proof on a result.
does:
- reply: send the user's words to whoever wrote the mail
- edit or delete: agents may change or take back their own mails, except that an ask the user has answered stays as it was
- reply privately: keep a remark beside this mail, as in `notes.Reply privately`

Agents can search mail. Mail written while the host is away waits on the device and goes out when the host is back.

## concept Status
purpose: say in one glance what the work is doing now, without asking the user to read another mail.
principle: an agent reports `Checking the clock fix`, then another reports `Running the tests`; the user sees only the latter on the thread and on its row when nothing waits.
sees: one line about what the work is doing now, replaced by the newest line rather than added to the mail history.
does:
- update: the agents working in the thread set its status

## concept Draft
status: a draft is kept on the device it was written on only.
purpose: keep what the user wrote in a thread and has not sent, so leaving the thread loses nothing.
principle: the user is halfway through an answer when a card that matters comes in; they deal it and answer it; later, on another of their devices, they open the first thread, and the half-written answer is in its box.
sees: the text in the thread's reply box, as it was left.

A thread has one draft. Sending the mail ends that draft.

## screen Thread
answers: what is this piece of work waiting for from the user?
shows:
- the asking `Mail` at the cursor when a `deal.Card` opens, with any quoted text or result proof
- the `Thread` subject and `Project` address in the `window.Status line`, with the card's words when it waits
- other `Mail` in order, with history reached by scrolling
- `Status` pinned immediately above the reply box and its `Draft`
- a `notes.Private reply` beside the mail it answers; tangents open as their own `Thread`, not folded into the main history
states:
- empty: the opening mail and the reply box; a thread begins with a mail
- loading: the thread's mail is not yet available; wording not agreed
- partial: only the available mail; wording not agreed
- error: the thread is unavailable; wording not agreed
- full: the asking mail in view, history above it, status above the reply box
keys:
- `i`: write the reply; `ctrl-enter`: send it
- `space`: `deal.Verdict`
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
reply
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
2. the user writes an answer in the thread's reply box
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
1. the user opens the idea box with the idea key, or uses the box on `desk.Desk`
2. the user writes the idea
   - if it is for a different project, the user changes the address with project completion
3. the user sends the mail and stays where they were
system: picks the thread's project, or the last project the user wrote to; starts the work where that project's agents work.
user: the words and, if different from the automatic choice, the project.
cases: the idea comes while reading a page; the current thread belongs to a different project.
reviewed: 2026-11-20 · laptop · reviewer · 1 finding

The historical review above was of Start a thread. Start an agent from a message was walked separately:
> reviewed: 2026-11-20 · laptop · reviewer · 4 findings

## rule a cold mail stands alone
when more than ten minutes have passed since the user had a `Thread` open, then it is cold, rho tells the `Project` once; cold `Mail` stands alone by quoting rather than repeating, and warm mail is short.
