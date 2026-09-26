# SPEC-agent2-wakes: Model wake scheduling

## Record justification

The wake contract spans notebook source facts, mail delivery, durable log projection, and the agent loop, so no single function owns both event production and delivery.

Every response runs to completion without interruption. A pending event's patience starts at the later of its occurrence and the current response's completion. Human messages wait 2 seconds, agent messages 15 seconds, task notifications 2 seconds, and undelivered failures 20 seconds. The latest exec finishing wakes at once; other successful completions do not wake. A response without an exec wakes immediately with a nudge, up to three consecutive responses before waiting for human input. A host restart wakes immediately.

Every wake reports all pending output and ends, and delivered ends no longer trigger failure wakes. A check-in runs at the last response's completion plus the most recent `set_max_wait` value, defaulting to 120 seconds on each response. Waiting for a reply does not disable check-ins. Archive mutes the model until a human message starts a fresh notebook.
