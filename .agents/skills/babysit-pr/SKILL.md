---
name: babysit-pr
description: Babysit a GitHub pull request's CI by monitoring workflow runs, inspecting failed logs, and rerunning likely flaky failures up to 3 times. Use when the user asks to watch a PR, wait on CI, or retry flaky GitHub Actions failures.
---

# PR CI Babysitter

## Objective
Monitor a PR's CI until one of these conditions is reached:

- CI completes successfully.
- CI fails for a reason that looks branch-related rather than flaky.
- Likely flaky failures have already been rerun 3 times for the same PR without going green.
- The user interrupts or redirects the task.

This skill is intentionally narrow. It is only for CI babysitting and flaky reruns. It does not handle review comments, mergeability, or merging.

## Commands
```bash
oct ci status <pr-num|branch|pr-url> # get the current ci status for a pr
oct ci wait <run-id> # wait for ci job to finish
oct ci logs <run-id> # download logs for run, prints path of extracted logs
oct ci rerun <run-id> # rerun a failed workflow run
```


## Core Workflow

1. Start with `oct ci status <target>`.
2. If runs are still in progress, pick the relevant run ID and use `oct ci wait <run-id>`.
3. If CI finishes green, stop.
4. If one or more runs fail, inspect the failed run logs with `oct ci logs <run-id>`.
5. Classify each failure:
   - Likely flaky/unrelated: rerun.
   - Likely branch-related: stop and fix code instead of rerunning.
6. After a rerun, use `oct ci wait <run-id>` again for the rerun target.
7. Repeat until CI is green or the retry budget is exhausted.

## Flaky Heuristics
Treat a failure as likely flaky when logs suggest transient infrastructure or external dependency issues, for example:

- network timeouts
- dependency fetch failures
- GitHub Actions runner startup/provisioning issues
- temporary service outages
- obviously unrelated intermittent test infrastructure failures

Treat a failure as branch-related when logs suggest the PR introduced a deterministic problem, for example:

- compile errors
- test failures in changed code
- lint/typecheck failures
- snapshot mismatches caused by the PR
- deterministic script/config failures introduced by the branch

If classification is unclear, inspect the logs once and then make a call. Do not burn retry budget blindly.

## Retry Policy

- Maximum flaky retry budget: 3 reruns per babysitting session.
- Only rerun failed workflow runs, not successful ones.
- Do not rerun if CI is still actively running.
- Do not rerun the same failure indefinitely.
- After the third flaky rerun fails, stop and report that the failure appears persistent.

## Output Expectations
While babysitting:

- Report only meaningful state changes.
- Mention which run IDs were rerun.
- Mention when retry budget changes, for example `flaky retries used: 2/3`.

When stopping:

- State whether CI is green, branch-broken, or retry-budget-exhausted.
- Include the failed run IDs, if any.
- Include the extracted log path you used for diagnosis when relevant.

## Safety Rules

- Prefer `oct ci wait <run-id>` over hand-rolled sleep loops once you know which run you are monitoring.
- Do not rerun before reading at least one failed run's logs.
- Do not rerun deterministic branch failures.
- Do not exceed 3 flaky reruns.
