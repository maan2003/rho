# slack

rho reads and writes the user's Slack itself, so Slack's asks come to the same deal as the agents' and the user answers them in the same window. A Slack conversation is a `home.Room` and its messages are a `chat.Thread`, with what only Slack can do in the `slack` menu on the same thread or message. Slack stays the source: what the user reads, mutes, stars, or follows here, Slack shows the same way in every other Slack app.

## bet The window is the Slack client
instead of: a host relaying Slack, or Slack staying an app of its own.
serves: Answer the world.
because: the user answers Slack all day, and it must come to the same deal as the agents' asks; the window reads and writes Slack with the user's own token, and Slack stays the record of what is read.
costs: a token and a cookie copied from a browser, and what Slack's apps can do that the window cannot.

## journey A colleague needs me
goal: the user answers their colleague without leaving the window.
starts when: someone in Slack writes to the user.
steps:
1. the user learns of it, `deal.Clear what waits`
2. the user reads what the colleague wrote, `Answer Slack`
3. the user replies in place, `Answer Slack`
4. the user says done; Slack shows it as read everywhere, `deal.Clear what waits`, `deal.done in rho is read in Slack`
   - if the user does not want the conversation to reach them again, they mute its card instead; Slack mutes the conversation too, `deal.Clear what waits`, `deal.mute in rho is mute in Slack`
   - if the user gave the wrong verdict, they undo it, `deal.Clear what waits`, `deal.Verdict.undo`, `deal.undo takes back the Slack side too`
done when: the colleague has their answer and nothing waits.
day: Answer the world.

## concept Workspace
purpose: connect rho to one of the user's Slack workspaces.
principle: the user runs `register workspace…`, gives the name, the token, and the cookie from their browser; the echo line reads `slack: acme registered`; Slack's asks start to come in as cards.
sees: its name, as the tag on its rooms on Home; on the echo line, `slack unavailable:` and the reason when Slack refuses it.
does:
- register workspace…, from the palette under `slack`: ask `slack workspace:`, then `… xoxc token:`, then `… d cookie:`; the token and the cookie show on screen as they are pasted, and rho's history of prompts keeps neither
- new message… `n` in the `slack` menu: ask `new message — people (comma separated):` with one to eight people, and open a conversation with them
- browse channels… `c` in the `slack` menu: ask `browse channels — choose to join and open:` with the workspace's channels, and join and open the one chosen
- next unread `shift-n` in the `slack` menu: open the next conversation with something unread; with nothing unread the echo line reads `slack: nothing unread`
- drafts… `d` in the `slack` menu: ask `slack drafts:` with every conversation that holds a `chat.Draft`, and open the one chosen at its box; with none, the echo line reads `slack: no drafts`
- search messages… `s`, search files… `f`, in the `slack` menu: as `Find something said in Slack`; started inside a conversation, the search covers that conversation only, and the prompt says so
- mark read before… in the `slack` menu: as `Catch up after time away`

Giving a registered name again replaces its token and cookie. The window answers a wrong token with what was wrong, never with the token.

rho works with one workspace at a time: the first one registered. A second one registered is kept, but rho does not connect to it while the first one is there.

A short drop of the connection says nothing. After a long one, or when Slack keeps refusing, the echo line says what is wrong, such as `slack: connection lost`. When rho has caught up on what it missed, the echo line reads `slack: caught up`.

## flow Answer Slack
runs: 50 a day
day: Answer the world.
when: someone in Slack needs the user
want: read what the person said and reply, without leaving the window
so that: the person is not kept waiting, and Slack shows it as read everywhere
steps:
1. the conversation is on screen, from the deal or from Home
2. the user reads from the cursor down
3. the user presses `i`, writes the reply, and presses `ctrl-enter`
4. the user says done with `deal.Verdict.done`; Slack marks it read, and the next card opens
system: makes a card of each direct message, mention, and reply in a followed thread; puts a small group's thread with the direct messages; lets plain channel traffic make only a small card that is gone within a day; dates a card from the message that asks, the newest from someone else; after a restart, deals exactly what Slack still holds unread, so a mark made in another Slack app while rho was off counts.
user: the reply, and the verdict.
cases: the reply is in a thread under a message; the user reads it in another Slack app first, and the card goes away by itself
reviewed: 2026-11-20 · laptop · reviewer · 6 findings

## flow Find something said in Slack
runs: 2 a day
day: Answer the world.
when: the user remembers something was said or shared in Slack
want: search for it
so that: the user finds the message or file without scrolling
steps:
1. the user chooses `search messages…` or `search files…` in the `slack` menu; inside a conversation, the search covers that conversation
2. the user types the words and presses `enter`; the results open
3. the user moves between pages of results with `[` and `]`
4. the user opens a result in its conversation with `enter`
system: asks Slack, which holds the history.
user: the words.
reviewed: 2026-11-20 · laptop · reviewer · 2 findings

## flow Catch up after time away
runs: 1 a week
day: Answer the world.
when: the user comes back to a workspace after days, with hundreds of old unread messages
want: say that everything before a time is read
so that: only what is recent asks for the user
steps:
1. the user chooses `mark read before…` in the `slack` menu
2. the user answers `mark read before (7d):` with an age such as `6h`, or a date such as `2026-08-15`; the answer shows how many conversations and threads it covers
3. the user presses `enter`; everything up to that time counts as done, in Slack too; one `shift-u` takes it all back
system: counts what the answer covers before the user commits, and leaves whatever came after the time as it was.
user: the time.
reviewed: 2026-11-20 · laptop · reviewer · 2 findings

## flow Connect a Slack workspace
runs: 1 a year
day: Answer the world.
when: the user sets up a new device, or Slack has changed their token
want: give rho the workspace's token and cookie
so that: Slack's asks reach the deal again
steps:
1. the user opens the palette and chooses `slack › register workspace…`
2. the user answers `slack workspace:` with its name
3. the user pastes the token at `… xoxc token:`
4. the user pastes the cookie at `… d cookie:`; the echo line reads `slack: … registered`
system: completes the name from the workspaces already registered, and keeps the token and cookie out of rho's history of prompts.
user: the token and the cookie, copied from a browser where they are signed in.
reviewed: 2026-11-20 · laptop · reviewer · 1 finding

## rule a Slack conversation that asks is a room
when the user starred a Slack conversation, or Slack would badge it for them, then it is a `home.Room` on Home, tagged with the `Workspace` name; a conversation that is neither is not.

## rule a snooze stays in rho
when `deal.Verdict.snooze` on a card from Slack, then Slack itself is not told, and the conversation stays unread in every other Slack app.
