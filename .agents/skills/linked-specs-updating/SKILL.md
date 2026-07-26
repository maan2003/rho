---
name: linked-specs-updating
description: Use whenever creating, modifying, moving, renaming, or deleting Linked Specs records.
user-invocable: true
---

# Updating Linked Specs

This skill extends `linked-specs` with conventions for keeping records concise, current, and useful to future implementation and review.

## Location and scope

Store records in a `specs/` directory within the directory whose code they govern. Prefer a project-root `specs/` for project-wide knowledge and a package-root `specs/` for package-local knowledge. Use deeper or otherwise placed directories only for a concrete locality or ownership benefit.

A record's natural scope is the parent of its `specs/` directory and that parent directory's descendants. Link across scopes when components interact or wider records constrain local ones. Do not centralize local knowledge merely to put all records together.

Do not create index files. Records should be discoverable through search, links, and references from code.

## IDs and filenames

Use `<TYPE>-<short-slug>.md`, with an uppercase type and concise lowercase kebab-case slug:

```text
ARCH-runtime.md
DECISION-nonblocking-network-operations.md
REQ-export-retention.md
SPEC-session-recovery.md
```

`ARCH`, `DECISION`, `REQ`, and `SPEC` are the standard types. For a non-standard type, follow the applicable additional skill as well as the shared conventions here.

The filename stem is the record ID. It must be unambiguous within the repository, so search before choosing one and add a component name when needed.

Avoid cosmetic renames. For a useful rename, rename the file, update every reference, and search for the old ID. Add `Previously known as: <old-id>` only when that history still matters. Do not retain redirect or tombstone files by default; version-control history is sufficient.

## Record types

### `ARCH-*`: architecture

Describe current responsibilities, boundaries, component relationships, dependency direction, ownership, flows, interfaces, trust boundaries, and invariants.

Name the default overview `ARCH-<project-or-component-name>`. Keep it focused on the most important high-level topology so readers can orient quickly; it is not an inventory of implementation details. Add focused records only for substantial, independently referable component or subsystem architecture that readers can load when needed. Put rationale for deliberate choices in `DECISION-*`, not in the architecture description.

### `DECISION-*`: decisions

Preserve the smallest statement of a major, durable choice that future work must not accidentally reverse. A qualifying choice constrains future work, a future maintainer could reasonably choose differently, and the code alone does not preserve why they should not. A decision record says what was decided and the decisive reason; it does not describe how the choice is implemented or managed.

Start its substantive content with `## Decision`. State the chosen constraint directly, preferably in one or a few normative sentences. Then give only the rationale needed to distinguish it from the most plausible contrary choice. Mention an alternative, downside, or assumption only when omitting it would make a likely future reversal appear reasonable. Do not inventory consequences, alternatives, assumptions, or tradeoffs.

Use exactly one authority form:

- `Authority: inferred` — the code embodies the choice, but no explicit decision source was found.
- `Authority: unconfirmed` — the choice lacks explicit human approval.
- `Authority: confirmed, YYYY-MM-DD, <human-id[, human-id...]>` — the identified human or humans explicitly approved it.

`inferred` and `unconfirmed` document major current choices already embodied by the implementation when explicit approval cannot be established. They are not proposal statuses. Keep prospective choices, options still being evaluated, and recommendations awaiting a decision in the planning system.

Do not create a `DECISION-*` merely because code changed. It is not an implementation description or design, a change summary, an implementation plan, or a post-hoc justification for a routine choice. Components, APIs, data flow, realization details, consequences, rollout, migration, testing, task breakdowns, sequencing, and progress belong in their appropriate local documentation or planning system. An implementation plan may cite a decision as a governing constraint, but must not be stored in the decision record.

Normally create a new record only after a user or maintainer has made a decision and asks that it be recorded, or as part of an explicit task to document existing decisions. A request to implement something is not by itself a request for a decision record. The decision must be important beyond the implementing change, and the record must remain useful independently of it. An explicit documentation request does not make a minor choice major; use comments, tests, API documentation, or ordinary project documentation for minor choices.

Only humans can confirm a decision, typically the user requesting the work. An agent's judgment or another agent's approval cannot confirm one. Do not elevate accidental or harmful behavior into a decision.

Do not edit a confirmed record to describe a proposed replacement. Keep the confirmed choice until a human approves a new one; then record the new choice with fresh confirmed authority. Approval changes the governing decision independently of implementation rollout. Synchronize an inferred or unconfirmed record only to a major choice already embodied by the implementation, never to a proposal. When implementation diverges and the intended choice is unclear, escalate instead of changing the record. Editorial corrections preserve existing authority.

### `REQ-*`: external requirements

Record an external obligation, its source or authority, strength and flexibility, justification, consequences, and relevant constraints, acceptance conditions, or exceptions.

Use clear normative language without presenting preferences as mandates. Explain the underlying need well enough to identify contradictions, obsolete assumptions, disproportionate cost, or better solutions. Internal implementation choices belong in `ARCH-*`, `DECISION-*`, or `SPEC-*`.

### `SPEC-*`: functional specifications

Create a functional specification only for a non-local behavioral contract whose implementation is necessarily distributed and which no single implementation artifact can own coherently. When the implementation is reasonably localized, keep its documentation beside it instead.

Every `SPEC-*` must include a `## Record justification` section after any `## Status` section and before the functional description. In one sentence, identify the distributed implementation areas and explain why none is a coherent local owner. If that cannot be stated honestly and concretely in one sentence, do not create the record; do not manufacture a justification to retain a desired document.

