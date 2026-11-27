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
decided: 2026-11-27 · pure view-only glance (NO text cursor, NO editor buffer, NO row selection); count (`desk · 13 wait`) sits in Desk's top-left title row; row 1 is Card 1 with no subheader; clean 1-line tabular sheet inside a 92ch table on the title's left edge, 14px below the top row (subject · exception tag in its colour · project · quiet right age); away hosts live in the Top-Right Global Status Bar only (never repeated inside Desk)
purpose: show what wants the user, and where to put the next idea.
principle: the user comes back after lunch, glances at three things waiting in a crisp view-only tabular sheet starting on row 1, and hits `space-j` to deal the first.
sees: in Desk's own top-left title row, `desk` and `N wait` (counted from local cache while hosts connect — never flashing a false `0 wait`; `away` hosts and active dealer focus stay in the Top-Right Global Status Bar and are never repeated inside the Desk body); in a `92 ch` centered table whose left edge the title row shares, starting at row 1 after the `14 px` top gap (no subheader), up to seven 1-line tabular `deal.Card` rows in deal order (including locally cached waiting cards from away hosts), each showing subject, optional exception tag (`approve push` in `attention`, a stop such as `failed` or `3 stopped` in `failed`, `away` in `quiet`), right-aligned `quiet` project address, and right-aligned `quiet` age, then `and N more · …` quietly below for the rest. Pure view-only glance — no cursor and no row selection.
does:
- open `tab` (or `open desk` in `window.Commands`): show the desk, as `window.Screen`
- deal `space-j` (`ctrl-j`), or click/tap a row: open the top card in active dealer focus (or the clicked/tapped card)
- prefix `space`: silent `window.Screen` prefix (`space f` places, `space u` undo — never card verdicts)
- write an idea `i` (anywhere on Desk, including when empty): open the idea draft, as `chat.Write an idea`
rejected:
- making Desk an interactive Helix editor buffer with a cursor or UI row-selection highlight/preview · 2026-11-27 · Desk is strictly a calm view-only glance; you read what waits and hit `space-j`, `i`, `space f`, `tab`, or `:`
- repeating `away` host notes inside the Desk body or adding a subheader/key-hint bar above row 1 · 2026-11-27 · count sits in Desk's top-left title row and `away` hosts / dealer focus sit in the Top-Right Global Status Bar; row 1 starts directly with Card 1
- a narrowed Desk screen per `chat.Project` · 2026-11-26 · Desk is one global glance of what waits; filtering by project belongs in `window.Places`, and scoping what deals belongs to `deal.Card.set dealer focus`
- deleting `Desk` and staying on the previous screen across cold returns · 2026-11-26 · after ten minutes idle the user's mental context has flushed; a calm home buffer screen is easier to return to than a stale mid-file view, and `tab` or `space f` returns to the previous place immediately
- immediate deal on bare `space` on `Desk` · 2026-11-26 · broke `space f` on `Desk`; `space-j` already deals in one gesture, keeping `space` a uniform prefix everywhere

## screen Desk
decided: 2026-11-27 · pure view-only 92ch tabular glance starting on row 1 (no cursor, no row selection; subject · exception tag · project · quiet age), top-left title row holds `desk · N wait`, Top-Right Global Status Bar holds `away` hosts and active focus
answers: what needs the user now?
shows:
- top-left title row: `desk` and the global waiting count (`N wait`, counted from local cache while hosts connect)
- `deal.Card` view-only tabular sheet (`92 ch`, starting on row 1 after the `14 px` top gap): up to seven 1-line waiting rows in deal order, each with subject, optional exception tag (`approve push` in `attention`, a stop in `failed`, `away` in `quiet`), right-aligned `quiet` project, and right-aligned `quiet` age. Repeated stops in one project share a row at the first stop's place. The overflow sits quietly below (`and 2 more · clock`).
states:
- empty: top-left title row reads `desk · 0 wait` once every attached host answers; calm empty sheet
- loading: counts from local cache (never flashes a false `0 wait` while hosts connect) while `connecting` shows calmly in the Top-Right Global Status Bar
- partial: waiting rows include locally cached cards from away hosts (marked calmly with `away` tag on those rows and `<host> away` once in the Top-Right Global Status Bar)
- error: hosts that are `away` stay calm in the Top-Right Global Status Bar; hosts in `needs you` state (key touch / enrollment) surface in red in the Top-Right Global Status Bar and `list hosts`
- full: the view-only tabular waiting rows and quiet overflow line (no cursor)
keys:
- `space-j` (`ctrl-j`): deal the first card in focus, `deal.Card.deal`
- click or tap a row: open that card directly (overflow opens the first hidden card)
- `space`: silent `window.Screen` prefix (`space f` places, `space u` undo)
- `i`: `chat.Write an idea`
- `tab`: `window.Screen` (return to the previous place)
- `:`: `window.Commands`
starts: `Glance at what waits`, `deal.Clear what waits`, `chat.Write an idea`, `window.Go to something by name`
captures: [the desk on the laptop](design/laptop-desk.png) and [on the phone](design/phone-desk.png).
rejected:
- an unread mark on what changed since the last glance · 2026-11-20 · the Desk is a glance; the deal orders what matters
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
