# The dealer's cases

What `rank` must make of a history. Each row is a scenario test in
`src/rank/tests.rs` under its name (`a1_…`, `z4_…`); the rules that hold
for every history are proptests there too. The constants the numbers come
from are in `src/curve.rs`.

Home shows three sections: **next**, every card by priority until it fades
below the floor, **recent**, the user's agents by their latest send, folded, and **piles**, what the
user put away, which they open to be dealt from it.

## Agents

An agent reaches the user only through their conversation: its sends, the
user's messages, and the host's notices about it. Its mail with other
agents is never unread, and whether it is running never ranks. Every send names its kind,
picked by these questions in order:

```
1. Does this ask the human for something the work needs: a decision,
   an approval, information, or an action? It counts even while I keep
   working on other parts. An offer of work beyond what they asked
   is not one.                                                  → ask
2. Does this deliver what the human asked for: an answer to their
   question, or finished work?                                   → result
3. Is it an acknowledgement or progress: "on it", "86% done"?    → status
4. None of these.                                                → other
```

The user reads a conversation explicitly: a done, or a message of their
own, reads it through to its end. Opening it reads nothing.

| # | History and facts | Card |
|---|---|---|
| A1 | An unread ask | "asks", rises |
| A2 | An unread result | "result", below an ask, fades, gone after ~3 days |
| A3 | An unread other | low, fades within a day |
| A4 | A status | nothing; it is the agent's status line until any later message, from either side, hides it |
| A5 | Several unread sends | one card, of the strongest kind, counting from its oldest unread send |
| A6 | Read | nothing until a newer send that is not a status |
| A7 | The user wrote within the hour before the send | a bonus, fading over the hour |
| A8 | Notice: it stopped on an error and will not go on alone (retries run out, crashed, needs an account or an approval) | as an ask |
| A9 | Notice: an error the host retries by itself | nothing |
| A10 | An engineer another agent started for the user | its brief opens the conversation as context, never unread; it deals by its own sends |
| A11 | Made by another agent for its own work | nothing; it belongs to that agent |
| A12 | Running, waiting, or idle | changes nothing |
| A13 | Muted, or its host is gone | nothing |

## Snoozes

| # | History and facts | Card |
|---|---|---|
| Z1 | Snoozed | nothing until it ends |
| Z2 | Snooze ends, the node still wants the user | its own card, its wait counting from the snooze's end |
| Z3 | Snooze ends, the node wants nothing | nothing |
| Z4 | Snoozed, then the user wrote to the agent | the snooze holds, but a reply within 1h of the user's message comes through |
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
| T4 | Todo on an agent | as T1, whatever the agent is doing |
| T5 | Todo on an agent, then the user writes to it | the todo stays; only done clears it |
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