State only the non-local contract and invariants that the identified local artifacts cannot own. Omit behavior already clear from APIs, types, tests, CLI help, configuration documentation, or nearby comments. Do not restate source files or write an implementation walkthrough.

`SPEC-*` records must not contain source code or implementation excerpts.
Refer to code only by stable identifiers, such as module, type, function,
command, or configuration-key names.

## Record shape and links

Start with the ID and a concise title:

```md
# DECISION-nonblocking-network-operations: Nonblocking network operations

Authority: confirmed, 2026-03-06, username

## Decision

Network operations must not block executor threads.

## Rationale

Blocking them would violate [REQ-responsive-shutdown](REQ-responsive-shutdown.md).

```

Use plain prose and only as much structure as needed. Metadata is type-specific, not universal.

Prefer Markdown links with the target ID as link text. Describe relationships accurately, for example `depends on`, `refines`, `implements`, `constrained by`, or `supersedes`. Add links that improve navigation or explain impact. Do not add relation sections mechanically; require reciprocal links only for the migration pairs below.

## Gradual changes and status

When agreed architecture, requirements, or functionality cannot be implemented atomically, add an optional `## Status` section to the applicable `ARCH-*`, `REQ-*`, or `SPEC-*` record so the record set still describes the codebase truthfully. Omit the section when the code is believed to be fully in sync with the record.

Place `## Status` immediately after the heading and required leading type-specific metadata, before substantive text. Status records implementation alignment, not approval; establish approval of the end state and staged transition independently, under the record type's normal rules and the project's decision process.

Do not put `## Status` in `DECISION-*`. Adoption, rollout, migration, progress, and current implementation coverage are not part of a decision. Track execution in the issue or planning system and describe independently important current structure or behavior in `ARCH-*` or `SPEC-*`. A future-facing decision must have `Authority: confirmed, ...` before being checked in; `inferred` and `unconfirmed` are not proposal statuses.

The section must concisely identify:

- the affected area and what the implementation currently does
- which parts of the record already apply and which do not
- justification, when it is not evident from the transition
- the intended resolution or transition, when known

Keep it as a current-state summary, not a progress log or task checklist. Link to the project's issue or planning system for detailed execution work.

An old and new status-bearing record may coexist while different parts of the code remain governed by each. Give both records a `## Status` section, link them to each other, and state precisely which scopes or behaviors each still describes. Use accurate relationship wording such as `partially supersedes`, `partially superseded by`, or `transitions to`; do not claim complete supersession prematurely.

A confirmed decision changes when a human approves the new choice, not when rollout finishes. An inferred or unconfirmed decision changes only to follow a major choice already embodied by the implementation and authorized under the normal conflict rules. Do not retain or qualify an old `DECISION-*` to track implementation progress. Let version-control history preserve a superseded decision, and describe independently important transitional structure or behavior in status-bearing records.

When an old record is retained solely to describe implementation that has not yet migrated to its successor, append `(obsolete)` to its heading title, for example `# SPEC-old-flow: Old flow (obsolete)`. Its status must identify the still-affected implementation. Do not label a record obsolete if it remains the agreed specification for an independent scope; narrow or split the record instead.

Once migration is complete, remove migration status sections from every record, and remove the superseded record unless it still describes independently current knowledge. Narrow or rewrite any retained predecessor and replace transitional relationship wording. Update references as part of the same change; version-control history preserves the obsolete record.

## Content boundaries

Linked Specs are not a planning system or general-purpose descriptive documentation. Create a record only for durable information substantial enough to affect future implementation, maintenance, or review. Keep one cohesive, independently referable subject per record.

There is no target record length or completeness threshold. Prefer omission over splitting. Splitting does not make incidental detail appropriate.

Prefer comments, API documentation, tests, or ordinary project documentation for local, mechanically evident, executable, or minor information.

Removing material from a `DECISION-*` does not imply discarding it. Judge any durable safety, security, reliability, operational, or behavioral constraint independently. When it qualifies, preserve it in the appropriate `REQ-*`, `SPEC-*`, `ARCH-*`, security or operational documentation, local comment, or test before removing it from the decision. Do not retain it in `DECISION-*` merely because it is important.

Do not add routine lifecycle metadata, and keep undecided future proposals in planning or issue systems.

Outside the deliberate gradual-change process above, correct editorial or mechanical errors, and synchronize exact semantic end states already explicitly approved by the requester, in every affected artifact you are authorized to edit; request synchronization of every affected artifact you are not authorized to edit. Exact approval concerns the intended result, not the wording of the record. If code and a record disagree and the correct artifact is not immediately clear, treat the conflict as a substantive change under the rule below.

When a governing record blocks requested implementation or appears to require a substantive change, stop the affected work and promptly escalate to the task requester. Describe the conflict concisely and state that resolving it likely requires a user decision. Do not start additional review, research, planning, alternative exploration, or drafting unless, after receiving the escalation, the requester explicitly asks for specific further work; investigate only enough to identify and explain the conflict. Never use `## Status` to bless accidental drift.

Architectural changes should support the change's main goal. An unnecessary or out-of-scope architectural change requires explicit user or maintainer approval and a corresponding `DECISION-*` record.

When strong technical, product, safety, or cost reasons undermine a `DECISION-*` or `REQ-*`, escalate the conflict under the rule above instead of silently rewriting or disregarding it.

## Existing documentation

Linked Specs gives no special meaning to `ARCHITECTURE.md`, `design.md`, or similar files. Respect their project-defined purpose. Migrate and remove them only when they duplicate Linked Specs and are truly obsolete and unreferenced. Do not create compatibility indexes. Create migration shims only when requested.
