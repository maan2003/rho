# A rewrite Slack refuses goes back to the reader

*eng-bgkw, 2026-09-08.*

Editing a message in rho went through `Session::edit_message`, the one write
path that had no answer: `pub fn edit_message(...)` with no return, a bare
`return` when there was no connection, and a wire failure that set the
surface's error line and stopped there. The surface had already done what
cancelling does — closed the edit, put the set-aside text back in the
composer, untinted the message — before it knew whether Slack took the
rewrite. So a rewrite that did not land was gone: the message on screen
unchanged, the composer holding something else, and the reader's words
nowhere. Sending has never worked that way, and neither has sending with a
picture; both put the refused text back above whatever was typed since.

`edit_message` now answers with a `Task<Result<()>>`, the same shape as
`send`, and says its failure in the notice line — "slack: not connected, the
rewrite was not sent", or whatever Slack gave — beside the surface error it
already set. On a refusal the surface reopens the edit on the same message,
puts the rewrite back in the composer, and sets aside again what the edit had
set aside: the reader is back where they pressed enter. If they started
another edit or touched the composer while it was in flight, theirs wins and
the refused words go under it, which is the rule a refused send already
follows. What changes for the user: a rewrite that does not reach Slack is
something they can press enter on again, and they are told why, instead of
losing it in silence.
