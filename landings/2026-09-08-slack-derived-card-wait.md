# A card derived from the mirror waits from the message

*eng-bgkw, 2026-09-08.*

`derive_units` walks the mirror's own history and replays it through
`Model::note_message`, which takes the time the unit was first seen. The
caller handed it `now_ms()`, so a mention that had been sitting on disk since
Friday was recorded as first seen this second, and `wait_days` — the number
that orders the desk — read zero.

A plain restart hid this: `restore_units` puts the stored units back before
derive runs, and `record` keeps the `first_seen_ms` of a unit it already has.
What reads zero is a unit that does not already exist, and derive is not the
once-ever pass its name suggests. It runs again on every connect, once Slack
has said which threads are followed, and again whenever the reader opts a
channel into being handed over.

So the case the user actually hits is the one that was worst: opting a channel
in shows exactly the cards its unread mentions have earned, and every one of
them claimed to be brand new, sorting a mention from Friday in beside one from
ten minutes ago. The replay now hands `note_message` the message's own
timestamp, and the `now_ms` parameter is gone rather than corrected — a
function replaying history has no use for the current time, and while it took
one the wrong one could be passed. What changes for the user: a card says how
long it has actually been waiting, and the desk orders on that.
