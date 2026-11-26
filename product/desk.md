# desk

The desk answers what waits. It is a glance, not a place to work: the deal walks the cards, and the idea key opens the draft.

## journey Switching projects
goal: the user picks up a different project where they left off.
starts when: the user decides to work on a different project now.
steps:
1. the user sees what waits on the desk, `Glance at what waits`
2. the user opens the thread by project name and subject in the same query, `window.Go to something by name`
3. the user picks up the thread, with their draft still there, `chat.Answer a message`, `chat.Draft`
done when: the user knows what waits and can carry on where they left off, drafts and all.
day: Know what wants them, and where they were.

## concept Desk
purpose: show what wants the user, and where to put the next idea.
principle: the user comes back after lunch, sees three things waiting, and deals the first.
sees: `N wait` and up to seven `deal.Card` rows, in the deal's order, each a subject and a project, with the thread's latest `chat.Flash` while it is fresh, then `and N more` for the rest.
does:
- open `tab`: show the desk, as `window.Screen`
- deal `space`: open the first waiting card
- open a row `enter` or one tap: open the selected card; the waiting overflow opens the first hidden card
- write an idea `i` on any row: open the idea draft, as `chat.Write an idea`
rejected:
- a narrowed Desk screen per `chat.Project` · 2026-11-26 · Desk is one global glance of what waits; filtering by project belongs in `window.Palette`
- deleting `Desk` and staying on the previous screen across cold returns · 2026-11-26 · after ten minutes idle the user's mental context has flushed; a calm home buffer screen is easier to return to than a stale mid-file view, and `ctrl-k` still returns to the previous place in one key

## screen Desk
answers: what needs the user now?
shows:
- `Desk`: the count of everything that waits
- `deal.Card`: up to seven waiting rows, in deal order, each its subject and project, plus the thread's latest flash as quiet context while it is fresh; nothing on a row says why or since when, the deal says that when it opens the card. Repeated stops in one project share a row at the first stop's place. The overflow names what it hides, such as `and 2 more · clock`
states:
- empty: `0 wait`; no rows
- loading: not agreed
- partial: not agreed
- error: not agreed
- full: the waiting rows and the overflow
keys:
- `space`: deal the first card, `Desk.deal`; with no card waiting and a verdict to take back, open `deal.Verdict`
- `enter`: open the selected row, `Desk.open a row`
- `j` and `k`: move between waiting and overflow rows, skipping labels and blank lines; `gg`: first row; `G`: last row
- `i`: `chat.Write an idea`
- `tab`: `window.Screen`
- `:`: `window.Palette`, which leaves out what waits here, since it is already in view
starts: `Glance at what waits`, `deal.Clear what waits`, `chat.Write an idea`, `window.Go to something by name`
captures: [the desk on the laptop](design/laptop-desk.png) and [on the phone](design/phone-desk.png).
rejected:
- `asks · 2h` and `result · 8m` words and ages on the rows · 2026-11-20 · the Desk is a glance; the deal says why and since when, one card at a time
- an unread mark on what changed since the last glance · 2026-11-20 · the same reason
- a draft tail at the bottom · 2026-11-20 · the idea key opens the draft, see `chat.Write an idea`
- `moving`, up to ten threads that are not waiting with the project's latest words · 2026-11-21 · the owner reads what waits and nothing else at a glance; what moves is one name away in `window.Palette`, where a thread row still shows the project's latest words

On the phone, rows keep their place while the Desk is on screen, so a row does not move under the thumb; a new order shows when the Desk opens again.

## flow Glance at what waits
runs: 30 a day
day: Know what wants them, and where they were.
when: the user has a moment or comes back after a while
want: see what asks for them without opening a thread
so that: they can choose whether to answer, or carry on where they were
steps:
1. the user opens the `Desk` with `tab`, or sees it on a cold return
2. the user reads what waits
system: counts and ranks waiting cards.
user: what, if anything, to open.
cases: nothing waits; more than seven cards; a cold return.

## rule a cold return opens the desk
when the user returns within ten minutes away, then their `window.Screen` is where they left it; after more than ten minutes away, the `Desk` is on screen. Away begins when the window loses the keyboard or the device goes idle.

## rule work has words before the agent writes any
when an agent works in a `chat.Thread` and the project has not written since the user did, then the thread's row in `window.Palette` reads `working`, or the first line of the agent's newest fresh `chat.Flash` when it has one; the word goes away when the agent stops or the thread waits.
