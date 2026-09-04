# Slack in rho

A design for bringing Slack mentions and threads into rho's dealer and
surfaces. This continues DESK-DESIGN.md and keeps its rules. It records the
*why* behind each decision; the engineer building it decides the mechanics.
Rho has exactly one user.

Reference implementation to port from: `~/src/emacs-slack` (Elisp, ~23k
lines, of which the transport and model half is ~6.7k). The files that
matter: `slack-websocket.el`, `slack-team-ws.el`, `slack-request.el`,
`slack-activity-feed-buffer.el`, `slack-message.el`,
`slack-message-sender.el`, `slack-conversations.el`, `slack-block.el`.

## The problem

Slack is where coworkers owe the user answers and the user owes them. Today
those obligations live in a separate app with its own badge, so the dealer
cannot see them and the user has to go look. The desk and the dealer exist
so the user never has to go look.

The goal is to replace the Slack app entirely, not to mirror its badge:
reading channels and direct messages, writing in them, and following
threads all happen in rho, in the editor, with vim keys. The dealer part
(mentions and threads as cards) is the reason to do it; the client part is
what makes it possible to close the other app.

## Core decisions and why

### Rho is the Slack client, on the client side, with no daemon involvement

rho-gui connects to Slack directly with the user's own web session (the
`xoxc` token and the `d` cookie), exactly as emacs-slack has for years:
`rtm.connect` for a websocket, the web API for everything else.

**Why:** the daemon is about agents and the desk; a Slack session belongs
to the person, and the person sits at the client. Going through the
embedded browser page instead (hooking its websocket) is fragile and only
works while a page is open; a direct client is a few hundred lines and has
a decade of precedent. This is unofficial and Slack owes it nothing; the
design accepts that.

### Credentials are entered by hand, for now

A prompt takes a workspace name, the `xoxc` token, and the `d` cookie, and
stores them in the client state directory with owner-only permissions.
Several workspaces can be registered. Nothing scrapes the browser yet.

**Why:** extraction can be automated later once the rest works; a manual
prompt is enough to use it today and keeps the first version small.

### A Slack unit is a conversation or a followed thread, never a message

The thing rho deals is a unit with a stable identity that Slack itself
keeps: a direct or group message conversation (workspace, channel), a
channel the user was mentioned in (workspace, channel), or a followed
thread (workspace, channel, thread timestamp). A message is never a unit
and never a card: a message timestamp is a fact about a unit, not an
identity. One card per unit at most, so a channel with three unhandled
mentions is one card that lands the reader on the oldest of them.

Facts, all read from the mirror and all monotonic (every source, a live
frame, a feed poll, a history page, a reconnect, a restart, can only raise
them by `max`, never lower or reset them): `newest`, the newest message
timestamp; `newest_from_other`, the newest message from someone else that
concerns the user (any message in a direct message, a mention in a
channel, a reply in a followed thread); `newest_author`, who wrote
`newest`, the user or someone else.

Rho state, per unit, in the Slack store: `handled_through`, a timestamp
cursor; `defer_until`; `pace_days`. Nothing but a verdict key moves them.

- `d` done: `handled_through := newest`.
- `t` todo: done, and a `slack` node for the unit is created in the tree
  at the area asked, deferred and paced the way todo notes are today, so
  the tree deals it as a todo and `j` on it opens the conversation.
- `x` discard: done, and the unit is silenced where Slack has a place
  for it: a thread is unfollowed (`subscriptions.thread.remove`), a
  conversation is marked read up to `newest`. Undo follows the thread
  again (`subscriptions.thread.add`).
- `s` snooze: `defer_until` set, cursor untouched. A message from someone
  else newer than the snooze voids `defer_until`; the card is back as
  "needs reply".
- `f` file: a `slack` node for the unit is created under the heading the
  user picks (or the existing one moved there); the cursor is untouched,
  the card keeps being dealt. Filing is a place, not a close.
