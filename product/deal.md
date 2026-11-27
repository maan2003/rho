# deal

The deal hands the user the one thing that wants them most. The usual answer is a reply; when there is nothing to say, the user says done.

## bet The deal, not an inbox
instead of: a list of unread things the user scans from top to bottom and marks read.
serves: Know what wants them, and where they were.
because: between two places the user has a minute and no memory of what asked; dealing hands them the next thing to answer, and the product remembers the rest.
costs: the `desk.Desk` shows only the first cards; the rest are behind them.

## journey Back from an hour elsewhere
goal: the user clears what wants them and gets back to their work.
starts when: the user looks up after an hour in a terminal or a page.
steps:
1. the user sees what wants them and how much, `desk.Glance at what waits`
2. the user clears it, `Clear what waits`
3. the user types the name of the place they want in `window.Places` (`space` then `f`) or hits `tab`
done when: nothing waits and the user is back in the place they want, as they left it.
day: Know what wants them, and where they were.

## concept Card
purpose: put a message that wants the user in reach, without making them look through the whole thread.
principle: a project asks whether a test can use a separate clock; the deal opens the thread at its top `chat.Draft` and newest message, the user replies, and its card goes away.
sees: the `chat.Project` address, the `chat.Thread` subject, and since when it waits, `waits · 2h`, a stop's word `failed · 1h`, or `approve push · 4m`; open, the `chat.Message` that pings (or pending push/fetch approval at the top of the thread, or a due `notes.Note` reminder).
has:
- one `chat.Thread` (carrying any pinging `chat.Message`, stop, or staged privileged push/fetch from `agents.Approve a push or a fetch`) — or one due `notes.Note` reminder on the user's devices
does:
- deal `space-j` (or `ctrl-j` in `NOR`, or a flick up on the phone): open the top card matching the active dealer focus (strictly respecting `focus <project>` and `quiet <duration>` — during `quiet`, opens Desk or echoes `quiet <remaining>`). If a card is in view, it goes behind the others for half an hour, still counted, and deals last, and the echo says `skipped: Flaky clock test`. When the card in view is the only one waiting, it stays, nothing goes behind, and the echo says `only waiting card: Flaky clock test`. When nothing waits in focus, `desk.Desk` opens and the echo reads `nothing waits` (or `nothing waits · lab away` when a host is away, or `nothing waits in tide` when focused on a project)
- set dealer focus, from `window.Commands` (weighted aliases `focus`, `quiet`, `dnd`): scope the deal (`space-j`), lamp, and chime to `all` (default), `project <p>` (inline or context, e.g. `:focus tide` — ignores **other projects**, while personal due `notes.Note` reminders still pass through!), or `quiet <duration>` (e.g. `:quiet 45m` — holds everything until quiet ends). Active focus always shows in the Top-Right Global Status Bar on every screen (`focus tide`, `quiet 32m`). `clear dealer focus` (`focus off`, `unfocus`) returns to `all`.

The highest-priority card deals first: waiting cards rise steadily with age (`+0.5/h`), and a thread the user wrote in within the last hour gets a recency boost so answers to active work jump to the front; `desk.Desk` follows the same order, and every deal re-evaluates to pull the top card at that moment. Cached waiting cards from an `away` host stay in the count, on Desk (tagged `away`), and in normal deal order (`done` works locally on the device; replies queue), **except** they stay off the lamp and chime, and staged push approvals on an `away` host are refused (`<host> away · approve when it is back` — never queued). A stop is a message from the project, written by the system bot. It says why the work stopped, such as `failed · cargo exited 101 · last lines`. A card is answered with a reply or a `Verdict` (or `approve`/`deny` for a staged push/fetch).
rejected:
- strict oldest-first FIFO ordering · 2026-11-26 · put stale threads ahead of an agent that just answered active work; priority + recency puts what matters most in front of the user while aging prevents starvation
- `asks` and `result` as two kinds of card · 2026-11-20 · both wait the same way; the message either pings or it does not
- a standalone push-approval card screen separate from its `chat.Thread` · 2026-11-26 · keeping the pending push inside its originating thread preserves full context, keeps one card type, and lets `deny` carry reply text from the draft
- a card that drops out of the count while it is skipped · 2026-11-20 · a skipped card is still something left
- duplicating `deal` at `space` then `n` · 2026-11-26 · `space-j` (`ctrl-j`) already deals from anywhere, and `space` is the clean leader for `Places`, `undo`, and card verdicts
- a separate pre-deal return point or excluding dealt cards from `Places` MRU · 2026-11-26 · pure MRU is simpler and predictable

## rule a result already gone before results waited stays gone
Until 2026-11-20 a result faded and left the deal three days after it was written, with no verdict asked. A result that had already left by then counts as read, so the rule change does not bring months of old results back as cards. Asks and stops never faded, so they stay as they were.

