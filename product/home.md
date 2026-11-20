# home

Home is where the user lands: every room they work in, one line each, the one that wants them most at the top.

## journey Switching projects
goal: the user picks up a different project where they left off.
starts when: the user decides to work on a different project now.
steps:
1. the user looks for the project's `Room` on `Home`, `Glance at what waits`
2. the user opens the room by name, `window.Go to something by name`
3. the user sees what moved in its threads, `chat.Answer an agent`
4. the user picks up where they left off, with their draft still there, `chat.Answer an agent`, `chat.Draft`
done when: the user is back in the project's room, knows what moved, and can carry on where they left off, drafts and all.
day: Know what wants them, and where they were.

## concept Room
purpose: give every place the user works in one line on Home, so they see at a glance where something waits.
principle: an agent asks the user something in a thread of the channel `debug`; on Home, `debug` moves to the top and reads `asks · 8.3d`; the user opens it and answers.
sees: its name; what waits in it: a card's words such as `asks · 48.4d` in yellow, or `1 unread · 22h` in grey, or nothing; where it is: the host's name or the Slack workspace's name.
has:
- many `chat.Thread`, when the room is a `chat.Channel`
- one `chat.Thread`, when the room is a Slack conversation
does:
- open `enter`, or `open` on the phone: open the room
- answer `space`: open the menu of `deal.Verdict` for the `deal.Card` over the room

A room is a place the user works in. It is one of two things: a `chat.Channel` the user made or has written in, or that holds a `deal.Card` or an unread mention of the user; or a Slack conversation the user starred or that asks them. A message without a parent that the user never wrote in and that asks nothing is not a room; it is reached by name in `window.Palette`. A note is never a room; see `notes.Note`. A long name is cut to keep the row on one line.

## screen Home
shows: `Room`, `deal.Card` words on the rooms that hold one
starts: `Glance at what waits`, `deal.Clear what waits`, `window.Go to something by name`

With no rooms, Home reads `no rooms yet · attach a host or register a Slack workspace from the palette (:)`.

The rooms go in this order: first the rooms that hold a card, in the order the deal would hand them; then the rooms with something unread, newest first; then every other room, newest first. With no room at all, Home reads `no rooms yet`.

## flow Glance at what waits
day: Know what wants them, and where they were.
when: the user has a moment and wants to know if anything needs them
want: see, in one look, which rooms want the user and how much
so that: the user can decide to answer now or later
steps:
1. the user presses `tab`, or opens the product; `Home` is on screen with the cursor on the top room
2. the user reads the top rows and their words
3. the user either opens a room with `Room.open` or presses `tab` to go back to where they were
system: orders the rooms by what wants the user most, and writes each room's words.
user: the choice of what to open.
cases: dozens of rooms with nothing waiting under the few that do; a room whose words come from a card in one of its threads
reviewed: 2026-11-20 · laptop · reviewer · 1 finding

## rule a card puts its room first
when the deal holds a `deal.Card` for a room or for a thread inside it, then that `Room` goes to the top of `Home` in the card's order and shows the card's words instead of its own.

## rule a verdict on a room row lands on its card
when the user gives a `deal.Verdict` on a `Room` on `Home` whose words come from a card on a thread inside it, then the verdict is about that card, and the row changes at once.
