# SPEC-restart-recovery: Restart recovery

## Record justification

Recovery spans committed agent history, native wake scheduling, notebook-process checkpoints, and workset lifecycle; none alone owns the boundary between persisted conversation and live Python execution.

## Contract

A restart reloads the committed conversation prefix. A workset crash can lose its unflushed tail, and neither a crash nor a fresh notebook replays Python source or rolls back external effects. Loading an agent never by itself authorizes a new model request ([DECISION-a-restart-does-not-resume-by-itself](DECISION-a-restart-does-not-resume-by-itself.md)); a new input or a live source must supply the wake.

On an orderly drain or idle retirement, the native runtime flushes its agent log and acknowledges notebook-originated messages before checkpointing its separate notebook process. It publishes a restorable snapshot only after the dump completes. Restoring that snapshot keeps Python globals, running Python tasks, their open files, and their notebook-source state. The restored process may notify the agent of work completing, but restoring it does not repeat a model response. The next eligible model request must distinguish a restored notebook from a new one.

An unexpected process death, a missing snapshot, or a failed restore instead starts a fresh notebook. The next eligible request warns that Python state and managed work were lost and that external side effects may remain. A snapshot is consumed before restore so failure cannot replay the same checkpoint again. A failed or incomplete dump must not publish a snapshot.

Notebook-originated messages are ordered and acknowledged only after their conversation records have been durably committed. Preparation waits for already-sent messages and owner services; new messages after preparation remain queued in the snapshot and are sent after restore. Python code may keep running during a planned drain, but no new model code is admitted once the notebook is prepared for the dump. Checkpoints do not turn provider responses or host calls into replayable work.

Claude Code owns its own conversation and does not use the native Python notebook checkpoint. A native notebook's checkpoint does not change Claude's recovery rules.
