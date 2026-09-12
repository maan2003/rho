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

The child always works in your workset and starts in your working directory:
you both see every edit immediately, so only share a directory when one of
you is reading rather than editing. For concurrent edits, make the child a
checkout of its own first and tell it where to work in the prompt:

```sh
git worktree add ../<repo>-<task> -b <task>        # a branch off your HEAD
```

The daemon creates no checkouts for children; the worktree, clone, or copy
is yours to make, inside the workset.

Give the Engineer an outcome-focused, self-contained prompt. It already receives
repository guidance, skills, tools, and environment context.

Use `message_agent` for follow-ups and `interrupt_engineer` to stop its current
turn. Results arrive as mail. After the Engineer reports completion, inspect its
work in the directory you gave it: `git log` and `git diff` there show its
changes, and its branch is visible from your own checkout since worktrees share
one repository. Integrate with an explicit `git merge <task>`, `git rebase`, or
`git cherry-pick` only when you intend to take over that work.
