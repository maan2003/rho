# A failed send says so once, not forever above the history

*eng-bgkw, 2026-09-08.*

`Loaded::error` is drawn by the conversation surface as a line above the
transcript, and it is cleared by the next page of history that lands — by
`load_older`, `load_newer` and `open_at`, which is what it is about. Three
write paths also set it: `send`, `send_file` and `edit_message`. Nothing
about a write clears it.

So a send Slack refused left a red line at the top of the conversation, a
long way from the composer where it happened, and left it there: after the
reader retried and got through, after the message they were told had failed
was plainly on screen. It went away only if they scrolled far enough back to
buy a page of history. An upload that failed had it worse — that line was its
only word anywhere, so a picture that did not go silently became a permanent
complaint about the conversation.

The three write sites are deleted rather than given a way to clear the line,
so the field has one meaning and its three set sites and three clear sites
are the same three paths. `send` and `edit_message` already say a refusal in
the notice line; `send_file` says one now, and was the last write path with
nothing to tell the reader. What changes for the user: a write that fails is
said once, beside where they were typing, and the line above the history is
about the history again.
