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

Rho state, per unit, as facts on the unit's id in the store
(`STORE-DESIGN.md`): `handled_through`, a timestamp cursor; `defer_until`;
`snoozed_at`, where the unit stood when the snooze was made; `pace_days`.
Nothing but a verdict key moves them.

- `d` done: `handled_through := newest`.
- `t` todo: done, and a note whose `parent` is the unit is created at the
  area asked, deferred and paced the way todo notes are today; `j` on the
  unit opens the conversation.
- `x` mute (was discard): `handled_through := newest` and `state := muted`,
  and the unit is silenced where Slack has a place for it: a thread is
  unfollowed (`subscriptions.thread.remove`), a conversation is marked read
  up to `newest`. The state is the difference from `d`: a done is "up to
  here", so the next message is news again, and a mute is "not this unit",
  so nothing arriving in it is. Opening the unit clears the state, and only
  that does; the cursor is left alone, so what was read stays read. Undo
  follows the thread again (`subscriptions.thread.add`).
- `s` snooze: `defer_until` set and `snoozed_at := newest`, cursor
  untouched, so the messages the user has not handled are still theirs when
  the snooze ends. A message from someone else newer than `snoozed_at`
  arrived during the snooze and voids `defer_until`; the card is back as
  "needs reply". Without `snoozed_at` the view cannot tell that message
  from one that was already sitting there when the snooze was made.
- `f` file: the unit's `parent` is set to the id the user picks (a label
  or any thing); the cursor is untouched, the card keeps being dealt.
  Filing is a place, not a close.
