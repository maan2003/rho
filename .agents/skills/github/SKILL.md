---
name: github
description: Work with GitHub repositories through Rho: clone or fetch with jj, push rho branches, create pull requests, and monitor or rerun CI.
---

# GitHub workflow

Use Jujutsu for repository operations and `oct` for GitHub operations.

## Clone and fetch

Octo keeps the GitHub token in the Rho daemon and can read every repository the
token can access:

```bash
jj git clone octo://github.com/OWNER/REPOSITORY /explicit/destination
jj git fetch --remote origin
```

Choose the destination explicitly and do not accidentally clone inside another
repository. Jujutsu clones colocated repositories by default.

If access fails, report the original error. A missing `OCTO_SOCKET` means the
Rho daemon environment is unavailable; a missing token requires `rho octo
init`. Do not silently switch credential sources.

## Push and create a pull request

Octo permits pushes only below `refs/heads/rho/*`:

```bash
jj git push --remote origin --named rho/CHANGE_NAME=@
oct pr create --head rho/CHANGE_NAME --title "TITLE" --body "BODY"
```

Inspect `jj log` first and push the intended change, not an incidental working
copy. For a stacked pull request, pass its parent bookmark with `--base`.
Never push a normal branch or tag through Octo.

## CI

```bash
oct ci status PR_OR_BRANCH
oct ci wait RUN_ID
oct ci logs RUN_ID
oct ci rerun RUN_ID
```

Wait for active runs before diagnosing failures. Read failed logs before
rerunning. Rerun only likely infrastructure flakes, at most three times; fix
deterministic failures instead.
