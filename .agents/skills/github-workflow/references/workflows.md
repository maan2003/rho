# ghapi workflow recipes

Use the client and approval rules in [the skill](../SKILL.md).

## PRs and comments

Create a PR with the repository's actual base branch:
```python
pr = await api.pulls.create(head="rho/CHANGE", base="BASE",
                            title="TITLE", body="BODY", draft=True)
```
Use `draft=False` or omit it for a normal PR. Edit with
`api.pulls.update(number, title=..., body=..., base=..., state=...)`.
Draft transitions use `api.pulls.set_draft(number, draft=True/False)`.
`api.pulls.review_decision(number)` returns `.review_decision` (`NONE` when null).

```python
diff = await api.pulls.get(number, headers_={"Accept": "application/vnd.github.diff"})
# Use application/vnd.github.patch for a patch.
comments = await api.issues.list_comments(number)
reviews = await api.pulls.list_reviews(number)
inline = await api.pulls.list_review_comments(number)
```

Conversation comments use `issues.create_comment(number, body=...)` and
`issues.update_comment(comment_id, body=...)`. Inline replies use
`pulls.create_reply_for_review_comment(number, comment_id, body=...)`;
inline edits use `pulls.update_review_comment(comment_id, body=...)`.
Use `issues.update(number, labels=..., assignees=..., milestone=...)` for
issue/PR metadata. Reviewers use `pulls.request_reviewers` and
`pulls.remove_requested_reviewers`. Re-check existing posts before retrying
an uncertain write; Octo does not deduplicate them.

## CI, logs, and reruns

After a push, inspect the current head:
```python
status = await api.pr_status(number)
# Without a PR: await api.check_status(commit_sha)
```
Inspect both `.check_runs` and `.statuses`. Legacy `.state == "pending"`
does not imply pending check runs. Empty lists do not prove CI passed.
Poll active checks until completion and report failures or unresolved checks.

- Run/job discovery: `actions.list_workflow_runs_for_repo(head_sha=sha)` and
  `actions.list_jobs_for_workflow_run(run_id)`.
- Failure detail: `checks.get(check_run_id)` and `checks.list_annotations(check_run_id)`.
- Logs: `actions.download_job_logs_for_workflow_run(job_id)` returns text;
  `actions.download_workflow_run_logs(run_id)` returns ZIP bytes.
- Approved reruns: `actions.re_run_job_for_workflow_run(job_id)`,
  `actions.re_run_workflow_failed_jobs(run_id)`, or `actions.re_run_workflow(run_id)`.
