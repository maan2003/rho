# Done is a cursor; mute and snooze are not

A verdict that a later message can undo has to say so. Done does: it is a
position, "up to here", and anything past it is news. Mute and snooze are
about the thing rather than the stream, and rho was letting both be undone
by the next thing that happened.

A Slack snooze recorded where the unit stood and treated anybody writing
during it as voiding it, so the card came straight back: "not until Monday"
meant "until somebody writes". That voiding is gone; the recorded position
stays as a note of what the user looked away from. An agent's attention read
a running turn first — a running turn is the agent's court — so a muted
agent came back to Working the moment it moved; the mute is now read before
the turn. And Home's running list asked neither question, listing every
agent with a turn in flight, so muting or snoozing a working agent did
nothing a reader could see until the turn ended; it now asks the one
question the dealer already asks per card: has the user put this agent away.

The same change retires `hide`. `shift-d` wrote the same `DeskVerdict::Mute`
that `x` writes, so one verdict had two keys, two names and two words in the
log — and the second name had spread into `rho-agents`, where an agent was
"hidden" while everything else called it muted, which is how some lists came
to filter one and not the other. The hide entry, `Command::AgentHide`, the
hide half of the agent-done key and the `hide` label are gone, and the
filing is `agent_muted`. A muted agent is left out of Home's running list,
the finder and the draft's start field; its handle still resolves and its
row is still on the map, which is where the mute is taken back.

Tests: a muted agent stays Quiet through a turn (`rho-agents`); a muted and
a snoozed agent are each off Home while their turn runs; a muted agent that
ends a turn asking is not a card; a muted agent is not offered as a start
target while its handle still reaches it; and the Slack snooze outlasts a
newer message from someone else, which is the test that used to assert the
opposite.
