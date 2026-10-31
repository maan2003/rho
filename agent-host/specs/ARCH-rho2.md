# ARCH-rho2: Worksets, mail, and Python policy

## Status

Target architecture for the `rho2` branch, approved in discussion with
maan2003 on 2026-10-31. The branch implements it in parts; until the branch
lands, [ARCH-rho-agent](../rho-agent/specs/ARCH-rho-agent.md) describes `main`.
Migration of existing stores is the last part.

## Shape

Rust is a small core, like the C core of Emacs. Python in each workset makes
the policy decisions, like elisp: prompts, skills, which agents exist and on
which model, how mail reaches them, and workset setup.

- **Agent host.** Owns worksets (creation, deletion, one process each), the
  workset logs, the agent directory, the mail log, and everything that must
  stay outside a workset: credentials, inference accounts, the git keeper, and
  the Slack, Notion and GitHub servers. It enforces only what protects the
  host: credentials, accounts, and caps on processes and loaded agents.
- **Workset process.** One per workset, with one CPython interpreter. An
  `Isolated` workset gets a private `/src` namespace; a `Host` workset runs
  in a host directory with no namespace. The process runs the host file
  `~/.config/rho/init.py` at each start, in a plain workset namespace with no
  cells or wakes. It holds the agents that Python creates: each one is a native
  or Claude loop with its own notebook thread, whose cwd is its own.
- **GUI.** A mail client with the dealer, a directory of agents, and a viewer
  of workset logs. It runs no Python: it sends code the user wrote, to create
  a workset or to evaluate in one. It never builds that code from content it
  displays.

## The workset log

Each workset has one append-only log keyed by `(WorksetId, wpos)`, dense from
zero. A record is its time and one typed event; events about an agent carry
its `AgentId`. The workset process is the only writer of its log after
creation. Agent heads and the workset head are projections committed with each
append. Deleting a workset deletes its log, heads and process; it is the only
deletion.

The log holds what each model saw and what each agent is: workset creation and
evaluated code, agent creation and whole-spec changes, inputs, and the runtime
rows of each loop. An agent's spec is its typed runtime (native or Claude,
model, effort, and the runtime's session identity) plus string props that
Python sets and the GUI reads (name, title, parent, whether it is user-facing).
Instructions are recomputed by Python before each request and are not logged
([DECISION-instructions-are-code](../rho-agent/specs/DECISION-instructions-are-code.md)).

An input enters a model's context only as an `Input` row, written by the loop
of the receiving agent. Every input before an agent's request in the log is
delivered by it, so the receiver writes its own inputs to keep that order true.
Tool output is drained at the boundary and carried inside the request, as
before ([DECISION-pull-based-sources](../rho-agent/specs/DECISION-pull-based-sources.md)).

## Mail

Mail is how people and agents talk: one host-wide append-only log of
`Mail { from, to, re, kind, content }` between addresses (`User`, or an
agent). It lives on the host because the GUI is often offline and mail crosses
worksets, and it survives workset deletion. Agents post with `rho.mail.send`;
policy Python pulls each agent's mailbox and hands mail to the agent as an
`Input` that names it. The delivery position is derived from those rows, so the
mail log keeps no cursors. Slack is not mail: agents reach it through the
host's Slack server as Python, as before.

## GUI sync

The GUI syncs the mail log from a sequence number and receives the agent
directory as whole heads. It reads workset logs on demand, and follows one live
while a transcript is open. There is no global journal of every row.
