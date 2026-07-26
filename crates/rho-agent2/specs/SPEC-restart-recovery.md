# SPEC-restart-recovery: Restart recovery

## Record justification

Recovery is split across `restore`, which derives what is owed from the event
log; `Phase::Idle`, which carries `owed` between load and the next request; and
`Agent::start_request`, which settles it — and none of the three can state the
contract the other two depend on.

## Contract

A restart is anything that stopped the process. A crash and a clean shutdown are
indistinguishable from the log and are treated alike, which is why the note
below says "restarted" rather than "crashed".

Restarting is not a state of its own. A loaded agent and a fresh one are both
`Phase::Idle`; loading only supplies `owed` differently.

### `owed`: what the next request must open with

No tool survives a restart, and nothing is recorded about what any of them did,
so every `ToolCall` in history that no `ToolResult` answers is a call nothing is
ever going to answer. `restore` derives that set by replaying history, adding
each call and removing each answered id, rather than by remembering which tools
were alive — history already says it, and a live-tool record would be a second
thing to keep true.

Membership does not depend on when the call was made. A call the model has moved
past is still unanswered, and a call from five turns ago whose tool ran the whole
time is the ordinary case rather than the exception.

`Agent::start_request` settles the whole of `owed`, and no earlier moment does.
It emits, ahead of everything the sources drain:

1. one `ToolResult` per owed call — empty, and `ToolOutputStatus::Cancelled`
   rather than a success, because an empty success reads as a command that ran
   quietly;
2. one `RESTART_NOTE`, as a user message, saying that every tool is gone —
   foreground and background alike — and that the empty results are placeholders
   rather than output. One note however many calls were owed, because the restart
   happened once, and prose belongs in a message rather than dressed up as
   output some tool never produced.

Settling at the first request rather than at load means an agent that is only
opened and read is never written to, and it puts the note beside the request it
explains. It also makes recovery idempotent: those call ids appear in a
`ToolResults` block of the resulting `AgentEvent::Sent`, so a second restart
derives an `owed` without them.

Nothing else may consume `owed`. In particular `boundary` never reads it: what a
request must carry is not a reason to make one.

### `standing`: when that request may happen

Always `Standing::Nothing` at load, whether or not a request was in flight when
the process stopped
([DECISION-a-restart-does-not-resume-by-itself](DECISION-a-restart-does-not-resume-by-itself.md)).
Recovery reads nothing out of the log about requests — only history and the
queues — which is why the log has no event for a request that ended without
replying. Two consequences worth stating:

- a request the process died during leaves no trace beyond the blocks its `Sent`
  already appended;
- a cancel does not survive a restart. `Standing` is in-memory only, so a
  cancelled agent that is reloaded comes back merely idle. In practice it stays
  quiet anyway, for the reason below.

`standing` then moves independently of `owed` for the rest of the agent's life,
and `owed` survives every move:

- a cancel gives `Standing::Cancelled` and keeps `owed`, because a cancel is not
  an answer; it has only stopped being a reason to send;
- fresh user input takes the agent back out of a stop without settling anything
  and without being written down: `Standing::stopped` compares the instant of the
  stop with the oldest thing the user has queued
  ([DECISION-stopped-agents-wait-for-a-person](DECISION-stopped-agents-wait-for-a-person.md));
- a retry gives `Standing::Asked`, hurrying the request rather than changing what
  has to be in it.

`Standing::Nothing` hands the question to the sources, which can mean *never* in
practice, and after a restart it commonly does: there are no tools and no model
turn, and the queues were drained by the `Sent` that preceded the model's last
reply, so the sources may name no moment at all. Such an agent is
`AgentActivity::Live` and silent until a person or a peer gives it something.
Whatever is owed is settled by that request when it comes, however long that
takes.

Required by
[REQ-provider-transcript-protocol](REQ-provider-transcript-protocol.md).
