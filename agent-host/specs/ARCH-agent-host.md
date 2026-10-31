# ARCH-agent-host: Worksets, chat, and Python policy

Approved in discussion with maan2003 on 2026-10-31. The runtimes inside a
workset process are in [ARCH-rho-agent](../rho-agent/specs/ARCH-rho-agent.md).

## Shape

Rust is a small core, like the C core of Emacs. Python in each workset makes
the policy decisions, like elisp: prompts, skills, which agents exist and on
which model, how messages reach them, and workset setup.

- **Agent host.** Owns worksets (creation, deletion, one process each), the
  workset logs, the agent directory, the chat log, and everything that must
  stay outside a workset: credentials, inference accounts, the git keeper, and
  the Slack, Notion and GitHub servers. It enforces only what protects the
  host: credentials, accounts, and caps on processes and loaded agents.
- **Workset process.** One per workset, with one CPython interpreter. An
  `Isolated` workset gets a private `/src` namespace; a `Host` workset runs
  in a host directory with no namespace. The process runs the host file
  `~/.config/rho/init.py` at each start, in a plain workset namespace with no
  cells or wakes. It holds the agents that Python creates: each one is a native
  or Claude loop with its own notebook thread, whose cwd is its own.
- **GUI.** A chat client of several hosts, with the dealer, a directory of agents, and a viewer
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
Python sets and the GUI reads (name, title, parent, the topic it was briefed in).
Instructions are recomputed by Python before each request and are not logged
([DECISION-instructions-are-code](../rho-agent/specs/DECISION-instructions-are-code.md)).

An input enters a model's context only as an `Input` row, written by the loop
of the receiving agent. Every input before an agent's request in the log is
delivered by it, so the receiver writes its own inputs to keep that order true.
Tool output is drained at the boundary and carried inside the request, as
before ([DECISION-pull-based-sources](../rho-agent/specs/DECISION-pull-based-sources.md)).

## Chat

Chat is how people and agents talk, shaped like Zulip: streams hold topics,
and topics hold messages. Each host owns one append-only chat log, dense from
1; a stream, topic or message is named by the sequence number of the event
that made it, and the log opens with the default stream `rho`. Hosts never
talk to each other: a GUI attached to several hosts shows each host's streams.
The log lives on the host because the GUI is often offline and chat crosses
worksets, and it survives workset deletion.

A message reaches every agent of its topic and every agent it mentions, never
its sender, and posting makes the sender an agent of the topic. A mention
carries a kind (`ask`, `result`, `fyi`); a mention of the user is the only way
a message asks for the user's attention, so a message has no kind of its own.
Agents post with `rho.chat`, always naming the topic. Policy Python reads each
agent's inbox and hands each message to the agent as an `Input` that names
it. The delivery position is derived from those rows, so the chat log keeps
no cursors. Slack is not chat: agents reach it through the host's Slack
server as Python, as before.

## GUI sync

The GUI syncs each host's chat log from the newest event it holds and receives the
directory of worksets and agents as whole entries. It names the worksets it
follows, each from the position it holds, and the host pushes their later rows
in order, never repeated or skipped. It pages older rows on demand and fetches
row bodies (tool output, whole responses) only when shown. Live streaming
frames come only for the agents it focuses.
