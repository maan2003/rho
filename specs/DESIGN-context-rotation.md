# DESIGN-context-rotation: Filesystem notes and context rotation

## Status

Implemented for the native Rho runtime. This document records the behavior
and its rationale, not an implementation plan.
It does not change Claude Code's own context management.

## Rotate context rather than summarize it

Replace summary-driven compaction with automatic context rotation. Keep a
recent stretch of conversation verbatim and let the model preserve older
knowledge in ordinary notes. Rho neither generates a summary nor requires a
structured checkpoint.

Why: the model can decide what matters while doing the task. Keeping recent
conversation verbatim avoids repeatedly compressing already-compressed
summaries, while files let the model recover older knowledge selectively.
This borrows Codex's model-managed continuity, not its backend-dependent notes
tools.

Rotation changes the active inference context, not the durable transcript.
Activation is a typed context item referencing the retained start in full block
history; the inference adapter interprets it and rejects pre-activation provider
continuations. Early notices alone do not activate rotation. There is no separate
request-window offset.
Stored history remains available under the existing
[history preservation constraint](../crates/rho-agent/specs/DECISION-history-only-branches.md).

## Notes are ordinary shared workset files

Give each workset one durable notes directory at `state/notes`, outside its
code checkouts. All agents in the workset, including children, share it. Expose
its location in instructions; the model can use `Path(...)` normally, without
a prebound notes variable. Python and shell
file operations are the notes interface: no notes CRUD tools, virtual
filesystem, database-backed memory service, or mandatory file format.

Encourage incremental notes during work, not just emergency note-taking at
rotation. The model owns their organization and contents. Notes survive
rotation and daemon restart; discarding the workset removes them. Agents build
on existing notes and preserve one another's contributions.

Why: real files already provide the editing and organization tools agents
need, avoid polluting repository changes, and remain inspectable outside the
harness. Notes belong to the workset rather than an agent or provider account.
The existing workset state mount makes them available in every agent namespace;
no separate notes mount or registration is needed.

## An early notice establishes the retention boundary

At the model rotation threshold minus 40000 tokens, insert a developer notice that becomes the
start of the next retained context. Continue normal work between this notice
and the threshold. At rotation, retain the notice, all subsequent conversation,
and the preparation exchange, alongside current system instructions.

The distance between notice and rotation determines the approximate retained
budget. The boundary is fixed when announced, rather than retrospectively
moving to produce an exact token count. A recognizable notice identifies the
boundary; turn counts and nearby text can supplement it but are not required
for the model to locate it.

Why: the model knows in advance exactly which older context needs preservation,
without interpreting invisible IDs or counting a long conversation. Token
usage advances in chunks, so a stable, protocol-valid boundary matters more
than exact tail size. Thresholds must leave room for preparation and repair
below the model's hard limit.

## Preparation is a dedicated opportunity

At the rotation threshold, give the model one preparation-only response to
preserve anything needed from before the retention notice. Following
[Codex's checkpoint guidance](https://github.com/openai/codex/blob/main/codex-rs/prompts/templates/compact/prompt.md),
ask for concise, structured notes covering progress and decisions, constraints
and preferences, clear next steps, and critical data or references. This is
continuity through files, not a generated handoff summary. Hold newly queued
user messages, mail, and unrelated tool output for the fresh window. Existing
background work continues and buffers output.

Wait for the preparation cell to finish its writes, not merely for model
generation to end. Its own tool results remain part of preparation, with one
bounded repair opportunity for failure while headroom permits. Do not wait
for unrelated background jobs. Explicit interrupt or cancellation stops the
agent; it is not ordinary input to defer past rotation.

Why: preparation should not compete with fresh tasks, and a switch must not
race a note write. Bounded repair avoids knowingly discarding context after a
failed write without allowing unbounded preparation.

The Rust runtime owns the transition and its durable state. Preserve live
Python state, original tool-call identities, pending work, and unanswered user
requests. Rebuild provider continuation against the retained context rather
than accidentally retaining the old server-side context. Continue respecting
[provider transcript obligations](../crates/rho-agent/specs/REQ-provider-transcript-protocol.md)
and [restart recovery](../crates/rho-agent/specs/SPEC-restart-recovery.md):
a live rotation preserves execution, whereas a daemon restart does not.

## Reorient the model with a small notes inventory

After rotation, insert a developer message explaining what was retained and
that live execution survived. Include the notes directory and a bounded list
of recently modified note files, newest first, with relative paths, line
counts, and byte counts. Display counts as plain integers without thousands
separators. Treat filenames as data, not instructions.

Use filesystem modification times; do not track reads or introduce a watcher
or journal for recency. Do not automatically inject note contents. The model
reads the files it needs using ordinary tools.

Why: the inventory provides immediate recovery landmarks and helps estimate
reading cost without refilling the context with all notes. Developer messages
accurately identify harness guidance rather than inventing user requests.
A snapshot at rotation avoids continually changing the prompt as files change.

## Keep the initial surface small

Use automatic rotation with a retained recent tail. Do not initially expose
a model-callable rotation API, add a full-reset mode, or build semantic memory
indexing. Their value can be evaluated after this design is exercised.
