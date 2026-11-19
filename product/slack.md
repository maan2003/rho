# slack

rho reads and writes the user's Slack itself, so Slack's asks come to the same deal as the agents' and the user answers them in the same window. A Slack conversation is a `home.Room` and its messages are a `chat.Thread`, with what only Slack can do in the `slack` menu on the same thread or message. Slack stays the source: what the user reads, mutes, stars, or follows here, Slack shows the same way in every other Slack app.

## bet The window is the Slack client
instead of: a host relaying Slack, or Slack staying an app of its own.
serves: Answer the world.
because: the user answers Slack all day, and it must come to the same deal as the agents' asks; the window reads and writes Slack with the user's own token, and Slack stays the record of what is read.
costs: a token and a cookie copied from a browser, and what Slack's apps can do that the window cannot.

## concept Workspace
status: the `slack` menu also has `conversations`, a screen listing every conversation, unread first, with `s` to narrow it by name and the muted ones under a rule at the bottom, which shows `connecting to slack…` or `slack unavailable:` at its top; and `activity`, a list of each conversation or thread that asks, with its newest message under it.
purpose: connect rho to one of the user's Slack workspaces.
principle: the user runs `register workspace…`, gives the name, the token, and the cookie from their browser; the echo line reads `slack: acme registered`; Slack's asks start to come in as cards.
sees: its name, as the tag on its rooms on Home; on the echo line, `slack unavailable:` and the reason when Slack refuses it.
does:
- register workspace…, from the palette under `slack`: ask `slack workspace:`, then `… xoxc token:`, then `… d cookie:`; the token and the cookie show on screen as they are pasted, and rho's history of prompts keeps neither
- new message… `ctrl-n`: ask `new message — people (comma separated):` with one to eight people, and open a conversation with them
- browse channels… `c` in the `slack` menu: ask `browse channels — choose to join and open:` with the workspace's channels, and join and open the one chosen
- next unread `shift-n`: open the next conversation with something unread
- drafts… `d` in the `slack` menu: ask `slack drafts:` with every conversation that holds a `chat.Draft`, and open the one chosen at its box; with none, the echo line reads `slack: no drafts`
- search messages… `shift-s`, search files… `ctrl-shift-s`, in the `slack` menu: as `Find something said in Slack`
- mark read before… in the `slack` menu: as `Catch up after time away`

Giving a registered name again replaces its token and cookie. The window answers a wrong token with what was wrong, never with the token.

rho works with one workspace at a time: the first one registered. A second one registered is kept, but rho does not connect to it while the first one is there.

A short drop of the connection says nothing. After a long one, or when Slack keeps refusing, the echo line says what is wrong, such as `slack: connection lost`. When rho has caught up on what it missed, the echo line reads `slack: caught up`.

## job Answer Slack
day: Answer the world.
when: someone in Slack needs me
want: read what they said and reply, without leaving the window
so that: they are not kept waiting, and Slack shows it as read everywhere
flow:
1. the conversation is on screen, from the deal or from Home
2. read from the cursor down
3. press `i`, write the reply, and press `ctrl-enter`
4. `deal.Verdict.done`; Slack marks it read, and the next card opens
system: makes a card of each direct message, mention, and reply in a followed thread; puts a small group's thread with the direct messages; lets plain channel traffic make only a small card that is gone within a day.
user: the reply, and the verdict.
cases: the reply is in a thread under a message; I read it in another Slack app first, and the card goes away by itself

## job Find something said in Slack
day: Answer the world.
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
day: Answer the world.
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
day: Answer the world.
when: I set up a new device, or Slack has changed my token
want: give rho the workspace's token and cookie
so that: Slack's asks reach the deal again
flow:
1. open the palette and choose `slack › register workspace…`
2. answer `slack workspace:` with its name
3. paste the token at `… xoxc token:`
4. paste the cookie at `… d cookie:`; the echo line reads `slack: … registered`
system: completes the name from the workspaces already registered, and keeps the token and cookie out of rho's history of prompts.
user: the token and the cookie, copied from a browser where they are signed in.

## rule a Slack conversation that asks is a room
when the user starred a Slack conversation, or Slack would badge it for them, then it is a `home.Room` on Home, tagged with the `Workspace` name; a conversation that is neither is not.

## rule a snooze stays in rho
when `deal.Verdict.snooze` on a card from Slack, then Slack itself is not told, and the conversation stays unread in every other Slack app.
