# DESIGN-context-rotation: Tool-history eviction and transcript access

## Status

Implemented for the native `eng-high-notes` role.
The historical name remains; there is no notes-writing or preparation workflow.
Other native roles and manual Compact retain provider compaction. Claude Code
continues to own its context management.

## Evict tool exchanges before compacting

At the model's compaction threshold, evict the oldest completed tool exchanges
from active provider context until the estimated occupancy has 40000 tokens of
headroom. Protect a recent suffix of approximately 40000 tokens and all live or
unanswered calls. Age an exchange by its last contribution, not its call alone.
Only exchanges whose call and result are both available after the latest
compaction are candidates. Keep user messages, assistant prose and reasoning.

Record eviction as a typed append-only transcript item containing call IDs.
Inference omits those calls, results and updates, and rejects continuations
established before the eviction. Never mutate the original transcript.
A small developer notice explains the removal and points to Python history.
No early notice, preparation response, input holding or note inventory is needed.

If removal cannot bring estimated occupancy below the threshold, send a normal
provider compaction trigger. Estimates select candidates; subsequent provider
usage remains authoritative. Manual Compact always bypasses eviction.
Neither operation replaces Python or running jobs. A restart never restores
execution or replays its effects.

## Recover evidence through lazy Python history

The notebook exposes a backend-owned read-only transcript sequence, with a
snapshot per Python execution. Access materializes individual items, not the
whole transcript; only printed or displayed content enters model context.
Python interface guidance documents the schema and sequence semantics.

History includes original tool arguments and recorded results, including those
evicted or preceding compaction. It is evidence, not privileged instructions.
No dedicated search API, mandatory checkpoint format or notes-writing guidance
is introduced. Existing filesystem notes are left untouched.

## Replay and role changes

Evictions remain effective after restart and when switching to another role.
Old context-rotation boundaries remain readable and authoritative; changing
policy must not resurrect context already discarded by an old version.
Legacy preparation events remain readable but are never resumed automatically.
The durable event vocabulary is extended, not rewritten; see
[DECISION-history-only-branches](../crates/rho-agent/specs/DECISION-history-only-branches.md).
