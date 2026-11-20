# chat

Chat is where the user talks with the agents and, through Slack, with people: on each host, channels, threads under them, and threads under those; in Slack, conversations and the threads under their messages. A message goes to a thread, not to a person, and reaches the user only when it mentions them.

## bet Chat is the medium
instead of: a session per agent, a task board, a form, or a pull request to review.
serves: Keep the work moving.
because: the user runs many projects and many agents at once, and a thread keeps each piece of work, its agents, and every word said about it in one place that both can read; a message is the brief, so an agent starts from one and reads it first.
costs: an agent's progress is a stream of messages the user must read, not a status they can see at a glance.

## bet An agent reaches the user by mention, or by a stop it did not choose
instead of: every message from an agent asking for the user's attention, or agent state driving attention.
serves: Know what wants them, and where they were.
because: agents write far more than the user can read; a mention that is an ask or a result is the agent's one way to say "you, now", and a stop it did not choose says the same without words.
costs: a card for a failure the user may not care about, which `deal.Verdict.mute` puts away.

## concept Channel
purpose: keep the threads of one subject or one project together, so the user reads them in one place.
principle: the user makes a channel `fedi`; every thread about that project starts in it; on Home, `fedi` is one row that says what waits in it.
sees: its name and its host; open, the list of its threads, one line each.
has:
- many `Thread`
does:
- new channel: ask `new channel:` and post the answer as the channel's first message; with more than one host attached, ask `on host:` first
- open `enter` on Home: show its threads

## concept Thread
purpose: hold one conversation, between the user and the agents working on it or between the user and people in Slack, with its replies folded under each message.
principle: the user writes `hey, fix the flaky test` in a channel; an agent starts from that message and joins its thread; it posts its progress there and mentions the user when it needs an answer or is done.
sees: its name, how many mentions of the user wait unread as `@1`, how many messages are new as `3 new`, how many replies it has, the first two agents in it, and how long since its newest message; open, the line of threads above it, its first message, `with` and the agents in it, then every message under it, then a box to write in.
has:
- many `Message`
- many `Thread`, one under each message that has replies
- many `agents.Agent`
- many `notes.Private reply`
- one `Draft`
does:
- open `enter` on its line: open the thread
- write `i`, or `ctrl-down`: put the cursor in the box at the bottom, which reads `message · ctrl-enter sends` while empty; `escape` or `ctrl-up` goes back to the messages
- send `ctrl-enter`, or `send` on the phone: post what is in the box
- name, from the palette as `name this thread`: ask `name (current):` and give the thread that name
- open a participant `enter` on an agent in the `with` line: open that agent's screen, `agents.Agent`
- go up `enter` on the line of threads above: open the thread this one is under
- star `z` in the `slack` menu, on a Slack conversation: make it a `home.Room`, or stop
- follow thread `t` in the `slack` menu, on a Slack thread: let its replies make cards, or stop
- broadcast reply `b` in the `slack` menu, on a Slack thread: send replies to the conversation too, or stop
- attach file… `a` in the `slack` menu, on a Slack conversation or thread: ask for a file to send with the next message; `remove attachment…` takes one back

A thread is named for the user: after its second message, the host gives it a short name from its first two messages, unless the user or an agent named it first. Until then, its first line is its name. A name keeps any marks in the text, such as `**`, as they were typed. In a channel's list, text between two `~` signs is drawn struck through, so a line that holds two paths such as `~/src` is partly crossed out.

The list of threads in a channel puts the threads with a mention of the user first, then the ones with something new, each group newest first.

A thread shows what each of its agents is doing. The `with` line reads each agent with its state and age, such as `with eng-q9tz · working 12m, eng-kjhi · failed 1h`, refreshed each minute, and with the cursor on that line the palette offers that agent's steering commands, so the user steers from the thread.

A Slack conversation is a thread whose messages are what was said in a Slack channel, group, or direct message, with the Slack thread under each message folded beneath it. Its name is such as `#design-24`, `@Barbara Liskov`, or the names of a group. It also shows a line for each new day and a `new` line above the first unread message; older messages load as the user scrolls up to `older messages not loaded`. A message being sent shows `sending…` under the box; one that failed shows `send failed · Enter retries`.

