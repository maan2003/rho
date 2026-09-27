# SPEC-notebook-tasks: Task ownership and reporting

## Record justification

Python coroutine and context behavior, Rust command lifetime, and session reports jointly implement task ownership, so no one local artifact can state the complete contract.

Each exec and `asyncio.create_task` call creates an independently identified task. A child inherits its creator's Python context but directs output to its own session. Threads and callbacks inherit the scheduling task's output destination without holding it open. Commands belong to the task that starts them; successful code implicitly awaits its commands, while raised code does not. Cancelling a task kills its commands and reports a quiet cancellation. A raised task propagates its original exception to awaiters; retrieved failures do not produce delayed reports. Failed commands do not raise on await and remain reportable unless already delivered.

Sources share scrambled session labels. Initial completed work reports plain output; work announced while running later reports under its session label. The latest exec reports first, followed by other sources in creation order. Output is retained only while the notebook lives, capped at 4 MB per source and 50 MB per notebook, with oldest retained sources dropped first when full.
