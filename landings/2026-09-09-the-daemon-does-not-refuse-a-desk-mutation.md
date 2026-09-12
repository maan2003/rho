# The daemon does not refuse a desk mutation

The user, 9 Sep: "it is not job of daemon, remove it!" The desk store is
the client's. The daemon holds a copy so that clients can sync through it
and catch up, and a copy does not get a vote on what the user wrote.

Gone from `rho-daemon`'s desk cells: the verdict shape check, the check of
a log entry's before-values against the cells, "the verdict is not applied
by its mutation", "a verdict cannot remove a fact", and the rule that a
stamp may not run more than one past the daemon's global maximum. The
daemon merges every mutation it can decode. Last-writer-wins is the whole
of it and it needs no frontier to enforce: a stamp already counted merges
as itself, an older one loses to what beat it, a newer one wins.

What is left there is about the connection rather than the verdict, and it
breaks the connection the way the text path already does: a mutation must
carry this connection's own device, a connection must sync before it
writes, and a connection displaced by a newer window on the same device
may not write. Those keep one writer in one device's namespace, which the
CRDT does need. A mutation that cannot be decoded into the store at all is
logged and dropped; there is no answer for it any more and nobody is
waiting for one.

`ServerMessage::DeskMutationRejected` is gone from the protocol, and with
it every client path that existed to serve it: `mutation_rejected`,
`reject_desk_mutation`, `rebuild_view`, and the `pending` queue of
mutations kept for replay. Nothing else needed that queue. A rejection was
the only thing that ever took a write back out of the middle, so with it
gone the view is `confirmed` plus what has been applied to it, and it
stays that way.

Undo lands on the same footing, decided on the client: a change whose
before-value no longer stands is nothing to put back. Undo returns what
the verdict took away; it is not a way to reach past a write made after
it. An undo with nothing left to put back is not an undo.

Why it was ever there: the daemon was the store and the client was a view
of it, so the daemon was the place that could say no. The refusal only
ever took back writes the client had already shown the user, which is the
one thing it must not do. The stamp-jump rule in particular stood in the
way of writes made offline, which are the point of the client's replica.

Needs a daemon restart: the wire lost a message.
