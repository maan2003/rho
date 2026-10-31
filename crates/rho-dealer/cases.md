# The dealer's cases

What `rank` must make of a history. Each row is a scenario test in
`src/rank/tests.rs` under its name (`a1_…`, `z4_…`); the rules that hold
for every history are proptests there too. The constants the numbers come
from are in `src/curve.rs`.

Home shows three sections: **next**, every card by priority until it fades
below the floor, **recent**, the user's agents by their latest send, folded, and **piles**, what the
user put away, which they open to be dealt from it.

## Mail threads

An agent reaches the user only through mail, and mail comes in threads:
a thread is named by the mail that started it, and an agent answers in the
thread of the newest mail between it and the user. Each thread between
the user and one of their agents is a node; the agent itself is never a
card. Its mail with other agents is never unread, and whether it is
running never ranks. Every send names its kind, picked by whether the
user needs to read it now:

```
1. Yes: it asks them for something the work needs: a decision, an
   approval, information, or an action, even while the agent keeps
   working. An offer of work beyond what they asked is not one. → ask
2. Yes: what they asked for is done, or it answers their question.
   Not while part of it is still pending.                       → result
3. No, but it is worth keeping: a finding, an assumption, a report
   while follow-through is pending.                             → fyi
4. No, and it is passing: "on it", "86% done".                  → status
```

The user reads a thread explicitly: a done, or a message of their own in
it, reads it through to its end. Opening it reads nothing. A card is
titled by the first line of the thread's first mail, and its context is
the agent.

| # | History and facts | Card |
|---|---|---|
| A1 | An unread ask | "asks", rises |
| A2 | An unread result | "result", below an ask, fades, gone after ~3 days |
| A3 | An fyi | nothing, and it hides no ask or result before it; it stays in the thread, where the user reads it |
| A4 | A status | nothing, and it hides nothing; it is in no thread |
| A5 | Several unread sends | one card, of the newest ask or result, counting from it |
| A6 | Read | nothing until a newer ask or result |
| A7 | The user wrote in the thread within the hour before the send | a bonus, fading over the hour |
| A10 | An engineer another agent started for the user | its brief, mail from that agent, is context, never unread; it deals by its own sends |
| A11 | Made by another agent for its own work | nothing; it belongs to that agent |
| A12 | Its runtime running, waiting, idle, stopped on an error, or retrying | changes nothing: the runtime is not mail |
| A13 | The thread or its agent muted, or its host gone | nothing |

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
| T4 | Todo on a mail thread | as T1, whatever the agent is doing |
| T5 | Todo on a mail thread, then the user writes in it | the todo stays; only done clears it |
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
