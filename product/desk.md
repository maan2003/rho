# desk

The desk answers what waits and what is moving, and gives the user a place to write a new idea.

## journey Switching projects
status: the user goes to a room on Home or finds a channel by name. A channel lists threads and unread counts, not how its work is moving.
goal: the user picks up a different project where they left off.
starts when: the user decides to work on a different project now.
steps:
1. the user sees what has moved on the desk, `Glance at what moves`
2. the user opens the project by name, `window.Go to something by name`
3. the user opens its work by subject, `window.Go to something by name`
4. the user picks up the thread, with their draft still there, `mail.Answer a mail`, `mail.Draft`
done when: the user knows what moved and can carry on where they left off, drafts and all.
day: Know what wants them, and where they were.

## concept Desk
purpose: show what wants the user, what has moved, and where to put the next idea.
principle: the user comes back after lunch, sees three things waiting and a test run under `moving`, and writes a new idea to `tide@lab`.
sees: `N wait` and the top seven `deal.Card` rows at most, in the deal's order; `moving`, with up to ten `mail.Thread` rows with `mail.Status`, newest status change first, then `and N more` for the rest; an idea box addressed to a `mail.Project`, with completion.
does:
- open `tab`: show the desk, as `window.Screen`
- deal `space`: open the first waiting card
- open a row `enter`: open the selected card or thread
- write an idea: address and send a new idea, as `mail.Write an idea`

## screen Desk
answers: what needs the user now, and what has moved?
shows:
- `Desk`: the count of everything that waits
- `deal.Card`: up to seven waiting rows, in deal order
- `mail.Thread`: up to ten rows under `moving`, in order of their latest `mail.Status` change; when a row has a card, its words take the place of its status
- `mail.Project`: the address on the `Desk` idea box
states:
- empty: `0 wait`; no waiting or moving rows; the idea box remains
- loading: not agreed
- partial: not agreed
- error: not agreed
- full: waiting rows, moving rows, and the addressed idea box
keys:
- `space`: deal the first card, `Desk.deal`
- `enter`: open the selected row, `Desk.open a row`
- `tab`: `window.Screen`
- `:`: `window.Palette`
starts: `Glance at what moves`, `deal.Clear what waits`, `mail.Write an idea`, `window.Go to something by name`
mock:
```text
desk                                                                        NOR

3 wait
Flaky clock test            asks · 2h                         tide@lab
Release window              asks · 20m                        slack@harness
Shoreline import            result · 8m                       shoal@lab

moving
Cache eviction tests        running the tests · 3m            tide@lab
Release notes               checking the links · 18m          shoal@lab

idea to tide@lab · ctrl-enter sends
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
system: counts and ranks waiting cards, and puts threads with the latest change of status first under `moving`.
user: what, if anything, to open.
cases: nothing waits but work is moving; more than seven cards or ten moving threads; a cold return.
reviewed: 2026-11-20 · laptop · reviewer · 1 finding

The historical review was of Glance at what waits on Home.

## rule a cold return opens the desk
when the user returns within ten minutes away, then their `window.Screen` is where they left it; after more than ten minutes away, the `Desk` is on screen.
