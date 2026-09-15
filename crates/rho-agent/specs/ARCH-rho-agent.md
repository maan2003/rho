# ARCH-rho-agent: concrete runtimes with shared notebook mechanisms

## Ownership

The native `Agent` and `ClaudeLoop` are separate concrete runtimes. There is no
universal runtime trait and Claude Code is not a raw inference provider.
One workset process contains its agents' runtimes, notebooks, local tools,
jobs, provider transports, retained terminals, and interactive shells. The daemon alone owns the shared
database, account and route policy, naming tasks, workset allocation, pool,
subscriptions, and UI projection. Each runtime serializes its own controls and
scheduling. Native events replicate through an ordered, bounded background writer;
Claude's durable operations retain acknowledged daemon services.
There is no in-daemon runtime fallback.

The append-only `NativeEvent` log owns the recoverable conversation prefix. The
native worker owns an ordered volatile tail; live provider input includes that
tail while restart recovery projects only committed transactions. Requests and responses use the same canonical grouped entries consumed by
inference. A temporary atomic database migration rewrites historical raw rows
at their original positions, preserving response boundaries, IDs, provider data,
and context-window offsets; normal replay does not normalize legacy events. Claude Code instead owns its session, history, and compaction; Rho
records bounded transcript observations, execution admission, output ownership,
and timing, and controls its worker-local MCP server.

`rho-inference` owns native wire adaptation and validates the model action as
prose or one custom Python `exec`. `rho-claude` owns CLI transport and MCP protocol
adaptation. Neither adapter owns Rho's scheduling or persistence.

One private Senax Unix connection multiplexes agent services and controls with
workset control and terminal/shell traffic. Inference policy has one daemon
subscription and one shared client per workset, not per agent. Its pushes and
RPC replies share workset FIFO ordering; agent retirement does not close it.
Bounded fragments preserve per-port
order; routing and fair writes do not await runtime work. Completion publication
does not await recipient acceptance, keeping reciprocal subscriptions outside
serialized actor-loop dependencies. Lost persistence acknowledgements stop the
runtime; uncertain mutations are not retried. Native inference does not wait for
healthy commit acknowledgments: request batches include pending timing, and
response batches include usage accounting in the same transaction. Explicit
barriers synchronize rewind, profile changes, terminal publication and shutdown.
A crash may lose the unflushed tail, but cannot expose a partial database batch.

The workset process builds one filesystem namespace before starting threads.
Normal execution inherits it. Claude launcher children alone clone it to install
private provider overlays; no generic agent namespace or setup thread is needed.
Mode changes exclude new admission, require settled agents and no live sessions,
then drain and replace the whole workset execution.

Agent retirement requires the runtime's serialized permission and fences new
admission; coalesced observations are not authority. Activation and retirement
serialize per agent even across caller cancellation. The ID is reused only after
runtime and daemon-handler drain, without incarnation IDs or extra sockets.
Unloading an agent or detaching a GUI leaves workset terminals and shells alive.
Workset failure loses all local ephemeral execution. Normal shutdown drains
owned work; crashes may leave descendants and external effects behind. Recovery
reconstructs conversation, never interpreters, jobs, or automatic execution.

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

Native Python units may execute while a response streams. `PythonExec` owns
the live admitted/settled/successful-prefix ledger and reports it directly;
the agent decides whether to admit a ready unit and owns provider-source
validation and canonical conversation publication. Admission and
settlement are ordered in memory, without per-unit database writes. Provider interruption is not EOF, and admitted effects are never replayed.
Only coherent conversation boundaries are persisted; a restart may lose recent
execution and output without rolling back external effects. Settlement, model
completion, and job completion are distinct live facts
([SPEC-restart-recovery](SPEC-restart-recovery.md)).

Output reads lease a stable contribution until acknowledgment. Notebook replies and reports project once into canonical native inputs carrying
complete text, images, and status. Historical function-call evidence remains
replay data, not an active tool capability. Native requests enqueue owned
contributions before acknowledging notebook buffers, without waiting for disk.
Claude transfers them to a durable outbox before transport; a failed handoff
leaves them recoverable.
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

Both runtimes make one bounded text-only naming attempt from the first task.
The attempted fact survives rewind; cancellation or failure never retries it.
Existing names win, and loading or viewing an agent never initiates naming.
Titles do not write conversation context or classify turn outcomes. Runtime
status and peer completion delivery remain independent of naming.

Native and Claude observations share the GUI projection, not a conversation
writer. Timing identifies provider first block, argument completion, response
completion, boundary, and transport handoff. It does not measure Python execution.
`WakeFacts` separately records source occurrence, observation, deadline, and trigger.
Native argument completion observes the provider's argument-end event, not
item completion or Python EOF. Live and committed tool rows use the same observed timing; later output must not
rewrite the original result's status or duration.
