# After a search, the point is on the first match

*eng-bgkw, 2026-09-08.*

The Slack list holds one rule about the point: it follows the conversation,
never the line number, so a message arriving in a busier conversation cannot
move the reader's selection under a keypress. The list kept that rule by
finding the held conversation again after each draw — but only when it was
still on screen.

A search takes rows away, and often the row the point was on. Nothing then
placed the point at all. It fell to wherever the editor clamped it, which is
the blank line under the listing, so after narrowing, the first `enter`
opened nothing and the reader had to move before anything answered.

Measured before the fix: six conversations narrowed to two, the point on row
three, and what `enter` would open reading as nothing.

Now, when the conversation the point was on is gone from the list, the point
goes to the first row — the match the reader typed for. When the point was
not on a conversation at all, on the break or below the rows, it is left
alone: that position is the reader's, and moving it because a message
arrived is the thing the rule exists to prevent.

What changes for the user: after a search, the point is on the first match
and `enter` opens it.
