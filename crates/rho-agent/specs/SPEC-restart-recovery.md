# SPEC-restart-recovery: Restart recovery

## Record justification

Recovery spans canonical conversation persistence, event-log replay, boundary
scheduling, and request assembly; none alone owns consistent history and how
interrupted executions are represented.

## Contract

A restart tears down and reloads an agent runtime, whether through idle eviction
or workset-process death. Other agents and retained sessions can survive an
individual runtime's eviction.
Crashes and clean shutdowns have the same conversation-recovery rules. A daemon
may record a coarse worker-failure event, but that is not an execution journal
or evidence of which unrecorded statements ran.

Restarting is not a state of its own. A loaded agent and a fresh one are both
`Phase::Idle`; loading supplies `owed` and a restart notice, not live execution state.

### `owed`: what the next request must open with

No notebook or managed job handle survives a restart. OS descendants may survive
an unexpected worker death and continue external effects. Only coherent conversation
boundaries are persisted; streaming source, unit admission, and settlement are
in memory. Recent execution, source, and output may be absent from the saved
conversation. External effects are not rolled back, and recovery must not claim
an exact executed prefix.

Every persisted call without a result is a call nothing is ever going to
answer. `load` derives `owed` by replaying canonical history, adding each call
and removing each answered identity. It does not reconstruct calls from
interpreter progress or execute saved source.

Membership does not depend on when the call was made. A call the model has moved
past is still unanswered, and a call from five turns ago whose tool ran the whole
time is the ordinary case rather than the exception.

`Agent::start_request` settles the whole of `owed`, and no earlier moment does.
It emits, ahead of everything the sources drain:

1. one `ToolResult` per owed call — empty, and `ToolOutputStatus::Cancelled`
   rather than a success, because an empty success reads as a command that ran
   quietly;
2. one note, as a user message, saying that every tool is gone —
   foreground and background alike — and that the empty results are placeholders
   rather than output. The note must explain that recent execution may be unrecorded and external
   side effects may remain; it must not suggest rollback or automatic replay. There is one general note however
   many calls were owed, because the restart happened once, and prose belongs in a message rather than dressed up as
   output some tool never produced.

Settling at the first request rather than at load means reading history never
writes placeholder tool answers, and it puts the note beside the request it
explains. It also makes recovery idempotent: those call ids appear in a
`ToolResults` block of the resulting `NativeEvent::RequestStarted`, so a second restart
derives an `owed` without them.

Nothing else may consume `owed`. In particular `boundary` never reads it: what a
request must carry is not a reason to make one.

### `standing`: when that request may happen

Always `Standing::Nothing` at load, whether or not a request was in flight when
the process stopped
([DECISION-a-restart-does-not-resume-by-itself](DECISION-a-restart-does-not-resume-by-itself.md)).
Recovery rebuilds saved conversation and queues, but never treats interrupted
work as permission to execute it again. An agent with saved history gets a
restart notice even when all its calls were answered: background jobs and
notebook globals are gone too. Two consequences:

- a response that never reached a conversation boundary may be absent entirely,
  even if some of its Python ran. A detected live interruption can record the
  accepted call before its result; a process crash need not preserve that call;
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
  ([DECISION-stopped-agents-wait-for-fresh-input](DECISION-stopped-agents-wait-for-fresh-input.md));
- a user-requested retry gives `Standing::Asked`, hurrying the request rather
  than changing what has to be in it; a recoverable provider failure supplies
  retry facts whose backoff and bounded budget are decided by `boundary`.

`Standing::Nothing` hands the question to the sources, which can mean *never* in
practice, and after a restart it commonly does: there are no tools and no model
turn, and the queues were drained by the `RequestStarted` that preceded the model's last
reply, so the sources may name no moment at all. Such an agent is
`AgentActivity::Live` and silent until a person or a peer gives it something.
Whatever is owed is settled by that request when it comes, however long that
takes.

Required by
[REQ-provider-transcript-protocol](REQ-provider-transcript-protocol.md).

### Context rotation

Only `eng-high-notes` schedules rotation. Role transitions reset unfinished
rotation state and append the same cancellation notice live and during replay;
committed retention boundaries remain authoritative regardless of current role.

A live rotation preserves Python and jobs; a restart never does. Replay retains
the last committed active-window boundary but abandons an unfinished preparation
exchange. The next independently triggered request explains that note writes may
already have happened; it must not automatically replay them. Preparing sends do
not acknowledge held input or unrelated live output. A committed rotation
clears old occupancy along with advancing the window. Notes are ordinary external
filesystem effects and are not rewound with conversation.

### Live stream failure

Transport failure is not source EOF. Stop admitting Python units, discard the
unadmitted suffix, and retain the active unit and its commands. Never transparently
replay admitted source. The original call must precede its one result in history,
including across failure, a later successful attempt, and restart.

Once any Python unit has been admitted, a recoverable provider failure ends the
model turn with the accepted prefix as its original `exec` call. Its cell and
commands are ordinary sources: completion, output batching, user input, mail, and
the cell's check-in and tool-wakeup policy determine the next request exactly as
after a completed model response. A transport retry deadline must not bypass
those sources. A pending `await` remains running, not uncertain execution.

Failures before any Python admission retain bounded transport-retry backoff and
add no call, result, or recovery notice to model history, including after restart.
For admitted code, the first tool result includes one concise annotation that the
response was interrupted while generating the call, execution was not cancelled,
and the call must not be replayed. Do not add separate user-role interruption
messages, discarded-source explanations, or source-range reports. Ordinary tool
output remains authoritative; restart recovery separately reports uncertainty
about execution whose live state has been lost.

Interpreter return, provider completion, and job completion remain distinct live
facts. Output ownership is acknowledged at conversation boundaries, not inferred
from interpreter return. None of these mechanisms requires a per-unit durable
execution journal.

### Claude ownership

Claude Code restores its own conversation; Rho does not replay it as native
provider input. Notebook admission and output ownership remain Rho's durable
facts. An admitted provider identity must never execute again, even after rewind.
A restart does not restore the Python namespace or its jobs.

Before releasing notebook output, Rho commits its attributed contributions.
A transport failure leaves that batch pending across restart. The next eligible
send carries it as a report, not a second initial result or a request to rerun
code. A completed handoff retires the batch; a crash between write and recorded
handoff can repeat the report, but cannot authorize repeating its side effects.
Handoff does not claim the remote model consumed the report. Slash commands retain
their command semantics; pending reports can wait for the CLI to become idle.
