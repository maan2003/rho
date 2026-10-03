# Octo-backed ghapi Python sources

Based on [ghapi](https://github.com/AnswerDotAI/ghapi) 2.1.5,
[`81b28a5`](https://github.com/AnswerDotAI/ghapi/commit/81b28a5325b311e9878a676a57fef801093242f6)
(Apache-2.0; see `LICENSE`).

`gh_spec.json` contains complete, unmodified upstream metadata for 76 REST
operations: 57 reads and 19 writes. Writes cover PR collaboration, issue and
issue-comment creation/editing, and the three existing CI reruns. It also defines Octo-only `pulls.review_decision`
and `pulls.set_draft` helpers backed by fixed, typed GraphQL operations.

REST reads and writes use the selected metadata as an explicit method/path-template
allowlist. Octo forwards query parameters and request bodies unchanged and relays
upstream responses without schema validation; GitHub validates its API. The Python
client still checks declared argument names and missing required parameters.

Omission or `UNSET` leaves a field out; `None` sends JSON null, and `False`, `0`,
and `""` remain values. Octo does not drop or interpret nulls: GitHub decides
whether a field accepts null. There is no schema generator, REST request/response
model, or write wrapper.

The selected PR surface excludes merging (sync/async), head-branch updates,
and dismissing another review. Generic GraphQL, Git ref writes, repository
administration, and credential operations are not exposed. Issue writes are
limited to `create`, `update`, `create_comment`, and `update_comment`. Use
`issues.update` for labels, assignees, and milestone assignment. Dedicated issue
dependency, sub-issue, suggestion, field-value, locking, pinning, deletion, and
label/milestone administration writes are unavailable; their selected reads remain.
API availability
does not authorize a live write: agents still need the user's specific approval.

`core.py` retains upstream operation generation, response decoding,
owner/repo overrides, and `pr_status`/`check_status` presentation.
`check_status` fetches all check-run pages. `pr_status(number)` remains CI for
one PR, not the gh CLI's personal PR overview.

Requests use the host's Octo Unix socket without Python-side credentials.
Both async and sync generated methods reject unknown keywords, missing required
parameters, and undeclared `query_`/`body_` fields. Use `result['items']` for
search rows: attribute `.items` is a dict method. Full PR diff/patch reads use
`headers_={"Accept": "application/vnd.github.diff"}` or
`application/vnd.github.patch`. Draft/ready transitions use
`await api.pulls.set_draft(number, draft=True/False)`.

Octo preserves selected pagination/cache/rate-limit headers, empty responses,
and text/binary response media. Existing job/run log download redirects
are fetched once on the host without credentials; signed URLs never reach the
agent. Repository responses nested in PR/search data omit `temp_clone_token`.
This is not a detector for arbitrary secrets in repository content or logs.

The package JSON and its `fastcore`, `fastspec`, and `fasttransport` dependencies
are installed in the Nix Python site-packages closure. Standalone Python must
use the same environment. `all.py` exports the client and CI helpers, not every
upstream helper module. See the GitHub workflow skill for usage and approval
rules.