## concept Message
purpose: carry one thing said in a thread, by the user, an agent, or someone in Slack.
sees: who wrote it, `you`, the agent's long code such as `q9tz45g2bcvs`, or a Slack writer's initials, and when, such as `06:16` today or `Fri 2 Oct 07:27` before; then its text, with lists, bold, code, emoji, and links drawn; a picture is named as `[image · image/png]`; a Slack message's reactions, such as `🎉 1`, and its files; under it, `↳ 2 replies · name · time` when it has replies.
does:
- open its replies `enter`: open the thread under this message
- start an agent from it: as `agents.Start an agent from a message`
- reply privately: as `notes.Reply privately`
- message actions… `e` in the `slack` menu, on a Slack message: `edit` and `delete…` on the user's own, `react…`, `copy link`, `forward…`, and `mark unread`
- react…: a menu of the emoji already on the message and the user's recent ones, each on a key, with `/` `by name…` for any other; picking one the user already gave takes it away
- delete…: ask `delete message? type delete:`; only the word `delete` deletes, for good
- forward…: ask `forward to:` with the user's Slack conversations, and send the message there
- open `enter` on a Slack message, on what the cursor is on, the most specific first: unfold a long message at `Show more ↓`; press an app's button, or open its link in the browser; open a file with the device's own app for it; open a link, and a link to a Slack message opens that message here; otherwise open the thread the line is in

A message the user wrote shows `you · sending…` until the host has it. A message written while the host is away waits on the device and goes out when the host is back.

An app's other controls and an app's dialogs in a Slack message do not work here. The echo line says so and sends the user to Slack, such as `slack: app dialogs are not supported here yet; use Slack for this one`.

## concept Draft
status: a draft is kept on the device it was written on only.
purpose: keep what the user wrote in a thread and has not sent, so leaving the thread loses nothing.
principle: the user is halfway through an answer when a card that matters comes in; they deal it and answer it; later, on another of their devices, they open the first thread, and the half-written answer is in its box.
sees: the text in the thread's box, as it was left; for Slack, in the `slack.Workspace` `drafts…` list.

A thread has one draft. A Slack draft also holds the files attached to it. Sending the message ends the draft.

## screen Channel
shows: `Channel`, `Thread`, the box to write in
starts: `Start a thread`, `Answer an agent`

With no threads yet, the channel reads `no threads yet · the composer starts one`.

## screen Thread
shows: `Thread`, `Message`, `Draft` in the box to write in, `notes.Private reply`, `agents.Agent` in the `with` line
starts: `Answer an agent`, `slack.Answer Slack`, `agents.Start an agent from a message`, `notes.Reply privately`

When a Slack conversation opens while it asks for the user, the cursor lands on the oldest message from someone else that the user has not dealt with.

## job Answer an agent
day: Keep the work moving.
when: an agent asked me something, or told me it is done
want: read what it said and reply in its thread
so that: its work goes on, or I know it is finished
flow:
1. the thread is on screen, from the deal or from Home
2. read the newest messages; open a reply's thread with `enter` when the answer is under it
3. press `i` and write the answer
4. `Thread.send`; the message shows `you · sending…`, then the time
5. `deal.Verdict.done` if nothing more is needed, or `deal.Card.deal` for the next card
system: names the thread, folds the replies under each message, keeps the `Draft` if I leave before sending, and counts my message as having read the thread.
user: the words of the answer.
cases: the host is away while I write; the agent that asked is gone

## job Start a thread
day: Turn ideas into work.
when: I have a new piece of work or a question
want: write it once in the right channel and have it become a thread
so that: an agent can start from it and the conversation stays in one place
flow:
1. open the `Channel`
2. press `i` and write the message
3. `Thread.send`; the message is a new thread in the channel
system: names the thread after the second message.
user: the channel and the words.
cases: I want an agent to start at once: use `send and start agent` instead, as in `agents.Start an agent from a message`

## rule a channel is a room
when a host's `Channel` is one the user made or has written in, or holds a `deal.Card` or an unread mention of the user, then it is a `home.Room` on Home, tagged with the host's name; a channel the user never wrote in that asks nothing is not.
