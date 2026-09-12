# DECISION-the-core-never-speaks-for-a-tool: Tools are cooperative and describe themselves

Authority: confirmed, 2026-07-26, maan2003

## Decision

Every block of tool output in the transcript is the tool's own words. The core
appends nothing to them, summarises nothing, and invents nothing when a tool
says little or nothing at all.

What that settles, case by case:

- `ToolSession::first_output` is required, because a provider rejects a call no
  result answers ([REQ-provider-transcript-protocol](REQ-provider-transcript-protocol.md)).
  It is taken at the first drain after the call, whatever the tool is doing, so
  a tool that has nothing yet says *that* in its own words. The core will not
  supply an empty success, a "still running", or anything else on its behalf.
- `ToolSession::more_output` is entirely the tool's choice, including whether to
  mention that it has ended. The core does not annotate an ending, so a tool
  that exits without saying so leaves the model believing it is still running.
  That is the tool's bug to fix.
- `ToolHaste` is a hint for `boundary` and nothing else. It never reaches the
  transcript, not even paraphrased, and nothing outside the decision reads it —
  which is why it does not say whether the call has been answered or whether
  the tool can be forgotten. Both were once inferred from it, and both were
  wrong to infer: a request that skipped a working tool left its call
  unanswered and was rejected whole.
- A call is forgotten when `ToolSession::done` says so, asked after the drain
  has taken whatever the tool had. A tool whose work is over but which still
  owes a summary reports `ToolHaste::Ended` and answers `false`, and the core
  waits — it does not decide on the tool's behalf that there is nothing more to
  hear.

## Rationale

Tools are written against this crate rather than found in the wild, so a tool
that will not describe itself is a tool to fix, not a case to carry code for.
The alternative costs more than it looks: prose the core writes reads to the
model exactly like output some tool produced, and there is no way for the model
to tell the two apart. A tool describes its own ending better than
`[the tool has exited]` ever did — it knows the exit status.

It also removes a piece of state. While the core annotated endings it had to
remember which endings it had announced, or repeat itself forever; that memory
was a third `ToolCallAnswer` variant that existed for no other reason. Reaping
on the tool's own report needs nothing remembered.

The cost is stated above and accepted: a silent tool is invisible when it dies.