- mark read before a cutoff (2.13): every unit's `handled_through :=
  max(handled_through, cutoff)`, plus Slack's own read cursors as today.
- `u` undo: the previous cursor and defer are restored from the unit's
  verdict log, a local grow-only log that exists for undo and the journal.

The card is derived, never stored: a unit is open iff
`newest_from_other > handled_through` and `defer_until` is unset,
reached, or voided. It reads "needs reply" when `newest_author` is
someone else and "replied" when it is the user; the wait is measured from
`newest_from_other` in the first case and from the user's reply in the
second. The user's own reply is not a verdict: it flips the word and the
curve and the card stays open until the user closes it. Reading is not a
verdict either, in rho or on another client. A dealt card lands the
reader on the oldest message from someone else after `handled_through`.

**Why:** the first model keyed every card on a message timestamp and, for
a direct message, made every top-level message its own thread. A history
page loading under a done card could then raise an older message as a new
card, and mark-read-before left cards standing for the same reason
(checklist 2.17, 4 Sep, real use). A unit with one cursor cannot regress:
whatever Slack sends, the only comparison is "is there something from
them past the cursor", and the cursor only ever moves by the user's hand.
The dealer, its curves, and its verdict keys stay exactly what they are
for agents; Slack is another source of the same card.

### A Slack card outranks an agent of the same wait

Both are someone waiting on the user. A thread that needs a reply takes
the agent blocked curve with a higher head start: 1.1 where an agent
starts at 1.0, rising at the same 12 per day. An agent the user just
spoke to keeps its recency bonus (1.5 at the moment of the user's message
or opening its surface, gone within the hour), so it still deals first;
past that, equal waits favour Slack. A thread the user has replied
to takes the agent fyi curve: 0 at the reply, falling a third per day,
under the queue floor after three days, so it is dealt only when nothing
else is waiting and fades on its own if they never answer. A reply from
them re-raises it as "needs reply" from a fresh start.

**Why:** the user asked for Slack above agents (3 Sep), with a gap small
enough that an agent that just spoke still leads. A coworker's wait costs
more than an agent's, but not so much more that Slack should always win.

### State lives in Slack wherever Slack has a place for it

Which threads are the user's is Slack's follow list
(`subscriptions.thread.getView` on connect, `thread_subscribed` and
`thread_unsubscribed` live). What has been read is Slack's read cursors,
per conversation and per thread (`subscriptions.thread.mark`,
`thread_marked`). Ignoring a thread is Slack's unfollow. Rho keeps no
private copy of any of these; the mirror caches them and Slack corrects
it. Only what Slack has no place for, the dealer's cursor, defer, and
pace per unit, lives in rho, in the Slack store beside the mirror, on
the device that made the verdict. Verdicts are not mirrored into Slack's
Later (saved items): the user's call, 3 Sep.

Between rho devices this state does not sync yet: a done on the laptop
does not close the card on the phone until the phone's user closes it
too. Accepted on 4 Sep as a later decision; the shape of the store (three
typed cells per unit key) does not change when a sync is chosen.

**Why:** the user's rule, 3 Sep. Slack's own clients on the phone and the
desktop share this state already; a private copy in rho would drift the
moment the user touched another client, which is exactly what made a
thread reply vanish (checklist 2.16). Slack was in the tree for one day
(slices 2 and 3) so its verdicts would ride the tree's CRDT; the user
asked on 4 Sep why Slack was in the tree at all, and the answer was only
that sync. A per-unit record does not need to be a node to sync, and a
node per thread cost a machine-owned identity next to Slack's own, a
bind round trip, and rows the user never filed.

### Ingest: the activity feed for truth, the websocket for latency

Catch-up and deduplication come from polling `activity.feed`, the endpoint
the web client's Activity view uses: mentions, thread replies, reactions,
with an unread-only view and cursor paging. The websocket delivers the same
things live. Only mentions, direct messages, and threads the user has
posted in ever become cards; channel traffic never does. From the fifty
odd websocket event types, rho handles `hello`, `reconnect_url`, `pong`,
`message` (including thread replies), `thread`, `channel_marked` and
`im_marked` (read elsewhere, so clear the unread badge; the card stays).

**Why:** the feed is a stable, paged list of exactly the things that
matter, so a missed websocket frame is never a missed mention. The
websocket exists only so the lamp lights within a second.

### Slack deals straight from the Slack store; there is no inbox and no node in between

The dealer takes Slack cards from the Slack store the way it takes agent
cards from the registry: it asks the model for open units and ranks them
with the same curves. Nothing about a unit lives in the tree. There is no
`thread` node, no bind request to the daemon, and the daemon never hears
of Slack. Card identity in the dealer, its skips, undo, and the journal is
therefore an enum, a node or a Slack unit, not a node id.

The tree meets Slack in one place, by the user's hand: a `slack` node, a
reference node like `agent` and `page`, carrying the unit's identity and
nothing else. Todo and file create one; "notes for this" from a
conversation surface creates one and puts the note under it. At most one
per unit: filing again moves it. The machine never creates one on its own:
a ping, a reply, a mark, or a restart never makes a node. Its title is the
unit's subject from the mirror, `j` opens the conversation, and children
under it are notes for that thread. The Slack card and the node never
touch each other after creation: done on the card moves the cursor,
closing the node is the user's own act, and a thread gone from Slack
leaves a node whose `j` opens nothing.

**Why:** the inbox was a redirection, and the thread node was the same
redirection one level up: a second identity with its own lifetime for a
thing Slack already identifies. Slack keeps the truth of what is waiting
and what was answered; rho keeps one cursor per unit and nothing else. A
heading with a Slack thread under it is useful and rare (the user, 4
Sep); a user-made `slack` node gives exactly that, with parents and
children like any node, without the tree holding anything the machine
created. A note with a link field was considered and rejected the same
day: a Slack thread in the tree is its own kind, not a note hack.

### A local mirror in rho-db, so reading never waits on Slack

Everything the client has ever fetched is kept on disk in a rho-db (redb)
file owned by the GUI, `~/.local/state/rho/slack.redb`, owner-only: users
and their avatar hashes, conversations and their labels, messages per
conversation in timestamp order with reactions, edits, and deletions
applied, thread replies under their parent, the activity cursor, per
conversation `last_read`; and the dealer's per-unit record
(`handled_through`, `defer_until`, `pace_days`) with its verdict log,
keyed by unit, in tables of its own.
Every surface renders from the mirror first and refreshes behind it: the
list and any conversation open instantly from disk, then the session
fetches only what is newer than the mirror's newest timestamp for that
conversation. A restart shows yesterday's Slack before the socket is up;
offline, all of it is readable. Sending while offline fails loudly into
the composer rather than queueing, for now.

The mirror fills only on demand. Nothing is prefetched: a conversation's
history is fetched when the user opens it, older pages when the reader
scrolls up to a gap, one page at a time, with no manual "load older",
and every update reaches the screen as an incremental edit of only the
messages that changed (a keyed transcript primitive shared with the rest
of rho), never a re-render of the conversation,
and the tail only for conversations that are open or that the feed named.
Rho never walks a workspace's history in the background, never fans out
over the conversation list, and never re-fetches what the mirror already
holds. The request pattern must look like a person reading, because an
unofficial client that bulk-pulls history is the kind Slack detects and
bans.

The budget, stated as a rule: rho's request volume must stay at or below
what Slack's own web client makes for a power user doing the same day.
Any fetch must correspond to something the web client would do for a user
action. Under that rule a ping named by the activity feed may trigger one
bounded fetch at ingest, the thread plus a small window of the channel
around the parent, because that is exactly what the web client fetches
when the user clicks the notification; rho only does it a little earlier,
so the deal renders from the mirror with no network wait. The bound is
two history calls (20 before the ping and 20 after, since Slack cannot
window both sides of a ts in one call) and one replies call per ping,
never a page back, never a second conversation. The user chose both-sided
context over the saved request.

Shape to borrow, from matrix-rust-sdk's event cache (cloned under
`~/src/matrix-rust-sdk`, `crates/matrix-sdk-common/src/linked_chunk` and
`crates/matrix-sdk/src/event_cache`): a conversation's history is a chain
of chunks with explicit gaps. A gap is a record, not an assumption: it
carries the cursor needed to fill it (for Slack, the `latest` timestamp
to page back from). Opening a conversation shows the newest chunk;
scrolling into the gap behind it fills it, one page at a time, so a
conversation reads as if it had always been whole; coming back after downtime appends the
live tail as a new chunk and, if it does not reach the cached newest
timestamp, leaves a gap between them rather than pretending continuity.
Every gap is drawn where it sits (`older messages not loaded`,
`newer messages not loaded`), never hidden between two runs; a ping's
prefetched window is a chunk of its own with gaps on both sides, and the
newest chunk stays loaded under it so live messages keep landing. A gap
below the reader fills forward the same way a gap above fills back: one
page per user action, and a stale conversation waits for the reader to
move before it spends a page. What counts as a user action is decided by
the cursor once per frame, not by scroll events: a vim motion moves the
cursor a frame before the view follows, autoscroll is never the reader,
and a page landing under the cursor cannot buy the next one.
Dedup by timestamp on every insert. Slack's model is simpler than
Matrix's (no encryption, no state events, a total order by `ts`), so the
port is the chunk-and-gap idea and the update stream to the view, not the
code. Deliberate simplification: chunks are derived, not stored. Messages
sit in one range-scannable table keyed by conversation and `ts`; only the
gaps are records, each carrying the cursor to fill it. A chunk is the run
between two gap records. Matrix stores chunk nodes because its timeline
has no total order; Slack's does, so stored chunks would be a second
source of truth. The beginning of history is recorded when a page returns
`has_more = false`, so paging back at the top is a no-op, never a request.

**Why:** Slack's own web client does exactly this (an IndexedDB mirror, so
boot is instant and history scrolls without a round trip). Rho's promise
is that the UI never waits on a remote, and the GitHub design already
takes the same shape. Read positions cache here too, one file and one
identity per unit; the dealer's per-unit record lives beside them.

### Channels, direct messages, and threads are all the same surface

A conversation surface is built the way the agent transcript is: a
read-only editor holding the rendered messages, vim motions and search
work inside it, a composer below it. The keys are in the table below, and
it enters the deal history like every other surface.
A picture goes with a message rather than instead of one: paste it, drop
it, or name it with the attach prompt, and a muted chip over the composer
says what is waiting; `enter` uploads it and the message comes back from
Slack with the picture on it.
Rewriting what was already sent puts the old text in the composer and
tints the message while the edit is open; posting it is `chat.update`. A
channel, a direct message, a group message, and a thread are the same
surface with a different source; opening a thread from a channel is
opening a child surface, and `ctrl-k` returns. Older history loads as the
user scrolls up. Reading a conversation in rho marks it read in Slack, so
the phone and other clients agree; read state is the unread badge and
nothing else, it never discharges a card.

**Why:** the transcript view is already the closest thing rho has to a
chat, and the point of rho is that everything is the same editor with the
same keys. One surface kind with four sources is also far less code than
four surfaces.

### A conversation list is the way in

A list surface shows the user's channels and direct messages, unread ones
first with the unread count, the rest by recency; a line per conversation.
Unread state comes from Slack, and the websocket keeps it current.

**Why:** the dealer only deals what the user is obliged to answer; the
rest of Slack is browsed, and browsing needs a list. It is the one piece of
Slack's own navigation worth keeping.

### The keys, in one table

Every Slack key, in one place. Anything not listed is vim: motions,
search, `G` to the end. Reading is the whole of the interface, so the
table is short on purpose, and each row below has an assertion in
`every_key_in_the_slack_table_is_bound`.

The list:

| Key | Does |
| --- | --- |
| `enter` | open the conversation under the cursor |
| `s` | narrow the list to what you type |
| `shift-n` | next conversation with something unread |
| `m` | mark read: asks for an age or a date, marks everything older |
| `q` | close the surface |

A conversation:

| Key | Does |
| --- | --- |
| `enter` | open the thread under the cursor, or the file link |
| `i` | go to the composer |
| `e` | rewrite your own message under the cursor |
| `s` | search the conversation |
| `shift-n` | next conversation with something unread |
| `ctrl-k` | out of a thread, back to the channel |
| `q` | close the surface |

The composer:

| Key | Does |
| --- | --- |
| `enter` | send |
| `shift-enter` | a second line |
| `up` | rewrite the last thing you said, when the composer is empty |
| `escape` | put back what the composer held, then normal mode |

`G` is vim's and is not bound here: it clears the `n new` count because it
puts the end of the conversation on screen, which is what reading them
means. With the completion menu open the composer's `enter`, `shift-enter`,
`up` and `escape` belong to the menu instead, so `enter` takes the name
being offered rather than posting half of it.

**Why:** a key table that lives in two places is a key table that is wrong
in one of them. This is the only one, and the test is what keeps it true.

### Block Kit renders to text

Messages arrive as blocks. They render to plain text with the rules ported
from `slack-block.el`: rich text sections, lists, quotes, code, user and
channel mentions resolved to names, links as text with the URL, files and
attachments as titles. Nothing interactive in the first version.

**Why:** the reader wants the message, not the layout; text is what the
editor can search and the user can yank.

### Silence never looks like quiet

Reconnect with backoff. If the websocket has been down for more than a
few minutes, or a feed poll has failed repeatedly, a notice goes to the
messages surface and the lamp lights; when the connection returns, one
catch-up poll fills any gap before the lamp clears.

**Why:** a Slack that is dark must be distinguishable from a Slack with
nothing to say, or the user starts checking the other app again.

### The journal sees Slack the way it sees agents

Typed events: connected, disconnected, item ingested, replied, each with
the thread identity. No strings where an enum will do.

## What stays the same for the human

- The dealer, verdict keys, deal history, filing.
- The tree owns structure; a Slack unit appears in it only as a `slack`
  node the user made.

## Deliberately deferred

- Automatic token and cookie extraction from the embedded browser.
- Dialogs and modals rendered as forms in the editor.
- Reactions, emoji, file upload, message editing and deletion, presence,
  workspace-wide search.
- Scopes and per-heading keyword filters.

## Symptoms to watch for

- Cards for channel chatter the user was not addressed in.
- A thread dealt twice because the feed and the websocket disagreed.
- Ids or raw timestamps visible anywhere.
- A dark connection with no lamp.
- A card keyed on a message timestamp, or a card per message.
- A fact (`newest`, `newest_from_other`) that moved backwards.
- A tree node the machine created for Slack.

## What done means

A mention lands as a card within a second, a reply from the thread surface
flips it to replied and drops it down the queue, `d` closes it, a later
answer from them brings it back, a dropped connection is
visible and heals itself, and a normal day of Slack, reading channels and
direct messages, answering, opening threads, happens without launching the
Slack app.
