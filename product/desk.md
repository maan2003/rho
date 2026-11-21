# desk

The desk answers what waits and what is moving. It is a glance, not a place to work: the deal walks the cards, and the idea key opens the draft.

## journey Switching projects
goal: the user picks up a different project where they left off.
starts when: the user decides to work on a different project now.
steps:
1. the user sees what has moved on the desk, `Glance at what moves`
2. the user opens the thread by project name and subject in the same query, `window.Go to something by name`
3. the user picks up the thread, with their draft still there, `mail.Answer a mail`, `mail.Draft`
done when: the user knows what moved and can carry on where they left off, drafts and all.
day: Know what wants them, and where they were.

## concept Desk
purpose: show what wants the user, what has moved, and where to put the next idea.
principle: the user comes back after lunch, sees three things waiting and a test run under `moving`, and deals the first.
sees: `N wait` and up to seven `deal.Card` rows, in the deal’s order, each a subject and a project; `moving`, with up to ten `mail.Thread` rows that aren't waiting, each with the project's latest words, newest first, then `and N more` for the rest.
does:
- open `tab`: show the desk, as `window.Screen`
- deal `space`: open the first waiting card
- open a row `enter` or one tap: open the selected card or thread; the waiting overflow opens the first hidden card
- write an idea `i` on any row: open the idea draft, as `mail.Write an idea`
- narrow: show only one `mail.Project`'s waiting and moving rows, chosen by name, with where the project works and its mode in the top line, such as `tide@lab · on tide · one agent`; a project has no screen of its own

## screen Desk
answers: what needs the user now, and what has moved?
shows:
- `Desk`: the count of everything that waits
- `deal.Card`: up to seven waiting rows, in deal order, each its subject and project; nothing on a row says why or since when, the deal says that when it opens the card. Repeated stops in one project share a row at the first stop's place. The overflow names what it hides, such as `and 2 more · clock@lab`
- `mail.Thread`: up to ten rows under `moving`, in order of the project's latest words, with those words after the subject; threads with a waiting card are excluded, even when their card isn't shown
states:
- empty: `0 wait`; no waiting or moving rows
- loading: not agreed
- partial: not agreed
- error: not agreed
- full: waiting rows, then moving rows
keys:
- `space`: deal the first card, `Desk.deal`; with no card waiting and a verdict to take back, open `deal.Verdict`
- `enter`: open the selected row, `Desk.open a row`
- `j` and `k`: move between waiting, overflow, and moving rows, skipping labels and blank lines; `gg`: first row; `G`: last row
- `i`: `mail.Write an idea`
- `tab`: `window.Screen`
- `:`: `window.Palette`, which leaves out what waits here, since it is already in view
starts: `Glance at what moves`, `deal.Clear what waits`, `mail.Write an idea`, `window.Go to something by name`
rejected:
- `asks · 2h` and `result · 8m` words and ages on the rows · 2026-11-20 · the Desk is a glance; the deal says why and since when, one card at a time
- a mark on what changed since the last glance · 2026-11-20 · the same reason
- a draft tail at the bottom · 2026-11-20 · the idea key opens the draft, see `mail.Write an idea`

On the phone, rows keep their place while the Desk is on screen, so a row does not move under the thumb; a new order shows when the Desk opens again.

Built renders: [moving on the laptop](design/laptop-desk.png) and [on the phone](design/phone-desk.png).

mock:
```text
desk                                                                        NOR

15 wait
Flaky clock test  tide@lab
Release window  slack@harness
reef@lab  6 failed
Switch the mail runtime  dune@lab
Limit the retry window  mire@lab
Rename the fixture  sand@lab
Shoreline import  shoal@lab
and 3 more · clock@lab

moving
Cache eviction tests  tide@lab · running the suite with a separate clock, 40 of 50
Release notes  shoal@lab · checking the links
```

mock, phone:
```text
desk                               NOR
15 wait
Flaky clock test               tide@lab
Release window            slack@harness
reef@lab                       6 failed
Switch the mail runtime        dune@lab
Limit the retry window         mire@lab
Rename the fixture             sand@lab
Shoreline import              shoal@lab
and 3 more · clock@lab

moving
Cache eviction tests           tide@lab
  running the suite with a separate clock
Release notes                 shoal@lab
  checking the links

        menu        open        more
```

## flow Glance at what moves
runs: 30 a day
day: Know what wants them, and where they were.
when: the user has a moment or comes back after a while
want: see what asks for them and what is moving without opening a thread
so that: they can choose whether to answer, look into the work, or carry on where they were
steps:
1. the user opens the `Desk` with `tab`, or sees it on a cold return
2. the user reads what waits at the top
3. the user reads the latest changes under `moving`
system: counts and ranks waiting cards, and puts threads that aren't waiting under `moving`, the project's latest words first.
user: what, if anything, to open.
cases: nothing waits but work is moving; more than seven cards or ten moving threads; a cold return.
reviewed: 2026-11-20 · laptop · reviewer · 1 finding

The historical review was of Glance at what waits on Home.

## rule a cold return opens the desk
when the user returns within ten minutes away, then their `window.Screen` is where they left it; after more than ten minutes away, the `Desk` is on screen. Away begins when the window loses the keyboard or the device goes idle.

## rule work has words before the agent writes any
when an agent works in a `mail.Thread` and the project has not written since the user did, then the thread's `moving` row reads `working`; the row goes away when the agent stops or the thread waits.
