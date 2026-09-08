# The record says what Slack accepted, not what was pressed

*eng-bgkw, 2026-09-08.*

rho keeps a journal of the reader's day. Two of its Slack entries were
written the moment enter was pressed, before Slack had answered: a rewrite
and a file sent with a message.

So a rewrite the server refused left a record saying the message was edited,
and an upload that failed left one saying a file was sent — while the surface
itself, rightly, put the reader's words back and told them it had not
happened. The screen was honest and the record was not, about the same
keystroke.

Sends were already recorded from the confirmed path: the session emits
`Replied` from `accept_own`, after Slack has returned a timestamp. The
difference was in how each was written, not in anything about the writes.

`submit` now answers with what became of the press — sent, the rewrite of
this message, a picture of this size, refused, or nothing to send — and the
host writes the record from that. The answer does not carry the work: the
write is spawned detached, and the returned task only watches for the
outcome, so a caller with no use for the answer cannot cancel a message by
dropping it.

What changes for the user: the journal no longer credits them with rewrites
they did not make or files that never arrived. Nothing on screen changes.
