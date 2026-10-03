# PR delivery, reviews and CI

Use the notebook client and approval rules in [the skill](../SKILL.md).

## Create or update a PR

Use the actual base branch, not an assumed `main`. `draft=True` creates
a draft; omit it or use `draft=False` for a normal PR.

```python
pr = await api.pulls.create(
    head="rho/CHANGE_NAME", base="BASE_BRANCH",
    title="TITLE", body="BODY", draft=True,
)
status = await api.pr_status(pr.number)
```

Update a PR with `api.pulls.update(number, base=..., title=..., body=...)`.
It also accepts `state="open"`/`"closed"` and `maintainer_can_modify`.
Creating a PR is not proof that CI passed; report its URL before a long wait.

Set draft/ready state with `api.pulls.set_draft(number, draft=True/False)`.
Read full diffs with `api.pulls.get(number,
headers_={"Accept": "application/vnd.github.diff"})` (or `.patch` media).
Manage PR labels/assignees/milestones through `api.issues.update` using the PR number;
manage reviewers with `api.pulls.request_reviewers` and
`api.pulls.remove_requested_reviewers`.

PR merge, branch update and review-dismissal operations are not exposed.

## Reviews and comments

Poll `api.issues.list_comments(number)` for conversation comments,
`api.pulls.list_reviews(number)` for verdicts, and
`api.pulls.list_review_comments(number)` for inline threads. Reviews include
reviewer ID, type and association; inline comments include review ID and
parent reply ID.

`api.pulls.review_decision(number)` returns GitHub's overall GraphQL verdict
in `.review_decision` (`NONE` when null). This is an Octo-only helper,
not an upstream REST endpoint.

Reply with `api.issues.create_comment(number, body=...)` for a conversation,
or `api.pulls.create_reply_for_review_comment(number, comment_id, body=...)`
for an existing inline thread. Re-check the thread before retrying an
uncertain write: Octo does not deduplicate replies.

Edit existing text with `api.issues.update_comment(comment_id, body=...)` or
`api.pulls.update_review_comment(comment_id, body=...)`; do not post correction
comments just because editing was absent from the old client.

`api.issues.create(title=..., body=...)` creates issues;
`api.issues.update(number, state=..., labels=..., milestone=...)` updates them.
Explicit `None` clears nullable fields; omission leaves them unchanged.
Review submission and new inline comments are available through upstream
methods, but require authorization for that specific live write.

## Track CI for the current head

Poll `api.pr_status(number)` until checks finish. It reads the PR head and
combines `.check_runs` with legacy `.statuses`. Inspect **both**:
`.state` describes only legacy statuses and can be `pending` even when
Actions checks passed. Every subsequent push starts a new CI obligation.
For a direct branch update without a PR, use `api.check_status(sha)` with
the pushed 40-character commit SHA.

For a failure:
- Inspect `api.checks.get(id)` and `api.checks.list_annotations(id)`.
- Find the head's run with `api.actions.list_workflow_runs_for_repo(head_sha=sha)`.
- Read jobs with `api.actions.list_jobs_for_workflow_run(run_id)`.
- Get text logs with `api.actions.download_job_logs_for_workflow_run(job_id)`;
  `api.actions.download_workflow_run_logs(run_id)` returns ZIP bytes.

List operations may require pagination. Reruns change shared CI state:
get **explicit approval for the specific live rerun** before calling
`api.actions.re_run_job_for_workflow_run(job_id)`,
`api.actions.re_run_workflow_failed_jobs(run_id)` (failed and dependent jobs),
or `api.actions.re_run_workflow(run_id)` (whole run).

There is **no automatic PR feedback or CI wakeup** after an agent stops;
resume only when directed by the user or parent agent. Report blockers
rather than claiming CI passed. For a spawned Engineer, send the PR URL
and terminal CI result to its parent for relay. The final report must say
whether CI reached a terminal result and name any unresolved failure.
