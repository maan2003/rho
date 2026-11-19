# slack

rho reads and writes the user's Slack itself, so Slack's asks come to the same deal as the agents' and the user answers them in the same window. Slack stays the source: what the user reads, mutes, stars, or follows here, Slack shows the same way in every other Slack app.

## concept Workspace
purpose: connect rho to one of the user's Slack workspaces.
principle: the user runs `register workspace…`, gives the name, the token, and the cookie from their browser; the echo line reads `slack: fake registered`; Slack's asks start to come in as cards.
sees: its name, as the tag on its rooms on Home.
does:
- register workspace…, from the palette under `slack`: ask `slack workspace:`, then `… xoxc token:`, then `… d cookie:`; the last two are hidden as they are typed

Giving a registered name again replaces its token and cookie. The window answers a wrong token with what was wrong, never with the token.

## concept Conversation
purpose: hold one Slack channel, group, or direct message, for reading and writing.
principle: a colleague writes to the user in a direct message; Home shows `@Barbara Liskov` near the top with `needs reply · 0m`; the user opens it, answers, and says done.
sees: its name, such as `#design-24`, `@Barbara Liskov`, or the names of a group; whether it has unread messages, in bold, with `@2` for mentions, a count of unread messages, or `•`; open, its messages with the writer's initials, the time, reactions such as `🎉 1`, `1 reply →` under a message with a thread, a line for each new day, a `new` line above the first unread message, and a box at the bottom such as `Message #infra-18…`.
has:
- many `Thread`
- many `Message`
does:
- open `enter` on its line, or `jump…` with `ctrl-p`: open it
- write `i`: put the cursor in the box; `enter` sends, `shift-enter` starts a new line, `up` in an empty box edits the user's last message, `escape` leaves the box
- star `z` in the `slack` menu: make it a room on Home, or stop
- attach file… `a` in the `slack` menu: ask for a file to send with the next message; `remove attachment…` takes one back
- next unread `shift-n`: open the next conversation with something unread
- new message… `ctrl-n`: ask `new message — people (comma separated):` and open a conversation with them

## concept Thread
purpose: hold the replies under one Slack message.
principle: a message in `#infra-18` shows `6 replies →`; the user opens it, reads the replies, and writes one in `Reply in #infra-18…`.
sees: open, the message and its replies, with `Reply in #channel…` as the box at the bottom.
does:
- open `enter` or a click on `N replies →`: open the thread
- follow thread `t` in the `slack` menu: let its replies make cards, or stop
- broadcast reply `b` in the `slack` menu: send replies to the channel too, or stop

## concept Message
purpose: carry one thing someone said in Slack, with what Slack lets the user do to it.
principle: a colleague posts a link the user wants later; the user puts the cursor on it and presses `b`; it is in `saved (local)` the next time they look.
sees: the writer's initials, the text with emoji and links drawn, reactions, files, and the replies under it.
does:
- message actions… `e` in the `slack` menu, over the message under the cursor: `edit` and `delete…` on the user's own, `react…`, `copy link`, `forward…`, `mark unread`, and `save / unsave for later (local)`
- edit `e`, react `r`, mark unread `m`, save for later `b`: the same, by key, on the message under the cursor

## concept Activity
purpose: list what Slack is asking the user about, in one place.
principle: the user opens `activity` from the `slack` menu; the two conversations that want them are listed with their newest message; `enter` opens one.
sees: each conversation or thread that asks, with `unread`, and its newest message under it.

## concept Saved
purpose: keep Slack messages the user wants to come back to, on this device only.
principle: the user saves a message with `b`; later they open `saved (local)` from the `slack` menu and find it there; Slack itself does not know.
sees: the saved messages, as `saved (local)` in the `slack` menu.

## screen Conversations
shows: `Conversation`
starts: `Answer Slack`, `Find something said in Slack`

Every conversation, one line each: the unread ones first, mentions before plain unreads, then by when they last moved; the muted ones are under a rule at the bottom. `s` narrows the list by name.

## screen Conversation
shows: `Conversation`, `Message`, `Thread`
starts: `Answer Slack`

When a conversation opens while it asks for the user, the cursor lands on the oldest message from someone else that the user has not dealt with.

## job Answer Slack
when: someone in Slack needs me
want: read what they said and reply, without leaving the window
so that: they are not kept waiting, and Slack shows it as read everywhere
flow:
1. the conversation is on screen, from the deal or from Home
2. read from the cursor down
3. press `i`, write the reply, and press `enter`
4. `deal.Card.done`; Slack marks it read, and the next card opens
system: makes a card of each direct message, mention, and reply in a followed thread; puts a small group's thread with the direct messages; lets plain channel traffic make only a small card that is gone within a day.
user: the reply, and the verdict.
cases: the reply is in a thread under a message; I read it in another Slack app first, and the card goes away by itself

## job Find something said in Slack
when: I remember something was said or shared in Slack
want: search for it
so that: I find the message or file without scrolling
flow:
1. press `shift-s` for messages or `ctrl-shift-s` for files, or choose `search messages…` or `search files…` in the `slack` menu
2. type the words and press `enter`; the results open
3. `[` and `]` move between pages of results
4. `enter` on a result opens it in its conversation
system: asks Slack, which holds the history.
user: the words.

## job Catch up after time away
when: I come back to a workspace after days, with hundreds of old unread messages
want: say that everything before a time is read
so that: only what is recent asks for me
flow:
1. choose `mark read before…` in the `slack` menu
2. answer `mark read before (7d):` with an age such as `6h`, or a date such as `2026-08-15`; the answer shows how many conversations and threads it covers
3. `enter`; everything up to that time counts as done, in Slack too; one `shift-u` takes it all back
system: counts what the answer covers before the user commits, and leaves whatever came after the time as it was.
user: the time.

## job Connect a Slack workspace
when: I set up a new device, or Slack has changed my token
want: give rho the workspace's token and cookie
so that: Slack's asks reach the deal again
flow:
1. open the palette and choose `slack › register workspace…`
2. answer `slack workspace:` with its name
3. paste the token at `… xoxc token:`
4. paste the cookie at `… d cookie:`; the echo line reads `slack: … registered`
system: completes the name from the workspaces already registered, and hides the token and cookie.
user: the token and the cookie, copied from a browser where they are signed in.

## rule a Slack conversation that asks is a room
when Slack would badge a `Conversation` for the user, or the user starred it, then it is a `home.Room` on Home, tagged with the `Workspace` name.
