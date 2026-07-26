# DECISION-instructions-are-code: Instructions are supplied by the caller, never stored

Authority: inferred

## Decision

An agent's instructions are passed in by the caller on both `create` and `load`,
and are never written to the store.

## Rationale

The prompt is policy, and policy is code. An agent reopened next month runs
today's prompt, and editing the prompt reaches every agent that already exists.
A copy in the database would be a stale one from the day it was written, and
keeping it fresh would mean migrating rows to ship a prompt change.
