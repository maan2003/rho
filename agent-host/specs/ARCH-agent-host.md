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
  cells or wakes. The host hands out the process only once init.py ended
  well, so every agent and evaluation runs under what it chose; a failed or
  hung init.py ends the process, and none of the workset's agents start
  until it is fixed. It holds the agents that Python creates: each one is a
  native or Claude loop with its own notebook thread, whose cwd is its own.
- **Permissions.** Python sets what a workset's calls go through, once for
  each run of its process and before its first agent starts: `bypass` runs
  every call, and `auto` has Claude Code's auto mode review each call
  against the `autoMode.environment` Python gives. All of a workset's agents
  share its process, so the choice is the workset's, and Rust refuses to
  change it once an agent started: an agent cannot turn its own review off.
  Nothing is stored, since init.py chooses again at each start, typically
  from the workset's props, so a changed policy reaches every workset at its
  next start. Without a choice the first agent fixes `bypass`. An `auto`
  workset runs Claude agents only, since nothing reviews a native agent's
  calls. A Claude agent sees its tool as
  `mcp__py__exec` there, because the server-side review skips a bare name
  that is not built in.
- **GUI.** A chat client of several hosts, with the dealer, a directory of agents, and a viewer
  of workset logs. It runs no Python: it sends code the user wrote, to create
  a workset or to evaluate in one. It never builds that code from content it
  displays.

## The workset log

Each workset has one append-only log keyed by `(WorksetId, wpos)`, dense from
zero. A record is its time and one typed event; events about an agent carry
its `AgentId`. The workset process is the only writer of its log after
creation. Agent heads and the workset head are projections committed with each
append. So is an index of each agent's rows, `(AgentId, wpos)`: a workset
holds many agents, and reading one of them must not cost the others' rows.
Deleting a workset deletes its log, heads and process; it is the only
deletion.

The log holds what each model saw and what each agent is: workset creation and
evaluated code, the workset's props, agent creation and whole-spec changes,
inputs, and the runtime rows of each loop. Workset props are strings Python
sets before the first agent, typically from the function the GUI's creation
code calls; they are fixed after that, so an agent cannot change what its
workset's policy reads at the next start. A workset whose props were never
set may set them once later, so that a workset made before its policy named
it can join that policy; its props are then fixed too. An agent's spec is its typed runtime (native or Claude,
model, effort, and the runtime's session identity) plus string props that
Python sets and the GUI reads (name, title, parent, and `origin`, the message
it was started from as `chat:<message id>`).
Instructions are recomputed by Python before each request and are not logged
([DECISION-instructions-are-code](../rho-agent/specs/DECISION-instructions-are-code.md)).

An input enters a model's context only inside a `Request` row, written by the
loop of the receiving agent. That row holds everything the request carried: tool
output, inputs and notices. Until then inputs and tool output wait in the
worker's memory, and a crash loses them
([DECISION-roles](../rho-agent/specs/DECISION-roles.md),
[DECISION-pull-based-sources](../rho-agent/specs/DECISION-pull-based-sources.md)).

## Chat

Chat is how people and agents talk, shaped like Zulip: streams hold topics,
and topics hold messages. Each host owns one append-only chat log, dense from
1; a stream, topic or message is named by the sequence number of the event
that made it. A fresh log is empty: there is no default stream, and whoever
starts a topic names its stream, which is made on first use. A host's chat is
its own: a GUI attached to several hosts shows each host's streams, and what
agents of different hosts say to each other (below) is not chat. The log lives
on the host because the GUI is often offline and chat crosses worksets, and it
survives workset deletion.

Chat and agents are separate hierarchies. They meet only through a topic's
members and through mentions. A message reaches its conversation's members and
every agent it mentions, never its sender. A topic's members are the agents
that joined it. Only Python joins agents to topics, so the host never
joins anyone on its own. The default delivery (`rho.hooks.deliver`) joins an
agent to the topic it was started from and to a topic where the user
mentions it. A mention by an agent only reaches. A policy with other rules
replaces the whole delivery, not one rule of it. Who receives a message is
fixed when it is posted, so a later member reads older messages, not receives
them. Two agents talk in their direct conversation, which is a conversation
outside every stream. Its members are its two agents, only they post in it,
and no one joins it. The two may be in different worksets of the host. The
default tools name it `agents.message`, not chat: an agent reads a message in
it as a message from the other agent, with no conversation id. Chat is the
user's. The GUI neither lists direct conversations nor makes cards for them:
each agent's own transcript shows what it sent and received.

