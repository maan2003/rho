# Selected ghapi Python sources

Copied from [ghapi](https://github.com/AnswerDotAI/ghapi) 2.1.5, commit
`81b28a5325b311e9878a676a57fef801093242f6` (Apache-2.0; see `LICENSE`).

`core.py` retains the upstream `GhApi` operation generation, response
decoding, owner/repo overrides, and `pr_status`/`check_status` presentation. `check_status` fetches all
check-run pages instead of treating the first page as the whole verdict. The constructor and request transport are adapted to use the host's Octo
Unix socket without Python-side GitHub credentials. `gh_spec.py` contains
only metadata for the issue and pull-request operations needed here, and
the two status endpoints. `all.py` exports that selection, not all upstream
helpers.

Octo validates each request independently. The supported package is
embedded in the notebook interpreter; it is not installed for standalone
Python or exposed as unrestricted GitHub API access. The upstream client
depends on `fastcore`, `fastspec`, and `fasttransport`, packaged by Nix in
`flake.nix` rather than copied here.
