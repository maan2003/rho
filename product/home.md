# home

Home is where the user lands: every room they can read, one line each, the one that wants them most at the top.

## concept Room
purpose: give every place the user reads one line on Home, so they see at a glance where something waits.
principle: an agent asks the user something in a thread of the channel `debug`; on Home, `debug` moves to the top and reads `asks · 8.3d`; the user opens it and answers.
sees: its name; what waits in it: a card's words such as `asks · 48.4d` in yellow, or `1 unread · 22h` in grey, or nothing; where it is: the host's name, the Slack workspace's name, or `private`.
has:
- many `chat.Thread`, when the room is a `chat.Channel`
- one `slack.Conversation`, when the room is from Slack
- one `notes.Note`, when the room is a note about nothing
does:
- open `enter`, or `open` on the phone: open the room
- answer `space`: open the verdicts of a `deal.Card` over the room

A room is one of three things: a channel on an attached host, a Slack conversation the user starred or that Slack is asking them about, or a note about nothing. A long name is cut to keep the row on one line.

## screen Home
shows: `Room`, `deal.Card` words on the rooms that hold one, `deal.Lamp`
starts: `Glance at what waits`, `deal.Clear what waits`, `notes.Write something down`, `window.Go to something by name`

The rooms go in this order: first the rooms that hold a card, in the order the deal would hand them; then the rooms with something unread, newest first; then every other room, newest first. With no room at all, Home reads `no rooms yet`.

## job Glance at what waits
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
when the user gives a verdict on a `Room` on `Home` whose words come from a card on a thread inside it, then the verdict is about that card, and the row changes at once.
