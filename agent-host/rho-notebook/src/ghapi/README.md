# Octo-backed ghapi Python sources

Based on [ghapi](https://github.com/AnswerDotAI/ghapi) 2.1.5,
[`81b28a5`](https://github.com/AnswerDotAI/ghapi/commit/81b28a5325b311e9878a676a57fef801093242f6)
(Apache-2.0; see `LICENSE`).

`gh_spec.json` contains complete, unmodified upstream metadata for 133 REST
operations: 91 reads and 42 writes. It also defines Octo-only
`pulls.review_decision` and `pulls.set_draft` helpers backed by fixed,
typed GraphQL operations.

The selected surface covers PR collaboration, issue/comment workflows,
issue labels/assignees/field values/dependencies/sub-issues, and reactions
on issues and issue/review comments. Reads also cover repository discovery,
metadata, contents, commits/comparisons, branches/tags, Git objects/refs,
releases/assets metadata, and workflow/run/attempt/artifact metadata.
Actions writes remain limited to the three existing CI reruns.

REST reads and writes use the selected metadata as an explicit method/path-template
allowlist. Octo forwards query parameters and request bodies unchanged and relays
upstream responses without schema validation; GitHub validates its API. The Python
client still checks declared argument names and missing required parameters.

Omission or `UNSET` leaves a field out; `None` sends JSON null, and `False`, `0`,
and `""` remain values. Octo does not drop or interpret nulls: GitHub decides
whether a field accepts null. There is no schema generator or REST request/response
model.

The selected PR surface includes standard and asynchronous merges. Merge calls
require specific user approval and project-required reviews/checks; pin the
reviewed head with `sha`. Head-branch updates and review dismissal remain excluded.
Generic GraphQL, Git/ref/content writes,
repository administration, credential operations, workflow dispatch/deployment,
issue locking/pinning/suggestion moderation, and label/milestone administration
are not exposed. `issues.update` still supports milestone assignment.
Artifact/release/archive downloads requiring additional redirect handling
are not exposed; existing Actions job/run log downloads remain available.
API availability does not authorize a live write: agents still need the user's
specific approval.

`core.py` retains upstream operation generation, response decoding,
owner/repo overrides, and `pr_status`/`check_status` presentation.
`check_status` fetches all check-run pages. `pr_status(number)` remains CI for
one PR, not the gh CLI's personal PR overview.

Requests use the host's Octo Unix socket without Python-side credentials.
The async-only generated methods reject unknown keywords, missing required
parameters, and undeclared `query_`/`body_` fields. Use `result['items']` for
search rows: attribute `.items` is a dict method. Full PR diff/patch reads use
`headers_={"Accept": "application/vnd.github.diff"}` or
`application/vnd.github.patch`. Draft/ready transitions use
`await api.pulls.set_draft(number, draft=True/False)`.
`pulls.update(number, draft=True/False, ...)` also supports draft state without
changing the pinned REST metadata. It sends other edits through PATCH first,
then changes draft state through the fixed Octo helper and reads the refreshed PR.
The writes are not atomic; if the draft transition fails, earlier edits can remain.
Draft must be a boolean and cannot be combined with `stream=True`; `raw_=True`
returns the final PR read response. Omitted draft or `UNSET` preserves ordinary
PATCH behavior.

`await api.upload_attachment(path, owner=..., repo=...)` uploads a local
image/video and returns the asset response (`.url`). Owner/repo overrides are
optional; client defaults apply. The helper resolves the numeric repository ID
with `repos.get`, then sends raw bytes through the host's fixed
`POST /user-attachments/assets` route.
Only that operation targets `uploads.github.com`; ordinary REST requests still
target `api.github.com`. The pinned REST metadata is unchanged.

The helper accepts PNG, JPG/JPEG, GIF, WebP, SVG, MP4, MOV, and WebM, checks for
a nonempty regular file, and applies the CLI's 10 MiB image/100 MiB video limits.
GitHub validates media and may enforce a lower plan-dependent video limit.
Repository write access is required; GitHub App installation tokens are not
supported. The host targets GitHub.com, not GitHub Enterprise Server.

Uploads do not create comments or edit bodies. Embed images as
`![Description](asset.url)` and put video URLs alone in a paragraph.
Uploads require specific approval for the file and repository, including any
private-data disclosure. If publication fails after upload, reuse the URL
rather than upload again. Octo does not deduplicate uploads or follow upload
redirects. See the GitHub workflow skill for complete examples and retry rules.

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
