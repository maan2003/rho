# ARCH-rho-agent: concrete runtimes with shared notebook mechanisms

## Ownership

The native `Agent` and `ClaudeLoop` are separate concrete runtimes. There is no
universal runtime trait and Claude Code is not a raw inference provider.
Each runtime serializes its controls, persistence, scheduling, and publication.

Native conversation authority is the append-only `NativeEvent` log. Provider
input, restart recovery, and presentation are disposable projections, not another
independently mutated history. Historical block records normalize at the read
boundary. Claude Code instead owns its session, history, and compaction; Rho
records bounded transcript observations, execution admission, output ownership,
and timing, and controls its in-process MCP server.

`rho-inference` owns native wire adaptation and validates the model action as
prose or one custom Python `exec`. `rho-claude` owns CLI transport and MCP protocol
adaptation. Neither adapter owns Rho's scheduling or persistence.

## Shared mechanisms, not shared runtime ownership

Every role exposes only the Python notebook, with at most one new exec per
model response. Earlier cells and jobs can remain live across later responses.
`rho-agent-tools` owns the concrete notebook, cells, jobs, and leased output;
host functions are callable inside Python, not through a top-level tool registry.
`notebook` builds host functions for both runtimes.

The provider call identity is the `ExecId` of the notebook execution. Command
identities and transport correlation IDs are separate. Claude MCP admission uses
the provider tool-use identity forwarded by the CLI, never its JSON-RPC ID.
Admission survives transcript rewind because rewind does not undo external effects.

Sources accumulate independently and expose facts, never scheduling decisions
([DECISION-pull-based-sources](DECISION-pull-based-sources.md)).
The shared pure `boundary` reads all facts, model patience, runtime availability
and standing, and the supplied clock. It has no store, provider, task, or native
phase dependency
([DECISION-boundary-is-the-only-decision](DECISION-boundary-is-the-only-decision.md)).
A cancelled or failed runtime waits for fresh input as specified by
[DECISION-stopped-agents-wait-for-fresh-input](DECISION-stopped-agents-wait-for-fresh-input.md).

## Durable ownership and recovery

Native Python units may execute while a response streams, only after admission
commits. Provider interruption is not EOF, and admitted effects are never replayed.
Settlement, model completion, and job completion are distinct facts
([SPEC-restart-recovery](SPEC-restart-recovery.md)).

Output reads lease a stable contribution until acknowledgment. Native inputs use
exec-specific replies and reports carrying complete text, images, and status;
wire tool classification belongs to inference adaptation. Native requests
commit contributions before acknowledging notebook buffers. Claude transfers them
to a durable outbox before transport; a failed handoff leaves them recoverable.
Retained batches participate in boundary scheduling and stop rules; when a later
exec is open, its reply carries retained output as reports. The replacement batch
owns both old and new contributions before fresh leases are acknowledged.
A recovered Claude batch becomes an attributed report, not another initial tool
result and never replayed source. Transport handoff is not proof of consumption.

One initial reply answers an exec; subsequent contributions are reports
([REQ-provider-transcript-protocol](REQ-provider-transcript-protocol.md)).
The notebook supplies its own words
([DECISION-the-core-never-speaks-for-a-tool](DECISION-the-core-never-speaks-for-a-tool.md)).
Reaping waits for final contribution acknowledgment, not merely Python return.

Loading alone starts no requests
([DECISION-a-restart-does-not-resume-by-itself](DECISION-a-restart-does-not-resume-by-itself.md)).
Instructions are code, not stored authority
([DECISION-instructions-are-code](DECISION-instructions-are-code.md)).
The notes role rotates the active provider window without replacing Python or
jobs; notes remain external effects
([DESIGN-context-rotation](../../../specs/DESIGN-context-rotation.md)).

## Read-only presentation

Native and Claude observations share the GUI projection, not a conversation
writer. Timing identifies provider first block, argument completion, response
completion, boundary, and transport handoff. It does not measure Python execution.
`WakeFacts` separately records source occurrence, observation, deadline, and trigger.
Native argument completion observes the provider's argument-end event, not
item completion or Python EOF. Live and committed tool rows use the same observed timing; later output must not
rewrite the original result's status or duration.
