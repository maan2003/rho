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
1. the user sees what wants them and how much, `desk.Glance at what waits`
2. the user clears it, `Clear what waits`
3. the user goes back to where they were, as they left it, via `window.Places` (`space` then `f`) or `tab`
done when: nothing waits and the user's place is on screen as they left it.
day: Know what wants them, and where they were.

## concept Card
purpose: put a message that wants the user in reach, without making them look through the whole thread.
principle: a project asks whether a test can use a separate clock; the deal opens the thread at its top `chat.Draft` and newest message, the user replies, and its card goes away.
sees: the `chat.Project` address, the `chat.Thread` subject, and since when it waits, `waits · 2h`, or a stop's word, `failed · 1h`; open, the `chat.Message` that pings or the staged privileged action.
has:
- one `chat.Thread` (or a staged privileged action from `agents.Approve a push or a fetch`)
- one `chat.Message` that pings the user, reports a stop, or asks approval for a staged push/fetch
does:
- deal `space-j` (or `ctrl-j` in `NOR`, or a flick up on the phone): open the top card; if a card is in view, it goes behind the others for half an hour, still counted, and deals last, and the echo says `skipped: Flaky clock test`. When the card in view is the only one waiting, it stays, nothing goes behind, and the echo says `only waiting card: Flaky clock test`. When nothing waits, `desk.Desk` opens and the echo reads `nothing waits` (or `nothing waits · lab offline` when a host is away)

The highest-priority card deals first: waiting cards rise steadily with age (`+0.5/h`), and a thread the user wrote in within the last hour gets a recency boost so answers to active work jump to the front; `desk.Desk` follows the same order, and every deal re-evaluates to pull the top card at that moment. Nothing addressed to the user disappears unread. A stop is a message from the project, written by the system bot. It says why the work stopped, such as `failed · cargo exited 101 · last lines`. A card is answered with a reply or a `Verdict`.
rejected:
- strict oldest-first FIFO ordering · 2026-11-26 · put stale threads ahead of an agent that just answered active work; priority + recency puts what matters most in front of the user while aging prevents starvation
- `asks` and `result` as two kinds of card · 2026-11-20 · both wait the same way; the message either pings or it does not
- a card that drops out of the count while it is skipped · 2026-11-20 · a skipped card is still something left
- duplicating `deal` at `space` then `n` · 2026-11-26 · `space-j` (`ctrl-j`) already deals from anywhere, and `space` is the clean leader for `Places`, `undo`, and card verdicts

## rule a result already gone before results waited stays gone
Until 2026-11-20 a result faded and left the deal three days after it was written, with no verdict asked. A result that had already left by then counts as read, so the rule change does not bring months of old results back as cards. Asks and stops never faded, so they stay as they were.

## concept Verdict
status: Slack in the browser has no rho verdicts.
purpose: let the user say once what a card means to them, so rho remembers their answer.
principle: the user replies if they have words; otherwise they say done (or approve/deny on a staged push card). If they change their mind about a verdict, they can take it back.
sees: the `space` leader `window.Menu` (showing card verdicts only on a waiting card screen, alongside `f` places and `u` undo) and an echo naming the verdict and card, such as `done: Flaky clock test`. On the phone, the last `done` leads with `nothing waits · done: Flaky clock test`, so a long subject gives way first.
does:
- reply: send the user's words and settle the thread through its latest message in view, as in `chat.Answer a message`; the thread stays on screen, and the next card waits for `Card.deal`
- done `space` then `d`, or `done` on the phone (thread cards): the user has read through the latest message they had in view at the top of the thread and has nothing more to say; put the thread's card away (only a newer message or edit deals it again), and if it was on screen, open the next card. A message or edit that came while the draft was stuck and the user read the history below is not read: it still waits and deals
- mute `space` then `m`, or `mute` on the phone (thread cards): nothing from it should reach the user until it is unmuted; put the card away, and if it was on screen, open the next one
- unmute: reopen the thread by name in `window.Places`; its heading says `muted` and its `space` menu and `window.Commands` offer `unmute`. Restore any message that still waits without leaving the thread.
- approve `space` then `a` (push/fetch action cards): approve the staged push or fetch so it runs with the user's ssh login, put the card away, and deal the next card; action cards take `approve` and `deny` in place of `done` and `mute`
- deny `space` then `d` (push/fetch action cards): refuse the staged push or fetch, put the card away, and deal the next card
- snooze `space` then `s` then a unit, with an optional count first (or `:snooze 3h` in `window.Commands`): hide the card until then (except that a reply arriving within one hour of the user's own message breaks through an active snooze); each row names when that is, such as `1 hour · 17:26`, `tomorrow · 09:00`, or `next week · Mon 09:00`; a count typed first multiplies the row; the rows are the same on both devices, and there is no one-minute row
- undo `space` then `u`: take back the last verdict and bring the card back as it was; with nothing to take back, the echo line reads `nothing to undo`. Inside a draft, `shift-u` redoes the last edit.

What the user says about a card is kept on their own devices. Between devices that hold the user's twelve-word secret phrase, it passes through the hosts, sealed, so no host and no agent can read it; see `hosts.Device` and `hosts.Pair this device`. A device without the phrase keeps its verdicts to itself, and its status line says so.

## flow Clear what waits
runs: 50 a day
day: Know what wants them, and where they were.
when: the user has a few minutes and wants to clear what asks for them
want: open each thing that needs them and either answer it or say what to do with it
so that: nothing waits on the user without their knowing
steps:
1. the user deals the next `Card` with `space-j` (or `enter` on `desk.Desk`, or `open` on the phone's `desk.Desk`)
2. the user reads the message or staged push where the card opened
3. the user replies, `chat.Answer a message`
   - without a reply, they give `Verdict.done`, `Verdict.snooze` with a time, or `Verdict.mute` (or `Verdict.approve` / `Verdict.deny` on a staged push); on the phone, `done` takes one tap on the open card and deals the next
4. the user continues until nothing waits and the `desk.Desk` is on screen
system: orders cards from the message that pings, from stops, and from staged privileged actions; snoozed cards can come back.
user: the reply or verdict, and the time for a snooze.
cases: a result from days ago that has not been read; a stop reported as message; a staged push to `main`; a snoozed card returns.

## rule a reply settles the card
when the user replies to a `chat.Thread` that waits, then the thread is settled through the latest message visible when they replied, and its `Card` goes away. The reply is a message in the thread, read by every agent in it; only a newer message or edit after that deals the thread again.

## rule opening is not reading
when the user opens a `chat.Thread` and leaves it without a `Verdict` or a reply, then its `Card` stays as it was.
