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

The three, named as the agent's speech acts (settled 3 Sep after two
renames; the user's original words in quotes; the tags are `<tell-human/>`,
`<ask-human/>`, `<answer-human/>`):

- **heads-up**: "working and not blocked on the human, but I have some
  stuff to show". Unprompted news the agent judges worth a look, and a
  finished task: the task was assigned, not asked, so its completion is
  news, not an answer. Most of a primary agent's life. The human looks
  out of interest, or when the daemon's observed idle time says the
  finished agent has been waiting long enough.
- **ask**: "might not be totally blocked, but the human would help
  massively". The agent needs something from the human.
- **answer**: "the human should check its response", narrowed to a
  question's answer: the human asked something and this reply is the
  answer; without the look the exchange was useless. First `done`, then
  `check`: renamed because it is about the exchange, not the task. An
  agent can be mid-work and still owe the human a look at the reply it
  just gave, so answer nests inside working. A reply that answers nothing
  ("ok" → "noted") declares nothing; the agent judges that, it is never
  derived from what triggered the turn.

Added in the talk that followed:

- **there is no "quiet" state.** Nothing for the human is the absence of
  an element, and whether the agent is running or idle is the daemon's
  observation, not a declaration (the user's call on 3 Sep, over an
  earlier draft that had quiet as a declared default).
- **heads-up is Slack's mention; unread is separate.** New output is a
  system fact. Heads-up is the agent's judgement that something is worth
  the human's eyes. Keep both, like a channel with chatter versus one
  where the human was named.
- **ask carries no details.** Whether the ask blocks is observed, not
  declared: an agent that asked and went idle is blocked, one that asked
  and kept working is not. Whether it wants a decision or an act (restart
  the daemon, push, approve) is in the reply's own words and changes no
  rank. An earlier draft had `blocked` and `for` attributes; dropped on
  3 Sep as redundant.
- **answer is Slack's "replied".** The agent's turn ended with a result
  and the ball is with the human; its twin, a turn that ended with a
  question, is ask. The deal bar words already exist: `needs reply · 1.9h`,
  `finished · 40m`.
- **went wrong is observed, never self-declared:** stalled (no events for
  minutes), crashed, waiting on a tool permission. Comes from the system,
  outranks everything, and an agent cannot paper over it with a cheerful
  status. The two sources stay visibly apart.
- **the reason is the turn's last line, not a separate field.** The word
  alone would send the human hunting; the reply's own closing line is the
  reason, at no extra tokens.

## Ranking: by what burns

Went wrong and an ask from an agent that has gone idle: steep. Answer:
medium, the human forgets. An ask from an agent still working: between. Heads-up: flat while the agent is working, and
rising with the daemon's observed idle time once it stops, which is how a
finished task climbs without a state of its own. Nothing declared: below the cutoff. The agent
sets the state and the reason; the curve is the human's, never the agent's.

## What clears each, or the status rots

Heads-up and answer clear when the human opens the transcript (the read
cursor, as in Slack). Ask clears when the human replies or performs the
act. Every status clears when the agent starts a new turn, so a stale answer
cannot outlive the next piece of work. An agent with a question
and things to show reports the higher one, the question on top.

## How the agent declares it: a tag in the response, not a tool call

The user's trick: the agent writes the status inline in its normal
response, as a small XML element, so declaring costs nothing and needs no
tool round trip:

```xml
<ask-human/>
```

The element name is the act with the human as its object, `tell-human`,
`ask-human`, `answer-human`, and that is the whole element: no attributes,
no body. Named so on 3 Sep because the goal is to put the act in the
agent's mind from the token alone; writing `<ask-human/>` is the thought
"I am asking the human". `tell` is the doc's `heads-up`. No body: the user's call on
3 Sep, a reason line is tokens spent repeating what the reply already
says. The reason shown on Home and in the deal bar is the last line of the
turn's own text, which Home shows for a running agent anyway. The daemon
strips the element from the transcript text, records it as a typed event,
and sets the fields; the element never renders. One element
per turn; the last one in a turn wins. No element means nothing declared:
the row shows only what the daemon observes (running, idle since, stalled),
and the human is not asked to look.

Why a tag and not a tool: a tool call ends the turn and costs an inference
step for a fact that never needs a result back. Four rules keep the tag
safe:

- Parse only outside code fences and only the last top-level element, or
  any agent that discusses the feature sets its own status by quoting the
  syntax (this document would).
- Strip while streaming, not after the turn, so a half-typed tag never
  flickers on screen.
- A malformed element is absent, logged, and never fails the turn.
- The names are distinctive (`<ask-human/>`, never a bare `<ask>`), so
  they cannot collide with markup an agent is talking about.

Forgetting is the weak spot and is survivable: a missing tag declares
nothing, and the states that matter most (stalled, crashed) are observed
by the daemon, so a forgetful agent degrades to "no self-declaration",
never to silence. A tool would only win if the declaration needed an answer back.

## Where it lives

One field on the agent node in the tree: `wants` (typed, with its
details); the reason is derived from the transcript's last line, never
stored. Observed health is a separate
field the daemon owns. Home and the phone header read them; the dealer
ranks by them.

## What not to do

- No more states than these. Agents misdeclare when the vocabulary is
  subtle; "needs your call" was already being used to mean "look at me".
- The agent never sets its own rank, only the state and the reason.
- Never let a self-declared state hide an observed one.
