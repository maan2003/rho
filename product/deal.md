# deal

The deal hands the user the one thing that wants them most, so they never scan a list for it. The user answers each card with a verdict, and the next one comes.

## concept Card
purpose: hand the user the one thing that wants them most, so they never scan a list.
principle: an agent's message in a thread asks the user something; the thread becomes a card; the user presses `space` then `n`, and the thread opens; they answer it and say done; the next card opens.
sees: where it is and its name, such as `rho › claude-tool-prompt-audit`; why it wants the user and since when, such as `asks · 48.4d`, `result · 5.4h ago`, or `unread in @Barbara Liskov · needs reply · 0m`; then the thing itself, open on screen.
has:
- one `chat.Thread`, when an agent's message mentions the user
- one `slack.Conversation` or one `slack.Thread`, when Slack would badge it for the user
does:
- deal `space` then `n`, or `ctrl-j` from the newest screen, or a flick up on the phone: open the top card; the card in view, if any, goes behind the others for half an hour
- done `space` then `d`, or `done` on the phone: say the user is through with everything in it so far; the card goes away, and when it was on screen, the next one opens
- mute `space` then `m`, or `mute` on the phone: say nothing from it should reach the user again; the card goes away, and when it was on screen, the next one opens
- snooze `space` then `s` then a unit, with an optional count first: hide the card until then; minutes and hours count from now, days and weeks end at the start of that day
- undo `shift-u`, or `space` then `u`: take back the last verdict, including what it did in Slack; the card opens again as it was; with nothing to take back, the echo line reads `nothing to undo`

A verdict lands on what the cursor is on. On a thread that the deal opened, the cursor starts on the newest message, so the verdict menu and the `window.Status line` name that message rather than the card, until the user moves the cursor to the thread's first line.

What the user says about a card is kept on their own devices. Between devices that hold the user's twelve-word secret phrase, it passes through the hosts, sealed, so no host and no agent can read it. The window has no way to enter the phrase. A device without it keeps its verdicts to itself, and when another device has written verdicts, the echo line says so and names `space s s` as the way to enter the phrase, a key that opens the snooze times instead.

## concept Ask
purpose: say that an agent needs something from the user before its work can go on.
principle: an agent posts `which repo should I use?` and mentions the user as an ask; the thread's card reads `asks` and rises the longer it waits.
sees: the word `asks` and how long it has waited, on the card and on its room.

## concept Result
purpose: say that what the user asked for is done.
principle: an agent finishes and mentions the user with its result; the thread's card reads `result · 2h ago`; if the user does not look, it sinks and is gone after about three days.
sees: the word `result` and how long ago, on the card and on its room.

A mention that is only for the record makes no card; the thread shows as unread on Home instead. A message that mentions nobody makes no card.

## concept Lamp
purpose: tell the user something important waits, while they are looking at something else.
principle: the user is reading a long transcript; an ask comes in that matters; a dot lights at the right end of the `window.Status line`; they finish and deal.
sees: a yellow dot `●` at the right end of the top line.

When a card rises past a higher line, the window also chimes once.

## job Clear what waits
when: I have a few free minutes
want: see each thing that asks for me, and answer it
so that: nothing waits on me without my knowing
flow:
1. press `space` then `n`; the top `Card` opens
2. read it; move the cursor to the thread's first line if I want the verdict to cover the whole thread
3. answer in the thread if it needs words from me
4. `Card.done`, or `Card.snooze` and a time, or `Card.mute`; the next card opens
5. repeat from 2 until the deal is empty and `home.Home` is on screen
system: reads every host's chat and Slack, orders the cards, lets a result fade, lets a card come back when its snooze ends, and opens Home when nothing is left.
user: the verdict, the time for a snooze, and any words of answer.
cases: on the phone, where a snooze is one unit at a time; an ask that is weeks old from an agent that still exists; a card I skipped that comes back half an hour later

## rule writing in a thread reads it
when the user posts a message in a `chat.Thread`, then everything in it so far counts as seen, and its `Card` goes away until something new asks.

## rule opening is not reading
when the user opens a `chat.Thread` and leaves it without a verdict or a message of their own, then its `Card` and its unread count stay as they were.

## rule done in rho is read in Slack
when `Card.done` on a card from Slack, then Slack marks it read through its newest message, so every other Slack app shows it as read too.

## rule mute in rho is mute in Slack
when `Card.mute` on a card from Slack, then Slack mutes that conversation or stops following that thread, and the user can lift it in any Slack app.

## rule undo takes back the Slack side too
when `Card.undo` takes back a done or a mute on a card from Slack, then Slack's read mark and mute go back to what they were.

## rule a gone agent asks nothing
when every agent of a `chat.Thread` is gone, then the thread makes no `Card`, whatever its messages asked.
