# deal

The deal hands the user the one thing that wants them most, so they never scan a list for it. The user answers each card with a verdict, and the next one comes.

## bet The deal, not an inbox
instead of: a list of unread things the user scans from top to bottom and marks read.
serves: Know what wants them, and where they were.
because: between two places the user has a minute and no memory of what asked; a list makes them choose, a card makes them answer, and the product remembers the rest.
costs: whatever the user has not answered is behind the top card, and a verdict is a decision they must make.

## journey Back from an hour elsewhere
goal: the user clears what wants them and gets back to where they were.
starts when: the user looks up after an hour in a terminal or a page.
steps:
1. the user sees what wants them and how much, `home.Glance at what waits`
2. the user clears it, `Clear what waits`
3. the user goes back to where they were, as they left it, `window.Screen.back`
done when: nothing waits and the user's place is on screen as they left it.
day: Know what wants them, and where they were.

## concept Card
purpose: hand the user the one thing that wants them most, so they never scan a list.
principle: an agent's message in a thread asks the user something; the thread becomes a card; the user presses `space` then `n`, and the thread opens; they answer it and say done; the next card opens.
sees: where it is and its name, such as `rho › claude-tool-prompt-audit`; why it wants the user and since when, such as `asks · 2h`, `result · 5h ago`, `failed · 1h`, or `unread · needs reply · 0m`, the room being in the heading already; then the thing itself, open on screen.
has:
- one `chat.Thread`: an agent's thread that wants the user, or a Slack conversation or thread that Slack would badge for them
does:
- deal `space` then `n`, or `ctrl-j` from the newest screen, or a flick up on the phone: open the top card; the card in view, if any, goes behind the others for half an hour

A card exists for one of three reasons, and its words say which:
- an ask: an agent mentions the user because it needs something from them before its work can go on, such as `which repo should I use?`; the card reads `asks · 2h` and rises the longer it waits
- a result: an agent mentions the user with what they asked for, done; the card reads `result · 5h ago`; if the user does not look, it sinks and is gone after about three days
- a stop the agent did not choose: the agent failed, has retried for an hour, or its host lost it; the card reads `failed · 1h`, `stuck · 2h`, or `lost · 5m` on the first thread the agent joined, and outranks a result; steering the agent, writing in the thread, or `Verdict.done` clears it

In a channel's list of threads, a thread with a mention of the user waiting unread shows `@1` beside its name in yellow. A mention that is only for the record makes no card; the thread shows as unread on Home instead. A message that mentions nobody makes no card. From Slack, a direct message, a mention, and a reply in a followed thread make a card; see `slack.Answer Slack`.

The user answers a card with a `Verdict`.

## concept Verdict
purpose: let the user say once what a card means to them, so it goes away and rho remembers their answer.
principle: the user reads a card's thread and has nothing to add; they press `space` then `d`; the echo line reads `done: debug`, or `done: debug · nothing waits` on the last card, and the next card opens; a minute later they change their mind and press `shift-u`, and the card is back as it was.
sees: the verdict menu over the card, after `space`; the echo line naming the verdict and the card, such as `done: debug`.
does:
- done `space` then `d`, or `done` on the phone: say the user is through with everything in the card so far; the card goes away, and when it was on screen, the next one opens
- mute `space` then `m`, or `mute` on the phone: say nothing from it should reach the user again; the card goes away, and when it was on screen, the next one opens
- snooze `space` then `s` then a unit, with an optional count first: hide the card until then; minutes and hours count from now, days and weeks end at the start of that day
- next `space` then `n`: open the next card; with nothing waiting, open Home and echo `nothing waits`; `space` on a screen with no card opens a short menu headed `no card here`, with `u` undo and `n` next
- undo `shift-u`, or `space` then `u`: take back the last verdict, including what it did in Slack; the echo line reads `undid done: debug`; the card opens again as it was; with nothing to take back, the echo line reads `nothing to undo`

A verdict is about the card: the thread or conversation it stands for, and everything under it so far, wherever the cursor is in it, on the `with` line or in the box too. On a channel's list, a thread's row is that thread's card. A verdict about one message alone is a rare act, by name from the palette: `done with this message` and `mute this message`, offered only when the line under the cursor is a message that is not itself the card; the card stays.

What the user says about a card is kept on their own devices. Between devices that hold the user's twelve-word secret phrase, it passes through the hosts, sealed, so no host and no agent can read it; see `hosts.Device` and `hosts.Pair this device`. A device without the phrase keeps its verdicts to itself, and its status line says so.

## flow Clear what waits
runs: 50 a day
day: Know what wants them, and where they were.
when: the user has a few free minutes
want: see each thing that asks for the user, and answer it
so that: nothing waits on the user without their knowing
steps:
1. the user presses `space` then `n`; the top `Card` opens
2. the user reads it
3. the user answers in the thread if it needs words from them
4. the user gives `Verdict.done`, or `Verdict.snooze` and a time, or `Verdict.mute`; the next card opens
5. the user repeats from 2 until the deal is empty and `home.Home` is on screen
system: reads every host's chat and Slack, orders the cards, lets a result fade, lets a card come back when its snooze ends, and opens Home when nothing is left.
user: the verdict, the time for a snooze, and any words of answer.
cases: on the phone, where a snooze is one unit at a time; an ask that is weeks old from an agent that still exists; a card the user skipped that comes back half an hour later
reviewed: 2026-11-20 · laptop · reviewer · 4 findings

## rule writing in a thread reads it
when the user posts a message in a `chat.Thread`, then everything in it so far counts as seen, and its `Card` goes away until something new asks.

## rule opening is not reading
when the user opens a `chat.Thread` and leaves it without a `Verdict` or a message of their own, then its `Card` and its unread count stay as they were.

## rule done in rho is read in Slack
when `Verdict.done` on a card from Slack, then Slack marks it read through its newest message, so every other Slack app shows it as read too.

## rule mute in rho is mute in Slack
when `Verdict.mute` on a card from Slack, then Slack mutes that conversation or stops following that thread, and the user can lift it in any Slack app.

## rule undo takes back the Slack side too
when `Verdict.undo` takes back a done or a mute on a card from Slack, then Slack's read mark and mute go back to what they were.

## rule a gone agent asks nothing
when every agent of a `chat.Thread` is gone, then the thread makes no `Card`, whatever its messages asked.
