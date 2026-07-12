---
name: pr-workflow
description: Submit completed code changes as a GitHub pull request and monitor its CI through completion.
---

# Pull request workflow

Use Jujutsu for repository operations and `octo` for GitHub operations. Carry
this workflow through from completed local changes to a pull request with a
terminal CI result; creating the pull request alone is not completion.

## Submit the change

1. Verify the implementation and inspect `jj status`, `jj diff`, and `jj log`.
2. Identify the intended change. Do not push incidental working-copy changes.
3. Push it below the only branch namespace Octo permits:

```bash
jj git push --remote origin --named rho/CHANGE_NAME=REVSET
```

4. Create the pull request:

```bash
octo pr create --head rho/CHANGE_NAME --title "TITLE" --body "BODY"
```

For a stacked pull request, pass its parent bookmark with `--base`. Never push a
normal branch or tag through Octo.

Creating the pull request is the first user-visible milestone, not the end of
the workflow. As soon as it exists, report its URL before beginning the
potentially longer CI wait. If you are a spawned agent with a parent, use
`message_agent` to send the parent a concise milestone containing the PR URL
and that CI monitoring is underway; continue working after sending it. The
parent can relay that update to the user while you monitor CI.

If GitHub access fails, report the original error. A missing local Octo socket
means the Rho daemon is unavailable; a missing token requires `rho octo init`.
Do not silently switch credential sources.

## Monitor CI

After creating the pull request, monitor its CI automatically. Do not ask for a
separate confirmation and do not stop merely because the pull request exists.

```bash
octo ci status PR_OR_BRANCH
octo ci wait RUN_ID
octo ci logs RUN_ID
octo ci rerun RUN_ID
```

Wait for active runs to reach a terminal state. For a failure, read its logs
before deciding what to do. Rerun only likely infrastructure flakes, at most
three times. Fix deterministic failures, push the correction to the same
branch, and monitor the new CI run. Do not alter correct code merely to make a
test green.

Send another parent update when CI requires a flake rerun, a deterministic fix,
or user action. Keep milestone messages factual and sparse; do not narrate
routine polling. The final report must still include the terminal CI result.

## Finish

Report the pull request URL and final CI result. Clearly identify any unresolved
failure or blocker. Do not describe the workflow as complete while CI is still
running or an actionable deterministic failure remains.
