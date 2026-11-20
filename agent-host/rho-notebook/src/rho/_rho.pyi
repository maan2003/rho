"""The Rust core of rho, as Python sees it ([ARCH-agent-host]).

This module is built into the workset process. The `rho` package wraps it;
policy code imports `rho`, not `_rho`. Every function runs in the workset
process and acts on its own workset only. Agent ids are their encoded text
(for example "x4cvbixbtifu"); a mail address is "user" or an agent id.

Blocking calls release the GIL. Calls marked async return awaitables that
run on the asyncio loop of the caller's thread.
"""

from typing import Literal, TypedDict

Role = Literal["user", "tool"]

class NativeRuntime(TypedDict):
    kind: Literal["native"]
    model: Literal["sol", "terra", "astra", "hydrogen"]
    effort: Literal["low", "medium", "high", "xhigh"]
    fast: bool

class ClaudeRuntime(TypedDict):
    kind: Literal["claude"]
    model: Literal["fable", "opus", "sonnet", "haiku"]
    effort: Literal["low", "medium", "high", "xhigh", "max"]

Runtime = NativeRuntime | ClaudeRuntime

class Spec(TypedDict):
    runtime: Runtime
    props: dict[str, str]

# How a mention reaches: mail to the user pings or is quiet; an agent is
# reached as a member would be, or directly, without the thread's members.
MentionKind = Literal["ping", "quiet", "member", "direct"]

class MessageRecord(TypedDict):
    id: int
    at: int  # unix ms
    parent: int | None  # the message it hangs under; None for a root
    sender: str  # "user" or an agent id
    mentions: list[tuple[str, MentionKind]]  # (address, kind)
    text: str

class ThreadRecord(TypedDict):
    id: int  # the message the thread hangs under
    name: str  # as named, else the first line of its message
    path: list[tuple[int, str]]  # ids and names from the root down to it
    agents: list[str]  # who a message posted in it reaches by membership

class AgentsFile(TypedDict):
    path: str
    content: str

class Skill(TypedDict):
    name: str
    description: str
    path: str  # its SKILL.md

class Context(TypedDict):
    agents_files: list[AgentsFile]
    skills: list[Skill]

# -- the workset -------------------------------------------------------------

def workset_id() -> str: ...
def workset_kind() -> Literal["isolated", "host"]:
    """An isolated workset has a private /src; a host one runs on the host."""
def workset_props() -> dict[str, str]:
    """The workset's props, empty until set."""
def set_workset_props(props: dict[str, str]) -> None:
    """Replace the workset's props. The host refuses once it has an agent."""
def set_permissions(mode: Literal["bypass", "auto"], environment: list[str] | None = None) -> None:
    """Fix this run's permissions: once, before the first agent starts.
    "bypass" takes no environment."""
def workset_permissions() -> Literal["bypass", "auto"] | None:
    """This run's permissions, once fixed."""
def clone(url: str, dest: str, rev: str = "") -> None:
    """Clone `url` into `dest` from the host's mirror, then detach at `rev`
    (empty: the remote's default branch). Blocking."""
def repo_context(cwd: str) -> Context:
    """The AGENTS.md at the root of the repository `cwd` is in, and the
    skills in its `.agents/skills`, as rho-context-config discovers them."""
def skills_in(directory: str) -> list[Skill]:
    """The skills in one skills directory; none if it does not exist."""
def builtin_skills() -> list[Skill]:
    """The skills this build of Rho bundles."""
def agents_file(path: str) -> AgentsFile | None:
    """One AGENTS.md-style file under a repository's size limit; None if
    there is none."""
def models() -> list[Runtime]:
    """The runtimes the host's accounts can run now."""

# -- agents ------------------------------------------------------------------

def create_agent(spec: Spec) -> str:
    """Log `AgentCreated` and start the agent's loop. Returns its id once the
    row is durable. Raises when the host is at its agent cap or the runtime
    is not available."""
def runs(agent: str) -> bool:
    """Whether this run's permissions let the agent run: an auto workset
    runs no native agent."""
def agents() -> dict[str, Spec]:
    """Every agent of this workset, by id."""
def set_spec(agent: str, spec: Spec) -> bool:
    """Log `AgentChanged`. A same-kind model change keeps conversation
    identity. Return true if a stopped agent's runtime loop was replaced."""
def input(agent: str, text: str, *, role: Role, message: int | None = None) -> None:
    """Give the agent an input: it waits in memory until a request can carry
    it, and that request's row holds it. This returns once the agent has it. A
    crash loses it unless it is a mail, which is handed again. `role`
    says who speaks: "user" for what a person typed, "tool" for anything else.
    `message` names the mail it hands over."""
def delivered(agent: str) -> int:
    """The newest message id a request carried to the agent, or 0. What waits
    in the agent is not counted."""
def cancel(agent: str) -> None: ...
def compact(agent: str) -> None: ...
def rewind(agent: str, turns: int) -> None:
    """Branch the agent's history before its `turns`-th last request that
    carried a person's input. The notebook stays."""
def new_cache_key(agent: str) -> None:
    """Give a native agent a fresh prompt cache key (an `AgentChanged`)."""

