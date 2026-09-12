---
name: git-subtree
description: Use when inspecting, updating, adding, or editing an upstream project vendored into rho as a squashed git subtree (vendor/zed, vendor/brush, vendor/iroh, vendor/noq, crates/senax-encoder).
---

# Git subtrees

Rho vendors selected upstream projects as squashed git subtrees. Their
source is in the tree and is first-class code: edit it directly when the
behavior belongs there (editor/GPUI behavior belongs in `vendor/zed`, not
in a Rho-side adapter), keep Rho-specific changes focused so they carry
across an upstream update, and avoid unrelated cleanup inside a subtree.

| Path | Upstream |
|---|---|
| `vendor/zed` | https://github.com/zed-industries/zed.git |
| `vendor/brush` | https://github.com/reubeno/brush.git |
| `vendor/iroh` | https://github.com/n0-computer/iroh.git |
| `vendor/noq` | https://github.com/n0-computer/noq.git |
| `crates/senax-encoder` | https://github.com/yossyX/senax-encoder.git |

The upstream baseline of a subtree is the newest squash commit reachable
from `HEAD`, found by its trailers:

```sh
git log -1 --grep='^git-subtree-dir: vendor/noq/*$' --format='%H%n%b'
```

Local commits above that baseline are intentional and must survive
updates.

## Updating

```sh
git subtree pull --prefix=vendor/noq https://github.com/n0-computer/noq.git main --squash
```

Pass the URL directly; no remote is needed. This fetches the upstream
revision, makes a new squash commit on the subtree's own line, and merges
it with `-Xsubtree`. Conflicts are ordinary merge conflicts: resolve in
favor of the current upstream API while keeping the intent of Rho's local
changes, then `git commit`. Watch for semantic collisions a textual merge
cannot see, such as protocol-number reuse or new upstream call sites for
a locally changed type. Keep the subtree update commit separate from the
compatibility fixes that follow it, and verify the consuming Rho crate
(`cargo check -p rho-gui`, for example), not only the subtree.

## Adding

```sh
git subtree add --prefix=vendor/NAME URL REVISION --squash
```

Keep the addition in a commit by itself; workspace membership, path
dependencies, Nix wiring and compatibility changes go in following
commits. The path need not be under `vendor/`: a project that is also a
Rho workspace crate may live under `crates/`, as Senax does.

## Avoid

- Git submodules, or importing an upstream's full history (no `--squash`).
- Replaying a fork's noisy history when a clean final delta can be ported.
- Treating vendored code as untouchable and layering a workaround in the
  wrong crate.
