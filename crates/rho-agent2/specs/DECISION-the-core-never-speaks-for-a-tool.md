# DECISION-the-core-never-speaks-for-a-tool: Tools are cooperative and describe themselves

Authority: confirmed, 2026-07-26, maan2003

## Decision

Every block of tool output in the transcript is the tool's own words. The core
appends nothing to them, summarises nothing, and invents nothing when a tool
says little or nothing at all.

What that settles, case by case:

- `ToolSession::first_output` is required, because a provider rejects a call no
  result answers ([REQ-provider-transcript-protocol](REQ-provider-transcript-protocol.md)).
  A tool that ends without doing anything says so in its own words; the core
  will not supply an empty success on its behalf.
- `ToolSession::more_output` is entirely the tool's choice, including whether to
  mention that it has ended. The core does not annotate an ending, so a tool
  that exits without saying so leaves the model believing it is still running.
  That is the tool's bug to fix.
- `ToolReport` is a hint for `boundary` and nothing else. It never reaches the
  transcript, not even paraphrased.
- A call is forgotten when it reports `ToolReport::Exited` and the drain beside
  the reap has taken its last words — not when the core has announced the
  ending, because the core no longer announces one.
- A tool holding output while it reports `ToolReport::Running` is holding it
  back, and by doing so is saying it has not answered the call yet. That is the
  only thing the core reads as "the model is still waiting on this", so a tool
  that answers early and then goes on blocking has told the model it is
  unblocked. Its bug, and the only place the difference is knowable.

## Rationale

Tools are written against this crate rather than found in the wild, so a tool
that will not describe itself is a tool to fix, not a case to carry code for.
The alternative costs more than it looks: prose the core writes reads to the
model exactly like output some tool produced, and there is no way for the model
to tell the two apart. A tool describes its own ending better than
`[the tool has exited]` ever did — it knows the exit status.

It also removes a piece of state. While the core annotated endings it had to
remember which endings it had announced, or repeat itself forever; that memory
was a third `Told` variant that existed for no other reason. Reaping on the
tool's own report needs nothing remembered.

The cost is stated above and accepted: a silent tool is invisible when it dies.