- mark read before a cutoff (2.13): every unit's `handled_through :=
  max(handled_through, cutoff)`, plus Slack's own read cursors as today.
- `u` undo: the previous cursor and defer are restored from the verdict
  log entry, as for any id.

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

### How a Slack unit sits in rho (8 Sep, operative)

Settled with the user. Where this disagrees with the sections above and
below it, this is what holds; the sections it corrects are named as it
goes.

**The word.** The thing is a *Slack unit*: a thread, a channel, or a
direct message. One word in the docs and one word in the code, and the
card is one unit -- a channel with three unhandled mentions is one card,
as it already was.

**A unit is a virtual node.** It is identified by its Slack ids and by
nothing rho writes. It becomes a real node in the store the moment a cell
is written that Slack has no place for: a snooze, a name, labels, About.
Nothing is written on attention, on opening a unit, or on leaving one.
The write of units as desk sources on attention goes. So the rho cells a
unit can carry are snooze, name, labels and About, and a unit that has
none of them exists only as long as Slack says it does.

**Done is a cursor, and the cursor is the later of two.** rho keeps a
local cursor per unit; Slack keeps a read mark. What has been dealt with
is the later of the two, so reading on the phone counts and pressing `d`
in rho counts. `d` advances rho's local cursor at once, and an outbox on
the action journal pushes the read mark to Slack when it can; a push that
has not been confirmed is stale, not a fault, because the other half of
the join already says the right thing on this machine. The local cursor
lives in the client's own local database, not in the CRDT: it is a
position in someone else's stream, not a fact about a thing rho owns. The
user's own reply advances the cursor, wherever they wrote it. rho never
writes Slack's read mark on leaving a conversation -- that write goes.
This replaces `handled_through` as a store cell in the section above.

Landed 8 Sep. Where the local cursor lives: the Slack mirror
(`slack.redb`), beside Slack's own mark, ruled by en1p -- local, the
crate's own, and the one file that already holds the other half. The
outbox is a second cursor per unit in the same file, "how far Slack has
been told": a unit whose cursor is past it is a push that has not
happened, retried at the next start, so a workspace that was offline
still pushes when it comes back. The store's old `SlackHandledThrough`
cells are seeded into it once, at the first start that has both, marked
in the mirror and never read again; nothing deletes them and the seed
pushes nothing to Slack. Undo puts both halves back: rho's cursor, and
Slack's mark pushed back to where it stood, because rho is what moved it
-- a mark moving backwards is what "mark unread" is, and this is the only
place rho asks for one. A done no longer crosses machines through the
store; what crosses is Slack's mark, which is what the outbox is for.

**Mute is Slack's.** A muted channel or direct message is muted in Slack,
and a thread the user is done with is unfollowed in Slack. There is no rho
mute cell for a unit, and following is Slack's too. A unit muted in Slack
makes no card and no attention.

**There is no watch.** The watch opt-in, its `w` key and its
`WatchedChannel` attention reason are deleted, and with them the mirror's
record of which channels were opted into. Every channel with unread
traffic from someone else is a unit with attention of its own, on a curve
below a direct message or a thread the user is in, and lower again once
anyone else has replied in it -- somebody is already answering. A mention
in a channel keeps the mention's priority; a channel muted in Slack makes
nothing. Per-channel priority, if it is ever wanted, is a later question.

**Nothing is owed for a message the user answered.** The "replied" state
and its fading curve are deleted. A unit the user has answered has no
attention at all until someone answers back; what is waiting is what
somebody else said last.

**Attention keeps its reasons**, now four without the watch: a mention, a
direct message, a reply in a followed thread, and unread channel traffic.
The mirror stays what it is, a local cache on disk; nothing in it enters
the store.

**The unit nodes already in the store.** The dealer simply stops reading
unit source nodes that carry no rho-only cell, and nothing deletes them.
A stale one is exactly that: a node whose id is a Slack unit and which
carries no snooze, name, label or About. The alternative, a marker-gated
one-shot that removes them, buys a little space at the cost of a
destructive migration that has to be right on every device the CRDT
reaches, and that races the user giving one of those units a name on
another machine the same day. Leaving them is idempotent, costs nothing
to write, and can be done later against the definition above if the space
ever matters. `SlackSnoozedAt` goes with eng-8gpr's pending removal.

Landed 8 Sep: `compute_nodes` in `rho-gui/src/desk_view.rs` drops a stored
`Id::Slack` row that neither the mirror is asking about nor carries a cell
of rho's own — `rho_wrote_of_unit` names them: a filing, a name, labels,
About, a snooze, a mute, a dismissal. A cursor cell is not one, so the
`SlackHandledThrough` rows the versions before today wrote leave nothing on
the map. Nothing deletes them and no pass looks for them: the walk that
already reads every fact reads one field more. The write of units as desk
sources on attention was already gone with the card rule; what is left of
it is the unused `SlackThreadBound` journal event, kept because the journal
decodes old files by variant order.

**Why:** every one of these takes a fact rho was keeping and gives it back
to whoever owns it. Slack owns what has been read, what is muted and what
is followed, and rho was keeping private copies that drifted the moment
the user touched another client. What rho owns is what Slack has no place
for -- a snooze, a name, a label, a note -- and that is exactly what makes
a unit worth a node. The watch was rho asking the user to tell it
something Slack already knows: which channels they are in and which they
muted.

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
pace per unit, lives in rho, as facts on the unit's id in the store,
which already syncs between rho devices. Verdicts are not mirrored into
Slack's Later (saved items): the user's call, 3 Sep.

**Why:** the user's rule, 3 Sep. Slack's own clients on the phone and the
desktop share this state already; a private copy in rho would drift the
moment the user touched another client, which is exactly what made a
thread reply vanish (checklist 2.16). Slack was in the tree for one day
(slices 2 and 3) so its verdicts would ride the tree's CRDT; the user
asked on 4 Sep why Slack was in the tree at all, and the answer was only
that sync. The store design keeps the sync and drops the node: the unit's
own identity is the id, and the cursor is a fact on it.

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

A restart is a third source of the same messages, and like the other two it
may only raise facts. The feed is a cursor, so a mention it has already
passed is never reported again; the mirror still holds the message, so at
startup the units are derived from the mirror's own history (checklist
2.19): every conversation it knows and every followed thread is walked and
the model decides which messages are the user's. It costs no request. The
walk runs twice, once before the network answers and once after the
followed list is in, because a reply in a thread nobody has yet said is
followed is channel traffic; the dedup makes the second pass free.

### Slack deals straight from the mirror; there is no inbox and no node in between

The dealer takes Slack cards the way it takes agent cards: the mirror
supplies the open units and their facts, the store supplies the user's
facts on the same ids, and the join is ranked with the same curves. No
node is ever created for a unit, there is no bind request to the daemon,
and the daemon never hears of Slack. Card identity in the dealer, its
skips, undo, and the journal is the typed `Id`.

The unit's id is a place like any other: a note with `parent` = the unit
is "notes for this thread"; the unit filed under a label or a project is
its `parent`; on the map it sits under its channel and workspace by the
derived edges until the user files it. None of that makes the machine
write anything: a ping, a reply, a mark, or a restart never touches the
store. Done on the card moves the cursor and nothing else; a note under
the thread is closed by the user's own act; a thread gone from Slack
leaves an id whose `j` opens nothing.

**Why:** the inbox was a redirection, and the thread node was the same
redirection one level up: a second identity with its own lifetime for a
thing Slack already identifies. Slack keeps the truth of what is waiting
and what was answered; rho keeps one cursor per unit and nothing else. A
heading with a Slack thread under it is useful and rare (the user, 4
Sep); the unit's own id as a place gives exactly that, with parents and
children like anything else, without the store holding anything the
machine wrote. A note with a link field and a user-made `slack` node kind
were both considered and rejected the same day, for the store design.

### A local mirror in rho-db, so reading never waits on Slack

Everything the client has ever fetched is kept on disk in a rho-db (redb)
file owned by the GUI, `~/.local/state/rho/slack.redb`, owner-only: users
and their avatar hashes, conversations and their labels, messages per
conversation in timestamp order with reactions, edits, and deletions
applied, thread replies under their parent, the activity cursor, per
conversation `last_read`. The dealer's cursor, defer, and pace are not
here: they are facts on the unit's id in the store.
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
identity per unit; the dealer's facts do not, they are the store's.

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
| `r` | react to the message under the cursor: a menu of emoji |
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

`r` opens a transient rather than acting: what is already on the message
comes first, so joining a reaction is one key, and a row for one you have
already put on says "remove", because the key is one state and not two.
Then the emoji you reached for most recently, then `/` for any emoji by
name. On the list `r` is vim's, because a list has no message to react to.

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

### A conversation is the transcript's document, in the transcript's markdown

The conversation surface renders through the primitives the agent
transcript renders through, and owns none of its own. Concretely:

- The buffer is markdown, configured by the host's own pipeline
  (`Hooks::configure_markdown`, which is `rho_window::markdown::configure_buffer`
  under the GUI). The parse styles emphasis, code, quotes, lists and links
  and conceals their markers, exactly as it does for an assistant turn.
- Slack speaks `mrkdwn`, which is the same ideas in different characters, so
  a message is converted at the block: `*bold*` to `**bold**`, `_italic_` to
  `*italic*`, `~struck~` to `~~struck~~`, `<url|text>` to `[text](url)`,
  mentions and channel refs to names, code and fences as they are. That
  conversion is `rho-slack`'s `markdown` module and the one place the two
  markups are told apart; `block.rs` resolves the ids, links and lists the
  same way for both.
- A message is one block of the document, keyed by its `ts`. A turn is named
  the way the transcript names one: the sender and the time. One line of
  speech reads `name: what they said  time`; words the parse would read as a
  block of their own — a list, a quote, a fence, a table — cannot begin after
  a name, so there the name and time are a line of their own and the words
  start under them at the margin. Nothing is indented into place.
- The day break and the unread line are the document's own headings, whose
  markers the parse hides.
- What came with a message rather than being it — an attachment's card, a
  link preview, a file — is marked with the gutter bar the transcript puts
  beside the user's own message, and starts at the margin like everything
  else. No bar is drawn into the text and no tint is painted behind it.
- What the surface still paints for itself is only what the parse cannot
  know: who is speaking, when, a file's caption, the reader's own mention,
  and the tint on the message a card or a search sent them to.

**Why:** the transcript has been solving this problem well for longer, and a
second renderer means a second set of bugs, a second theme to keep in step,
and markup painted by hand that the parse already understands. It also means
the concealment, the folds and the gutter come to Slack for free as they
land in `rho-window`.

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

### A narrowing lasts as long as the reader is in it

The finder narrows the list to the names that answer what was typed. What
should be true of that narrowing when the reader walks away from the list
and comes back, and what should be true of it when it stops matching
anything, are two questions the code answers today without having decided
either.

**What is true now, observed rather than assumed.** Narrowing to `ops`,
opening a conversation from the narrowed list, and coming back with
`ctrl-k` leaves the list showing `#dev-ops` and `#ops-alerts` and the
banner still reading `matching "ops" · 2 of 6`. The narrowing survives the
visit, because the query lives in the model rather than in the view, and
the model outlives every surface drawn from it. It does not survive a
restart: no table holds the query, so a fresh run starts on the whole
list.

