---
name: rho-desk
description: Use when asked to read or edit the Rho Desk document (the org-like dashboard outline of tasks and agents), or to migrate/file agents into it. Covers rho desk cat/checkout/apply.
---

# Edit the Desk document

The Desk is a daemon-owned, org-like outline. Headings are tasks; an
agent is filed under a heading by a `:handle:` tag at the end of the
heading line. The text is the only truth — filing, states, and structure
are all just text.

## Reading

```sh
rho desk cat
```

Prints the current document. Read-only, always safe.

## Editing

1. Check the document out to a file:

   ```sh
   rho desk checkout /tmp/desk.org
   ```

   This writes the document plus a `/tmp/desk.org.base` sidecar recording
   the exact state you forked from. When `$RHO_AGENT_ID` is set (it is,
   inside agent shells), your edits are attributed to you in the CRDT
   history; pass `--agent <handle>` to override.

2. Edit the file with ordinary file tools. Change only what you mean to
   change — `apply` sends a minimal diff, so untouched lines keep their
   identity and merge cleanly with concurrent edits by the user or other
   agents.

3. Apply:

   ```sh
   rho desk apply /tmp/desk.org
   ```

   The diff against the checkout base becomes one CRDT edit. Edits others
   made since your checkout are merged, not overwritten. The sidecar
   advances, so you can keep editing the same file and apply again. If
   others may have edited meanwhile and placement matters, re-checkout to
   see the latest text first.

## Document format

- Headings: `* Title`, `** Subtask`, deeper with more stars. Body text and
  `:key: value` property lines belong to the heading above them.
- State: a keyword at the end of the title — `* Ship it DONE`. Keywords:
  `TODO`, `DONE`, `DISCARDED`, `STAFFED`.
- Filing an agent: append its handle as an org tag on the heading line —
  `* investigate flaky CI :eng-x7y2:`. Several agents share one token:
  `:eng-a1:eng-b2:`. Unknown handles are left as plain text for the user.
- Projects: a `:project: /path` property line under a heading; subtrees
  inherit it.

## Migrating old agents

`rho debug agents` lists every persisted agent (name, disposition, mode,
workdir). To file old agents into the document: checkout, add headings
(for example under `* Archive`) with each agent's handle as a tag and a
state matching its disposition, then apply.
