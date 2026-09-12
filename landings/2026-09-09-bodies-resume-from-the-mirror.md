# Bodies resume from the mirror

The cells resumed and the text did not. `DeskSynced` sent
`desk_cells.bodies()` — every note's words, in full, on every sync,
however small the delta — and the replica's copy was written down but
never asked about.

Now `DeskSync` carries a `BodyVersion` per note: the highest operation
counter this client holds from each text replica. A text replica numbers
its own operations in order, so that one number covers every earlier one
and the whole of "what I have" is one small map per note. The daemon
answers with the operations those versions lack, and says nothing at all
about a body with nothing new in it. A note the client has never held is
missing from the map and comes whole, as before.

Three consequences, each with a test:

- `BodySnapshot::version`, `since` and `merge` in rho-desk. A body
  answers a version with what it lacks, and nothing when it lacks none.
- The replica builds a history up out of the pieces it is sent. A delta
  that replaced the body on disk would throw away the words this client
  already had the moment somebody typed the next one.
- Text typed here is kept here. The daemon never sends a client its own
  operations back, so a note written on this client and only sent was
  gone from the mirror at the next cold open: it read empty until the
  daemon answered, and the sync asked for words this client wrote
  itself. `DeskCells::keep_text` writes the operation into the replica
  beside sending it, through a body-only write that leaves the host's
  cell version vector alone — local typing is not a claim to have seen
  anything of the store.
- The client says what it holds. Its per-note versions come from the
  histories the replica resumed with, so the first sync after a cold
  open already asks for the rest rather than the whole.

**Restart needed:** `ClientMessage::DeskSync` has a new field, so the
daemon and the client have to be the same build. An old client against
this daemon sends no map and is answered with every body, which is what
it expects; a new client against an old daemon is refused at decode.