**Both of those are right, and this is the decision that says so.** A
narrowing is the reader in the middle of something -- they typed `ops` to
get to the ops channels, and going into one of them and back out is the
middle of that, not the end of it. Widening the list under them on the way
back would put a conversation where they left one and hand the same
keypress a different meaning, which is the failure the point-on-a-
conversation rule already exists to prevent. Across a restart the opposite
holds: rho coming up showing two of six conversations, with a banner
explaining a word the reader typed yesterday, is rho hiding their Slack
from them. A narrowing is a motion, not a setting, and motions do not
outlive the run.

**What follows from keeping it, and is wrong today.** `shift-n` walks
`Model::next_unread`, which reads the whole list and knows nothing about
the query. With a narrowing standing, the next-unread key takes the reader
to a conversation the list is not showing and cannot show, and the banner
then describes a list they are no longer in. Keeping the narrowing means
`shift-n` walks what is on screen: the narrowed vector when a query
stands, the whole list otherwise. That is the reader's own rule -- the key
moves them through the list they are looking at -- and it costs the
matches rather than the workspace, since the narrowed set is already held
in list order.

**A narrowing that no longer matches anything.** Today the list draws no
rows, a banner reading `matching "zzz" · 0 of 6`, and a line saying
`nothing matches`. That is the right shape and it stays: an empty
narrowing must never widen itself back to the whole list. The reader's
query is theirs until they change it, and a list that silently became a
different list is worse than an empty one -- the count on the banner
already says the six conversations are still there.

