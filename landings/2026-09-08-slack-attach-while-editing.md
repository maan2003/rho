# A picture cannot be attached to a rewrite, and now says so

*eng-bgkw, 2026-09-08.*

Press `up` to fix the message you just sent, drop in the screenshot you meant
to include, press enter. Until now that posted a brand new message carrying
the picture and the rewrite's words: the message being fixed was left
unchanged and still tinted as being edited, and the half-written line put
aside for the edit stayed stashed with no way back to it. Two messages where
one was wanted, and a composer left in a mode with no visible end.

`submit` checked for an attachment before it checked for an open edit, and
none of the three ways to attach — paste, drop, the attach prompt — looked at
whether an edit was open at all.

Slack has no way to put a file on a message that already exists: `chat.update`
carries text. So the answer is a refusal rather than a limitation worked
around, and it comes at attach time, while the rewrite is still on screen to
be finished or left, rather than at send time when the rewrite is already
going out.

What changes on screen: attaching during a rewrite says "a rewrite cannot
carry a picture; finish or leave the edit first", the rewrite stays open with
its words in it, nothing already typed moves, and enter finishes the rewrite
as it always did. Attaching outside a rewrite is unchanged.
