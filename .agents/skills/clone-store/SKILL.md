---
name: clone-store
description: Create instant private jj clones and workspaces from a shared clone store — for sub-agents, experiments, or any task needing an isolated full checkout.
---

# Clone-store workspaces

A clone store gives every agent a full private clone—own refs, op log, config,
and fetch/push/gc freedom—without O(repo) disk or network per clone. Design:
`CLONES.md`; implementation: `jj_lib::clone_store` and the rho fork's
`jj store` CLI.

Requires a jj build with `jj store`. If `jj store --help` fails, use rho's
bundled jj or ask for a redeploy.

## Daemon-managed workflow (normal)

Do not manually allocate paths for Rho agents. The daemon's `Worksets` manager
owns `~/src/.rho`:

- shared stores: `~/src/.rho/stores/<repo>`;
- generated Worksets: `~/src/.rho/worksets/<id>/src/`;
- durable primary/order record: the `worksets` table in rho-db;
- host-frame Checkouts: `.../src/<name>` with `.stores/<repo>` plumbing;
- view-mode agent paths: `/src/<name>` and `/src/.stores/<repo>`;
- exposed-mode paths: temporarily `/ws/<name>` and `/ws/.stores/<repo>`.

Use the normal spawn/agent tools. A child needing its own checkout is forked by
Worksets from the parent's snapshotted commit and starts on a fresh jj change.
A shared checkout request joins the existing Workset. Never pass or derive
`~/src/.rho` bookkeeping paths as model-facing workdirs.

## Manual CLI workflow (out of daemon only)

The commands below remain useful for experiments, debugging the jj primitive,
or other workflows that do not run through the daemon. Choose an isolated root;
these paths are examples, not daemon conventions:

```sh
root=~/src/.jj-stores/rho
checkout=~/src/ws/fix-index-race

jj store init "$root" octo://github.com/maan2003/rho.git
jj store fetch "$root"                    # optional prefetch
jj store clone "$root" fix-index-race
jj store workspace "$root" fix-index-race "$checkout" \
    --name fix-index-race                  # optional: --at COMMIT_HEX
```

Each command prints a JSON record. `--at` defaults to the clone's trunk. Final
paths only hold complete artifacts, so interrupted commands are safe to retry.
Inside the checkout, plain `jj` and `git` work normally.

Manual clones collaborate through the remote. For a local handoff without a
push, fetch from the clone's private Git directory:

```sh
git fetch "$root/clones/fix-index-race/git" <ref>
```

## Rules

- Never delete or prune a store's shared `git` object database; clones borrow
  those objects. Store configuration disables auto-gc—do not override it.
- Everything inside a private clone is fair game (`jj op undo`, `git gc`,
  reindexing); its blast radius is that clone.
- Delete manual clone/checkouts only after their work is pushed or abandoned.
- Do not manually mutate daemon-owned `~/src/.rho`; use Worksets/agent APIs.