What has to be added is the difference between the two ways of arriving
there. Typing a word nothing answers and watching the last channel that
answered your word get renamed, archived, or left are the same screen now,
and they are not the same fact. When a narrowing empties because the
mirror moved rather than because of the last keystroke, the line says so
-- that what matched has gone, not that nothing ever did -- and it names
the way out, since `s` and an empty query is the only way back and nothing
on screen says it.

**Why:** the narrowing is the one piece of state on this surface the
reader put there by typing. Everything else on the list -- what is unread,
what is muted, where a row sits -- is Slack's, and rho redraws it as it
changes. State the reader typed is cleared by the reader, and state Slack
owns is never allowed to clear it for them.

### Searching what people said

The finder narrows the conversation *list* by name; nothing finds a
message. "What did dana say about the staging rollback" is a daily
question, and today it is answered by opening the other app, which is the
one thing this design exists to stop.

**A hit is a place, not a thing.** A hit is a workspace, a channel, an
optional thread and a timestamp -- exactly the argument `Session::open_at`
already takes. It is not a unit, not a card, never filed and never in the
store: the rule that a message timestamp is a fact about a unit and not an
identity holds here too. A result list is a way of getting somewhere, and
it is thrown away once the reader has arrived.

**A hit opens where a ping opens.** Landing the reader on a message rho
may hold nothing around is solved already: `prefetch_ping` fetches a
bounded window either side of the pinging message, `mirror_island` records
that the window is an island with unknown history under it, and `open_at`
opens the chunk that contains it. A search hit is a ping the reader raised
themselves and takes the same road: the conversation surface, the chunk
around the message, the point on its row, the row marked so the reader can
see which one matched. One thing has to change -- `open_at` returns
silently today when the mirror holds nothing at that timestamp, and a hit
from the server is precisely that case, so it gains the window fetch the
ping path already has.

