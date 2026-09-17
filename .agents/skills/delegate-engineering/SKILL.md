---
name: delegate-engineering
description: Delegate independent implementation work to Engineer sub-agents in your workset and integrate the results.
---

# Delegate engineering work

Start an Engineer with the Python interface:

agents.spawn_new_engineer(*, task_name: str, prompt: str, workdir: str | None = None) → Awaitable[str]

task_name is a short user-visible kebab-case label. prompt is the complete, self-contained task.
workdir selects an existing absolute directory inside your workset; omission inherits your working
directory. The call starts immediately and returns an awaitable identifying the Engineer.

Use `agents.spawn_new_engineer` only when the user explicitly requests delegation or an
active workflow authorizes it. Delegate a concrete task that can proceed
independently.

Once an Engineer owns a task, your role for that task is coordination only
until it reports completion. Do not independently investigate, edit, or verify
the same task while the Engineer is working; that duplicates work and weakens
the ownership boundary. You may work concurrently only on a clearly disjoint
subtask with separately assigned ownership. Otherwise, send necessary
follow-ups and use a check-in rather than doing the delegated work yourself
or yielding a final response while it is still running.

The child always works in your workset. Without workdir it starts in your
working directory: you both see every edit immediately, so only share a
directory when one of you is reading rather than editing. For concurrent edits,
make the child a checkout of its own first and pass its absolute path as workdir:

```sh
git worktree add ../<repo>-<task> -b <task>        # a branch off your HEAD
```

The daemon creates no checkouts for children; the worktree, clone, or copy
is yours to make, inside the workset.

Give the Engineer an outcome-focused, self-contained prompt. It already receives
repository guidance, skills, tools, and environment context.

Use `agents.message` for follow-ups and `agents.cancel` to stop its current
turn. Results arrive as mail. After the Engineer reports completion, inspect its
work in the directory you gave it: `git log` and `git diff` there show its
changes, and its branch is visible from your own checkout since worktrees share
one repository. Integrate with an explicit `git merge <task>`, `git rebase`, or
`git cherry-pick` only when you intend to take over that work.
