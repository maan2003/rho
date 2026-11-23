### Engineers the user manages

```python
agents.spawn_user_owned_engineer(*, task_name: str, prompt: str, workdir: str | None = None, thread: int | None = None) → Awaitable[str]
```

Start an Engineer that the user manages in its own thread. It appears in the user's agent list, and its responses go to the user, not to you, though it may message you for context. Returns its identity.

The prompt posts to the user as the opening message of a new thread named `task_name` (in `thread`, or in your origin thread's project root when omitted; pass `thread` if you were not started from a thread). Because it posts to the user, write `prompt` in Simplified Technical English.

Give it its own checkout: create a git worktree with `git worktree add` and pass its absolute path as `workdir` (defaults to your cwd) so its edits do not collide with yours.

Use this only when the user asks for a separate agent or thread, or agrees when you propose one; every thread competes for the user's attention. The prompt is its whole brief. Afterwards, tell the user its handle and purpose.

