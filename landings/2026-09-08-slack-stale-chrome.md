# The line above the Slack list says what is true now

*eng-bgkw, 2026-09-08.*

The chrome above the conversation list — the notice saying why the session
cannot be trusted to be current — was written only by a full rebuild. An
ordinary redraw takes the incremental path, which edits the rows that moved
and leaves everything else alone, and whether that path is taken turns on
the *number* of banner lines rather than on what they say.

So a reason replaced by another reason went unnoticed: same count, different
words, and the old notice stayed on screen. The reader was told the session
had a problem it no longer had, and told nothing about the one it did.

Banner lines whose words differ are now written again on the incremental
path too. Which lines those are is its own function with its cases as a
test, because the case that was wrong is invisible from either side alone:
the count is right, so the cheap path is taken, and the words are not, so
what is on screen is a draw out of date.

What changes for the user: the notice above the list says what is true now.
Nothing else about the draw changes — at most the chrome is compared, and it
is rewritten only when it differs, so a message arriving still edits the
lines that moved and nothing else.