# -- mail --------------------------------------------------------------------
#
# The host's mail: a forest of messages. Every id is the position of the
# event that made it in the host's mail log; a thread's id is its message's.
# A message reaches the members of the thread it is posted in and of the
# threads above it, and every agent it mentions, never its sender. An agent
# joins or leaves a thread; its nearest record up the chain decides, so a
# join above and a leave below cut that branch. Neither posting nor a
# mention joins. Messages from the user come only from the GUI, so Python
# cannot forge them. Direct agent mail reaches only its addressed recipient.

class MailHit(TypedDict):
    project: int
    thread: int
    message: int
    subject: str
    excerpt: str

def edit_message(agent: str, id: int, text: str, *, ping: bool | None = None) -> None:
    """Change your own mail. `ping` turns the ping of mail to the user on or
    off; None keeps it."""
def delete_message(agent: str, id: int) -> None: ...
def search_mail(query: str, project: int | None, limit: int) -> list[MailHit]: ...

def send_mail(
    sender: str,
    parent: int | None,
    text: str,
    *,
    to: str | None = None,
    ping: bool | None = None,
    reply_to: int | None = None,
) -> tuple[int, tuple[int, str] | None]:
    """Return the new id and, if unanswered, the preceding project's mail id and writer.
    Send durable mail as `sender`. With an agent in `to`, the host
    chooses the conversation. Only mail to "user" takes `ping`, which it
    needs, and must pass the STE check."""
def agent_handle(agent: str) -> str | None: ...
def resolve_agent(prefix: str) -> str:
    """The id of the one agent of the host, in any workset, whose id starts
    with `prefix`. Raises when none or several do."""
def external_address(agent: str) -> str:
    """The address agents on other hosts reach `agent` at:
    `<endpoint id>/<local id>`. Raises when the host has no iroh endpoint."""
def external_accept(agent: str, address: str, name: str) -> None:
    """`agent` accepts `address`, on another host, under `name`. Raises when
    the user has not approved that host, or the agent already accepted the
    address under another name."""
async def external_send(agent: str, address: str, text: str) -> str:
    """Send `text` to `address`, which `agent` accepted, and wait for what
    the address's host says: the name the agent gave the address, once that
    host took the message. Raises when it refuses, or gives no result in
    the host's three tries."""
def external_contacts(agent: str) -> list[dict]:
    """The addresses `agent` accepted, each a dict of `address` and `name`."""
def join(thread: int, agent: str) -> None:
    """Make `agent` a member of the thread from now on."""
def leave(thread: int, agent: str) -> None:
    """`agent` stops being a member of the thread, whether it joined it or
    one above."""
def reparent(agent: str, message: int, parent: int | None) -> None:
    """Put a message and the thread under it under `parent`, or among the
    roots, for `agent`. Not under itself or its own thread. A move
    delivers nothing again."""
def name(agent: str, thread: int, name: str) -> None:
    """Name a thread for `agent`; an empty name takes the name back."""
def project_named(name: str) -> int | None: ...
def thread(id: int) -> ThreadRecord | None: ...
def message(id: int) -> MessageRecord | None:
    """A message, under the parent it has now: messages may be moved."""
def read(thread: int | None, last: int) -> list[MessageRecord]:
    """The `last` newest messages under `thread`, oldest first; the roots
    for None."""
async def inbox(agent: str, after: int) -> list[MessageRecord]:
    """What `agent` receives after message `after`, waiting until there is
    some."""

# -- Slack routes ------------------------------------------------------------
# The workset's, in `workset.redb` in its state directory; `rho.slack` wraps
# them. Agent ids must be of this workset. A route without `channel` is the
# workspace's; one with `thread_ts` too is a thread's.

def slack_who_hears(event: dict, bot_user: str, bot_id: str) -> tuple[str, str] | None:
    """The agent a new Slack message, or a reaction added to or removed from
    an agent's post, goes to, if anyone hears it, and the root ts of the
    thread it is in, given the bot's user id and bot id, as `auth.test`
    gives them. A mention in a thread with no route routes the thread to
    that agent."""
def slack_owner(channel: str | None = None, thread_ts: str | None = None) -> str | None:
    """The agent of the most specific route that covers the scope."""
def slack_route(
    agent: str,
    channel: str | None = None,
    thread_ts: str | None = None,
    mode: Literal["mentions", "top_level", "all"] | None = None,
) -> None:
    """Route the scope to `agent`. A thread route takes no mode; the others
    hear "mentions" without one."""
def slack_unroute(channel: str | None = None, thread_ts: str | None = None) -> None: ...
def slack_posted(channel: str, ts: str, thread_ts: str, agent: str) -> str:
    """`agent` wrote the bot's post `ts` in the thread: reactions to it go to
    `agent`, and the thread is routed to `agent` unless it has a route.
    Returns the agent the thread routes to."""
def slack_routes() -> dict:
    """`{"workspace": {"agent", "mode"} | None, "channels": {channel:
    {"agent", "mode"}}, "threads": {(channel, thread_ts): agent}}`."""

# -- inside an agent's notebook ----------------------------------------------

def current_agent() -> str | None:
    """The agent whose notebook thread this is, or None on the workset
    thread."""
