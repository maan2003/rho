---
name: clone-store
description: Create instant private jj clones and workspaces from a shared clone store — for sub-agents, experiments, or any task needing an isolated full checkout.
---

# Clone-store workspaces

A clone store gives every agent what a human contributor has: a full private
clone — own refs, op log, config, free to fetch/push/gc — without O(repo)
disk or network per clone. Design and internals: `CLONES.md` in the rho
repo; implementation: `jj_lib::clone_store` and the `jj store` CLI in the
jj fork.

Requires a jj build that has `jj store` (the rho package's bundled jj; if
`jj store --help` fails, the deployed jj predates it — use
`vendor/jj/target/release/jj` from a rho checkout or ask for a redeploy).

## Conventions on this machine

- Stores: `~/src/.jj-stores/<repo-name>` (one per repository, shared by all
  agents; `~/src` is the big bcachefs volume).
- Workspaces: `~/src/ws/<name>` until rho's mount layer lands, then `/ws/<name>`.
  Name after the task or the agent that will use it.
- Clone ids: one clone per agent or workstream, named after it
  (e.g. `eng-h6u7` or `fix-index-race`). Clones are cheap (~60ms) —
  prefer a fresh clone over sharing one.

## Commands

```sh
# Once per repository (slow: full fetch of the remote):
jj store init ~/src/.jj-stores/rho octo://github.com/maan2003/rho.git

# Optional prefetch; clones also fetch on their own:
jj store fetch ~/src/.jj-stores/rho

# Per agent/task — instant, born at the store's last-fetched state:
jj store clone ~/src/.jj-stores/rho <id>

# Materialize a checkout (a real colocated git worktree + jj workspace):
jj store workspace ~/src/.jj-stores/rho <id> ~/src/ws/<name> \
    [--name NAME] [--at COMMIT_HEX]
```

Each command prints a JSON record of what it created. `--at` defaults to the
clone's trunk (`main@origin`). All commands are safe to interrupt and retry:
final paths only ever hold complete artifacts.

## Giving a workspace to a sub-agent

Create the clone and workspace yourself, then pass the workspace path as the
sub-agent's workdir:

```ts
tools.spawn_engineer({
  task_name: "fix-index-race",
  prompt: "...",
  workdirs: [{ repo: "/home/maan2003/src/ws/fix-index-race" }],
});
```

Inside the workspace, plain `jj` and `git` just work — it is a stock
colocated checkout. The clone is private to that agent: its commits, op log,
and refs are invisible to everyone else until pushed.

## Integrating results

Clones collaborate through the remote, like coworkers on different machines:
the sub-agent pushes a bookmark, you fetch it in your own checkout. For local
handoff without a push, add the clone's git dir as a remote and fetch:

```sh
git fetch ~/src/.jj-stores/rho/clones/<id>/git <ref>
```

## Rules

- Never delete or prune inside `~/src/.jj-stores/*/git` — clones borrow the
  store's objects. (The store's own config already disables auto-gc; just
  don't fight it.)
- Everything *inside a clone* is fair game: `jj op undo`, `git gc`,
  reindexing — blast radius is that clone only.
- Deleting a clone or workspace directory you created is safe cleanup once
  its work is pushed or abandoned.
