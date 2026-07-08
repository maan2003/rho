---
name: land
description: Use when the user asks to land the current change, land a jj stack, or run the repository land flow after committing.
---

# Land a change

Use this skill when the user asks to land the change, land the current stack,
run the land flow, or otherwise publish/move the base bookmark through the
repository's validated landing command.

## Workflow

1. Make sure the intended change is committed/described in jj.
   - If the working-copy commit has a description and file changes, it is the
     commit to land.
   - Do not move `main` manually before running land; `rho land` computes the
     stack above the configured base bookmark and moves the bookmark after
     checks pass.

2. Run the repository land command from the checkout root:

```sh
rho land
```

If `rho` is not on `PATH`, run the CLI crate directly:

```sh
cargo run -p rho-cli -- land
```

3. If land fails, read `.rho/log/land-failure.txt` and fix the underlying
   issue. Do not report success unless the land command exits successfully.

4. After a successful land, verify jj state if useful:

```sh
jj status
jj log -r 'main|@|@-' --no-graph --template 'commit_id.short() ++ " " ++ bookmarks ++ " | " ++ description.first_line() ++ "\n"'
```

## Notes

- `rho-ci` is the selfci worker binary; the land command is `rho land`.
- If `rho land` seals the working copy, `@` may become an empty child of the
  landed commit. This is normal.
