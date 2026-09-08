# The point does not go dead when a message under it is deleted

*eng-bgkw, 2026-09-08.*

Someone deletes a message you are looking at, from the Slack app or another
client. The row goes. Where does the point go?

Three of the four answers were already right, and nobody had written down
why. The point in a transcript is an editor selection over an anchor, not a
line number, and an anchor inside deleted text collapses to the boundary.
So the point lands on the message that followed; deleting the last message
leaves it on the message before; an edit leaves it exactly where it was.
The list's cursor is a row index, which is why *it* needed a decision and
got one — the transcript looks like the same problem and is not.

The fourth answer was wrong. When the message that followed is under a day
rule, the boundary is the rule, and a point on a rule is a point with
nothing under it: `e` does nothing, a reaction does nothing, `enter` does
nothing, and nothing on screen says why. That is what happens whenever the
deleted message was the last one of its day.

`point_after_removal` is the rule, written down: the message that followed,
or the message before when there was nothing after it, and only when the
point was on the deleted message — a point anywhere else is the reader's own
place. `message_beside` walks off the removed row to the first message,
over a rule, never over a run of them.

Cost: one comparison per removal in the plan plus that walk, and nothing at
all when no removal is under the point.

What changes for the user: a deletion never leaves the point somewhere the
keys do nothing. The three cases that were already right are pinned by the
same test, so the next person to look at this does not have to re-derive
which surface anchors and which counts rows.
