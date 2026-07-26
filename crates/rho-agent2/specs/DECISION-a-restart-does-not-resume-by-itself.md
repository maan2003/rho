# DECISION-a-restart-does-not-resume-by-itself: Coming up is never a reason to send

Authority: confirmed, 2026-07-26, maan2003

## Decision

Loading an agent never sets `Resume::AtOnce`. A process that died mid-request
comes back up owing whatever the request left hanging, but does not make that
request again on its own; it waits for the sources like any other idle agent,
and a person asks for the retry.

## Rationale

A request that fires the instant the process starts is one nobody chose to make,
and if that request is what brought the process down, restarting is a crashloop
that spends money on every lap.

Nothing is lost by waiting. Whatever the interrupted request carried was already
appended to history by the `AgentEvent::Sent` that preceded it, and input that
was still queued replays into its source and names its own moment as usual. The
only thing that does not happen automatically is a request with nothing new in
it.
