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
sees: `N wait` (calmly noting any `away` or `connecting` host, or active dealer focus, in `quiet` beside the count, such as `3 wait · lab away`, `3 wait · focus tide`, or `0 wait · lab connecting`, because an away host is a normal everyday state) and up to seven `deal.Card` rows (including locally cached waiting cards from away hosts), in the deal's order, each a subject and a project, with the thread's latest `chat.Flash` while it is fresh, then `and N more` for the rest.
does:
- open `tab`: show the desk, as `window.Screen`
- deal `space-j` (`ctrl-j`), or `enter` on a row: open the top or selected card; the waiting overflow opens the first hidden card
- leader `space`: open the universal `window.Menu` (`f` places, `u` undo when undoable — never card verdicts)
- write an idea `i` (anywhere on Desk, including when empty): open the idea draft, as `chat.Write an idea`
rejected:
- a narrowed Desk screen per `chat.Project` · 2026-11-26 · Desk is one global glance of what waits; filtering by project belongs in `window.Places`, and scoping what deals belongs to `deal.Card.set dealer focus`
- deleting `Desk` and staying on the previous screen across cold returns · 2026-11-26 · after ten minutes idle the user's mental context has flushed; a calm home buffer screen is easier to return to than a stale mid-file view, and `tab` or `space f` + `enter` returns to the previous place immediately
- immediate deal on bare `space` on `Desk` · 2026-11-26 · broke `space f` on `Desk`; `space-j` and `enter` already deal in one gesture, keeping `space` a uniform leader everywhere

## screen Desk
answers: what needs the user now?
shows:
- `Desk`: the global count of everything that waits (including locally cached cards from away hosts), with any `away` or `connecting` host and any active dealer focus (`focus tide` / `quiet 45m`) noted quietly beside it
- `deal.Card`: up to seven waiting rows, in deal order, each its subject and project, plus the thread's latest flash as quiet context while it is fresh; nothing on a row says why or since when, the deal says that when it opens the card. Repeated stops in one project share a row at the first stop's place. The overflow names what it hides, such as `and 2 more · clock`
states:
- empty: `0 wait` (or `0 wait · lab away` when a host is away); no rows
- loading: `0 wait · lab connecting` (or `connecting to lab…` while every attached host is connecting and no cached cards exist); never flashes a bare `0 wait` while hosts are still connecting
- partial: `N wait · lab away` (or `N wait · 2 hosts away`), showing all waiting cards including locally cached ones from away hosts in calm `quiet` host tone
- error: hosts that are `away` are noted quietly beside the count (`lab away`); hosts in `needs you` state (key touch / enrollment) surface in `window.Status line` and `list hosts`
- full: the waiting rows and the overflow
keys:
- `space-j` (`ctrl-j`): deal the first card in focus, `deal.Card.deal`
- `enter`: open the selected row, `Desk.deal`
- `space`: universal `window.Menu` leader (`f` places, `u` undo when undoable)
- `j` and `k`: move between waiting and overflow rows, skipping blank lines; `gg`: first row; `G`: last row
- `i`: `chat.Write an idea`
- `tab`: `window.Screen` (return to the previous place)
- `:`: `window.Commands`
starts: `Glance at what waits`, `deal.Clear what waits`, `chat.Write an idea`, `window.Go to something by name`
captures: [the desk on the laptop](design/laptop-desk.png) and [on the phone](design/phone-desk.png).
rejected:
- `asks · 2h` and `result · 8m` words and ages on the rows · 2026-11-20 · the Desk is a glance; the deal says why and since when, one card at a time
- an unread mark on what changed since the last glance · 2026-11-20 · the same reason
- a draft tail at the bottom · 2026-11-20 · the idea key opens the draft, see `chat.Write an idea`
- `moving`, up to ten threads that are not waiting with the project's latest words · 2026-11-21 · the owner reads what waits and nothing else at a glance; what moves is one name away in `window.Places`, where `working:` lists active threads and each thread row shows the project's latest words

On the phone, rows keep their place while the Desk is on screen, so a row does not move under the thumb; a new order shows when the Desk opens again.

## flow Glance at what waits
runs: 30 a day
day: Know what wants them, and where they were.
when: the user has a moment or comes back after a while
want: see what asks for them without opening a thread
so that: they can choose whether to answer, or carry on where they were
steps:
1. the user opens the `Desk` with `tab`, or sees it on a cold return
2. the user reads what waits (and sees calmly if a host is away or connecting, or if dealer focus is active)
system: counts and ranks waiting cards (keeping locally cached cards from away hosts) without pretending `0 wait` while hosts are away or still connecting.
user: what, if anything, to open.
cases: nothing waits; more than seven cards; a cold return; one of two hosts is away; dealer focus is set to one project.

## rule a cold return opens the desk
when the user returns within ten minutes away, then their `window.Screen` is where they left it; after more than ten minutes away, the `Desk` is on screen. Away begins when the window loses the keyboard or the device goes idle.
