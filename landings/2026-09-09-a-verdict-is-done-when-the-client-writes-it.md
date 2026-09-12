# A verdict is done when the client writes it

The desk store lives on the client. The daemon holds a copy so that
clients can sync through it and catch up, which is the user's stated goal
and now the operative design. A verdict was still waiting on that copy to
say it had happened, so this takes that out.

`ServerMessage::DeskMutationAccepted` is gone from the protocol, and with
it `pending_tree_verdicts`, `pending_tree_undos` and `pending_desk_texts`,
the three maps that held a verdict, an undo and a paste's new text until
the answer came back. Everything they were holding happens where the
write is made: `complete_tree_verdict` and `complete_verdict_undo` are
called from `submit_tree_verdict`, the filing path and `undo_verdict`
directly, and a new note's body is typed into its buffer as soon as the
write that created it is applied. The undo is armed, the dealer is told,
the card leaves and the bar says the words, all inside the keystroke.

`DeskCells::apply` writes to the replica on disk as well as to the view,
before the message goes out. The replica used to hold only what the
daemon had acknowledged, on the reasoning that an unacknowledged write
was the daemon's to accept or refuse; it cannot refuse any more, so a
write the reader has been shown must survive a restart whether or not a
daemon ever heard about it.

The cold-open gate reads "the client's replica is loaded" rather than
"the daemon has answered": `is_synced` is `is_loaded` and
`HostNodes::desk_synced` is `desk_loaded`, and `Workspace::new` opens
each host's replica off disk before a socket exists. The rule it guards
is unchanged. Nothing is dealt until the store has been read, because a
client that has not read it cannot say the user did not put this agent
away yesterday; what changed is that the reading no longer waits on a
round trip.

Two smaller things fell out. `verdict_todo` echoed `todo: 7d` after
submitting, because the verdict's own words waited on the daemon; the
words are said as the verdict is made now, so the pace line would only
take the card's name back off the bar and it goes. And
`phone_blocks_navigation_while_a_tree_verdict_is_pending` tested a state
that no longer exists, so it goes with the guards it covered: nothing is
pending long enough to block a flick.

Next on this path, not done here: a note's body does not resume from the
mirror. Every sync still carries the whole desk's prose, and the reader's
text is the one thing the replica cannot give them at open. That needs
per-body versions.

Needs a daemon restart: the wire lost another message.
