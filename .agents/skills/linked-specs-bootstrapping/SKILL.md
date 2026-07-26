---
name: linked-specs-bootstrapping
description: Use when introducing Linked Specs to an existing project and creating its initial coherent set of records.
user-invocable: true
---

# Bootstrapping Linked Specs

This skill extends `linked-specs` and `linked-specs-updating` with a project-wide migration process. Bootstrapping establishes a useful baseline, not an exhaustive transcription of the repository.

## Survey and initial records

Survey the project structure, major components, existing specifications and documentation, and representative code and tests. Separate project-wide from component-local knowledge. Treat the survey as discovery, not as a mandate to fill every record category.

Create a small initial set of records. Architecture, requirement, and functional records describe the current system; decision records preserve governing choices:

- a project-root `ARCH-<project-name>` overview of the major components and their relationships
- component `ARCH-*` records where independently useful
- `DECISION-*`, `REQ-*`, and `SPEC-*` records only for knowledge that passes the strict qualification rules in `linked-specs-updating` and lacks a better existing source of truth
- only the non-standard records called for by applicable additional skills

An initial corpus with no records of one or more types is normal. Do not create records to make the corpus appear complete.

Reuse suitable existing records where appropriate and preserve documentation with a distinct purpose rather than mechanically converting or deleting it. Correct clear editorial or mechanical contradictions. Escalate substantive contradictions to the task requester, state that resolving them likely requires a user decision, and do not investigate or propose a resolution unless, after receiving the escalation, the requester explicitly asks for specific further work. For an existing deliberate gradual change, use the optional `## Status` sections defined by `linked-specs-updating` to record current deviations.

Do not copy secrets, personal data, or unnecessarily sensitive operational details into records.

## Delegation

When delegation is available, prefer assigning each major component's survey to a separate agent. Survey agents propose candidate knowledge; they do not create records merely to produce a component deliverable.

Keep the project-wide overview and final integration under one coordinating agent. The coordinator reconciles cross-component boundaries, flows, IDs, links, duplicated knowledge, and conflicting conclusions.

## Project instruction

Add the adoption instruction to the project-root `AGENTS.md`, creating the file when necessary:

```md
This project uses the Linked Specs convention; consult the `linked-specs`
skill before working with specs or governed code.
```

Finish with a `linked-specs-review` pass. For a project too large to survey safely in one change, establish the project overview and highest-value component records first. Propose follow-up only for specific qualifying gaps already identified during the survey.
