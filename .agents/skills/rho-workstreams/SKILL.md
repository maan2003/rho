---
name: rho-workstreams
description: Use only when the user explicitly asks to organize rho agents into workstreams — listing, renaming, labeling, grouping, or moving agents with the rho workstream CLI. Never reorganize workstreams on your own initiative.
---

# Workstreams

A workstream is rho's persistent unit of work: the user saying "these agents
are the same piece of work". Every agent belongs to exactly one workstream.
A newly created top-level agent founds its own; spawned sub-agents join
their parent's.

Both workstreams and agents carry free-form labels. A few label names have
meaning to the UI:

- `pin` — keeps the workstream (or agent) at the top of the rail.
- `hide` — folds the carrier out of view.
- `group:<name>` — on a workstream, shelves it under the named group
  header in the rail (for example `group:slack`).

Everything else is yours to invent; unknown labels are stored and displayed
but change nothing.

Attention (quiet / working / pending / needs input) is derived from agent
state and disposition; it is never set directly. A workstream's attention is
its most urgent member.

## CLI

`rho workstream` (alias `rho ws`) talks to the running daemon:

```sh
rho ws list                       # every workstream: labels, members, attention
rho ws show <ws>                  # one workstream and its member agents
rho ws rename <ws> "<name>"       # retitle
rho ws label <ws> <label>         # add a label (pin, hide, group:infra, ...)
rho ws unlabel <ws> <label>       # remove a label
rho ws move <agent> <ws>          # move an agent (and its spawned subtree)
```

`<ws>` is `ws-<id>` (as printed by `list`) or the workstream's exact name.
`<agent>` is a role-prefixed handle like `eng-16lh`, or a bare id prefix.

`move` with a name that matches no workstream creates one — so "spin off a
new workstream around this agent" is `rho ws move eng-xyz "new topic name"`.
Moving an agent always brings its spawned subtree along; a spawn tree never
straddles workstreams.

## Guidance

- Prefer renaming a workstream over leaving the auto-generated provisional
  title once the work's shape is clear.
- Group related long-lived streams (`group:<name>`) rather than merging
  unrelated agents into one stream.
- Do not add or remove `pin`/`hide` on the user's behalf unless asked; they
  encode the user's own triage.
