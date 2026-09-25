# The dealer's cases

What `rank` must make of a history. Each row is a scenario test in
`src/rank/tests.rs` under its name (`a1_…`, `z4_…`); the rules that hold
for every history are proptests there too. The constants the numbers come
from are in `src/curve.rs`.

Home shows two sections: **next**, every card by priority until it fades
below the floor, and **running**, the agents at work.

## Agents

| # | History and facts | Card |
|---|---|---|
| A1 | Turn ended, finished | low, "finished · 0m ago", fades, gone after ~3 days |
| A2 | Turn ended asking for the user | "waiting on reply", rises |
| A3 | Turn errored | "errored", rises |
| A4 | Turn running, or the user's message is queued | no card; under running |
| A5 | The user wrote to it, and it finished within ~5 min | at the top, chimes |
| A6 | Done | nothing until a newer turn ends; then counts from that turn |
| A7 | Made by another agent | no card of its own |
| A8 | Muted, or its host is gone | nothing |
| A9 | Opened, not replied | priority unchanged |

## Snoozes

| # | History and facts | Card |
|---|---|---|
| Z1 | Snoozed | nothing until it ends |
| Z2 | Snooze ends, the node still wants the user | its own card, its wait counting from the snooze's end |
| Z3 | Snooze ends, the node wants nothing | nothing |
| Z4 | Snoozed, then the user wrote to the agent | the snooze holds, but a reply within 1h of the user's message comes through |
| Z5 | Snoozed, then the other side wrote | a direct message, or a thread of 3 people or fewer: a small bump per message when it comes back; nothing for other threads and agents |
| Z6 | Snoozed again and again | every snooze is kept; not ranked on yet |

## Todos and deadlines

| # | History and facts | Card |
|---|---|---|
| T1 | Todo | low, never fades, rises slowly, stays until done |
| T2 | Todo with a start date | nothing until then, then as T1 |
| T3 | Deadline, lead N days (3 unless said) | shows N days before, rises, jumps to the top once late |
| T4 | Todo on an agent at work | hidden while it works; back when its turn ends |
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
| X2 | A skip lowers the card, most in its first ~5 minutes and not at all after 30, in memory only; the node's source moving voids it. Every deal takes the top card at that moment |
| X3 | The hand changes by itself only at `next_change`: a snooze ending, a todo starting, a deadline coming into view or passing, a skip running out |
| X4 | "In 1h" is an hour on any clock; "tomorrow" starts at the user's own midnight |
| X5 | Ranking with a warm cache is ranking from scratch |
