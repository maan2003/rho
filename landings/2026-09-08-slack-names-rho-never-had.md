# A name rho does not have is asked for

*eng-bgkw, 2026-09-08.*

The roster is fetched once, on connect: `users.list` in one call, so that
mentions get names without a request per author. Anyone who joined after
that is not in it. `Model::author` looks the id up, finds nothing, and
falls back to `someone` — and it kept falling back for the rest of the run,
because nothing ever asked Slack who that was. `Client::user_info` existed
and had no caller anywhere in the crate.

Observed against the fake: with a conversation open, a person added to the
workspace after startup posts, and the message draws as
`someone: hello, just joined`. It stays `someone` for as long as rho is
running.

`Session::learn_names` is the caller. Every place a message enters a
surface — a live frame, a page, a hole being filled, a scroll back — hands
it what arrived; it keeps the author ids the roster has no name for, asks
`users.info` once for each, and puts the answer in the model and the
mirror. `asked_names` is what makes it once: a person is asked about on
their first message and never again, whether or not the ask answered, so a
channel full of a stranger's messages is one request and a refused request
is not retried on every page.

Cost: the author ids in what just arrived that are new, which is bounded by
the messages that arrived and is nothing at all once the roster covers
them.

What changes for the user: someone who joined since rho started has their
name on their messages, rather than reading as `someone` until the next
restart. A name that arrives after a row is already drawn is the other half
of this and is its own change.
