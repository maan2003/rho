---
name: delegate-engineering
description: Delegate independent implementation work to isolated Engineer workspaces and integrate the results.
---

# Delegate engineering work

`spawn_engineer` is installed dynamically in code mode rather than declared in
the always-present `exec` documentation. Call it with this interface:

```ts
declare const tools: { spawn_engineer(args: {
  // Complete, self-contained task for the sub-agent.
  prompt: string;
  // Short user-visible kebab-case label for the sub-task.
  task_name: string;
  // The child's working set. Omit to fork your complete working set.
  workdirs?: Array<{
    // Absolute repository or directory path.
    repo: string;
    // Optional jj revision from which to fork the isolated checkout.
    revset?: string;
  }>;
}): Promise<string>; };

declare const tools: {
  interrupt_engineer(args: {
    engineer_id: string;
  }): Promise<string>;
  wait_agent(args: {
    timeout_seconds?: number;
  }): Promise<string>;
};
```

Use `spawn_engineer` only when the user explicitly requests delegation or an
active workflow authorizes it. Delegate a concrete task that can proceed
independently.

Once an Engineer owns a task, your role for that task is coordination only
until it reports completion. Do not independently investigate, edit, or verify
the same task while the Engineer is working; that duplicates work and weakens
the ownership boundary. You may work concurrently only on a clearly disjoint
subtask with separately assigned ownership. Otherwise, send necessary
follow-ups and use `wait_agent` rather than doing the delegated work yourself
or yielding a final response while it is still running.

Each jj workdir is always isolated in the child. Omit `workdirs` to fork the
parent's complete working set. Otherwise list the repositories the child needs;
`revset` starts an isolated checkout from a specific revision. Plain directories
cannot be isolated and remain shared.

Give the Engineer an outcome-focused, self-contained prompt. It already receives
repository guidance, skills, tools, and environment context.

Use `message_agent` for follow-ups and `interrupt_engineer` to stop its current
turn. Results arrive as mail. After the Engineer reports completion, inspect
completed jj work through the workspace handle reported by `spawn_engineer`, for example with
`jj diff -r '<workspace>@' --stat`, and integrate it with an explicit `jj edit`
or `jj squash --from '<workspace>@' --into @` only when you intend to take over
that work.
