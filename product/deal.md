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
status: new waiting mail after a reply comes before older unanswered mail in that thread. A stop reports a repeated error once with a count. The built card still reads `asks` or `result`.
purpose: put a mail that wants the user in reach, without making them look through the whole thread.
principle: a project asks whether a test can use a separate clock; the deal opens the thread on that mail, the user replies, and its card goes away.
sees: the `mail.Project` address, the `mail.Thread` subject, and since when it waits, `waits · 2h`, or a stop's word, `failed · 1h`; open, the `mail.Mail` that pings.
has:
- one `mail.Thread`
- one `mail.Mail` that pings the user, or reports a stop
does:
- deal `space` then `n`, `space` on the `desk.Desk`, `ctrl-j` from the newest screen, or a flick up on the phone: open the top card; if a card is in view, it goes behind the others for half an hour, still counted, and deals last

The longest-waiting card deals first; `desk.Desk` follows the same order. Nothing addressed to the user disappears unread. A stop is a mail from the project, written by the system bot. It says why the work stopped, such as `failed · cargo exited 101 · last lines`. A card is answered with a reply or a `Verdict`.
rejected:
- `asks` and `result` as two kinds of card · 2026-11-20 · both wait the same way; the mail either pings or it does not
- a card that drops out of the count while it is skipped · 2026-11-20 · a skipped card is still something left

## rule a result already gone before results waited stays gone
Until 2026-11-20 a result faded and left the deal three days after it was written, with no verdict asked. A result that had already left by then counts as read, so the rule change does not bring months of old results back as cards. Asks and stops never faded, so they stay as they were.

## concept Verdict
status: Slack in the browser has no rho verdicts.
purpose: let the user say once what a card means to them, so rho remembers their answer.
principle: the user replies if they have words; otherwise they say done. If they change their mind about a verdict, they can take it back.
sees: the menu of verdicts and an echo naming the verdict and card, such as `done: Flaky clock test`. On the phone, the last `done` leads with `nothing waits · done: Flaky clock test`, so a long subject gives way first.
does:
- reply: send the user's words and settle the card, as in `mail.Answer a mail`; the thread stays on screen, and the next card waits for `Card.deal`
- done `space` then `d`, or `done` on the phone: the user has nothing more to say; put away the card, and if it was on screen, open the next one
- mute `space` then `m`, or `mute` on the phone: nothing from it should reach the user again; put the card away, and if it was on screen, open the next one
- snooze `space` then `s` then a unit, with an optional count first: hide the card until then; each row names when that is, such as `1 hour · 17:26`, `tomorrow · 09:00`, or `next week · Mon 09:00`; a count typed first multiplies the row; the rows are the same on both devices, and there is no one-minute row
- next `space` then `n`: open the next card; `nothing waits` comes only when the `desk.Desk` would read `0 wait`, and then the Desk opens
- undo `space` then `u`: take back the last verdict and bring the card back as it was; with nothing to take back, the echo line reads `nothing to undo`. Inside a draft, `shift-u` redoes the last edit.

What the user says about a card is kept on their own devices. Between devices that hold the user's twelve-word secret phrase, it passes through the hosts, sealed, so no host and no agent can read it; see `hosts.Device` and `hosts.Pair this device`. A device without the phrase keeps its verdicts to itself, and its status line says so.

## flow Clear what waits
runs: 50 a day
day: Know what wants them, and where they were.
when: the user has a few minutes and wants to clear what asks for them
want: open each thing that needs them and either answer it or say what to do with it
so that: nothing waits on the user without their knowing
steps:
1. the user deals the next `Card` (`open` on the phone’s `desk.Desk`)
2. the user reads the mail where the card opened
3. the user replies, `mail.Answer a mail`
   - without a reply, they give `Verdict.done`, `Verdict.snooze` with a time, or `Verdict.mute`; on the phone, `done` takes one tap on the open card and deals the next
4. the user continues until nothing waits and the `desk.Desk` is on screen
system: orders cards from the mail that pings and from stops; snoozed cards can come back.
user: the reply or verdict, and the time for a snooze.
cases: a result from days ago that has not been read; a stop reported as mail; a snoozed card returns.
reviewed: 2026-11-20 · laptop · reviewer · 4 findings

## rule a reply settles the card
when the user replies to a `mail.Mail` that waits, then its `Card` goes away, and the reply goes to whoever wrote the mail.

## rule opening is not reading
when the user opens a `mail.Thread` and leaves it without a `Verdict` or a reply, then its `Card` stays as it was.
