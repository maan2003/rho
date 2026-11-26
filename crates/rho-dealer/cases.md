# The dealer's cases

What `rank` must make of a history. Each row is a scenario test in
`src/rank/tests.rs` under its name (`a1_…`, `z4_…`); the rules that hold
for every history are proptests there too. The constants the numbers come
from are in `src/curve.rs`.

The deal ranks every card by priority, highest first. Pinging mail and
stops wait until answered, rising half a unit an hour from a head start,
so the longest wait comes to the top and none starves. A thread the user
wrote in within the hour gains a bonus that fades over it, so the answer
to their active work deals first. The same priority lights the lamp and
rings the chime. The user's overdue deadlines go first, and skips go to
the back. The user answers waiting mail with a reply or verdict; either
reads the whole thread through the newest mail in view.

## Threads

Agents and the user talk in mail: each host holds a forest of messages,
and the thread under a message is every reply below it. A message goes to
a thread, not to a person, and draws the user only by pinging them.
Each thread is a node; an agent is never a card. What the mail claims for
each of its cases is the mail's own, in `rho-chat/src/cards.rs`, tested
there. The rows below are what the dealer makes of a source shaped like
it, with a stand-in thread that asks and answers.

A card is titled by the thread's subject and the project's address.

| # | History and facts | Card |
|---|---|---|
| A1 | Unseen pinging mail | `waits`, rises |
| A2 | A result that pings | `waits` just like a question, stays until answered |
| A3 | Unseen non-pinging mail | no card; it does not hide pinging mail |
| A4 | A message that mentions nobody, or another agent | nothing, and it hides nothing |
| A5 | Several unseen pings | one card, waiting from the oldest |
| A6 | Seen | nothing until newer pinging mail |
| A7 | The user wrote in the thread within the hour before the mention | a bonus worth three hours of waiting, fading over the hour: it deals ahead of mail that waited less, and chimes at once |
| A8 | An older device said done through a message before the mention | the mention still deals |
| A15 | Done | the thread is read through its newest mail in view: every ping in it goes, and only a newer ping or edit deals it again; undo brings the card back as it was |
| A16 | A reply, logged or still queued on this device | the thread is read through the newest mail in view when it was sent; mail after that deals |
| A10 | Another agent's message in the thread, mentioning the thread's agent | context, never unread; the thread deals by mentions of the user |
| A11 | An agent made by another agent | deals only by mentioning the user, like any other |
| A12 | Its agents' runtimes running, waiting, idle, stopped on an error, or retrying | changes nothing: the runtime is not mail |
| A13 | The thread muted | nothing |
| A14 | A stop mailed by the system bot | one card, with the stop’s reason; stays until answered or recovered |

## Snoozes

| # | History and facts | Card |
|---|---|---|
| Z1 | Snoozed | nothing until it ends |
| Z2 | Snooze ends, the node still wants the user | its own card, its wait counting from the snooze's end |
| Z3 | Snooze ends, the node wants nothing | nothing |
| Z4 | Snoozed, then the user wrote in the thread | the snooze holds, but a reply within 1h of the user's message comes through |
| Z6 | Snoozed again and again | every snooze is kept; not ranked on yet |
| Z7 | A channel is snoozed | its threads hold too |

## What older builds wrote

Piles and todos went with the rooms (`docs/rho2.md`); their facts stay in
the user's logs and still read.

| # | History and facts | Card |
|---|---|---|
| P1 | An old pile | done said then, having seen nothing: takes back the snooze and deadline before it, and holds nothing back |
| T1 | An old todo | a read: what it saw is handled, it takes nothing back and deals no card of its own |

## Deadlines

| # | History and facts | Card |
|---|---|---|
| T3 | Deadline, lead N days (3 unless said) | shows N days before, rises, jumps to the top once late |
| T6 | Done or muted | takes back the deadline and the snooze |

## Notes

| # | History and facts | Card |
|---|---|---|
| N1 | A plain note | no card |
| N2 | Deadline | as T3 |
| N3 | Done or deleted | nothing |

## Every history

| # | Rule |
|---|---|
| X1 | One card per node |
| X6 | Cards not skipped deal highest priority first, equal ones by address |
| X2 | A skip sends the card behind every card not skipped, for 30 minutes, in memory only; skipped cards come round longest-skipped first, so each is dealt once before any repeats. The node's source moving voids the skip. Every deal takes the top card at that moment |
| X3 | The hand changes by itself only at `next_change`: a snooze ending, a deadline coming into view or passing, a skip running out, a card's words saying another age |
| X4 | "In 1h" is an hour on any clock; "tomorrow" starts at the user's own midnight |
| X5 | Ranking with a warm cache is ranking from scratch |
