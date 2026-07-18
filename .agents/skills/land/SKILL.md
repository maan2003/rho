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

2. Run the repository land command from the checkout root using the installed
   `rho` binary:

```sh
rho land
```

   Do not substitute `cargo run -p rho-cli -- land`: that executes the working
   copy's client, which may differ from the installed daemon/server version.
   If `rho` is not on `PATH`, install the flake package first rather than
   building an ad hoc landing client.

3. If land fails, read `.rho/log/land-failure.txt` and fix the underlying
   issue. Do not report success unless the land command exits successfully.

4. After a successful land, trust the command's success output. It prints the
   landed commit, the base bookmark's final target, and the current working
   copy state. Do not run extra `jj status`/`jj log` commands unless the
   output is missing, surprising, or the user explicitly asks for more detail.

## Notes

- `rho-ci` is the selfci worker binary; the land command is `rho land`.
- If `rho land` seals the working copy, `@` may become an empty child of the
  landed commit. This is normal.
