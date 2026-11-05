# DECISION-roles: Every input has a role, and each role reads as itself

Authority: confirmed, 2026-11-06, maan2003

## Decision

Every input carries one `Role`, in both runtimes:

- `user`: what a person typed, with a chat header around it. It wakes the agent
  soon.
- `tool`: anything no person typed and Rho did not write, such as another
  agent's message. With notebook output, it reaches the model only inside an
  `exec` result. It wakes the agent after a wait.
- `system`: Rho's own words. It wakes nobody and rides along with the next
  request. The model reads it as `role: user`, like the fixed lines that say
  output waits.

When `end_turn()` runs, the model waits inside its open call, and news answers
that call. With no call open, `tool` text stays where it is and the model gets
one fixed line telling it so; its next `exec` result carries the text.

In a Claude agent, a person's message is never written while a turn is going:
the open call is answered with the end-turn flag and the message follows it, so
the provider sees the tool result and the text in one user message. A native
request places all of it itself.

## Rationale

A role is whatever the harness writes: the API checks no sender, and a model and
auto mode's classifier both read `role: user` as the person's intent. A rule on
what may enter that role is the only way to keep an attacker's text out of it,
because any text can imitate any format inside it.

Holding the call is why the rule costs little: nearly every wake has a call to
answer, so the fixed line and its extra request are for a response with no call,
a restart, a compaction, and an agent's first message.

`system` reads as `user` because neither provider path gives Rho a better one
worth its cost. Claude Code's only mid-conversation `role: system` row says "The
user sent a new message", and its mods can add a text block to a row but cannot
change a role. Rho's lines are fixed text, so `user` carries no attacker's words.

Mid-turn delivery through Claude Code does not keep the rule. Depending on the
model, the CLI folds the message into a system message, which some models can
ignore as untrusted, or into the tool result string.

Labels in the text cannot replace the rule. They tell the model who to believe
but stop nothing, so the hard limit on what an agent may do stays at the host's
servers.