Results are their own surface, the same shape as the conversation list: a
read-only buffer, one row per hit, the point on the first one, `enter`
opens it, `escape` comes back to the results. A row is the author's name,
the conversation's name, the day, and the line that matched -- no ids, no
timestamps, nothing that says "you".

**Hits come from Slack, not from the mirror.** `search.messages` on the
same session, paged, which `slack-search.el` has used for a decade. The
mirror is not searched and not indexed, and the measurements are why.
Measured on a synthetic mirror of 250,000 messages over 200 conversations,
twelve words each, release build, file warm:

- The mirror is 128 MiB. One pass over it, decoding every message and
  matching, is 290 ms. Per keystroke that is a pass over the mirror, which
  the cost rule forbids outright.
- A word index in memory, the shape the name index uses, is 3.0M postings:
  279 MiB of resident memory, 4.6 s to build at startup. It costs more RAM
  than the whole mirror costs disk.
- A word index on disk costs 257 MiB beside the 128 MiB of messages, and
  twelve inserts per message on the socket path (mean 323 us per message,
  worst 579 us). Lookup is honest for a whole word (43 us) and for a long
  prefix (143 us for 1,505 postings), but a two-letter prefix reaches
  1.5M postings in 167 ms -- so even with the index, per keystroke is a
  pass over the index.
- And the mirror holds only what rho has paged in. A search of it answers
  "what have I already read", which is not the question asked.

**What it costs per keystroke: nothing.** This search is submit-then-ask,
not narrow-as-you-type. The finder narrows on every keystroke because it
is answered from an index of a few thousand short names held in memory;
neither half of that is true of messages. The message prompt offers no
candidates while typing, reads nothing, and asks nothing until `enter`. A
query still in flight when a second is submitted is dropped, so the
results shown are the last query's and never an older one arriving late. A
query that fails says so on one line and leaves the results that were
there.

**The token has to be allowed to search.** The session holds what the
register prompt was given and nothing else: the desktop client's own
`xoxc` user token and the `d` cookie that authenticates it, the same pair
every other call in this design already carries. `search.messages` is a
user-token method behind `search:read`, which a desktop session normally
has because the web client searches with it; when it does not, Slack
answers `ok: false` with `missing_scope` or `not_allowed_token_type`, and
the reader sees one line -- that this Slack session is not allowed to
search, and that a fresh token and cookie will fix it. Nothing else
changes: the search fails, no other call is affected, and the lamp is not
lit, because the connection is fine and only this method was refused.
Getting a session that may search is the user's act in their browser and
their registration prompt; rho neither inspects, refreshes, nor repairs a
credential, and this design adds no exception to that.

**A results row is spans, like every other Slack surface.** The buffer is
an editor over laid-out lines, so a hit is `Vec<Span>` through `lay_out`
and `apply_highlights` exactly as a conversation row is. Two lines per
hit: a header of the author's name (`Class::Sender`, or `Class::You` when
it is the reader's own), two spaces, the conversation's name
(`Class::Conversation`), two spaces, the day from `when_label`
(`Class::Time`); then the matched message's own line, rendered by the
same block walker the transcript uses, so a hit reads the way it will
read when it opens. The words the query asked for are not styled here --
the reader typed them and the row is a place, not a diff -- and the
message the reader lands on when the hit opens takes a tint of its own,
a sibling of `Class::Dealt` rather than `Dealt` itself, because "what I
searched for" and "what rho is asking me to answer" are two different
reasons for a message to be lit and a reader should not have to guess
which one they are looking at.

The query goes to Slack as the reader typed it. `from:@dana staging` works
without rho knowing what `from:` means, because Slack parses its own
modifiers -- the same reason blocks are rendered rather than reinterpreted.

**Why:** the workspace's history is Slack's, not rho's, and a client that
answers "find it" from its own partial copy answers a different question
than the reader asked. Indexing that partial copy would cost more than the
copy, on both the memory the client holds and the write path every
incoming message crosses, to answer worse.

## The fake is a server, not a test double

