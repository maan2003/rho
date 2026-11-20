# desk

The desk answers what waits and what is moving, and gives the user a place to write a new idea.

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
principle: the user comes back after lunch, sees three things waiting and a test run under `moving`, and writes a new idea to `tide@lab`.
sees: `N wait` and up to seven `deal.Card` rows, in the deal’s order; `moving`, with up to ten `mail.Thread` rows with `mail.Status` that aren't waiting, newest status change first, then `and N more` for the rest; the `design.Draft tail` addressed to a `mail.Project`, with completion.
does:
- open `tab`: show the desk, as `window.Screen`
- deal `space`: open the first waiting card
- open a row `enter` or one tap: open the selected card or thread
- write an idea: address and send a new idea, as `mail.Write an idea`
- narrow: show only one `mail.Project`'s waiting and moving rows, chosen by name; a project has no screen of its own

## screen Desk
status: narrowing to one project is not built; the old channel screen still exists in its place.
answers: what needs the user now, and what has moved?
shows:
- `Desk`: the count of everything that waits
- `deal.Card`: up to seven waiting rows, in deal order; the same project and reason share a row at the first card’s place; a line below counts the unseen cards and any results among them
- `mail.Thread`: up to ten rows under `moving`, in order of their latest status change; threads with a waiting card are excluded, even when their card isn't shown
- `mail.Project`: the address on the `design.Draft tail`
states:
- empty: `0 wait`; no waiting or moving rows; the idea tail remains
- loading: not agreed
- partial: not agreed
- error: not agreed
- full: waiting rows, moving rows, and the addressed idea tail
keys:
- `space`: deal the first card, `Desk.deal`
- `enter`: open the selected row, `Desk.open a row`
- `G`: go to the idea; `i`, `A`, `o`, or tap: write in place; `esc`: Normal in the same draft; `ctrl-enter` or `send`: send the idea
- `tab`: `window.Screen`
- `ctrl-t` while writing: choose another project without losing the draft
- `:`: `window.Palette`
starts: `Glance at what moves`, `deal.Clear what waits`, `mail.Write an idea`, `window.Go to something by name`

Built renders: [waiting and moving on the laptop](design/laptop-desk.png) and [on the phone](design/phone-desk.png).

mock:
```text
desk                                                                  ●     NOR

15 wait
Flaky clock test            asks · 2.0h                       tide@lab
reef@lab                    6 failed · 5m
Release window              asks · 20m                        slack@harness
Shoreline import            result · 8m                       shoal@lab
Switch the mail runtime     asks · 3m                         dune@lab
Limit the retry window      asks · 2m                         mire@lab
Rename the fixture          asks · now                        sand@lab
and 3 more · 1 result

moving
Cache eviction tests        working · 3m                      tide@lab
Release notes               checking the links · 18m          shoal@lab

── idea to tide@lab · ctrl-enter sends · ctrl-t project
Keep the asymmetric clock test in the suite.
```

mock, phone:
```text
desk                          ●    NOR
15 wait
Flaky clock test               tide@lab
  asks · 2.0h
reef@lab
  6 failed · 5m
Release window            slack@harness
  asks · 20m
Shoreline import              shoal@lab
  result · 8m
Switch the mail runtime        dune@lab
  asks · 3m
Limit the retry window         mire@lab
  asks · 2m
Rename the fixture             sand@lab
  asks · now
and 3 more · 1 result

moving
Cache eviction tests           tide@lab
  working · 3m
Release notes                 shoal@lab
  checking the links · 18m

── idea to tide@lab
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
system: counts and ranks waiting cards, and puts threads that aren't waiting under `moving`, latest status change first.
user: what, if anything, to open.
cases: nothing waits but work is moving; more than seven cards or ten moving threads; a cold return.
reviewed: 2026-11-20 · laptop · reviewer · 1 finding

The historical review was of Glance at what waits on Home.

## rule a cold return opens the desk
when the user returns within ten minutes away, then their `window.Screen` is where they left it; after more than ten minutes away, the `Desk` is on screen.

## rule work has a status before the agent writes one
when an agent works in a `mail.Thread` without a `mail.Status`, then its status reads `working` and its age; its `moving` row goes away when the agent stops or its card is dealt.
