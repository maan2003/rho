---
name: delegate-engineering
description: Delegate independent implementation work to Engineer sub-agents in your workset and integrate the results.
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
  // Where the child works: at most one entry. Omit to share your working
  // directory.
  workdirs?: Array<{
    // A directory in your workset, absolute or relative to your working
    // directory.
    repo: string;
    // Optional jj revset: the child gets its own jj workspace of that
    // repository, on a new change atop the revset (for example `@`).
    revset?: string;
  }>;
}): Promise<string>; };

declare const tools: {
  interrupt_engineer(args: {
    engineer_id: string;
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
follow-ups and use `wait` rather than doing the delegated work yourself
or yielding a final response while it is still running.

The child always works in your workset. Omit `workdirs` and it shares your
working directory: you both see every edit immediately, so only do that when
one of you is reading rather than editing. Give it a `revset` (`@` for your
current change) and it gets its own jj workspace of the repository, made
beside yours in the workset (`<repo>-2`, `<repo>-3`, ...); that is the right
setup for concurrent edits, and no further worktree, clone, or copy is needed.
You can also prepare a workspace yourself with `jj workspace add` and name its
directory in `repo`.

Give the Engineer an outcome-focused, self-contained prompt. It already receives
repository guidance, skills, tools, and environment context.

Use `message_agent` for follow-ups and `interrupt_engineer` to stop its current
turn. Results arrive as mail. After the Engineer reports completion, inspect its
work in the directory `spawn_engineer` reported: `jj log` and `jj diff` there
show its changes, and its workspace's commits are visible from your own
workspace as `<workspace name>@`. Integrate with an explicit `jj squash --from
'<workspace name>@' --into @` (or `jj new` on top of its commits) only when
you intend to take over that work.
