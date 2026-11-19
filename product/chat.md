# chat

Chat is where the user and the agents talk: on each host, channels, threads under them, and threads under those. A message goes to a thread, not to a person, and reaches the user only when it mentions them.

## bet Chat is the medium
instead of: a session per agent, a task board, a form, or a pull request to review.
serves: Keep the work moving.
because: the user runs many projects and many agents at once, and a thread keeps each piece of work, its agents, and every word said about it in one place that both can read; a message is the brief, so an agent starts from one and reads it first.
costs: an agent's progress is a stream of messages the user must read, not a status they can see at a glance.

## bet An agent reaches the user by mention, or by a stop it did not choose
status: an agent that fails, retries for a long time, or is lost by its host makes no card; the user finds it in the transcript or in `window.Find`.
instead of: every message from an agent asking for the user's attention, or agent state driving attention.
serves: Know what wants them, and where they were.
because: agents write far more than the user can read; a mention that is an ask or a result is the agent's one way to say "you, now", and a stop it did not choose says the same without words.
costs: a card for a failure the user may not care about, which `deal.Card.mute` puts away.

## concept Channel
purpose: keep the threads of one subject or one project together, so the user reads them in one place.
principle: the user makes a channel `fedi`; every thread about that project starts in it; on Home, `fedi` is one row that says what waits in it.
sees: its name and its host; open, the list of its threads, one line each.
has:
- many `Thread`
does:
- new channel: ask `new channel:` and post the answer as the channel's first message; with more than one host attached, ask `on host:` first
- open `enter` on Home: show its threads

On Home a channel is a `home.Room`. Every message without a parent is a channel, so a host that posts one message per event, such as each deploy, fills Home with one room per event.

## concept Thread
purpose: hold one conversation between the user and the agents working on it, with its replies folded under each message.
principle: the user writes `hey, fix the flaky test` in a channel; an agent starts from that message and joins its thread; it posts its progress there and mentions the user when it needs an answer or is done.
sees: its name, how many mentions of the user wait unread as `@1`, how many messages are new as `3 new`, how many replies it has, the first two agents in it, and how long since its newest message; open, the line of threads above it, its first message, `with` and the agents in it, then every message under it, then a box to write in.
has:
- many `Message`
- many `Thread`, one under each message that has replies
- many `agents.Agent`
- many `notes.Private reply`
does:
- open `enter` on its line: open the thread
- write `i`, or `ctrl-down`: put the cursor in the box at the bottom, which reads `message · ctrl-enter sends` while empty; `escape` or `ctrl-up` goes back to the messages
- send `ctrl-enter`, or `send` on the phone: post what is in the box
- name, from the palette as `name this thread`: ask `name (current):` and give the thread that name
- open a participant `enter` on the `with` line: open the first agent's transcript
- go up `enter` on the line of threads above: open the thread this one is under

A thread is named for the user: after its second message, the host gives it a short name from its first two messages, unless the user or an agent named it first. Until then, its first line is its name. A name keeps any marks in the text, such as `**`, as they were typed. In a channel's list, text between two `~` signs is drawn struck through, so a line that holds two paths such as `~/src` is partly crossed out.

The list of threads in a channel puts the threads with a mention of the user first, then the ones with something new, each group newest first.

A thread shows what each of its agents is doing, and the user steers an agent from the thread, not from its transcript.

status: the `with` line names the agents without their state, and steering is from the transcript's palette.

## concept Message
purpose: carry one thing said in a thread, by the user or by an agent.
sees: who wrote it, `you` or the agent's long code such as `q9tz45g2bcvs`, and when, such as `06:16` today or `Fri 2 Oct 07:27` before; then its text, with lists, bold, code, and links drawn; a picture is named as `[image · image/png]`; under it, `↳ 2 replies · name · time` when it has replies.
does:
- open its replies `enter`: open the thread under this message
- start an agent from it: as `agents.Start an agent from a message`
- reply privately: as `notes.Reply privately`

A message the user wrote shows `you · sending…` until the host has it. A message written while the host is away waits on the device and goes out when the host is back.

## concept Mention
purpose: let an agent draw the user to a thread only when it matters.
principle: an agent's message mentions the user as an ask; the thread becomes a `deal.Card` that reads `asks`.
sees: in a channel's list of threads, `@1` beside the thread's name in yellow.

A mention is an ask, a result, or for the record. Only an ask and a result make a card; see `deal.Ask` and `deal.Result`.

## screen Channel
shows: `Channel`, `Thread`, the box to write in
starts: `Start a thread`, `Answer an agent`

With no threads yet, the channel reads `no threads yet · the composer starts one`.

## screen Thread
shows: `Thread`, `Message`, `notes.Private reply`, `agents.Agent` in the `with` line, the box to write in
starts: `Answer an agent`, `agents.Start an agent from a message`, `notes.Reply privately`

## job Answer an agent
when: an agent asked me something, or told me it is done
want: read what it said and reply in its thread
so that: its work goes on, or I know it is finished
flow:
1. the thread is on screen, from the deal or from Home
2. read the newest messages; open a reply's thread with `enter` when the answer is under it
3. press `i` and write the answer
4. `Thread.send`; the message shows `you · sending…`, then the time
5. `deal.Card.done` if nothing more is needed, or `deal.Card.deal` for the next card
system: names the thread, folds the replies under each message, keeps the draft if I leave before sending, and counts my message as having read the thread.
user: the words of the answer.
cases: the host is away while I write; the agent that asked is gone

An idea becomes an agent at work with the fewest questions the product can manage. Where the agent works follows from the channel, which is a project, and the model follows from a default written in the product; the user changes either when they want to.

status: every start asks where the agent works and which runtime, through the palette.

## job Start a thread
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
when a host has a message with no parent, then that `Channel` is a `home.Room` on Home, tagged with the host's name.
