---
name: clone-store
description: Make jj git clone instant and jj git fetch local through the shared clone store, for sub-agents, experiments, or any task needing an isolated full checkout.
---

# Clone stores

A clone store gives every clone a full private repo — own refs, op log,
config, and fetch/push/gc freedom — without O(repo) disk or network per
clone. Design: `CLONES.md`. It is a cache behind the ordinary commands;
there is no separate workflow to learn.

## Inside a Rho agent

The daemon has already set `JJ_STORE_SOCKET` (and mounted the store root
read-only). Just use jj:

```sh
jj git clone https://github.com/org/repo      # instant, born on latest main
jj git fetch                                  # from the local mirror
jj git push                                   # to the real remote
jj workspace add ../repo-child                # more workspaces of the same clone
```

`/src` is your working directory; clone whatever you need into it. Fetches
never touch the network from your namespace: the daemon keeps every store
fetched in the background, and a fetch asks it to refresh first. Clone the
real upstream URL, not a local path, so `origin` is pushable.

## Outside the daemon

Set `JJ_STORE` (or `git.clone-store`) to a directory and use the same
commands; the first clone of a URL initializes its store, later ones reuse
it. Optionally run `jj store serve --socket PATH` and point
`JJ_STORE_SOCKET` at it so stores stay fetched and clients never write them.
`jj store fetch [URL...]` and `jj store list` maintain a root by hand.

## Rules

- Never delete or prune a store's `git` object database; clones borrow
  those objects. Store configuration disables auto-gc — do not override it.
- Everything inside your own clone is fair game (`jj op undo`, `git gc`,
  reindexing); its blast radius is that clone.
- A clone is not tied to its store's location by relative paths, but it is
  by absolute path: do not move a store root while clones exist.
