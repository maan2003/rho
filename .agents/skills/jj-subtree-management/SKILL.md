---
name: jj-subtree-management
description: Use when adding, adopting, inspecting, updating, or editing a dependency managed as a jj subtree in rho.
---

# JJ subtree management

Rho keeps selected upstream projects as managed jj subtrees. Their source is
present in the repository so it can be maintained at the layer that owns the
behavior, while `.jjsubtree.toml` records the upstream baseline used by
`jj subtree update`.

## First-class code

- Treat every path from `jj subtree list` as part of the codebase.
- Edit subtree code directly when it is the correct ownership layer. Do not
  add a Rho-side adapter, copy, or override merely because the implementation
  lives under `vendor/`.
- This is especially important for `vendor/zed` and `vendor/jj`: editor/GPUI
  behavior belongs in Zed, and repository/workspace semantics may belong in
  jj. The same principle applies to Brush, iroh, noq, Senax, and future
  subtrees.
- Keep Rho-specific changes focused and suitable for carrying across an
  upstream update. Avoid unrelated cleanup in a subtree.

## Inspecting subtrees

Use jj for all repository operations:

```sh
jj subtree list
cat path/to/subtree/.jjsubtree.toml
jj git remote list
jj status
```

The `source_commit` is the upstream baseline, not the newest Rho commit that
touches the directory. Local commits above that baseline are intentional and
must survive updates.

## Adding a subtree

1. Add and fetch the canonical upstream with jj if its revision is not already
   present:

   ```sh
   jj git remote add NAME URL
   jj git fetch --remote NAME
   ```

2. Resolve and inspect the exact upstream revision. Prefer the upstream default
   branch for a new live dependency, or a release tag when Rho intentionally
   tracks a release.
3. Import it at its permanent path:

   ```sh
   jj subtree add --source REVISION PATH
   jj subtree list
   ```

4. Keep the subtree addition in a commit by itself. Put workspace membership,
   path dependencies, Nix wiring, and Rho compatibility changes in following
   commits.

The destination does not have to be under `vendor/`. A project that is also a
Rho workspace crate may remain under `crates/`, as Senax does.

## Adopting an existing vendored tree

Do not replace accumulated local work with a fresh upstream checkout.

1. Use history, release metadata, and a tree comparison to identify the exact
   upstream revision from which the existing tree was imported.
2. Verify that the original imported tree matches that revision. Account only
   for deliberate import differences such as omitting an upstream lockfile.
3. Let `jj subtree add` generate canonical metadata at a temporary path, copy
   only its `.jjsubtree.toml` to the existing tree, and restore the temporary
   path:

   ```sh
   jj subtree add --source REVISION tmp/subtree-adoption
   cp tmp/subtree-adoption/.jjsubtree.toml EXISTING_PATH/.jjsubtree.toml
   jj restore tmp/subtree-adoption
   jj subtree list
   ```

4. Commit only the new metadata. Confirm the existing source files did not
   change.

Do not adopt a guessed baseline. If provenance cannot be established, stop and
document the uncertainty rather than creating misleading update metadata.

## Updating a subtree

1. Start with a clean working copy and fetch upstream using jj.
2. Review the upstream target, then update:

   ```sh
   jj git fetch --remote NAME
   jj subtree update --source REVISION PATH
   ```

3. Resolve conflicts in favor of the current upstream API while preserving the
   intent of Rho's local changes. Detect semantic collisions that textual merge
   cannot find, such as protocol-number reuse or new upstream call sites for a
   locally changed type.
4. Keep the upstream subtree update separate from compatibility fixes where
   practical. Split independent carried changes into focused commits rather
   than importing a fork's noisy history.
5. Verify the consuming Rho target, not only the subtree crate.

## Verification

At minimum:

```sh
jj status
jj subtree list
```

Also search manifests and build files for stale Git pins after switching a
dependency to local paths. Run the narrowest relevant consumer check, such as:

```sh
cargo check -p rho-gui
cargo check -p rho-workspaces
```

Inspect the final commit stack with `jj log` and ensure temporary adoption or
comparison directories are gone.

## Avoid

- Git commands for subtree management.
- Git submodules or Git subtree history imports.
- Replaying a poor fork history when a clean final delta can be ported.
- Editing or deleting `.jjsubtree.toml` without understanding the recorded
  baseline.
- Treating vendored code as untouchable and layering a workaround in the wrong
  crate.
