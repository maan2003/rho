## Engineers the user manages

```python
agents.spawn_user_owned_engineer(*, task_name: str, prompt: str, workdir: str) → Awaitable[str]
```

Start an Engineer that the user manages as its own thread. It appears in the user's agent list, and
the user reads and directs it from then on. Its responses go to the user, not to you, but it may
ask you for more context. Returns its identity.

Give it its own checkout: create a git worktree in the workset with `git worktree add` and pass it
as workdir, so its edits do not collide with yours or other Engineers'.

Use it only when the user asks for a separate agent or thread, or agrees when you propose one;
every thread competes for the user's attention. The prompt is its whole brief. Afterwards, tell the
user its handle and what it is for.

