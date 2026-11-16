# The dealer's cases

What `rank` must make of a history. Each row is a scenario test in
`src/rank/tests.rs` under its name (`a1_…`, `z4_…`); the rules that hold
for every history are proptests there too. The constants the numbers come
from are in `src/curve.rs`.

Home shows three sections: **next**, every card by priority until it fades
below the floor, **recent**, the threads agents wrote in lately, unread
ones marked, folded, and **piles**, what the
user put away, which they open to be dealt from it.

## Threads

Agents and the user talk in chat: each host holds a forest of messages,
and the thread under a message is every reply below it. A message goes to
a thread, not to a person, and draws the user only by mentioning them.
Each thread is a node; an agent is never a card. What the chat claims for
each of its cases is the chat's own, in `rho-chat/src/cards.rs`, tested
there. The rows below are what the dealer makes of a source shaped like
it, with a stand-in thread that asks and answers.

A card is titled by the thread's name, and its context is its root, the
channel.

| # | History and facts | Card |
|---|---|---|
| A1 | An unseen ask | "asks", rises |
| A2 | An unseen result | "result", below an ask, fades, gone after ~3 days |
| A3 | An unseen fyi | nothing, and it hides no ask or result; the thread is unread on Home |
| A4 | A message that mentions nobody, or another agent | nothing, and it hides nothing |
| A5 | Several unseen mentions | one card: an ask if any, counting from the first ask unseen, else the newest result |
| A6 | Seen | nothing until a newer ask or result |
| A7 | The user wrote in the thread within the hour before the mention | a bonus, fading over the hour |
| A8 | Done through a message before the mention | the mention still deals |
| A10 | Another agent's message in the thread, mentioning the thread's agent | context, never unread; the thread deals by mentions of the user |
| A11 | An agent made by another agent | deals only by mentioning the user, like any other |
| A12 | Its agents' runtimes running, waiting, idle, stopped on an error, or retrying | changes nothing: the runtime is not chat |
| A13 | The thread muted, its host gone, or every one of its agents deleted | nothing |

## Snoozes

| # | History and facts | Card |
|---|---|---|
| Z1 | Snoozed | nothing until it ends |
| Z2 | Snooze ends, the node still wants the user | its own card, its wait counting from the snooze's end |
| Z3 | Snooze ends, the node wants nothing | nothing |
| Z4 | Snoozed, then the user wrote in the thread | the snooze holds, but a reply within 1h of the user's message comes through |
| Z5 | Snoozed, then the other side wrote | a direct message, or a thread of 3 people or fewer: a small bump per message when it comes back; nothing for other threads and agents |
| Z6 | Snoozed again and again | every snooze is kept; not ranked on yet |

## Piles

| # | History and facts | Card |
|---|---|---|
| P1 | Put on a named pile | nothing, whatever its source says, until the user opens the pile |
| P2 | Then a todo, done, a mute, a snooze or another pile | off the pile; as that says |
| P3 | The piles | named ones by name, each oldest first; then one unnamed pile of everything snoozed, soonest back first |

## Todos and deadlines

| # | History and facts | Card |
|---|---|---|
| T1 | Todo | low, never fades, rises slowly, stays until done |
| T2 | Todo with a start date | nothing until then, then as T1 |
| T3 | Deadline, lead N days (3 unless said) | shows N days before, rises, jumps to the top once late |
| T4 | Todo on a thread | as T1, whatever its agents are doing |
| T5 | Todo on a thread, then the user writes in it | the todo stays; only done clears it |
| T6 | Done or muted | takes back the todo, the deadline and the snooze |

## Slack

| # | History and facts | Card |
|---|---|---|
| S1 | Unread direct message / mention / reply in a followed thread | starts at 1.2 / 1.1 / 0.6 and rises alike; a thread of 3 people or fewer asks like a direct message |
| S2 | Unread channel traffic | starts at 0.1, gone within a day, or half a day once someone else answers |
| S3 | Read anywhere, or replied | nothing |
| S4 | A new message after that | a card again, counting from the new message |
| S5 | Todo on a thread | read in Slack; then as T1 |
| S6 | Muted in Slack | nothing |
| S7 | A room snoozed | every thread in it holds too |

## Notes

| # | History and facts | Card |
|---|---|---|
| N1 | A plain note | no card |
| N2 | Todo or deadline | as T1–T3 |
| N3 | Done or deleted | nothing |

## Every history

| # | Rule |
|---|---|
| X1 | One card per node |
| X2 | A skip sends the card behind every card not skipped, for 30 minutes, in memory only; skipped cards come round longest-skipped first, so each is dealt once before any repeats. The node's source moving voids the skip. Every deal takes the top card at that moment |
| X3 | The hand changes by itself only at `next_change`: a snooze ending, a todo starting, a deadline coming into view or passing, a skip running out |
| X4 | "In 1h" is an hour on any clock; "tomorrow" starts at the user's own midnight |
| X5 | Ranking with a warm cache is ranking from scratch |
