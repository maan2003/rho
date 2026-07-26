---
name: linked-specs-review
description: Use when reviewing code or documentation in a project that adopts Linked Specs, either as a focused review or the Linked Specs pass within maintainability review.
advertise: false
user-invocable: true
editor-notes: The advertise flag is intentionally Tau-specific so this review skill remains searchable without appearing in the default prompt.
---

# Linked Specs Review

Read `linked-specs` and `linked-specs-updating`; this skill adds review behavior to their conventions.

## Goal and scope

Prevent drift of already-recorded constraints and keep affected records minimal, coherent, and truthful. Detect code/spec drift, unjustified deviations, and architectural changes outside the approved goal. Request new coverage only for a specific piece of qualifying durable knowledge with no better source of truth.

Trace the relevant record graph from:

- changed behavior, structure, interfaces, and documentation
- every applicable `specs/` directory in changed paths' scope hierarchy
- governing records found through links, IDs, and targeted search
- records linked from or to affected records when the relationship may have changed
- downstream consumers of changed public interfaces, persisted data, protocols, events, or shared state
- project-defined legacy architecture or design documentation with a distinct purpose

Do not read every record indiscriminately. Linked Specs review owns record quality and consistency, not every code, test, product, security, or reliability concern described by a record; route those concerns to the appropriate review or requester.

## Authority

Review-only agents do not edit files. Request editorial and mechanical fixes. Request a substantive record change only when the requester already explicitly approved its exact semantic end state. Identify the record and required semantic delta; give exact wording only when wording matters. For a new record, request its type, ID, scope, purpose, and required content rather than drafting it. Documentation authority does not authorize source changes.

When a record blocks implementation, appears to require an unapproved substantive change, disagrees with code without an approved resolution, or a confirmed decision or external requirement merits challenge:

1. investigate only enough to state the conflict and known evidence
2. report it promptly through the request chain and say that resolution likely requires a user decision
3. do not research alternatives, plan a resolution, or draft semantic changes unless the requester explicitly asks for that specific follow-up after receiving the escalation

Never silently disregard or rewrite an authoritative record. An approved resolution must synchronize every affected artifact.

For each affected `DECISION-*`, verify its authority:

- `inferred` means the current implementation embodies the choice but no explicit decision source was found
- `unconfirmed` means the current choice lacks explicit human approval
- `confirmed, ...` identifies the humans who explicitly approved it

Only humans can confirm a decision. Inferred and unconfirmed decisions must describe major current choices, not proposals or plans. Do not edit a confirmed record to describe a proposed replacement. If implementation embodies a substantively different choice, the old confirmation does not transfer; report the conflict until humans approve and the record can be synchronized. Editorial changes preserve confirmation.

## Review workflow

1. Identify the change's goal, changed surfaces and behavior, affected records, and approval evidence.
2. Read the governing and dependent records, applicable record-type skills, and relevant legacy documentation.
3. Compare intended behavior, records, code, and tests. Determine whether architecture, decisions, requirements, or functionality changed.
4. Check record coverage, validity, relationships, migration state, and anti-patterns.
5. Recheck missing links, stale references, undocumented exceptions, and architecture outside the main goal.
6. Report permitted fixes, escalations, findings, and unknowns.

## Coverage and control

Request records only for important, durable, independently useful knowledge:

- `ARCH-*` for significant components, boundaries, dependency direction, ownership, flows, interfaces, or invariants
- `DECISION-*` for major durable choices that have actually been made, constrain future work, could reasonably be reversed, and whose decisive reason is not preserved by code
- `REQ-*` for external obligations, including their strength, source, justification, and exceptions
- `SPEC-*` for a specific non-local behavioral contract that necessarily spans distributed implementation areas and has no coherent local owner
- non-standard types defined by an applicable additional skill

Do not request records for local details, mechanically evident behavior, ordinary API documentation, minor choices, proposals, or implementation plans. Behavior spanning files does not by itself justify a record. Request one only for a specific durable contract that cannot be understood or preserved through a coherent local source of truth. Check the default `ARCH-<project-or-component-name>` when overall shape changes. Consider requesting a test comment that cites the record defining protected behavior; if none exists, assess whether the behavior passes the same strict record threshold.

