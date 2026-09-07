# rho-fake-slack: several clients, the server checking they agree, and the binary form

*eng-bgkw, 2026-09-07. Third landing on `rho-fake-slack`, after the store and
the read side, and the socket and the living schedule.*

With one client a fake Slack is a mock with better manners. With several it
becomes the thing worth having: a workspace that can be wrong in the way real
Slack is wrong — one client hearing about a message the others did not, a read
cursor that moved for someone and not for everyone else. This landing makes
the server the judge of that, and gives it a binary form so the rig can point
rho at it.

## The server is the only place that can check

A client cannot tell you whether it missed something; not having heard is
exactly what it does not know. So the bookkeeping lives on the server, on the
one side that knows what it sent to whom.

Every socket registers a `Watcher` when it connects: an id, the user, and the
sequence the workspace was at when it arrived — what happened before that was
never its to hear, so a client that joins late is not held to it. Every frame
now carries what it is news about (`Frame { value, message, cursor }`),
because whoever built the frame already knew which conversation it touched and
parsing that back out of the JSON per client would be silly. Each connection
records what it was handed: two counters and one entry in a map keyed by the
conversation.

`observations()` compares, and hands back typed values rather than a verdict:

- `Behind { client, by }` — has not been handed everything published yet.
  Ordinary for a moment after a write, which is what `settled(timeout)` waits
  out before a test asserts anything.
- `Missed { client, frames }` — was cut, and by how much.
- `Message { client, channel, told, published }` — the newest message in a
  conversation is not the newest message this client was told about.
- `Cursor { client, channel, told, held }` — the reader's cursor moved and
  this client was told something else, or was not told at all.

An empty list is the claim: what one client was told, all of them were told,
and it matches what the server holds.

## A client that stops reading is named, not left looking slow

The first version of this found a real hole in the previous landing. A client
that stops reading its socket does not lag the broadcast — it blocks the
server's write to it, so the connection never gets as far as noticing it is
behind, and it sits there reading as merely `Behind` forever. A frame now has
two seconds to be accepted; past that the client is cut and the frames it will
never see are counted against it. "One client quietly stopped hearing about
the workspace" is the failure this crate exists to catch, so it cannot be the
one failure it reports as patience.

## The binary form

`rho-fake-slack --port 7300 --rate 5` serves the same store, the same seed and
the same schedule as the in-process form, on a port known in advance. It
prints what the rig pastes:

```text
seed              1 (20 conversations, 5000 messages, 2.610646ms)
RHO_SLACK_API_BASE=http://127.0.0.1:7391/api
socket            ws://127.0.0.1:7391/socket
control           http://127.0.0.1:7391/control
rate              5 happenings/s
```

`POST /control` takes the same typed `Action` the in-process handle takes —
now covering time as well as refusals (`advance`, `live`, `still`, `refuse`) —
and `GET /control` hands back the observations as JSON. One enum, two
transports, no second vocabulary to keep in step:

```text
$ curl -XPOST .../control -d '{"action":"advance","happenings":25}'
{"happenings":25,"ok":true}
$ curl .../control
{"published":36,"clients":[],"disagreements":[]}
```

## The cost, and the numbers

Per frame per client it is two counters and one map entry, so a frame costs
O(clients) and a client costs O(conversations it heard about) — never a walk
of the workspace or of a history. The comparison happens when someone asks for
it, against the newest thing published per conversation rather than against a
log of everything that ever happened, so it is O(clients × conversations
something happened in).

Measured at the default 300 conversations / 450,000 messages, with real
`rho-slack` clients each reading its own socket:

| | 8 clients | 32 clients |
|---|---|---|
| connect | 26 ms (3.3 ms each) | 67 ms (2.1 ms each) |
| one happening | 4.7 µs | 4.5–5.5 µs |
| everyone caught up after 18,415 frames | 18 ms | 19–36 ms |
| the agreement check | 165 µs | 286 µs warm, 1.1–4.2 ms cold |
| frames delivered | 18,415 each | 18,415 each |
| resident | 184 MiB | 191 MiB (≈350 KiB a client) |

At a rate rather than a burst — 2,000 happenings/s for three seconds, which is
what a workspace actually does — 32 clients took 171,612 frames between them
and all caught up.

One honest finding from the burst. Applying 20,000 happenings as fast as the
server can (≈220,000/s, far past anything a real workspace does) can push one
of 32 clients past the 8,192-frame backlog, and the server cuts it and says
`Missed { client, frames }`. That is the check working rather than a client
bug: at any rate a workspace could plausibly produce, nobody is cut.

## What is next

Change 5: keys and card-handing out of `rho-gui/src/slack.rs`.

Still open from the earlier landings: the world comes from this crate's own
`world::build` until eng-8gpr's generator lands here as `world::generate`, so
one seed means one world *here*, not yet everywhere.

Gate: clippy `-D warnings` clean, `cargo fmt --check` clean, workspace suite
green; rho-fake-slack 19 tests and 1 doc test.

A few lines in other people's crates came along, because the full-feature
clippy pass does not go green on main without them and this landing's gate
runs it. No behaviour changed in any of them:

- `rho-gui/src/walk.rs`: a `type Rejection` for the two eight-field tuples
  `type_complexity` rejects.
- `rho-qa/src/telemetry.rs`: `Stage` no longer names `transforms` or
  `affected_offsets` and `Work` no longer names `start_ns`. The daemon still
  emits all three and serde drops what the struct does not name, so they are
  waiting rather than gone — **eng-8gpr**, add them back when there is a
  summary that reads them.
- `rho-qa/src/rig.rs`: a redundant `&` in a `format!`.
- `rho-gui/src/tests/fold_widening_check.rs`: the deliberately inverted range
  is built as `Range { start, end }` rather than written as `900..100`, which
  is a mistake everywhere except where it is the input under test.
