# "Next unread" looks at the whole workspace, not at the search you left on

*eng-bgkw, 2026-09-08.*

The Slack list narrows to a typed query, and the narrowing is a state of the
model: nothing clears it but another search. Open a conversation, come back,
open a new list — the query still stands, and only restarting rho drops it.

`next_unread` walked those narrowed rows. So an hour after searching for
"des", pressing the next-unread key walked four channels, found nothing, and
sent the reader back to the list, while three DMs sat unread outside the
query. The one key that exists to find unread messages was the one thing
that could not see them, and the list showed no reason: a narrowing that
reaches some rows says nothing about itself.

A query is what the reader is looking at. Where the unread messages are is a
question about the workspace, so `next_unread` now reads the whole order
whatever the query is — the same set `mark_plan` reads, so the next-unread
key and the backlog-marking prompt can no longer disagree about how much is
waiting.

What changes for the user: with a search left standing, the next-unread key
still takes them to unread conversations outside it. The list itself is
unchanged — it still shows what was typed for, and it still does not say so.
That second half is its own change.