An agent talks to an agent on another harness over GCCP
([docs/gccp.md](../../docs/gccp.md)). That is not chat: nothing of it is in the
chat log and no client syncs it. An agent's address is the host's iroh endpoint
id and the agent's id, so only a host that listens over iroh speaks GCCP. Trust
has two levels. The user approves a harness by its endpoint (`rho iroh approve
--role gccp`). That approval is kept apart from the clients', and each lets an
endpoint through on its own ALPN only, so a harness never gets what a client
of the host has. An agent then accepts an address on an approved harness under
a name (`agents.external.accept`). The host takes a message only for an agent
that accepted the sender's address; it refuses everything else and keeps no
trace of it. A message it takes is input to the agent with a tool's authority,
none, and counts as taken once the agent holds it, which is in memory until
the agent's next request records it. The host remembers the
messages it took last in memory and gives none of them twice; after a restart
it remembers none, so a message sent again across one is given twice. The host
keeps no message: an agent's send is tried while its call waits, under one id
picked at random, up to three times when the other harness gives no result,
and the call ends with what became of it, so the agent decides whether to
send again. The user sees approved harnesses and
acceptances with `rho gccp list`, and takes them back with `rho iroh revoke`
and `rho gccp remove`.

An agent leaves a topic itself (`chat.leave`), or the user takes it out.
The user, or an agent of the topic through Python, may rename a topic, move
it to another stream, or resolve it when its work is done; each change logs
the topic's whole head. They may also move messages to another topic, which
may be new. The default tools let an agent edit only topics it is in. A move delivers nothing
again, and a message is read in the topic it is in now, so `#41` always
leads to it. A direct conversation is never renamed, moved or resolved, and
its messages stay in it. What the user has read is the user's, not the
host's: it lives with the dealer's marks in the GUI's synced desk.

A mention carries a kind (`ask`, `result`, `fyi`); a mention of the user is
the only way a message asks for the user's attention, so a message has no kind
of its own. The user's mentions of agents are `ask`.

The message comes first and the agent second. The GUI starts an agent from an
existing message, and Python records that message as the agent's `origin` prop
(`chat:<message id>`), not as a host field, so other origins such as Slack can
follow. The default policy then joins the agent to the message's topic and
hands it that message first. An Engineer's child or Advisor is briefed in their
direct conversation with it, and reports there with `agents.message`.

Agents post with `rho.chat`, always naming the conversation. Policy Python
reads each agent's inbox and hands each message to the agent as an input that
names it. The delivery position is derived from the `Request` rows that carried
them, so the chat log keeps no cursors. Slack is not chat: agents reach it through the host's
Slack server as Python, and a workset may route its Slack messages to its agents (below).

## Slack

The host holds the Slack app's connection and buffers its events in memory;
agents read Slack and post through the host's Slack server. In a workset
process, one Python poll serves every handler that `rho.slack` adds, a
notebook's or the workset's own; handlers live until their process or
notebook ends.

A workset's Slack is routed when its policy routes it, at each start
(`rho.slack.route_messages`). Its routes are workset state, kept by the
workset process in `workset.redb` in the workset's state directory, so they
outlive the process and go with the workset. A route gives a scope to one
agent: the workspace, a channel, or one thread, and the most specific route
wins. Rust decides who hears a message; the policy's Python decides what the
agent is given, and with which role.

A thread route hears every new message of its thread. A workspace or
channel route hears mentions of the bot and what its mode adds: `mentions`
nothing more, `top_level` each new top-level message, `all` every message.
A mention in a thread with no route routes the thread to the agent it goes
to, so the replies that follow reach that agent before it answers; a post
from an agent's notebook routes a thread with no route to that agent. Only
a new route moves a routed thread. The workspace route's agent also gets
the bot's joins to channels and the notice that the host lost events.
Direct messages, edits, reactions and the bot's own posts are not routed;
a handler of the policy's own may act on them.

## GUI sync

The GUI syncs each host's chat log from the newest event it holds and receives the
directory of worksets and agents as whole entries. It reads the rows of each
agent it opens, from the newest it holds, through that agent's index; the read
ends with where the workset's log ends. It then names the worksets it follows,
each from where a read ended, and the host pushes their later rows in order,
never repeated or skipped. It pages older rows on demand and fetches
row bodies (tool output, whole responses) only when shown. Live streaming
frames come only for the agents it focuses.
