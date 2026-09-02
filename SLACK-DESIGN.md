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
replied. Your own reply is the done verdict, automatically. A newer reply
after any verdict brings the thread back, keyed on the latest message
timestamp, the same way an agent reply voids a skip.

**Why:** the dealer, its curves, verdict keys, deal history, and journal
already model exactly this for agents. Slack adds no new concept; it adds
another source of the same card.

### Ingest: the activity feed for truth, the websocket for latency

Catch-up and deduplication come from polling `activity.feed`, the endpoint
the web client's Activity view uses: mentions, thread replies, reactions,
with an unread-only view and cursor paging. The websocket delivers the same
things live. Only mentions, direct messages, and threads the user has
posted in ever become cards; channel traffic never does. From the fifty
odd websocket event types, rho handles `hello`, `reconnect_url`, `pong`,
`message` (including thread replies), `thread`, `channel_marked` and
`im_marked` (read elsewhere, so quiet the card).

**Why:** the feed is a stable, paged list of exactly the things that
matter, so a missed websocket frame is never a missed mention. The
websocket exists only so the lamp lights within a second.

### Items land in the inbox, then file into the desk

A mention becomes an inbox obligation with a typed Slack source (workspace,
channel, timestamp), so it rises immediately under the obligation curve.
Filing it (`f`) under a heading creates a machine-owned thread node bound
to that thread; it updates in place and is removed when the thread has
been quiet for a while, like agent rows.

**Why:** the inbox is where new things wait for a verdict and the desk is
where the user's structure lives; Slack should not write into the desk on
its own, and a filed thread should be as much a first-class row as an
agent.

### The thread surface is a real editor

A dealt or opened thread is a surface built the way the agent transcript
is: a read-only editor holding the rendered thread, vim motions and search
work inside it, a composer below it. `i` focuses the composer, Enter sends
a reply into the thread, `q` closes. It enters the deal history like every
other surface.

**Why:** the transcript view is already the closest thing rho has to a
chat, and the point of rho is that everything is the same editor with the
same keys.

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

- The dealer, verdict keys, deal history, inbox, filing.
- The desk owns structure; Slack rows appear only where the user filed them.

## Deliberately deferred

- Automatic token and cookie extraction from the embedded browser.
- Dialogs and modals rendered as forms in the editor.
- Reactions, sending new messages outside a thread, search, presence,
  browsing unread channels.
- Scopes and per-heading keyword filters.

## Symptoms to watch for

- Cards for channel chatter the user was not addressed in.
- A thread dealt twice because the feed and the websocket disagreed.
- Ids or raw timestamps visible anywhere.
- A dark connection with no lamp.

## What done means

A mention lands as a card within a second, a reply from the thread surface
marks it done, a later answer brings it back, a dropped connection is
visible and heals itself, and reading a thread in rho feels like reading a
transcript.
