# Agent instructions

Read `ARCHITECTURE.md` before making or reviewing architectural changes in this
workspace.

Before changing runtime, provider, persistence, tool, or CLI behavior in this
repository, read `SECURITY.md`.

This project uses the Linked Specs convention; consult the `linked-specs`
skill before working with specs or governed code. Records currently cover
`rho-agent2` only, in `crates/rho-agent2/specs/`.

Managed subtrees are first-class parts of this codebase, not opaque third-party
dependencies. In particular, edit `vendor/zed` or `vendor/jj` directly when
Zed or jj is the correct ownership layer instead of adding a workaround in a
Rho crate. Apply the same rule to every path reported by `jj subtree list`,
including `crates/senax-encoder`. Consult the `jj-subtree-management` skill
when adding, adopting, or updating a subtree.
