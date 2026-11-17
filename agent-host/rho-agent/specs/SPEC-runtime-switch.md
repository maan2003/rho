# SPEC-runtime-switch: Moving an agent between runtimes

## Record justification

A switch spans the host's `SetSpec` handling, the store's native cursor, the
Claude loop's session start, the provider's replay format and the Python
guard, and none of them alone sees both runtimes' histories.

## Contract

`set_spec` with a runtime of the other kind switches the agent. It happens
only while the agent has no work: the host retires its loaded runtime first
and refuses the switch otherwise, so the history handed over is complete.
An auto workset refuses a switch to native, which it could not load
([permissions](../src/worker/permissions.rs)).

The new runtime's history is rebuilt from the log, the one record both
runtimes leave with roles intact (`handoff`): what each `Request` carried,
and what the model said and ran. What only one provider can read stays
behind: Claude's thinking and signatures, OpenAI's encrypted reasoning and
compactions. The rebuilt history opens with one line in Rho's `system` voice
that says what moved and what was lost, and keeps the newest whole turns
within `HANDOFF_BYTES`; a call is never parted from its answer.

- **Claude to native.** The `AgentChanged` and the native rows that follow it
  are one transaction. The store starts the native context at that
  `AgentChanged` (`NativeCursor::from`), so Claude's rows are never replayed
  as native ones. The replay starts at the turn Claude last compacted in,
  and Claude's own summary of what came before joins the opening line. Each
  reply becomes a `Step` whose carry the provider builds
  (`Backend::transcribed`), so the next report pairs with its call like any
  other.
- **Native to Claude.** The Claude loop, finding no transcript for its new
  session after a switch, writes one from the native era before Claude Code
  starts and resumes it. The loop does it because only it knows the name
  Claude Code calls exec by under the workset's permissions. A native
  compaction cannot be read, so the rows it summarized are replayed instead.
  A call the switch cut off is answered, since Claude takes no call without
  its result.

The notebook does not survive a switch: the new runtime starts as after a
restart ([SPEC-restart-recovery](SPEC-restart-recovery.md)).
