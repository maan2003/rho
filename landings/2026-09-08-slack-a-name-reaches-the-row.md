# A name that arrives late reaches the row it belongs to

*eng-bgkw, 2026-09-08.*

The other half of asking Slack who someone is: having asked, the answer has
to reach what is already on screen.

A transcript row is rendered once, from the model as it stood when the
message landed. `Model::author` is read at that moment, so a message whose
author rho has no name for draws as `someone` — and went on saying
`someone` for as long as the conversation stayed open, because a name
arriving is not a change to the message and nothing in the update log
speaks for one. The reader had to leave the conversation and come back.

Observed against the fake, before this: a conversation opened on an empty
mirror draws `someone: morning` for a message whose author is in the
roster, because the page landed before the roster did, and it stays
`someone` while a later message from the same person draws as `ada:`. A
person who joins after startup draws as `someone:` on every message.

`awaiting_names` is the surface's own record of which rows drew without a
name, kept the way `awaiting_images` keeps the rows drawn before a picture
finished downloading. `settle_names` runs on refresh and redraws exactly
those rows whose author now has a name. Nothing else moves: the run, the
anchors, the cursor and the scroll are where the reader left them, because
a name is not a change to the message.

Cost: the rows that carried `someone`, which is none at all once every
author on screen has a name.

What changes for the user: a message whose author rho only learns about
afterwards says who sent it, in place, without leaving the conversation.
