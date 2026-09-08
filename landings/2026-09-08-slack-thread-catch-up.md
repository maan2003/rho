# A thread on screen catches up after an outage

*eng-bgkw, 2026-09-08.*

`Session::resync_tail` is what asks the surface on screen for anything newer
than its last message, on reconnect and on every successful feed poll. Its
comment says why: the socket is the fast path and not the reliable one, and a
socket that dies without announcing it delivers nothing, so this is what makes
that case a minute of lag rather than silence. It opened by matching only
`Source::Conversation`, and `focused` is set for threads too — so for a thread
it was the silence. The reader sat in a thread, the machine slept, and the
replies posted meanwhile did not arrive at all until they left and came back.

Threads were excluded because there was nothing to ask with.
`conversations_replies` takes a cursor and no lower bound, so re-syncing one
meant fetching the whole thread, once a poll, for as long as the reader stayed
in it. `conversations_replies_since` is the bound: `oldest` and
`inclusive=false` on the replies endpoint, the twin of the call a conversation
already uses, so the request costs what is new rather than what is there.

Slack returns a thread's root message on a replies call whatever the bound, so
one row comes back that the caller already holds; the loaded run deduplicates
on the timestamp, so it costs a row on the wire and nothing on screen. What
changes for the user: replies posted while the machine was asleep are on
screen when they come back to it, in the thread they were already reading.
