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

### A Slack thread is shaped like an agent

Identity is (workspace, channel, thread timestamp). A thread is "waiting on
you" when the last message is not yours, and "waiting on them" once you
replied. The state word and the curve follow from that; verdicts do not.
Only the user's keys set a verdict (done, discard, snooze, todo, filed).
The user's own reply is not a verdict: it flips the card to "replied" and
lets its priority fall, but the card stays open until the user closes it.
Reading is not a verdict either, in rho or on another client: it clears
unread and nothing more. A newer message from someone else after any
verdict brings the thread back, keyed on the latest message timestamp,
the same way an agent reply voids a skip.

Discard is also Slack's "ignore thread": `x` on a thread card unfollows it
in Slack (`subscriptions.thread.remove`), and a thread unfollowed in Slack
(`thread_unsubscribed` on the socket, or absent from the feed) is
discarded in rho. Rho keeps no subscription state of its own; Slack's
follow list is the truth and the two clients agree. Undoing a discard is
made in the same place: `shift-u` follows the thread again
(`subscriptions.thread.add`) and the card is dealt as before. An undo that
reopened the node but left the thread ignored would be worse than none.

**Why:** the dealer, its curves, verdict keys, deal history, and journal
already model exactly this for agents. Slack adds no new concept; it adds
another source of the same card. Reading and replying used to count as
done; the user rejected both (3 Sep): a reply is a move in a conversation,
not the end of it, and a thread read on the phone is still owed an answer.
Closing a thread is a decision only the user makes.

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
it. Only what Slack has no place for, the dealer's verdicts, lives in rho,
on the thread node, where the tree already syncs it between rho devices.
Verdicts are not mirrored into Slack's Later (saved items) either: the
user's call, 3 Sep; the tree's CRDT is the only sync for them.

**Why:** the user's rule, 3 Sep. Slack's own clients on the phone and the
desktop share this state already; a private copy in rho would drift the
moment the user touched another client, which is exactly what made a
thread reply vanish (checklist 2.16).

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

### Slack deals straight from the session; there is no inbox in between

A mention, a DM, or a reply to a thread the user is in becomes a dealer
candidate directly from the Slack session's own store, the way an agent
card comes from the registry. The card carries the thread identity and the
latest message timestamp; verdicts (done, discard, snooze, todo) are kept
client-side in the Slack store, keyed on thread plus latest timestamp, so a
verdicted thread stays quiet until a newer message from someone else voids
it; the user's own messages never touch the verdict. Filing (`f`)
under a heading creates a machine-owned thread node bound to that thread;
it updates in place and is removed when the thread has been quiet for a
while, like agent rows. The rho inbox is not involved: nothing is copied
into it and nothing is read back from it.

**Why:** the inbox was a redirection. Slack already keeps the truth of what
is waiting and what has been answered; copying it into an inbox item meant
a second identity, a second lifetime, and a surface that showed a message
body with no conversation around it. Human-entered items may want an inbox
of their own later; Slack does not.

### A local mirror in rho-db, so reading never waits on Slack

Everything the client has ever fetched is kept on disk in a rho-db (redb)
file owned by the GUI, `~/.local/state/rho/slack.redb`, owner-only: users
and their avatar hashes, conversations and their labels, messages per
conversation in timestamp order with reactions, edits, and deletions
applied, thread replies under their parent, the activity cursor, per
conversation `last_read`, and the verdict state from the section above.
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
takes the same shape. The mirror is also the only sane home for verdict
state and read positions: one file, one identity per thread.

### Channels, direct messages, and threads are all the same surface

A conversation surface is built the way the agent transcript is: a
read-only editor holding the rendered messages, vim motions and search
work inside it, a composer below it. `i` focuses the composer, Enter sends,
`q` closes, and it enters the deal history like every other surface.
A picture goes with a message rather than instead of one: paste it, drop
it, or name it with the attach prompt, and a muted chip over the composer
says what is waiting; `enter` uploads it and the message comes back from
Slack with the picture on it.
Rewriting what was already sent is `e` on the reader's own message (or
`up` on an empty composer, Slack's habit): the composer holds the old
text, the message is tinted while the edit is open, `enter` posts
`chat.update` and `escape` gives the composer back what it held. A
channel, a direct message, a group message, and a thread are the same
surface with a different source; opening a thread from a channel is
opening a child surface, and Space+K returns. Older history loads as the
user scrolls up. Reading a conversation in rho marks it read in Slack, so
the phone and other clients agree; read state is the unread badge and
nothing else, it never discharges a card.

**Why:** the transcript view is already the closest thing rho has to a
chat, and the point of rho is that everything is the same editor with the
same keys. One surface kind with four sources is also far less code than
four surfaces.

### A conversation list is the way in

A list surface shows the user's channels and direct messages, unread ones
first with the unread count, the rest by recency; a line per conversation,
Enter opens it, search narrows it. Unread state comes from Slack, and the
websocket keeps it current.

**Why:** the dealer only deals what the user is obliged to answer; the
rest of Slack is browsed, and browsing needs a list. It is the one piece of
Slack's own navigation worth keeping.

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
- The desk owns structure; Slack rows appear only where the user filed them.

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

## What done means

A mention lands as a card within a second, a reply from the thread surface
flips it to replied and drops it down the queue, `d` closes it, a later
answer from them brings it back, a dropped connection is
visible and heals itself, and a normal day of Slack, reading channels and
direct messages, answering, opening threads, happens without launching the
Slack app.
