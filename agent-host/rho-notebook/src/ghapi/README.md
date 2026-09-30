# Selected ghapi Python sources

Copied from [ghapi](https://github.com/AnswerDotAI/ghapi) 2.1.5, commit
`81b28a5325b311e9878a676a57fef801093242f6` (Apache-2.0; see `LICENSE`).

`core.py` retains the upstream `GhApi` operation generation, response
decoding, owner/repo overrides, and `pr_status`/`check_status` presentation. `check_status` fetches all
check-run pages instead of treating the first page as the whole verdict. The constructor and request transport are adapted to use the host's Octo
Unix socket without Python-side GitHub credentials. `gh_spec.py` contains
complete endpoint metadata for the supported issue/PR reads, PR creation/updates,
conversation comments, reviews, inline-thread replies, commit status,
PR files, review metadata and an Octo-only GraphQL-backed review-decision read,
check details and annotations, Actions runs and jobs, job and run
log downloads, and job/failed-jobs/whole-run reruns. `all.py` exports
that selection, not all upstream helpers. Standard endpoint metadata matches the
pinned upstream version. Status/check reads accept commit SHAs, branch names and
tag names, including slash-containing refs. The custom review-decision endpoint
is Octo-only.

Octo validates each request independently. The selected package and its `fastcore`, `fastspec`, and `fasttransport` dependencies
are installed in the Nix Python site-packages closure. The notebook uses that
closure; standalone Python must use the same environment. This does not expose
unrestricted GitHub API access: Octo still validates every request.
