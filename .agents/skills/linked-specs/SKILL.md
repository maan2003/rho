---
name: linked-specs
description: Use whenever working with Linked Specs or artifacts they govern in a project that adopts the convention, including when discovering, interpreting, using, or referencing records.
user-invocable: true
editor-notes: Keep this core skill short; put detailed record mechanics in the updating and review skills.
---

# Linked Specs

Linked Specs keeps important project knowledge in small, interlinked Markdown records. Each record has a human-readable ID, occupies one file, and can be cited from specs, code, tests, reviews, and discussions.

Consulting this skill does not imply that any record should be created or changed. The default outcome of ordinary implementation work is no Linked Specs change. Create or update a record only for knowledge that meets its record type's qualification threshold. A request to document something supplies intent, not qualification.

Architecture, requirement, and functional records describe the current project state and should normally agree with the code. Decision records preserve settled governing choices. During deliberate gradual change, an optional `## Status` section on a non-decision record can explain known current deviations. Linked Specs are not a planning system or general-purpose descriptive documentation. Do not use them as a historical archive, issue tracker, implementation plan, changelog, or catalog of implementation details.

`linked-specs-updating` covers creating and changing records. `linked-specs-review` covers reviewing records and checking code against them.

## Records

Records use a `<TYPE>-<short-slug>.md` filename and begin with `# <TYPE>-<short-slug>: Title`.

- `ARCH-*` gives a concise, high-level map of system shape, boundaries, relationships, flows, and architectural invariants. It is not an implementation inventory.
- `DECISION-*` preserves a major, durable choice that future work must not accidentally reverse: the chosen constraint, its decisive reason, and its authority. It does not describe how the choice is implemented or managed.
- `REQ-*` records externally imposed product, business, legal, regulatory, interoperability, or stakeholder obligations.
- `SPEC-*` preserves a non-local behavioral contract only when its implementation is necessarily distributed and no single implementation artifact can own it coherently. Its required `## Record justification` section explains why local documentation cannot suffice.

Additional skills may describe non-standard Linked Specs record types and their conventions.

Records normally live in a `specs/` directory within the project or package they govern. Their natural scope is the parent of that `specs/` directory and its descendants, though records may link across scopes.

## Blocking records and changes

When a relevant record blocks requested implementation or appears to require a substantive change, stop the affected work and promptly escalate to the task requester. Describe the conflict concisely and state that resolving it likely requires a user decision.

Do not turn the blockage into additional review, research, planning, alternative exploration, or drafted record changes unless, after receiving the escalation, the requester explicitly asks for specific further work. Investigate only enough to identify and explain the conflict.

Do not re-escalate an editorial or mechanical correction, or synchronization to an exact semantic end state that the requester has already explicitly approved. Exact approval concerns the intended result, not the wording of the record.

## Using records

Relevant records form part of the context for artifacts they govern. Records need no index: IDs appear in filenames, Markdown links, code, and tests, so basic filesystem or text search (`find`, `grep`, or `rg`) locates both records and references. Links expose governing constraints and affected dependents.

Treat records as current project knowledge, not unquestionable authority. If code and a record disagree, do not assume either one is authoritative. When the conflict is substantive or the correct artifact is not immediately clear, escalate under the rule above rather than deciding independently.

Code and test references use record IDs where they provide useful rationale or traceability, not as ceremonial labels on self-explanatory implementation.

Optimize every record for the minimum durable knowledge needed to govern future work, not for completeness.
