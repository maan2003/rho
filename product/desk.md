# desk

The desk is the user's calm home canvas: it gives ambient awareness of how much waits by visual size rather than a scannable list, hands out the next card on `space-j`, and takes a new thread directly when the user starts typing.

## journey Switching threads of work
goal: the user picks up a different thread where they left off.
starts when: the user decides to work on a different thread now.
steps:
1. the user lands on the desk for an ambient glance, `Glance at what waits`
2. the user opens the thread by tag or subject in `window.Places`, `window.Go to something by name`
3. the user picks up the thread, with their draft still there, `chat.Answer a message`, `chat.Draft`
done when: the user knows roughly what waits and can carry on where they left off, drafts and all.
day: Know what wants them, and where they were.

## concept Desk
status: the built desk currently renders a 7-row 92ch tabular list and opens a separate Idea screen on `i`; the ambient visual-size canvas and inline new-thread draft are agreed and queued for visual lab + build.
decided: 2026-11-28 · NOT a scannable inbox list or numbers-first table; Desk is a calm blank canvas combining (1) ambient awareness of how much waits by visual size/mass (plus urgent push/failure color cues and quiet `skipped` awareness) and (2) the live new-thread `chat.Draft` right on the canvas (`i` types directly on Desk with `:foo` / `:foo <value>` tokens, `ctrl-enter` starts the thread)
purpose: give an effortless peripheral sense of how much waits, deal the top card on `space-j`, and turn a fresh thought into a new thread right on the blank canvas.
principle: the user comes back after lunch, feels at a 200 ms glance whether the pending stack is light or heavy without scanning titles or numbers, and either hits `space-j` to deal the top card or presses `i` to start writing a new thread directly on the canvas.
sees: a calm blank canvas with ambient visual mass representing the pending waiting pile (exact visual treatment to be picked in a visual lab; urgent `approve push` or `failed` stops carry subtle accent weight, and skipped cards register quietly without inflating the active pile; `away` hosts and active dealer focus stay in the Top-Right Global Status Bar). When the user starts typing (`i`), the `chat.Draft` lives directly on this canvas; leaving via `space-j` or `tab` preserves the unsent draft on `Desk` until sent or cleared.
does:
- open `tab` (or `open desk` in `window.Commands`): show the desk, as `window.Screen`
- deal `space-j` (`ctrl-j`): open the top card in active dealer focus
- prefix `space`: silent `window.Screen` prefix (`space f` places, `space u` undo — never card verdicts)
- start a thread `i`, `A`, or `o`: write directly in the blank-canvas `chat.Draft` (supporting inline `:foo` and `:foo <value>` tokens with live completion); `ctrl-enter` sends it as a new `chat.Thread`, as `chat.Start a thread`, and clears the canvas back to calm ambient awareness
rejected:
- a scannable 7-row tabular waiting list on Desk · 2026-11-28 · Desk does not need to be scannable; scanning rows turned Desk into a mini inbox (`non-goal: A feed or inbox`) and forced every thread into rigid columns; ambient visual mass conveys backlog size at a glance while `space-j` deals
- a separate `Idea` composer screen with `to:` / `in:` / `on:` header fields · 2026-11-28 · typing directly on the blank-canvas Desk with inline `:tokens` removes both the separate screen and the header form
- a narrowed Desk screen per tag or project · 2026-11-26 · Desk is one global ambient home; filtering belongs in `window.Places`, and dealer scoping belongs in `deal.Card.set dealer focus`
- deleting `Desk` and staying on the previous screen across cold returns · 2026-11-28 · after ten minutes idle, landing on `Desk` ensures `space-j` always deals the next (or chimed) card instead of accidentally skipping the thread left open before stepping away
- immediate deal on bare `space` on `Desk` · 2026-11-26 · broke `space f` on `Desk`; `space-j` already deals in one gesture, keeping `space` a uniform prefix everywhere

## screen Desk
status: currently built as a view-only 92ch tabular list; migrating to the ambient visual-size canvas + live new-thread draft.
decided: 2026-11-28 · calm blank canvas with ambient visual mass for pending backlog size (not a scannable list) + direct new-thread `chat.Draft` on `i`; Top-Right Global Status Bar holds `away` hosts and active focus
answers: roughly how much waits right now, and ready to deal or write?
shows:
- top-left title row: `desk` (quietly noting `· N skipped` when cards are temporarily skipped, and counting from local cache while hosts connect)
- ambient visual mass representing the size and urgency of the waiting pile (never a scannable inbox table)
- the live `chat.Draft` on the blank canvas when typing or holding an unsent new-thread draft (muted teal with `2 px` teal left gutter bar, supporting inline `:foo` / `:foo <value>` completion)
states:
- empty: calm still canvas once every attached host answers and no draft is typed
- loading: reflects locally cached waiting mass while `connecting` shows calmly in the Top-Right Global Status Bar
- partial: waiting mass includes locally cached cards from `away` hosts (`<host> away` sits calmly in the Top-Right Global Status Bar)
- error: hosts that are `away` stay calm in the Top-Right Global Status Bar; hosts in `needs you` state (key touch / enrollment) surface in red in the Top-Right Global Status Bar and `list hosts`
- full: ambient waiting mass, plus the user's unsent new-thread draft if present
keys:
- `space-j` (`ctrl-j`): deal the first card in focus, `deal.Card.deal`
- `i`, `A`, `o`: write a new thread directly on the canvas, `chat.Start a thread` (`ctrl-enter` sends)
- `space`: silent `window.Screen` prefix (`space f` places, `space u` undo)
- `tab`: `window.Screen` (return to the previous place)
- `:`: `window.Commands`
starts: `Glance at what waits`, `deal.Clear what waits`, `chat.Start a thread`, `window.Go to something by name`
captures: [the desk on the laptop](design/laptop-desk.png) and [on the phone](design/phone-desk.png).
rejected:
- a 7-row scannable waiting table (`subject · tag · project · age`) · 2026-11-28 · replaced by ambient visual size + live blank-canvas draft
- an unread mark on what changed since the last glance · 2026-11-20 · the Desk is ambient; the deal orders what matters
- `moving`, up to ten threads that are not waiting with the latest words · 2026-11-21 · what moves is one name away in `window.Places`, where `working:` lists active threads

## flow Glance at what waits
runs: 30 a day
day: Know what wants them, and where they were.
when: the user has a moment or comes back after a while
want: sense roughly how much asks for them without reading a list or opening a thread
so that: they can deal the next card, start a new thread on the canvas, or carry on where they were
steps:
1. the user opens the `Desk` with `tab`, or lands on it on a cold return
2. the user senses the pending backlog size from the ambient canvas (and sees calmly if a host is away or connecting, or if dealer focus is active)
system: weighs waiting cards from live and cached host state without pretending nothing waits while hosts are away or still connecting.
user: whether to deal (`space-j`), write a new thread (`i`), or return (`tab` / `space f`).
cases: nothing waits; heavy pile; skipped cards held back; a cold return; one of two hosts is away; dealer focus is set to a tag.

## rule a cold return opens the desk
decided: 2026-11-28 · kept at 10m so returning after a chime lands on Desk where `space-j` always opens the chimed card instead of skipping the thread left on screen
when the user returns within ten minutes away, then their `window.Screen` is where they left it; after more than ten minutes away, the `Desk` is on screen. Away begins when the window loses the keyboard or the device goes idle.