## concept Verdict
status: Slack in the browser has no rho verdicts.
purpose: let the user say once what a card means to them, so rho remembers their answer.
principle: the user replies if they have words; otherwise they say done (or approve/deny on a staged push inside its thread). If they change their mind about a sealed verdict, they can take it back.
sees: silent `space` prefix chords on waiting cards (`space d` done, `space m` mute, `space s` → `:snooze thread ` / `:snooze note `, `space a` approve push, `space x` deny push, `space u` undo), every verdict listed with its shortcut in `window.Commands` (`:`), and an echo in the Right Ear naming the verdict and card, such as `done: Flaky clock test`. On the phone, the last `done` leads with `nothing waits · done: Flaky clock test`, so a long subject gives way first.
does:
- reply: send the user's words and settle the thread through its latest message in view, as in `chat.Answer a message`; the thread stays on screen, and the next card waits for `Card.deal`
- done `space` then `d` (or `done` in `window.Commands` — scoped strictly to a waiting card in view or its inspected transcript; elsewhere echoes `not waiting here`): the user has read through the latest message they had in view at the top of the thread and has nothing more to say; put the card away (only a newer message or edit deals a thread again), and if it was on screen, open the next card. A message or edit that came while the draft was stuck and the user read the history below is not read: it still waits and deals. Not offered while a staged push/fetch awaits approval
- mute `space` then `m` (or `mute` in `window.Commands` — works on any open `chat.Thread` without a pending push/fetch, even before a card arrives): truly silences everything from the thread (including later pushes) until unmuted; put any active card away, and if it was on screen, open the next one
- unmute: reopen the thread in `window.Places` (or filter with `muted:`); its title row says `muted` and `window.Commands` offers `unmute`. Restore any message that still waits without leaving the thread.
- approve `space` then `a` (or `approve push` in `window.Commands` — executes ONLY when the push's thread is on screen; off-thread the command row reads `open thread to approve` and `enter` opens the thread at the push; refused when the host is `away`): host-visible acting decision (not sealed, not undoable once executed); approve the staged push or fetch so it runs with the user's ssh login, clear the pending push item, and deal the next card
- deny `space` then `x` (or `deny push` in `window.Commands` — executes ONLY on the push's thread; never `space d`, so `done` muscle memory cannot accidentally deny a push): opens `:deny push ` in `Commands` prefilled with any current draft text visible as the reason so `enter` confirms (preventing stale drafts from firing un-undoably). Host-visible acting decision; refuses the staged push or fetch, sends the confirmed reason to the agent, clears the pending push item, and deals the next card. Agents may also withdraw their own staged push so a plain reply can resolve it
- snooze `space` then `s` (prefilling `:snooze thread ` — or `:snooze note ` on a `notes.Note` — in `window.Commands`): opens the upper-third `Commands` panel with duration presets (`1h`, `3h`, `evening`, `tomorrow`, `next week`, plus `now` / `clear` when already snoozed) and live resolved timestamp previews as you type (`45m`, `2d`, `mon 9am`); hides the card (or schedules a sealed reminder on a `notes.Note`) until then (showing `snoozed until …` visibly in the thread or note title row; a reply arriving within one hour of the user's own message still breaks through an active thread snooze)
- unsnooze (`unsnooze thread` / `unsnooze note`, aliases `wake`, `clear reminder`, in `window.Commands`): wake a snoozed thread early or clear a scheduled note reminder without relying on `space u`
- undo `space` then `u` (or `undo` in `window.Commands`): take back the last sealed verdict (`done`, `mute`, `snooze`) and bring the card back as it was; with nothing to take back, the echo line reads `nothing to undo`. Does not undo executed push/fetch `approve`/`deny` decisions or sent messages. Inside a draft, `shift-u` redoes the last edit.
rejected:
- a nested snooze sub-menu with count-digit prefixes (`space s 3 h`) · 2026-11-27 · `space s` opening `:snooze thread ` / `:snooze note ` in `Commands` gives live resolved timestamp previews, natural time input, and one unified command UI with zero separate `Menu` primitive
- binding `deny` to `space` then `d` on push approvals · 2026-11-26 · collided with 50×/day `done` muscle memory; `space x` prevents accidental denials
- `space u` undoing a just-sent idea or reply · 2026-11-26 · the user tells the agent or runs `cancel agent request` rather than overloading verdict undo

Sealed verdicts (`done`, `mute`, `snooze`) are kept on the user's own devices. Between devices that hold the user's twelve-word secret phrase, they pass through attached hosts, sealed, so no host and no agent can read them; see `hosts.Device` and `hosts.Pair this device`. A device without the phrase keeps its verdicts to itself, and its status line says so. Acting decisions (`approve`, `deny`) are sent directly to the host so it can execute or refuse the staged git transport.

## flow Clear what waits
runs: 50 a day
day: Know what wants them, and where they were.
when: the user has a few minutes and wants to clear what asks for them
want: open each thing that needs them and either answer it or say what to do with it
so that: nothing waits on the user without their knowing
steps:
1. the user deals the next `Card` with `space-j` (or `enter` on `desk.Desk`, or `open` on the phone's `desk.Desk`)
2. the user reads the message, staged push, or due note reminder where the card opened
3. the user replies, `chat.Answer a message`
   - without a reply, they give `Verdict.done`, `Verdict.snooze` with a time, or `Verdict.mute` (or `Verdict.approve` `space a` / `Verdict.deny` `space x` on a staged push); on the phone, `done` takes one tap on the open card and deals the next
4. the user continues until nothing waits in focus and the `desk.Desk` is on screen
system: orders cards from messages that ping, stops, staged privileged actions inside their threads, and due device-only note reminders; snoozed cards can come back; respects active dealer focus.
user: the reply or verdict, and the time for a snooze.
cases: a result from days ago that has not been read; a stop reported as message; a staged push to `main` inside its thread; a snoozed card or note reminder returns; a host is away (cached cards stay answerable).

## rule a reply settles the card
when the user replies to a `chat.Thread` that waits (without an unresolved staged push/fetch), then the thread is settled through the latest message visible when they replied, and its `Card` goes away. The reply is a message in the thread, read by every agent in it; only a newer message or edit after that deals the thread again.

## rule opening is not reading
when the user opens a `chat.Thread` and leaves it without a `Verdict` or a reply, then its `Card` stays as it was.
