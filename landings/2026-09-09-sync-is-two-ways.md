# Sync is two ways

A verdict is complete when the client writes it, and nothing replays a
write that was never sent. So a write made while the daemon was down, or
lost on the wire, reached no other device ever again: the client's own
QA showed it, the daemon it synced with afterwards never saw that write.

The store lives on the client, so the handshake carries both halves. The
daemon answers `DeskSync` with the cells above the client's `known`, and
`DeskSynced`'s delta carries the daemon's frontier as its version. The
client takes that frontier, asks its own replica for `since(frontier)`,
and sends what comes back as `ClientMessage::DeskCellsApply`. The daemon
merges it into the store like anything else, moves its frontier and pokes
the other clients.

An ordinary sync sends nothing back. The cells the daemon just answered
with are in the client's view and counted as its own, so `since` is
empty, and the replica-open path computes the same empty answer against
its own snapshot. Only a client holding writes the daemon has not got
sends anything, which is exactly the case this is for.

The daemon's side keeps the two conditions the mutation path keeps, for
the same reason: they are about the connection rather than about the
cells. The connection must have synced, and a connection displaced by a
newer window on the same device may not write. A push that does not merge
is logged and dropped.

What this is not yet: the client does not push on its own. It pushes when
a sync happens, which is at connect, at a poke, and at a resync. A client
that writes offline and never reconnects still holds the only copy, and
that is what a replica is for.

Needs a daemon restart: the wire has a new message.
