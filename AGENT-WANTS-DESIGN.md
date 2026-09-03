# What an agent wants from the human

Status: idea, the user's, noted 2026-09-03 for a later pass. Not decided,
not being built. Sits on `TREE-DESIGN.md` (fields on the agent node) and
`HOME-DESIGN.md` (the words in the row).

## The problem

An agent's transcript says what the agent did. Nothing says what the human
loses by not looking at it, or how fast that loss grows. Today the human
learns that by opening every transcript, or by an engineer writing "I need
your call" into a message and hoping.

## The idea: a self-declared status, phrased as what the human should do

The user's three, in their words:

- **show**: "working and not blocked on the human, but I have some stuff to
  show". Most of a primary agent's life. The human looks out of interest.
- **needs input**: "might not be totally blocked, but the human would help
  massively".
- **done, check**: "the agent is done, the human should check its response".
  A question answered, or a task finished: without the look it was useless,
  and a finished task usually has a follow-up.

Added in the talk that followed:

- **quiet** is a real, declared fourth state, the default: working, nothing
  for the human. Without it "show" is indistinguishable from "no news yet".
- **show is Slack's mention; unread is separate.** New output is a system
  fact. Show is the agent's judgement that something is worth the human's
  eyes. Keep both, like a channel with chatter versus one where the human
  was named.
- **needs input carries two details, not two more states:** blocked or not
  (blocked burns the agent's time and context), and the kind of ask: a
  decision, or an act only the human can do (restart the daemon, push,
  approve). The act kind is the one that goes unnoticed today.
- **done is Slack's "replied".** The agent's turn ended with a result and
  the ball is with the human; its twin, a turn that ended with a question,
  is needs input. The deal bar words already exist: `needs reply · 1.9h`,
  `finished · 40m`.
- **went wrong is observed, never self-declared:** stalled (no events for
  minutes), crashed, waiting on a tool permission. Comes from the system,
  outranks everything, and an agent cannot paper over it with a cheerful
  status. The two sources stay visibly apart.
- **every status carries a one-line reason.** The word alone sends the
  human hunting through the transcript.

## Ranking: by what burns

Went wrong and needs-input-blocked: steep. Done: medium, the human forgets
and the agent's context goes cold. Needs-input-not-blocked: between. Show:
flat, the human comes when interested. Quiet: below the cutoff. The agent
sets the state and the reason; the curve is the human's, never the agent's.

## What clears each, or the status rots

Show and done clear when the human opens the transcript (the read cursor,
as in Slack). Needs input clears when the human replies or performs the
act. Every status resets to quiet when the agent starts a new turn, so a
stale done cannot outlive the next piece of work. An agent with a question
and things to show reports the higher one, the question on top.

## How the agent declares it: a tag in the response, not a tool call

The user's trick: the agent writes the status inline in its normal
response, as a small XML element, so declaring costs nothing and needs no
tool round trip:

```xml
<wants kind="needs-input" blocked="false" ask="decision">
V2 has no place for a todo's date; (b) maps it to defer_until, your call.
</wants>
```

`kind` is one of `quiet`, `show`, `needs-input`, `done`; `blocked` and `ask`
(`decision` or `act`) only on `needs-input`; the body is the reason line.
The daemon strips the element from the transcript text, records it as a
typed event, and sets the fields; the element never renders. One element
per turn; the last one in a turn wins. No element means the turn's default
(quiet while working, done when the turn ends).

## Where it lives

Two fields on the agent node in the tree: `wants` (typed, with its details)
and `reason` (one line, machine-written from the agent's own words, the one
place machine text is the agent's text). Observed health is a separate
field the daemon owns. Home and the phone header read them; the dealer
ranks by them.

## What not to do

- No more states than these. Agents misdeclare when the vocabulary is
  subtle; "needs your call" was already being used to mean "look at me".
- The agent never sets its own rank, only the state and the reason.
- Never let a self-declared state hide an observed one.