`rho-fake-slack` is its own crate, a library and a binary. The fake lived
inside `rho-slack` for as long as it was a test double: a client library
carrying the server it is tested against, which means the server is only ever
as good as one test needs at a time, only one client can be pointed at it, and
nothing outside the crate pays its cost so nobody sees it. Out on its own the
goal changes: a workspace a client cannot tell from Slack, that several
clients connect to at once, that keeps living while they are connected.
`rho-slack` depends on it only for tests, so nothing about the fake can reach
the client.

**The surface is every method rho calls, in Slack's shapes.** Real cursor
pagination (`response_metadata.next_cursor`, `has_more`, `latest`/`oldest`/
`inclusive` meaning what they mean at Slack), and real error codes rather than
a 500: `invalid_auth`, `channel_not_found`, `not_in_channel`,
`message_not_found`, `already_reacted`, `no_reaction`, and `ratelimited` as a
429 with `Retry-After`, because a client that only reads the body never backs
off. The socket surface is held to the same standard — `hello`, `message` with
the subtypes rho reads, the reaction and mark events, `reconnect_url`, a
disconnect with a reason — and the failure paths stay first-class, since a
real server drops sockets and refuses calls and rho has to survive it.

**One typed store, and the counts derived from it.** Conversations, users,
messages, threads, reactions, and read cursors per person and per thread, as
types rather than JSON blobs; the wire shapes are made from them at the edge.
Unread counts are derived from the cursors and never stored beside them, so
the server cannot contradict itself — `client.counts` saying four while the
history shows three is a thing rho would then have to cope with, and Slack
does not do it. The cost rule holds here as in the client: a request costs the
rows it touches plus a lookup to place them, never a pass over the workspace.

**One seed is one world.** The generated workspace comes from eng-8gpr's
generator as a library call, so this crate owns no fixture data, and the
default is the scale that matters: 300 conversations and 450,000 messages,
long-tailed. The seed fixes the world *and* the traffic that arrives after it,
so a run replays.

**Time runs in it.** Other people post, reply in threads, react, edit and mark
read on a schedule taken off the same seed, at a configurable rate, so a
connected client sees a workspace that is alive rather than a fixture that is
frozen. It can be advanced explicitly as well as run in real time — a test
says "an hour passes" instead of sleeping through it — and it stops cleanly on
drop.

**Several clients, and the server checking them.** Each connected client is a
named user and the server knows what it told whom, which is what makes
consistency checkable from the one place that holds the whole truth: a message
visible to a conversation's members reached every connected member, read
cursors and unread counts agree between clients and with the store, and no
client was told something the store does not say. Violations come back as
typed observations, not log lines.

**Two transports, one vocabulary.** In-process from a test — start it, get an
API base and a socket URL, point one `rho-slack` session or several or a whole
GUI at it — and as a binary for the rig, where the control endpoint takes the
same typed actions the in-process handle takes.

## What stays the same for the human

- The dealer, verdict keys, deal history, filing.
- The store owns the user's facts; a Slack unit is a place in it only
  where the user filed something there or filed it somewhere.

## Deliberately deferred

- Automatic token and cookie extraction from the embedded browser.
- Dialogs and modals rendered as forms in the editor.
- Presence and typing indicators.
- Searching what people said, which is designed above and not yet built.
  Narrowing the conversation list by name is built.
- Searching the mirror while offline, and search modifiers rho understands
  itself rather than passing through.
- Scopes and per-heading keyword filters.

Reactions, emoji, file upload, message editing and deletion have all been
built and are no longer deferred.

## Symptoms to watch for

- Cards for channel chatter the user was not addressed in.
- A thread dealt twice because the feed and the websocket disagreed.
- Ids or raw timestamps visible anywhere.
- A dark connection with no lamp.
- A card keyed on a message timestamp, or a card per message.
- A fact (`newest`, `newest_from_other`) that moved backwards.
- Anything written to the store by a ping, a reply, a mark, or a restart.

## What done means

A mention lands as a card within a second, a reply from the thread surface
flips it to replied and drops it down the queue, `d` closes it, a later
answer from them brings it back, a dropped connection is
visible and heals itself, and a normal day of Slack, reading channels and
direct messages, answering, opening threads, happens without launching the
Slack app.
