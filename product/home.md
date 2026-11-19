# home

Home is where the user lands: every room they work in, one line each, the one that wants them most at the top.

## concept Room
status: every message without a parent on a host is a room, so a host that posts one message per event fills Home with one room per event; each note about nothing is a room too, tagged `private`; and a Slack conversation opens as a screen of its own rather than as a `chat.Thread`.
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

The rooms go in this order: first the rooms that hold a card, in the order the deal would hand them; then the rooms with something unread, newest first; then every other room, newest first. With no room at all, Home reads `no rooms yet`.

## job Glance at what waits
day: Know what wants them, and where they were.
when: I have a moment and want to know if anything needs me
want: see, in one look, which rooms want me and how much
so that: I can decide to answer now or later
flow:
1. press `tab`, or open the product; `Home` is on screen with the cursor on the top room
2. read the top rows and their words
3. either open a room with `Room.open`, or press `tab` to go back to where I was
system: orders the rooms by what wants the user most, and writes each room's words.
user: the choice of what to open.
cases: dozens of rooms with nothing waiting under the few that do; a room whose words come from a card in one of its threads

## rule a card puts its room first
when the deal holds a `deal.Card` for a room or for a thread inside it, then that `Room` goes to the top of `Home` in the card's order and shows the card's words instead of its own.

## rule a verdict on a room row lands on its card
status: the verdict is said about the room itself, and the thread's card keeps its place and its words.
when the user gives a `deal.Verdict` on a `Room` on `Home` whose words come from a card on a thread inside it, then the verdict is about that card, and the row changes at once.
