# deal

The deal hands the user the one thing that wants them most. The usual answer is a reply; when there is nothing to say, the user says done.

## bet The deal, not an inbox
instead of: a list of unread things the user scans from top to bottom and marks read.
serves: Know what wants them, and where they were.
because: between two places the user has a minute and no memory of what asked; dealing hands them the next thing to answer, and the product remembers the rest.
costs: the `desk.Desk` shows only the first cards; the rest are behind them.

## journey Back from an hour elsewhere
goal: the user clears what wants them and gets back to where they were.
starts when: the user looks up after an hour in a terminal or a page.
steps:
1. the user sees what wants them and how much, `desk.Glance at what moves`
2. the user clears it, `Clear what waits`
3. the user goes back to where they were, as they left it, `window.Screen.back`
done when: nothing waits and the user's place is on screen as they left it.
day: Know what wants them, and where they were.

## concept Card
status: new waiting mail after a reply comes before older unanswered mail in that thread. A stop reports a repeated error once with a count.
purpose: put a mail that wants the user in reach, without making them look through the whole thread.
principle: a project asks whether a test can use a separate clock; the deal opens the thread on that mail, the user replies, and its card goes away.
sees: the `mail.Project` address, the `mail.Thread` subject, and why and since when it waits, such as `asks · 2h`, `result · 8m`, or `failed · 1h`; open, the `mail.Mail` that asks.
has:
- one `mail.Thread`
- one `mail.Mail` that asks for the user, gives a result, or reports a stop
does:
- deal `space` then `n`, `space` on the `desk.Desk`, `ctrl-j` from the newest screen, or a flick up on the phone: open the top card; if a card is in view, it goes behind the others for half an hour

A result waits like an ask; nothing addressed to the user disappears unread. A stop is a mail from the project, written by the system bot. It says why the work stopped, such as `failed · cargo exited 101 · last lines`. A card is answered with a reply or a `Verdict`.

## concept Verdict
status: Slack in the browser has no rho verdicts.
purpose: let the user say once what a card means to them, so rho remembers their answer.
principle: the user replies if they have words; otherwise they say done. If they change their mind about a verdict, they can take it back.
sees: the menu of verdicts and an echo naming the verdict and card, such as `done: Flaky clock test`.
does:
- reply: send the user's words and settle the card, as in `mail.Answer a mail`
- done `space` then `d`, or `done` on the phone: the user has nothing more to say; put away the card, and if it was on screen, open the next one
- mute `space` then `m`, or `mute` on the phone: nothing from it should reach the user again; put the card away, and if it was on screen, open the next one
- snooze `space` then `s` then a unit, with an optional count first: hide the card until then; minutes and hours count from now, days and weeks end at the start of that day
- next `space` then `n`: open the next card; with nothing waiting, open the `desk.Desk` and echo `nothing waits`
- undo `shift-u`, or `space` then `u`: take back the last verdict and bring the card back as it was; with nothing to take back, the echo line reads `nothing to undo`

What the user says about a card is kept on their own devices. Between devices that hold the user's twelve-word secret phrase, it passes through the hosts, sealed, so no host and no agent can read it; see `hosts.Device` and `hosts.Pair this device`. A device without the phrase keeps its verdicts to itself, and its status line says so.

## flow Clear what waits
runs: 50 a day
day: Know what wants them, and where they were.
when: the user has a few minutes and wants to clear what asks for them
want: open each thing that needs them and either answer it or say what to do with it
so that: nothing waits on the user without their knowing
steps:
1. the user deals the next `Card`
2. the user reads the mail where the card opened
3. the user replies, `mail.Answer a mail`
   - without a reply, they give `Verdict.done`, `Verdict.snooze` with a time, or `Verdict.mute`
4. the user continues until nothing waits and the `desk.Desk` is on screen
system: orders cards from project mail; asks, results, and stops wait; snoozed cards can come back.
user: the reply or verdict, and the time for a snooze.
cases: a result from days ago that has not been read; a stop reported as mail; a snoozed card returns.
reviewed: 2026-11-20 · laptop · reviewer · 4 findings

## rule a reply settles the card
when the user replies to a `mail.Mail` that waits, then its `Card` goes away, and the reply goes to whoever wrote the mail.

## rule opening is not reading
when the user opens a `mail.Thread` and leaves it without a `Verdict` or a reply, then its `Card` stays as it was.