Architectural changes must be necessary to the stated main goal. Otherwise require explicit user or maintainer approval and a corresponding decision record. Resulting architecture records must describe current shape and link to rationale where useful. Report missing goal or approval evidence as an open gap.

Reject these record anti-patterns:

- a `DECISION-*` used to propose, design, plan, narrate, or track implementation rather than state a consequential chosen constraint, its decisive reason, and accurate authority metadata
- an `ARCH-*` overview buried under file, API, or data-structure inventory instead of showing topology, boundaries, relationships, direction, ownership, and flows
- a `SPEC-*` used as remote documentation without an honest, concrete, one-sentence `## Record justification` identifying the distributed implementation areas and why none is a coherent local owner
- any record padded with repetition, evident facts, incidental details, or unrelated subjects; shorten it and split only independently useful cohesive subtopics
- Linked Specs used as a planning system or substitute for ordinary local documentation

When removing out-of-scope material from a decision, check whether it independently preserves a qualifying durable safety, security, reliability, operational, or behavioral constraint. Request an appropriate durable owner before removal when one is needed; do not keep the material in `DECISION-*` merely because it is important.

## Agreement and migration

A non-decision record's specification plus optional `## Status` describes the current system. When code disagrees, first check for an accurate status qualification. Request correction only for editorial or mechanical errors or an exact semantic end state already approved; otherwise use the authority escalation above. A decision preserves its recorded governing choice and accurate authority rather than implementation progress. Do not rewrite any record to bless accidental or unjustified behavior.

Status is a scoped description, not approval. Reject it in `DECISION-*`. In other records, verify that it identifies the affected area, actual behavior, which specification parts apply, non-evident justification, and intended resolution when known. Independently verify approval for both the end state and staged transition. Future-facing decisions require confirmed human authority; `inferred` and `unconfirmed` are not proposal states.

When status-bearing predecessor and successor records coexist:

- both have status sections and reciprocal links
- their currently applicable scopes or behaviors partition unambiguously
- neither claims complete supersession prematurely
- a predecessor retained only for unmigrated implementation ends its heading with `(obsolete)`
- a predecessor that remains independently authoritative is narrowed or split instead

After migration, remove obsolete status sections and superseded records, and rewrite any retained predecessor. Report stale, unjustified, overly broad, or effectively permanent status qualifications.

## Record validity

For each affected record, check:

- repository-unique ID, matching filename, and leading `# <ID>: <title>`
- a standard or additionally defined record type
- placement matching code locality and ownership
- resolvable paths and IDs, accurate relationship wording, and no stale rename references
- necessary and complete `## Status` before specification text, absent from `DECISION-*` and when fully aligned
- every `DECISION-*` starts its substantive content with `## Decision` and limits rationale to what is decisive
- every `SPEC-*` has an honest, concrete, one-sentence `## Record justification` after status and before its functional description, identifying the distributed implementation areas and why none is a coherent local owner
- appropriate scope and cohesion, with omission preferred over splitting and no incidental detail preserved merely by moving it
- no index files
- tombstones and redirects exist only for a concrete need
- migration shims exist only by explicit request

Require links only when they aid navigation, explain constraints, clarify impact, or connect migration counterparts. Recommend removing legacy documentation only when it duplicates Linked Specs, is truly obsolete, and all references can be updated.

## Response format

Report:

1. **Specs checked:** records, source areas, and relevant legacy documentation.
2. **Spec changes needed:** permitted editorial/mechanical fixes and explicitly approved semantic additions, edits, moves, renames, or deletions; list unresolved substantive changes as escalations. Say none when appropriate.
3. **Findings:** each conflict, missing coverage, unjustified exception, authority defect, or architecture-control violation. For non-blockers give severity, location, evidence, requested outcome, and whether the change introduced/worsened it. For blockers state the conflict and required escalation without inventing a fix. Separate pre-existing issues.
4. **Open gaps:** behavior, scope, goal, or authority that could not be determined safely.

When there are no findings, report `No Linked Specs findings.` Do not justify the absence of changes.
